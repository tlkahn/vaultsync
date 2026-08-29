//! Env-gated S3 integration suite (Slice 7/8/10).
//!
//! Compiled always, but every test **skips** at runtime (printing a note)
//! unless `VAULTSYNC_TEST_S3_BUCKET` is set. Optional:
//! `VAULTSYNC_TEST_S3_ENDPOINT`, `VAULTSYNC_TEST_S3_REGION`,
//! `VAULTSYNC_TEST_S3_PREFIX`, `VAULTSYNC_TEST_S3_PATH_STYLE=1`.
//!
//! `VAULTSYNC_TEST_S3_REQUIRE=1` (CI sentinel, I6-sentinel): turns the
//! bucket-missing skip into a hard test failure, so a green CI job proves
//! the suite really ran against S3 instead of silently skipping.
//!
//! Credentials come from the ambient AWS default chain (env, shared
//! credentials file, profile); the tests never read secret values themselves.
//!
//! Each test runs under a unique `vaultsync-itest-<ts>-<name>/` prefix and
//! cleans up its objects afterwards (Drop/finally-style via `with_store`).

use std::io::Read;
use std::sync::atomic::{AtomicU64, Ordering};

use vaultsync::config::{RetrySettings, StoreSettings};
use vaultsync::error::Error;
use vaultsync::local::LocalFs;
use vaultsync::plan::{Mode, PlanOpts};
use vaultsync::store::ObjectStore;
use vaultsync::store::s3::S3Store;

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique_num() -> u64 {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    ts.wrapping_add(COUNTER.fetch_add(1, Ordering::Relaxed))
}

/// Skip-or-require decision for the bucket env gate (I6-sentinel, issue #6).
/// Pure function so it is unit-testable without mutating process env
/// (edition 2024 makes `std::env::set_var` unsafe; parallel tests would race
/// on env anyway). An unset OR empty/whitespace-only value counts as missing:
/// in GitHub Actions a deleted repo variable expands to `""`, which must hit
/// the same sentinel path rather than failing later with an opaque
/// "failed to construct request" from the S3 client. Returns
/// `Ok(Some(bucket))` to run, `Ok(None)` to skip (caller prints the `[skip]`
/// note), `Err(msg)` when require mode is on and the bucket is missing - the
/// caller panics with the message.
fn bucket_or_skip(
    bucket: Option<String>,
    require: bool,
    name: &str,
) -> Result<Option<String>, String> {
    let missing = bucket.as_deref().map(str::trim).unwrap_or("").is_empty();
    match (missing, require) {
        (false, _) => Ok(bucket),
        (true, false) => Ok(None),
        (true, true) => Err(format!(
            "{name}: VAULTSYNC_TEST_S3_BUCKET is unset or empty but \
             VAULTSYNC_TEST_S3_REQUIRE=1 - refusing to silently skip"
        )),
    }
}

fn require_mode() -> bool {
    std::env::var("VAULTSYNC_TEST_S3_REQUIRE")
        .map(|v| v == "1")
        .unwrap_or(false)
}

fn put_bytes(
    store: &S3Store,
    key: &str,
    bytes: &[u8],
    mtime: Option<u64>,
) -> Result<vaultsync::entity::Entity, String> {
    let mut c = std::io::Cursor::new(bytes.to_vec());
    store
        .put_from(key, &mut c, bytes.len() as u64, mtime)
        .map_err(|e| format!("{e}"))
}

/// Run a test against a unique-prefix store, cleaning up afterwards. Skips
/// (with a printed note) when `VAULTSYNC_TEST_S3_BUCKET` is unset.
fn with_store<F>(name: &str, f: F)
where
    F: FnOnce(&S3Store) -> Result<(), String>,
{
    let bucket = match bucket_or_skip(
        std::env::var("VAULTSYNC_TEST_S3_BUCKET").ok(),
        require_mode(),
        name,
    ) {
        Ok(Some(b)) => b,
        Ok(None) => {
            eprintln!(
                "[skip] {}: VAULTSYNC_TEST_S3_BUCKET not set (set to run real S3 tests)",
                name
            );
            return;
        }
        Err(msg) => panic!("{msg}"),
    };
    let region = std::env::var("VAULTSYNC_TEST_S3_REGION").unwrap_or_else(|_| "us-west-1".into());
    let endpoint = std::env::var("VAULTSYNC_TEST_S3_ENDPOINT").ok();
    let path_style = std::env::var("VAULTSYNC_TEST_S3_PATH_STYLE")
        .map(|v| v == "1")
        .unwrap_or(false);
    let base = std::env::var("VAULTSYNC_TEST_S3_PREFIX").unwrap_or_default();
    let prefix = format!("{base}vaultsync-itest-{}-{name}/", unique_num());

    let settings = StoreSettings {
        bucket: bucket.clone(),
        region: Some(region),
        endpoint,
        prefix,
        path_style,
    };
    let store = S3Store::new(&settings, &RetrySettings::default())
        .map_err(|e| format!("S3Store::new: {e}"));
    let store = match store {
        Ok(s) => s,
        Err(e) => {
            panic!("{name}: failed to build store against {bucket}: {e}");
        }
    };

    let result = f(&store);

    // Cleanup: list + delete everything (files only; folders are views).
    // L12 (W104): a failed cleanup must be reported on stderr, never silently
    // swallowed - leaked objects make CI flakes hard to diagnose (a later run
    // re-encounters them under a fresh unique prefix, but the bucket slowly
    // fills). Reporting never fails the test outcome; the harness already
    // eprintln!s its skip path, so this matches the file's convention.
    match store.list("") {
        Ok(listing) => {
            for e in listing.entities {
                if e.is_folder() {
                    continue;
                }
                if let Err(err) = store.delete(&e.key) {
                    eprintln!("cleanup: failed to delete {}: {err}", e.key);
                }
            }
        }
        Err(err) => eprintln!("cleanup: failed to list for cleanup: {err}"),
    }

    if let Err(e) = result {
        panic!("{name} failed: {e}");
    }
    eprintln!("[ok] {}", name);
}

#[test]
fn s3_integ_put_get_head_delete_roundtrip() {
    with_store("roundtrip", |s| {
        let put_ent = put_bytes(s, "a.txt", b"hello", Some(1_700_000_000_123))?;
        assert!(
            put_ent.etag.is_some(),
            "put_from must return the S3 ETag (R5-L2/W44)"
        );
        let h = s.head("a.txt").map_err(|e| format!("{e}"))?;
        assert_eq!(h.size, 5, "head size");
        assert_eq!(h.mtime_ms, Some(1_700_000_000_123), "metadata mtime");
        assert!(h.etag.is_some(), "etag present");
        // the entity returned by put_from must agree with head on the etag
        // (R5-L2: put_from previously always returned etag: None).
        assert_eq!(put_ent.etag, h.etag, "put_from vs head etag");

        let mut buf = Vec::new();
        let got = s.get_to("a.txt", &mut buf).map_err(|e| format!("{e}"))?;
        assert_eq!(buf, b"hello", "get bytes");
        assert_eq!(got.mtime_ms, Some(1_700_000_000_123), "get mtime");

        s.delete("a.txt").map_err(|e| format!("{e}"))?;
        assert!(
            matches!(s.head("a.txt"), Err(Error::NotFound(_))),
            "head after delete should be NotFound"
        );
        Ok(())
    });
}

#[test]
fn s3_integ_list_paginates() {
    // Seed >1000 keys (S3 pages at 1000) concurrently, confirm list returns all.
    let n = 1050usize;
    with_store("paginate", |s| {
        std::thread::scope(|scope| {
            for t in 0..16 {
                scope.spawn(move || {
                    for i in (t..n).step_by(16) {
                        put_bytes(s, &format!("p/obj-{i:05}.dat"), b"x", Some(i as u64)).unwrap();
                    }
                });
            }
        });
        let ents = s.list("").map_err(|e| format!("{e}"))?.entities;
        // files + synthesized folder views
        let files: Vec<_> = ents.iter().filter(|e| !e.is_folder()).collect();
        assert_eq!(files.len(), n, "all paged objects returned");
        Ok(())
    });
}

#[test]
fn s3_integ_prefix_isolation() {
    // Objects under another (non-store) prefix are invisible to this store.
    // The "other" store must mirror the primary's endpoint / region / path
    // style, from the same env-derived settings as `with_store`, with a
    // normalized sibling prefix (R5-M2): otherwise it would not even talk to
    // the primary's endpoint on MinIO/R2, or would drop the path-style
    // requirement, and the leak assertion would be vacuous.
    with_store("isolation", |s| {
        put_bytes(s, "mine.txt", b"m", None)?;
        // write something under a sibling prefix directly via a second store
        // sharing the bucket but a different prefix.
        let bucket = std::env::var("VAULTSYNC_TEST_S3_BUCKET").unwrap();
        let region =
            std::env::var("VAULTSYNC_TEST_S3_REGION").unwrap_or_else(|_| "us-west-1".into());
        let endpoint = std::env::var("VAULTSYNC_TEST_S3_ENDPOINT").ok();
        let path_style = std::env::var("VAULTSYNC_TEST_S3_PATH_STYLE")
            .map(|v| v == "1")
            .unwrap_or(false);
        let base = std::env::var("VAULTSYNC_TEST_S3_PREFIX").unwrap_or_default();
        let other = S3Store::new(
            &StoreSettings {
                bucket,
                region: Some(region),
                endpoint: endpoint.clone(),
                // normalized trailing `/` (R5-M2): an unnormalized sibling prefix
                // would actually share the primary prefix on a raw-concat.
                prefix: format!("{base}vaultsync-other-prefix-{}/", unique_num()),
                path_style,
            },
            &RetrySettings::default(),
        )
        .expect("other store");
        put_bytes(&other, "secret.txt", b"s", None)?;
        // Capture the listing, then clean up the sibling-prefix object BEFORE
        // asserting (W55/B-L4): a failed assertion must not permanently litter
        // the shared bucket - `with_store`'s sweeper covers only the primary
        // prefix, so an assertion failure used to leave
        // `vaultsync-other-prefix-<ts>/secret.txt` behind.
        let keys: Vec<String> = s
            .list("")
            .unwrap()
            .entities
            .iter()
            .map(|e| e.key.clone())
            .collect();
        let _ = other.delete("secret.txt");
        // our store must not see it
        assert!(
            !keys.iter().any(|k| k == "secret.txt"),
            "sibling-prefix object leaked into list: {keys:?}"
        );
        assert!(keys.iter().any(|k| k == "mine.txt"));
        Ok(())
    });
}

/// Shared env resolution for the path-style flavor tests (W94/r2-M5):
/// returns `None` (caller prints the suite-skip note) when
/// `VAULTSYNC_TEST_S3_BUCKET` is unset.
fn path_style_env(name: &str) -> Option<(String, String, Option<String>, String)> {
    let bucket = match bucket_or_skip(
        std::env::var("VAULTSYNC_TEST_S3_BUCKET").ok(),
        require_mode(),
        name,
    ) {
        Ok(Some(b)) => b,
        Ok(None) => return None,
        Err(msg) => panic!("{msg}"),
    };
    let region = std::env::var("VAULTSYNC_TEST_S3_REGION").unwrap_or_else(|_| "us-west-1".into());
    let endpoint = std::env::var("VAULTSYNC_TEST_S3_ENDPOINT").ok();
    let base = std::env::var("VAULTSYNC_TEST_S3_PREFIX").unwrap_or_default();
    let prefix = format!("{base}vaultsync-itest-{name}-{}", unique_num());
    Some((bucket, region, endpoint, prefix))
}

/// Put a probe object, assert exactly one non-folder object lists back, then
/// delete the probe (W94/r2-M5). Returns an error string on failure.
fn path_style_roundtrip(flavor: &str, s: &S3Store) -> Result<(), String> {
    put_bytes(s, "f.txt", b"ps", None)?;
    let n = s
        .list("")
        .unwrap()
        .entities
        .iter()
        .filter(|e| !e.is_folder())
        .count();
    if n != 1 {
        return Err(format!("path-style {flavor}: expected 1 file, got {n}"));
    }
    // cleanup the probe
    if let Ok(listing) = s.list("") {
        for e in listing.entities {
            if !e.is_folder() {
                let _ = s.delete(&e.key);
            }
        }
    }
    Ok(())
}

#[test]
fn s3_integ_path_style_true() -> Result<(), String> {
    // r2-M5 (W94): the path-style addressing flavor must be exercised on
    // EVERY enabled run (AWS and MinIO/R2 alike) - the old combined
    // `path_style_toggle` test could pass with only one flavor via
    // `roundtrip >= 1`, masking a mis-skip. `roundtrip == stores.len()` is
    // the lock: a skip here is a loud failure, not a silent one-flavor pass.
    let Some((bucket, region, endpoint, prefix)) = path_style_env("pathstyle-true") else {
        eprintln!(
            "[skip] path-style true: VAULTSYNC_TEST_S3_BUCKET not set (set to run real S3 tests)"
        );
        return Ok(());
    };
    let stores: Vec<(bool, S3Store)> = vec![(
        true,
        S3Store::new(
            &StoreSettings {
                bucket,
                region: Some(region),
                endpoint,
                prefix,
                path_style: true,
            },
            &RetrySettings::default(),
        )
        .map_err(|e| format!("path-style true store: {e}"))?,
    )];
    let mut roundtrip = 0usize;
    for (flavor, s) in &stores {
        path_style_roundtrip(&format!("{flavor}"), s)
            .map_err(|e| format!("path-style {flavor}: {e}"))?;
        roundtrip += 1;
    }
    if roundtrip != stores.len() {
        return Err(format!(
            "path-style true: expected {} flavor(s) exercised, got {roundtrip}",
            stores.len()
        ));
    }
    eprintln!("[ok] pathstyle true");
    Ok(())
}

#[test]
fn s3_integ_path_style_vhost() -> Result<(), String> {
    // r2-M5 (W94): the vhost addressing flavor is AWS-only - a custom
    // endpoint (MinIO/R2) requires path-style addressing, so this test
    // early-returns with an explicit note there. On AWS (no endpoint) it
    // MUST exercise; the `roundtrip == stores.len()` lock makes a mis-skip
    // fail loudly instead of passing on the other flavor. (W12/A-M8
    // original rationale; split out of `path_style_toggle` in W94.)
    let Some((bucket, region, endpoint, prefix)) = path_style_env("pathstyle-vhost") else {
        eprintln!(
            "[skip] path-style vhost: VAULTSYNC_TEST_S3_BUCKET not set (set to run real S3 tests)"
        );
        return Ok(());
    };
    if endpoint.is_some() {
        eprintln!(
            "[skip] path-style vhost: VAULTSYNC_TEST_S3_ENDPOINT set (custom endpoints require path-style addressing; vhost is AWS-only - intentional branch, not a missing-config skip, even under VAULTSYNC_TEST_S3_REQUIRE=1)"
        );
        return Ok(());
    }
    let stores: Vec<(bool, S3Store)> = vec![(
        false,
        S3Store::new(
            &StoreSettings {
                bucket,
                region: Some(region),
                endpoint: None,
                prefix,
                path_style: false,
            },
            &RetrySettings::default(),
        )
        .map_err(|e| format!("path-style vhost store: {e}"))?,
    )];
    let mut roundtrip = 0usize;
    for (flavor, s) in &stores {
        path_style_roundtrip(&format!("{flavor}"), s)
            .map_err(|e| format!("path-style {flavor}: {e}"))?;
        roundtrip += 1;
    }
    if roundtrip != stores.len() {
        return Err(format!(
            "path-style vhost: expected {} flavor(s) exercised, got {roundtrip}",
            stores.len()
        ));
    }
    eprintln!("[ok] pathstyle vhost");
    Ok(())
}

#[test]
fn s3_integ_streaming_put_large() {
    // An 8 MiB body streamed from a counting reader: proves the backend pulls
    // the reader incrementally (no `size`-sized in-memory buffer, P1r-put-size).
    struct Probe {
        remaining: u64,
        max_chunk: usize,
    }
    impl Read for Probe {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.max_chunk = self.max_chunk.max(buf.len());
            if self.remaining == 0 {
                return Ok(0);
            }
            let n = (64 * 1024).min(buf.len()).min(self.remaining as usize);
            buf[..n].fill(0xAB);
            self.remaining -= n as u64;
            Ok(n)
        }
    }
    let size = 8 * 1024 * 1024u64;
    with_store("streamput", |s| {
        let mut probe = Probe {
            remaining: size,
            max_chunk: 0,
        };
        s.put_from("big.bin", &mut probe, size, Some(1_700_000_000_999))
            .map_err(|e| format!("{e}"))?;
        assert_eq!(probe.remaining, 0, "exactly size bytes consumed");
        assert!(
            probe.max_chunk <= 128 * 1024,
            "reader asked to fill {} bytes (should stream)",
            probe.max_chunk
        );
        let h = s.head("big.bin").map_err(|e| format!("{e}"))?;
        assert_eq!(h.size, size, "stored size");
        assert_eq!(h.mtime_ms, Some(1_700_000_000_999));
        Ok(())
    });
}

/// A local temp directory that removes itself (and its contents) on drop
/// (W73/A-N4): an assertion failure must not leak the dir - the old explicit
/// `remove_dir_all` calls ran only on the success path and were unreachable
/// after a panicking assert, while `with_store`'s sweeper covers bucket
/// objects only.
struct TestDir(std::path::PathBuf);

impl TestDir {
    fn new(label: &str) -> TestDir {
        let p =
            std::env::temp_dir().join(format!("vaultsync-itest-dir-{}-{}", unique_num(), label));
        std::fs::create_dir_all(&p).unwrap();
        TestDir(p)
    }

    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl std::ops::Deref for TestDir {
    type Target = std::path::Path;
    fn deref(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn mtime_ms(p: &std::path::Path) -> u64 {
    std::fs::metadata(p)
        .unwrap()
        .modified()
        .unwrap()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

fn set_mtime(p: &std::path::Path, ms: u64) {
    let t = std::time::UNIX_EPOCH + std::time::Duration::from_millis(ms);
    let times = std::fs::FileTimes::new().set_modified(t);
    std::fs::File::open(p).unwrap().set_times(times).unwrap();
}

#[test]
fn s3_integ_check() {
    // Real check (Slice 8) through the shared probe path: put/get/delete.
    with_store("check", |s| {
        vaultsync::check_store(s).map_err(|e| format!("{e}"))?;
        Ok(())
    });
}

#[test]
fn s3_integ_e2e_push_pull() {
    // The automated half of the exit criteria: push a sample vault (nested
    // folders + binary + unicode), wipe local, pull into a fresh dir, and
    // byte- and mtime-compare.
    with_store("e2e", |s| {
        let src = TestDir::new("src");
        std::fs::create_dir_all(src.join("notes")).unwrap();
        let files: Vec<(&str, Vec<u8>)> = vec![
            ("note.md", b"hello world\n".to_vec()),
            (
                "notes/img.png",
                vec![0x89, 0x50, 0x4e, 0x47, 0x00, 0x0a, 0xff, 0xfe],
            ),
            (
                "notes/\u{4e2d}\u{6587}.md",
                "\u{63a8}\u{8350}\u{ff01}\n".to_string().into_bytes(),
            ),
        ];
        let fixed = 1_600_000_000_123u64;
        for (rel, bytes) in &files {
            let path = src.join(rel);
            std::fs::write(&path, bytes).unwrap();
            set_mtime(&path, fixed);
        }

        // push
        let local = LocalFs::new(src.path());
        let plan = vaultsync::build_plan(&local, s, Mode::Push, &PlanOpts::default())
            .map_err(|e| format!("{e}"))?
            .plan;
        let rep = vaultsync::exec::execute_plan(&local, s, &plan, Mode::Push, &PlanOpts::default());
        assert!(rep.failed.is_empty(), "push failures: {:?}", rep.failed);

        // wipe the source, pull into a fresh dir
        let _ = std::fs::remove_dir_all(src.path());
        let dst = TestDir::new("dst");
        let ldst = LocalFs::new(dst.path());
        let plan2 = vaultsync::build_plan(&ldst, s, Mode::Pull, &PlanOpts::default())
            .map_err(|e| format!("{e}"))?
            .plan;
        let rep2 =
            vaultsync::exec::execute_plan(&ldst, s, &plan2, Mode::Pull, &PlanOpts::default());
        assert!(rep2.failed.is_empty(), "pull failures: {:?}", rep2.failed);

        // byte- and mtime-compare each expected file
        for (rel, bytes) in &files {
            let got = std::fs::read(dst.join(rel)).map_err(|e| format!("{rel}: {e}"))?;
            assert_eq!(&got, bytes, "byte mismatch for {rel}");
            let gm = mtime_ms(&dst.join(rel));
            assert!(gm.abs_diff(fixed) < 2000, "{rel} mtime {gm} != {fixed}");
        }

        Ok(())
    });
}

#[test]
fn s3_integ_status_converges_after_push() {
    // Issue #15 acceptance bullet 1 (W112): after a real push, a Status plan
    // on the same vault plans 0 mutating actions. RED on real S3 before the
    // W113 list-enrich wiring (list sees upload LastModified, so it plans N
    // downloads - the exact issue report); GREEN after. Env-gated like the
    // rest of this suite.
    with_store("statusconv", |s| {
        let src = TestDir::new("src");
        std::fs::create_dir_all(src.join("notes")).unwrap();
        let files: Vec<(&str, Vec<u8>)> = vec![
            ("note.md", b"hello world\n".to_vec()),
            ("notes/a.md", b"nested\n".to_vec()),
            ("b.md", b"bb\n".to_vec()),
        ];
        let fixed = 1_600_000_000_123u64;
        for (rel, bytes) in &files {
            let path = src.join(rel);
            std::fs::write(&path, bytes).unwrap();
            set_mtime(&path, fixed);
        }
        let local = LocalFs::new(src.path());
        let plan = vaultsync::build_plan(&local, s, Mode::Push, &PlanOpts::default())
            .map_err(|e| format!("{e}"))?
            .plan;
        // W120/R1-M3: assert the push plan actually planned the seeded uploads
        // (a vacuous pass on an empty push would hide regressions).
        assert_eq!(
            plan.stats.upload,
            files.len() as u32,
            "push plan must plan the seeded uploads: {:?}",
            plan.actions
        );
        let rep = vaultsync::exec::execute_plan(&local, s, &plan, Mode::Push, &PlanOpts::default());
        assert!(rep.failed.is_empty(), "push failures: {:?}", rep.failed);
        assert_eq!(
            rep.executed,
            files.len() as u32,
            "push must execute exactly the seeded uploads: {:?}",
            rep
        );
        // per-key size sanity: each upload landed with the true byte count.
        for (rel, bytes) in &files {
            let h = s.head(rel).map_err(|e| format!("{e}"))?.size;
            assert_eq!(h, bytes.len() as u64, "uploaded {rel} size wrong");
        }

        let status = vaultsync::build_plan(&local, s, Mode::Status, &PlanOpts::default())
            .map_err(|e| format!("{e}"))?
            .plan;
        assert_eq!(status.stats.upload, 0, "uploads: {:?}", status.actions);
        assert_eq!(status.stats.download, 0, "downloads: {:?}", status.actions);
        assert_eq!(status.stats.conflict, 0, "conflicts: {:?}", status.actions);
        Ok(())
    });
}

#[test]
fn bucket_or_skip_sentinel_unit() {
    // I6-sentinel (issue #6): the skip-or-require decision, all three arms.
    // Pure-function test - no env mutation, runs everywhere (no S3 needed),
    // including in the CI integration job where REQUIRE=1 is set.
    assert_eq!(
        bucket_or_skip(Some("b".into()), false, "t").unwrap(),
        Some("b".to_string()),
        "bucket set, require off: run"
    );
    assert_eq!(
        bucket_or_skip(Some("b".into()), true, "t").unwrap(),
        Some("b".to_string()),
        "bucket set, require on: run"
    );
    assert_eq!(
        bucket_or_skip(None, false, "t").unwrap(),
        None,
        "bucket missing, require off: skip"
    );
    // Empty string counts as missing (break-test finding: a deleted GitHub
    // repo variable expands to "", not to an unset var).
    assert_eq!(
        bucket_or_skip(Some(String::new()), false, "t").unwrap(),
        None,
        "bucket empty, require off: skip"
    );
    let err = bucket_or_skip(Some(String::new()), true, "t").unwrap_err();
    assert!(
        err.contains("VAULTSYNC_TEST_S3_BUCKET") && err.contains("VAULTSYNC_TEST_S3_REQUIRE"),
        "bucket empty, require on: loud failure naming both vars: {err}"
    );
    let err = bucket_or_skip(None, true, "t").unwrap_err();
    assert!(
        err.contains("VAULTSYNC_TEST_S3_BUCKET") && err.contains("VAULTSYNC_TEST_S3_REQUIRE"),
        "bucket missing, require on: loud failure naming both vars: {err}"
    );
}

#[test]
fn s3_integ_pull_then_status_converges() {
    // Issue #15 acceptance bullet 2 (W112): wipe local, pull into a fresh dir
    // (exact mtimes restored), then a Status plan there plans 0 mutating
    // actions - download-direction incrementality exists. RED on real S3
    // before the W113 wiring; GREEN after.
    with_store("pullconv", |s| {
        let src = TestDir::new("src");
        std::fs::create_dir_all(src.join("notes")).unwrap();
        let files: Vec<(&str, Vec<u8>)> = vec![
            ("note.md", b"hello world\n".to_vec()),
            ("notes/a.md", b"nested\n".to_vec()),
        ];
        let fixed = 1_600_000_000_123u64;
        for (rel, bytes) in &files {
            let path = src.join(rel);
            std::fs::write(&path, bytes).unwrap();
            set_mtime(&path, fixed);
        }
        // stage the remote
        let local = LocalFs::new(src.path());
        let plan = vaultsync::build_plan(&local, s, Mode::Push, &PlanOpts::default())
            .map_err(|e| format!("{e}"))?
            .plan;
        let rep = vaultsync::exec::execute_plan(&local, s, &plan, Mode::Push, &PlanOpts::default());
        assert!(rep.failed.is_empty(), "push failures: {:?}", rep.failed);

        // wipe local, pull into a fresh dir
        let _ = std::fs::remove_dir_all(src.path());
        let dst = TestDir::new("dst");
        let ldst = LocalFs::new(dst.path());
        let plan2 = vaultsync::build_plan(&ldst, s, Mode::Pull, &PlanOpts::default())
            .map_err(|e| format!("{e}"))?
            .plan;
        let rep2 =
            vaultsync::exec::execute_plan(&ldst, s, &plan2, Mode::Pull, &PlanOpts::default());
        assert!(rep2.failed.is_empty(), "pull failures: {:?}", rep2.failed);

        // exact mtimes restored (existing feature must hold)
        for (rel, _bytes) in &files {
            let gm = mtime_ms(&dst.join(rel));
            assert!(gm.abs_diff(fixed) < 2000, "{rel} mtime {gm} != {fixed}");
        }

        // status on the fresh dir plans 0 mutating actions
        let status = vaultsync::build_plan(&ldst, s, Mode::Status, &PlanOpts::default())
            .map_err(|e| format!("{e}"))?
            .plan;
        assert_eq!(status.stats.upload, 0, "uploads: {:?}", status.actions);
        assert_eq!(status.stats.download, 0, "downloads: {:?}", status.actions);
        assert_eq!(status.stats.conflict, 0, "conflicts: {:?}", status.actions);
        Ok(())
    });
}
