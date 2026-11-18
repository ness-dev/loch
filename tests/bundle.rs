//! Ported from test/test_bundle.py.

use loch::bundle::{
    bundle_ref, gzip_deterministic, pack_bundle, unpack_bundle, Bundle, BundleOrigin, BundleSession,
};
use sha2::{Digest, Sha256};
use std::io::Read;

fn sample_bundle(git_branch: Option<&str>) -> Bundle {
    Bundle {
        version: 1,
        agent: "claude-code".into(),
        created_at: "2026-08-14T13:27:07.373Z".into(),
        origin: BundleOrigin {
            home: "/home/mike".into(),
            platform: "linux".into(),
            hostname: "box".into(),
        },
        sessions: vec![BundleSession {
            session_id: "abc".into(),
            cwd: "/home/mike/proj".into(),
            git_branch: git_branch.map(str::to_string),
            claude_version: Some("1.0.99".into()),
            modified_at: "2026-08-14T13:00:00.000Z".into(),
            transcript: "{\"cwd\":\"/home/mike/proj\"}\n".into(),
        }],
    }
}

/// Bundle has no PartialEq, so equality is spelled out field by field.
fn assert_same(a: &Bundle, b: &Bundle) {
    assert_eq!(a.version, b.version);
    assert_eq!(a.agent, b.agent);
    assert_eq!(a.created_at, b.created_at);
    assert_eq!(a.origin.home, b.origin.home);
    assert_eq!(a.origin.platform, b.origin.platform);
    assert_eq!(a.origin.hostname, b.origin.hostname);
    assert_eq!(a.sessions.len(), b.sessions.len());
    for (x, y) in a.sessions.iter().zip(&b.sessions) {
        assert_eq!(x.session_id, y.session_id);
        assert_eq!(x.cwd, y.cwd);
        assert_eq!(x.git_branch, y.git_branch);
        assert_eq!(x.claude_version, y.claude_version);
        assert_eq!(x.modified_at, y.modified_at);
        assert_eq!(x.transcript, y.transcript);
    }
}

fn gunzip(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(data)
        .read_to_end(&mut out)
        .unwrap();
    out
}

fn sha256_hex(data: &[u8]) -> String {
    Sha256::digest(data)
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect()
}

#[test]
fn pack_round_trips() {
    let bundle = sample_bundle(Some("main"));
    assert_same(
        &unpack_bundle(&pack_bundle(&bundle).unwrap()).unwrap(),
        &bundle,
    );
}

#[test]
fn pack_is_deterministic() {
    assert_eq!(
        pack_bundle(&sample_bundle(Some("main"))).unwrap(),
        pack_bundle(&sample_bundle(Some("main"))).unwrap()
    );
}

#[test]
fn ref_is_the_sha256_of_the_packed_bytes() {
    let packed = pack_bundle(&sample_bundle(Some("main"))).unwrap();
    assert_eq!(bundle_ref(&packed), sha256_hex(&packed));
}

#[test]
fn absent_metadata_stays_absent_rather_than_null() {
    let packed = pack_bundle(&sample_bundle(None)).unwrap();
    let raw = loch::json::loads(std::str::from_utf8(&gunzip(&packed)).unwrap()).unwrap();
    let session = &raw.get("sessions").unwrap().as_array().unwrap()[0];
    assert!(session.as_object().unwrap().get("gitBranch").is_none());
    assert!(unpack_bundle(&packed).unwrap().sessions[0]
        .git_branch
        .is_none());
}

#[test]
fn rejects_an_unsupported_version() {
    let packed = pack_bundle(&Bundle {
        version: 2,
        agent: "claude-code".into(),
        created_at: String::new(),
        origin: BundleOrigin {
            home: String::new(),
            platform: String::new(),
            hostname: String::new(),
        },
        sessions: vec![],
    })
    .unwrap();
    let Err(error) = unpack_bundle(&packed) else {
        panic!("a version 2 bundle unpacked");
    };
    assert!(error.contains("unsupported bundle version 2"), "{}", error);
}

#[test]
fn packing_matches_python_310_on_every_platform() {
    let mut data = b"{\"a\":\"".to_vec();
    data.extend(b"xyz".repeat(5000));
    data.extend(b"\"}");
    let packed = gzip_deterministic(&data);
    // What 3.10's gzip.compress(data, 9, mtime=0) produced, OS byte 255
    // included; 3.12 on macOS writes 19 there and Linux 3.
    let header: String = packed[..10].iter().map(|b| format!("{:02x}", b)).collect();
    assert_eq!(header, "1f8b08000000000002ff");
    assert_eq!(&sha256_hex(&packed)[..16], "405552bff938ef58");
    assert_eq!(gunzip(&packed), data);
}
