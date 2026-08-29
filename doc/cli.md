# CLI surface

Binary name: `vaultsync`

## Global flags

```text
vaultsync [global flags] <command> [command flags]
```

| Flag | Meaning |
| ---- | ------- |
| `--config <path>` | config file (default: `./.vaultsync.toml` then `~/.config/vaultsync/config.toml`) |
| `--vault <path>` | vault root override |
| `--follow-symlinks` | follow symlinks below the vault root (off by default; out-of-vault targets are skipped with a warning) |
| `-v, --verbose` | repeatable debug noise on stderr |
| `--json` | machine-readable stdout (**Phase 3**: parses, but dispatch rejects it as not implemented) |
| `-y, --yes` | skip confirmation for destructive flags (**Phase 3**: rejected as unknown today) |
| `--concurrency <n>` | transfer workers (**Phase 3**: rejected as unknown today) |

## Commands

### `vaultsync status`

Show diff between local vault and remote prefix.

```text
vaultsync status
vaultsync status --json   # Phase 3: parses, but dispatch rejects it as not implemented
```

Exit codes:

- `0` - no pending actions (clean)
- `1` - error
- `2` - dirty (actions or conflicts would occur) so scripts can gate on cleanliness

### `vaultsync pull`

Download remote-newer and local-missing paths.

```text
vaultsync pull
vaultsync pull --delete          # remove local extras
vaultsync pull --force-remote    # conflicts prefer remote
vaultsync pull --dry-run         # plan only, no mutations (push/pull only, not a global flag)
```

### `vaultsync push`

Upload local-newer and remote-missing paths.

```text
vaultsync push
vaultsync push --delete          # remove remote extras
vaultsync push --force-local
vaultsync push --dry-run         # plan only, no mutations (push/pull only, not a global flag)
```

### `vaultsync check`

Connectivity probe: put/get/delete a tiny probe object under the prefix (no
head-bucket fallback, by design).

```text
vaultsync check
```

### `vaultsync version`

Print version and optional feature flags.

## Config file (TOML)

```toml
vault_root = "/Users/me/Notes"

[store]
type = "s3"
bucket = "my-vaults"
region = "us-west-2"
# endpoint = "https://minio.example"
# prefix = "notes/"
# path_style = true

# [ignore]     # Phase 3 (ignored patterns are not yet applied)
# patterns = [
#   ".git/",
#   ".trash/",
#   ".DS_Store",
#   ".obsidian/workspace",
#   ".obsidian/workspace.json",
#   ".obsidian/workspace-mobile.json",
# ]

[transfer]
# concurrency = 4   # Phase 3 (parallel transfers are not yet applied); an explicit copy of the default is silent
mtime_tolerance_ms = 1000
# max_delete = 100

[transfer.retry]
# max_attempts = 3       # total attempts incl. the initial one; SDK standard default; 1 disables retries
# base_delay_ms = 1000   # first backoff duration, ms; SDK standard default
# max_delay_ms = 20000   # backoff ceiling, ms; SDK standard default
```

> `[ignore].patterns` is a **Phase 3 feature**: it is parsed and validated
> but not yet applied. A `push`/`pull`/`check` run refuses loudly when it is
> present (exit 1); `status` warns on stderr and proceeds. Do not expect it
> to filter the plan until the roadmap's ignore-patterns phase lands.
>
> `[transfer].concurrency` is likewise a **Phase 3 feature** (inert until the
> pool exists). Setting a value that differs from the default warns on every
> run; an explicit copy of the default (`4`) is silent.

`[transfer.retry]` is **live** (not Phase 3): the three knobs map directly to
AWS SDK **standard-mode** retry policy (exponential backoff with jitter, and
SDK-classified throttling / 5xx / connection-reset retryables), set on the S3
client at build time. `max_attempts = 1` disables retries entirely. Each key
is optional; an absent key (or absent section) keeps the SDK-standard defaults
shown above. `max_attempts` must be >= 1 and `base_delay_ms` <= `max_delay_ms`
(loud config errors otherwise); both delays must be >= 1 (the SDK requires
non-zero backoffs). The SDK's client-side **retry quota** (standard-mode
token bucket) also applies: under sustained failure with no interleaved
successes the SDK may stop retrying before `max_attempts` is reached (a
retryable error then fails on the first attempt), and retries remain silent
(no log line).

vaultsync's retry policy is **config-owned**: `AWS_MAX_ATTEMPTS`,
`AWS_RETRY_MODE`, and profile `max_attempts` / `retry_mode` do **not** apply
to the S3 client vaultsync builds - the resolved `[transfer.retry]` policy
replaces the ambient AWS retry configuration at client build, whether or not
the section is present.

If you want the full populated form (Phase 3, not yet applied), it is shown
here for reference; copying it as-is will refuse `push`/`pull`/`check` until
the ignore-patterns phase lands.

```toml
# Phase 3 (not yet applied): do not copy verbatim
vault_root = "/Users/me/Notes"

[store]
type = "s3"
bucket = "my-vaults"
region = "us-west-2"
endpoint = "https://minio.example"
prefix = "notes/"
path_style = true

[ignore]
patterns = [
  ".git/",
  ".trash/",
  ".DS_Store",
  ".obsidian/workspace",
  ".obsidian/workspace.json",
  ".obsidian/workspace-mobile.json",
]

[transfer]
concurrency = 4
mtime_tolerance_ms = 1000
max_delete = 100
```

Credentials: environment / shared AWS config, not the TOML file.
```text
AWS_ACCESS_KEY_ID
AWS_SECRET_ACCESS_KEY
AWS_PROFILE
AWS_REGION
```

Same as `aws` CLI where possible.

Region resolution: when `[store].region` is not set (and `AWS_REGION` env is
absent), the S3 client falls through to the normal AWS default chain (env,
shared config, profile) - there is no hardcoded region guess. Deliberately,
`AWS_REGION` (env) overrides an explicit config `region` on purpose: env is
an operator override of a checked-in file, matching how `AWS_*` behaves
across AWS tooling.

An empty or whitespace-only `region` or `endpoint` in the config (or an
empty `AWS_REGION`) means **unset**: it is filtered at resolution, so it can
never reach the SDK as `Region::new("")` / `endpoint_url("")` (which fail
late with an opaque error). Treat an empty string as "not configured".

A relative `vault_root` (e.g. `vault_root = "."` or `notes`) resolves
against the process working directory, not the config file's directory;
anchor it absolutely if you rely on the file's location.

Uploads are single-PUT, so a single object is limited to 5 GiB; a larger
size is rejected client-side before buffering (W80), and multipart remains
a post-v1 item.

`--follow-symlinks` + `--delete` is a footgun, but the delete half is inert
in v1: a followed *file* symlink key plans `Skip(followed_symlink)` in every
mutating mode - including `pull --delete` - so the link is never removed
(`--follow-symlinks` is inventory-only; see below). `delete_file_guarded`
additionally refuses a symlink leaf that swapped in *after* planning (fail
closed): the guarded delete is no-follow, so a link planted between plan and
execute is never unlinked. Default mode never plans through symlinks (they
are skipped and counted), so this only applies when you explicitly opt in.

`--follow-symlinks` is **inventory-only in v1**: the walker follows symlinks
and lists them, but push/pull plan any followed *file* symlink as
`Skip(followed_symlink)` (the executor refuses to open a symlink), so a vault
containing a followed file symlink no longer fails a transfer. Only `status`
shows followed-symlink rows as live inventory. Dir-symlink children transfer
normally; a `pull` write through a symlink destination stays refused (fail
closed).

**Dir-symlink alias footgun:** a dir symlink whose target is inside the vault
double-lists the target's content under both keys (push uploads the same
bytes twice; pull writes both keys). The walk warns on every such alias
(e.g. `following linkdir duplicates realdir/`); both copies are still listed
and synced - dedup is deliberately not performed, because which copy would
survive would depend on directory enumeration order.

**Local deletes re-verify freshness.** A `pull --delete` re-stats a local file
before removing it (size + mtime within `transfer.mtime_tolerance_ms`); a
file that changed since the plan is left on disk with a per-key error,
symmetric to the upload/download freshness guards. `DeleteRemote` (push
`--delete`) re-verifies the remote object with a head-before-delete check
(size only): an object that changed size since the plan is left in place
with a per-key error. Same-size replacement between list and delete remains
a documented residual (the list-to-delete gap is an accepted cross-machine
race on the store side). The guarded delete is a check-then-act stat
followed by a by-path `remove_file` (std has no fd-based delete), so a leaf
swapped in the window between the stat and the unlink is still removed - the
same residual class as the download note; fd-based delete is a post-v1 item
(A-L3). After the deletes, the empty-dir post-pass is scoped to the ancestor
chains of the files deleted this run (deepest-first, never the vault root):
pre-existing, plan-unrelated empty dirs are kept (W77). Downloads are
additionally capped mid-stream at the size the plan recorded (W106): a
remote object that exceeds its planned size is refused before the extra
bytes are written to disk.

**Planner identity is codepoint-exact (no NFC fold).** APFS folds NFD/NFC (a
note named in decomposed form appears under its composed name), while S3
does not, so a vault round-tripped through another machine can show a false
`local_only` + `remote_only` pair for the "same" note name. v1 does not
normalize (A-L4).

**One invalid remote key aborts the plan loudly.** A store listing that yields
an invalid key (escaping, control-character) fails the whole plan; the error
names the offending key. This is the fail-closed security lock - a hostile or
buggy backend cannot silently shrink a plan. (The exact-prefix folder-marker
empty key, which stripped to `""`, is dropped at the source instead, so it
does not trip this.)

## Output

### Human (default)

Emitted by `format_plan_human`: split delete counts, no byte-size column, and
the conflict reason is the planner's reason token as emitted. `S` (skip) rows
are hidden by default; pass `-v` to show them (the stats line still counts
them). The JSON block below remains the structured contract
(`delete_local` / `delete_remote` counts already match the formatter).

```text
plan: 3 upload, 1 download, 0 delete_local, 0 delete_remote, 2 skip, 1 conflict
U  notes/a.md
D  notes/b.md
*  notes/c.md    conflict_mtime_size
```

(`-v` additionally shows the skip rows with their planner reasons, e.g.
`S  notes/    folder`.)

### JSON (`--json`)

```json
{
  "stats": { "upload": 3, "download": 1, "delete_local": 0, "delete_remote": 0, "skip": 2, "conflict": 1 },
  "actions": [
    { "key": "notes/a.md", "kind": "upload", "reason": "local_newer" }
  ]
}
```

Stable field names; versioned later with a `schema` field if needed.

## Composition examples

```text
# cron backup: local is truth
vaultsync push --delete

# new laptop: remote is truth once
vaultsync pull --delete

# careful bidirectional without deletes
vaultsync pull && vaultsync push

# gate commit on clean remote mirror
vaultsync status; test $? -eq 0
```

(`--yes` is a Phase 3 flag and is rejected today, so the delete examples
above omit it; the confirmation rail is a Phase 3 roadmap item.)

## Non-commands (v1)

- no `serve`, `daemon`, `auth login` wizard (beyond docs pointing at AWS profiles)
- no `encrypt` / `decrypt`
- no `merge`
- no TUI
