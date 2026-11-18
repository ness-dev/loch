//! Ported from test/test_store.py.
//!
//! The file store is tested fully. The Python's S3 tests ran against moto
//! in-process; Rust has no mock, so what needs no network (URI parsing,
//! describe(), prefixes) runs always and the rest is `#[ignore]`d, to be run
//! by hand against MinIO or `moto_server`:
//!
//!     LOCH_TEST_S3_ENDPOINT=http://127.0.0.1:5000 cargo test --test store -- --ignored
//!
//! The bucket named by LOCH_TEST_S3_BUCKET (default `sessions`) must already
//! exist; each test works under its own unique prefix within it.

use loch::s3::S3Store;
use loch::store::{create_store, default_store, resolve_store_uri, FileStore, Store, StoreError};
use std::fs;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};

/// Every test that reads or writes process-wide environment holds this.
static ENV: Mutex<()> = Mutex::new(());

fn env_lock() -> MutexGuard<'static, ()> {
    ENV.lock().unwrap_or_else(|e| e.into_inner())
}

fn tmp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("loch-test-store-{}-{}", std::process::id(), name));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn s(path: &std::path::Path) -> String {
    path.to_str().unwrap().to_string()
}

#[test]
fn file_store_round_trips() {
    let root = s(&tmp_dir("round-trip").join("store"));
    let store = FileStore::new(root.as_str());
    store.put("abc.loch", b"payload").unwrap();
    assert_eq!(store.get("abc.loch").unwrap(), b"payload");
    assert_eq!(store.describe(), root);
}

#[test]
fn file_store_lists_by_prefix_and_deletes() {
    let store = FileStore::new(s(&tmp_dir("list-delete").join("store")));
    store.put("bundles/abc.loch", b"a").unwrap();
    store.put("index/branch/main/abc", b"b").unwrap();
    store.put("index/branch/feature%2Ffoo/abc", b"c").unwrap();

    assert_eq!(
        store.list("").unwrap(),
        vec![
            "bundles/abc.loch",
            "index/branch/feature%2Ffoo/abc",
            "index/branch/main/abc",
        ]
    );
    assert_eq!(
        store.list("index/branch/ma").unwrap(),
        vec!["index/branch/main/abc"]
    );
    assert!(store.list("nothing/").unwrap().is_empty());

    store.delete("index/branch/main/abc").unwrap();
    store.delete("index/branch/main/abc").unwrap();
    assert_eq!(
        store.list("index/").unwrap(),
        vec!["index/branch/feature%2Ffoo/abc"]
    );
}

#[test]
fn file_store_lists_nothing_before_anything_is_written() {
    let store = FileStore::new(s(&tmp_dir("nothing-yet").join("missing")));
    assert!(store.list("").unwrap().is_empty());
}

#[test]
fn file_store_reports_an_absent_object_as_missing_not_as_an_os_error() {
    let store = FileStore::new(s(&tmp_dir("absent").join("store")));
    assert!(matches!(
        store.get("bundles/nope.loch"),
        Err(StoreError::Missing(_))
    ));
}

/// A store that can't answer is not a store answering "absent" — the caller
/// retries one and gives up on the other.
#[test]
fn file_store_lets_an_unreachable_root_surface_as_itself() {
    let blocked = tmp_dir("unreachable").join("blocked");
    fs::write(&blocked, "not a directory").unwrap();

    assert!(matches!(
        FileStore::new(s(&blocked)).get("bundles/nope.loch"),
        Err(StoreError::Unreachable(_))
    ));
}

#[test]
fn create_store_parses_uris() {
    let tmp = s(&tmp_dir("parses"));
    // No isinstance in Rust: describe() is what tells the backends apart.
    assert_eq!(create_store(&tmp).unwrap().describe(), tmp);
    assert_eq!(
        create_store(&format!("file://{}", tmp)).unwrap().describe(),
        tmp
    );
    let cwd = std::env::current_dir().unwrap();
    assert_eq!(
        create_store("relative/path").unwrap().describe(),
        s(&cwd.join("relative/path"))
    );

    assert_eq!(
        create_store("s3://bucket/sessions/").unwrap().describe(),
        "s3://bucket/sessions"
    );
    assert_eq!(
        create_store("s3://bucket").unwrap().describe(),
        "s3://bucket/"
    );
}

#[test]
fn create_store_rejects_a_bucketless_s3_uri() {
    let Err(error) = create_store("s3:///sessions") else {
        panic!("a bucketless URI was accepted");
    };
    assert!(error.0.contains("invalid S3 store URI"), "{}", error.0);
}

#[test]
fn resolve_store_uri_prefers_explicit_then_env() {
    let _guard = env_lock();
    let saved = std::env::var_os("LOCH_STORE");

    std::env::remove_var("LOCH_STORE");
    let unset = resolve_store_uri(None);
    std::env::set_var("LOCH_STORE", "s3://from-env/x");
    let from_env = resolve_store_uri(None);
    let explicit = resolve_store_uri(Some("/explicit"));

    match saved {
        Some(value) => std::env::set_var("LOCH_STORE", value),
        None => std::env::remove_var("LOCH_STORE"),
    }

    assert_eq!(unset, default_store());
    assert_eq!(from_env, "s3://from-env/x");
    assert_eq!(explicit, "/explicit");
}

#[test]
fn s3_store_describes_bucket_and_prefix() {
    assert_eq!(
        S3Store::new("sessions", "bundles").describe(),
        "s3://sessions/bundles"
    );
    assert_eq!(S3Store::new("sessions", "").describe(), "s3://sessions/");
}

// Skipped: test_s3_store_reports_an_uninstalled_sdk_as_such. boto3 being an
// optional extra is Python packaging; the Rust build carries its own client,
// so MissingDependency (s3-extra-missing) cannot arise.

/// Points the S3 backend at LOCH_TEST_S3_ENDPOINT and returns the bucket and
/// a prefix no other run uses. Holds the env lock for the test's duration.
fn live(name: &str) -> (MutexGuard<'static, ()>, String, String) {
    let guard = env_lock();
    let endpoint = std::env::var("LOCH_TEST_S3_ENDPOINT")
        .expect("set LOCH_TEST_S3_ENDPOINT to an S3-compatible endpoint");
    std::env::set_var("LOCH_S3_ENDPOINT", endpoint);
    std::env::remove_var("LOCH_S3_REGION");
    for (variable, default) in [
        ("AWS_ACCESS_KEY_ID", "testing"),
        ("AWS_SECRET_ACCESS_KEY", "testing"),
        ("AWS_DEFAULT_REGION", "us-east-1"),
    ] {
        if std::env::var_os(variable).is_none() {
            std::env::set_var(variable, default);
        }
    }
    let bucket = std::env::var("LOCH_TEST_S3_BUCKET").unwrap_or_else(|_| "sessions".into());
    let prefix = format!(
        "loch-test-store-{}-{}-{}",
        std::process::id(),
        name,
        loch::sys::uuid4()
    );
    (guard, bucket, prefix)
}

/// The Python also checked the stored ContentType was application/gzip by
/// reading the object back through boto3; the Store interface can't see
/// headers, so only the round trip is checked here.
#[test]
#[ignore = "needs an S3 endpoint: set LOCH_TEST_S3_ENDPOINT"]
fn s3_store_round_trips() {
    let (_guard, bucket, prefix) = live("round-trip");
    let store = S3Store::new(&bucket, &format!("{}/bundles", prefix));
    store.put("abc.loch", b"payload").unwrap();

    assert_eq!(store.get("abc.loch").unwrap(), b"payload");
    // Stored under the prefix, as `bundles/abc.loch` was in the Python.
    assert_eq!(
        S3Store::new(&bucket, &prefix).list("").unwrap(),
        vec!["bundles/abc.loch"]
    );
}

#[test]
#[ignore = "needs an S3 endpoint: set LOCH_TEST_S3_ENDPOINT"]
fn s3_store_reports_an_absent_key_as_missing() {
    let (_guard, bucket, prefix) = live("absent");
    assert!(matches!(
        S3Store::new(&bucket, &format!("{}/bundles", prefix)).get("abc.loch"),
        Err(StoreError::Missing(_))
    ));
}

#[test]
#[ignore = "needs an S3 endpoint: set LOCH_TEST_S3_ENDPOINT"]
fn s3_store_lets_a_missing_bucket_surface_as_itself() {
    let (_guard, _bucket, prefix) = live("missing-bucket");
    assert!(matches!(
        S3Store::new(&format!("no-such-bucket-{}", prefix), "bundles").get("abc.loch"),
        Err(StoreError::Unreachable(_))
    ));
}

/// With no prefix the key is used as-is, with nothing prepended. Checked by
/// writing through an unprefixed store and listing through a prefixed one,
/// since the bucket's root is shared with other runs.
#[test]
#[ignore = "needs an S3 endpoint: set LOCH_TEST_S3_ENDPOINT"]
fn s3_store_without_a_prefix() {
    let (_guard, bucket, prefix) = live("no-prefix");
    let store = S3Store::new(&bucket, "");
    store
        .put(&format!("{}/abc.loch", prefix), b"payload")
        .unwrap();

    assert_eq!(
        S3Store::new(&bucket, &prefix).list("").unwrap(),
        vec!["abc.loch"]
    );
    assert_eq!(
        store.list(&prefix).unwrap(),
        vec![format!("{}/abc.loch", prefix)]
    );
}

#[test]
#[ignore = "needs an S3 endpoint: set LOCH_TEST_S3_ENDPOINT"]
fn s3_store_lists_by_prefix_and_deletes() {
    let (_guard, bucket, prefix) = live("list-delete");
    let store = S3Store::new(&bucket, &format!("{}/bundles", prefix));
    store.put("bundles/abc.loch", b"a").unwrap();
    store.put("index/branch/main/abc", b"b").unwrap();

    assert_eq!(
        store.list("").unwrap(),
        vec!["bundles/abc.loch", "index/branch/main/abc"]
    );
    assert_eq!(store.list("index/").unwrap(), vec!["index/branch/main/abc"]);

    store.delete("index/branch/main/abc").unwrap();
    store.delete("index/branch/main/abc").unwrap();
    assert_eq!(store.list("").unwrap(), vec!["bundles/abc.loch"]);
}

/// ListObjectsV2 truncates at 1000 keys.
#[test]
#[ignore = "needs an S3 endpoint: set LOCH_TEST_S3_ENDPOINT"]
fn s3_store_lists_past_the_first_page() {
    let (_guard, bucket, prefix) = live("pages");
    let store = S3Store::new(&bucket, &prefix);
    for number in 0..1001 {
        store
            .put(&format!("index/branch/main/{:04}", number), b"")
            .unwrap();
    }

    assert_eq!(store.list("index/").unwrap().len(), 1001);
}

/// Against AWS the Python created the bucket in us-east-2; against a custom
/// endpoint the override only changes the signing region, which is what this
/// exercises.
#[test]
#[ignore = "needs an S3 endpoint: set LOCH_TEST_S3_ENDPOINT"]
fn s3_store_honours_the_region_override() {
    let (_guard, bucket, prefix) = live("region");
    std::env::set_var("LOCH_S3_REGION", "us-east-2");
    let store = S3Store::new(&bucket, &prefix);
    let put = store.put("abc.loch", b"payload");
    let got = store.get("abc.loch");
    std::env::remove_var("LOCH_S3_REGION");

    put.unwrap();
    assert_eq!(got.unwrap(), b"payload");
}
