//! Linux implementation of the WebView host abstraction.
//!
//! GTK init → `gtk::ApplicationWindow` → non-blocking
//! `gtk::main_iteration_do(false)` pump → widget closed via `Drop`.
//!
//! wry's `WebViewBuilder::build(&W: HasWindowHandle)` path only
//! supports X11 on Linux. Wayland sessions (and X11 too) work
//! through `WebViewBuilderExtUnix::new_gtk(container)` instead, so
//! we expose `build_webview` to hide that dispatch from callers.

use super::{WindowOptions, unsupported};
use crate::error::FabCliError;
use gtk::glib;
use gtk::prelude::*;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use wry::WebViewBuilderExtUnix;

pub struct HostGuard;

static CLOSED: AtomicBool = AtomicBool::new(false);

pub struct HostWindow {
    inner: gtk::Window,
}

impl HostWindow {
    pub fn was_closed(&self) -> bool {
        CLOSED.load(Ordering::Acquire)
    }

    /// Show / hide the window after creation. Used by the Epic-login
    /// flow to keep the window off-screen until `about:blank` loads,
    /// avoiding a white flash.
    pub fn set_visible(&self, visible: bool) {
        if visible {
            self.inner.show_all();
        } else {
            self.inner.hide();
        }
    }
}

impl Drop for HostWindow {
    fn drop(&mut self) {
        eprintln!("[webview-host] HostWindow::drop start");
        self.inner.close();
        eprintln!("[webview-host] HostWindow::drop after close(), pumping");
        pump_once();
        eprintln!("[webview-host] HostWindow::drop end");
    }
}

pub fn init() -> Result<HostGuard, FabCliError> {
    static READY: OnceLock<Result<(), String>> = OnceLock::new();
    let outcome = READY.get_or_init(|| {
        gtk::init().map_err(|e| format!("gtk::init failed: {}", e))
    });
    match outcome {
        Ok(()) => {
            CLOSED.store(false, Ordering::Release);
            Ok(HostGuard)
        }
        Err(msg) => Err(unsupported(&format!("WebView host init on Linux: {}", msg))),
    }
}

pub fn create_window(opts: WindowOptions) -> Result<HostWindow, FabCliError> {
    // Plain `gtk::Window` — not `ApplicationWindow`. The latter
    // requires an initialized `GApplication` with its `startup`
    // signal emitted (via `application.run()`), which we can't run
    // because our event loop is a manual `pump_once` loop, not
    // GApplication's. A bare Toplevel window has no such dependency.
    let window = gtk::Window::new(gtk::WindowType::Toplevel);
    window.set_title(opts.title);
    window.set_default_size(opts.size.0, opts.size.1);

    // `new_gtk` needs a realized widget (one that has a GdkWindow).
    // `show_all` realizes; `realize` realizes without mapping.
    if opts.visible {
        window.show_all();
    } else {
        window.realize();
    }

    window.connect_delete_event(|_, _| {
        CLOSED.store(true, Ordering::Release);
        glib::Propagation::Proceed
    });

    Ok(HostWindow { inner: window })
}

pub fn pump_once() {
    // Drain pending GTK events but bound the loop. WebKit's
    // rendering pipeline (timers, network callbacks, JS setInterval)
    // can produce events faster than we drain them, which would
    // turn an unbounded `while gtk::main_iteration_do(false) {}`
    // into a livelock — we'd never return to the caller's poll loop
    // and so never observe `OnceLock` updates set from inside an IPC
    // handler. 64 iterations per call is plenty to keep the UI
    // responsive without monopolising the thread.
    for _ in 0..64 {
        if !gtk::main_iteration_do(false) {
            break;
        }
    }
}

pub fn build_webview<'a>(
    builder: wry::WebViewBuilder<'a>,
    window: &'a HostWindow,
) -> wry::Result<wry::WebView> {
    builder.build_gtk(&window.inner)
}
