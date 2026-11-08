// Every call may arrive on a worker thread: nothing here may touch the host.

use crate::phone::{Phone, Reading, Target, Writing};
use ic_plugin_api::{
    IcBytes, IcConnectionVTable, IcDirEntry, IcFsHandle, IcFsStream, IcFsVTable, IcListing,
    IC_ERR_IO, IC_OK, IC_OPEN_READ, IC_OPEN_WRITE,
};
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_void};
use std::sync::{Arc, Mutex};

type Said = Arc<Mutex<Option<String>>>;

struct Mounted {
    phone: Arc<Phone>,
    failure: Said,
    said: Option<CString>,
    names: Vec<CString>,
    entries: Vec<IcDirEntry>,
    body: Vec<u8>,
}

enum Open {
    Reading(Reading),
    Writing(Writing),
}

struct Streaming {
    open: Option<Open>,
    failure: Said,
}

fn remember(failure: &Said, reason: String) {
    if let Ok(mut slot) = failure.lock() {
        *slot = Some(reason);
    }
}

fn wanted(path: *const c_char) -> String {
    if path.is_null() {
        return String::new();
    }
    unsafe { CStr::from_ptr(path) }
        .to_string_lossy()
        .to_string()
}

fn held<R>(handle: IcFsHandle, work: impl FnOnce(&mut Mounted) -> R) -> Option<R> {
    if handle.is_null() {
        return None;
    }
    Some(work(unsafe { &mut *(handle as *mut Mounted) }))
}

fn reported(handle: IcFsHandle, outcome: Option<Result<(), String>>) -> c_int {
    match outcome {
        Some(Ok(())) => IC_OK,
        Some(Err(reason)) => {
            let _ = held(handle, |mounted| remember(&mounted.failure, reason));
            IC_ERR_IO
        }
        None => IC_ERR_IO,
    }
}

pub extern "C" fn open(bytes: *const u8, len: u64, _user_data: *mut c_void) -> IcFsHandle {
    if bytes.is_null() || len == 0 {
        return std::ptr::null_mut();
    }
    let encoded = unsafe { std::slice::from_raw_parts(bytes, len as usize) };
    let Ok(settings) = serde_json::from_slice::<serde_json::Value>(encoded) else {
        return std::ptr::null_mut();
    };
    let text = |key: &str| {
        settings
            .get(key)
            .and_then(|value| value.as_str())
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    };
    let serial = text("serial");
    let location = text("location").and_then(|value| value.parse::<u64>().ok());
    if serial.is_none() && location.is_none() {
        return std::ptr::null_mut();
    }
    Box::into_raw(Box::new(Mounted {
        phone: Arc::new(Phone::new(Target {
            serial,
            location: location.unwrap_or(0),
        })),
        failure: Arc::default(),
        said: None,
        names: Vec::new(),
        entries: Vec::new(),
        body: Vec::new(),
    })) as IcFsHandle
}

pub extern "C" fn close(handle: IcFsHandle) {
    if handle.is_null() {
        return;
    }
    let mounted = unsafe { Box::from_raw(handle as *mut Mounted) };
    mounted.phone.close();
}

pub extern "C" fn list(handle: IcFsHandle, path: *const c_char) -> IcListing {
    let path = wanted(path);
    held(handle, |mounted| {
        mounted.names.clear();
        mounted.entries.clear();
        match mounted.phone.list(&path) {
            Ok(rows) => {
                for row in rows {
                    let Ok(name) = CString::new(row.name) else {
                        continue;
                    };
                    mounted.names.push(name);
                    mounted.entries.push(IcDirEntry {
                        name: std::ptr::null(),
                        is_dir: row.is_dir as c_int,
                        size: row.size,
                        modified: row.modified,
                        permissions: 0,
                        has_permissions: 0,
                    });
                }
                for (slot, name) in mounted.entries.iter_mut().zip(mounted.names.iter()) {
                    slot.name = name.as_ptr();
                }
                IcListing {
                    items: mounted.entries.as_ptr(),
                    count: mounted.entries.len() as u32,
                }
            }
            Err(reason) => {
                remember(&mounted.failure, reason);
                IcListing::EMPTY
            }
        }
    })
    .unwrap_or(IcListing::EMPTY)
}

pub extern "C" fn read(handle: IcFsHandle, path: *const c_char) -> IcBytes {
    let path = wanted(path);
    held(handle, |mounted| match mounted.phone.read_whole(&path) {
        Ok(body) => {
            mounted.body = body;
            IcBytes {
                data: mounted.body.as_ptr(),
                len: mounted.body.len() as u64,
            }
        }
        Err(reason) => {
            remember(&mounted.failure, reason);
            IcBytes::EMPTY
        }
    })
    .unwrap_or(IcBytes::EMPTY)
}

pub extern "C" fn write(
    handle: IcFsHandle,
    path: *const c_char,
    bytes: *const u8,
    len: u64,
) -> c_int {
    let path = wanted(path);
    let body = if bytes.is_null() || len == 0 {
        &[][..]
    } else {
        unsafe { std::slice::from_raw_parts(bytes, len as usize) }
    };
    let outcome = held(handle, |mounted| mounted.phone.write_whole(&path, body));
    reported(handle, outcome)
}

pub extern "C" fn create_dir(handle: IcFsHandle, path: *const c_char) -> c_int {
    let path = wanted(path);
    let outcome = held(handle, |mounted| mounted.phone.create_dir(&path));
    reported(handle, outcome)
}

pub extern "C" fn remove(handle: IcFsHandle, path: *const c_char) -> c_int {
    let path = wanted(path);
    let outcome = held(handle, |mounted| mounted.phone.remove(&path));
    reported(handle, outcome)
}

pub extern "C" fn rename(handle: IcFsHandle, from: *const c_char, to: *const c_char) -> c_int {
    let (from, to) = (wanted(from), wanted(to));
    let outcome = held(handle, |mounted| mounted.phone.rename(&from, &to));
    reported(handle, outcome)
}

pub extern "C" fn is_read_only(_handle: IcFsHandle) -> c_int {
    0
}

pub extern "C" fn last_error(handle: IcFsHandle) -> *const c_char {
    held(handle, |mounted| {
        let taken = mounted.failure.lock().ok().and_then(|mut slot| slot.take());
        mounted.said = taken.and_then(|text| CString::new(text).ok());
        mounted
            .said
            .as_ref()
            .map(|text| text.as_ptr())
            .unwrap_or(std::ptr::null())
    })
    .unwrap_or(std::ptr::null())
}

pub extern "C" fn stream_open(
    handle: IcFsHandle,
    path: *const c_char,
    mode: u32,
    size: u64,
) -> IcFsStream {
    let path = wanted(path);
    let opened = held(handle, |mounted| {
        let open = match mode {
            IC_OPEN_READ => mounted.phone.open_read(&path).map(Open::Reading),
            IC_OPEN_WRITE => mounted.phone.open_write(&path, size).map(Open::Writing),
            _ => Err("The phone cannot change a file in place.".into()),
        };
        match open {
            Ok(open) => Some(Streaming {
                open: Some(open),
                failure: mounted.failure.clone(),
            }),
            Err(reason) => {
                remember(&mounted.failure, reason);
                None
            }
        }
    });
    match opened.flatten() {
        Some(streaming) => Box::into_raw(Box::new(streaming)) as IcFsStream,
        None => std::ptr::null_mut(),
    }
}

pub extern "C" fn stream_read(stream: IcFsStream, into: *mut u8, len: u64) -> i64 {
    if stream.is_null() || into.is_null() {
        return -1;
    }
    let streaming = unsafe { &mut *(stream as *mut Streaming) };
    let Some(Open::Reading(reading)) = streaming.open.as_mut() else {
        return -1;
    };
    let room = unsafe { std::slice::from_raw_parts_mut(into, len as usize) };
    match reading.read(room) {
        Ok(got) => got as i64,
        Err(reason) => {
            remember(&streaming.failure, reason);
            -1
        }
    }
}

pub extern "C" fn stream_write(stream: IcFsStream, from: *const u8, len: u64) -> i64 {
    if stream.is_null() || from.is_null() {
        return -1;
    }
    let streaming = unsafe { &mut *(stream as *mut Streaming) };
    let Some(Open::Writing(writing)) = streaming.open.as_mut() else {
        return -1;
    };
    let body = unsafe { std::slice::from_raw_parts(from, len as usize) };
    match writing.write(body) {
        Ok(()) => len as i64,
        Err(reason) => {
            remember(&streaming.failure, reason);
            -1
        }
    }
}

pub extern "C" fn stream_close(stream: IcFsStream) -> c_int {
    if stream.is_null() {
        return IC_ERR_IO;
    }
    let mut streaming = unsafe { Box::from_raw(stream as *mut Streaming) };
    match streaming.open.take() {
        Some(Open::Writing(writing)) => match writing.close() {
            Ok(()) => IC_OK,
            Err(reason) => {
                remember(&streaming.failure, reason);
                IC_ERR_IO
            }
        },
        _ => IC_OK,
    }
}

pub extern "C" fn stream_abort(stream: IcFsStream) {
    if !stream.is_null() {
        drop(unsafe { Box::from_raw(stream as *mut Streaming) });
    }
}

extern "C" fn never_opened_inside_a_file(
    _source: ic_plugin_api::IcFsSource,
    _path: *const c_char,
    _user_data: *mut c_void,
) -> IcFsHandle {
    std::ptr::null_mut()
}

pub fn vtable() -> IcFsVTable {
    IcFsVTable {
        struct_size: std::mem::size_of::<IcFsVTable>() as u32,
        open_in: never_opened_inside_a_file,
        close,
        list,
        read,
        is_read_only,
        last_error,
        write: Some(write),
        create_dir: Some(create_dir),
        remove: Some(remove),
        rename: Some(rename),
        shell_open: None,
        shell_read: None,
        shell_write: None,
        shell_resize: None,
        shell_close: None,
        shell_available: None,
        columns: None,
        list_rows: None,
        action_state: None,
        cell_clicked: None,
        set_permissions: None,
        stream_open: Some(stream_open),
        stream_read: Some(stream_read),
        stream_write: Some(stream_write),
        stream_close: Some(stream_close),
        copy_within: None,
        on_disk: None,
        stream_abort: Some(stream_abort),
    }
}

pub fn connection_vtable(fs: *const IcFsVTable) -> IcConnectionVTable {
    IcConnectionVTable {
        struct_size: std::mem::size_of::<IcConnectionVTable>() as u32,
        open,
        fs,
        describe: None,
        on_event: None,
    }
}
