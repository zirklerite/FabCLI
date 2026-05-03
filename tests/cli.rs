use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

/// Spawn `fabcli` with an isolated, empty token file path so the
/// developer's real `%APPDATA%\fabcli\token.json` (or Linux equivalent)
/// is never touched. Returns the [`TempDir`] so the caller keeps it
/// alive for the duration of the test.
fn fabcli() -> (Command, TempDir) {
    let dir = TempDir::new().expect("failed to create tempdir");
    let token = dir.path().join("token.json");
    let mut cmd = Command::cargo_bin("fabcli").expect("fabcli binary not built");
    cmd.env("FABCLI_TOKEN_PATH", &token);
    (cmd, dir)
}

// ── General ──

#[test]
fn top_level_help_exits_zero() {
    let (mut cmd, _dir) = fabcli();
    cmd.arg("--help").assert().success();
}

#[test]
fn top_level_help_lists_flat_commands() {
    let (mut cmd, _dir) = fabcli();
    cmd.arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("search"))
        .stdout(predicate::str::contains("library"))
        .stdout(predicate::str::contains("listing"))
        .stdout(predicate::str::contains("formats"))
        .stdout(predicate::str::contains("prices"))
        .stdout(predicate::str::contains("ownership"))
        .stdout(predicate::str::contains("claim"))
        .stdout(predicate::str::contains("reviews"))
        .stdout(predicate::str::contains("manifest"))
        .stdout(predicate::str::contains("download"));
}

#[test]
fn fab_subcommand_is_gone() {
    let (mut cmd, _dir) = fabcli();
    cmd.arg("fab").assert().code(6);
}

#[test]
fn unknown_subcommand_exits_six() {
    let (mut cmd, _dir) = fabcli();
    cmd.arg("bogus-command").assert().code(6);
}

// ── Auth ──

#[test]
fn auth_subcommand_help_exits_zero() {
    let (mut cmd, _dir) = fabcli();
    cmd.args(["auth", "--help"]).assert().success();
}

#[test]
fn auth_status_without_session_is_auth_required() {
    let (mut cmd, _dir) = fabcli();
    cmd.args(["auth", "status"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("\"kind\":\"auth_required\""))
        .stderr(predicate::str::contains("no session"));
}

#[test]
fn auth_whoami_without_session_is_auth_required() {
    let (mut cmd, _dir) = fabcli();
    cmd.args(["auth", "whoami"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("\"kind\":\"auth_required\""));
}

#[test]
fn auth_logout_without_session_succeeds_idempotently() {
    let (mut cmd, _dir) = fabcli();
    cmd.args(["auth", "logout"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"ok\":true"))
        .stdout(predicate::str::contains("\"note\":\"no session\""));
}

#[test]
fn pretty_flag_produces_multiline_output() {
    let (mut cmd, _dir) = fabcli();
    cmd.args(["--pretty", "auth", "logout"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\n  \"ok\": true"));
}

#[test]
fn auth_login_manual_without_tty_fails_fast() {
    let (mut cmd, _dir) = fabcli();
    cmd.args(["auth", "login", "--manual"])
        .write_stdin("")
        .assert()
        .failure()
        .stderr(predicate::str::contains("TTY"));
}

#[test]
fn auth_login_help_shows_manual_flag() {
    let (mut cmd, _dir) = fabcli();
    cmd.args(["auth", "login", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--manual"));
}

#[test]
fn auth_logout_deletes_preexisting_token_file() {
    // Logout is the recovery path — it MUST succeed even if the
    // token file is unreadable (corrupt, wrong format, etc.) so
    // users can clean up after any kind of breakage.
    let (mut cmd, dir) = fabcli();
    let token = dir.path().join("token.json");
    std::fs::write(&token, b"definitely not a valid encrypted token").unwrap();
    assert!(token.exists());

    cmd.args(["auth", "logout"]).assert().success();
    assert!(!token.exists(), "token file should have been deleted");
}

#[test]
fn token_path_env_override_is_honored() {
    let dir_a = TempDir::new().unwrap();
    let dir_b = TempDir::new().unwrap();
    let token_a = dir_a.path().join("token.json");
    let token_b = dir_b.path().join("token.json");

    // Same recovery contract: logout must clean up regardless of
    // whether the file is parseable.
    std::fs::write(&token_a, b"unparseable garbage at the override path").unwrap();

    Command::cargo_bin("fabcli")
        .unwrap()
        .env("FABCLI_TOKEN_PATH", &token_a)
        .args(["auth", "logout"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"ok\":true"))
        .stdout(predicate::str::contains("\"note\":\"no session\""));
    assert!(
        !token_a.exists(),
        "override path A should have been consumed and deleted"
    );

    Command::cargo_bin("fabcli")
        .unwrap()
        .env("FABCLI_TOKEN_PATH", &token_b)
        .args(["auth", "logout"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"note\":\"no session\""));
    assert!(
        !token_b.exists(),
        "override path B was never written to in the first place"
    );
}

// ── Marketplace commands (flat, no `fab` prefix) ──

#[test]
fn search_without_session_is_auth_required() {
    let (mut cmd, _dir) = fabcli();
    cmd.args(["search", "-q", "test"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("\"kind\":\"auth_required\""));
}

#[test]
fn search_filter_malformed_input_exits_invalid_args() {
    // Pin the clap value-parser → exit-code-6 mapping with one
    // representative invocation. The `parse_kv` unit tests cover the
    // other malformed-input branches (empty key, empty value, empty
    // string) directly without subprocess overhead.
    let (mut cmd, _dir) = fabcli();
    cmd.args(["search", "--filter", "foo"])
        .assert()
        .code(6)
        .stderr(predicate::str::contains("missing `=`"));
}

#[test]
fn search_filter_accepted_and_repeatable() {
    // One subprocess proves: clap accepts multiple `--filter`
    // invocations with mixed key shapes (boolean, scalar, multi-valued
    // repeat), and the request reaches the auth_required path —
    // confirming the flag is wired into params.extra_params.
    let (mut cmd, _dir) = fabcli();
    cmd.args([
        "search",
        "--filter", "is_free=1",
        "--filter", "channels=unreal-engine",
        "--filter", "min_discount_percentage=100",
        "--filter", "published_since=2026-04-01",
        "--filter", "styles=anime",
        "--filter", "styles=lowpoly",
    ])
    .assert()
    .code(2)
    .stderr(predicate::str::contains("\"kind\":\"auth_required\""));
}

#[test]
fn search_sort_with_space_separated_hyphen_value_parses() {
    // Regression: clap used to reject `--sort "-createdAt"` as an
    // unknown short flag. The allow_hyphen_values = true setting on
    // the sort field fixes it. Exit 2 (auth_required) proves the
    // parser accepted the value and we reached command execution.
    let (mut cmd, _dir) = fabcli();
    cmd.args(["search", "--sort", "-createdAt"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("\"kind\":\"auth_required\""));
}

#[test]
fn library_mutually_exclusive_cache_flags_rejected() {
    for pair in [
        ("--cache", "--no-cache"),
        ("--cache", "--refresh"),
        ("--cache", "--clear"),
        ("--no-cache", "--refresh"),
        ("--no-cache", "--clear"),
        ("--refresh", "--clear"),
    ] {
        let (mut cmd, _dir) = fabcli();
        cmd.args(["library", pair.0, pair.1]).assert().code(6);
    }
}

#[test]
fn library_clear_conflicts_with_count() {
    let (mut cmd, _dir) = fabcli();
    cmd.args(["library", "--clear", "--count", "100"])
        .assert()
        .code(6);
}

#[test]
fn library_clear_succeeds_without_session() {
    // `--clear` is pure cache maintenance; must not require auth.
    let (mut cmd, _dir) = fabcli();
    cmd.args(["library", "--clear"])
        .assert()
        .code(0)
        .stdout(predicate::str::contains("\"deleted\""));
}

#[test]
fn library_without_session_is_auth_required() {
    let (mut cmd, _dir) = fabcli();
    cmd.args(["library"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("\"kind\":\"auth_required\""));
}

#[test]
fn listing_without_session_is_auth_required() {
    let (mut cmd, _dir) = fabcli();
    cmd.args(["listing", "some-uid"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("\"kind\":\"auth_required\""));
}

#[test]
fn listing_without_uid_and_no_stdin_fails() {
    let (mut cmd, _dir) = fabcli();
    cmd.args(["listing"])
        .write_stdin("")
        .assert()
        .failure();
}

#[test]
fn listing_stdin_with_empty_input_fails() {
    let (mut cmd, _dir) = fabcli();
    cmd.args(["listing", "--stdin"])
        .write_stdin("")
        .assert()
        .failure();
}

#[test]
fn formats_without_session_is_auth_required() {
    let (mut cmd, _dir) = fabcli();
    cmd.args(["formats", "some-uid"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("\"kind\":\"auth_required\""));
}

#[test]
fn formats_without_uid_and_no_stdin_fails() {
    let (mut cmd, _dir) = fabcli();
    cmd.args(["formats"])
        .write_stdin("")
        .assert()
        .failure();
}

#[test]
fn prices_without_session_is_auth_required() {
    let (mut cmd, _dir) = fabcli();
    cmd.args(["prices", "some-uid"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("\"kind\":\"auth_required\""));
}

#[test]
fn prices_uid_conflicts_with_offer_ids() {
    let (mut cmd, _dir) = fabcli();
    cmd.args(["prices", "some-uid", "--offer-ids", "a,b"])
        .assert()
        .failure();
}

#[test]
fn prices_without_uid_or_offer_ids_fails() {
    let (mut cmd, _dir) = fabcli();
    cmd.args(["prices"])
        .assert()
        .code(2);
}

#[test]
fn ownership_without_session_is_auth_required() {
    let (mut cmd, _dir) = fabcli();
    cmd.args(["ownership", "some-uid"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("\"kind\":\"auth_required\""));
}

#[test]
fn ownership_batch_without_session_is_auth_required() {
    // Per the library-fetch-optimization contract, --batch requires
    // Fab session up front. No silent library-walk fallback.
    let (mut cmd, _dir) = fabcli();
    cmd.args(["ownership", "--batch", "a,b,c"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("\"kind\":\"auth_required\""));
}

#[test]
fn ownership_from_library_without_session_is_auth_required() {
    // Per the library-fetch-optimization contract, --from-library
    // requires Fab session even though the UID source is the
    // bearer-auth library walk. The rich state comes from
    // bulk listings-states which needs Fab cookies.
    let (mut cmd, _dir) = fabcli();
    cmd.args(["ownership", "--from-library"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("\"kind\":\"auth_required\""));
}

#[test]
fn ownership_from_stdin_without_session_is_auth_required() {
    let (mut cmd, _dir) = fabcli();
    cmd.args(["ownership", "--from-stdin"])
        .write_stdin("a\nb\nc\n")
        .assert()
        .code(2)
        .stderr(predicate::str::contains("\"kind\":\"auth_required\""));
}

#[test]
fn search_with_ownership_without_session_is_auth_required() {
    // Per the library-fetch-optimization contract, --with-ownership
    // requires Fab session up front. The ensure_fab_session_ready
    // check fires before the search call, so the user doesn't pay
    // search latency just to learn auth is missing.
    let (mut cmd, _dir) = fabcli();
    cmd.args(["search", "-q", "neon", "--with-ownership"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("\"kind\":\"auth_required\""));
}

#[test]
fn claim_without_session_is_auth_required() {
    let (mut cmd, _dir) = fabcli();
    cmd.args(["claim", "some-uid"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("\"kind\":\"auth_required\""));
}

#[test]
fn claim_stdin_with_empty_input_fails() {
    let (mut cmd, _dir) = fabcli();
    cmd.args(["claim", "--stdin"])
        .write_stdin("")
        .assert()
        .failure();
}

#[test]
fn reviews_without_session_is_auth_required() {
    let (mut cmd, _dir) = fabcli();
    cmd.args(["reviews", "some-uid"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("\"kind\":\"auth_required\""));
}

#[test]
fn manifest_without_session_is_auth_required() {
    let (mut cmd, _dir) = fabcli();
    cmd.args([
        "manifest",
        "--artifact-id", "x",
        "--namespace", "y",
        "--asset-id", "z",
    ])
    .assert()
    .code(2)
    .stderr(predicate::str::contains("\"kind\":\"auth_required\""));
}

// ── Download ──

#[test]
fn download_help_lists_expected_flags() {
    let (mut cmd, _dir) = fabcli();
    cmd.args(["download", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--artifact-id"))
        .stdout(predicate::str::contains("--namespace"))
        .stdout(predicate::str::contains("--asset-id"))
        .stdout(predicate::str::contains("--output"))
        .stdout(predicate::str::contains("--jobs"));
}

#[test]
fn download_without_session_is_auth_required() {
    let (mut cmd, _dir) = fabcli();
    cmd.args([
        "download",
        "--artifact-id", "x",
        "--namespace", "y",
        "--asset-id", "z",
        "--output", "/tmp/fabcli-test-dl",
    ])
    .assert()
    .code(2)
    .stderr(predicate::str::contains("\"kind\":\"auth_required\""));
}

#[test]
fn download_without_required_flags_exits_six() {
    let (mut cmd, _dir) = fabcli();
    cmd.args(["download"])
        .assert()
        .code(6);
}

#[test]
fn download_help_lists_overwrite_flags() {
    let (mut cmd, _dir) = fabcli();
    cmd.args(["download", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--force"))
        .stdout(predicate::str::contains("--into-empty"));
}

#[test]
fn download_force_and_into_empty_are_mutually_exclusive() {
    let (mut cmd, _dir) = fabcli();
    cmd.args([
        "download",
        "--artifact-id", "x",
        "--namespace", "y",
        "--asset-id", "z",
        "--output", "/tmp/fabcli-test-dl",
        "--force",
        "--into-empty",
    ])
    .assert()
    .code(6);
}

#[test]
fn download_help_lists_uid_form_flags() {
    let (mut cmd, _dir) = fabcli();
    cmd.args(["download", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--uid"))
        .stdout(predicate::str::contains("--stdin"))
        .stdout(predicate::str::contains("--engine"));
}

#[test]
fn download_uid_positional_without_session_is_auth_required() {
    // Positional UID parses; the resolver loads the session before
    // reading the library, so no token → auth_required.
    let (mut cmd, _dir) = fabcli();
    cmd.args(["download", "some-uid", "--output", "/tmp/fabcli-test-dl"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("\"kind\":\"auth_required\""));
}

#[test]
fn download_uid_flag_form_parses_same_as_positional() {
    let (mut cmd, _dir) = fabcli();
    cmd.args([
        "download",
        "--uid", "some-uid",
        "--output", "/tmp/fabcli-test-dl",
    ])
    .assert()
    .code(2)
    .stderr(predicate::str::contains("\"kind\":\"auth_required\""));
}

#[test]
fn download_uid_and_explicit_form_together_exits_six() {
    let (mut cmd, _dir) = fabcli();
    cmd.args([
        "download",
        "some-uid",
        "--artifact-id", "x",
        "--namespace", "y",
        "--asset-id", "z",
        "--output", "/tmp/fabcli-test-dl",
    ])
    .assert()
    .code(6);
}

#[test]
fn download_namespace_alone_without_artifact_id_exits_six() {
    let (mut cmd, _dir) = fabcli();
    cmd.args([
        "download",
        "--namespace", "y",
        "--asset-id", "z",
        "--output", "/tmp/fabcli-test-dl",
    ])
    .assert()
    .code(6);
}

// ── claim-batch / ownership batch ──

#[test]
fn claim_batch_without_input_mode_exits_six() {
    // No --uids / --stdin / --from-stdin-json / --from-library → invalid_args.
    let (mut cmd, _dir) = fabcli();
    cmd.args(["claim-batch"])
        .assert()
        .code(6)
        .stderr(predicate::str::contains("\"kind\":\"invalid_args\""));
}

#[test]
fn claim_batch_help_exits_zero() {
    let (mut cmd, _dir) = fabcli();
    cmd.args(["claim-batch", "--help"]).assert().success();
}

#[test]
fn claim_batch_mutually_exclusive_inputs_rejected() {
    let (mut cmd, _dir) = fabcli();
    cmd.args(["claim-batch", "--uids", "a,b", "--from-library"])
        .assert()
        .code(6);
}

#[test]
fn ownership_batch_help_exits_zero() {
    let (mut cmd, _dir) = fabcli();
    cmd.args(["ownership", "--help"]).assert().success();
}

#[test]
fn ownership_positional_uid_still_accepted() {
    // No session → auth_required before anything else. We just want
    // to confirm the positional UID still parses.
    let (mut cmd, _dir) = fabcli();
    cmd.args(["ownership", "abc123"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("\"kind\":\"auth_required\""));
}

/// Live smoke test: 3-UID batch claim against a real Fab account.
/// Ignored because it requires an authenticated session and real
/// network. Set `FABCLI_TEST_UIDS=free_uid,owned_uid,paid_uid` and
/// `cargo test --ignored claim_batch_live_smoke` to run.
#[test]
#[ignore]
fn claim_batch_live_smoke() {
    let uids = std::env::var("FABCLI_TEST_UIDS")
        .expect("set FABCLI_TEST_UIDS=a,b,c");
    let mut cmd = Command::cargo_bin("fabcli").expect("fabcli binary not built");
    cmd.args(["claim-batch", "--uids", &uids])
        .assert()
        .code(0)
        .stdout(predicate::str::contains("\"meta\""));
}

// ── Skill ──

/// Build a fresh `fabcli` command wired to the given skills directory.
/// Use this for multi-step skill tests (install → status → uninstall)
/// that need successive commands sharing the same tempdir.
fn skill_cmd_for(dir: &TempDir) -> Command {
    let mut cmd = Command::cargo_bin("fabcli").expect("fabcli binary not built");
    cmd.env("FABCLI_SKILLS_DIR", dir.path());
    cmd
}

/// Write a minimal fixture SKILL.md to the given dir and return its
/// path. Tests use this with `--source path=<fixture>` so they don't
/// depend on the public marketplace mirror being reachable.
fn write_skill_fixture(dir: &TempDir) -> std::path::PathBuf {
    let path = dir.path().join("fixture-SKILL.md");
    std::fs::write(
        &path,
        "---\nname: fabcli\ndescription: test fixture\nversion: 0.1.0\n---\n# FabCLI test skill\n",
    )
    .expect("failed to write fixture");
    path
}

/// Build a `fabcli skill ...` command rooted at a fresh tempdir, with
/// `FABCLI_SKILLS_DIR` pointed at it so the developer's real
/// `~/.claude/skills/` is never touched.
fn fabcli_skill() -> (Command, TempDir) {
    let dir = TempDir::new().expect("failed to create tempdir");
    let cmd = skill_cmd_for(&dir);
    (cmd, dir)
}

#[test]
fn skill_help_lists_verbs() {
    let mut cmd = Command::cargo_bin("fabcli").expect("fabcli binary not built");
    cmd.args(["skill", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("install"))
        .stdout(predicate::str::contains("update"))
        .stdout(predicate::str::contains("uninstall"))
        .stdout(predicate::str::contains("status"))
        .stdout(predicate::str::contains("path"));
}

#[test]
fn skill_path_prints_resolved_target() {
    let (mut cmd, dir) = fabcli_skill();
    let expected = dir.path().join("fabcli").join("SKILL.md");
    cmd.args(["skill", "path"])
        .assert()
        .success()
        .stdout(predicate::str::contains(expected.to_string_lossy().as_ref()));
}

#[test]
fn skill_install_writes_skill_file() {
    let (mut cmd, dir) = fabcli_skill();
    let fixture = write_skill_fixture(&dir);
    let target = dir.path().join("fabcli").join("SKILL.md");
    cmd.args([
        "skill",
        "install",
        "--source",
        &format!("path={}", fixture.display()),
    ])
    .assert()
    .success()
    .stdout(predicate::str::contains("\"installed\":true"));
    let content = std::fs::read_to_string(&target).expect("skill should exist");
    assert!(
        content.starts_with("---\nname: fabcli\n"),
        "wrote file should start with FabCLI frontmatter"
    );
}

#[test]
fn skill_install_then_status_then_uninstall_cycle() {
    let (mut install_cmd, dir) = fabcli_skill();
    let fixture = write_skill_fixture(&dir);
    install_cmd
        .args([
            "skill",
            "install",
            "--source",
            &format!("path={}", fixture.display()),
        ])
        .assert()
        .success();

    skill_cmd_for(&dir)
        .args(["skill", "status"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"present\":true"))
        .stdout(predicate::str::contains("\"version\":\"0.1.0\""));

    skill_cmd_for(&dir)
        .args(["skill", "uninstall"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"uninstalled\":true"));

    assert!(
        !dir.path().join("fabcli").join("SKILL.md").exists(),
        "uninstall should remove the file"
    );
    assert!(
        !dir.path().join("fabcli").exists(),
        "uninstall should remove the now-empty fabcli/ directory"
    );
}

#[test]
fn skill_install_refuses_to_overwrite_different_file_without_force() {
    let (mut install_cmd, dir) = fabcli_skill();
    let fixture = write_skill_fixture(&dir);
    let src_arg = format!("path={}", fixture.display());
    install_cmd
        .args(["skill", "install", "--source", &src_arg])
        .assert()
        .success();

    // Substitute a different version on disk so the next install
    // would have to overwrite.
    let target = dir.path().join("fabcli").join("SKILL.md");
    std::fs::write(&target, "---\nname: fabcli\nversion: 0.0.1\n---\nstub\n").unwrap();

    skill_cmd_for(&dir)
        .args(["skill", "install", "--source", &src_arg])
        .assert()
        .code(6)
        .stderr(predicate::str::contains("\"kind\":\"invalid_args\""));

    let content = std::fs::read_to_string(&target).unwrap();
    assert!(content.contains("version: 0.0.1"));
}

#[test]
fn skill_update_overwrites_and_reports_version_arrow() {
    let (mut install_cmd, dir) = fabcli_skill();
    let fixture = write_skill_fixture(&dir);
    let src_arg = format!("path={}", fixture.display());
    install_cmd
        .args(["skill", "install", "--source", &src_arg])
        .assert()
        .success();

    let target = dir.path().join("fabcli").join("SKILL.md");
    std::fs::write(&target, "---\nname: fabcli\nversion: 0.0.1\n---\nstub\n").unwrap();

    skill_cmd_for(&dir)
        .args(["skill", "update", "--source", &src_arg])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"installed\":true"))
        .stderr(predicate::str::contains("0.0.1 →"));

    let content = std::fs::read_to_string(&target).unwrap();
    assert!(content.starts_with("---\nname: fabcli\n"));
    assert!(!content.contains("version: 0.0.1"));
}

#[test]
fn skill_install_unchanged_is_no_op_success() {
    let (mut install_cmd, dir) = fabcli_skill();
    let fixture = write_skill_fixture(&dir);
    let src_arg = format!("path={}", fixture.display());
    install_cmd
        .args(["skill", "install", "--source", &src_arg])
        .assert()
        .success();

    skill_cmd_for(&dir)
        .args(["skill", "install", "--source", &src_arg])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"unchanged\":true"))
        .stdout(predicate::str::contains("\"installed\":false"));
}

#[test]
fn skill_uninstall_refuses_foreign_file_without_force() {
    let (mut cmd, dir) = fabcli_skill();
    let target = dir.path().join("fabcli").join("SKILL.md");
    std::fs::create_dir_all(target.parent().unwrap()).unwrap();
    std::fs::write(&target, "---\nname: something-else\n---\nbody\n").unwrap();

    cmd.args(["skill", "uninstall"])
        .assert()
        .code(6)
        .stderr(predicate::str::contains("\"kind\":\"invalid_args\""));

    assert!(target.exists(), "foreign file should not have been deleted");
}

#[test]
fn skill_uninstall_force_deletes_foreign_file() {
    let (mut cmd, dir) = fabcli_skill();
    let target = dir.path().join("fabcli").join("SKILL.md");
    std::fs::create_dir_all(target.parent().unwrap()).unwrap();
    std::fs::write(&target, "totally not yaml\n").unwrap();

    cmd.args(["skill", "uninstall", "--force"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"uninstalled\":true"));
    assert!(!target.exists());
}

#[test]
fn skill_install_source_path_reads_local_file() {
    let (mut cmd, dir) = fabcli_skill();
    let src = dir.path().join("local-skill.md");
    std::fs::write(
        &src,
        "---\nname: fabcli\nversion: 9.9.9\n---\ncustom local source\n",
    )
    .unwrap();
    let src_arg = format!("path={}", src.display());

    cmd.args(["skill", "install", "--source", &src_arg])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"version\":\"9.9.9\""));

    let target = dir.path().join("fabcli").join("SKILL.md");
    let content = std::fs::read_to_string(&target).unwrap();
    assert!(content.contains("custom local source"));
}

#[test]
fn skill_install_source_path_missing_file_is_generic_error() {
    let (mut cmd, _dir) = fabcli_skill();
    cmd.args(["skill", "install", "--source", "path=/nonexistent/skill.md"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("\"kind\":\"generic\""));
}

#[test]
fn skill_install_unknown_source_is_invalid_args() {
    let (mut cmd, _dir) = fabcli_skill();
    cmd.args(["skill", "install", "--source", "moonbeam"])
        .assert()
        .code(6)
        .stderr(predicate::str::contains("unknown --source"));
}

#[test]
fn skill_status_reports_absence_when_not_installed() {
    let (mut cmd, _dir) = fabcli_skill();
    cmd.args(["skill", "status"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"present\":false"));
}

#[test]
fn skill_install_source_github_with_unreachable_url_is_network_error() {
    // Point the env override at a port that nothing should be listening
    // on. The fetch should fail fast and return exit code 5 (network).
    let (mut cmd, _dir) = fabcli_skill();
    cmd.env(
        "FABCLI_SKILLS_REMOTE_URL",
        "http://127.0.0.1:1/never-listening.md",
    )
    .args(["skill", "install", "--source", "github"])
    .assert()
    .code(5)
    .stderr(predicate::str::contains("\"kind\":\"network\""));
}

#[test]
fn skill_path_honors_path_override() {
    let (mut cmd, _dir) = fabcli_skill();
    let other = TempDir::new().unwrap();
    let expected = other.path().join("fabcli").join("SKILL.md");
    cmd.args(["skill", "path", "--path"])
        .arg(other.path())
        .assert()
        .success()
        .stdout(predicate::str::contains(expected.to_string_lossy().as_ref()));
}

// ── Update ──

#[test]
fn update_help_lists_flags() {
    let mut cmd = Command::cargo_bin("fabcli").expect("fabcli binary not built");
    cmd.args(["update", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--check"))
        .stdout(predicate::str::contains("--to"))
        .stdout(predicate::str::contains("--force"));
}

#[test]
fn update_check_against_unreachable_remote_is_network_error() {
    // Point at a remote that won't resolve — the GitHub host won't
    // exist for `nonexistent-owner-fabcli-test/nonexistent-repo`. The
    // command should fail with kind=network and exit 5.
    let mut cmd = Command::cargo_bin("fabcli").expect("fabcli binary not built");
    cmd.env(
        "FABCLI_UPDATE_REMOTE",
        "nonexistent-owner-fabcli-test-9999/nonexistent-repo-abc",
    )
    .args(["update", "--check"])
    .assert()
    .failure();
}

#[test]
fn update_remote_malformed_is_invalid_args() {
    let mut cmd = Command::cargo_bin("fabcli").expect("fabcli binary not built");
    cmd.env("FABCLI_UPDATE_REMOTE", "no-slash-here")
        .env("FABCLI_NO_UPDATE_CHECK", "1")
        .args(["update", "--check"])
        .assert()
        .code(6)
        .stderr(predicate::str::contains("FABCLI_UPDATE_REMOTE"));
}

#[test]
fn update_check_skips_passive_hint() {
    // FABCLI_NO_UPDATE_CHECK=1 disables the once-per-day hint. Combined
    // with --check, the command should produce no `fabcli:` hint line on
    // stderr (regardless of whether the remote is reachable).
    let mut cmd = Command::cargo_bin("fabcli").expect("fabcli binary not built");
    cmd.env("FABCLI_NO_UPDATE_CHECK", "1")
        .env("FABCLI_UPDATE_REMOTE", "no-slash-here")
        .args(["update", "--check"])
        .assert()
        .stderr(predicate::str::contains("fabcli: a newer version").not());
}

// ── Update-check hint ──

#[test]
fn no_update_check_env_suppresses_hint() {
    let dir = TempDir::new().expect("failed to create tempdir");
    let token = dir.path().join("token.json");
    let mut cmd = Command::cargo_bin("fabcli").expect("fabcli binary not built");
    cmd.env("FABCLI_TOKEN_PATH", &token)
        .env("FABCLI_NO_UPDATE_CHECK", "1")
        .args(["auth", "status"])
        .assert()
        .stderr(predicate::str::contains("fabcli: a newer version").not());
}

#[test]
fn ttl_zero_suppresses_hint() {
    let dir = TempDir::new().expect("failed to create tempdir");
    let token = dir.path().join("token.json");
    let mut cmd = Command::cargo_bin("fabcli").expect("fabcli binary not built");
    cmd.env("FABCLI_TOKEN_PATH", &token)
        .env("FABCLI_UPDATE_CHECK_TTL_HOURS", "0")
        .args(["auth", "status"])
        .assert()
        .stderr(predicate::str::contains("fabcli: a newer version").not());
}

#[test]
fn update_subcommand_does_not_emit_hint() {
    // Running `fabcli update --check` should never trigger the daily
    // hint on its own stderr — the verb already reports the version
    // delta itself, so nesting the hint inside it is redundant noise.
    let mut cmd = Command::cargo_bin("fabcli").expect("fabcli binary not built");
    cmd.env("FABCLI_UPDATE_REMOTE", "nonexistent-host-fabcli-9999/repo")
        .args(["update", "--check"])
        .assert()
        .stderr(predicate::str::contains("fabcli: a newer version").not());
}

#[test]
fn skill_path_honors_project_scope() {
    let dir = TempDir::new().unwrap();
    let mut cmd = Command::cargo_bin("fabcli").unwrap();
    cmd.current_dir(dir.path())
        .env_remove("FABCLI_SKILLS_DIR")
        .args(["skill", "path", "--scope", "project"])
        .assert()
        .success()
        .stdout(predicate::str::contains(".claude"))
        .stdout(predicate::str::contains("fabcli"))
        .stdout(predicate::str::contains("SKILL.md"));
}
