//! Runs `tests/install_sh_test.sh` against `dist/install.sh` on Linux,
//! so `cargo test` there covers the Linux installer. Windows' installer
//! (`dist/install.bat` + `dist/add-to-path.ps1`) is tested by
//! `tests/installer_windows.rs`.

#[cfg(target_os = "linux")]
#[test]
fn install_sh() {
    let script = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/install_sh_test.sh");
    assert_cmd::Command::new("bash")
        .arg(script)
        .assert()
        .success();
}
