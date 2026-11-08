use bytes::Bytes;
use futures::executor::block_on;
use futures::SinkExt;
use mtp_rs::{
    ByteRange, Error, FileDownload, MtpDevice, NewObjectInfo, ObjectHandle, ObjectInfo, Storage,
    UploadError, UtcOffset, DEFAULT_CANCEL_TIMEOUT,
};
use std::collections::HashMap;
use std::io::{Read, Seek, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Android's first data read is 16 KiB and a short packet inside it does not end the transfer.
const FIRST_BLOCK: u64 = 1 << 20;
const CHUNK: usize = 1 << 20;

pub struct Target {
    pub serial: Option<String>,
    pub location: u64,
}

pub struct Entry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub modified: u64,
}

struct Session {
    device: MtpDevice,
    storages: Vec<(String, Arc<Storage>)>,
    listings: HashMap<String, Vec<ObjectInfo>>,
}

pub struct Phone {
    target: Target,
    session: Mutex<Option<Session>>,
    reading: AtomicUsize,
}

struct Failure {
    said: String,
    broken: bool,
}

impl From<Error> for Failure {
    fn from(e: Error) -> Self {
        Failure {
            broken: unusable(&e),
            said: describe(&e),
        }
    }
}

impl From<String> for Failure {
    fn from(said: String) -> Self {
        Failure {
            said,
            broken: false,
        }
    }
}

impl From<&str> for Failure {
    fn from(said: &str) -> Self {
        said.to_string().into()
    }
}

/// After these the session must not be used again: the next command could panic in nusb.
fn unusable(e: &Error) -> bool {
    matches!(
        e,
        Error::Timeout | Error::DeviceReset | Error::Disconnected | Error::Io { .. }
    )
}

fn describe(e: &Error) -> String {
    match e {
        Error::Timeout => "The phone did not answer: unlock it and try again.".into(),
        Error::NoDevice | Error::Disconnected => {
            "The phone is not connected, or not in File transfer mode.".into()
        }
        other => other.to_string(),
    }
}

/// Android's shared storage is case-insensitive while its MTP object table is not.
fn same_name(a: &str, b: &str) -> bool {
    a == b || a.to_lowercase() == b.to_lowercase()
}

fn find<'a>(objects: &'a [ObjectInfo], name: &str) -> Option<&'a ObjectInfo> {
    objects
        .iter()
        .find(|o| o.filename == name)
        .or_else(|| objects.iter().find(|o| same_name(&o.filename, name)))
}

fn segments(path: &str) -> Vec<String> {
    path.split('/')
        .filter(|s| !s.is_empty() && *s != ".")
        .map(str::to_string)
        .collect()
}

fn local_offset() -> UtcOffset {
    let minutes = chrono::Local::now().offset().local_minus_utc() / 60;
    UtcOffset::from_minutes(minutes as i16).unwrap_or(UtcOffset::UTC)
}

#[cfg(target_os = "linux")]
fn info_of(target: &Target) -> Option<mtp_rs::MtpDeviceInfo> {
    MtpDevice::list_devices()
        .ok()?
        .into_iter()
        .find(|d| match &target.serial {
            Some(serial) => d.serial_number.as_deref() == Some(serial),
            None => d.location_id == target.location,
        })
}

fn busy(target: &Target) -> String {
    #[cfg(target_os = "linux")]
    if let Some(who) = info_of(target).and_then(|info| crate::holder::who_holds(&info)) {
        return format!("The phone is busy: it is held by {who}.");
    }
    #[cfg(not(target_os = "linux"))]
    let _ = target;
    "The phone is busy: another program holds it.".into()
}

fn open_once(target: &Target) -> Result<MtpDevice, Error> {
    block_on(async {
        match &target.serial {
            Some(serial) => MtpDevice::builder().open_by_serial(serial).await,
            None => MtpDevice::builder().open_by_location(target.location).await,
        }
    })
}

fn connect(target: &Target) -> Result<Session, Failure> {
    let started = Instant::now();
    let mut retried = false;
    let device = loop {
        match open_once(target) {
            Ok(device) => break device,
            // The desktop may be grabbing a phone that has just arrived.
            Err(e) if e.is_exclusive_access() && started.elapsed() < Duration::from_secs(3) => {
                std::thread::sleep(Duration::from_millis(500))
            }
            Err(e) if e.is_exclusive_access() => return Err(busy(target).into()),
            Err(e) if e.is_permission_denied() => {
                return Err("The system refused access to the phone's USB device.".into())
            }
            // A host that died mid-upload leaves the phone taking OpenSession as file data.
            Err(Error::Other { .. }) if !retried => {
                retried = true;
                std::thread::sleep(Duration::from_millis(300))
            }
            Err(e) => return Err(e.into()),
        }
    };
    let found = match block_on(device.storages()) {
        Ok(found) => found,
        Err(e) => {
            let failure = Failure::from(e);
            if !failure.broken {
                let _ = block_on(device.close());
            }
            return Err(failure);
        }
    };
    if found.is_empty() {
        let _ = block_on(device.close());
        return Err("The phone shows no storage: unlock it and allow access to its files.".into());
    }
    let mut storages: Vec<(String, Arc<Storage>)> = Vec::new();
    for storage in found {
        let described = storage.info().description.trim().replace('/', "_");
        let mut name = if described.is_empty() {
            format!("Storage {}", storages.len() + 1)
        } else {
            described
        };
        if storages.iter().any(|(taken, _)| *taken == name) {
            name = format!("{name} ({})", storage.id().0);
        }
        storages.push((name, Arc::new(storage)));
    }
    Ok(Session {
        device,
        storages,
        listings: HashMap::new(),
    })
}

impl Session {
    fn storage(&self, name: &str) -> Result<Arc<Storage>, Failure> {
        self.storages
            .iter()
            .find(|(known, _)| known == name)
            .map(|(_, storage)| storage.clone())
            .ok_or_else(|| format!("{name} is not on the phone").into())
    }

    fn listing(
        &mut self,
        storage: &Storage,
        key: &str,
        handle: Option<ObjectHandle>,
        fresh: bool,
    ) -> Result<Vec<ObjectInfo>, Failure> {
        if !fresh {
            if let Some(known) = self.listings.get(key) {
                return Ok(known.clone());
            }
        }
        let objects = block_on(storage.list_objects(handle))?;
        self.listings.insert(key.to_string(), objects.clone());
        Ok(objects)
    }

    fn folder(&mut self, segs: &[String]) -> Result<(Arc<Storage>, Option<ObjectHandle>), Failure> {
        let Some(first) = segs.first() else {
            return Err("This is the list of the phone's storages, not a folder.".into());
        };
        let storage = self.storage(first)?;
        let mut parent = None;
        let mut key = first.clone();
        for segment in &segs[1..] {
            let found = self.child(&storage, &key, parent, segment)?;
            if !found.is_folder() {
                return Err(format!("{key}/{segment} is not a folder").into());
            }
            parent = Some(found.handle);
            key = format!("{key}/{}", found.filename);
        }
        Ok((storage, parent))
    }

    fn child(
        &mut self,
        storage: &Storage,
        key: &str,
        parent: Option<ObjectHandle>,
        name: &str,
    ) -> Result<ObjectInfo, Failure> {
        let known = self.listing(storage, key, parent, false)?;
        if let Some(found) = find(&known, name) {
            return Ok(found.clone());
        }
        let fresh = self.listing(storage, key, parent, true)?;
        find(&fresh, name)
            .cloned()
            .ok_or_else(|| format!("{key}/{name} is not on the phone").into())
    }

    fn item(&mut self, segs: &[String]) -> Result<(Arc<Storage>, ObjectInfo), Failure> {
        if segs.len() < 2 {
            return Err("A storage is not a file.".into());
        }
        let (parent_segs, name) = segs.split_at(segs.len() - 1);
        let (storage, parent) = self.folder(parent_segs)?;
        let found = self.child(&storage, &parent_segs.join("/"), parent, &name[0])?;
        Ok((storage, found))
    }
}

impl Phone {
    pub fn new(target: Target) -> Self {
        Phone {
            target,
            session: Mutex::new(None),
            reading: AtomicUsize::new(0),
        }
    }

    fn slot(&self) -> MutexGuard<'_, Option<Session>> {
        self.session
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn with<T>(&self, work: impl FnOnce(&mut Session) -> Result<T, Failure>) -> Result<T, String> {
        let mut slot = self.slot();
        if slot.is_none() {
            *slot = Some(connect(&self.target).map_err(|f| f.said)?);
        }
        let Some(session) = slot.as_mut() else {
            return Err("The phone is not open.".into());
        };
        match work(session) {
            Ok(done) => Ok(done),
            Err(failure) => {
                if failure.broken {
                    *slot = None;
                }
                Err(failure.said)
            }
        }
    }

    fn forget_session(&self) {
        *self.slot() = None;
    }

    fn forget_listings(&self) {
        if let Some(session) = self.slot().as_mut() {
            session.listings.clear();
        }
    }

    pub fn close(&self) {
        if let Some(session) = self.slot().take() {
            let _ = block_on(session.device.close());
        }
    }

    pub fn list(&self, path: &str) -> Result<Vec<Entry>, String> {
        let segs = segments(path);
        self.with(|s| {
            if segs.is_empty() {
                return Ok(s
                    .storages
                    .iter()
                    .map(|(name, _)| Entry {
                        name: name.clone(),
                        is_dir: true,
                        size: 0,
                        modified: 0,
                    })
                    .collect());
            }
            let (storage, handle) = s.folder(&segs)?;
            let objects = s.listing(&storage, &segs.join("/"), handle, true)?;
            let offset = local_offset();
            Ok(objects
                .iter()
                .map(|o| Entry {
                    name: o.filename.clone(),
                    is_dir: o.is_folder(),
                    size: if o.is_folder() { 0 } else { o.size },
                    modified: o
                        .modified
                        .and_then(|d| d.to_unix_seconds_with_fallback(offset))
                        .map(|secs| secs.max(0) as u64)
                        .unwrap_or(0),
                })
                .collect())
        })
    }

    pub fn open_read(self: &Arc<Self>, path: &str) -> Result<Reading, String> {
        let segs = segments(path);
        let download = self.with(|s| {
            let (storage, found) = s.item(&segs)?;
            if found.is_folder() {
                return Err(format!("{path} is a folder").into());
            }
            Ok(block_on(storage.download(found.handle, ByteRange::Full))?)
        })?;
        self.reading.fetch_add(1, Ordering::SeqCst);
        Ok(Reading {
            phone: self.clone(),
            download,
            held: Bytes::new(),
        })
    }

    pub fn read_whole(self: &Arc<Self>, path: &str) -> Result<Vec<u8>, String> {
        let mut reading = self.open_read(path)?;
        let mut body = Vec::new();
        let mut room = vec![0u8; CHUNK];
        loop {
            match reading.read(&mut room)? {
                0 => return Ok(body),
                got => body.extend_from_slice(&room[..got]),
            }
        }
    }

    pub fn open_write(self: &Arc<Self>, path: &str, size: u64) -> Result<Writing, String> {
        let segs = segments(path);
        let spool = size == 0 || self.reading.load(Ordering::SeqCst) > 0;
        self.with(|s| {
            if segs.len() < 2 {
                return Err("Files go inside one of the phone's storages.".into());
            }
            let (parent_segs, name) = segs.split_at(segs.len() - 1);
            let name = name[0].clone();
            let (storage, parent) = s.folder(parent_segs)?;
            let siblings = s.listing(&storage, &parent_segs.join("/"), parent, true)?;
            let exact = siblings.iter().find(|o| o.filename == name);
            let replacing = match exact {
                Some(o) if o.is_folder() => return Err(format!("{path} is a folder").into()),
                Some(o) => Some(o.handle),
                None => {
                    if let Some(o) = siblings.iter().find(|o| same_name(&o.filename, &name)) {
                        return Err(format!(
                            "“{name}” would replace “{}”: the phone does not tell names apart by case.",
                            o.filename
                        )
                        .into());
                    }
                    None
                }
            };
            let temp = format!(".~tmp~{name}");
            let temp_fits = temp.encode_utf16().count() <= mtp_rs::MAX_NAME_UNITS && temp.len() <= 255;
            let via_temp = replacing.is_some() && s.device.supports_rename() && temp_fits;
            for stale in siblings.iter().filter(|o| same_name(&o.filename, &temp)) {
                block_on(storage.delete(stale.handle))?;
            }
            if let (Some(old), false) = (replacing, via_temp) {
                block_on(storage.delete(old))?;
            }
            s.listings.clear();
            let mode = if spool {
                Mode::Spooling(tempfile::tempfile().map_err(|e| e.to_string())?)
            } else {
                Mode::Gathering(Vec::with_capacity(size.min(FIRST_BLOCK) as usize))
            };
            Ok(Writing {
                phone: self.clone(),
                storage,
                parent,
                upload_name: if via_temp { temp } else { name.clone() },
                name,
                replacing: replacing.filter(|_| via_temp),
                size,
                written: 0,
                mode,
            })
        })
    }

    pub fn write_whole(self: &Arc<Self>, path: &str, body: &[u8]) -> Result<(), String> {
        let mut writing = self.open_write(path, body.len() as u64)?;
        writing.write(body)?;
        writing.close()
    }

    pub fn create_dir(&self, path: &str) -> Result<(), String> {
        let segs = segments(path);
        self.with(|s| {
            if segs.len() < 2 {
                return Err("Folders go inside one of the phone's storages.".into());
            }
            let (parent_segs, name) = segs.split_at(segs.len() - 1);
            let name = &name[0];
            let (storage, parent) = s.folder(parent_segs)?;
            let siblings = s.listing(&storage, &parent_segs.join("/"), parent, true)?;
            match siblings.iter().find(|o| same_name(&o.filename, name)) {
                Some(o) if o.filename == *name && o.is_folder() => return Ok(()),
                Some(o) => return Err(format!("“{}” is already there.", o.filename).into()),
                None => {}
            }
            block_on(storage.create_folder(parent, name))?;
            s.listings.clear();
            Ok(())
        })
    }

    pub fn remove(&self, path: &str) -> Result<(), String> {
        let segs = segments(path);
        self.with(|s| {
            let (storage, found) = s.item(&segs)?;
            block_on(storage.delete(found.handle))?;
            s.listings.clear();
            Ok(())
        })
    }

    pub fn rename(&self, from: &str, to: &str) -> Result<(), String> {
        let (from_segs, to_segs) = (segments(from), segments(to));
        self.with(|s| {
            if to_segs.len() < 2 {
                return Err("A storage cannot be renamed or replaced.".into());
            }
            let (storage, found) = s.item(&from_segs)?;
            let (to_parent_segs, to_name) = to_segs.split_at(to_segs.len() - 1);
            let to_name = &to_name[0];
            let (to_storage, to_parent) = s.folder(to_parent_segs)?;
            let siblings = s.listing(&to_storage, &to_parent_segs.join("/"), to_parent, true)?;
            if let Some(o) = siblings
                .iter()
                .find(|o| o.handle != found.handle && same_name(&o.filename, to_name))
            {
                return Err(format!("“{}” is already there.", o.filename).into());
            }
            s.listings.clear();
            if from_segs[..from_segs.len() - 1] != *to_parent_segs {
                block_on(storage.move_object(
                    found.handle,
                    to_parent.unwrap_or(ObjectHandle::ROOT),
                    Some(to_storage.id()),
                ))?;
            }
            if found.filename != *to_name {
                rename_retrying(&to_storage, found.handle, to_name)?;
            }
            Ok(())
        })
    }
}

/// Android may still list the object just deleted under the new name for a moment.
fn rename_retrying(storage: &Storage, handle: ObjectHandle, name: &str) -> Result<(), Failure> {
    for delay in [50, 200, 500] {
        match block_on(storage.rename(handle, name)) {
            Err(Error::Other { .. }) => std::thread::sleep(Duration::from_millis(delay)),
            other => return Ok(other?),
        }
    }
    Ok(block_on(storage.rename(handle, name))?)
}

pub struct Reading {
    phone: Arc<Phone>,
    download: FileDownload,
    held: Bytes,
}

impl Reading {
    pub fn read(&mut self, into: &mut [u8]) -> Result<usize, String> {
        while self.held.is_empty() {
            match block_on(self.download.next_chunk()) {
                None => return Ok(0),
                Some(Ok(chunk)) => self.held = chunk,
                Some(Err(e)) => {
                    if unusable(&e) {
                        self.phone.forget_session();
                    }
                    return Err(describe(&e));
                }
            }
        }
        let got = into.len().min(self.held.len());
        into[..got].copy_from_slice(&self.held[..got]);
        self.held = self.held.slice(got..);
        Ok(got)
    }
}

impl Drop for Reading {
    fn drop(&mut self) {
        if self.download.bytes_received() < self.download.size() {
            if let Err(e) = block_on(self.download.cancel(DEFAULT_CANCEL_TIMEOUT)) {
                if unusable(&e) {
                    self.phone.forget_session();
                }
            }
        }
        self.phone.reading.fetch_sub(1, Ordering::SeqCst);
    }
}

type Upload = JoinHandle<Result<ObjectHandle, UploadError>>;

enum Mode {
    Gathering(Vec<u8>),
    Sending {
        data: futures::channel::mpsc::Sender<std::io::Result<Bytes>>,
        upload: Upload,
    },
    Spooling(std::fs::File),
    Done,
}

pub struct Writing {
    phone: Arc<Phone>,
    storage: Arc<Storage>,
    parent: Option<ObjectHandle>,
    name: String,
    upload_name: String,
    replacing: Option<ObjectHandle>,
    size: u64,
    written: u64,
    mode: Mode,
}

impl Writing {
    fn start(
        &self,
        size: u64,
        first: Vec<u8>,
    ) -> (
        futures::channel::mpsc::Sender<std::io::Result<Bytes>>,
        Upload,
    ) {
        let (mut data, received) = futures::channel::mpsc::channel(4);
        let storage = self.storage.clone();
        let parent = self.parent;
        let info = NewObjectInfo::file(self.upload_name.clone(), size);
        let upload = std::thread::spawn(move || block_on(storage.upload(parent, info, received)));
        if !first.is_empty() {
            let _ = block_on(data.send(Ok(Bytes::from(first))));
        }
        (data, upload)
    }

    pub fn write(&mut self, body: &[u8]) -> Result<(), String> {
        let after = self.written + body.len() as u64;
        if self.size > 0 && after > self.size {
            return Err("The copy sent more bytes than the file's size.".into());
        }
        match std::mem::replace(&mut self.mode, Mode::Done) {
            Mode::Gathering(mut first) => {
                first.extend_from_slice(body);
                self.mode = if first.len() as u64 >= self.size.min(FIRST_BLOCK) {
                    let (data, upload) = self.start(self.size, first);
                    Mode::Sending { data, upload }
                } else {
                    Mode::Gathering(first)
                };
            }
            Mode::Sending { mut data, upload } => {
                if block_on(data.send(Ok(Bytes::copy_from_slice(body)))).is_err() {
                    drop(data);
                    return Err(self.failed(upload.join()));
                }
                self.mode = Mode::Sending { data, upload };
            }
            Mode::Spooling(mut file) => {
                file.write_all(body).map_err(|e| e.to_string())?;
                self.mode = Mode::Spooling(file);
            }
            Mode::Done => return Err("The file is already closed.".into()),
        }
        self.written = after;
        Ok(())
    }

    pub fn close(mut self) -> Result<(), String> {
        let joined = match std::mem::replace(&mut self.mode, Mode::Done) {
            Mode::Gathering(first) if self.written == self.size => {
                let (data, upload) = self.start(self.size, first);
                drop(data);
                upload.join()
            }
            Mode::Gathering(_) => return Err(self.short()),
            Mode::Sending { data, upload } => {
                drop(data);
                let joined = upload.join();
                if self.written != self.size {
                    self.clean_up(joined);
                    return Err(self.short());
                }
                joined
            }
            Mode::Spooling(mut file) => {
                let size = file
                    .seek(std::io::SeekFrom::End(0))
                    .map_err(|e| e.to_string())?;
                file.rewind().map_err(|e| e.to_string())?;
                self.size = size;
                let (data, upload) = self.start(size, Vec::new());
                spool_into(file, data);
                upload.join()
            }
            Mode::Done => return Err("The file is already closed.".into()),
        };
        let handle = match joined {
            Ok(Ok(handle)) => handle,
            other => return Err(self.failed(other)),
        };
        self.phone.forget_listings();
        let stored = block_on(self.storage.get_object_info(handle)).map_err(|e| describe(&e))?;
        if stored.size != self.size {
            let _ = block_on(self.storage.delete(handle));
            return Err(format!(
                "The phone stored {} bytes instead of {}.",
                stored.size, self.size
            ));
        }
        if let Some(old) = self.replacing {
            block_on(self.storage.delete(old)).map_err(|e| describe(&e))?;
            rename_retrying(&self.storage, handle, &self.name).map_err(|f| {
                format!("{} The new file is kept as “{}”.", f.said, self.upload_name)
            })?;
        }
        Ok(())
    }

    fn short(&self) -> String {
        format!(
            "The copy stopped after {} of {} bytes.",
            self.written, self.size
        )
    }

    fn failed(&self, joined: std::thread::Result<Result<ObjectHandle, UploadError>>) -> String {
        let said = match &joined {
            Ok(Err(e)) => describe(&e.source),
            Ok(Ok(_)) => self.short(),
            Err(_) => "The upload stopped unexpectedly.".into(),
        };
        self.clean_up(joined);
        said
    }

    fn clean_up(&self, joined: std::thread::Result<Result<ObjectHandle, UploadError>>) {
        match joined {
            // 4 GiB and more: Android takes the early end of data as the end and keeps the file.
            Ok(Ok(handle)) => {
                let _ = block_on(self.storage.delete(handle));
            }
            Ok(Err(e)) if unusable(&e.source) => self.phone.forget_session(),
            Ok(Err(e)) => {
                if let Some(partial) = e.partial {
                    let _ = block_on(self.storage.delete(partial));
                }
            }
            Err(_) => {}
        }
        self.phone.forget_listings();
    }
}

/// An early end, not an error, closes the data phase so the phone rejects the object and stays in sync.
fn spool_into(
    mut file: std::fs::File,
    mut data: futures::channel::mpsc::Sender<std::io::Result<Bytes>>,
) {
    loop {
        let mut room = vec![0u8; CHUNK];
        let got = match file.read(&mut room) {
            Ok(0) | Err(_) => return,
            Ok(got) => got,
        };
        room.truncate(got);
        if block_on(data.send(Ok(Bytes::from(room)))).is_err() {
            return;
        }
    }
}

impl Drop for Writing {
    fn drop(&mut self) {
        if let Mode::Sending { data, upload } = std::mem::replace(&mut self.mode, Mode::Done) {
            drop(data);
            self.clean_up(upload.join());
        }
    }
}
