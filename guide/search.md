# Search

```
fabcli search -q "medieval kitbash" --filter channels=unreal-engine --filter is_free=1
```

Every filter is a Fab URL key passed as `--filter KEY=VALUE`
(repeatable). Other flags: `-q/--query`, `--sort`, `--count`,
`--cursor` (from `cursors.next`), `--with-ownership` (adds
`owned: bool` per row; needs a login).

**Common filters:**

| Want | Filter |
|---|---|
| Permanently free | `is_free=1` |
| Limited Time Free promos (paid, 100% off now) | `min_discount_percentage=100` |
| Any discount | `is_discounted=true` |
| Engine / store | `channels=unreal-engine` (also `unity`, `uefn`, `metahuman`; several = OR) |
| Style | `styles=lowpoly` (several = AND) |
| Feature, format, license | `technical_features=rigged`, `asset_formats=fbx`, `licenses=cc-by` |
| Type, category, seller | `listing_types=3d-model`, `categories=<slug>`, `seller=<name>` |
| Rating, price | `min_average_rating=4`, `min_price=0`, `max_price=20` |
| Published since | `published_since=YYYY-MM-DD` (compute the date; no `7d` form) |

Unknown keys aren't errors: Fab ignores them and returns nothing
(`style=` instead of `styles=` gives zero results).

**Sort:** `-relevance` (default), `-createdAt`, `createdAt`,
`firstPublishedAt`, `price`, `-price`, `-min_discount_percentage`,
`title`, `-title`, `-ratings.averageRating`. A leading `-` is
descending.

**Response:** `{"results":[{"uid","title","listing_type","is_free","seller","rating",...}],"count","cursors":{"next","previous"}}`.

**Next:** `fabcli listing <uid>` (detail), `formats <uid>` (engine
versions), `prices <uid>` (`limitedTimeFree`, `freeUntil`),
`reviews <uid>`.

For assets *added to your library* since a date, use
`library --since`, not search.

Playbooks: `search-and-inspect`, `check-ownership`, `free-new-assets`,
`claim-promos`.
