//! Windows implementation of the WebView host abstraction.
//!
//! COM apartment init → `CreateWindowExW` against a registered
//! window class → non-blocking `PeekMessageW` pump → `DestroyWindow`.
//! Factor-out of the logic that used to live inline in
//! `fab_sso_webview.rs` and `fab_browser.rs::in_process`.

use super::{WindowOptions, unsupported};
use crate::error::FabCliError;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::Com::{
    COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoInitializeEx, CoUninitialize,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, HWND_TOP, MSG, PM_REMOVE,
    PeekMessageW, RegisterClassW, SWP_NOACTIVATE, SWP_NOZORDER, SW_HIDE, SW_SHOW, SetWindowPos,
    ShowWindow, TranslateMessage, WINDOW_EX_STYLE, WM_DESTROY, WNDCLASSW, WS_OVERLAPPEDWINDOW,
};
use windows::core::{Error as WinError, w};
use wry::raw_window_handle::{
    HandleError, HasWindowHandle, RawWindowHandle, Win32WindowHandle, WindowHandle,
};

/// RAII guard that pairs with `init()` to run `CoUninitialize` on
/// drop. Callers hold the guard for the duration of the command.
pub struct HostGuard;

impl Drop for HostGuard {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

/// Callers should assume at most one `HostWindow` exists at a time
/// (serialized per CLI invocation). A single process-wide
/// `CLOSED` latch is sufficient — `wnd_proc` cannot capture
/// per-window state without `SetWindowLongPtrW` ceremony, and the
/// architecture opens exactly one WebView at a time anyway.
static CLOSED: AtomicBool = AtomicBool::new(false);

pub struct HostWindow {
    hwnd: HWND,
    /// Size captured at create time. `set_visible(true)` uses it to
    /// restore the window to its intended on-screen bounds if it was
    /// initially created off-screen (the `opts.visible == false` path).
    visible_size: (i32, i32),
}

impl HostWindow {
    /// True if the wnd_proc observed `WM_DESTROY` on this window
    /// (user closed the window, or we called `DestroyWindow`
    /// externally). Used by SSO to detect early cancellation.
    pub fn was_closed(&self) -> bool {
        CLOSED.load(Ordering::Acquire)
    }

    /// Show / hide the window after creation. Used by the Epic-login
    /// flow to keep the window off-screen until `about:blank` loads,
    /// avoiding a white flash. On `true`, also repositions to (100,
    /// 100) in case it was created off-screen.
    pub fn set_visible(&self, visible: bool) {
        unsafe {
            if visible {
                let _ = SetWindowPos(
                    self.hwnd,
                    Some(HWND_TOP),
                    100,
                    100,
                    self.visible_size.0,
                    self.visible_size.1,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );
                let _ = ShowWindow(self.hwnd, SW_SHOW);
            } else {
                let _ = ShowWindow(self.hwnd, SW_HIDE);
            }
        }
    }
}

impl Drop for HostWindow {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

impl HasWindowHandle for HostWindow {
    fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
        let isize_handle =
            std::num::NonZeroIsize::new(self.hwnd.0 as isize).ok_or(HandleError::Unavailable)?;
        let raw = RawWindowHandle::Win32(Win32WindowHandle::new(isize_handle));
        Ok(unsafe { WindowHandle::borrow_raw(raw) })
    }
}

/// Windows uses wry's generic `build(&W: HasWindowHandle)` path.
/// Linux needs `new_gtk` instead, so callers route through this
/// helper to stay platform-agnostic.
pub fn build_webview<'a>(
    builder: wry::WebViewBuilder<'a>,
    window: &'a HostWindow,
) -> wry::Result<wry::WebView> {
    builder.build(window)
}

pub fn init() -> Result<HostGuard, FabCliError> {
    unsafe {
        let hr = CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE);
        if hr.is_err() {
            return Err(unsupported(&format!("CoInitializeEx failed: {:?}", hr)));
        }
    }
    CLOSED.store(false, Ordering::Release);
    Ok(HostGuard)
}

pub fn create_window(opts: WindowOptions) -> Result<HostWindow, FabCliError> {
    make_window(opts).map_err(|e| unsupported(&format!("CreateWindowExW: {}", e)))
}

fn make_window(opts: WindowOptions) -> Result<HostWindow, WinError> {
    static REGISTERED: OnceLock<()> = OnceLock::new();
    unsafe {
        let hmodule = GetModuleHandleW(None)?;
        let hinstance: HINSTANCE = hmodule.into();
        REGISTERED.get_or_init(|| {
            let class = WNDCLASSW {
                lpfnWndProc: Some(wnd_proc),
                hInstance: hinstance,
                lpszClassName: w!("FabCliWebViewHost"),
                ..Default::default()
            };
            RegisterClassW(&class);
        });

        // Hidden windows: position off-screen at (-32000, -32000)
        // so any transient flicker is invisible on normal displays.
        // Visible windows: standard top-left corner with the
        // requested size.
        let (x, y, w_px, h_px) = if opts.visible {
            (100, 100, opts.size.0, opts.size.1)
        } else {
            (-32000, -32000, 10, 10)
        };

        // Title is ASCII-only for now; use `w!` lit or convert.
        // Since `WindowOptions::title` is `&'static str`, this does
        // a heap alloc for the UTF-16 string. Negligible for a
        // one-shot command.
        let title_utf16: Vec<u16> = opts.title.encode_utf16().chain(std::iter::once(0)).collect();
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            w!("FabCliWebViewHost"),
            windows::core::PCWSTR(title_utf16.as_ptr()),
            WS_OVERLAPPEDWINDOW,
            x,
            y,
            w_px,
            h_px,
            None,
            None,
            Some(hinstance),
            None,
        )?;

        if opts.visible {
            let _ = ShowWindow(hwnd, SW_SHOW);
        }
        Ok(HostWindow {
            hwnd,
            visible_size: opts.size,
        })
    }
}

pub fn pump_once() {
    unsafe {
        let mut msg = MSG::default();
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        match msg {
            WM_DESTROY => {
                CLOSED.store(true, Ordering::Release);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}
