# Download

```
fabcli download <uid> -o ./MyAsset/
```

Downloads an owned Unreal Engine asset's files. The UID form reads the
library to find the asset (set `FABCLI_LIBRARY_CACHE=1` so repeat calls
skip the walk). `--uid <uid>` and `--stdin` also take the UID; the
legacy form takes `--artifact-id`, `--namespace` and `--asset-id`.

Flags: `-o/--output` (required), `--engine UE_X.Y`, `--platform <P>`,
`--jobs 1..64` (default 8), `--force` (overwrite), `--into-empty`
(refuse unless the folder is empty).

**Errors to recover from:**
- `ambiguous_artifact` (exit 6): the listing has several engine
  versions or platforms. `error.available` lists them; re-run with
  `--engine` (and `--platform`) from it.
- `not_owned` (exit 2): not in the library. `claim <uid>` if it's
  free, otherwise the user buys it on Fab.
- `output_collision` (exit 6): files already exist (`error.conflicts`).
  Choose another folder, or `--force` if the user agrees.
- `auth_required` (exit 2): run `fabcli auth login`.

Every chunk and file is hash-checked; a failed run leaves no
truncated file. The response is
`{"ok":true,"files","total_bytes","elapsed_seconds","file_hashes_verified","output_dir","sidecar"}`,
and `.fabcli-asset.json` in the folder records what was downloaded.

Only Unreal Engine formats download; web formats (FBX, GLB, ...) are
not supported. Importing into a project is a separate step.

Playbook: `download-owned`.
