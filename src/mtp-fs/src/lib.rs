#![allow(clippy::not_unsafe_ptr_arg_deref)]

mod devices;
#[cfg(target_os = "linux")]
mod holder;
pub mod mount;
mod phone;

use ic_plugin_api::{
    check_host, needs_connection_kinds, needs_drive_source, HostCheck, IcHost, IC_ABI_VERSION,
    IC_ERR_HOST_TOO_OLD, IC_ERR_HOST_UNKNOWN, IC_ERR_INIT_FAILED, IC_OK,
};
use std::ffi::CString;
use std::os::raw::{c_char, c_int};

pub const KIND: &str = "mtp";
pub const ICON: &str = include_str!("../assets/phone.svg");

include!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../version.rs"));

ic_plugin_api::declare_about!(
    "ic-mtp-fs",
    "MTP",
    plugins_version!(),
    "Android phones and other MTP devices"
);

#[cfg_attr(feature = "export-abi", no_mangle)]
pub extern "C" fn ic_plugin_init(host: *const IcHost, _kind: *const c_char) -> c_int {
    let needed = needs_drive_source().max(needs_connection_kinds());
    match check_host(host, IC_ABI_VERSION, needed) {
        HostCheck::Ok => {}
        HostCheck::WrongMagic => return IC_ERR_HOST_UNKNOWN,
        _ => return IC_ERR_HOST_TOO_OLD,
    }
    let (Ok(kind), Ok(svg)) = (CString::new(KIND), CString::new(ICON)) else {
        return IC_ERR_INIT_FAILED;
    };
    let fs: &'static ic_plugin_api::IcFsVTable = Box::leak(Box::new(mount::vtable()));
    let connection = mount::connection_vtable(fs);
    unsafe {
        let declared = ((*host).register_connection_kind)(
            kind.as_ptr(),
            std::ptr::null(),
            0,
            &connection,
            std::ptr::null_mut(),
        );
        if declared != IC_OK {
            return declared;
        }
        let offered = ((*host).register_drive_source)(
            kind.as_ptr(),
            svg.as_ptr(),
            devices::rows,
            std::ptr::null_mut(),
        );
        if offered != IC_OK {
            return offered;
        }
    }
    devices::watch(host);
    IC_OK
}
