# FabCLI v0.2.1

Command-line tool for the Epic Games Store and Fab marketplace, designed for AI coding agents (Claude Code, Cursor, Codex CLI, Gemini CLI, Aider) and usable by humans directly.

## Supported platforms

- Windows x64 — fabcli-v0.2.1-windows64.zip
- Linux x64 (Ubuntu 24.04 LTS) — fabcli-v0.2.1-linux64.tar.gz (runtime deps: libgtk-3-0t64 libwebkit2gtk-4.1-0 libsoup-3.0-0)

## Upgrading

From v0.2.0: `fabcli update`. Then `fabcli skill update` to get the matching Claude Code skill.

From v0.1.0: install this release by hand once (v0.1.0's `fabcli update` can't read the current archive layout); `fabcli update` works from then on.

## One-step install

The archives now include an installer for the first install. Unpack the archive, then run it from the unpacked folder:

- **Windows:** `install.bat` puts the folder on your user PATH (no admin rights). It shows the change and asks first, backs up the old PATH, and writes every other entry back exactly as it was. An older FabCLI release folder on PATH is replaced, not duplicated. `install.bat --yes` skips the question; `install.bat status` shows which `fabcli` a new terminal runs; `install.bat uninstall` removes only FabCLI's entry.
- **Linux:** `./install.sh` links `~/.local/bin/fabcli` to the unpacked binary (no sudo, no profile edits) and prints the `apt` command if a runtime library is missing. `status` and `uninstall` as on Windows.

Both print the next steps: `fabcli auth login`, then `fabcli skill install` for Claude Code. Later upgrades still go through `fabcli update`; there is no need to run the installer again. The manual PATH steps in README.md still work.

## Guidance for agents, built into the binary

- **`fabcli guide [<topic>]`** prints a short primer (output contract, exit codes, pipelines, claim safety rules), or one topic: `auth`, `search`, `library`, `claim`, `download`.
- **`fabcli playbook list [--tag <tag>]`** and **`fabcli playbook show <name>`** give step-by-step recipes as JSON: the exact command for each step, what to check, which fields to keep, and what to do on known errors. Ten to start with, including `first-run`, `search-and-inspect`, `download-owned`, `claim-free-batch` and `claim-promos`.
- None of these need a login, and they always match the binary you run. The Claude Code skill is smaller now and points at them.

## Verification

SHA256SUMS.txt ships alongside the archives. `fabcli update` verifies the archive's SHA-256 against it automatically.

## Disclaimer

Unofficial third-party tool — not affiliated with Epic Games or Fab. Upstream APIs are undocumented and may change without notice. Driving them may breach Epic's/Fab's ToS and put your account at risk. Use modestly, prefer the cache over parallel fan-out, and don't share an account between automation drivers. Provided as-is under GPL-3.0-or-later with no warranty.
