# Borrowed from remotely-save

Source tree referenced: `~/Projects/remotely-save` (local checkout).

We treat that project as a **worked example** of vault-to-cloud sync, not as code to vendor wholesale.

## Take (ideas and small rules)

| Idea | Where in remotely-save | How we use it |
| ---- | ---------------------- | ------------- |
| Uniform `Entity` with flat primitive fields | `src/baseTypes.ts` | Slimmer `Entity` / `ObjectMeta` without enc fields |
| Folders are strings ending with `/` | `docs/code_design.md` | Same hard rule |
| Storage backends do not import sync planner | `docs/code_design.md` | Same dependency direction |
| Abstract FS / store interface | `src/fsAll.ts` `FakeFs` | Smaller `ObjectStore` + `LocalFs` traits; drop OAuth-ish methods |
| S3 user-metadata for accurate mtime | `src/fsS3.ts` | Keep; name our metadata key clearly |
| S3 prefix, path-style, endpoint | `S3Config` in `baseTypes.ts` | Keep as first-class config |
| Synthesized folders from prefixes | S3 list behavior | Keep when no folder markers |
| Second-level mtime rounding for S3 | `pro/src/sync.ts` time fixups | mtime tolerance default 1s |
| Decision table thinking | `docs/sync_algorithm/v3/design.md` | **Simplify** to one-way rsync-like tables; do not port all branches |
| Minimal intrusion: no required remote sidecar metadata (post-0.4.1) | `docs/minimal_intrusive_design.md` | v1 also avoids remote control files |
| Dry-run / exportable sync plan | debug/export plan docs | `status` + `--json` + `--dry-run` |
| Deletion safety rail | `protectModifyPercentage` | Replace with simpler `--max-delete` + `--yes` |
| Ignore paths | settings ignore lists | TOML + flags; smaller default set |
| Pure-ish core functions | code design "pure except main" | planner pure; executor IO at the edge |

## Leave (consciously)

| Item | Why leave it |
| ---- | ------------ |
| Multi-service monolith (`serviceType` union of 12 backends) | Violates S3-first / KISS |
| PRO / free split, account checks | Not relevant |
| `FakeFsEncrypt`, rclone/OpenSSL crypt, workers | Non-goal v1 |
| `localdb` / localforage prev-sync records | Non-goal v1 (no true multi-device deletion journal) |
| Smart conflict merge / duplicate-file conflict logic | Non-goal v1 |
| Obsidian settings tab mega-UI (`settings.ts` ~3k lines) | CLI first |
| Browser CORS, `requestUrl` handler, PKCE OAuth | Native CLI uses normal HTTPS + AWS auth |
| i18n packs | English CLI messages first |
| Webpack/esbuild Obsidian bundle constraints | Different runtime |
| WebDAV depth quirks, Dropbox delta, OneDrive delta | Not S3 |
| Sync direction enum with five modes | Two verbs: push and pull |
| Bookmarks / configDir special listers as core | Optional later filters only |
| Metadata-on-remote v1/v2 legacy | Historical; we start clean |

## Algorithm stance vs v3

remotely-save v3 uses **three inputs** after migration: local tree, remote tree, **prev sync history**. That enables true deletion detection without remote sidecars.

vaultsync v1 uses **two inputs**: local tree, remote tree. Deletion is only via explicit `--delete` mirror mode.

If vaultsync later needs multi-device deletion without `--delete` hazards, the remotely-save v3 prev-sync approach (or a small local SQLite history) is the **first place to look** - as an optional layer above the same planner, not a rewrite.

## Code reuse policy

- Do **not** copy TypeScript sources into this repo.
- Do **not** re-license dump large files.
- When an algorithm table is useful, rewrite in Rust from the design docs and our simplified semantics.
- If a tiny pure helper is reimplemented (e.g. folder parent chain), write it fresh with tests.

## Credit

Design docs should acknowledge remotely-save as prior art for the entity/store split and S3 mtime practices. License of remotely-save (Apache-2.0 free tier / PolyForm PRO) does not automatically apply to our clean-room Rust code; still keep third-party notices if any text is quoted at length.
