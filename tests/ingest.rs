//! The converter: Claude Code JSONL -> loch turns (`ingest.rs`).
//! Ported from test/test_ingest.py.

mod derived_reader;

use loch::format::{pack_derived, Turn};
use loch::ingest::{
    convert, ingest, ssh_author_id, Converted, Origin, ESCAPE, HOME, ROOT, ROOT_SLUG, SESSION,
};
use loch::json::{self, Map, Value};
use loch::object;
use loch::rewrite::{rewrite_transcript, RewriteSpec};
use sha2::{Digest, Sha256};
use std::path::PathBuf;

const HOME_DIR: &str = "/Users/origin";
const CWD: &str = "/Users/origin/code/repo";
const SESSION_ID: &str = "11111111-1111-1111-1111-111111111111";

fn origin() -> Origin {
    Origin {
        cwd: CWD.into(),
        home: HOME_DIR.into(),
        session_id: SESSION_ID.into(),
    }
}

fn human() -> Map {
    object! {"kind" => "human"}
}

fn run(raw: &str) -> Converted {
    convert(raw, &origin(), &human()).expect("converts")
}

fn ids(turns: &[Turn]) -> Vec<String> {
    turns.iter().map(|t| t.id.clone()).collect()
}

fn kinds(turns: &[Turn]) -> Vec<String> {
    turns.iter().map(|t| t.kind().to_string()).collect()
}

fn strs(items: &[&str]) -> Value {
    Value::Array(items.iter().map(|s| Value::str(*s)).collect())
}

fn record(uuid: &str, parent: Option<&str>, kind: &str, fields: Map) -> Map {
    let mut base = object! {
        "type" => kind,
        "uuid" => uuid,
        "parentUuid" => Value::opt_str(parent),
        "sessionId" => SESSION_ID,
        "cwd" => CWD,
        "gitBranch" => "main",
        "version" => "2.0.0",
        "timestamp" => format!("2026-01-01T00:00:{}Z", &uuid[uuid.len() - 2..]),
    };
    base.extend(fields);
    base
}

fn user_with(uuid: &str, parent: Option<&str>, content: Value, fields: Map) -> Map {
    let mut all = object! {"message" => object!{"role" => "user", "content" => content}};
    all.extend(fields);
    record(uuid, parent, "user", all)
}

fn user(uuid: &str, parent: Option<&str>, content: impl Into<Value>) -> Map {
    user_with(uuid, parent, content.into(), Map::new())
}

fn assistant(uuid: &str, parent: Option<&str>, blocks: Vec<Value>, message_id: &str) -> Map {
    let message =
        object! {"id" => message_id, "role" => "assistant", "model" => "m", "content" => blocks};
    record(uuid, parent, "assistant", object! {"message" => message})
}

fn text(text: &str) -> Value {
    Value::Object(object! {"type" => "text", "text" => text})
}

fn jsonl(records: &[Map]) -> String {
    records
        .iter()
        .map(|r| json::dumps(&Value::Object(r.clone()), json::DEFAULT) + "\n")
        .collect()
}

/// A prompt, an assistant message split across two records (thinking, then a
/// tool call), the tool result, and a reply — the shape Claude Code writes
/// for one tool-using exchange.
fn session() -> String {
    jsonl(&[
        object! {"type" => "custom-title", "customTitle" => "demo", "sessionId" => SESSION_ID},
        user("u-01", None, format!("look at {CWD}/src/main.py")),
        assistant(
            "a-02",
            Some("u-01"),
            vec![Value::Object(
                object! {"type" => "thinking", "thinking" => "hmm"},
            )],
            "msg_1",
        ),
        assistant(
            "a-03",
            Some("a-02"),
            vec![Value::Object(object! {
                "type" => "tool_use",
                "id" => "toolu_1",
                "name" => "Read",
                "input" => object!{"file_path" => format!("{CWD}/src/main.py")},
            })],
            "msg_1",
        ),
        user_with(
            "u-04",
            Some("a-03"),
            Value::Array(vec![Value::Object(object! {
                "type" => "tool_result", "tool_use_id" => "toolu_1", "content" => "print()",
            })]),
            object! {"toolUseResult" => object!{"file" => object!{"filePath" => format!("{CWD}/src/main.py")}}},
        ),
        assistant("a-05", Some("u-04"), vec![text("done")], "msg_2"),
        object! {"type" => "last-prompt", "leafUuid" => "a-05", "sessionId" => SESSION_ID},
    ])
}

#[test]
fn one_turn_per_chain_record_linked_by_parent() {
    let Converted { turns, heads, .. } = run(&session());

    assert_eq!(
        kinds(&turns),
        ["message", "message", "message", "tool-result", "message"]
    );
    for pair in turns.windows(2) {
        assert_eq!(pair[1].parents(), [pair[0].id.clone()]);
    }
    assert!(turns[0].parents().is_empty());
    let last = turns.last().unwrap().id.as_str();
    assert_eq!(
        heads,
        object! {"current" => last, "leaves" => strs(&[last])}
    );
}

#[test]
fn authors_follow_who_produced_the_record() {
    let alice = object! {"kind" => "human", "id" => "alice"};
    let turns = convert(&session(), &origin(), &alice).unwrap().turns;

    let authors: Vec<_> = turns.iter().map(|t| t.author_kind().unwrap()).collect();
    assert_eq!(authors, ["human", "agent", "agent", "tool", "agent"]);
    assert_eq!(turns[0].core["author"], Value::Object(alice));
}

#[test]
fn split_assistant_message_is_regrouped_by_message_id_fact() {
    let turns = run(&session()).turns;

    let message_ids: Vec<_> = turns
        .iter()
        .map(|t| t.facts.get("messageId").and_then(Value::as_str))
        .collect();
    assert_eq!(
        message_ids,
        [None, Some("msg_1"), Some("msg_1"), None, Some("msg_2")]
    );
}

#[test]
fn blocks_are_normalized_to_loch_shapes() {
    let turns = run(&session()).turns;

    assert_eq!(
        turns[1].blocks,
        Some(vec![Value::Object(
            object! {"type" => "thinking", "text" => "hmm"}
        )])
    );
    assert_eq!(
        turns[2].blocks,
        Some(vec![Value::Object(object! {
            "type" => "tool-call",
            "call" => "toolu_1",
            "name" => "Read",
            "input" => object!{"file_path" => format!("{ROOT}/src/main.py")},
        })])
    );
    assert_eq!(
        turns[3].blocks,
        Some(vec![Value::Object(object! {
            "type" => "tool-result",
            "call" => "toolu_1",
            "content" => vec![text("print()")],
        })])
    );
}

#[test]
fn machine_specific_strings_become_placeholders() {
    let raw = jsonl(&[user(
        "u-01",
        None,
        format!(
            "{CWD}/a {HOME_DIR}/.zshrc \
             {HOME_DIR}/.claude/projects/-Users-origin-code-repo/{SESSION_ID}.jsonl \
             literal {{{{loch:root}}}}"
        ),
    )]);
    let turns = run(&raw).turns;

    assert_eq!(
        turns[0].blocks,
        Some(vec![text(&format!(
            "{ROOT}/a {HOME}/.zshrc \
             {HOME}/.claude/projects/{ROOT_SLUG}/{SESSION}.jsonl \
             literal {ESCAPE}root}}}}"
        ))])
    );
    assert_eq!(turns[0].facts["cwd"].as_str(), Some(ROOT));
}

/// The boundary guard: `<repo>-worktrees/...` extends the root's last
/// component and must not be taken for the root.
#[test]
fn sibling_of_the_root_is_not_taken_for_it() {
    let raw = jsonl(&[user("u-01", None, format!("{CWD}-worktrees/feature/x"))]);
    let turns = run(&raw).turns;

    assert_eq!(
        turns[0].blocks,
        Some(vec![text(&format!("{HOME}/code/repo-worktrees/feature/x"))])
    );
}

/// The shared normalizer is what makes this hold: a copy rewritten by one
/// set of pairs normalizes back through the same set.
#[test]
fn turn_ids_survive_a_restore_to_another_machine() {
    let target = Origin {
        cwd: "/home/bob/src/repo".into(),
        home: "/home/bob".into(),
        session_id: format!("{}-2222-2222-2222-{}", "2".repeat(8), "2".repeat(12)),
    };
    let spec = RewriteSpec {
        origin_cwd: CWD.into(),
        origin_home: HOME_DIR.into(),
        target_cwd: target.cwd.clone(),
        target_home: target.home.clone(),
        origin_session_id: Some(SESSION_ID.into()),
        target_session_id: Some(target.session_id.clone()),
    };
    let original = run(&session());
    let copied = convert(&rewrite_transcript(&session(), &spec), &target, &human()).unwrap();

    assert_eq!(ids(&copied.turns), ids(&original.turns));
    assert_eq!(copied.heads, original.heads);
}

#[test]
fn timestamps_and_record_uuids_are_facts_not_identity() {
    let renamed = session()
        .replace("u-01", "x-99")
        .replace("00:01Z", "09:09Z");
    let original = run(&session()).turns;
    let changed = run(&renamed).turns;

    assert_eq!(ids(&changed), ids(&original));
    assert_eq!(changed[0].facts["uuid"].as_str(), Some("x-99"));
}

#[test]
fn a_rewind_leaves_two_heads() {
    let raw = jsonl(&[
        user("u-01", None, "start"),
        assistant("a-02", Some("u-01"), vec![text("first try")], "m1"),
        assistant("a-03", Some("u-01"), vec![text("second try")], "m2"),
        object! {"type" => "last-prompt", "leafUuid" => "a-02", "sessionId" => SESSION_ID},
    ]);
    let Converted { turns, heads, .. } = run(&raw);

    assert_eq!(
        heads["leaves"],
        strs(&[turns[1].id.as_str(), turns[2].id.as_str()])
    );
    assert_eq!(heads["current"].as_str(), Some(turns[1].id.as_str()));
}

#[test]
fn identical_resubmission_from_the_same_point_is_one_turn() {
    let raw = jsonl(&[
        user("u-01", None, "start"),
        user("u-02", Some("u-01"), "again"),
        user("u-03", Some("u-01"), "again"),
    ]);
    let turns = run(&raw).turns;

    assert_eq!(turns.len(), 2);
    assert_eq!(turns[1].facts["alsoUuids"], strs(&["u-03"]));
}

#[test]
fn compaction_boundary_follows_its_logical_parent() {
    let raw = jsonl(&[
        user("u-01", None, "long conversation"),
        record(
            "s-02",
            None,
            "system",
            object! {
                "subtype" => "compact_boundary",
                "logicalParentUuid" => "u-01",
                "content" => "Conversation compacted",
            },
        ),
        user_with(
            "u-03",
            Some("s-02"),
            "summary of before".into(),
            object! {"isCompactSummary" => true},
        ),
    ]);
    let Converted { turns, heads, .. } = run(&raw);

    assert_eq!(kinds(&turns), ["message", "compaction", "context"]);
    assert_eq!(turns[1].parents(), [turns[0].id.clone()]);
    assert_eq!(heads["leaves"], strs(&[turns[2].id.as_str()]));
}

#[test]
fn dangling_parent_becomes_a_root_and_says_so() {
    let raw = jsonl(&[user("u-02", Some("u-gone"), "orphan")]);
    let turns = run(&raw).turns;

    assert!(turns[0].parents().is_empty());
    assert_eq!(turns[0].facts["danglingParent"].as_str(), Some("u-gone"));
}

#[test]
fn a_child_written_before_its_parent_still_links() {
    let raw = jsonl(&[
        user("u-02", Some("u-01"), "child"),
        user("u-01", None, "parent"),
    ]);
    let turns = run(&raw).turns;

    assert_eq!(turns[0].facts["uuid"].as_str(), Some("u-01"));
    assert_eq!(turns[1].parents(), [turns[0].id.clone()]);
}

#[test]
fn unknown_blocks_and_records_pass_through_or_stay_out() {
    let raw = jsonl(&[
        object! {"type" => "cost-state", "totalCostUSD" => Value::Float(1.0), "sessionId" => SESSION_ID},
        assistant(
            "a-01",
            None,
            vec![Value::Object(
                object! {"type" => "future_block", "x" => 1i64},
            )],
            "msg_1",
        ),
    ]);
    let turns = run(&raw).turns;

    assert_eq!(turns.len(), 1);
    assert_eq!(
        turns[0].blocks,
        Some(vec![Value::Object(object! {
            "type" => "x-claude-code:block",
            "block" => object!{"type" => "future_block", "x" => 1i64},
        })])
    );
}

#[test]
fn attachment_payload_excludes_the_record_envelope() {
    let attachment = object! {"type" => "todo", "items" => Vec::<Value>::new()};
    let raw = jsonl(&[record(
        "t-01",
        None,
        "attachment",
        object! {"attachment" => attachment.clone()},
    )]);
    let turns = run(&raw).turns;

    assert_eq!(turns[0].kind(), "context");
    assert_eq!(
        turns[0].blocks,
        Some(vec![Value::Object(object! {
            "type" => "x-claude-code:attachment",
            "attachment" => object!{"attachment" => attachment},
        })])
    );
}

/// A fresh scratch directory, removed when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Scratch {
        let dir =
            std::env::temp_dir().join(format!("loch-test-ingest-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }

    fn write(&self, raw: &str) -> String {
        let path = self.0.join(format!("{SESSION_ID}.jsonl"));
        std::fs::write(&path, raw).unwrap();
        path.to_str().unwrap().to_string()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn source_manifest_hashes_the_transcript_and_its_sidecar() {
    let scratch = Scratch::new("manifest");
    let path = scratch.write(&session());
    let sidecar = scratch.0.join(SESSION_ID).join("subagents");
    std::fs::create_dir_all(&sidecar).unwrap();
    std::fs::write(sidecar.join("agent-a.jsonl"), "{}\n").unwrap();

    let derived = ingest(&path, &origin(), None).unwrap();

    let source = &derived.header["source"];
    let files: Vec<_> = source
        .get("files")
        .and_then(Value::as_array)
        .unwrap()
        .iter()
        .map(|f| f.get("path").and_then(Value::as_str).unwrap())
        .collect();
    assert_eq!(
        files,
        ["sidecar/subagents/agent-a.jsonl", "transcript.jsonl"]
    );
    assert!(source
        .get("id")
        .and_then(Value::as_str)
        .unwrap()
        .starts_with("sha256:"));
    assert_eq!(
        derived.header["session"],
        Value::Object(object! {"title" => "demo"})
    );
}

#[test]
fn ingest_is_idempotent_and_reads_back_verified() {
    let scratch = Scratch::new("idempotent");
    let path = scratch.write(&session());

    let first = pack_derived(&ingest(&path, &origin(), Some("alice")).unwrap()).unwrap();
    let second = pack_derived(&ingest(&path, &origin(), Some("alice")).unwrap()).unwrap();

    assert_eq!(first, second);
    let back = derived_reader::read_packed(&first).unwrap();
    assert_eq!(pack_derived(&back).unwrap(), first);
}

#[test]
fn appending_to_a_session_keeps_every_earlier_turn_id() {
    let before = run(&session()).turns;
    let grown = session() + &jsonl(&[user("u-06", Some("a-05"), "and then?")]);
    let after = run(&grown).turns;

    assert_eq!(ids(&after[..before.len()]), ids(&before));
}

#[test]
fn events_fold_into_the_turn_they_follow() {
    let raw = jsonl(&[
        user("u-01", None, "start"),
        record(
            "s-02",
            Some("u-01"),
            "system",
            object! {"subtype" => "stop_hook_summary", "hookCount" => 1i64},
        ),
        assistant("a-03", Some("s-02"), vec![text("reply")], "msg_1"),
        object! {"type" => "last-prompt", "leafUuid" => "s-02", "sessionId" => SESSION_ID},
    ]);
    let Converted { turns, heads, .. } = run(&raw);

    assert_eq!(kinds(&turns), ["message", "message"]);
    assert_eq!(turns[1].parents(), [turns[0].id.clone()]);
    let events = turns[0].facts["events"].as_array().unwrap();
    assert_eq!(events.len(), 1);
    let event = &events[0];
    assert_eq!(
        event.get("type").and_then(Value::as_str),
        Some("x-claude-code:system")
    );
    assert_eq!(
        event.get("system"),
        Some(&Value::Object(
            object! {"subtype" => "stop_hook_summary", "hookCount" => 1i64}
        ))
    );
    assert_eq!(heads["current"].as_str(), Some(turns[0].id.as_str()));
}

#[test]
fn events_do_not_change_the_ids_around_them() {
    let plain = jsonl(&[
        user("u-01", None, "start"),
        assistant("a-03", Some("u-01"), vec![text("reply")], "msg_1"),
    ]);
    let with_event = jsonl(&[
        user("u-01", None, "start"),
        record(
            "s-02",
            Some("u-01"),
            "system",
            object! {"subtype" => "turn_duration", "durationMs" => 5i64},
        ),
        assistant("a-03", Some("s-02"), vec![text("reply")], "msg_1"),
    ]);

    assert_eq!(ids(&run(&with_event).turns), ids(&run(&plain).turns));
}

#[test]
fn an_event_with_no_turn_before_it_is_kept_at_session_level() {
    let raw = jsonl(&[
        record(
            "s-01",
            None,
            "system",
            object! {"subtype" => "informational", "content" => "hi"},
        ),
        user("u-02", Some("s-01"), "start"),
    ]);
    let Converted { turns, session, .. } = run(&raw);

    assert_eq!(turns.len(), 1);
    let subtype = session["events"].as_array().unwrap()[0]
        .get("system")
        .and_then(|s| s.get("subtype"))
        .and_then(Value::as_str);
    assert_eq!(subtype, Some("informational"));
}

fn b64encode(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |acc, (i, &b)| acc | (b as u32) << (16 - 8 * i));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// ssh-keygen -l hashes the decoded key blob with SHA-256 and prints it as
/// unpadded base64; checked against ssh-keygen by hand on a real key.
#[test]
fn ssh_author_id_is_the_ssh_keygen_fingerprint_of_the_key_blob() {
    let mut blob = b"\x00\x00\x00\x0bssh-ed25519".to_vec();
    blob.extend(0u8..32);
    let line = format!("ssh-ed25519 {} me@host", b64encode(&blob));
    let expected = b64encode(&Sha256::digest(&blob));
    let expected = expected.trim_end_matches('=');

    assert_eq!(
        ssh_author_id(&line).unwrap(),
        format!("ssh:SHA256:{expected}")
    );
    let without_comment = line.rsplit_once(' ').unwrap().0;
    assert_eq!(
        ssh_author_id(without_comment).unwrap(),
        ssh_author_id(&line).unwrap()
    );
}

#[test]
fn ssh_author_id_rejects_something_that_is_not_a_key() {
    assert!(ssh_author_id("not-a-key").is_err());
}
