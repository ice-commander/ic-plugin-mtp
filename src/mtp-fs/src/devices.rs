use futures::StreamExt;
use ic_plugin_api::{IcDrive, IcDrives, IcHost};
use mtp_rs::{HotplugEvent, MtpDeviceInfo};
use std::cell::RefCell;
use std::ffi::CString;
use std::os::raw::c_void;
use std::sync::Mutex;

static SEEN: Mutex<Vec<MtpDeviceInfo>> = Mutex::new(Vec::new());

thread_local! {
    static HELD: RefCell<(Vec<CString>, Vec<IcDrive>)> = const { RefCell::new((Vec::new(), Vec::new())) };
}

pub fn key_of(info: &MtpDeviceInfo) -> String {
    match &info.serial_number {
        Some(serial) => format!("mtp.{serial}"),
        None => format!("mtp.{:016x}", info.location_id),
    }
}

fn settings_of(info: &MtpDeviceInfo) -> String {
    let mut settings = serde_json::Map::new();
    if let Some(serial) = &info.serial_number {
        settings.insert("serial".into(), serial.clone().into());
    }
    settings.insert("location".into(), info.location_id.to_string().into());
    serde_json::Value::Object(settings).to_string()
}

pub extern "C" fn rows(_user_data: *mut c_void) -> IcDrives {
    let seen = SEEN.lock().map(|seen| seen.clone()).unwrap_or_default();
    let mut text = Vec::with_capacity(seen.len() * 4);
    for info in &seen {
        let name = info.product.clone().unwrap_or_else(|| "MTP device".into());
        let subtitle = info.manufacturer.clone().unwrap_or_else(|| "MTP".into());
        for line in [key_of(info), name, subtitle, settings_of(info)] {
            text.push(CString::new(line).unwrap_or_default());
        }
    }
    let rows: Vec<IcDrive> = (0..seen.len())
        .map(|at| IcDrive {
            key: text[at * 4].as_ptr(),
            name: text[at * 4 + 1].as_ptr(),
            subtitle: text[at * 4 + 2].as_ptr(),
            settings: text[at * 4 + 3].as_ptr(),
            svg: std::ptr::null(),
            online: 1,
        })
        .collect();
    HELD.with(|held| {
        *held.borrow_mut() = (text, rows);
        let held = held.borrow();
        IcDrives {
            count: held.1.len() as u32,
            rows: held.1.as_ptr(),
            ..IcDrives::EMPTY
        }
    })
}

fn heard(event: HotplugEvent) {
    let Ok(mut seen) = SEEN.lock() else {
        return;
    };
    match event {
        HotplugEvent::Arrived(info) => {
            let key = key_of(&info);
            seen.retain(|known| key_of(known) != key);
            seen.push(info);
        }
        HotplugEvent::Left(info) => {
            let key = key_of(&info);
            seen.retain(|known| key_of(known) != key);
        }
    }
}

pub fn watch(host: *const IcHost) {
    let host = host as usize;
    let _ = std::thread::Builder::new()
        .name("ic-mtp-hotplug".into())
        .spawn(move || {
            futures::executor::block_on(async move {
                let Ok(mut watch) = mtp_rs::watch_devices() else {
                    return;
                };
                while let Some(event) = watch.next().await {
                    heard(event);
                    unsafe { ((*(host as *const IcHost)).drives_changed)() };
                }
            })
        });
}
