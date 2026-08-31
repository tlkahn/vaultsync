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
| `--concurrency <n>` | transfer workers (config-only: no CLI flag; set `[transfer].concurrency`, live since issue 20) |

## Commands

### `vaultsync status`

Show diff between local vault and remote prefix.

```text
vaultsync status
vaultsync status --write-manifest   # issue 48: explicitly publish a cold manifest
vaultsync status --json   # Phase 3: parses, but dispatch rejects it as not implemented
```

Exit codes:

- `0` - no pending actions (clean)
- `1` - error
- `2` - dirty (actions or conflicts would occur) so scripts can gate on cleanliness

`--write-manifest` (issue 48) is an explicit opt-in - the default `status` is
read-side-effect free for the remote (Q3). With the flag, under `mode=auto` a
COLD status (missing or corrupt manifest) publishes one via B1
(`ensure_remote_manifest`) and prints the bootstrap line; a warm status prints
a skip line and writes nothing. Mode is checked before warm (a `mode=manifest`
or `mode=list_head` flag run prints `skipped (mode=...)` even when a valid
manifest is present); `[inventory].bootstrap = "never"` does NOT gate the
flag (an explicit write is always allowed). Any bootstrap failure under the
flag exits `1` (write is the requested op).

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

A fresh `push` may also run the **inventory bootstrap** (issue 48, B1)- see
[Push-time inventory bootstrap](#push-time-inventory-bootstrap-issue-48) below.

### Progress output (push/pull)

Push and pull render a live progress bar on stderr while the executor runs
(issue 27), e.g. one `\r`-refreshed line per active pass:

```text
Uploading   notes/foo.md  847/1204  [=====>--]   70% 12.4 MiB/s ETA 1:12
```

The fixed layout is budgeted for an 80-column terminal: a 12-column verb
field, a 12-column key field, and an 8-cell bar, with one-space rate/ETA
suffixes. Terminal-width detection (rendering below 80 columns on narrow
ttys) remains a follow-up option.

- **stderr only, TTY only.** The bar appears only when stderr is a terminal
  (`std::io::stderr().is_terminal()`); piped or redirected stderr stays quiet.
  stdout is never touched: it carries the plan text (and the `--json` stream
  later, Phase 3).
- Passes with nothing to do render nothing; each pass ends with a newline.
- `check` and `--json` never render progress. `status`, `repair`, and
  `--dry-run` push/pull render the *plan-phase* feed below when the inventory
  is cold, but never an executor bar (no transfers run).
- Rate is cumulative (`bytes / elapsed`); ETA is `remaining / rate` - no
  sliding window, so a mixed-size vault shows a jittery ETA (accepted v1).
- Rate/ETA count bytes of **successful** transfers only: a failed key still
  advances the key count, but its planned size is removed from the pass
  total, so a pass with failures still ends at a clean 100% with no leftover
  ETA.
- There is no `--progress=` flag yet; the mode seam (`Auto`/`Off`/`Always`)
  is the extension point for one.

### Plan-phase progress (cold inventory, issue 42)

The **cold** inventory path (live `ListObjectsV2` pages + per-object
`HeadObject`s, i.e. `mode = "list_head"`, an auto fallback, or `repair`) is
multi-minute on large vaults and previously silent. Issue 42 adds a stderr
feed over the same TTY-only `Auto`/`Off`/`Always` seam, e.g.:

```text
Listing     page 7  6900 keys
Heading     1200/6894  [====>---]   17%
```

The plan-phase lines use the same 12-column verb budget and 8-cell bar as
push/pull (no byte rate/ETA - there is no byte stream yet). They are
`\r`-refreshed per `ListObjectsV2` page (`Listing`) and per completed head
(`Heading`), and finalized with a newline before the W236 inventory source
line (below) so a mid-line bar can never collide with it - including on
error paths, where the partial bar is cleared defensively.

- **Coverage:** cold `status`, `push`, `pull`, `--dry-run`, and `repair`
  (repair has no executor phase at all - plan-phase only).
- **Warm runs are silent:** a warm manifest load emits zero plan-phase
  events, so warm runs show exactly one `inventory: manifest (N entries)`
  line and gain no fake bar.
- `check` and `--json` never render any progress (JSON is rejected before a
  renderer is built).

### `vaultsync check`

Connectivity probe: put/get/delete a tiny probe object under the prefix (no
head-bucket fallback, by design).

```text
vaultsync check

```

### `vaultsync repair`

Rebuild the remote inventory manifest from a live list+head (issue 45,
W241-W242). Never touches file bodies - it rewrites only the control-plane
object (and the local cache mirror).

```text
vaultsync repair              # conditional write (If-Match / If-None-Match: *)
vaultsync repair --dry-run    # count only; writes nothing
vaultsync repair --force      # unconditional overwrite
```

Requires a configured `[store]` like push/pull. Summary lines on stdout:

```text
repair: listed 18390 objects via list+head
repair: wrote .vaultsync/manifest/v1.json (18390 entries, etag="...")
```

Exit codes: `0` ok (including dry-run), `1` store error. Run it to bootstrap
a manifest on an existing bucket, after console/aws-cli uploads outside
vaultsync, after repeated `manifest not committed` warnings, or when
`status` disagrees with raw `aws s3 ls` expectations.

Like `push`, `repair` refreshes the local manifest mirror
(`.vaultsync/cache/`) with the rebuilt body + etag (issue 45, W246/W251).
It requires a conditional-PUT-capable backend (see `object-store.md`); the
etag reported on success comes from the write itself, not a follow-up
head.

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

# [ignore]     # live (issue #34): compiled once, applied on both sides
# profile = "obsidian"   # default when absent; "none" disables built-ins
# patterns = [
#   ".git/",
#   ".trash/",
#   ".DS_Store",
#   ".obsidian/workspace",
#   ".obsidian/workspace.json",
#   ".obsidian/workspace-mobile.json",
# ]

[transfer]
# concurrency = 4   # live (issue 20): bounds transfer passes AND list-enrichment heads; 1 = sequential; 1..=256 (256 = I20-r1 cap)
mtime_tolerance_ms = 1000
# max_delete = 100

[transfer.retry]
# max_attempts = 3       # total attempts incl. the initial one; SDK standard default; 1 disables retries
# base_delay_ms = 1000   # first backoff scale, ms (full jitter: realized delay is uniform in [0, this]); SDK standard default
# max_delay_ms = 20000   # pre-jitter backoff ceiling, ms; SDK standard default

[inventory]              # live (issue 45): warm manifest vs live list+head
# mode = "auto"         # auto | manifest | list_head; default when absent
# bootstrap = "push-ensure"   # issue 48: push-ensure | never; default when absent
```

> `[transfer.retry]` defaults are filled **per field**, so validation runs
> against the filled mix: a lone `max_delay_ms = 500` fails because the
> resolved base stays 1000, and a lone `base_delay_ms = 30000` fails against
> the default max 20000. Set both when tightening either bound.

> `[inventory]` (issue 45) selects how plan build gets the remote file set:
> `auto` (default) uses the remote manifest `.vaultsync/manifest/v1.json`
> when present and valid, else falls back to live list+head with a warning;
> `manifest` requires a valid manifest (fails closed, suggesting `repair`);
> `list_head` never reads the manifest (debug / bisect / #42 baseline). The
> local cache mirror lives at `<vault_root>/.vaultsync/cache/` (never
> walked, never uploaded; owner-only on Unix). Unknown mode strings are
> loud errors naming the allowed set.
>
> `bootstrap` (issue 48, IQ6) is the push-time inventory bootstrap policy:
> `push-ensure` (default) lets a fresh `push` running under `mode=auto` on a
> COLD inventory publish a baseline `.vaultsync/manifest/v1.json` BEFORE any
> transfer (so later plans are warm even if transfers fail or are zero);
> `never` disables that automatic push-time write. The knob ONLY gates push;
> the explicit `status --write-manifest` flag always wins (IQ-status-flag-vs-
> bootstrap), and `repair` is unchanged. `bootstrap = "never"` is the escape
> hatch for operators who do not want `push` touching the control plane with
> zero transfers (IQ-zero-xfer). Unknown values are loud errors naming
> `inventory.bootstrap` and the allowed set.
>
> Mode-first note (F4): under `mode=manifest` / `mode=list_head` the
> push-time bootstrap is always skipped, and `status --write-manifest` prints
> `skipped (mode=...)`. Strict `mode=manifest` still hard-errors at load on a
> missing/corrupt manifest (F6); the `--write-manifest` flag does NOT bypass
> that strict load - use `vaultsync repair` to rebuild when planning is in
> strict mode.

> `[ignore]` is **live** (issue #34): the resolved pattern list is compiled
> once at dispatch and applied on **both** sides - the local walk prunes
> matching paths before they enter the plan, and the remote listing drops
> matching keys before planning, so a `--delete` run never removes an
> ignored remote-only key or an ignored local-only path (D-both-sides).
> `profile` selects the built-in default set: `"obsidian"` is the default
> when the key is absent, `"none"` disables the built-ins (user `patterns`
> still apply). User `patterns` **extend** the active profile (union, never
> replacement). When a run skips paths/keys by ignore patterns, stderr
> reports the count only:
> `warning: ignored N local path(s) by ignore patterns` / `warning: ignored
> N remote key(s) by ignore patterns`.
>
> Upgrade note: keys already uploaded under these patterns before the
> defaults activated stay on the remote - they are ignored on both sides, so
> `push --delete` will not remove them (delete invariant); remove them
> manually in the store, or temporarily set `profile = "none"` for a run
> (built-ins are disabled for that run).
>
> Pattern shapes (issue #30 matcher; `IgnoreSet` is the single source of
> truth):
>
> | Shape | Example | Matches |
> | ----- | ------- | ------- |
> | dir prefix (trailing `/`) | `.git/` | the directory key and everything under it |
> | basename (no `/`) | `.DS_Store` | the final path segment anywhere (`notes/.DS_Store`) |
> | exact key (has `/`, no trailing `/`) | `.obsidian/workspace.json` | exactly that key |
> | per-segment `*` glob | `notes/*.md` | one segment matching the wildcard shape |
>
> Unsupported: `**`, `!` negation, character classes, `?`, escapes - a
> pattern using them is a loud config error at resolve time.

`[transfer].concurrency` is **live** (issue 20): it bounds how many
operations run in flight - the transfer passes (downloads, uploads,
`DeleteRemote`, `DeleteLocal`) and the per-object head calls that enrich
`list` results. Default `4`; valid range `1..=256` (loud config errors
otherwise - `0` is rejected, and `256` is the I20-r1 cap: S3 single-client
throughput saturates well below 256 concurrent ops, so anything above is
OS-thread cost for zero gain and is a config mistake, not a tuning choice);
`1` runs everything sequentially on the caller's thread (the pre-issue-20
behavior). There is no `--concurrency` CLI flag - this is a config-only
knob, so `--concurrency` stays rejected as an unknown flag. The cap is
config-layer only: library callers of `execute_plan`/`S3Store::new` are
uncapped, and the pool clamps workers to `min(concurrency, items)`.

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

The full populated form (copy-paste runnable):

```toml
vault_root = "/Users/me/Notes"

[store]
type = "s3"
bucket = "my-vaults"
region = "us-west-2"
endpoint = "https://minio.example"
prefix = "notes/"
path_style = true

[ignore]
profile = "obsidian"
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

**Inventory source line (issue 45).** Every plan build prints one always-on
stderr line naming where the remote inventory came from:

```text
inventory: manifest (18390 entries)   # warm: parsed the remote manifest
inventory: list+head (cold)           # cold: live ListObjectsV2 + per-object heads
```

**Lost-race warning (issue 45, Q2).** When a `push` succeeds but its manifest
commit loses the conditional race (another writer committed first), stderr
carries the locked warning and the exit code stays `0` when the transfers
succeeded (bodies are live; data ok):

```text
warning: manifest not committed (lost race or changed under us); run vaultsync repair if status looks wrong
```

**Corrupt / cold push (H1, review 5472028291).** A push planned from
`list+head` (corrupt or missing manifest, or `mode = "list_head"`) still
commits: at commit time vaultsync heads the manifest object and uses
If-Match on the live etag when a (possibly corrupt) object is present,
If-None-Match: * when absent. So a push after a corrupt manifest heals it;
`list_head` planning never stops the control plane. A backend without
conditional PUT cannot do this - push warns (`manifest commit failed`),
bodies stay live, and the control plane never advances; cold `auto` planning
still works (see `object-store.md`).

**Push-time inventory bootstrap (issue 48, B1 / C-PA).** Under `mode=auto`
+ `[inventory].bootstrap = "push-ensure"` (the default), a `push` that plans
from a COLD inventory (`inventory: list+head (cold)`) publishes a baseline
manifest BEFORE any transfer, so a later plan is warm even when this run's
transfers fail or nothing is planned. Bootstrap runs between the inventory
source line and the executed plan; the local cache mirror is filled on a
write. One always-on stderr line on each bootstrap:

```text
inventory: manifest bootstrap written (18390 entries)     # we published
inventory: manifest bootstrap adopted (already present, 18390 entries)  # a concurrent valid manifest won; no write
inventory: manifest bootstrap skipped (already warm)      # status --write-manifest on a warm auto store
inventory: manifest bootstrap skipped (mode=list_head)    # status --write-manifest under a non-auto mode (mode-first)
```

B1 **never** re-lists and never claims in-flight uploads: it publishes the
pre-transfer remote snapshot, or **adopts** a concurrent-valid manifest
without writing (H1-V validate-before-overwrite - it is never a blind
clobber of another writer's valid manifest; a present-but-corrupt object is
healed via If-Match). `push --dry-run` never bootstraps, `bootstrap = "never"`
skips push B1, `mode=list_head` skips it, and a WARM push (any valid
manifest) never bootstraps. `pull` never bootstraps (Q6).

**B1 failure policy (push, F2 split).** A lost conditional race at bootstrap
`PreconditionFailed` (another writer owned the manifest) ABORTS the push:
exit `1`, no transfers - this prevents the final manifest commit from
cascade-clobbering the winner with a stale cold base (F0):

```text
error: manifest bootstrap failed (lost race); another writer owns the manifest; aborting before transfers
```

A TRANSIENT bootstrap error instead warns and continues cold (availability is
preserved; transfers may still run):

```text
warning: manifest bootstrap failed: <err>; continuing without bootstrap
```

`status --write-manifest` fails CLOSED on any bootstrap failure (exit `1`,
never the continue-warning), because the write is the requested operation.

**`--json` (D-json).** `status --write-manifest --json` is still rejected
today (`--json` is Phase 3, rejected at dispatch before the flag runs; no
manifest write happens). Future contract: when JSON lands the combo is
allowed and all bootstrap/skip lines stay on stderr so the JSON stdout stays
clean.

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
- no deletion journal / per-device ids in the manifest (issue 45 non-goal)
