//! Windows taskbar progress via `ITaskbarList3`, using raw COM FFI so the
//! project keeps its zero-Windows-crate dependency policy.
//!
//! The taskbar button mirrors the in-window progress bar: green fill while
//! the run is active, red fill if any item failed, cleared otherwise.
#![cfg(windows)]

use std::ffi::c_void;

type Hwnd = *mut c_void;

#[repr(C)]
struct Guid {
    data1: u32,
    data2: u16,
    data3: u16,
    data4: [u8; 8],
}

/// CLSID_TaskbarList {56FDF344-FD6D-11d0-958A-006097C9A090}
const CLSID_TASKBAR_LIST: Guid = Guid {
    data1: 0x56FD_F344,
    data2: 0xFD6D,
    data3: 0x11d0,
    data4: [0x95, 0x8A, 0x00, 0x60, 0x97, 0xC9, 0xA0, 0x90],
};

/// IID_ITaskbarList3 {EA1AFB91-9E28-4B86-90E9-9E9F8A5EEFAF}
const IID_ITASKBAR_LIST3: Guid = Guid {
    data1: 0xEA1A_FB91,
    data2: 0x9E28,
    data3: 0x4B86,
    data4: [0x90, 0xE9, 0x9E, 0x9F, 0x8A, 0x5E, 0xEF, 0xAF],
};

const CLSCTX_INPROC_SERVER: u32 = 0x1;
const COINIT_APARTMENTTHREADED: u32 = 0x2;
/// `CoInitializeEx` returns this when the thread is already initialized
/// with a different model (winit initializes OLE for drag-and-drop).
/// COM still works in that case; we just must not pair a `CoUninitialize`.
const RPC_E_CHANGED_MODE: i32 = 0x8001_0106_u32 as i32;

const TBPF_NOPROGRESS: u32 = 0x0;
const TBPF_NORMAL: u32 = 0x2;
const TBPF_ERROR: u32 = 0x4;

/// `ITaskbarList3` vtable. Inherits IUnknown → ITaskbarList →
/// ITaskbarList2. Methods we never call are declared as `usize` slots —
/// same size as a function pointer, keeps the layout correct.
#[repr(C)]
#[allow(non_snake_case)]
struct ITaskbarList3Vtbl {
    QueryInterface: usize,
    AddRef: usize,
    Release: unsafe extern "system" fn(*mut ITaskbarList3) -> u32,
    HrInit: unsafe extern "system" fn(*mut ITaskbarList3) -> i32,
    AddTab: usize,
    DeleteTab: usize,
    ActivateTab: usize,
    SetActiveAlt: usize,
    MarkFullscreenWindow: usize,
    SetProgressValue: unsafe extern "system" fn(*mut ITaskbarList3, Hwnd, u64, u64) -> i32,
    SetProgressState: unsafe extern "system" fn(*mut ITaskbarList3, Hwnd, u32) -> i32,
}

#[repr(C)]
struct ITaskbarList3 {
    vtbl: *const ITaskbarList3Vtbl,
}

#[link(name = "ole32")]
extern "system" {
    fn CoInitializeEx(reserved: *mut c_void, co_init: u32) -> i32;
    fn CoUninitialize();
    fn CoCreateInstance(
        clsid: *const Guid,
        outer: *mut c_void,
        cls_context: u32,
        iid: *const Guid,
        out: *mut *mut c_void,
    ) -> i32;
}

#[link(name = "user32")]
extern "system" {
    fn EnumThreadWindows(
        thread_id: u32,
        callback: unsafe extern "system" fn(Hwnd, isize) -> i32,
        lparam: isize,
    ) -> i32;
    fn GetParent(hwnd: Hwnd) -> Hwnd;
}

#[link(name = "kernel32")]
extern "system" {
    fn GetCurrentThreadId() -> u32;
}

unsafe extern "system" fn enum_thread_windows_cb(hwnd: Hwnd, lparam: isize) -> i32 {
    // Take the first top-level window owned by this thread.
    if GetParent(hwnd).is_null() {
        *(lparam as *mut Hwnd) = hwnd;
        return 0; // stop enumeration
    }
    1 // continue
}

/// Find the top-level window of the *current* thread. Must be called from
/// the UI thread (eframe's `update` runs there). Returns `None` until the
/// window actually exists — callers should simply retry next frame.
pub fn find_thread_window() -> Option<usize> {
    let mut hwnd: Hwnd = std::ptr::null_mut();
    unsafe {
        EnumThreadWindows(
            GetCurrentThreadId(),
            enum_thread_windows_cb,
            &mut hwnd as *mut Hwnd as isize,
        );
    }
    if hwnd.is_null() {
        None
    } else {
        Some(hwnd as usize)
    }
}

pub struct TaskbarProgress {
    list: *mut ITaskbarList3,
    hwnd: Hwnd,
    must_uninitialize: bool,
}

impl TaskbarProgress {
    /// Create the COM taskbar object for `hwnd` (from [`find_thread_window`]).
    /// Returns `None` if COM init or object creation fails — taskbar progress
    /// is cosmetic, so callers should degrade gracefully.
    pub fn new(hwnd: usize) -> Option<Self> {
        let hr = unsafe { CoInitializeEx(std::ptr::null_mut(), COINIT_APARTMENTTHREADED) };
        // S_OK (0) and S_FALSE (1) must be paired with CoUninitialize;
        // RPC_E_CHANGED_MODE means someone else owns the init — usable,
        // but not ours to uninitialize.
        let must_uninitialize = match hr {
            0 | 1 => true,
            RPC_E_CHANGED_MODE => false,
            _ => return None,
        };

        let mut raw: *mut c_void = std::ptr::null_mut();
        let hr = unsafe {
            CoCreateInstance(
                &CLSID_TASKBAR_LIST,
                std::ptr::null_mut(),
                CLSCTX_INPROC_SERVER,
                &IID_ITASKBAR_LIST3,
                &mut raw,
            )
        };
        if hr < 0 || raw.is_null() {
            if must_uninitialize {
                unsafe { CoUninitialize() };
            }
            return None;
        }
        let list = raw as *mut ITaskbarList3;
        let hr_init = unsafe { ((*(*list).vtbl).HrInit)(list) };
        if hr_init < 0 {
            unsafe { ((*(*list).vtbl).Release)(list) };
            if must_uninitialize {
                unsafe { CoUninitialize() };
            }
            return None;
        }
        Some(Self {
            list,
            hwnd: hwnd as Hwnd,
            must_uninitialize,
        })
    }

    /// Show `done/total` as a green fill on the taskbar button.
    pub fn set_progress(&self, done: u64, total: u64) {
        unsafe {
            ((*(*self.list).vtbl).SetProgressState)(self.list, self.hwnd, TBPF_NORMAL);
            ((*(*self.list).vtbl).SetProgressValue)(self.list, self.hwnd, done, total);
        }
    }

    /// Show a full red bar (some items failed).
    pub fn set_error(&self) {
        unsafe {
            ((*(*self.list).vtbl).SetProgressValue)(self.list, self.hwnd, 100, 100);
            ((*(*self.list).vtbl).SetProgressState)(self.list, self.hwnd, TBPF_ERROR);
        }
    }

    /// Remove the progress overlay from the taskbar button.
    pub fn clear(&self) {
        unsafe {
            ((*(*self.list).vtbl).SetProgressState)(self.list, self.hwnd, TBPF_NOPROGRESS);
        }
    }
}

impl Drop for TaskbarProgress {
    fn drop(&mut self) {
        unsafe {
            ((*(*self.list).vtbl).Release)(self.list);
            if self.must_uninitialize {
                CoUninitialize();
            }
        }
    }
}
