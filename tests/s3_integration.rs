//! Env-gated S3 integration suite (Slice 7/8/10).
//!
//! Compiled always, but every test **skips** at runtime (printing a note)
//! unless `VAULTSYNC_TEST_S3_BUCKET` is set. Optional:
//! `VAULTSYNC_TEST_S3_ENDPOINT`, `VAULTSYNC_TEST_S3_REGION`,
//! `VAULTSYNC_TEST_S3_PREFIX`, `VAULTSYNC_TEST_S3_PATH_STYLE=1`.
//!
//! Credentials come from the ambient AWS default chain (env, shared
//! credentials file, profile); the tests never read secret values themselves.
//!
//! Each test runs under a unique `vaultsync-itest-<ts>-<name>/` prefix and
//! cleans up its objects afterwards (Drop/finally-style via `with_store`).

use std::io::Read;
use std::sync::atomic::{AtomicU64, Ordering};

use vaultsync::config::StoreSettings;
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
    let bucket = match std::env::var("VAULTSYNC_TEST_S3_BUCKET") {
        Ok(b) => b,
        Err(_) => {
            eprintln!(
                "[skip] {}: VAULTSYNC_TEST_S3_BUCKET not set (set to run real S3 tests)",
                name
            );
            return;
        }
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
    let store = S3Store::new(&settings).map_err(|e| format!("S3Store::new: {e}"));
    let store = match store {
        Ok(s) => s,
        Err(e) => {
            panic!("{name}: failed to build store against {bucket}: {e}");
        }
    };

    let result = f(&store);

    // Cleanup: list + delete everything (files only; folders are views).
    if let Ok(ents) = store.list("") {
        for e in ents {
            if !e.is_folder() {
                let _ = store.delete(&e.key);
            }
        }
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
        let ents = s.list("").map_err(|e| format!("{e}"))?;
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
        let other = S3Store::new(&StoreSettings {
            bucket,
            region: Some(region),
            endpoint: endpoint.clone(),
            // normalized trailing `/` (R5-M2): an unnormalized sibling prefix
            // would actually share the primary prefix on a raw-concat.
            prefix: format!("{base}vaultsync-other-prefix-{}/", unique_num()),
            path_style,
        })
        .expect("other store");
        put_bytes(&other, "secret.txt", b"s", None)?;
        // our store must not see it
        let keys: Vec<String> = s.list("").unwrap().iter().map(|e| e.key.clone()).collect();
        assert!(
            !keys.iter().any(|k| k == "secret.txt"),
            "sibling-prefix object leaked into list: {keys:?}"
        );
        assert!(keys.iter().any(|k| k == "mine.txt"));
        // clean up the other store's object
        let _ = other.delete("secret.txt");
        Ok(())
    });
}

#[test]
fn s3_integ_path_style_toggle() -> Result<(), String> {
    // W12/A-M8: build the same bucket/prefix with BOTH path-style flavors and
    // require a put/list round-trip on each. The old test asserted only that
    // one configured flavor put/list succeeded and could not fail either way.
    let Some(bucket) = std::env::var("VAULTSYNC_TEST_S3_BUCKET").ok() else {
        eprintln!("[skip] pathstyle: VAULTSYNC_TEST_S3_BUCKET not set (set to run real S3 tests)");
        return Ok(());
    };
    let region = std::env::var("VAULTSYNC_TEST_S3_REGION").unwrap_or_else(|_| "us-west-1".into());
    let endpoint = std::env::var("VAULTSYNC_TEST_S3_ENDPOINT").ok();
    let base = std::env::var("VAULTSYNC_TEST_S3_PREFIX").unwrap_or_default();
    let prefix = format!("{base}vaultsync-itest-pathstyle-{}", unique_num());

    let mut stores: Vec<(bool, S3Store)> = Vec::new();
    stores.push((
        true,
        S3Store::new(&StoreSettings {
            bucket: bucket.clone(),
            region: Some(region.clone()),
            endpoint: endpoint.clone(),
            prefix: prefix.clone(),
            path_style: true,
        })
        .map_err(|e| format!("path-style true store: {e}"))?,
    ));
    // Skip the false flavor only when a custom endpoint is known to require
    // path-style addressing (the true flavor above still exercised).
    if endpoint.is_none() {
        stores.push((
            false,
            S3Store::new(&StoreSettings {
                bucket: bucket.clone(),
                region: Some(region.clone()),
                endpoint: None,
                prefix: prefix.clone(),
                path_style: false,
            })
            .map_err(|e| format!("path-style false store: {e}"))?,
        ));
    }

    let mut roundtrip = 0usize;
    let mut result: Result<(), String> = Ok(());
    for (flavor, s) in &stores {
        if let Err(e) = (|| -> Result<(), String> {
            put_bytes(s, "f.txt", b"ps", None)?;
            let n = s
                .list("")
                .unwrap()
                .iter()
                .filter(|e| !e.is_folder())
                .count();
            if n != 1 {
                return Err(format!("path-style {flavor}: expected 1 file, got {n}"));
            }
            Ok(())
        })() {
            result = Err(format!("path-style {flavor}: {e}"));
            break;
        }
        roundtrip += 1;
    }

    // cleanup both stores' objects (same prefix)
    for (_, s) in &stores {
        if let Ok(ents) = s.list("") {
            for e in ents {
                if !e.is_folder() {
                    let _ = s.delete(&e.key);
                }
            }
        }
    }

    if roundtrip == 0 {
        return Err("no path-style flavor was exercised".to_string());
    }
    eprintln!("[ok] pathstyle");
    result
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

fn temp_dir(label: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!("vaultsync-itest-dir-{}-{}", unique_num(), label));
    std::fs::create_dir_all(&p).unwrap();
    p
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
        let src = temp_dir("src");
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
        let local = LocalFs::new(&src);
        let plan = vaultsync::build_plan(&local, s, Mode::Push, &PlanOpts::default())
            .map_err(|e| format!("{e}"))?;
        let rep = vaultsync::exec::execute_plan(&local, s, &plan, Mode::Push, &PlanOpts::default());
        assert!(rep.failed.is_empty(), "push failures: {:?}", rep.failed);

        // wipe the source, pull into a fresh dir
        let _ = std::fs::remove_dir_all(&src);
        let dst = temp_dir("dst");
        let ldst = LocalFs::new(&dst);
        let plan2 = vaultsync::build_plan(&ldst, s, Mode::Pull, &PlanOpts::default())
            .map_err(|e| format!("{e}"))?;
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

        let _ = std::fs::remove_dir_all(&dst);
        Ok(())
    });
}
