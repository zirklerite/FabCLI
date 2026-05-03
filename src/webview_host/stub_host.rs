//! Stub for platforms without a WebView host implementation.
//! Every function returns `FabCliError::Generic` with a clear
//! "not supported" message. Call sites that reach this stub should
//! already have been gated out by `cfg`; this exists so the crate
//! still compiles.

use super::{WindowOptions, unsupported};
use crate::error::FabCliError;
use wry::raw_window_handle::{HandleError, HasWindowHandle, WindowHandle};

pub struct HostGuard;

pub struct HostWindow;

impl HasWindowHandle for HostWindow {
    fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
        Err(HandleError::NotSupported)
    }
}

pub fn init() -> Result<HostGuard, FabCliError> {
    Err(unsupported("WebView host init"))
}

pub fn create_window(_opts: WindowOptions) -> Result<HostWindow, FabCliError> {
    Err(unsupported("WebView window creation"))
}

pub fn pump_once() {}

pub fn build_webview<'a>(
    _builder: wry::WebViewBuilder<'a>,
    _window: &'a HostWindow,
) -> wry::Result<wry::WebView> {
    // Unreachable: `create_window` above already errors before
    // anyone can produce a `HostWindow` on this platform.
    Err(wry::Error::UnsupportedWindowHandle)
}
