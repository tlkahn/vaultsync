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
| `--json` | machine-readable stdout |
| `--dry-run` | plan only; no mutations (alias of `status` behavior when passed to push/pull) |
| `-v, --verbose` | repeatable debug noise on stderr |
| `-y, --yes` | skip confirmation for destructive flags |
| `--concurrency <n>` | transfer workers |

## Commands

### `vaultsync status`

Show diff between local vault and remote prefix.

```text
vaultsync status
vaultsync status --json
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
vaultsync pull --delete --yes
vaultsync pull --force-remote    # conflicts prefer remote
vaultsync pull --dry-run
```

### `vaultsync push`

Upload local-newer and remote-missing paths.

```text
vaultsync push
vaultsync push --delete          # remove remote extras
vaultsync push --force-local
vaultsync push --dry-run
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
# max_delete = 100
```

> `[ignore].patterns` is a **Phase 3 feature**: it is parsed and validated
> but not yet applied. A `push`/`pull`/`check` run refuses loudly when it is
> present (exit 1); `status` warns on stderr and proceeds. Do not expect it
> to filter the plan until the roadmap's ignore-patterns phase lands.

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

A relative `vault_root` (e.g. `vault_root = "."` or `notes`) resolves
against the process working directory, not the config file's directory;
anchor it absolutely if you rely on the file's location.

Uploads are single-PUT, so a single object is limited to 5 GiB; larger files
need multipart (a post-v1 item).

`--follow-symlinks` + `--delete` is a footgun: with the flag on, a followed
symlink and its target are two names for the same inode, and deleting a key
that is a symlink removes that inode (exactly like `rm` on the link path).
Default mode never plans through symlinks (they are skipped and counted), so
this only applies when you explicitly opt in.

## Output

### Human (default)

Phase 1 subset emitted by `format_plan_human`: split delete counts, no
byte-size column, and the conflict reason is the planner's reason token as
emitted. Byte sizes / tighter column alignment / hiding `S` rows by default
are Phase 2+ (formatter growth is out of scope for Phase 1). The JSON block
below remains the structured contract (`delete_local` / `delete_remote`
counts already match the formatter).

```text
plan: 3 upload, 1 download, 0 delete_local, 0 delete_remote, 2 skip, 1 conflict
U  notes/a.md
D  notes/b.md
*  notes/c.md    conflict_mtime_size
S  notes/
```

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
vaultsync push --delete --yes

# new laptop: remote is truth once
vaultsync pull --delete --yes

# careful bidirectional without deletes
vaultsync pull && vaultsync push

# gate commit on clean remote mirror
vaultsync status --json | jq -e '.stats.upload + .stats.download == 0'
```

## Non-commands (v1)

- no `serve`, `daemon`, `auth login` wizard (beyond docs pointing at AWS profiles)
- no `encrypt` / `decrypt`
- no `merge`
- no TUI
