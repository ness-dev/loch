//! A verifying reader for derived files, the test-side counterpart of
//! `format.py`'s `read_derived`/`verify_turn`. The CLI never reads a derived
//! file back, so the crate has no reader; the tests that need one share this.
//! Every id is recomputed, so a file that reads back without an error is
//! internally consistent, not merely well-formed.

#![allow(dead_code)]

use loch::format::{content_id, digest, Derived, Turn, FORMAT_VERSION};
use loch::json::{self, Map, Value};
use std::collections::HashSet;

#[derive(Debug)]
pub struct FormatError(pub String);

pub fn verify_turn(turn: &Turn) -> Result<(), FormatError> {
    let core = Value::Object(turn.core.clone());
    if digest(&core).ok().as_deref() != Some(turn.id.as_str()) {
        return Err(FormatError(format!(
            "turn {} does not hash to its id",
            turn.id
        )));
    }
    if let Some(blocks) = &turn.blocks {
        let expected = turn.core.get("content").and_then(Value::as_str);
        if content_id(blocks).ok().as_deref() != expected {
            return Err(FormatError(format!(
                "turn {} content does not match its hash",
                turn.id
            )));
        }
    }
    Ok(())
}

pub fn read_derived<'a>(lines: impl IntoIterator<Item = &'a str>) -> Result<Derived, FormatError> {
    let mut header: Option<Map> = None;
    let mut heads = Map::new();
    let mut turns = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    for (index, line) in lines.into_iter().enumerate() {
        let number = index + 1;
        if loch::py::strip(line).is_empty() {
            continue;
        }
        let mut record = match json::loads(line) {
            Ok(Value::Object(map)) => map,
            _ => return Err(FormatError(format!("line {}: not a JSON object", number))),
        };
        let kind = record.shift_remove("type");
        match kind.as_ref().and_then(Value::as_str) {
            Some("header") => {
                let version = record.get("loch");
                if version.and_then(Value::as_i64) != Some(FORMAT_VERSION) {
                    let shown = version
                        .map(|v| json::dumps(v, json::DEFAULT))
                        .unwrap_or_else(|| "None".into());
                    return Err(FormatError(format!(
                        "unsupported loch format {} (this build reads {})",
                        shown, FORMAT_VERSION
                    )));
                }
                header = Some(record);
            }
            Some("turn") => {
                let field = |name: &str| {
                    record
                        .get(name)
                        .cloned()
                        .ok_or_else(|| FormatError(format!("line {}: no {}", number, name)))
                };
                let id = field("id")?
                    .as_str()
                    .map(str::to_string)
                    .ok_or_else(|| FormatError(format!("line {}: id is not a string", number)))?;
                let core = match field("turn")? {
                    Value::Object(map) => map,
                    _ => {
                        return Err(FormatError(format!(
                            "line {}: turn is not an object",
                            number
                        )))
                    }
                };
                let blocks = match record.get("content") {
                    None | Some(Value::Null) => None,
                    Some(content) => Some(
                        content
                            .get("blocks")
                            .and_then(Value::as_array)
                            .cloned()
                            .ok_or_else(|| FormatError(format!("line {}: no blocks", number)))?,
                    ),
                };
                let facts = match record.get("facts") {
                    Some(Value::Object(map)) => map.clone(),
                    _ => Map::new(),
                };
                let turn = Turn {
                    id,
                    core,
                    blocks,
                    facts,
                };
                verify_turn(&turn)?;
                for parent in turn.parents() {
                    if !seen.contains(&parent) {
                        return Err(FormatError(format!(
                            "turn {} precedes its parent {}",
                            turn.id, parent
                        )));
                    }
                }
                seen.insert(turn.id.clone());
                turns.push(turn);
            }
            Some("heads") => heads = record,
            _ => {
                let shown = kind
                    .map(|v| json::dumps(&v, json::DEFAULT))
                    .unwrap_or_else(|| "None".into());
                return Err(FormatError(format!(
                    "line {}: unknown record type {}",
                    number, shown
                )));
            }
        }
    }

    let header = header.ok_or_else(|| FormatError("no header".into()))?;
    Ok(Derived {
        header,
        turns,
        heads,
    })
}

/// `read_derived(packed.decode().split("\n"))`.
pub fn read_packed(packed: &[u8]) -> Result<Derived, FormatError> {
    let text = std::str::from_utf8(packed).map_err(|_| FormatError("not UTF-8".into()))?;
    read_derived(text.split('\n'))
}
