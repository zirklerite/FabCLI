# Library

`fabcli library` lists every asset the account owns. It walks Fab's
library page by page: about 100 s for 1,000 items.

**Server-side filters** make it fast (Fab filters, no walk, no cache):

| Flag | Meaning |
|---|---|
| `--since YYYY-MM-DD` | added to the library on or after |
| `--query <text>` | keyword |
| `--seller <sellerId>`, `--exclude-seller <sellerId>` | IDs from `fabcli sellers <name>` |
| `--license`, `--tag`, `--type`, `--category`, `--format` | repeatable |
| `--min-rating 1..5` | minimum rating |
| `--sort newest\|oldest\|title\|title-desc\|last-updated\|relevance` | `newest`/`oldest` = date added |
| `--limit N` | most rows (default 100) |

Filtered output is `{"results":[{"uid","entitlementUid","createdAt","listing":{"title",...}}],"meta":{"source":"agents","count","truncated"}}`.
`uid` is the listing UID; `createdAt` is when it was added.

**Cache (opt-in).** `FABCLI_LIBRARY_CACHE=1` keeps the unfiltered
library on disk for 24 h (`FABCLI_LIBRARY_CACHE_TTL` seconds), used by
`library`, `ownership --from-library`, `claim-batch --from-library` and
`download <uid>`. Per call: `--cache`, `--no-cache`, `--refresh`,
`--clear`. A successful `claim` or `claim-batch` clears it.

**Don't run library walks in parallel on a cold cache:** each pays the
full walk. Run one, then the rest.

**Ownership:**
- `fabcli ownership <uid>`: `{"listingUid","owned","state":{...}}`
- `--batch a,b,c`, `--from-stdin` (one UID per line),
  `--from-library`: `{"ok","results":[...],"meta":{"total"}}`
- `search --with-ownership` marks search rows instead.

All of these need a login (exit 2 otherwise); none falls back to
walking the library.

Playbooks: `library-since`, `find-by-seller`, `check-ownership`,
`verify-library`.
