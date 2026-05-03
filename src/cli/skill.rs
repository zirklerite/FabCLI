use crate::error::FabCliError;
use crate::output::print_json;
use clap::{Args, Subcommand, ValueEnum};
use serde_json::json;
use std::fs;
use std::path::{Path, PathBuf};

/// The skill is fetched from the public marketplace mirror at install
/// time — never embedded in the binary. Operators install/update by
/// pulling the latest published copy. If the URL is unreachable the
/// command fails fast with a clear network error.
const DEFAULT_REMOTE_URL: &str =
    "https://raw.githubusercontent.com/zirklerite/fabcli-skills/master/skills/fabcli/SKILL.md";

#[derive(Subcommand, Debug)]
pub enum SkillCommand {
    /// Write the FabCLI skill to the local Claude Code skills directory.
    Install(InstallArgs),
    /// Refresh an existing skill install (equivalent to `install --force`)
    /// with old → new version reporting.
    Update(InstallArgs),
    /// Remove the installed skill (and its empty parent directory).
    Uninstall(UninstallArgs),
    /// Report installed (and optionally remote) skill versions as JSON.
    Status(StatusArgs),
    /// Print the resolved install path (single line, no JSON envelope).
    Path(ResolverArgs),
}

#[derive(ValueEnum, Clone, Debug, Default)]
pub enum Scope {
    /// `~/.claude/skills/` — Claude Code's user-level skills directory.
    #[default]
    User,
    /// `<cwd>/.claude/skills/` — repo-local skills directory.
    Project,
}

/// Common path-resolution flags shared by every `skill` verb.
#[derive(Args, Debug)]
pub struct ResolverArgs {
    /// Install scope: `user` (default, ~/.claude/skills/) or `project`
    /// (./.claude/skills/ relative to cwd).
    #[arg(long, value_enum, default_value_t = Scope::User)]
    pub scope: Scope,
    /// Override the install directory entirely (highest priority).
    #[arg(long)]
    pub path: Option<PathBuf>,
}

#[derive(Args, Debug)]
pub struct InstallArgs {
    #[command(flatten)]
    pub resolver: ResolverArgs,
    /// Source for the skill content.
    /// `github` (default) fetches the latest published copy from the
    /// public `zirklerite/fabcli-skills` repo (overridable via
    /// `FABCLI_SKILLS_REMOTE_URL`). Fails with a network error if the
    /// repo is unreachable.
    /// `path=<file>` reads from a local file (offline / pre-staged
    /// install).
    #[arg(long, default_value = "github")]
    pub source: String,
    /// Overwrite an existing different file.
    #[arg(long)]
    pub force: bool,
}

#[derive(Args, Debug)]
pub struct UninstallArgs {
    #[command(flatten)]
    pub resolver: ResolverArgs,
    /// Skip the `name: fabcli` frontmatter check; delete unconditionally.
    #[arg(long)]
    pub force: bool,
}

#[derive(Args, Debug)]
pub struct StatusArgs {
    #[command(flatten)]
    pub resolver: ResolverArgs,
    /// Also fetch the latest version from the public GitHub repo.
    #[arg(long)]
    pub remote: bool,
}

pub async fn run(cmd: SkillCommand, pretty: bool) -> Result<(), FabCliError> {
    match cmd {
        SkillCommand::Install(args) => install(args, pretty, false).await,
        SkillCommand::Update(args) => install(args, pretty, true).await,
        SkillCommand::Uninstall(args) => uninstall(args, pretty),
        SkillCommand::Status(args) => status(args, pretty).await,
        SkillCommand::Path(resolver) => print_path(resolver),
    }
}

/// Resolve the directory under which `fabcli/SKILL.md` will be placed.
///
/// Priority (highest first):
///   1. `--path <dir>` explicit override
///   2. `FABCLI_SKILLS_DIR` env var
///   3. `--scope project` → `<cwd>/.claude/skills/`
///   4. `--scope user` (default) → `~/.claude/skills/`
fn resolve_skill_dir(resolver: &ResolverArgs) -> Result<PathBuf, FabCliError> {
    if let Some(p) = &resolver.path {
        return Ok(p.clone());
    }
    if let Ok(env) = std::env::var("FABCLI_SKILLS_DIR") {
        if !env.is_empty() {
            return Ok(PathBuf::from(env));
        }
    }
    match resolver.scope {
        Scope::Project => {
            let cwd = std::env::current_dir().map_err(|e| {
                FabCliError::Generic(format!("could not resolve cwd: {}", e))
            })?;
            Ok(cwd.join(".claude").join("skills"))
        }
        Scope::User => default_user_skills_dir(),
    }
}

/// `~/.claude/skills/` on both Windows and Linux.
///
/// Claude Code looks under `$HOME/.claude/`, not the OS app-config
/// directory — so we resolve the user's home directly rather than
/// using `directories::ProjectDirs` (which would return
/// `%APPDATA%\fabcli\` on Windows, the wrong place).
fn default_user_skills_dir() -> Result<PathBuf, FabCliError> {
    let home = home_dir().ok_or_else(|| {
        FabCliError::Generic("could not resolve user home directory".into())
    })?;
    Ok(home.join(".claude").join("skills"))
}

#[cfg(windows)]
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE").map(PathBuf::from)
}

#[cfg(not(windows))]
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

fn resolve_skill_file(skill_dir: &Path) -> PathBuf {
    skill_dir.join("fabcli").join("SKILL.md")
}

enum Source {
    Github,
    Path(PathBuf),
}

fn parse_source(s: &str) -> Result<Source, FabCliError> {
    if s == "github" {
        Ok(Source::Github)
    } else if let Some(rest) = s.strip_prefix("path=") {
        Ok(Source::Path(PathBuf::from(rest)))
    } else {
        Err(FabCliError::InvalidArgs(format!(
            "unknown --source '{}'; expected 'github' or 'path=<file>'",
            s
        )))
    }
}

async fn load_source(src: &Source) -> Result<String, FabCliError> {
    match src {
        Source::Github => fetch_remote().await,
        Source::Path(p) => fs::read_to_string(p).map_err(|e| {
            FabCliError::Generic(format!("failed to read {}: {}", p.display(), e))
        }),
    }
}

fn remote_url() -> String {
    std::env::var("FABCLI_SKILLS_REMOTE_URL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_REMOTE_URL.to_string())
}

async fn fetch_remote() -> Result<String, FabCliError> {
    let url = remote_url();
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| FabCliError::Network(format!("reqwest client: {}", e)))?;
    let resp = client.get(&url).send().await.map_err(|e| {
        FabCliError::Network(format!(
            "skill repo unreachable at {} ({}). Check your internet connection or set FABCLI_SKILLS_REMOTE_URL.",
            url, e
        ))
    })?;
    if !resp.status().is_success() {
        return Err(FabCliError::Network(format!(
            "skill repo at {} returned HTTP {}",
            url,
            resp.status()
        )));
    }
    resp.text().await.map_err(|e| {
        FabCliError::Network(format!("skill repo body unreadable at {}: {}", url, e))
    })
}

/// Extract a `version:` value from the leading `---`-delimited
/// frontmatter block. Returns `None` if the file has no frontmatter,
/// no `version:` line, or malformed YAML.
fn parse_version(content: &str) -> Option<String> {
    parse_frontmatter_field(content, "version")
}

fn parse_frontmatter_field(content: &str, field: &str) -> Option<String> {
    let mut lines = content.lines();
    if lines.next()? != "---" {
        return None;
    }
    for line in lines {
        if line == "---" {
            return None;
        }
        if let Some(rest) = line.strip_prefix(&format!("{}:", field)) {
            return Some(rest.trim().to_string());
        }
    }
    None
}

async fn install(args: InstallArgs, pretty: bool, is_update: bool) -> Result<(), FabCliError> {
    let dir = resolve_skill_dir(&args.resolver)?;
    let target = resolve_skill_file(&dir);
    let src = parse_source(&args.source)?;
    let new_content = load_source(&src).await?;
    let new_version = parse_version(&new_content);
    let force = args.force || is_update;

    let existing = match fs::read_to_string(&target) {
        Ok(s) => Some(s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            return Err(FabCliError::Generic(format!(
                "failed to read existing {}: {}",
                target.display(),
                e
            )))
        }
    };

    if let Some(prev) = &existing {
        if prev == &new_content {
            print_json(
                &json!({
                    "installed": false,
                    "unchanged": true,
                    "path": target.to_string_lossy(),
                    "version": new_version,
                }),
                pretty,
            );
            return Ok(());
        }
        if !force {
            return Err(FabCliError::InvalidArgs(format!(
                "target {} already exists with different content; pass --force (or use `update`) to overwrite",
                target.display()
            )));
        }
    }

    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).map_err(|e| {
            FabCliError::Generic(format!(
                "failed to create {}: {}",
                parent.display(),
                e
            ))
        })?;
    }
    fs::write(&target, &new_content).map_err(|e| {
        FabCliError::Generic(format!("failed to write {}: {}", target.display(), e))
    })?;

    if is_update {
        let prev_version = existing.as_deref().and_then(parse_version);
        if let (Some(old), Some(new)) = (prev_version.as_deref(), new_version.as_deref()) {
            if old != new {
                eprintln!("{} → {}", old, new);
            }
        }
    }

    print_json(
        &json!({
            "installed": true,
            "path": target.to_string_lossy(),
            "version": new_version,
        }),
        pretty,
    );
    Ok(())
}

fn uninstall(args: UninstallArgs, pretty: bool) -> Result<(), FabCliError> {
    let dir = resolve_skill_dir(&args.resolver)?;
    let target = resolve_skill_file(&dir);

    let content = match fs::read_to_string(&target) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            print_json(
                &json!({
                    "uninstalled": false,
                    "path": target.to_string_lossy(),
                    "reason": "not_present",
                }),
                pretty,
            );
            return Ok(());
        }
        Err(e) => {
            return Err(FabCliError::Generic(format!(
                "failed to read {}: {}",
                target.display(),
                e
            )))
        }
    };

    if !args.force {
        let name = parse_frontmatter_field(&content, "name");
        if name.as_deref() != Some("fabcli") {
            return Err(FabCliError::InvalidArgs(format!(
                "{} does not look like a FabCLI skill (frontmatter `name:` is {:?}); pass --force to delete anyway",
                target.display(),
                name
            )));
        }
    }

    fs::remove_file(&target).map_err(|e| {
        FabCliError::Generic(format!("failed to remove {}: {}", target.display(), e))
    })?;

    if let Some(parent) = target.parent() {
        // Best-effort: remove the empty fabcli/ wrapper directory. If
        // it still has siblings (unlikely), leave it.
        let _ = fs::remove_dir(parent);
    }

    print_json(
        &json!({
            "uninstalled": true,
            "path": target.to_string_lossy(),
        }),
        pretty,
    );
    Ok(())
}

async fn status(args: StatusArgs, pretty: bool) -> Result<(), FabCliError> {
    let dir = resolve_skill_dir(&args.resolver)?;
    let target = resolve_skill_file(&dir);

    let installed_content = fs::read_to_string(&target).ok();
    let installed_version = installed_content.as_deref().and_then(parse_version);

    let mut payload = json!({
        "installed": {
            "path": target.to_string_lossy(),
            "present": installed_content.is_some(),
            "version": installed_version,
        },
    });

    if args.remote {
        let url = remote_url();
        let remote_content = fetch_remote().await?;
        let remote_version = parse_version(&remote_content);
        let matches_remote = installed_content
            .as_deref()
            .map(|c| c == remote_content)
            .unwrap_or(false);
        payload["remote"] = json!({
            "url": url,
            "version": remote_version,
        });
        payload["matches_remote"] = json!(matches_remote);
    }

    print_json(&payload, pretty);
    Ok(())
}

fn print_path(resolver: ResolverArgs) -> Result<(), FabCliError> {
    let dir = resolve_skill_dir(&resolver)?;
    let target = resolve_skill_file(&dir);
    println!("{}", target.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library_cache::env_lock;
    use std::ffi::OsString;

    /// Save and restore an env var so tests don't bleed state. Tests
    /// using this MUST hold `env_lock()` first; env vars are
    /// process-global, and Cargo runs tests in parallel.
    struct EnvGuard {
        key: &'static str,
        previous: Option<OsString>,
    }
    impl EnvGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let previous = std::env::var_os(key);
            std::env::set_var(key, value);
            EnvGuard { key, previous }
        }
        fn unset(key: &'static str) -> Self {
            let previous = std::env::var_os(key);
            std::env::remove_var(key);
            EnvGuard { key, previous }
        }
    }
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.previous {
                Some(v) => std::env::set_var(self.key, v),
                None => std::env::remove_var(self.key),
            }
        }
    }

    fn resolver(scope: Scope, path: Option<PathBuf>) -> ResolverArgs {
        ResolverArgs { scope, path }
    }

    #[test]
    fn explicit_path_wins_over_env_and_scope() {
        let _g = env_lock().lock().unwrap();
        let _e = EnvGuard::set("FABCLI_SKILLS_DIR", "/tmp/from-env");
        let p = resolve_skill_dir(&resolver(Scope::User, Some(PathBuf::from("/tmp/explicit"))))
            .unwrap();
        assert_eq!(p, PathBuf::from("/tmp/explicit"));
    }

    #[test]
    fn env_wins_over_scope() {
        let _g = env_lock().lock().unwrap();
        let _e = EnvGuard::set("FABCLI_SKILLS_DIR", "/tmp/from-env");
        let p = resolve_skill_dir(&resolver(Scope::User, None)).unwrap();
        assert_eq!(p, PathBuf::from("/tmp/from-env"));
    }

    #[test]
    fn project_scope_uses_cwd() {
        let _g = env_lock().lock().unwrap();
        let _e = EnvGuard::unset("FABCLI_SKILLS_DIR");
        let p = resolve_skill_dir(&resolver(Scope::Project, None)).unwrap();
        let expected = std::env::current_dir().unwrap().join(".claude").join("skills");
        assert_eq!(p, expected);
    }

    #[test]
    fn user_scope_resolves_to_home_dot_claude_skills() {
        let _g = env_lock().lock().unwrap();
        let _e = EnvGuard::unset("FABCLI_SKILLS_DIR");
        let p = resolve_skill_dir(&resolver(Scope::User, None)).unwrap();
        let home = home_dir().expect("test host has HOME / USERPROFILE");
        assert_eq!(p, home.join(".claude").join("skills"));
    }

    #[test]
    fn skill_file_layout() {
        let p = resolve_skill_file(Path::new("/x/y"));
        assert_eq!(p, PathBuf::from("/x/y").join("fabcli").join("SKILL.md"));
    }

    #[test]
    fn empty_env_falls_back_to_scope() {
        let _g = env_lock().lock().unwrap();
        let _e = EnvGuard::set("FABCLI_SKILLS_DIR", "");
        let p = resolve_skill_dir(&resolver(Scope::Project, None)).unwrap();
        let expected = std::env::current_dir().unwrap().join(".claude").join("skills");
        assert_eq!(p, expected);
    }

    #[test]
    fn parse_source_variants() {
        assert!(matches!(parse_source("github").unwrap(), Source::Github));
        match parse_source("path=/tmp/foo.md").unwrap() {
            Source::Path(p) => assert_eq!(p, PathBuf::from("/tmp/foo.md")),
            _ => panic!("expected Source::Path"),
        }
        assert!(parse_source("nonsense").is_err());
        // Embedded source is gone — the binary no longer ships a baked-in copy.
        assert!(parse_source("embedded").is_err());
    }

    #[test]
    fn parse_version_happy_path() {
        let s = "---\nname: fabcli\nversion: 0.5.0\n---\nbody\n";
        assert_eq!(parse_version(s).as_deref(), Some("0.5.0"));
    }

    #[test]
    fn parse_version_missing_returns_none() {
        let s = "---\nname: fabcli\n---\nbody\n";
        assert_eq!(parse_version(s), None);
    }

    #[test]
    fn parse_version_no_frontmatter_returns_none() {
        let s = "no frontmatter here\nversion: 1.0.0\n";
        assert_eq!(parse_version(s), None);
    }

    #[test]
    fn parse_version_malformed_returns_none() {
        let s = "---\nname: fabcli\n";
        assert_eq!(parse_version(s), None);
    }

    #[test]
    fn remote_url_default_when_unset() {
        let _g = env_lock().lock().unwrap();
        let _e = EnvGuard::unset("FABCLI_SKILLS_REMOTE_URL");
        assert_eq!(remote_url(), DEFAULT_REMOTE_URL);
    }

    #[test]
    fn remote_url_honors_env() {
        let _g = env_lock().lock().unwrap();
        let _e = EnvGuard::set("FABCLI_SKILLS_REMOTE_URL", "http://localhost:9/x.md");
        assert_eq!(remote_url(), "http://localhost:9/x.md");
    }
}
