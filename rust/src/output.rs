//! The result envelope and its two renderings, as in `loch/output.py`.
//!
//! JSON is written the way Python's `json.dumps` writes it by default — `", "`
//! and `": "` separators, every non-ASCII character escaped, floats in `repr`
//! form — so the equivalence harness can compare the two implementations'
//! stdout byte for byte rather than only after parsing.

use serde_json::{Map, Value};
use std::io::Write;

pub const SCHEMA: i64 = 1;

pub const JSON_FLAG: &str = "--json";

/// What a command produced, in both renderings. Building the payload and the
/// prose in one place is what keeps them from drifting apart.
pub struct Outcome {
    pub command: String,
    pub payload: Map<String, Value>,
    pub text: String,
    pub warnings: Vec<String>,
}

impl Outcome {
    pub fn new(command: &str) -> Self {
        Outcome {
            command: command.to_string(),
            payload: Map::new(),
            text: String::new(),
            warnings: Vec::new(),
        }
    }
}

/// A failure the caller is meant to act on. `code` is a stable token —
/// consumers branch on it, so reword `message` freely but not `code`.
pub struct CommandError {
    pub code: &'static str,
    pub message: String,
    pub payload: Map<String, Value>,
    pub exit_status: i32,
    pub prose: Option<String>,
}

impl CommandError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        CommandError {
            code,
            message: message.into(),
            payload: Map::new(),
            exit_status: 1,
            prose: None,
        }
    }
}

pub fn document(
    command: &str,
    ok: bool,
    payload: Map<String, Value>,
    failure: Option<(&str, &str)>,
) -> Value {
    let mut body = Map::new();
    body.insert("schema".into(), Value::from(SCHEMA));
    body.insert("ok".into(), Value::from(ok));
    body.insert("command".into(), Value::from(command));
    if let Some((code, error)) = failure {
        body.insert("code".into(), Value::from(code));
        body.insert("error".into(), Value::from(error));
    }
    body.extend(payload);
    Value::Object(body)
}

pub fn emit(outcome: Outcome, as_json: bool) {
    if as_json {
        write_document(&document(&outcome.command, true, outcome.payload, None));
        return;
    }
    // Warnings are stderr-only prose; JSON carries the same facts as per-item
    // status, so nothing but the document is written in that mode.
    let mut stderr = std::io::stderr();
    for warning in &outcome.warnings {
        let _ = stderr.write_all(warning.as_bytes());
    }
    let _ = std::io::stdout().write_all(outcome.text.as_bytes());
}

pub fn emit_error(command: &str, error: CommandError, as_json: bool) -> ! {
    if as_json {
        write_document(&document(
            command,
            false,
            error.payload,
            Some((error.code, &error.message)),
        ));
    } else {
        let prose = error
            .prose
            .unwrap_or_else(|| format!("loch: {}\n", error.message));
        let _ = std::io::stderr().write_all(prose.as_bytes());
    }
    let _ = std::io::stdout().flush();
    std::process::exit(error.exit_status);
}

fn write_document(body: &Value) {
    let mut out = python_dumps(body);
    out.push('\n');
    let mut stdout = std::io::stdout();
    let _ = stdout.write_all(out.as_bytes());
    let _ = stdout.flush();
}

/// `json.dumps(value)` with Python's defaults.
pub fn python_dumps(value: &Value) -> String {
    let mut out = String::new();
    write_value(&mut out, value, ", ", ": ");
    out
}

/// `json.dumps(value, sort_keys=True, separators=(",", ":"))`, the form a
/// commit note is stored in.
pub fn python_dumps_compact_sorted(value: &Value) -> String {
    let mut out = String::new();
    write_sorted(&mut out, value);
    out
}

fn write_sorted(out: &mut String, value: &Value) {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            // Python sorts str keys by code point; Rust's str order is UTF-8
            // byte order, which is the same thing.
            keys.sort();
            out.push('{');
            for (i, key) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_string(out, key);
                out.push(':');
                write_sorted(out, &map[key]);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_sorted(out, item);
            }
            out.push(']');
        }
        other => write_value(out, other, ",", ":"),
    }
}

fn write_value(out: &mut String, value: &Value, item_sep: &str, key_sep: &str) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(number) => {
            if let Some(f) = number.as_f64().filter(|_| number.is_f64()) {
                out.push_str(&python_float_repr(f));
            } else {
                out.push_str(&number.to_string());
            }
        }
        Value::String(text) => write_string(out, text),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(item_sep);
                }
                write_value(out, item, item_sep, key_sep);
            }
            out.push(']');
        }
        Value::Object(map) => {
            out.push('{');
            for (i, (key, item)) in map.iter().enumerate() {
                if i > 0 {
                    out.push_str(item_sep);
                }
                write_string(out, key);
                out.push_str(key_sep);
                write_value(out, item, item_sep, key_sep);
            }
            out.push('}');
        }
    }
}

/// `ensure_ascii=True`: everything outside printable ASCII becomes `\uXXXX`,
/// astral characters as a surrogate pair, exactly as Python escapes them.
fn write_string(out: &mut String, text: &str) {
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            ' '..='~' => out.push(ch),
            _ => {
                let mut units = [0u16; 2];
                for unit in ch.encode_utf16(&mut units) {
                    out.push_str(&format!("\\u{:04x}", unit));
                }
            }
        }
    }
    out.push('"');
}

/// Python's `float.__repr__`: the shortest digits that round-trip, written
/// positionally when the exponent is in [-4, 16) and as `d.ddde±XX` outside.
pub fn python_float_repr(value: f64) -> String {
    if value.is_nan() {
        return "NaN".into();
    }
    if value.is_infinite() {
        return if value > 0.0 { "Infinity" } else { "-Infinity" }.into();
    }
    // `{:e}` gives the shortest round-trip digits in scientific form.
    let scientific = format!("{:e}", value);
    let (mantissa, exponent) = scientific.split_once('e').expect("{:e} has an e");
    let exponent: i32 = exponent.parse().expect("{:e} exponent is an integer");
    let negative = mantissa.starts_with('-');
    let digits: String = mantissa.chars().filter(|c| c.is_ascii_digit()).collect();
    let sign = if negative { "-" } else { "" };

    if (-4..16).contains(&exponent) {
        let point = exponent + 1;
        let body = if point <= 0 {
            format!("0.{}{}", "0".repeat((-point) as usize), digits)
        } else if point as usize >= digits.len() {
            format!("{}{}.0", digits, "0".repeat(point as usize - digits.len()))
        } else {
            let (whole, fraction) = digits.split_at(point as usize);
            format!("{}.{}", whole, fraction)
        };
        format!("{}{}", sign, body)
    } else {
        let (first, rest) = digits.split_at(1);
        let fraction = if rest.is_empty() {
            String::new()
        } else {
            format!(".{}", rest)
        };
        let exponent_sign = if exponent < 0 { '-' } else { '+' };
        format!(
            "{}{}{}e{}{:02}",
            sign,
            first,
            fraction,
            exponent_sign,
            exponent.abs()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn floats_match_python_repr() {
        let cases = [
            (1.0, "1.0"),
            (0.1, "0.1"),
            (1731859200.123456, "1731859200.123456"),
            (1e16, "1e+16"),
            (1.5e16, "1.5e+16"),
            (123456789012345.6, "123456789012345.6"),
            (0.0001, "0.0001"),
            (0.00001, "1e-05"),
            (-2.5e-7, "-2.5e-07"),
            (0.0, "0.0"),
            (1e300, "1e+300"),
        ];
        for (value, expected) in cases {
            assert_eq!(python_float_repr(value), expected, "{value}");
        }
    }

    #[test]
    fn strings_escape_like_ensure_ascii() {
        let value = json!({"a": "é😀\u{7f}\u{1}\"\\\n", "b": [1, true, null]});
        assert_eq!(
            python_dumps(&value),
            r#"{"a": "\u00e9\ud83d\ude00\u007f\u0001\"\\\n", "b": [1, true, null]}"#
        );
    }

    #[test]
    fn notes_are_compact_and_sorted() {
        let value = json!({"session": "s", "loch": 1, "agent": "claude-code"});
        assert_eq!(
            python_dumps_compact_sorted(&value),
            r#"{"agent":"claude-code","loch":1,"session":"s"}"#
        );
    }
}
