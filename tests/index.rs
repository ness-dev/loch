//! Ported from test/test_index.py.

use loch::bundle::{bundle_ref, pack_bundle, Bundle, BundleOrigin, BundleSession, BUNDLE_VERSION};
use loch::index::{
    bundle_key, delete_bundle, dimension_prefix, encode_segment, entries_for, index_bundle,
    index_key, load_entries, ref_from_key, reindex, unquote, value_prefix, IndexEntry, IndexError,
    BRANCH, UNSET_SEGMENT,
};
use loch::s3::S3Store;
use loch::store::{FileStore, Store, StoreError};
use std::fs;
use std::path::PathBuf;

/// index.py's `decode_segment`, which the Rust build has no use for: the
/// inverse of `encode_segment`.
fn decode_segment(segment: &str) -> Option<String> {
    if segment == UNSET_SEGMENT {
        None
    } else {
        Some(unquote(segment))
    }
}

fn tmp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("loch-test-index-{}-{}", std::process::id(), name));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn file_store(name: &str) -> FileStore {
    FileStore::new(tmp_dir(name).join("store").to_str().unwrap())
}

fn session(session_id: &str, branch: Option<&str>) -> BundleSession {
    BundleSession {
        session_id: session_id.into(),
        cwd: "/repo".into(),
        git_branch: branch.map(str::to_string),
        claude_version: Some("1.0.99".into()),
        modified_at: "2026-08-15T12:00:00.000Z".into(),
        transcript: "{}\n".into(),
    }
}

fn make_bundle(sessions: Vec<BundleSession>, created_at: &str) -> Bundle {
    Bundle {
        version: BUNDLE_VERSION,
        agent: "claude-code".into(),
        created_at: created_at.into(),
        origin: BundleOrigin {
            home: "/home/origin".into(),
            platform: "linux".into(),
            hostname: "box".into(),
        },
        sessions,
    }
}

const CREATED: &str = "2026-08-15T12:00:00.000Z";

fn put_bundle(store: &dyn Store, bundle: &Bundle) -> (String, usize) {
    let packed = pack_bundle(bundle).unwrap();
    let ref_ = bundle_ref(&packed);
    store.put(&bundle_key(&ref_), &packed).unwrap();
    index_bundle(store, bundle, &ref_, packed.len()).unwrap();
    (ref_, packed.len())
}

fn refs(entries: &[IndexEntry]) -> Vec<String> {
    entries.iter().map(|e| e.ref_.clone()).collect()
}

#[test]
fn encoding_keeps_a_slashed_branch_in_one_segment() {
    assert_eq!(encode_segment(Some("feature/foo")), "feature%2Ffoo");
    assert!(!encode_segment(Some("feature/foo/bar")).contains('/'));
    assert_eq!(
        decode_segment(&encode_segment(Some("feature/foo"))).as_deref(),
        Some("feature/foo")
    );
}

#[test]
fn encoding_distinguishes_branches_claudes_scheme_would_collide() {
    assert_ne!(
        encode_segment(Some("feature/foo")),
        encode_segment(Some("feature-foo"))
    );
    assert_eq!(encode_segment(Some("feature-foo")), "feature-foo");
}

#[test]
fn encoding_round_trips_an_absent_value() {
    assert_eq!(decode_segment(&encode_segment(None)), None);
    assert!(!encode_segment(None).contains('/'));
}

#[test]
fn keys_follow_the_dimension_layout() {
    assert_eq!(bundle_key("abc"), "bundles/abc.loch");
    assert_eq!(ref_from_key(&bundle_key("abc")), "abc");
    assert_eq!(
        index_key(BRANCH, Some("feature/foo"), "abc"),
        "index/branch/feature%2Ffoo/abc"
    );
    assert_eq!(value_prefix(BRANCH, Some("main")), "index/branch/main/");
    assert_eq!(dimension_prefix(BRANCH), "index/branch/");
}

#[test]
fn a_bundle_spanning_branches_is_indexed_under_each() {
    let bundle = make_bundle(
        vec![
            session("one", Some("main")),
            session("two", Some("feature/foo")),
            session("three", Some("feature/foo")),
        ],
        CREATED,
    );
    let entries = entries_for(&bundle, "abc", 512);

    let pairs: Vec<(Option<String>, i64)> = entries
        .iter()
        .map(|e| (e.value.clone(), e.sessions))
        .collect();
    assert_eq!(
        pairs,
        vec![
            (Some("feature/foo".to_string()), 2),
            (Some("main".to_string()), 1)
        ]
    );
    assert!(entries.iter().all(|e| e.ref_ == "abc"));
    assert_eq!(entries[0].bytes, 512);
    assert_eq!(entries[0].cwd, "/repo");
    assert_eq!(entries[0].claude_version.as_deref(), Some("1.0.99"));
    assert_eq!(entries[0].agent, "claude-code");
}

#[test]
fn a_branchless_session_still_gets_an_entry() {
    let entries = entries_for(&make_bundle(vec![session("one", None)], CREATED), "abc", 1);
    assert_eq!(entries[0].value, None);
    assert_eq!(index_key(BRANCH, None, "abc").matches('/').count(), 3);
}

#[test]
fn entries_are_readable_without_fetching_the_bundle() {
    let store = file_store("readable");
    let bundle = make_bundle(
        vec![
            session("one", Some("main")),
            session("two", Some("feature/foo")),
        ],
        CREATED,
    );
    let (ref_, size) = put_bundle(&store, &bundle);

    let on_branch = load_entries(&store, &value_prefix(BRANCH, Some("feature/foo"))).unwrap();
    assert_eq!(refs(&on_branch), vec![ref_]);
    assert_eq!(on_branch[0].sessions, 1);
    assert_eq!(on_branch[0].bytes, size as i64);

    assert_eq!(
        load_entries(&store, &dimension_prefix(BRANCH))
            .unwrap()
            .len(),
        2
    );
    assert!(load_entries(&store, &value_prefix(BRANCH, Some("nope")))
        .unwrap()
        .is_empty());
}

#[test]
fn entries_come_back_newest_first() {
    let store = file_store("newest-first");
    for (created_at, session_id) in [
        ("2026-08-01T00:00:00.000Z", "old"),
        ("2026-08-14T00:00:00.000Z", "new"),
    ] {
        let bundle = make_bundle(vec![session(session_id, Some("main"))], created_at);
        let packed = pack_bundle(&bundle).unwrap();
        index_bundle(&store, &bundle, &bundle_ref(&packed), packed.len()).unwrap();
    }

    let created: Vec<String> = load_entries(&store, &dimension_prefix(BRANCH))
        .unwrap()
        .into_iter()
        .map(|e| e.created_at)
        .collect();
    assert_eq!(
        created,
        vec!["2026-08-14T00:00:00.000Z", "2026-08-01T00:00:00.000Z"]
    );
}

#[test]
fn delete_removes_the_bundle_and_every_entry_it_produced() {
    let store = file_store("delete-all");
    let bundle = make_bundle(
        vec![
            session("one", Some("main")),
            session("two", Some("feature/foo")),
        ],
        CREATED,
    );
    let (ref_, _) = put_bundle(&store, &bundle);

    let removed = delete_bundle(&store, &ref_).unwrap();

    assert!(removed.contains(&bundle_key(&ref_)));
    assert!(removed.contains(&index_key(BRANCH, Some("feature/foo"), &ref_)));
    assert!(store.list("").unwrap().is_empty());
}

#[test]
fn delete_leaves_other_bundles_indexed() {
    let store = file_store("delete-others");
    let mut made = Vec::new();
    for session_id in ["one", "two"] {
        let bundle = make_bundle(vec![session(session_id, Some("main"))], CREATED);
        made.push(put_bundle(&store, &bundle).0);
    }

    delete_bundle(&store, &made[0]).unwrap();

    assert_eq!(
        refs(&load_entries(&store, &dimension_prefix(BRANCH)).unwrap()),
        vec![made[1].clone()]
    );
}

#[test]
fn delete_reports_a_missing_ref() {
    let store = file_store("delete-missing");
    assert!(matches!(
        delete_bundle(&store, "nope"),
        Err(IndexError::Store(StoreError::Missing(_)))
    ));
}

#[test]
fn reindex_rebuilds_the_index_from_the_bundles_alone() {
    let store = file_store("reindex-rebuilds");
    let bundle = make_bundle(
        vec![
            session("one", Some("main")),
            session("two", Some("feature/foo")),
        ],
        CREATED,
    );
    put_bundle(&store, &bundle);
    // IndexEntry has no PartialEq; its Debug form names every field.
    let before = format!(
        "{:?}",
        load_entries(&store, &dimension_prefix(BRANCH)).unwrap()
    );

    for key in store.list("index/").unwrap() {
        store.delete(&key).unwrap();
    }
    assert!(load_entries(&store, &dimension_prefix(BRANCH))
        .unwrap()
        .is_empty());

    let result = reindex(&store).unwrap();

    assert_eq!(result.bundles, 1);
    assert_eq!(result.entries, 2);
    assert_eq!(
        format!(
            "{:?}",
            load_entries(&store, &dimension_prefix(BRANCH)).unwrap()
        ),
        before
    );
}

#[test]
fn reindex_drops_entries_no_bundle_backs() {
    let store = file_store("reindex-orphans");
    let bundle = make_bundle(vec![session("one", Some("main"))], CREATED);
    let (ref_, _) = put_bundle(&store, &bundle);
    let orphan = index_key(BRANCH, Some("gone"), "deadbeef");
    store
        .put(
            &orphan,
            b"{\"ref\":\"deadbeef\",\"dimension\":\"branch\",\"value\":\"gone\"}",
        )
        .unwrap();

    let result = reindex(&store).unwrap();

    assert_eq!(result.removed, 1);
    assert!(!store.list("index/").unwrap().contains(&orphan));
    assert_eq!(
        store.list(&value_prefix(BRANCH, Some("main"))).unwrap(),
        vec![index_key(BRANCH, Some("main"), &ref_)]
    );
}

/// The Python ran this against moto in-process. Here it needs a live
/// endpoint (MinIO, `moto_server`, …) holding an existing bucket, named by
/// LOCH_TEST_S3_BUCKET (default `sessions`). Credentials come from
/// AWS_ACCESS_KEY_ID/AWS_SECRET_ACCESS_KEY, defaulting to `testing`. A unique
/// prefix keeps runs apart.
#[test]
#[ignore = "needs an S3 endpoint: set LOCH_TEST_S3_ENDPOINT"]
fn an_encoded_branch_survives_a_round_trip_through_s3() {
    let endpoint = std::env::var("LOCH_TEST_S3_ENDPOINT")
        .expect("set LOCH_TEST_S3_ENDPOINT to an S3-compatible endpoint");
    let bucket = std::env::var("LOCH_TEST_S3_BUCKET").unwrap_or_else(|_| "sessions".into());
    std::env::set_var("LOCH_S3_ENDPOINT", endpoint);
    std::env::remove_var("LOCH_S3_REGION");
    for (name, default) in [
        ("AWS_ACCESS_KEY_ID", "testing"),
        ("AWS_SECRET_ACCESS_KEY", "testing"),
        ("AWS_DEFAULT_REGION", "us-east-1"),
    ] {
        if std::env::var_os(name).is_none() {
            std::env::set_var(name, default);
        }
    }
    let prefix = format!(
        "loch-test-index-{}-{}",
        std::process::id(),
        loch::sys::uuid4()
    );

    let store = S3Store::new(&bucket, &prefix);
    let bundle = make_bundle(vec![session("one", Some("feature/foo"))], CREATED);
    let (ref_, _) = put_bundle(&store, &bundle);

    let entries = load_entries(&store, &value_prefix(BRANCH, Some("feature/foo"))).unwrap();
    let values: Vec<Option<String>> = entries.into_iter().map(|e| e.value).collect();
    assert_eq!(values, vec![Some("feature/foo".to_string())]);

    delete_bundle(&store, &ref_).unwrap();
    assert!(store.list("").unwrap().is_empty());
}
