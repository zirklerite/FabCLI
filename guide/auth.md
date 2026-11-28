# Auth

FabCLI logs in to Epic once; every command after that is headless.

| Command | What it does |
|---|---|
| `fabcli auth login` | Epic sign-in in one window (credentials + 2FA). `--manual` for the paste flow, same access. Returns at once, with no window, when the session is still valid. |
| `fabcli auth status` | Session health, headless: `{"authenticated", "expires_at", "refreshed", "fab": {"mode", "session_ok"}}` |
| `fabcli auth whoami` | `account_id`, `display_name`, `email` |
| `fabcli auth logout` | Ends the Epic session and deletes the token, keystore key and library cache |

- The Epic refresh token lasts about a year. Commands refresh the
  access token themselves.
- Fab-only calls (`claim`, rich `ownership`, `search --with-ownership`)
  mint a Fab web session from the Epic token in memory. There is no
  separate Fab login.
- **Exit 2 (`auth_required`):** run `fabcli auth login` yourself. The
  window is for the user; from your side it is a long-running command.
  Allow a couple of minutes for 2FA.
- `fab.session_ok: false` with `authenticated: true` means Fab rejects
  the token: run `auth login` again.
- The token file is encrypted (DPAPI on Windows, libsecret on Linux).
  Don't read it; use `auth status`. `FABCLI_TOKEN_PATH` points FabCLI
  at another token file (another account, or tests).
- On Linux, "OS keystore unavailable" means the keyring daemon isn't
  running (`gnome-keyring-daemon`).

**`fabcli` not found?** The user hasn't put it on PATH yet: in the
unpacked release folder, `install.bat` (Windows) or `./install.sh`
(Linux) does it. That changes their PATH, so suggest it rather than
running it unasked; they open a new terminal afterwards.

Playbook: `first-run`.
