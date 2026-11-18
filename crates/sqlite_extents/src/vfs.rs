// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A file set as a SQLite VFS ([`register`]): its databases' files are the file set's, by the names
//! SQLite opens them by, and their journals and temporary files are memory (see `file_set`).
//!
//! With `PRAGMA mmap_size`, SQLite reads pages in place, in the file set's window, if it has one,
//! with nothing copied (`xFetch`); otherwise, or for pages changed since the last commit, it reads
//! them as it would without.
//!
//! A transaction is committed as SQLite syncs the database's file at its end, or, if told not to
//! sync (`synchronous=OFF`), as it unlocks it. WAL mode is refused: its log would be memory, which
//! a crash loses, and SQLite then keeps to the rollback journal; nor does the VFS give SQLite the
//! shared memory WAL mode needs otherwise. Randomness, sleeping and the time are the default VFS's.

use crate::file_set::{FileSet, MemoryFile};
use crate::locks::Level;
use anyhow::{Error, bail};
use extent_files::{Failed, NotFound};
use rusqlite::ffi;
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::ptr::NonNull;
use std::sync::{Arc, Mutex, Weak};

/// The longest name a file of a file set has.
const MAX_PATHNAME: c_int = 512;

/// What the VFS's functions find in it.
struct App {
    /// The file set, while anything holds it: its registration does, until dropped.
    file_set: Weak<FileSet>,
    /// The default VFS, which randomness, sleeping and the time are of.
    default: *mut ffi::sqlite3_vfs,
}

/// A file set registered as a SQLite VFS, until dropped, which only a file set none of whose files
/// is open may be.
///
/// What SQLite is given of it, the VFS's struct, its name and its app, a few hundred bytes, it
/// keeps as long as the process: a connection SQLite opened on the VFS with none of its files open,
/// an in-memory database's, keeps calling the VFS once unregistered, as its clock (TASK-444.14).
/// Its file set it holds only while the registration does, or a file open of it: a connection's
/// opens of files once the file set went are refused.
pub struct Registration {
    vfs: NonNull<ffi::sqlite3_vfs>,
    file_set: Arc<FileSet>,
}

// SAFETY: `vfs` points to a struct of the process's life that nothing changes once registered,
// SQLite's lock aside, which `sqlite3_vfs_unregister` takes: any thread may unregister it.
unsafe impl Send for Registration {}
// SAFETY: a shared registration gives nothing of `vfs` to any thread.
unsafe impl Sync for Registration {}

impl Drop for Registration {
    fn drop(&mut self) {
        let opens = self.file_set.opens();
        assert_eq!(opens, 0, "unregistering a file set whose files are open");
        // SAFETY: registered by `register`, and of the process's life; a connection still
        // holding it keeps calling it, which it then still serves.
        unsafe { ffi::sqlite3_vfs_unregister(self.vfs.as_ptr()) };
    }
}

/// Registers `file_set` as the SQLite VFS `name`, which no other is, not as the default: a
/// connection opens databases on it as `rusqlite::Connection::open_with_flags_and_vfs` does.
pub fn register(name: &str, file_set: Arc<FileSet>) -> Result<Registration, Error> {
    let name = CString::new(name)?;
    // SAFETY: what SQLite asks before registering.
    let initialized = unsafe { ffi::sqlite3_initialize() };
    if initialized != ffi::SQLITE_OK {
        bail!("initializing SQLite: {initialized}");
    }
    // SAFETY: a lookup of the default VFS, SQLite initialized.
    let default = unsafe { ffi::sqlite3_vfs_find(std::ptr::null()) };
    // SAFETY: a lookup of `name`'s, a C string, which must not be registered.
    if unsafe { !ffi::sqlite3_vfs_find(name.as_ptr()).is_null() } {
        bail!("a SQLite VFS named {name:?} is registered already");
    }
    // The process's, as a connection may keep calling the VFS after it is unregistered.
    let app: &'static App =
        Box::leak(Box::new(App { file_set: Arc::downgrade(&file_set), default }));
    let name: &'static CStr = Box::leak(name.into_boxed_c_str());
    let vfs = Box::leak(Box::new(ffi::sqlite3_vfs {
        iVersion: 2,
        szOsFile: size_of::<Handle>() as c_int,
        mxPathname: MAX_PATHNAME,
        pNext: std::ptr::null_mut(),
        zName: name.as_ptr(),
        pAppData: app as *const App as *mut c_void,
        xOpen: Some(x_open),
        xDelete: Some(x_delete),
        xAccess: Some(x_access),
        xFullPathname: Some(x_full_pathname),
        xDlOpen: None,
        xDlError: None,
        xDlSym: None,
        xDlClose: None,
        xRandomness: Some(x_randomness),
        xSleep: Some(x_sleep),
        xCurrentTime: Some(x_current_time),
        xGetLastError: Some(x_get_last_error),
        xCurrentTimeInt64: Some(x_current_time_int64),
        xSetSystemCall: None,
        xGetSystemCall: None,
        xNextSystemCall: None,
    }));
    // SAFETY: `vfs`, its name and its app are the process's, unchanged once registered.
    let registered = unsafe { ffi::sqlite3_vfs_register(vfs, 0) };
    if registered != ffi::SQLITE_OK {
        bail!("registering the SQLite VFS {name:?}: {registered}");
    }
    Ok(Registration { vfs: NonNull::from(vfs), file_set })
}

/// What a connection has open.
enum Open {
    Database { file_set: Arc<FileSet>, name: String, lock: Level },
    Memory(MemoryFile),
}

/// A file SQLite opened on the VFS: its `sqlite3_file`, then what it is.
#[repr(C)]
struct Handle {
    base: ffi::sqlite3_file,
    open: Open,
}

static METHODS: ffi::sqlite3_io_methods = ffi::sqlite3_io_methods {
    // 3, for memory-mapped reads (`xFetch`); without shared memory, which WAL mode would need.
    iVersion: 3,
    xClose: Some(x_close),
    xRead: Some(x_read),
    xWrite: Some(x_write),
    xTruncate: Some(x_truncate),
    xSync: Some(x_sync),
    xFileSize: Some(x_file_size),
    xLock: Some(x_lock),
    xUnlock: Some(x_unlock),
    xCheckReservedLock: Some(x_check_reserved_lock),
    xFileControl: Some(x_file_control),
    xSectorSize: Some(x_sector_size),
    xDeviceCharacteristics: Some(x_device_characteristics),
    xShmMap: None,
    xShmLock: None,
    xShmBarrier: None,
    xShmUnmap: None,
    xFetch: Some(x_fetch),
    xUnfetch: Some(x_unfetch),
};

/// The VFS's app.
///
/// # Safety
///
/// `vfs` must be one [`register`] made.
unsafe fn app<'a>(vfs: *mut ffi::sqlite3_vfs) -> &'a App {
    // SAFETY: register set pAppData to an App of the process's life, which nothing changes.
    unsafe { &*((*vfs).pAppData as *const App) }
}

/// What `file` has open.
///
/// # Safety
///
/// `file` must be one `x_open` opened, not closed.
unsafe fn open<'a>(file: *mut ffi::sqlite3_file) -> &'a mut Open {
    // SAFETY: x_open made `file` a Handle; SQLite calls one file's methods one at a time, from the
    // connection holding it (its mutex, in serialized mode), so the reference is the only one.
    unsafe { &mut (*(file as *mut Handle)).open }
}

/// `name`, a C string SQLite passes.
///
/// # Safety
///
/// `name` must be a C string.
unsafe fn name_of(name: *const c_char) -> Option<String> {
    match name.is_null() {
        true => None,
        // SAFETY: a C string, as the caller's contract says, not null.
        false => Some(unsafe { CStr::from_ptr(name) }.to_string_lossy().into_owned()),
    }
}

/// The SQLite result of `result`: `code` if it failed, logged.
fn code_of(result: Result<(), Error>, code: c_int) -> c_int {
    match result {
        Ok(()) => ffi::SQLITE_OK,
        Err(error) => {
            log::error!("sqlite_extents: {error:#}");
            match error.is::<NotFound>() {
                true => ffi::SQLITE_CANTOPEN,
                false if error.is::<Failed>() => ffi::SQLITE_IOERR,
                false => code,
            }
        }
    }
}

unsafe extern "C" fn x_open(
    vfs: *mut ffi::sqlite3_vfs,
    name: ffi::sqlite3_filename,
    file: *mut ffi::sqlite3_file,
    flags: c_int,
    out_flags: *mut c_int,
) -> c_int {
    // SAFETY: as SQLite calls it.
    let (app, name) = unsafe { (app(vfs), name_of(name)) };
    // SAFETY: `file`, room of szOsFile bytes SQLite gives x_open: no methods until it opens.
    unsafe { (*file).pMethods = std::ptr::null() };
    let opened = match name {
        _ if flags & ffi::SQLITE_OPEN_WAL != 0 => {
            log::warn!("sqlite_extents: WAL mode is refused: its log would be lost in a crash");
            return ffi::SQLITE_CANTOPEN;
        }
        Some(name) if flags & ffi::SQLITE_OPEN_MAIN_DB != 0 => {
            let create = flags & ffi::SQLITE_OPEN_CREATE != 0;
            let Some(file_set) = app.file_set.upgrade() else { return ffi::SQLITE_CANTOPEN };
            file_set.open_file(&name, create).map(|()| Open::Database {
                file_set,
                name,
                lock: Level::None,
            })
        }
        Some(name) if flags & ffi::SQLITE_OPEN_DELETEONCLOSE == 0 => {
            let create = flags & ffi::SQLITE_OPEN_CREATE != 0;
            let Some(file_set) = app.file_set.upgrade() else { return ffi::SQLITE_CANTOPEN };
            file_set.memory_file(&name, create).map(Open::Memory)
        }
        _ => Ok(Open::Memory(Arc::new(Mutex::new(Vec::new())))),
    };
    match opened {
        Ok(opened) => {
            // SAFETY: SQLite gives `szOsFile` bytes, a handle's.
            unsafe {
                std::ptr::write(
                    file as *mut Handle,
                    Handle { base: ffi::sqlite3_file { pMethods: &METHODS }, open: opened },
                );
                if !out_flags.is_null() {
                    *out_flags = flags;
                }
            }
            ffi::SQLITE_OK
        }
        Err(error) => code_of(Err(error), ffi::SQLITE_CANTOPEN),
    }
}

unsafe extern "C" fn x_delete(
    vfs: *mut ffi::sqlite3_vfs,
    name: *const c_char,
    _sync_dir: c_int,
) -> c_int {
    // SAFETY: as SQLite calls it.
    let (app, name) = unsafe { (app(vfs), name_of(name).unwrap_or_default()) };
    let Some(file_set) = app.file_set.upgrade() else { return ffi::SQLITE_IOERR_DELETE };
    match file_set.delete(&name) {
        Ok(true) => ffi::SQLITE_OK,
        Ok(false) => ffi::SQLITE_IOERR_DELETE_NOENT,
        Err(error) => code_of(Err(error), ffi::SQLITE_IOERR_DELETE),
    }
}

unsafe extern "C" fn x_access(
    vfs: *mut ffi::sqlite3_vfs,
    name: *const c_char,
    _flags: c_int,
    out: *mut c_int,
) -> c_int {
    // SAFETY: as SQLite calls it.
    let (app, name) = unsafe { (app(vfs), name_of(name).unwrap_or_default()) };
    let Some(file_set) = app.file_set.upgrade() else { return ffi::SQLITE_IOERR_ACCESS };
    match file_set.exists(&name) {
        Ok(exists) => {
            // SAFETY: as SQLite calls it: `out` is its int.
            unsafe { *out = exists as c_int };
            ffi::SQLITE_OK
        }
        Err(error) => code_of(Err(error), ffi::SQLITE_IOERR_ACCESS),
    }
}

unsafe extern "C" fn x_full_pathname(
    _vfs: *mut ffi::sqlite3_vfs,
    name: *const c_char,
    size: c_int,
    out: *mut c_char,
) -> c_int {
    // SAFETY: as SQLite calls it: `out` holds `size` bytes.
    let name = unsafe { CStr::from_ptr(name) }.to_bytes_with_nul();
    if name.len() > size as usize {
        return ffi::SQLITE_CANTOPEN;
    }
    // SAFETY: `name.len()` bytes, no more than `out`'s `size`; SQLite's buffer and the name don't
    // overlap.
    unsafe { std::ptr::copy_nonoverlapping(name.as_ptr() as *const c_char, out, name.len()) };
    ffi::SQLITE_OK
}

unsafe extern "C" fn x_randomness(
    vfs: *mut ffi::sqlite3_vfs,
    len: c_int,
    out: *mut c_char,
) -> c_int {
    // SAFETY: the default VFS's, as SQLite calls it.
    unsafe {
        let default = app(vfs).default;
        (*default).xRandomness.unwrap()(default, len, out)
    }
}

unsafe extern "C" fn x_sleep(vfs: *mut ffi::sqlite3_vfs, micros: c_int) -> c_int {
    // SAFETY: the default VFS's, as SQLite calls it.
    unsafe {
        let default = app(vfs).default;
        (*default).xSleep.unwrap()(default, micros)
    }
}

unsafe extern "C" fn x_current_time(vfs: *mut ffi::sqlite3_vfs, out: *mut f64) -> c_int {
    // SAFETY: the default VFS's, as SQLite calls it.
    unsafe {
        let default = app(vfs).default;
        (*default).xCurrentTime.unwrap()(default, out)
    }
}

unsafe extern "C" fn x_current_time_int64(
    vfs: *mut ffi::sqlite3_vfs,
    out: *mut ffi::sqlite3_int64,
) -> c_int {
    // SAFETY: the default VFS's, as SQLite calls it.
    unsafe {
        let default = app(vfs).default;
        (*default).xCurrentTimeInt64.unwrap()(default, out)
    }
}

unsafe extern "C" fn x_get_last_error(
    vfs: *mut ffi::sqlite3_vfs,
    len: c_int,
    out: *mut c_char,
) -> c_int {
    // SAFETY: the default VFS's, as SQLite calls it.
    unsafe {
        let default = app(vfs).default;
        (*default).xGetLastError.unwrap()(default, len, out)
    }
}

unsafe extern "C" fn x_close(file: *mut ffi::sqlite3_file) -> c_int {
    // SAFETY: SQLite closes what it opened once; its memory is SQLite's after.
    let handle = unsafe { std::ptr::read(file as *mut Handle) };
    if let Open::Database { file_set, name, lock } = handle.open {
        let mut lock = lock;
        let unlocked = file_set.unlock_file(&name, &mut lock, Level::None);
        file_set.close_file(&name);
        return code_of(unlocked, ffi::SQLITE_IOERR_CLOSE);
    }
    ffi::SQLITE_OK
}

unsafe extern "C" fn x_read(
    file: *mut ffi::sqlite3_file,
    buf: *mut c_void,
    len: c_int,
    offset: ffi::sqlite3_int64,
) -> c_int {
    // SAFETY: as SQLite calls it, `buf` holds `len` bytes, its own, which no other thread touches
    // meanwhile, and which may be uninitialized, as a page its cache just allocated: zeroed first,
    // as a slice's bytes must be initialized (TASK-444.4).
    let buf = unsafe {
        std::ptr::write_bytes(buf as *mut u8, 0, len as usize);
        std::slice::from_raw_parts_mut(buf as *mut u8, len as usize)
    };
    // SAFETY: as SQLite calls it: a file x_open opened.
    let read = match unsafe { open(file) } {
        Open::Database { file_set, name, .. } => file_set.read(name, offset as u64, buf),
        Open::Memory(data) => {
            let data = data.lock().unwrap();
            let start = (offset as usize).min(data.len());
            let read = (data.len() - start).min(buf.len());
            buf[..read].copy_from_slice(&data[start..start + read]);
            Ok(read)
        }
    };
    match read {
        Ok(read) if read == buf.len() => ffi::SQLITE_OK,
        Ok(read) => {
            buf[read..].fill(0);
            ffi::SQLITE_IOERR_SHORT_READ
        }
        Err(error) => code_of(Err(error), ffi::SQLITE_IOERR_READ),
    }
}

unsafe extern "C" fn x_write(
    file: *mut ffi::sqlite3_file,
    data: *const c_void,
    len: c_int,
    offset: ffi::sqlite3_int64,
) -> c_int {
    // SAFETY: as SQLite calls it: `data` holds `len` bytes.
    let data = unsafe { std::slice::from_raw_parts(data as *const u8, len as usize) };
    // SAFETY: as SQLite calls it: a file x_open opened.
    let written = match unsafe { open(file) } {
        Open::Database { file_set, name, .. } => file_set.write(name, offset as u64, data),
        Open::Memory(file) => {
            let mut file = file.lock().unwrap();
            let end = offset as usize + data.len();
            if file.len() < end {
                file.resize(end, 0);
            }
            file[offset as usize..end].copy_from_slice(data);
            Ok(())
        }
    };
    code_of(written, ffi::SQLITE_IOERR_WRITE)
}

unsafe extern "C" fn x_truncate(file: *mut ffi::sqlite3_file, size: ffi::sqlite3_int64) -> c_int {
    // SAFETY: as SQLite calls it.
    let truncated = match unsafe { open(file) } {
        Open::Database { file_set, name, .. } => file_set.truncate(name, size as u64),
        Open::Memory(file) => {
            file.lock().unwrap().truncate(size as usize);
            Ok(())
        }
    };
    code_of(truncated, ffi::SQLITE_IOERR_TRUNCATE)
}

unsafe extern "C" fn x_sync(file: *mut ffi::sqlite3_file, _flags: c_int) -> c_int {
    // SAFETY: as SQLite calls it.
    match unsafe { open(file) } {
        Open::Database { file_set, name, .. } => {
            code_of(file_set.sync(name), ffi::SQLITE_IOERR_FSYNC)
        }
        Open::Memory(_) => ffi::SQLITE_OK,
    }
}

unsafe extern "C" fn x_file_size(
    file: *mut ffi::sqlite3_file,
    out: *mut ffi::sqlite3_int64,
) -> c_int {
    // SAFETY: as SQLite calls it.
    let size = match unsafe { open(file) } {
        Open::Database { file_set, name, .. } => file_set.size(name),
        Open::Memory(file) => Ok(file.lock().unwrap().len() as u64),
    };
    match size {
        Ok(size) => {
            // SAFETY: as SQLite calls it: `out` is its integer.
            unsafe { *out = size as ffi::sqlite3_int64 };
            ffi::SQLITE_OK
        }
        Err(error) => code_of(Err(error), ffi::SQLITE_IOERR_FSTAT),
    }
}

unsafe extern "C" fn x_lock(file: *mut ffi::sqlite3_file, level: c_int) -> c_int {
    let want = Level::from_sqlite(level).expect("a lock level");
    // SAFETY: as SQLite calls it.
    match unsafe { open(file) } {
        Open::Database { file_set, name, lock } => match file_set.lock_file(name, lock, want) {
            Ok(true) => ffi::SQLITE_OK,
            Ok(false) => ffi::SQLITE_BUSY,
            Err(error) => code_of(Err(error), ffi::SQLITE_IOERR_LOCK),
        },
        Open::Memory(_) => ffi::SQLITE_OK,
    }
}

unsafe extern "C" fn x_unlock(file: *mut ffi::sqlite3_file, level: c_int) -> c_int {
    let want = Level::from_sqlite(level).expect("a lock level");
    // SAFETY: as SQLite calls it.
    match unsafe { open(file) } {
        Open::Database { file_set, name, lock } => {
            code_of(file_set.unlock_file(name, lock, want), ffi::SQLITE_IOERR_UNLOCK)
        }
        Open::Memory(_) => ffi::SQLITE_OK,
    }
}

unsafe extern "C" fn x_check_reserved_lock(file: *mut ffi::sqlite3_file, out: *mut c_int) -> c_int {
    // SAFETY: as SQLite calls it.
    let reserved = match unsafe { open(file) } {
        Open::Database { file_set, name, .. } => file_set.reserved(name),
        Open::Memory(_) => Ok(false),
    };
    match reserved {
        Ok(reserved) => {
            // SAFETY: as SQLite calls it: `out` is its int.
            unsafe { *out = reserved as c_int };
            ffi::SQLITE_OK
        }
        Err(error) => code_of(Err(error), ffi::SQLITE_IOERR_CHECKRESERVEDLOCK),
    }
}

unsafe extern "C" fn x_file_control(
    _file: *mut ffi::sqlite3_file,
    _op: c_int,
    _arg: *mut c_void,
) -> c_int {
    ffi::SQLITE_NOTFOUND
}

unsafe extern "C" fn x_sector_size(file: *mut ffi::sqlite3_file) -> c_int {
    // SAFETY: as SQLite calls it.
    match unsafe { open(file) } {
        Open::Database { file_set, .. } => file_set.device().block_size() as c_int,
        Open::Memory(_) => 512,
    }
}

unsafe extern "C" fn x_device_characteristics(_file: *mut ffi::sqlite3_file) -> c_int {
    // A write damages nothing around it, and none is durable before the next, nor reordered:
    // nothing is durable until the commit.
    ffi::SQLITE_IOCAP_POWERSAFE_OVERWRITE
        | ffi::SQLITE_IOCAP_SAFE_APPEND
        | ffi::SQLITE_IOCAP_SEQUENTIAL
}

unsafe extern "C" fn x_fetch(
    file: *mut ffi::sqlite3_file,
    offset: ffi::sqlite3_int64,
    len: c_int,
    out: *mut *mut c_void,
) -> c_int {
    // SAFETY: as SQLite calls it.
    let fetched = match unsafe { open(file) } {
        Open::Database { file_set, name, .. } => file_set.fetch(name, offset as u64, len as u64),
        Open::Memory(_) => Ok(None),
    };
    match fetched {
        Ok(fetched) => {
            // Read-only, as SQLite reads what it fetches, and copies what it changes.
            let at = fetched.map_or(std::ptr::null_mut(), |at| at as *mut c_void);
            // SAFETY: as SQLite calls it: `out` is its pointer.
            unsafe { *out = at };
            ffi::SQLITE_OK
        }
        Err(error) => {
            // SAFETY: as SQLite calls it: `out` is its pointer.
            unsafe { *out = std::ptr::null_mut() };
            code_of(Err(error), ffi::SQLITE_IOERR_MMAP)
        }
    }
}

unsafe extern "C" fn x_unfetch(
    file: *mut ffi::sqlite3_file,
    _offset: ffi::sqlite3_int64,
    page: *mut c_void,
) -> c_int {
    // SAFETY: as SQLite calls it. A page of none is SQLite asking for its mapping to go, which the
    // window keeps in step with the file whatever it holds.
    if let (Open::Database { file_set, name, .. }, false) = (unsafe { open(file) }, page.is_null())
    {
        file_set.unfetch(name);
    }
    ffi::SQLITE_OK
}
