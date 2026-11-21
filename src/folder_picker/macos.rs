use super::Selection;
use anyhow::{Context, Result, anyhow};
use std::{
    ffi::{CStr, CString, OsStr, c_char, c_void},
    os::unix::ffi::OsStrExt,
    path::PathBuf,
    ptr::{self, NonNull},
    slice,
};
use tokio::sync::oneshot;

unsafe extern "C" {
    fn kb_picker_has_gui() -> bool;
    fn kb_picker_run();
    fn kb_picker_cancel();
    fn kb_picker_stop();
    fn kb_picker_select(
        initial: *const c_char,
        context: *mut c_void,
        callback: extern "C" fn(*mut c_void, *const c_char, *const c_void, usize, *const c_char),
    );
    fn kb_picker_restore(
        bytes: *const u8,
        size: usize,
        context: *mut c_void,
        callback: extern "C" fn(*mut c_void, *mut c_void, *const c_char, *const c_char),
    );
    fn kb_picker_release(handle: *mut c_void);
}

pub fn has_gui() -> bool {
    unsafe { kb_picker_has_gui() }
}

// AppKit runs on the process's main thread.
pub fn run() {
    unsafe { kb_picker_run() }
}

pub fn cancel() {
    unsafe { kb_picker_cancel() }
}

pub fn stop() {
    unsafe { kb_picker_stop() }
}

type PendingSelection = oneshot::Sender<Result<Option<Selection>>>;

pub async fn select(initial: Option<PathBuf>) -> Result<Option<Selection>> {
    let initial = initial
        .map(|path| CString::new(path.as_os_str().as_bytes()))
        .transpose()?;
    let (send, receive) = oneshot::channel();
    let context = Box::into_raw(Box::new(send));
    // The native function copies the initial path and returns the context exactly once.
    unsafe {
        kb_picker_select(
            initial.as_ref().map_or(ptr::null(), |path| path.as_ptr()),
            context.cast(),
            picked,
        )
    };
    receive.await.context("文件夹选择窗口已关闭")?
}

extern "C" fn picked(
    context: *mut c_void,
    path: *const c_char,
    bytes: *const c_void,
    size: usize,
    error: *const c_char,
) {
    let send = unsafe { Box::from_raw(context.cast::<PendingSelection>()) };
    let result = if !error.is_null() {
        Err(anyhow!(
            unsafe { CStr::from_ptr(error) }
                .to_string_lossy()
                .into_owned()
        ))
    } else if path.is_null() || bytes.is_null() {
        Ok(None)
    } else {
        let path = unsafe { CStr::from_ptr(path) }.to_bytes();
        let bookmark = unsafe { slice::from_raw_parts(bytes.cast::<u8>(), size) }.to_vec();
        Ok(Some(Selection {
            path: OsStr::from_bytes(path).into(),
            bookmark,
        }))
    };
    let _ = send.send(result);
}

pub struct Scope(NonNull<c_void>);

// NSURL is immutable, and accessing/releasing its scope is safe off the main thread.
unsafe impl Send for Scope {}
unsafe impl Sync for Scope {}

impl Drop for Scope {
    fn drop(&mut self) {
        unsafe { kb_picker_release(self.0.as_ptr()) }
    }
}

type RestoredScope = Option<Result<(Scope, PathBuf)>>;

pub fn restore(bookmark: &[u8]) -> Result<(Scope, PathBuf)> {
    let mut result: RestoredScope = None;
    // The native restore callback runs synchronously before the borrowed buffer expires.
    unsafe {
        kb_picker_restore(
            bookmark.as_ptr(),
            bookmark.len(),
            ptr::from_mut(&mut result).cast(),
            restored,
        )
    };
    result.context("macOS 没有返回文件夹授权")?
}

extern "C" fn restored(
    context: *mut c_void,
    handle: *mut c_void,
    path: *const c_char,
    error: *const c_char,
) {
    let result = unsafe { &mut *context.cast::<RestoredScope>() };
    *result = Some(if let Some(handle) = NonNull::new(handle) {
        let scope = Scope(handle);
        if path.is_null() {
            Err(anyhow!("macOS 没有返回选中的路径"))
        } else {
            let path = unsafe { CStr::from_ptr(path) };
            Ok((scope, OsStr::from_bytes(path.to_bytes()).into()))
        }
    } else {
        Err(anyhow!(
            unsafe { CStr::from_ptr(error) }
                .to_string_lossy()
                .into_owned()
        ))
    });
}
