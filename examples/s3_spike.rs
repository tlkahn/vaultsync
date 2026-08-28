//! Slice 0 spike probe (THROWAWAY, NOT TDD).
//!
//! Validates the official S3 stack (`aws-sdk-s3` + `aws-config` + `tokio`)
//! before any production backend code. Covers the 6 probe items:
//!   1. list (paginated ListObjectsV2), head, get, put, delete
//!   2. write + read back user-metadata mtime (`vaultsync-mtime`, decimal ms)
//!   3. prefix support (`myvault/` style)
//!   4. path-style addressing toggle
//!   5. custom endpoint (R2) AND AWS default endpoint
//!   6. credentials from the default AWS chain via `aws-config`
//!
//! Env:
//!   VS_SPIKE_BUCKET   (required)      writable bucket
//!   VS_SPIKE_REGION   (default us-west-1)
//!   VS_SPIKE_PREFIX   (default myvault/)
//!   VS_SPIKE_ENDPOINT (optional)      custom endpoint (R2/minio)
//!   VS_SPIKE_PATH_STYLE (0/1, default 0)
//!
//! Credentials come from the ambient AWS default chain (env, shared
//! credentials file, profile). Probe objects are tiny and removed after.

use aws_sdk_s3::Client;
use aws_sdk_s3::primitives::ByteStream;

const MTIME_KEY: &str = "vaultsync-mtime";

fn println_ok(tag: &str, msg: &str) {
    println!("[ok] {tag}: {msg}");
}
async fn put_bytes(
    client: &Client,
    bucket: &str,
    key: &str,
    body: &[u8],
    mtime: Option<u64>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut req = client
        .put_object()
        .bucket(bucket)
        .key(key)
        .body(ByteStream::from(body.to_vec()));
    if let Some(ms) = mtime {
        req = req.metadata(MTIME_KEY, ms.to_string());
    }
    req.send().await?;
    Ok(())
}

async fn probe_put_head_get_delete(
    client: &Client,
    bucket: &str,
    prefix: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let key = format!("{prefix}spike-probe.txt");
    // 1. put
    put_bytes(
        client,
        bucket,
        &key,
        b"hello spike",
        Some(1_700_000_000_123),
    )
    .await?;
    println_ok("put", &format!("wrote {key}"));
    // head
    let head = client.head_object().bucket(bucket).key(&key).send().await?;
    let size = head.content_length().unwrap_or(0);
    println_ok("head", &format!("{key} size={size}"));
    // metadata mtime read-back (item 2)
    let mtime = head
        .metadata()
        .and_then(|m| m.get(MTIME_KEY).cloned())
        .ok_or("no vaultsync-mtime metadata in head")?;
    println_ok("metadata-mtime", &format!("head {}={}", MTIME_KEY, mtime));
    assert_eq!(mtime, "1700000000123", "metadata mtime mismatch");
    // get
    let body = client
        .get_object()
        .bucket(bucket)
        .key(&key)
        .send()
        .await?
        .body
        .collect()
        .await?
        .into_bytes();
    assert_eq!(body.as_ref(), b"hello spike", "get body mismatch");
    println_ok("get", &format!("read {key} -> {:?}", body.as_ref()));
    // delete
    client
        .delete_object()
        .bucket(bucket)
        .key(&key)
        .send()
        .await?;
    let after = client.head_object().bucket(bucket).key(&key).send().await;
    assert!(after.is_err(), "head after delete should fail");
    println_ok("delete", &format!("removed {key}"));
    Ok(())
}

async fn probe_prefix_isolation(
    client: &Client,
    bucket: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    // item 3: prefix scoping - objects under two prefixes must not mix.
    let p1 = "vs-spike-a/";
    let p2 = "vs-spike-b/";
    put_bytes(client, bucket, &format!("{p1}x.txt"), b"a", None).await?;
    put_bytes(client, bucket, &format!("{p2}x.txt"), b"b", None).await?;
    // list with prefix p1 -> only p1
    let l1 = client
        .list_objects_v2()
        .bucket(bucket)
        .prefix(p1)
        .send()
        .await?;
    let keys1: Vec<String> = l1
        .contents()
        .iter()
        .map(|o| o.key().map(|k| k.to_string()).unwrap_or_default())
        .collect();
    assert!(
        keys1.iter().all(|k| k.starts_with(p1)),
        "prefix isolation broken: {keys1:?}"
    );
    assert_eq!(keys1.len(), 1, "p1 count: {keys1:?}");
    println_ok("prefix", &format!("{p1} isolated -> {keys1:?}"));
    client
        .delete_object()
        .bucket(bucket)
        .key(format!("{p1}x.txt"))
        .send()
        .await?;
    client
        .delete_object()
        .bucket(bucket)
        .key(format!("{p2}x.txt"))
        .send()
        .await?;
    Ok(())
}

async fn probe_paginated_list(
    client: &Client,
    bucket: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    // item 1: paginated ListObjectsV2. Seed >1000 tiny objects (S3 pages at
    // 1000), paginate, confirm we see every one.
    let prefix = "vs-spike-pages/";
    const N: usize = 1100;
    // Bounded window: fire up to W puts in flight at once to avoid throttling.
    const W: usize = 25;
    let mut inflight: tokio::task::JoinSet<Result<(), String>> = tokio::task::JoinSet::new();
    for i in 0..N {
        let c = client.clone();
        let b = bucket.to_string();
        let p = prefix.to_string();
        inflight.spawn(async move {
            put_bytes(&c, &b, &format!("{p}obj-{i:05}.dat"), b"x", None)
                .await
                .map_err(|e| e.to_string())
        });
        if inflight.len() >= W {
            if let Some(res) = inflight.join_next().await {
                res.map_err(|e| e.to_string())??;
            }
        }
    }
    while let Some(res) = inflight.join_next().await {
        res.map_err(|e| e.to_string())??;
    }
    let mut count = 0usize;
    let mut marker: Option<String> = None;
    loop {
        let mut req = client
            .list_objects_v2()
            .bucket(bucket)
            .prefix(prefix)
            .max_keys(1000);
        if let Some(m) = marker.as_deref() {
            req = req.continuation_token(m);
        }
        let resp = req.send().await?;
        count += resp.contents().len();
        match resp.next_continuation_token() {
            Some(tok) if !tok.is_empty() => marker = Some(tok.to_string()),
            _ => break,
        }
    }
    assert_eq!(count, N, "paginated count mismatch: {count} != {N}");
    println_ok("paginated-list", &format!("seeded {N}, listed {count}"));
    let mut inflight: tokio::task::JoinSet<Result<(), String>> = tokio::task::JoinSet::new();
    for i in 0..N {
        let c = client.clone();
        let b = bucket.to_string();
        let p = prefix.to_string();
        inflight.spawn(async move {
            c.delete_object()
                .bucket(&b)
                .key(format!("{p}obj-{i:05}.dat"))
                .send()
                .await
                .map_err(|e| e.to_string())?;
            Ok(())
        });
        if inflight.len() >= W {
            if let Some(res) = inflight.join_next().await {
                res.map_err(|e| e.to_string())??;
            }
        }
    }
    while let Some(res) = inflight.join_next().await {
        res.map_err(|e| e.to_string())??;
    }
    println_ok("paginated-list-cleanup", &format!("deleted {N}"));
    Ok(())
}

async fn probe_path_style(client: &Client, bucket: &str) -> Result<(), Box<dyn std::error::Error>> {
    // item 4: path-style toggle. The client was built with force_path_style;
    // a put/list under a unique prefix must succeed.
    let prefix = "vs-spike-pathstyle/";
    put_bytes(client, bucket, &format!("{prefix}f.txt"), b"ps", None).await?;
    let l = client
        .list_objects_v2()
        .bucket(bucket)
        .prefix(prefix)
        .send()
        .await?;
    let n = l.contents().len();
    assert_eq!(n, 1, "path-style put/list failed, got {n} objects");
    println_ok("path-style", "force_path_style put+list OK");
    client
        .delete_object()
        .bucket(bucket)
        .key(format!("{prefix}f.txt"))
        .send()
        .await?;
    Ok(())
}

// W74 (B nit): no `#[tokio::main]` here - the throwaway spike builds a
// current-thread runtime manually so the package can stay on the minimal
// `tokio/rt` feature (no `macros`).
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(async_main())
}

async fn async_main() -> Result<(), Box<dyn std::error::Error>> {
    let bucket =
        std::env::var("VS_SPIKE_BUCKET").map_err(|_| "VS_SPIKE_BUCKET is required".to_string())?;
    let region = std::env::var("VS_SPIKE_REGION").unwrap_or_else(|_| "us-west-1".to_string());
    let prefix_override =
        std::env::var("VS_SPIKE_PREFIX").unwrap_or_else(|_| "myvault/".to_string());
    let endpoint = std::env::var("VS_SPIKE_ENDPOINT").ok();
    let path_style = std::env::var("VS_SPIKE_PATH_STYLE")
        .map(|v| v == "1")
        .unwrap_or(false);

    // item 6: default AWS credential chain + region via aws-config.
    println!(
        "== spike: region={region} endpoint={:?} path_style={path_style} ==",
        endpoint
    );
    let base = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
    let mut b = aws_sdk_s3::config::Builder::from(&base)
        .region(aws_sdk_s3::config::Region::new(region))
        .force_path_style(path_style);
    if let Some(ep) = &endpoint {
        b = b.endpoint_url(ep); // item 5: custom endpoint (R2/minio)
    }
    let conf = b.build();
    let client = Client::from_conf(conf);

    probe_put_head_get_delete(&client, &bucket, &prefix_override).await?;
    probe_prefix_isolation(&client, &bucket).await?;
    probe_paginated_list(&client, &bucket).await?;
    probe_path_style(&client, &bucket).await?;

    println!("== spike: ALL PROBES PASSED ==");
    Ok(())
}
