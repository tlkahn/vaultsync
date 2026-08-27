# Sync model (v1)

## Mental model

Treat the vault directory and the S3 prefix as two trees of objects. The tool never invents a third "cloud app" namespace beyond an optional key prefix.

```text
Local tree                         Remote tree
/path/to/vault/                    s3://bucket/prefix/
  notes/foo.md                       notes/foo.md
  notes/img.png                      notes/img.png
  .obsidian/app.json                 .obsidian/app.json   # if not ignored
```

## Commands and direction

| Command | Download | Upload | Delete local extras | Delete remote extras |
| ------- | -------- | ------ | ------------------- | -------------------- |
| `status` | plan only | plan only | plan only | plan only |
| `pull` | yes (remote newer or local missing) | no | no | no |
| `pull --delete` | yes | no | yes | no |
| `push` | no | yes (local newer or remote missing) | no | no |
| `push --delete` | no | yes | no | yes |

v1 does **not** ship a `sync` bidirectional command. If users want both directions without deletes:

```text
vaultsync pull
vaultsync push
```

order is a user policy (usually pull then push, or the reverse). Document the race: two devices editing without history will last-write-wins only if they use opposing one-way mirrors carefully; **v1 is not a multi-master session layer**.

## Comparison keys

For each relative path present on either side:

1. **Missing local, present remote** -> download (pull) or delete-remote (push --delete) or report
2. **Present local, missing remote** -> upload (push) or delete-local (pull --delete) or report
3. **Both present** -> compare

### Equality

Two entities are **equal enough to skip** when:

- both are folders, or
- both are files AND size matches AND mtime matches within configured tolerance

**mtime tolerance:** default 1 second (S3 and some FS round to seconds). Borrowed lesson from remotely-save S3/Dropbox second rounding.

**etag:** if both sides have a comparable content hash/etag and they match, treat as equal even if mtime drifts (`--trust-etag` maybe later). v1 may use etag only as remote opaque token after upload, not as local hash, to avoid hashing every file on large vaults unless `--checksum` is set.

### Newer

When both exist and not equal:

- if local.mtime > remote.mtime -> local newer
- if remote.mtime > local.mtime -> remote newer
- if mtime equal (within tolerance) but size differs -> **conflict** (report; do not clobber unless `--force-local` / `--force-remote`)

### Conflict policy (v1: minimal)

Default on conflict: **skip and warn** (non-zero exit if any conflict).

Optional flags:

- `--force-local` - on conflict, treat as local wins (upload on push; keep local on pull)
- `--force-remote` - on conflict, treat as remote wins

No rename-to-conflict-copy in v1. No three-way merge.

## Deletes

Without a previous-sync database, the tool **cannot** know "deleted on device A, therefore delete on device B" unless the user states a mirror direction with `--delete`.

Semantics of `--delete`:

- `pull --delete`: any local path not present remotely (after filters) is deleted locally
- `push --delete`: any remote path not present locally (after filters) is deleted remotely

Safety rails:

- `--delete` **must** require interactive confirmation unless `--yes` is set
- `--max-delete N` (optional) aborts if planned deletes exceed N
- protect percentage option (remotely-save `protectModifyPercentage`) is deferred; `--max-delete` is simpler and enough for v1

## Filters

Applied when building entity lists (both sides):

- default ignores: `.git/`, `.trash/`, OS junk (`.DS_Store`), maybe `.obsidian/workspace`, `.obsidian/workspace.json`, `.obsidian/workspace-mobile.json` (session state)
- user `--exclude` / `--include` glob patterns (gitignore-style if a small crate is acceptable; else simple prefix/suffix/glob)

**Locked default:** sync `.obsidian/` **except** workspace session files (`.obsidian/workspace`, `.obsidian/workspace.json`, `.obsidian/workspace-mobile.json`), matching "settings yes, ephemeral no". Expose as the built-in `--profile obsidian` defaults (and the default profile when none is named).

## Folder representation

- Local walk emits folder entities with trailing `/`.
- Remote list synthesizes folders from prefixes when no folder marker object exists (same idea as remotely-save synthesized folders).
- `push` of a folder-only path: by default create nothing on S3 (prefix appears when a child is uploaded). Empty folders **do not round-trip** unless `--folder-markers` is enabled later.

Document empty-folder limitation clearly.

## Mtime on S3

Object stores do not preserve client mtime as a first-class writable field on all providers.

v1 S3 policy:

1. On `put`, write user metadata `mtime` (milliseconds since epoch, decimal string) when permitted by the API.
2. On `list`/`head`, prefer metadata mtime; else `LastModified`.
3. After `download`, apply mtime to local file when the platform allows.

This matches the remotely-save practical approach without copying its metadata complexity.

## Plan structure

```text
Plan {
  actions: [
    { key, kind: Upload|Download|DeleteLocal|DeleteRemote|Skip|Conflict, reason, local?, remote? }
  ]
  stats: { upload, download, delete_local, delete_remote, skip, conflict, bytes_in, bytes_out }
}
```

`status` prints this. `push`/`pull` build the same plan then execute the subset allowed by mode.

## Execution order

1. Deletes that unblock paths (rare; v1 can defer renames - there is no rename op, only delete+upload)
2. Downloads (pull)
3. Uploads (push)
4. Destination-side deletes (`--delete`) **after** successful transfers, or before?  

Recommendation: **transfers first, deletes last**, so a crash mid-run loses less. For `push --delete`, remote orphans remain until the end.

Within a phase, topological order for directories: parents before children on create; children before parents on delete.

## Non-goals restated for sync

- No prev-sync history records
- No remote deletion journal
- No encryption-aware keys
- No "smart conflict" content merge
- No partial file / delta transfer (whole-object only)
