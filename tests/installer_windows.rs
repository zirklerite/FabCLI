//! Tests for the Windows installer (`dist/add-to-path.ps1`, `dist/install.bat`).
//!
//! Every PATH test runs `add-to-path.ps1 -Exe fabcli.exe` against a scratch
//! registry key (`HKCU\Software\fabcli-test-<pid>-<n>`, through `-Key`) and
//! scratch folders, never the real PATH: each test checks that
//! `HKCU\Environment`'s PATH (value and type) is the same afterwards.
//! `install.bat` is only run in ways that stop before any PATH change.

#![cfg(windows)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

use tempfile::TempDir;

fn dist(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("dist")
        .join(name)
}

/// Runs `cmd` with no terminal (stdin redirected); (exit code, stdout + stderr).
fn output(cmd: &mut Command) -> (i32, String) {
    let out = cmd
        .stdin(Stdio::null())
        .output()
        .expect("run a test process");
    let text =
        String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
    (out.status.code().unwrap_or(-1), text)
}

/// Runs PowerShell code with the given environment.
fn powershell(code: &str, envs: &[(&str, &str)]) -> (i32, String) {
    output(
        Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", code])
            .envs(envs.iter().copied()),
    )
}

/// PATH under `HKCU\<key>` as (registry kind, value), or None. Read without
/// expanding `%VARS%`, and passed back as hex so the console code page can't
/// change it.
fn read_path(key: &str) -> Option<(String, String)> {
    read_path_after("", &[], key)
}

/// `read_path`, in the same PowerShell process as `before` (one start instead
/// of two).
fn read_path_after(before: &str, envs: &[(&str, &str)], key: &str) -> Option<(String, String)> {
    let code = before.to_string()
        + r#"
$k = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey($env:T_KEY)
if ($null -eq $k) { 'NONE'; exit }
$v = $k.GetValue('PATH', $null, 'DoNotExpandEnvironmentNames')
if ($null -eq $v) { 'NONE'; exit }
$k.GetValueKind('PATH').ToString()
(([Text.Encoding]::UTF8.GetBytes([string]$v)) | ForEach-Object { $_.ToString('x2') }) -join ''
"#;
    let mut all = envs.to_vec();
    all.push(("T_KEY", key));
    let (code_, out) = powershell(&code, &all);
    assert_eq!(code_, 0, "reading {key}: {out}");
    let mut lines = out.lines().map(str::trim);
    match lines.next() {
        Some("NONE") => None,
        Some(kind) => {
            let hex = lines.next().unwrap_or("");
            let bytes: Vec<u8> = (0..hex.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
                .collect();
            Some((kind.to_string(), String::from_utf8(bytes).unwrap()))
        }
        None => panic!("no output reading {key}"),
    }
}

/// A scratch registry key, deleted on drop. Creating one also records the
/// real user PATH, which must be unchanged when the test ends.
struct ScratchKey {
    name: String,
    real_before: Option<(String, String)>,
}

impl ScratchKey {
    fn new() -> Self {
        static N: AtomicUsize = AtomicUsize::new(0);
        let name = format!(
            r"Software\fabcli-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        );
        // The key itself is created by its first writer (`set`, or add-to-path.ps1).
        ScratchKey {
            name,
            real_before: read_path("Environment"),
        }
    }

    fn set(&self, value: &str, kind: &str) {
        let (code, out) = powershell(
            "[Microsoft.Win32.Registry]::CurrentUser.CreateSubKey($env:T_KEY).SetValue('PATH', $env:T_VALUE, $env:T_KIND)",
            &[("T_KEY", &self.name), ("T_VALUE", value), ("T_KIND", kind)],
        );
        assert_eq!(code, 0, "{out}");
    }

    fn path(&self) -> Option<(String, String)> {
        read_path(&self.name)
    }
}

impl Drop for ScratchKey {
    fn drop(&mut self) {
        let real_after = read_path_after(
            "[Microsoft.Win32.Registry]::CurrentUser.DeleteSubKeyTree($env:T_DEL, $false)\n",
            &[("T_DEL", &self.name)],
            "Environment",
        );
        if !std::thread::panicking() {
            assert_eq!(
                real_after, self.real_before,
                "the real user PATH changed during the test"
            );
        }
    }
}

/// A scratch folder for one test: the release folder being installed
/// (`release`), backups (`backups`), and room for more folders.
struct Scratch {
    tmp: TempDir,
    key: ScratchKey,
}

impl Scratch {
    fn new() -> Self {
        let s = Scratch {
            tmp: TempDir::new().unwrap(),
            key: ScratchKey::new(),
        };
        s.release("release", Some("1.0.0"));
        s
    }

    fn dir(&self, name: &str) -> String {
        self.tmp.path().join(name).to_string_lossy().into_owned()
    }

    /// A folder holding fabcli.exe; with a version, also a BUILD_INFO.txt
    /// (a release folder), else a folder that merely holds fabcli.exe.
    fn release(&self, name: &str, version: Option<&str>) -> String {
        self.release_of(name, "fabcli.exe", version)
    }

    /// `release`, for another program.
    fn release_of(&self, name: &str, exe: &str, version: Option<&str>) -> String {
        let dir = self.tmp.path().join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(exe), b"").unwrap();
        if let Some(v) = version {
            fs::write(
                dir.join("BUILD_INFO.txt"),
                format!("version={v}\nplatform=windows64\n"),
            )
            .unwrap();
        }
        self.dir(name)
    }

    fn backups(&self) -> Vec<String> {
        let dir = self.tmp.path().join("backups");
        match fs::read_dir(&dir) {
            Ok(entries) => entries
                .map(|e| fs::read_to_string(e.unwrap().path()).unwrap())
                .collect(),
            Err(_) => Vec::new(),
        }
    }

    /// Runs add-to-path.ps1 for `release` against the scratch key, with no
    /// terminal (stdin redirected); (exit code, output).
    fn run(&self, extra: &[&str]) -> (i32, String) {
        self.run_as("fabcli.exe", extra)
    }

    /// `run`, with another `-Exe` value.
    fn run_as(&self, exe: &str, extra: &[&str]) -> (i32, String) {
        let release = self.dir("release");
        let backups = self.dir("backups");
        let script = dist("add-to-path.ps1");
        let mut args: Vec<&str> = vec![
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
            script.to_str().unwrap(),
            "-Exe",
            exe,
            "-Dir",
            &release,
            "-Key",
            &self.key.name,
            "-SystemPath",
            r"Z:\fabcli-test-no-such-folder",
            "-BackupDir",
            &backups,
        ];
        args.extend_from_slice(extra);
        output(Command::new("powershell").args(&args))
    }
}

/// A PATH value as `ScratchKey::path` returns it.
fn reg(kind: &str, value: &str) -> Option<(String, String)> {
    Some((kind.to_string(), value.to_string()))
}

#[test]
fn appends_to_an_existing_path_and_backs_it_up() {
    let s = Scratch::new();
    s.key.set(r"C:\a;C:\b", "ExpandString");
    let (code, out) = s.run(&["-Yes"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(
        s.key.path(),
        reg("ExpandString", &format!(r"C:\a;C:\b;{}", s.dir("release")))
    );
    assert_eq!(s.backups(), vec!["ExpandString\r\nC:\\a;C:\\b".to_string()]);
}

#[test]
fn creates_the_value_when_there_is_none() {
    let s = Scratch::new();
    let (code, out) = s.run(&["-Yes"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(s.key.path(), reg("ExpandString", &s.dir("release")));
}

#[test]
fn already_present_writes_nothing() {
    let s = Scratch::new();
    let value = format!(r"C:\a;{}\", s.dir("release")); // trailing slash: same folder
    s.key.set(&value, "ExpandString");
    let (code, out) = s.run(&["-Yes"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("already on your user PATH"), "{out}");
    assert_eq!(s.key.path(), reg("ExpandString", &value));
    assert!(s.backups().is_empty());
}

#[test]
fn replaces_an_older_install_in_place() {
    let s = Scratch::new();
    let old = s.release("old", Some("0.1.0"));
    let older = s.release("older", Some("0.0.9"));
    s.key
        .set(&format!(r"C:\a;{old};C:\b;{older}"), "ExpandString");
    let (code, out) = s.run(&["-Yes"]);
    assert_eq!(code, 3, "{out}");
    assert!(out.contains("v0.1.0"), "{out}");
    assert_eq!(
        s.key.path(),
        reg("ExpandString", &format!(r"C:\a;{};C:\b", s.dir("release")))
    );
}

#[test]
fn leaves_a_shared_folder_alone_and_warns() {
    // No BUILD_INFO.txt: not a release folder. A cargo dev build put on PATH
    // on purpose is one of these.
    let s = Scratch::new();
    let shared = s.release(r"repo\target\release", None);
    s.key.set(&shared, "ExpandString");
    let (code, out) = s.run(&["-Yes"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("WARNING"), "{out}");
    assert!(out.contains("rename that fabcli.exe"), "{out}");
    assert_eq!(
        s.key.path(),
        reg("ExpandString", &format!("{shared};{}", s.dir("release")))
    );
}

#[test]
fn keeps_vars_and_empty_entries_byte_for_byte() {
    let s = Scratch::new();
    s.key
        .set(r"%USERPROFILE%\bin;;C:\x & y;C:\a!b^c;", "ExpandString");
    let (code, out) = s.run(&["-Yes"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(
        s.key.path(),
        reg(
            "ExpandString",
            &format!(
                r"%USERPROFILE%\bin;;C:\x & y;C:\a!b^c;{};",
                s.dir("release")
            )
        )
    );
}

#[test]
fn keeps_a_reg_sz_value_as_reg_sz() {
    let s = Scratch::new();
    s.key.set(r"C:\100%;C:\a", "String");
    let (code, out) = s.run(&["-Yes"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(
        s.key.path(),
        reg("String", &format!(r"C:\100%;C:\a;{}", s.dir("release")))
    );
}

#[test]
fn refuses_a_path_over_the_registry_limit() {
    let s = Scratch::new();
    let long = format!(r"C:\{}", "a".repeat(32_760));
    s.key.set(&long, "ExpandString");
    let (code, out) = s.run(&["-Yes"]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("32767"), "{out}");
    assert_eq!(s.key.path(), reg("ExpandString", &long));
}

#[test]
fn without_a_terminal_and_without_yes_it_skips() {
    let s = Scratch::new();
    s.key.set(r"C:\a", "ExpandString");
    let (code, out) = s.run(&[]);
    assert_eq!(code, 2, "{out}");
    assert!(out.contains("--yes"), "{out}");
    assert_eq!(s.key.path(), reg("ExpandString", r"C:\a"));
    assert!(s.backups().is_empty());
}

#[test]
fn a_path_changed_mid_run_is_not_overwritten() {
    let s = Scratch::new();
    s.key.set(r"C:\a", "ExpandString");
    let hook = format!(
        "[Microsoft.Win32.Registry]::CurrentUser.CreateSubKey('{}').SetValue('PATH', 'C:\\changed', 'ExpandString')",
        s.key.name
    );
    let (code, out) = s.run(&["-Yes", "-TestHookBeforeWrite", &hook]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("changed while the installer ran"), "{out}");
    assert_eq!(s.key.path(), reg("ExpandString", r"C:\changed"));
}

#[test]
fn uninstall_removes_only_fabcli_entries() {
    let s = Scratch::new();
    let old = s.release("old", Some("0.1.0"));
    let shared = s.release("tools", None);
    s.key.set(
        &format!(r"C:\a;{};{shared};{old};;C:\b", s.dir("release")),
        "ExpandString",
    );
    let (code, out) = s.run(&["-Uninstall", "-Yes"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(
        s.key.path(),
        reg("ExpandString", &format!(r"C:\a;{shared};;C:\b"))
    );
    assert_eq!(s.backups().len(), 1);
}

#[test]
fn uninstall_with_nothing_to_remove_writes_nothing() {
    let s = Scratch::new();
    s.key.set(r"C:\a", "ExpandString");
    let (code, out) = s.run(&["-Uninstall", "-Yes"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("nothing to remove"), "{out}");
    assert!(s.backups().is_empty());
}

#[test]
fn status_names_the_folder_a_new_terminal_finds() {
    let s = Scratch::new();
    s.key
        .set(&format!(r"C:\a;{}", s.dir("release")), "ExpandString");
    let (code, out) = s.run(&["-Status"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("That is this folder"), "{out}");

    let other = s.release("other", Some("0.1.0"));
    s.key.set(&other, "ExpandString");
    let (_, out) = s.run(&["-Status"]);
    assert!(out.contains("not this folder"), "{out}");
    assert!(out.contains("older fabcli install"), "{out}");
    assert!(s.backups().is_empty());
}

#[test]
fn replacing_an_older_install_also_drops_duplicates_of_this_folder() {
    let s = Scratch::new();
    let old = s.release("old", Some("0.1.0"));
    s.key
        .set(&format!(r"{old};C:\a;{}", s.dir("release")), "ExpandString");
    let (code, out) = s.run(&["-Yes"]);
    assert_eq!(code, 3, "{out}");
    assert_eq!(
        s.key.path(),
        reg("ExpandString", &format!(r"{};C:\a", s.dir("release")))
    );
}

#[test]
fn this_folder_listed_twice_alone_is_not_rewritten() {
    let s = Scratch::new();
    let value = format!(r"{0};C:\a;{0}", s.dir("release"));
    s.key.set(&value, "ExpandString");
    let (code, out) = s.run(&["-Yes"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("already on your user PATH"), "{out}");
    assert_eq!(s.key.path(), reg("ExpandString", &value));
}

#[test]
fn another_tools_install_is_left_alone() {
    let s = Scratch::new();
    let other = s.release_of("ue5cli", "ue5cli.exe", Some("9.9.9"));
    s.key.set(&other, "ExpandString");
    let (code, out) = s.run(&["-Yes"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(
        s.key.path(),
        reg("ExpandString", &format!("{other};{}", s.dir("release")))
    );
}

#[test]
fn exe_must_be_a_file_name() {
    let s = Scratch::new();
    s.key.set(r"C:\a", "ExpandString");
    let (code, out) = s.run_as(r"..\fabcli.exe", &["-Yes"]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("-Exe must be"), "{out}");
    assert_eq!(s.key.path(), reg("ExpandString", r"C:\a"));
}

/// install.bat in a folder of its own, with add-to-path.ps1 and fabcli.exe
/// only as asked: these runs stop before any PATH change.
fn run_bat(with_ps1: bool, with_exe: bool, args: &[&str]) -> (i32, String) {
    let tmp = TempDir::new().unwrap();
    fs::copy(dist("install.bat"), tmp.path().join("install.bat")).unwrap();
    if with_ps1 {
        fs::copy(dist("add-to-path.ps1"), tmp.path().join("add-to-path.ps1")).unwrap();
    }
    if with_exe {
        fs::write(tmp.path().join("fabcli.exe"), b"").unwrap();
    }
    output(
        Command::new("cmd")
            .arg("/c")
            .arg(tmp.path().join("install.bat"))
            .args(args),
    )
}

#[test]
fn bat_help() {
    let (code, out) = run_bat(false, false, &["--help"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("install.bat uninstall"), "{out}");
}

#[test]
fn bat_needs_the_binary_and_its_ps1() {
    let (code, out) = run_bat(true, false, &["--yes"]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("fabcli.exe not found"), "{out}");
    let (code, out) = run_bat(false, true, &["--yes"]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("add-to-path.ps1 not found"), "{out}");
}
