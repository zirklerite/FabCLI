//! Platform abstraction for hosting a wry WebView.
//!
//! Callers (`fab_sso_webview`, `fab_browser::in_process`) need four
//! things: initialise any platform-global state (COM on Windows,
//! GTK on Linux), create a window that wry can bind to, pump
//! platform events until a done-predicate fires, and clean up.
//!
//! This module exposes a uniform surface — `init()`, `create_window()`,
//! `pump_once()`, `build_webview()`, and a `HostWindow` type — backed
//! by a platform-specific implementation selected at compile time.
//! `build_webview` hides wry's platform split (`.build(&window)` on
//! Windows vs `.new_gtk(container)` on Linux — the latter needed for
//! Wayland support).

use crate::error::FabCliError;

/// Options for creating a platform window.
#[derive(Debug, Clone)]
pub struct WindowOptions {
    /// Window title (shown in taskbar / decorations when visible).
    pub title: &'static str,
    /// Visible windows are mapped on screen (SSO flow). Hidden
    /// windows are created but never shown — used by
    /// `fab_browser::call`'s hidden-WebView-per-call path.
    pub visible: bool,
    /// Width × height in logical pixels. Ignored for hidden windows.
    pub size: (i32, i32),
}

impl Default for WindowOptions {
    fn default() -> Self {
        Self {
            title: "fabcli",
            visible: false,
            size: (800, 600),
        }
    }
}

#[cfg(windows)]
mod windows_host;
#[cfg(windows)]
pub use windows_host::{build_webview, create_window, init, pump_once};

#[cfg(target_os = "linux")]
mod linux_host;
#[cfg(target_os = "linux")]
pub use linux_host::{build_webview, create_window, init, pump_once};

// Stub for platforms with the `webview` feature but no host impl
// (macOS / BSD today). Lets the crate compile; every operation
// fails cleanly at runtime with a "not supported" error.
#[cfg(not(any(windows, target_os = "linux")))]
mod stub_host;
#[cfg(not(any(windows, target_os = "linux")))]
pub use stub_host::{build_webview, create_window, init, pump_once};

pub(crate) fn unsupported(what: &str) -> FabCliError {
    FabCliError::Generic(format!(
        "{} is not supported on this platform / build",
        what
    ))
}
