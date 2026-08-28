# Phase 2 manual test matrix

Executed once per endpoint row after the env-gated integration suite passes.
Credentials come from the ambient AWS chain; R2 row needs a Cloudflare R2
endpoint/bucket/credentials.

## Rows

| # | Scenario | Verify | AWS S3 (pdf-tmp-repo) |
| - | -------- | ------ | ---------------------- |
| 1 | `check` | exit 0; actionable failure modes (bad creds / bad bucket) | done (exit 0 good; bad bucket 404 -> 1; bad creds 401/403 -> 1 + hint) |
| 2 | push sample vault: nested folders + `.png` + unicode filename | bytes + mtimes on remote (pull into fresh dir or `aws s3 cp` back) | done (nested `notes/`, binary `.png`, `中文.md` uploaded; mtime preserved) |
| 3 | pull into empty dir | tree identical (`diff -r` + mtime spot check) | done (byte-identical; exact mtimes; empty dirs correctly not round-tripped) |
| 4 | modify local, push again | only changed keys transferred | done (B changed, A untouched on remote) |
| 5 | modify remote (console), pull | remote wins per planner rules | done (future-mtime remote overwrite pulled as remote content) |
| 6 | push/pull with `--delete` | extras removed on the destination side only | done (delete local A, `push --delete` removed remote A; B kept) |
| 7 | conflict case | exit 2, nothing clobbered | automated (run_push_conflict_exit_2: exit 2, conflict key not transferred) |
| 8 | prefix + path-style (R2 row) | objects land under prefix only; path-style works | prefix: done throughout; path-style: done (s3_integ_path_style_toggle) |
| 9 | `--follow-symlinks` | default counts skipped; follow includes in-vault, skips escaping target with warning; loop-safe | done (local walker tests + CLI on a real tree) |

R2 row (Cloudflare): **pending** - requires an R2 endpoint/bucket/credentials
(not provided this session). The custom-endpoint + path-style paths R2 needs
are already exercised and passing on AWS (`s3_integ_path_style_toggle`,
`s3_integ_prefix_isolation`); R2 metadata/list/checksum quirks remain to be
verified here before ticking the row.

## Known limitation (documented in `src/store/s3.rs`)

`list` uses each object's `LastModified` (ListObjectsV2 does not return user
metadata), so after a push many unmodified files can appear "remote newer" by
seconds-of-granularity and a later `pull` may re-download them. Bytes are
correct and downloads apply the true client mtime from `get_to` metadata.
A per-object `head` in `list` (to surface client mtimes in plans) is post-v1.
