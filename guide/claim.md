# Claim

Three commands add free assets to the library. None can buy anything.

| Asset | Command | Batch |
|---|---|---|
| Permanently free (`is_free`, or an offer priced 0) | `fabcli claim <uid>` | `claim-batch` |
| Limited Time Free promo (paid, 100% off now) | `fabcli claim-promo <uid>` | `claim-promo-batch` |
| Paid | none: answers `ok:false`, `reason:"not_free"`, with `purchase_url` | |

**Batch input:** `--uids a,b,c`, `--stdin` (one per line),
`--from-stdin-json` (`fabcli search` output), and for `claim-batch`
`--from-library`. A batch uses one Fab session; prefer it over a loop
for more than two UIDs.

**Read `ok`, not the exit code.** Exit 0 can still be "not claimed":
- `{"ok":true,"claimed":true}`, `{"ok":true,"already_owned":true}`;
- `claim`: `reason` `not_free`, or `promo_only` (use `claim-promo`);
- `claim-promo`: `reason` `not_promo` (use `claim` if free),
  `non_zero_total` (refused, nothing ordered, exit 1), `cart_not_empty`
  (the user's own Fab cart has items: they empty it), `claim_timeout`.

Batch envelopes: `{"ok","results":[...],"meta":{...}}`. `claim-batch`
counts `claimed`, `already_owned`, `skipped_paid`,
`skipped_promo_only`, `failed`; it stops early on `auth_required` or
`rate_limited` (`stopped` says where). Re-running is safe: owned UIDs
answer `already_owned`.

**`claim-promo`** runs Fab's checkout at a total of 0 in a hidden
window (about 20 s per asset, needs a desktop session). It orders only
when Epic's checkout total is exactly 0. If hCaptcha wants a
challenge, the window appears and stderr says so: the user must solve
it, you can't.

**Always show the user what will be claimed and ask first.**

Playbooks: `claim-free-batch`, `claim-promos`.
