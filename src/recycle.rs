//! Recycle Bin integration through the shell's `IFileOperation`.
//!
//! Compared with the legacy `SHFileOperationW` this gives:
//! * a result per item (via the progress sink), so one bad path does not
//!   fail — or need a retry of — the whole selection;
//! * progress (`UpdateProgress`) and cancellation (the stop flag skips the
//!   items not yet started);
//! * no hard 260-character limit of our own;
//! * **no silent permanent deletion.** `SHFileOperationW` with
//!   `FOF_NOCONFIRMATION` destroyed anything the Recycle Bin could not take
//!   (too large, a drive without a bin) without asking. Here such an item
//!   is skipped with an error before anything happens, and if the shell
//!   still deletes something outright it is reported, never hidden.
//!
//! The operation runs on its own single-threaded-apartment thread, as
//! `IFileOperation` requires, so callers' COM state is never touched.
#![cfg(windows)]

use std::cell::RefCell;
use std::ffi::c_void;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

type Hresult = i32;
type Ptr = *mut c_void;

#[repr(C)]
#[derive(PartialEq, Eq)]
struct Guid(u32, u16, u16, [u8; 8]);

const CLSID_FILE_OPERATION: Guid = Guid(
    0x3AD0_5575,
    0x8857,
    0x4850,
    [0x92, 0x77, 0x11, 0xB8, 0x5B, 0xDB, 0x8E, 0x09],
);
const IID_IFILE_OPERATION: Guid = Guid(
    0x947A_AB5F,
    0x0A5C,
    0x4C13,
    [0xB4, 0xD6, 0x4B, 0xF7, 0x83, 0x6F, 0xC9, 0xF8],
);
const IID_ISHELL_ITEM: Guid = Guid(
    0x4382_6D1E,
    0xE718,
    0x42EE,
    [0xBC, 0x55, 0xA1, 0xE2, 0x61, 0xC3, 0x7B, 0xFE],
);
const IID_IUNKNOWN: Guid = Guid(0, 0, 0, [0xC0, 0, 0, 0, 0, 0, 0, 0x46]);
const IID_IFILE_OPERATION_PROGRESS_SINK: Guid = Guid(
    0x04B0_F1A7,
    0x9490,
    0x44BC,
    [0x96, 0xE1, 0x42, 0x96, 0xA3, 0x12, 0x52, 0xE2],
);

const S_OK: Hresult = 0;
const E_NOINTERFACE: Hresult = 0x8000_4002_u32 as i32;
const E_ABORT: Hresult = 0x8000_4004_u32 as i32;
/// HRESULT_FROM_WIN32(ERROR_CANCELLED): skip this item.
const HR_CANCELLED: Hresult = 0x8007_04C7_u32 as i32;

const COINIT_APARTMENTTHREADED: u32 = 0x2;
const COINIT_DISABLE_OLE1DDE: u32 = 0x4;
const CLSCTX_INPROC_SERVER: u32 = 0x1;
const CLSCTX_LOCAL_SERVER: u32 = 0x4;

const FOF_SILENT: u32 = 0x0004;
const FOF_NOCONFIRMATION: u32 = 0x0010;
const FOF_ALLOWUNDO: u32 = 0x0040;
const FOF_NOCONFIRMMKDIR: u32 = 0x0200;
const FOF_NOERRORUI: u32 = 0x0400;
const FOFX_RECYCLEONDELETE: u32 = 0x0008_0000;

/// TRANSFER_SOURCE_FLAGS: set when the shell will recycle the item.
const TSF_DELETE_RECYCLE_IF_POSSIBLE: u32 = 0x80;
/// SIGDN_FILESYSPATH.
const SIGDN_FILESYSPATH: u32 = 0x8005_8000;

#[link(name = "ole32")]
extern "system" {
    fn CoInitializeEx(reserved: Ptr, flags: u32) -> Hresult;
    fn CoUninitialize();
    fn CoCreateInstance(
        clsid: *const Guid,
        outer: Ptr,
        context: u32,
        iid: *const Guid,
        out: *mut Ptr,
    ) -> Hresult;
    fn CoTaskMemFree(pv: Ptr);
}

#[link(name = "shell32")]
extern "system" {
    fn SHCreateItemFromParsingName(
        path: *const u16,
        bind_ctx: Ptr,
        iid: *const Guid,
        out: *mut Ptr,
    ) -> Hresult;
}

/// The error behind an HRESULT: Win32 codes keep their localized system
/// text, anything else shows the raw value.
fn hresult_error(hr: Hresult) -> io::Error {
    let code = hr as u32;
    if code & 0xFFFF_0000 == 0x8007_0000 {
        io::Error::from_raw_os_error((code & 0xFFFF) as i32)
    } else {
        io::Error::other(format!(
            "Recycle Bin operation failed (shell error {code:#010x})"
        ))
    }
}

fn cancelled() -> io::Error {
    io::Error::new(io::ErrorKind::Interrupted, "cancelled by user")
}

fn check(hr: Hresult) -> io::Result<()> {
    if hr < 0 {
        Err(hresult_error(hr))
    } else {
        Ok(())
    }
}

/// Call COM method `index` of `obj`'s vtable as a function of type `F`.
///
/// # Safety
/// `obj` must be a live COM interface pointer whose vtable has a method of
/// signature `F` at `index`.
unsafe fn method<F: Copy>(obj: Ptr, index: usize) -> F {
    let vtbl = *(obj as *const *const usize);
    let slot = vtbl.add(index);
    std::mem::transmute_copy::<usize, F>(&*slot)
}

/// Owned COM interface pointer, released on drop.
struct Com(Ptr);

impl Drop for Com {
    fn drop(&mut self) {
        // SAFETY: IUnknown::Release is slot 2 of every COM vtable.
        unsafe {
            let release: unsafe extern "system" fn(Ptr) -> u32 = method(self.0, 2);
            release(self.0);
        }
    }
}

/// File-system path of a shell item, lower-cased for matching.
fn item_path(item: Ptr) -> Option<String> {
    if item.is_null() {
        return None;
    }
    let mut raw: *mut u16 = std::ptr::null_mut();
    // SAFETY: IShellItem::GetDisplayName is slot 5; `raw` receives a
    // CoTaskMem string that is freed below.
    unsafe {
        let get_name: unsafe extern "system" fn(Ptr, u32, *mut *mut u16) -> Hresult =
            method(item, 5);
        if get_name(item, SIGDN_FILESYSPATH, &mut raw) < 0 || raw.is_null() {
            return None;
        }
        let len = (0..).take_while(|&i| *raw.add(i) != 0).count();
        let path = String::from_utf16_lossy(std::slice::from_raw_parts(raw, len));
        CoTaskMemFree(raw as Ptr);
        Some(path.to_lowercase())
    }
}

/// Per-item outcome collected by the progress sink.
#[derive(Default)]
struct Outcome {
    recycled: bool,
    error: Option<io::Error>,
}

struct SinkState<'a> {
    /// Lower-cased absolute paths of the requested items.
    roots: Vec<String>,
    outcomes: RefCell<Vec<Outcome>>,
    progress: Option<&'a (dyn Fn(u64, u64) + Sync)>,
}

impl SinkState<'_> {
    /// Index of the requested item that `item` is, or lies inside.
    fn root_of(&self, item: Ptr) -> Option<usize> {
        let path = item_path(item)?;
        self.roots.iter().position(|root| {
            path == *root
                || (path.len() > root.len()
                    && path.starts_with(root.as_str())
                    && path.as_bytes()[root.len()] == b'\\')
        })
    }

    fn fail(&self, index: usize, error: io::Error) {
        let mut outcomes = self.outcomes.borrow_mut();
        if outcomes[index].error.is_none() {
            outcomes[index].error = Some(error);
        }
    }
}

/// `IFileOperationProgressSink` implemented over a `SinkState`. Lives on
/// the operation thread's stack; the shell only calls it on that thread.
#[repr(C)]
struct Sink<'a> {
    vtbl: *const SinkVtbl,
    refs: AtomicU32,
    state: &'a SinkState<'a>,
}

#[repr(C)]
struct SinkVtbl {
    query_interface: unsafe extern "system" fn(Ptr, *const Guid, *mut Ptr) -> Hresult,
    add_ref: unsafe extern "system" fn(Ptr) -> u32,
    release: unsafe extern "system" fn(Ptr) -> u32,
    start_operations: unsafe extern "system" fn(Ptr) -> Hresult,
    finish_operations: unsafe extern "system" fn(Ptr, Hresult) -> Hresult,
    pre_rename: unsafe extern "system" fn(Ptr, u32, Ptr, *const u16) -> Hresult,
    post_rename: unsafe extern "system" fn(Ptr, u32, Ptr, *const u16, Hresult, Ptr) -> Hresult,
    pre_move: unsafe extern "system" fn(Ptr, u32, Ptr, Ptr, *const u16) -> Hresult,
    post_move: unsafe extern "system" fn(Ptr, u32, Ptr, Ptr, *const u16, Hresult, Ptr) -> Hresult,
    pre_copy: unsafe extern "system" fn(Ptr, u32, Ptr, Ptr, *const u16) -> Hresult,
    post_copy: unsafe extern "system" fn(Ptr, u32, Ptr, Ptr, *const u16, Hresult, Ptr) -> Hresult,
    pre_delete: unsafe extern "system" fn(Ptr, u32, Ptr) -> Hresult,
    post_delete: unsafe extern "system" fn(Ptr, u32, Ptr, Hresult, Ptr) -> Hresult,
    pre_new: unsafe extern "system" fn(Ptr, u32, Ptr, *const u16) -> Hresult,
    post_new: unsafe extern "system" fn(
        Ptr,
        u32,
        Ptr,
        *const u16,
        *const u16,
        u32,
        Hresult,
        Ptr,
    ) -> Hresult,
    update_progress: unsafe extern "system" fn(Ptr, u32, u32) -> Hresult,
    reset_timer: unsafe extern "system" fn(Ptr) -> Hresult,
    pause_timer: unsafe extern "system" fn(Ptr) -> Hresult,
    resume_timer: unsafe extern "system" fn(Ptr) -> Hresult,
}

/// # Safety
/// `this` must point to a live `Sink`.
unsafe fn sink<'a>(this: Ptr) -> &'a Sink<'a> {
    &*(this as *const Sink<'a>)
}

unsafe extern "system" fn sink_query_interface(
    this: Ptr,
    iid: *const Guid,
    out: *mut Ptr,
) -> Hresult {
    if *iid == IID_IUNKNOWN || *iid == IID_IFILE_OPERATION_PROGRESS_SINK {
        sink(this).refs.fetch_add(1, Ordering::Relaxed);
        *out = this;
        S_OK
    } else {
        *out = std::ptr::null_mut();
        E_NOINTERFACE
    }
}

unsafe extern "system" fn sink_add_ref(this: Ptr) -> u32 {
    sink(this).refs.fetch_add(1, Ordering::Relaxed) + 1
}

/// The sink is stack-owned: the count is tracked but never frees it.
unsafe extern "system" fn sink_release(this: Ptr) -> u32 {
    sink(this)
        .refs
        .fetch_sub(1, Ordering::Relaxed)
        .saturating_sub(1)
}

unsafe extern "system" fn sink_ok(_: Ptr) -> Hresult {
    S_OK
}
unsafe extern "system" fn sink_finish(_: Ptr, _: Hresult) -> Hresult {
    S_OK
}
unsafe extern "system" fn sink_pre_rename(_: Ptr, _: u32, _: Ptr, _: *const u16) -> Hresult {
    S_OK
}
unsafe extern "system" fn sink_post_rename(
    _: Ptr,
    _: u32,
    _: Ptr,
    _: *const u16,
    _: Hresult,
    _: Ptr,
) -> Hresult {
    S_OK
}
unsafe extern "system" fn sink_pre_transfer(
    _: Ptr,
    _: u32,
    _: Ptr,
    _: Ptr,
    _: *const u16,
) -> Hresult {
    S_OK
}
unsafe extern "system" fn sink_post_transfer(
    _: Ptr,
    _: u32,
    _: Ptr,
    _: Ptr,
    _: *const u16,
    _: Hresult,
    _: Ptr,
) -> Hresult {
    S_OK
}
unsafe extern "system" fn sink_pre_new(_: Ptr, _: u32, _: Ptr, _: *const u16) -> Hresult {
    S_OK
}
#[allow(clippy::too_many_arguments)]
unsafe extern "system" fn sink_post_new(
    _: Ptr,
    _: u32,
    _: Ptr,
    _: *const u16,
    _: *const u16,
    _: u32,
    _: Hresult,
    _: Ptr,
) -> Hresult {
    S_OK
}

/// Last chance before an item is touched: skip it on a stop request, and
/// refuse to let anything be deleted outright instead of recycled.
unsafe extern "system" fn sink_pre_delete(this: Ptr, flags: u32, item: Ptr) -> Hresult {
    let state = sink(this).state;
    let root = state.root_of(item);
    if crate::stop::is_stop_requested() {
        if let Some(index) = root {
            state.fail(index, cancelled());
        }
        return HR_CANCELLED;
    }
    if flags & TSF_DELETE_RECYCLE_IF_POSSIBLE == 0 {
        if let Some(index) = root {
            state.fail(
                index,
                io::Error::other(
                    "cannot be moved to the Recycle Bin (too large, or the drive has none); nothing was deleted",
                ),
            );
        }
        return HR_CANCELLED;
    }
    S_OK
}

unsafe extern "system" fn sink_post_delete(
    this: Ptr,
    _flags: u32,
    item: Ptr,
    result: Hresult,
    newly_created: Ptr,
) -> Hresult {
    let state = sink(this).state;
    let Some(index) = state.root_of(item) else {
        return S_OK;
    };
    if result < 0 {
        // Our own skips are already recorded with a better message.
        if result != HR_CANCELLED {
            state.fail(index, hresult_error(result));
        }
    } else if newly_created.is_null() {
        state.fail(
            index,
            io::Error::other("was deleted permanently: the Recycle Bin could not take it"),
        );
    } else if state.roots[index] == item_path(item).unwrap_or_default() {
        state.outcomes.borrow_mut()[index].recycled = true;
    }
    S_OK
}

unsafe extern "system" fn sink_update_progress(this: Ptr, total: u32, done: u32) -> Hresult {
    if let Some(progress) = sink(this).state.progress {
        progress(u64::from(done), u64::from(total));
    }
    if crate::stop::is_stop_requested() {
        E_ABORT
    } else {
        S_OK
    }
}

static SINK_VTBL: SinkVtbl = SinkVtbl {
    query_interface: sink_query_interface,
    add_ref: sink_add_ref,
    release: sink_release,
    start_operations: sink_ok,
    finish_operations: sink_finish,
    pre_rename: sink_pre_rename,
    post_rename: sink_post_rename,
    pre_move: sink_pre_transfer,
    post_move: sink_post_transfer,
    pre_copy: sink_pre_transfer,
    post_copy: sink_post_transfer,
    pre_delete: sink_pre_delete,
    post_delete: sink_post_delete,
    pre_new: sink_pre_new,
    post_new: sink_post_new,
    update_progress: sink_update_progress,
    reset_timer: sink_ok,
    pause_timer: sink_ok,
    resume_timer: sink_ok,
};

/// Move one file or directory to the Recycle Bin.
pub fn recycle_path(path: &Path, progress: Option<&(dyn Fn(u64, u64) + Sync)>) -> io::Result<()> {
    match recycle_paths(&[path], progress)?.pop() {
        Some((_, err)) => Err(err),
        None => Ok(()),
    }
}

/// Move several paths to the Recycle Bin in one shell operation (a single
/// Explorer "Undo" entry). Returns the paths that were not recycled, each
/// with its own error. `Err` means the operation could not start at all.
/// `progress` receives `(done, total)` in the shell's work units.
pub fn recycle_paths<P: AsRef<Path> + Sync>(
    paths: &[P],
    progress: Option<&(dyn Fn(u64, u64) + Sync)>,
) -> io::Result<Vec<(PathBuf, io::Error)>> {
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    if crate::stop::is_stop_requested() {
        return Ok(paths
            .iter()
            .map(|p| (p.as_ref().to_path_buf(), cancelled()))
            .collect());
    }
    // The shell resolves relative paths against an unpredictable working
    // directory — always hand it absolute ones.
    let absolute: Vec<PathBuf> = paths
        .iter()
        .map(|p| std::path::absolute(p.as_ref()))
        .collect::<io::Result<_>>()?;
    std::thread::scope(|scope| {
        scope
            .spawn(|| run_on_sta(&absolute, progress))
            .join()
            .unwrap_or_else(|_| Err(io::Error::other("Recycle Bin operation panicked")))
    })
    .map(|failures| {
        // Report under the caller's spelling of each path.
        failures
            .into_iter()
            .map(|(index, err)| (paths[index].as_ref().to_path_buf(), err))
            .collect()
    })
}

/// Initialize an STA on the current (fresh) thread and run the operation.
fn run_on_sta(
    paths: &[PathBuf],
    progress: Option<&(dyn Fn(u64, u64) + Sync)>,
) -> io::Result<Vec<(usize, io::Error)>> {
    // SAFETY: plain COM initialization of this thread, balanced below.
    let hr = unsafe {
        CoInitializeEx(
            std::ptr::null_mut(),
            COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE,
        )
    };
    check(hr)?;
    let result = run_operation(paths, progress);
    // SAFETY: matches the successful CoInitializeEx above.
    unsafe { CoUninitialize() };
    result
}

fn run_operation(
    paths: &[PathBuf],
    progress: Option<&(dyn Fn(u64, u64) + Sync)>,
) -> io::Result<Vec<(usize, io::Error)>> {
    let state = SinkState {
        roots: paths
            .iter()
            .map(|p| p.to_string_lossy().to_lowercase())
            .collect(),
        outcomes: RefCell::new(paths.iter().map(|_| Outcome::default()).collect()),
        progress,
    };
    let mut sink = Sink {
        vtbl: &SINK_VTBL,
        refs: AtomicU32::new(1),
        state: &state,
    };

    let mut raw: Ptr = std::ptr::null_mut();
    // SAFETY: valid CLSID/IID and out-pointer.
    check(unsafe {
        CoCreateInstance(
            &CLSID_FILE_OPERATION,
            std::ptr::null_mut(),
            CLSCTX_INPROC_SERVER | CLSCTX_LOCAL_SERVER,
            &IID_IFILE_OPERATION,
            &mut raw,
        )
    })?;
    let operation = Com(raw);
    let op = operation.0;

    // SAFETY (all vtable calls below): `op` is a live IFileOperation and
    // each index/signature follows the interface definition in shobjidl.h.
    unsafe {
        let set_flags: unsafe extern "system" fn(Ptr, u32) -> Hresult = method(op, 5);
        check(set_flags(
            op,
            FOF_ALLOWUNDO
                | FOF_NOCONFIRMATION
                | FOF_SILENT
                | FOF_NOERRORUI
                | FOF_NOCONFIRMMKDIR
                | FOFX_RECYCLEONDELETE,
        ))?;

        let advise: unsafe extern "system" fn(Ptr, Ptr, *mut u32) -> Hresult = method(op, 3);
        let mut cookie = 0u32;
        check(advise(op, &mut sink as *mut Sink as Ptr, &mut cookie))?;

        let delete_item: unsafe extern "system" fn(Ptr, Ptr, Ptr) -> Hresult = method(op, 18);
        let mut items = Vec::with_capacity(paths.len());
        let mut queued = 0usize;
        for (index, path) in paths.iter().enumerate() {
            let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
            let mut item: Ptr = std::ptr::null_mut();
            let hr = SHCreateItemFromParsingName(
                wide.as_ptr(),
                std::ptr::null_mut(),
                &IID_ISHELL_ITEM,
                &mut item,
            );
            if hr < 0 {
                state.fail(index, hresult_error(hr));
                continue;
            }
            let item = Com(item);
            let hr = delete_item(op, item.0, std::ptr::null_mut());
            if hr < 0 {
                state.fail(index, hresult_error(hr));
                continue;
            }
            items.push(item);
            queued += 1;
        }

        let perform: unsafe extern "system" fn(Ptr) -> Hresult = method(op, 21);
        let overall = if queued > 0 { perform(op) } else { S_OK };

        let unadvise: unsafe extern "system" fn(Ptr, u32) -> Hresult = method(op, 4);
        unadvise(op, cookie);
        drop(items);

        // Every requested path ends up either recycled or with an error.
        let outcomes = state.outcomes.take();
        let mut failures = Vec::new();
        for (index, outcome) in outcomes.into_iter().enumerate() {
            if let Some(err) = outcome.error {
                failures.push((index, err));
            } else if !outcome.recycled && std::fs::symlink_metadata(&paths[index]).is_ok() {
                // Still there and the shell reported nothing for it.
                let err = if crate::stop::is_stop_requested() {
                    cancelled()
                } else if overall < 0 {
                    hresult_error(overall)
                } else {
                    io::Error::other("Recycle Bin operation failed: the item was not moved")
                };
                failures.push((index, err));
            }
        }
        Ok(failures)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::AtomicU64;

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_dir() -> PathBuf {
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("zap-recycle-test-{}-{id}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn stop_guard() -> std::sync::MutexGuard<'static, ()> {
        crate::stop::TEST_FLAG_SERIAL
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[test]
    fn test_recycle_file_and_dir_move_to_bin() {
        let _guard = stop_guard();
        let dir = temp_dir();
        let file = dir.join("recycle_me.txt");
        fs::write(&file, b"test data").unwrap();
        let sub = dir.join("folder_to_recycle");
        fs::create_dir(&sub).unwrap();
        fs::write(sub.join("inside.txt"), b"data").unwrap();

        recycle_path(&file, None).unwrap();
        recycle_path(&sub, None).unwrap();
        assert!(!file.exists());
        assert!(!sub.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_recycle_batch_attributes_errors_per_path() {
        let _guard = stop_guard();
        let dir = temp_dir();
        let good = dir.join("good.txt");
        fs::write(&good, b"x").unwrap();
        let ghost = dir.join("ghost.txt");

        let failures = recycle_paths(&[good.clone(), ghost.clone()], None).unwrap();
        assert!(!good.exists(), "the valid item is recycled anyway");
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].0, ghost);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_recycle_reports_progress() {
        let _guard = stop_guard();
        let dir = temp_dir();
        let sub = dir.join("tree");
        fs::create_dir(&sub).unwrap();
        for i in 0..20 {
            fs::write(sub.join(format!("f{i}.txt")), b"x").unwrap();
        }
        let calls = AtomicU64::new(0);
        let progress = |_: u64, _: u64| {
            calls.fetch_add(1, Ordering::Relaxed);
        };
        recycle_path(&sub, Some(&progress)).unwrap();
        assert!(!sub.exists());
        assert!(calls.load(Ordering::Relaxed) > 0, "progress was reported");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_recycle_honours_stop_request() {
        let _guard = stop_guard();
        let dir = temp_dir();
        let file = dir.join("keep.txt");
        fs::write(&file, b"x").unwrap();

        crate::stop::request_stop();
        let result = recycle_paths(std::slice::from_ref(&file), None);
        crate::stop::reset();

        let failures = result.unwrap();
        assert!(file.exists(), "nothing may be recycled after a stop");
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].1.kind(), io::ErrorKind::Interrupted);
        let _ = fs::remove_dir_all(&dir);
    }
}
