use mtp_rs::MtpDeviceInfo;
use nusb::MaybeFuture;
use std::path::{Path, PathBuf};

struct UsbNode {
    node: PathBuf,
    sysfs: PathBuf,
    mtp_interfaces: Vec<u8>,
    adb_interfaces: Vec<u8>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Gvfs,
    Kiod,
    Adb,
    Other,
}

struct Holder {
    name: String,
    kind: Kind,
}

fn is_mtp(i: &nusb::InterfaceInfo) -> bool {
    matches!(
        (i.class(), i.subclass(), i.protocol()),
        (0x06 | 0xff, 0x01, 0x01)
    ) || i
        .interface_string()
        .is_some_and(|s| s.trim().eq_ignore_ascii_case("MTP"))
}

fn is_adb(i: &nusb::InterfaceInfo) -> bool {
    (i.class(), i.subclass(), i.protocol()) == (0xff, 0x42, 0x01)
}

fn locate(info: &MtpDeviceInfo) -> Option<UsbNode> {
    let d = nusb::list_devices().wait().ok()?.find(|d| {
        d.vendor_id() == info.vendor_id
            && d.product_id() == info.product_id
            && match (info.serial_number.as_deref(), d.serial_number()) {
                (Some(a), Some(b)) => a == b,
                _ => location_id(d) == info.location_id,
            }
    })?;
    let numbers = |pred: fn(&nusb::InterfaceInfo) -> bool| {
        let mut v: Vec<u8> = d
            .interfaces()
            .filter(|i| pred(i))
            .map(|i| i.interface_number())
            .collect();
        v.dedup();
        v
    };
    Some(UsbNode {
        node: format!("/dev/bus/usb/{:03}/{:03}", d.busnum(), d.device_address()).into(),
        sysfs: d.sysfs_path().to_owned(),
        mtp_interfaces: numbers(is_mtp),
        adb_interfaces: numbers(is_adb),
    })
}

/// mtp-rs's private `location_id`: FNV-1a over bus id, 0xFF, port chain.
fn location_id(d: &nusb::DeviceInfo) -> u64 {
    const PRIME: u64 = 0x0100_0000_01b3;
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let bytes = d
        .bus_id()
        .as_bytes()
        .iter()
        .chain(&[0xff])
        .chain(d.port_chain());
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(PRIME);
    }
    h
}

impl UsbNode {
    fn claimed(&self) -> Vec<u8> {
        let Some(kname) = self.sysfs.file_name().and_then(|n| n.to_str()) else {
            return Vec::new();
        };
        let prefix = format!("{kname}:");
        let Ok(entries) = std::fs::read_dir(&self.sysfs) else {
            return Vec::new();
        };
        entries
            .flatten()
            .filter_map(|e| {
                let name = e.file_name();
                let ifnum = name
                    .to_str()?
                    .strip_prefix(&prefix)?
                    .split_once('.')?
                    .1
                    .parse::<u8>()
                    .ok()?;
                let driver = std::fs::read_link(e.path().join("driver")).ok()?;
                driver.ends_with("usbfs").then_some(ifnum)
            })
            .collect()
    }

    fn mtp_claimed(&self) -> bool {
        let claimed = self.claimed();
        if self.mtp_interfaces.is_empty() {
            claimed.iter().any(|n| !self.adb_interfaces.contains(n))
        } else {
            claimed.iter().any(|n| self.mtp_interfaces.contains(n))
        }
    }
}

fn comm(pid: i32) -> Option<String> {
    Some(
        std::fs::read_to_string(format!("/proc/{pid}/comm"))
            .ok()?
            .trim()
            .to_owned(),
    )
}

fn holders(node: &Path) -> Vec<Holder> {
    let me = std::process::id() as i32;
    let Ok(procs) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut found: Vec<Holder> = procs
        .flatten()
        .filter_map(|p| {
            let pid: i32 = p.file_name().to_str()?.parse().ok()?;
            if pid == me {
                return None;
            }
            let has_node = std::fs::read_dir(p.path().join("fd"))
                .ok()?
                .flatten()
                .any(|fd| std::fs::read_link(fd.path()).is_ok_and(|target| target == node));
            if !has_node {
                return None;
            }
            let name = comm(pid).unwrap_or_default();
            let kind = match name.as_str() {
                "gvfsd-mtp" | "gvfsd-gphoto2" => Kind::Gvfs,
                "kiod6" | "kiod5" => Kind::Kiod,
                "adb" | "fastboot" => Kind::Adb,
                _ => Kind::Other,
            };
            Some(Holder { name, kind })
        })
        .collect();
    found.sort_by_key(|h| h.kind as u8);
    found
}

pub fn who_holds(info: &MtpDeviceInfo) -> Option<String> {
    let usb = locate(info)?;
    if !usb.mtp_claimed() {
        return None;
    }
    let holder = holders(&usb.node).into_iter().find(|h| h.kind != Kind::Adb);
    Some(match holder.map(|h| (h.kind, h.name)) {
        Some((Kind::Gvfs, _)) => {
            "the desktop's gvfs mount (GNOME Files): unmount the phone there".into()
        }
        Some((Kind::Kiod, _)) => {
            "KDE (kiod6): close Dolphin's phone view or log out and back in".into()
        }
        Some((_, name)) => format!("{name}: close it"),
        None => "a process this user cannot see".into(),
    })
}
