//! Browser daemon: a per-user background process that hosts a single
//! hidden WebView and serves `fab_browser::call` requests over a
//! named pipe. Keeps per-call overhead at ~100ms across CLI
//! invocations instead of the ~1-2s one-WebView-per-call cost.

pub mod log;
pub mod protocol;

// `script` holds the browser-side fetch template; it's platform-
// agnostic JavaScript, reused by the in-process WebView path on both
// Windows and Linux.
#[cfg(any(windows, target_os = "linux"))]
pub mod script;

#[cfg(windows)]
pub mod server;

#[cfg(windows)]
pub mod client;

use std::path::Path;

/// Derive a stable named-pipe name from the absolute user-data-dir
/// path. The same path always yields the same pipe, so the daemon
/// and clients can find each other without a shared registry. Two
/// users (different token paths) get different pipes, so they never
/// contend.
pub fn pipe_name(data_dir: &Path) -> String {
    use sha2::{Digest, Sha256};
    let canonical = data_dir.to_string_lossy();
    let digest = Sha256::digest(canonical.as_bytes());
    let hex: String = digest.iter().take(12).map(|b| format!("{:02x}", b)).collect();
    format!(r"\\.\pipe\fabcli-browser-{}", hex)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn pipe_name_is_stable() {
        let p = PathBuf::from(r"C:\Users\alice\AppData\Roaming\fabcli\webview");
        assert_eq!(pipe_name(&p), pipe_name(&p));
    }

    #[test]
    fn pipe_name_differs_per_path() {
        let a = PathBuf::from(r"C:\Users\alice\AppData\Roaming\fabcli\webview");
        let b = PathBuf::from(r"C:\Users\bob\AppData\Roaming\fabcli\webview");
        assert_ne!(pipe_name(&a), pipe_name(&b));
    }

    #[test]
    fn pipe_name_has_expected_prefix() {
        let p = PathBuf::from(r"C:\x");
        let name = pipe_name(&p);
        assert!(name.starts_with(r"\\.\pipe\fabcli-browser-"));
        assert_eq!(name.len(), r"\\.\pipe\fabcli-browser-".len() + 24);
    }
}
