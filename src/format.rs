//! The loch format's ids and derived files. Every id
//! is a SHA-256 over `json::canonical` (RFC 8785, integers exact).

use crate::json::{self, Map, Value};
use crate::object;
use sha2::{Digest, Sha256};

pub const FORMAT_VERSION: i64 = 1;
pub const HASH_PREFIX: &str = "sha256:";

#[derive(Debug)]
pub struct NotJson;

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

pub fn digest(value: &Value) -> Result<String, NotJson> {
    let bytes = json::canonical(value).map_err(|_| NotJson)?;
    Ok(format!("{}{}", HASH_PREFIX, hex(&Sha256::digest(bytes))))
}

pub fn digest_bytes(data: &[u8]) -> String {
    format!("{}{}", HASH_PREFIX, hex(&Sha256::digest(data)))
}

/// One node of the conversation graph. `core` is the hashed envelope, which
/// holds the content's hash and never the content; `facts` are what the
/// converter knew that is not part of the turn's identity.
#[derive(Clone, Debug)]
pub struct Turn {
    pub id: String,
    pub core: Map,
    pub blocks: Option<Vec<Value>>,
    pub facts: Map,
}

pub fn content_id(blocks: &[Value]) -> Result<String, NotJson> {
    digest(&Value::Object(object! {"blocks" => blocks.to_vec()}))
}

pub fn make_turn(
    kind: &str,
    author: &Map,
    parents: Vec<String>,
    blocks: Vec<Value>,
    facts: Map,
) -> Result<Turn, NotJson> {
    let core = object! {
        "loch" => FORMAT_VERSION,
        "kind" => kind,
        "author" => author.clone(),
        "parents" => parents.into_iter().map(Value::Str).collect::<Vec<_>>(),
        "content" => content_id(&blocks)?,
    };
    Ok(Turn {
        id: digest(&Value::Object(core.clone()))?,
        core,
        blocks: Some(blocks),
        facts,
    })
}

impl Turn {
    pub fn parents(&self) -> Vec<String> {
        self.core
            .get("parents")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn kind(&self) -> &str {
        self.core.get("kind").and_then(Value::as_str).unwrap_or("")
    }

    pub fn author_kind(&self) -> Option<&str> {
        self.core.get("author")?.get("kind")?.as_str()
    }
}

pub struct Derived {
    pub header: Map,
    pub turns: Vec<Turn>,
    pub heads: Map,
}

pub fn pack_derived(derived: &Derived) -> Result<Vec<u8>, NotJson> {
    let mut out = Vec::new();
    let mut line = |value: Value| -> Result<(), NotJson> {
        out.extend(json::canonical(&value).map_err(|_| NotJson)?);
        out.push(b'\n');
        Ok(())
    };
    let mut header = object! {"type" => "header"};
    header.extend(derived.header.clone());
    line(Value::Object(header))?;
    for turn in &derived.turns {
        let mut record = object! {
            "type" => "turn",
            "id" => turn.id.as_str(),
            "turn" => turn.core.clone(),
        };
        if let Some(blocks) = &turn.blocks {
            record.insert(
                "content".into(),
                Value::Object(object! {"blocks" => blocks.clone()}),
            );
        }
        if !turn.facts.is_empty() {
            record.insert("facts".into(), Value::Object(turn.facts.clone()));
        }
        line(Value::Object(record))?;
    }
    let mut heads = object! {"type" => "heads"};
    heads.extend(derived.heads.clone());
    line(Value::Object(heads))?;
    Ok(out)
}
