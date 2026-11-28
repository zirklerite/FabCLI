# FabCLI

Search, inspect, claim and download Epic Games / Fab marketplace assets.

**Output.** Every command prints one compact JSON value on stdout
(`--pretty` to indent). Errors go to stderr as
`{"error":{"kind":"...","message":"..."}}`. Lines starting `fabcli: `
on stderr are hints, not errors.

**Exit codes.** 0 ok, 1 generic, 2 `auth_required` (or `not_owned`),
3 not found, 4 rate limited (wait ~30 s), 5 network, 6 invalid
arguments. Exit 0 is not always success: claim commands answer
`ok:false` with a `reason`.

**Login.** `fabcli auth login` is the only interactive command: the
user signs in to Epic in a window, about once a year. Every other
command is headless. On exit 2, run `auth login` and let the user
finish; `fabcli auth status` checks the session without a window.

**Pipelines.** Single-ID commands take `--stdin`;
`claim-batch` and `claim-promo-batch` take `--from-stdin-json`, so
`fabcli search ... | fabcli claim-batch --from-stdin-json` works with
no `jq`.

**Free means two things.** `--filter is_free=1` is permanently free;
`--filter min_discount_percentage=100` is Fab's Limited Time Free
promos. Ask which the user means. Promos are claimed with
`claim-promo` / `claim-promo-batch`, everything else free with `claim`
/ `claim-batch`. No command can buy a paid asset.

**Before claiming,** show the user what will be added and ask.

**Dates.** Filters take `YYYY-MM-DD`; compute it yourself ("this
month" is the 1st of the current month).

**More:**
- `fabcli guide <topic>`: `auth`, `search`, `library`, `claim`,
  `download`.
- `fabcli playbook list`, then `fabcli playbook show <name>`: steps
  for a task, with the exact commands, what to check, and what to do
  on known errors.
- `fabcli <command> --help` for every flag.
