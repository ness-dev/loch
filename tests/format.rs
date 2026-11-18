//! The loch format's ids and derived files (`format.rs`, `json::canonical`).
//! Ported from test/test_format.py.

mod derived_reader;

use derived_reader::{read_derived, read_packed};
use loch::format::{content_id, digest, make_turn, pack_derived, Derived, Turn};
use loch::json::{canonical, Map, Value};
use loch::object;

fn human() -> Map {
    object! {"kind" => "human", "id" => "alice"}
}

fn header() -> Map {
    object! {"loch" => 1i64, "converter" => object!{"id" => "test", "version" => 1i64}}
}

fn text_blocks(text: &str) -> Vec<Value> {
    vec![Value::Object(object! {"type" => "text", "text" => text})]
}

fn canon(value: &Value) -> Vec<u8> {
    canonical(value).expect("canonical")
}

#[test]
fn canonical_is_independent_of_key_order() {
    let a = Value::Object(object! {
        "b" => 1i64,
        "a" => vec![Value::int(1), Value::Object(object!{"d" => 2i64, "c" => 3i64})],
    });
    let b = Value::Object(object! {
        "a" => vec![Value::int(1), Value::Object(object!{"c" => 3i64, "d" => 2i64})],
        "b" => 1i64,
    });
    assert_eq!(canon(&a), canon(&b));
}

#[test]
fn canonical_is_compact_and_keeps_non_ascii() {
    let value = Value::Object(object! {"a" => "é", "b" => vec![Value::int(1), Value::int(2)]});
    assert_eq!(canon(&value), "{\"a\":\"é\",\"b\":[1,2]}".as_bytes());
}

#[test]
fn canonical_refuses_nan() {
    let value = Value::Object(object! {"x" => Value::Float(f64::NAN)});
    assert!(canonical(&value).is_err());
}

/// The core holds the content's hash, never the content: deleting content
/// must never invalidate an id.
#[test]
fn turn_id_covers_the_content_hash_not_the_content() {
    let turn = make_turn("message", &human(), vec![], text_blocks("hi"), Map::new()).unwrap();

    assert_eq!(
        turn.core["content"].as_str(),
        Some(content_id(&text_blocks("hi")).unwrap().as_str())
    );
    assert!(!turn.core.contains_key("blocks"));
    assert_eq!(turn.id, digest(&Value::Object(turn.core.clone())).unwrap());
}

#[test]
fn facts_do_not_change_identity() {
    let plain = make_turn("message", &human(), vec![], text_blocks("hi"), Map::new()).unwrap();
    let annotated = make_turn(
        "message",
        &human(),
        vec![],
        text_blocks("hi"),
        object! {"timestamp" => "2026"},
    )
    .unwrap();

    assert_eq!(plain.id, annotated.id);
}

#[test]
fn parents_and_author_change_identity() {
    let root = make_turn("message", &human(), vec![], text_blocks("hi"), Map::new()).unwrap();

    let child = make_turn(
        "message",
        &human(),
        vec![root.id.clone()],
        text_blocks("hi"),
        Map::new(),
    )
    .unwrap();
    assert_ne!(child.id, root.id);
    let anonymous = make_turn(
        "message",
        &object! {"kind" => "human"},
        vec![],
        text_blocks("hi"),
        Map::new(),
    )
    .unwrap();
    assert_ne!(anonymous.id, root.id);
}

fn chain() -> Derived {
    let first = make_turn("message", &human(), vec![], text_blocks("a"), Map::new()).unwrap();
    let second = make_turn(
        "message",
        &human(),
        vec![first.id.clone()],
        text_blocks("b"),
        Map::new(),
    )
    .unwrap();
    let heads = object! {
        "current" => second.id.as_str(),
        "leaves" => vec![Value::str(second.id.as_str())],
    };
    Derived {
        header: header(),
        turns: vec![first, second],
        heads,
    }
}

#[test]
fn round_trips_byte_for_byte() {
    let packed = pack_derived(&chain()).unwrap();
    assert_eq!(
        pack_derived(&read_packed(&packed).unwrap()).unwrap(),
        packed
    );
}

#[test]
fn reads_a_tombstoned_turn_and_still_verifies_its_id() {
    let derived = chain();
    let tombstoned = Turn {
        id: derived.turns[0].id.clone(),
        core: derived.turns[0].core.clone(),
        blocks: None,
        facts: Map::new(),
    };
    let packed = pack_derived(&Derived {
        header: derived.header.clone(),
        turns: vec![tombstoned, derived.turns[1].clone()],
        heads: derived.heads.clone(),
    })
    .unwrap();

    let back = read_packed(&packed).unwrap();

    assert!(back.turns[0].blocks.is_none());
    assert_eq!(back.turns[0].id, derived.turns[0].id);
}

fn assert_rejected(packed: &[u8], message: &str) {
    match read_packed(packed) {
        Ok(_) => panic!("expected a FormatError matching {message:?}"),
        Err(error) => assert!(error.0.contains(message), "{:?}", error.0),
    }
}

#[test]
fn rejects_content_that_does_not_match_its_hash() {
    let derived = chain();
    let forged = Turn {
        id: derived.turns[0].id.clone(),
        core: derived.turns[0].core.clone(),
        blocks: Some(text_blocks("forged")),
        facts: Map::new(),
    };
    let packed = pack_derived(&Derived {
        header: derived.header,
        turns: vec![forged],
        heads: Map::new(),
    })
    .unwrap();

    assert_rejected(&packed, "content does not match");
}

#[test]
fn rejects_an_id_that_does_not_match_its_core() {
    let derived = chain();
    let turn = &derived.turns[0];
    let forged = Turn {
        id: digest(&Value::Object(object! {"other" => 1i64})).unwrap(),
        core: turn.core.clone(),
        blocks: turn.blocks.clone(),
        facts: Map::new(),
    };
    let packed = pack_derived(&Derived {
        header: derived.header.clone(),
        turns: vec![forged],
        heads: Map::new(),
    })
    .unwrap();

    assert_rejected(&packed, "does not hash to its id");
}

#[test]
fn rejects_a_turn_before_its_parent() {
    let derived = chain();
    let packed = pack_derived(&Derived {
        header: derived.header,
        turns: derived.turns.into_iter().rev().collect(),
        heads: derived.heads,
    })
    .unwrap();

    assert_rejected(&packed, "precedes its parent");
}

#[test]
fn rejects_an_unknown_format_version() {
    match read_derived(["{\"type\":\"header\",\"loch\":2}"]) {
        Ok(_) => panic!("read a version 2 file"),
        Err(error) => assert!(
            error.0.contains("unsupported loch format 2"),
            "{:?}",
            error.0
        ),
    }
}

/// RFC 8785 prints numbers as ECMAScript does; Python's repr places the
/// exponent differently, so only its digits could be borrowed.
#[test]
fn numbers_print_as_ecmascript_does() {
    for (value, expected) in [
        (1e21, "1e+21"),
        (1e20, "100000000000000000000"),
        (1e16, "10000000000000000"),
        (0.00001, "0.00001"),
        (1e-7, "1e-7"),
        (1.5e-7, "1.5e-7"),
        (100.0, "100"),
        (-0.0, "0"),
        (5e-324, "5e-324"),
        (1.797_693_134_862_315_7e308, "1.7976931348623157e+308"),
        (333_333_333.333_333_3, "333333333.3333333"),
        (-1.5, "-1.5"),
    ] {
        assert_eq!(
            canon(&Value::Float(value)),
            expected.as_bytes(),
            "{value:e}"
        );
    }
}

/// The deliberate departure from RFC 8785: integers print exactly at any
/// size, rather than being rounded through a double.
#[test]
fn integers_stay_exact_beyond_double_precision() {
    assert_eq!(
        canon(&Value::Int("1180591620717411303424".into())),
        b"1180591620717411303424"
    );
}

#[test]
fn keys_sort_by_utf16_code_units() {
    let value = Value::Object(object! {"\u{ffff}" => 1i64, "\u{1f600}" => 2i64, "a" => 3i64});
    assert_eq!(
        canon(&value),
        "{\"a\":3,\"\u{1f600}\":2,\"\u{ffff}\":1}".as_bytes()
    );
}

/// Transcripts hold lone surrogates (a `\ud83d` cut from its pair by
/// truncation); they are escaped rather than refused.
#[test]
fn a_lone_surrogate_is_escaped() {
    let text = format!("x{}", loch::py::surrogate(0xd83d));
    assert_eq!(canon(&Value::Str(text)), b"\"x\\ud83d\"");
}

#[test]
fn control_characters_escape_as_json_stringify_does() {
    assert_eq!(
        canon(&Value::str("a\u{1}\"\\\n")),
        b"\"a\\u0001\\\"\\\\\\n\""
    );
}
