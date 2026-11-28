# FabCLI — FAQ

Reference for users who already have FabCLI installed and want to
understand how it works, what it can and can't do, and how to recover
from common issues.

For install instructions see [`README.md`](README.md).

---

## About FabCLI

### What is FabCLI?

An AI-agent-first command-line tool for the Epic Games Store and the
Fab marketplace. It lets you (or an AI coding assistant — Claude
Code, Cursor, Codex CLI, Gemini CLI, Aider, etc.) search, inspect,
claim, and download Fab assets through composable shell commands.

Surface properties:

- **Compact JSON on stdout** — every command emits parseable JSON by
  default. Add `--pretty` only when a human is reading.
- **Structured errors on stderr** — failures emit
  `{"error":{"kind":"…","message":"…"}}` so an agent can branch on
  `kind` instead of grepping prose.
- **Meaningful exit codes** — see the table below.
- **Headless after one login** — `fabcli auth login` is the only
  interactive command. Every other invocation runs without prompts,
  TTY checks, or browser pop-ups.
- **`--stdin` on every single-ID command** — pipelines compose
  cleanly: `fabcli search … | jq -r '.results[0].uid' | fabcli listing --stdin`.
- **Batch endpoints** — `claim-batch`, `claim-promo-batch`,
  `ownership --batch`, `--from-stdin`, `--from-library` reuse one Fab
  session across all UIDs. Use them whenever you have more than two
  UIDs to process.
- **Hard safety rails on `claim`** — see the safety section below.

### Heads-up: FabCLI talks to undocumented APIs

Epic Games and Fab don't publish a public, versioned API contract for
the endpoints FabCLI uses. Field names, response shapes, auth flows,
and even entire endpoints can — and historically have — changed
without notice. When that happens, individual commands will start
returning errors, malformed JSON, or wrong results until FabCLI
catches up.

A FabCLI release that worked perfectly last month is **not**
guaranteed to work today. Pin a known-good version for production
automation, watch the
[Releases page](https://github.com/zirklerite/FabCLI/releases) for
compatibility patches, and treat any sudden cross-command failure as
"Fab probably moved something" before assuming it's a bug in your
script.

### Is there a companion project for installing assets into UE projects?

**UE5CLI** — planned, not yet released. Once published, it will pair
with FabCLI so agents can chain `fabcli download` into
`ue5cli install-asset` and take a marketplace search result all the
way into a working UE project. For now, `fabcli download` writes the
asset files; importing them into a UE project is a manual step.

### What library does FabCLI use to talk to Epic / Fab?

[`egs-api-rs`](https://github.com/zirklerite/egs-api-rs) — a
**maintenance fork** of upstream
[`achetagames/egs-api-rs`](https://github.com/achetagames/egs-api-rs).
The fork carries a small `rustls` patch so the binary cross-compiles
from Windows to Linux without a system OpenSSL dependency, and
periodically rebases on upstream HEAD to pick up Epic / Fab API
fixes. Original project credit goes to the upstream authors;
FabCLI's own source lives in this repo.

### Can I use FabCLI with Cursor / Codex CLI / Gemini CLI / Aider / etc.?

Yes — the binary itself is agent-agnostic, you just call `fabcli`
from any shell-driving agent. The simplest way to teach any agent is
the guidance built into the binary: `fabcli guide` (a short primer
plus topics) and `fabcli playbook list` / `fabcli playbook show
<name>` (step-by-step recipes). Put this line in the agent's
instruction file (`AGENTS.md`, `GEMINI.md`, `.cursorrules`, ...):

```
Before using fabcli, run `fabcli guide` and follow the matching `fabcli playbook`.
```

The full skill is more detailed. **Auto-discovery** of it (when to
reach for FabCLI, command shapes, exit codes) is Claude-Code-only
today.

`fabcli skill install` writes to `~/.claude/skills/fabcli/SKILL.md`,
which only Claude Code reads. The format (YAML frontmatter +
markdown body) is Claude's plugin-skill schema.

For other agents:
- **Cursor:** copy the SKILL.md into `.cursorrules` (strip the
  YAML frontmatter at the top first; Cursor expects bare markdown).
- **Codex CLI / OpenAI's CLI:** drop into `AGENTS.md` at the repo
  root or under `~/.codex/`.
- **Gemini CLI / Google's CLI:** drop into `GEMINI.md` at the repo
  root (Gemini CLI auto-loads it on startup, similar to how Claude
  Code auto-loads `CLAUDE.md`).
- **Aider:** add to your conventions file (`.aider.conf.yml`
  references it).
- **Cline / Continue.dev / others:** consult your tool's
  rules/conventions docs.

To get a clean copy you can paste:

```bash
fabcli skill install --path /tmp/fabcli-export
cat /tmp/fabcli-export/fabcli/SKILL.md
```

The CONTENT (command reference, exit codes, recipes, JSON shapes)
is portable across agents — only the frontmatter and install
location are Claude-specific.

---

## Authentication

### How do I sign in?

```bash
fabcli auth login
```

This opens a small embedded browser window (WebView2 on Windows,
WebKitGTK on Linux) pointed at Epic's sign-in page. You enter your
Epic email + password and complete 2FA if your account has it
enabled. One login lasts about a year.

What happens behind the scenes:

1. Epic redirects to its authorization-code endpoint; FabCLI
   intercepts the redirect inside the WebView and captures the code
   in-process — no copy-paste, no terminal prompt.
2. FabCLI exchanges the code for an Epic OAuth token pair (access +
   refresh) and writes it, encrypted, to your per-user config
   directory. That is the only session FabCLI keeps.
3. The window closes.

Commands that need a Fab web session (`claim`, `claim-batch`,
`claim-promo`, rich `ownership`, `search --with-ownership`) mint one
from the Epic access token when they run. The Fab session lives in
memory for that one invocation and is never written to disk.
FabCLI's refresh token is never sent to Fab.

If the Epic session is already valid when you run `auth login`, no
window opens — FabCLI prints
`{"ok":true,"already_authenticated":true,…}` and exits in
milliseconds.

### How long does a login last?

| Token | Lifetime | Auto-refresh? |
|---|---|---|
| **Epic access token** | ~36 h | **Yes** — every command silently refreshes it with the refresh token |
| **Epic refresh token** | ~1 year | Rotated on every refresh |

As long as Epic keeps the current ~1-year refresh-token policy and
you run *any* FabCLI command at least once a year, you never need to
log in again. There is no separate Fab expiry: earlier releases kept
a 90-day Fab cookie and needed a second login stage, which is gone.

### How do I check if my session is still valid?

```bash
fabcli auth status --pretty
```

Sample output:

```json
{
  "authenticated": true,
  "expires_at": "2026-11-28T20:17:39+00:00",
  "refreshed": true,
  "fab": {
    "mode": "token",
    "session_ok": true
  }
}
```

`fab.session_ok` says whether Fab minted a web session from the Epic
token just now. `false` with `authenticated: true` means Fab is
rejecting the token: re-run `fabcli auth login`, and if that doesn't
help, try the fallback below.

**Changed in v0.2.0:** the `fab` block no longer has
`session_present`, `expires_at`, `days_remaining` or `needs_refresh`
in the default mode. Scripts should branch on `fab.mode`.

### What if Fab stops accepting the Epic token?

Set `FABCLI_FAB_AUTH=webview` to restore the pre-v0.2.0 behaviour:
`auth login` adds a second Fab sign-in stage, session-gated commands
run through a hidden WebView (about 1–2 s per call), the Fab session
expires after 90 days, and `auth status` reports the old fields plus
`"mode":"webview"`. Please report the problem if you need it. This
fallback is deprecated and will be removed in a later release.

### What happens when authentication fails?

If your session has expired, FabCLI exits with **code 2**
(`auth_required`) and a structured error on stderr:

```json
{"error":{"kind":"auth_required","message":"session expired. Run 'fabcli auth login' to refresh."}}
```

Re-run `fabcli auth login` and retry the original command. Scripts
should treat exit code 2 as "ask the user to re-authenticate."

### Can I authenticate without a graphical environment?

Yes. `fabcli auth login --manual` runs a paste flow: open the
printed URL in any browser, sign in, and paste the code back. It
needs an interactive terminal but no WebView, and it gives full
access, including `claim` and rich `ownership`.

The one exception is `claim-promo` / `claim-promo-batch`: Epic's
checkout step carries hCaptcha and runs in a WebView, so they need a
desktop session.

### What are the auth subcommands?

| Command | What it does |
|---|---|
| `fabcli auth login` | Epic login via embedded WebView (`--manual` for the paste flow). Skips the window if the session is already valid. |
| `fabcli auth status` | Print the session's health as JSON, including whether Fab accepts it. Headless. |
| `fabcli auth whoami` | Print `account_id`, `display_name`, and `email` as JSON. |
| `fabcli auth logout` | Invalidate the Epic session, delete the local token, wipe the WebView data folder, and clear the library cache. |

---

## Commands

### Search & browse

```bash
fabcli search -q "medieval kitbash" --filter is_free=1 --filter channels=unreal-engine
fabcli search --filter min_discount_percentage=100  # Limited Time Free / "Free for the Month"
fabcli search --filter is_free=1 --filter published_since=2026-04-23  # new free assets since a date
fabcli search --filter styles=anime --filter styles=lowpoly  # multi-style (AND)
fabcli search -q "lamp" --with-ownership     # decorate each result with owned: true/false
fabcli library                               # list owned assets
fabcli library --since 2026-11-01 --sort newest   # added since a date (server-side filter)
fabcli library --query castle --seller <sellerId> # more: --exclude-seller, --license, --tag, --type, --category, --format, --min-rating, --limit
fabcli sellers "PolyOne"                     # look up a seller's sellerId for --seller
fabcli listing <uid>                         # full detail for one asset (incl. aiGenerated / aiTrainingForbidden)
fabcli formats <uid>                         # UE versions & platforms
fabcli prices <uid>                          # pricing info (incl. limitedTimeFree / freeUntil)
fabcli reviews <uid>                         # user reviews
```

`--with-ownership` adds an `owned: bool` field to every result row.
Batches the result UIDs through Fab's bulk listings-states endpoint
(1 HTTP call per ~24 results, <2s for any typical search size).

Plain `fabcli library` walks your whole library (~100 s on a
1k-item library; set `FABCLI_LIBRARY_CACHE=1` to cache it). Any of
the filter flags switches to Fab's server-side library search, which
answers in seconds and marks its output `meta.source: "agents"`.

### Ownership

```bash
fabcli ownership <uid>                       # do I own this?
fabcli ownership --batch uid1,uid2,uid3      # batch
fabcli ownership --from-library              # every UID in the library
```

All ownership-flavored commands require you to be logged in.
Exit 2 with `auth_required` if your session has expired or is
missing — re-login is fast (already-valid sessions short-circuit
in <1 s). There is no library-walk fallback for ownership state;
the slow surprise path was retired in favor of the predictable
hard-fail.

For "which of my search results do I already own?", reach for
`fabcli search --with-ownership` instead — one command, one JSON,
no client-side merge.

### Claim free assets

```bash
fabcli claim <uid>                           # add a free asset to library
fabcli claim-batch --uids uid1,uid2,uid3     # batch claim
fabcli claim-batch --from-library            # re-verify whole library
```

`claim` only works on permanently free listings. For a paid listing
discounted 100% for a limited time, it answers
`{"ok":false,"reason":"promo_only",…}` (exit 0) without sending
anything; use `claim-promo` for those.

### Claim "Free for the Month" assets

```bash
fabcli claim-promo <uid>                     # claim one 100%-off promo asset
fabcli search --filter min_discount_percentage=100 | fabcli claim-promo-batch --from-stdin-json   # this month's set
```

Fab only lets Limited Time Free assets be claimed through checkout,
so `claim-promo` runs Fab's cart and Epic's checkout at a total of
exactly 0. Epic's checkout carries hCaptcha, so it runs in a hidden
window that appears only if hCaptcha asks you to solve a challenge.
It takes about 20 s per asset and needs a desktop session. See the
safety section below for how it avoids paid orders.

### Download

```bash
fabcli download <uid> -o ./my-asset/                                   # UID form (recommended)
fabcli download <uid> -o ./my-asset/ --engine UE_5.4                   # disambiguate multi-version
fabcli download ... -o ./existing/ --force                             # overwrite existing files
fabcli download ... -o ./empty/   --into-empty                         # refuse if dir is non-empty
```

By default, `download` refuses to overwrite pre-existing files in
`--output` — use `--force` to opt in, or `--into-empty` for
clean-install workflows. If the listing exposes multiple engine
versions, the error message lists them so you can pick one with
`--engine` (and `--platform` if that still leaves several). Run
`fabcli download --help` for the full flag set (including
`--jobs` and the legacy explicit-IDs form).

Every chunk and, when the manifest provides it, every file is
hash-checked. Files are assembled as `*.fabcli-partial` and renamed
into place only after they pass, so a failed download never leaves a
truncated file behind. Transient network errors are retried.

### Self-update

```bash
fabcli update                                # install the latest stable release, if newer
fabcli update --check                        # report running vs latest, no download
fabcli update --force                        # re-install the latest release
fabcli update --to 0.2.0                     # pin to a specific tag (forward or back)
```

`fabcli update` verifies the archive against the release's
`SHA256SUMS.txt` before installing it. Upgrading from v0.1.0 is the
exception: v0.1.0's updater can't install later releases, so install
v0.2.0 by hand once.

### Skill management (Claude Code)

```bash
fabcli skill install                         # fetch the latest skill from GitHub, install into ~/.claude/skills/
fabcli skill status [--remote]               # report installed version (and optionally compare against remote)
fabcli skill update                          # re-fetch and overwrite installed copy
fabcli skill uninstall                       # remove the installed skill
fabcli skill path                            # print the resolved install path
```

`install` and `update` always fetch from
`https://raw.githubusercontent.com/zirklerite/fabcli-skills/master/skills/fabcli/SKILL.md`
(override via `FABCLI_SKILLS_REMOTE_URL`). If the URL is unreachable the
command exits with code 5 and a clear `network` error. For offline
installs use `fabcli skill install --source path=<file>`.

### Guide and playbooks

```bash
fabcli guide                        # short primer, lists topics and playbooks
fabcli guide claim                  # one topic: auth, search, library, claim, download
fabcli playbook list [--tag claim]  # step-by-step recipes
fabcli playbook show claim-promos   # one recipe: inputs, steps, checks, error advice
```

Built into the binary (always matching your version), JSON output, no
login and no network needed. Each step gives the exact command and a
ready shell `line`.

### Pipelines

Every single-UID command also accepts `--stdin` so pipelines compose:

```bash
fabcli search -q "rock" --filter is_free=1 | jq -r '.results[0].uid' | fabcli listing --stdin --pretty
```

Add `--pretty` to any command for human-readable JSON. Run
`fabcli <subcommand> --help` for the full flag list.

---

## Safety

### Can FabCLI charge my credit card?

**No.** FabCLI only ever claims assets that cost nothing, and each
path that adds an asset to your library has a guard that no flag,
env var or debug switch can bypass:

1. **`claim` / `claim-batch` use Fab's free-claim endpoint.**
   `add-to-library` takes an `offerId` only, not payment
   information. Before sending it, FabCLI checks that the listing is
   permanently free (`isFree=true` or `price=0`). A paid listing
   gets `{"ok":false,"reason":"not_free", …}`, and a paid listing
   on a 100%-off promo gets `reason: "promo_only"`; in both cases
   the POST is never sent. Sending a paid `offerId` would also be
   rejected by Fab itself.
2. **`claim-promo` / `claim-promo-batch` place orders, but only at a
   total of exactly 0.** Promo assets can only be claimed through
   checkout, so these commands do run Epic's checkout. Before
   placing the order, FabCLI reads what Epic's own checkout reports
   for this purchase, and places it only if the total is 0 and the
   cart holds exactly the one offer being claimed. Anything else is
   refused with `reason: "non_zero_total"` before any order, and the
   cart is cleared. This check lives in one module, and a test fails
   if order-placing code appears anywhere else in the source.
3. **FabCLI never calls a purchase API directly.** The underlying
   `egs-api-rs` crate (see *"What library does FabCLI use to talk
   to Epic / Fab?"* above) exposes purchase functions
   (`quick_purchase`, the `store` facade); FabCLI doesn't use them.
4. **`download` is read-only.** It fetches chunks from signed CDN
   URLs for assets in your library. The CDN rejects requests for
   assets you don't own, so `download` can't grant ownership.

The worst case with FabCLI is "I tried to claim a paid asset and got
a `not_free` (or `non_zero_total`) response."

The honest caveat: this guarantee depends on running the
**unmodified upstream binary**. A fork or tampered build could strip
these checks. Verify the SHA-256 of release archives against
`SHA256SUMS.txt` on the
[Releases page](https://github.com/zirklerite/FabCLI/releases), or
build from source yourself.

---

## Exit codes

| Code | Meaning | What scripts should do |
|---|---|---|
| 0 | success | parse stdout JSON normally |
| 1 | generic / unexpected failure | report the error message |
| 2 | authentication required | tell user to run `fabcli auth login` |
| 3 | requested resource not found | the listing/asset doesn't exist — check the UID |
| 4 | rate limited by upstream | wait ~30 s and retry |
| 5 | network / transport failure | report connectivity issue |
| 6 | invalid command-line arguments | fix the command (wrong flags, missing UID, etc.) |

Errors are emitted on stderr as
`{"error":{"kind":"<kind>","message":"<description>"}}`.

---

## Token storage

The OAuth session is persisted to a per-user config directory:

- **Windows:** `%APPDATA%\fabcli\token.json`
- **Linux:** `$XDG_CONFIG_HOME/fabcli/token.json` (usually
  `~/.config/fabcli/token.json`)

### How is the token file stored on disk?

By default, the file is an **AES-256-GCM ciphertext** sealed with
a key stored in your **OS user keystore**:

- **Windows:** the key lives in Windows Credential Manager (DPAPI).
- **Linux (Ubuntu 24.04 desktop):** the key lives in
  `libsecret` / GNOME Keyring.

Decryption is bound to "this user account, on this machine."
Copying the encrypted file to another machine — or to a different
user on the same machine — produces an unreadable blob.

The file begins with a magic header (`FABCLI…`) so a stray
non-FabCLI file at the same path is rejected with a clear error
rather than silently misparsed.

### What's the trust boundary?

"Only this user, on this machine." Any process running as you on
the same machine can spawn FabCLI, which decrypts on its behalf.
This is the OS trust boundary every headless CLI in this class
operates within (`gh`, `az`, `gcloud`, `doctl`, `1password-cli`).

> **"Can you encrypt the token so only `fabcli` can read it?"**
> Not on a single-user OS, no. Any process running as you can
> simply call `fabcli auth status` (which reads the token to
> answer). Embedded keys, code-sign checks, and similar tricks
> all lose to "the attacker just runs the legitimate fabcli
> binary." DPAPI / libsecret give us "this user, this machine" —
> that's the genuine ceiling.

### Can I run FabCLI without leaving anything on disk after logout?

Yes, by redirecting the token file onto RAM-only tmpfs storage:

```bash
# Linux (Ubuntu desktop):
export FABCLI_TOKEN_PATH=/run/user/$UID/fabcli/token.json
mkdir -p $(dirname $FABCLI_TOKEN_PATH)
fabcli auth login
# ... do your work ...
fabcli auth logout    # also deletes the keystore entry
# On system logout, /run/user/$UID/ is wiped automatically.
```

`/run/user/$UID/` is a tmpfs mount that the OS clears on logout.
The token file is encrypted there (same as anywhere else); the
keystore entry holding the AES key is deleted by `fabcli auth
logout`. Net result: no FabCLI state survives the session.

**Trade-off:** you must re-run `fabcli auth login` after every
logout / reboot.

**What this is NOT:** truly zero-anything storage with continued
headless operation is impossible. Epic OAuth needs the refresh
token persisted somewhere to renew access tokens silently. If we
deleted the refresh token after each command, every command would
need an interactive WebView login from scratch, which defeats the
whole headless design.

### Multi-account workflows

Set `FABCLI_TOKEN_PATH=/path/to/alt-token.json` to point FabCLI at
a different file. Sibling state (WebView data, library cache,
update-check cache) follows the override automatically.

---

## Environment variables

| Variable | Effect |
|---|---|
| `FABCLI_TOKEN_PATH` | Override the token file path. Sibling state (WebView data, library cache, update-check cache) follows the override automatically. |
| `FABCLI_FAB_AUTH=token\|webview` | How Fab-session commands authenticate. `token` (default) mints the Fab session from the Epic token. `webview` is the deprecated pre-v0.2.0 fallback: a second Fab login stage and a hidden WebView per call. |
| `FABCLI_LIBRARY_CACHE=1` | Enable the opt-in on-disk library cache. See `fabcli library --help` for the per-call flags `--cache` / `--no-cache` / `--refresh` / `--clear`. |
| `FABCLI_LIBRARY_CACHE_TTL=<secs>` | TTL for cached library reads (default 86400 s = 24 h; `0` disables reads). |
| `FABCLI_NO_TIPS=1` | Suppress the once-per-24h stderr tip pointing at `FABCLI_LIBRARY_CACHE` after a slow library fetch. |
| `FABCLI_FAB_SESSION_WARN_DAYS=<days>` | `webview` mode only: threshold for the Fab-session-expiry warning on stderr (default 7). `0` disables. |
| `FABCLI_NO_UPDATE_CHECK=1` | Disable the once-per-day "newer version available" stderr hint. Useful in CI / scripted contexts. |
| `FABCLI_UPDATE_CHECK_TTL_HOURS=<int>` | Override the 24-hour cache TTL on the daily update check. `0` disables. |
| `FABCLI_SKILLS_DIR=<dir>` | Redirect `fabcli skill {install,update,uninstall,status,path}` to write/read at `<dir>/fabcli/SKILL.md` instead of `~/.claude/skills/fabcli/SKILL.md`. |

---

## Disclaimer & user responsibility

FabCLI is an **unofficial, third-party** tool. It is **not affiliated
with, endorsed by, or sponsored by Epic Games, Inc., the Epic Games
Store, or Fab.** Names, logos, and APIs referenced here belong to
their respective owners. Use of FabCLI is subject to Epic Games' and
Fab's Terms of Service — make sure your usage complies with them.

By using FabCLI you accept full responsibility for:

- **Your account.** FabCLI authenticates against your real Epic Games
  / Fab account. The OAuth refresh token (~1 year) is persisted on
  disk under your user account. The Fab web session is kept in
  memory only, unless you opt into `FABCLI_FAB_AUTH=webview`, which
  stores a 90-day Fab cookie in the WebView data folder.
  By default the token file is encrypted at rest using the OS user
  keystore (DPAPI on Windows, libsecret on Linux), so a copy of the
  file is useless on another machine or under another user account.
  However, **any process running as you on this machine can ask
  FabCLI to use the credentials** — that's the trust boundary the
  operating system gives us; "only fabcli can read it" is not
  achievable on a single-user OS. The WebView data folder is
  similarly protected by the underlying browser engine. Treat all of these as
  **password-equivalent secrets** when considering machine-level
  trust: anyone who can run code as your local user can act as you
  on Fab and Epic until you run `fabcli auth logout` or rotate
  the credentials by signing out on Epic's side. See **Token
  storage** above for the full storage model.
- **Your machine.** FabCLI runs whatever commands you (or an AI
  agent acting on your behalf) ask it to. It writes downloaded
  assets to the directories you specify. It does not sandbox the
  agent driving it; an agent compromised by prompt injection in
  third-party metadata (asset titles, descriptions, seller names)
  could attempt to make decisions you wouldn't. You are responsible
  for reviewing what gets claimed, downloaded, and installed.
- **Your purchases (or absence thereof).** `claim` is hard-blocked
  in source from POSTing to Fab's `add-to-library` endpoint for any
  non-free listing, and `claim-promo` places an order only when
  Epic's checkout reports a total of exactly 0. Those guards rely on
  the binary you are running
  being the unmodified upstream FabCLI. **If you build from a fork,
  run a tampered binary, or modify the source, that guarantee no
  longer holds.** Verify the binary you run against the published
  `SHA256SUMS.txt`.

  **Residual API-change risk — not covered by FabCLI's guarantees.**
  FabCLI's free checks inspect the listing and checkout fields *as
  Fab and Epic return them today*. Epic and Fab can — without
  notice — change field semantics, add new payment-gating flags
  FabCLI doesn't read, or repurpose the `add-to-library` endpoint to
  charge saved payment methods. In any of those scenarios FabCLI's
  checks could pass on a claim that the server then treats as paid.
  **The maintainer accepts no liability for charges resulting from
  upstream API changes.** If you only want to claim free assets and
  want the risk floored to zero, **remove all saved payment methods
  from your Fab / Epic Games account** — that converts the residual
  risk from "card charged" to "request rejected." You may also pin
  a known-good FabCLI version (`fabcli update --to <version>`) so
  the binary you run today doesn't silently change tomorrow, but
  no compatibility patch is promised on any timeline.
- **Compliance and account-suspension risk.** Whether automated
  marketplace operations are permitted by Epic Games' / Fab's
  Terms of Service is your call to make and your obligation to
  verify. **Driving these undocumented endpoints from FabCLI may
  breach those terms and lead to account suspension or other
  action against your account. By using FabCLI you accept that
  risk; the project authors disclaim all liability for account
  actions taken against you.** FabCLI ships no rate limiting
  beyond what the upstream API enforces — keep cadences modest,
  prefer the cache, avoid parallel fan-out, and don't share an
  account between multiple automation drivers.
- **Data loss.** `download` writes files. `auth logout` deletes the
  token, WebView data, and library cache. Back up anything you care
  about; commands are not transactional.

FabCLI is **provided "as is", without warranty of any kind**, as
spelled out in the GPL v3.0 license text (sections 15 and 16). **No
guarantee** is made that the tool works correctly against any
specific version of the Epic Games or Fab APIs, that downloads are
complete or uncorrupted, that claim operations succeed, or that any
upstream behaviour will continue to exist. Epic and Fab are free to
change their APIs at any time and FabCLI may break without notice.

If you discover a security issue, please **do not** open a public
issue. Contact the maintainer privately first so they have a chance
to ship a fix before disclosure.

---

## License

[GNU GPL v3.0 or later](LICENSE) — see [`LICENSE`](LICENSE) for the
full text. FabCLI is free software: you can redistribute and/or
modify it under the terms of the GPL. Any binary you distribute
must be accompanied by (or offer access to) the corresponding
source.

The release archives ship a `THIRD-PARTY-LICENSES.html` file listing
every dependency and its license, generated with
[`cargo about`](https://github.com/EmbarkStudios/cargo-about).
