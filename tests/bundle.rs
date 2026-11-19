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
            agent: None,
            agent_version: None,
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
        version: 3,
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
        panic!("a version 3 bundle unpacked");
    };
    assert!(error.contains("unsupported bundle version 3"), "{}", error);
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

// -- format pins ----------------------------------------------------------------
//
// The ref is a hash of the packed bytes, so anything that changes them —
// separators, key order, a null where a key was omitted, a field added to
// every session — changes the identity of every bundle ever made. These pin
// the bytes of today's format; a failure here means existing refs would no
// longer reproduce, which needs a deliberate decision, not a test update.

fn pinned_bundle() -> Bundle {
    Bundle {
        version: 1,
        agent: "claude-code".into(),
        created_at: "2026-08-14T13:27:07.373Z".into(),
        origin: BundleOrigin {
            home: "/home/mike".into(),
            platform: "linux".into(),
            hostname: "box".into(),
        },
        sessions: vec![
            BundleSession {
                session_id: "11111111-2222-4333-8444-555555555555".into(),
                agent: None,
                agent_version: None,
                cwd: "/home/mike/proj".into(),
                git_branch: Some("feature/é".into()),
                claude_version: Some("1.0.99".into()),
                modified_at: "2026-08-14T13:00:00.000Z".into(),
                transcript: concat!(
                    "{\"type\":\"user\",\"cwd\":\"/home/mike/proj\",\"message\":",
                    "{\"content\":\"quote \\\" newline \\n tab \\t snowman ☃\"}}\n"
                )
                .into(),
            },
            BundleSession {
                session_id: "66666666-7777-4888-9999-000000000000".into(),
                agent: None,
                agent_version: None,
                cwd: "/home/mike/proj".into(),
                git_branch: None,
                claude_version: None,
                modified_at: "2026-08-14T12:00:00.000Z".into(),
                transcript: "{\"cwd\":\"/home/mike/proj\"}\n".into(),
            },
        ],
    }
}

#[test]
fn the_packed_json_is_pinned() {
    let packed = pack_bundle(&pinned_bundle()).unwrap();
    let json = String::from_utf8(gunzip(&packed)).unwrap();
    assert_eq!(
        json,
        r##"{"version":1,"agent":"claude-code","createdAt":"2026-08-14T13:27:07.373Z","origin":{"home":"/home/mike","platform":"linux","hostname":"box"},"sessions":[{"sessionId":"11111111-2222-4333-8444-555555555555","cwd":"/home/mike/proj","gitBranch":"feature/é","claudeVersion":"1.0.99","modifiedAt":"2026-08-14T13:00:00.000Z","transcript":"{\"type\":\"user\",\"cwd\":\"/home/mike/proj\",\"message\":{\"content\":\"quote \\\" newline \\n tab \\t snowman ☃\"}}\n"},{"sessionId":"66666666-7777-4888-9999-000000000000","cwd":"/home/mike/proj","modifiedAt":"2026-08-14T12:00:00.000Z","transcript":"{\"cwd\":\"/home/mike/proj\"}\n"}]}"##
    );
}

#[test]
fn the_ref_is_pinned() {
    let packed = pack_bundle(&pinned_bundle()).unwrap();
    assert_eq!(
        bundle_ref(&packed),
        "3069348d032085727a057739310cef4cb14ab8dba1fe838f865e46bf90235dad"
    );
}

#[test]
fn repacking_an_unpacked_bundle_reproduces_its_bytes() {
    let packed = pack_bundle(&pinned_bundle()).unwrap();
    assert_eq!(
        pack_bundle(&unpack_bundle(&packed).unwrap()).unwrap(),
        packed
    );
}

/// A bundle holding a Codex session: no ref predates it, so it carries the
/// version a build that ignores `agent` refuses, rather than being restored
/// there as a Claude transcript with `ok: true`.
fn codex_bundle() -> Bundle {
    let mut bundle = pinned_bundle();
    bundle.sessions[1].agent = Some("codex".into());
    bundle.sessions[1].agent_version = Some("0.166.0".into());
    bundle.version = bundle.required_version();
    bundle
}

#[test]
fn claude_sessions_alone_stay_version_1() {
    assert_eq!(pinned_bundle().required_version(), 1);
}

#[test]
fn any_other_agents_session_makes_a_version_2_bundle() {
    let bundle = codex_bundle();
    assert_eq!(bundle.version, 2);
    let packed = pack_bundle(&bundle).unwrap();
    let json = String::from_utf8(gunzip(&packed)).unwrap();
    // What v0.2.0 checks, and refuses as "unsupported bundle version 2".
    assert!(json.starts_with("{\"version\":2,"), "{}", json);
    assert!(json.contains(
        "\"sessionId\":\"66666666-7777-4888-9999-000000000000\",\"agent\":\"codex\",\"cwd\""
    ));
    assert!(json.contains("\"agentVersion\":\"0.166.0\""));
    assert_eq!(
        bundle_ref(&packed),
        "08f5c37e73bf5b24c31ee64aaadbbe831b076875ec8de781edf10461462a7654"
    );
    let back = unpack_bundle(&packed).unwrap();
    assert_eq!(back.version, 2);
    assert_eq!(back.sessions[1].agent.as_deref(), Some("codex"));
    assert_eq!(pack_bundle(&back).unwrap(), packed);
}

#[test]
fn a_bundle_whose_agent_is_not_claude_is_version_2() {
    let mut bundle = pinned_bundle();
    bundle.agent = "codex".into();
    assert_eq!(bundle.required_version(), 2);
}
