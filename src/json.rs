//! JSON as Python's `json` module reads and writes it.
//!
//! serde_json can't stand in: it refuses lone surrogates (five transcripts on
//! one machine hold them), rounds integers wider than 64 bits, and rejects the
//! `NaN`/`Infinity` Python accepts. Every id in the loch format, every bundle
//! ref and every byte of `--json` output depends on agreeing with Python
//! exactly, so this is a small parser and writer written to its rules:
//! duplicate keys keep their first position and last value, integers stay
//! exact, and strings hold surrogates in the mapping `py.rs` describes.

use crate::py;
use indexmap::IndexMap;

pub type Map = IndexMap<String, Value>;

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    /// Exact decimal, as Python's arbitrary-precision int prints.
    Int(String),
    Float(f64),
    Str(String),
    Array(Vec<Value>),
    Object(Map),
}

impl Value {
    pub fn str(text: impl Into<String>) -> Value {
        Value::Str(text.into())
    }

    pub fn int(value: i64) -> Value {
        Value::Int(value.to_string())
    }

    pub fn opt_str(value: Option<&str>) -> Value {
        value.map(Value::str).unwrap_or(Value::Null)
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(text) => Some(text),
            _ => None,
        }
    }

    pub fn as_object(&self) -> Option<&Map> {
        match self {
            Value::Object(map) => Some(map),
            _ => None,
        }
    }

    pub fn as_object_mut(&mut self) -> Option<&mut Map> {
        match self {
            Value::Object(map) => Some(map),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&Vec<Value>> {
        match self {
            Value::Array(items) => Some(items),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Int(digits) => digits.parse().ok(),
            _ => None,
        }
    }

    /// `obj.get(key)` on something that may not be a dict.
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.as_object().and_then(|map| map.get(key))
    }

    /// Python truthiness.
    pub fn truthy(&self) -> bool {
        match self {
            Value::Null => false,
            Value::Bool(b) => *b,
            Value::Int(digits) => digits != "0",
            Value::Float(f) => *f != 0.0,
            Value::Str(s) => !s.is_empty(),
            Value::Array(a) => !a.is_empty(),
            Value::Object(o) => !o.is_empty(),
        }
    }

    pub fn is_str(&self, text: &str) -> bool {
        self.as_str() == Some(text)
    }
}

impl From<&str> for Value {
    fn from(text: &str) -> Value {
        Value::Str(text.to_string())
    }
}

impl From<String> for Value {
    fn from(text: String) -> Value {
        Value::Str(text)
    }
}

impl From<bool> for Value {
    fn from(b: bool) -> Value {
        Value::Bool(b)
    }
}

impl From<i64> for Value {
    fn from(n: i64) -> Value {
        Value::int(n)
    }
}

impl From<usize> for Value {
    fn from(n: usize) -> Value {
        Value::Int(n.to_string())
    }
}

impl From<Map> for Value {
    fn from(map: Map) -> Value {
        Value::Object(map)
    }
}

impl From<Vec<Value>> for Value {
    fn from(items: Vec<Value>) -> Value {
        Value::Array(items)
    }
}

/// Builds a `Map` in insertion order: `object! { "a" => 1, "b" => "x" }`.
#[macro_export]
macro_rules! object {
    () => { $crate::json::Map::new() };
    ($($key:expr => $value:expr),+ $(,)?) => {{
        let mut map = $crate::json::Map::new();
        $( map.insert(String::from($key), $crate::json::Value::from($value)); )+
        map
    }};
}

// -- parsing -----------------------------------------------------------------

#[derive(Debug)]
pub struct ParseError;

/// Nesting deeper than this fails rather than overflowing the stack; Python
/// would have raised RecursionError somewhat earlier.
const MAX_DEPTH: usize = 2000;

/// `json.loads(text)`.
pub fn loads(text: &str) -> Result<Value, ParseError> {
    let mut parser = Parser {
        bytes: text.as_bytes(),
        text,
        pos: 0,
        depth: 0,
    };
    parser.skip_ws();
    let value = parser.value()?;
    parser.skip_ws();
    if parser.pos != parser.bytes.len() {
        return Err(ParseError);
    }
    Ok(value)
}

struct Parser<'a> {
    bytes: &'a [u8],
    text: &'a str,
    pos: usize,
    depth: usize,
}

impl Parser<'_> {
    fn skip_ws(&mut self) {
        while let Some(b' ' | b'\t' | b'\n' | b'\r') = self.bytes.get(self.pos) {
            self.pos += 1;
        }
    }

    fn eat(&mut self, literal: &str) -> bool {
        if self.bytes[self.pos..].starts_with(literal.as_bytes()) {
            self.pos += literal.len();
            true
        } else {
            false
        }
    }

    fn value(&mut self) -> Result<Value, ParseError> {
        match self.bytes.get(self.pos).copied().ok_or(ParseError)? {
            b'"' => {
                self.pos += 1;
                Ok(Value::Str(self.string()?))
            }
            b'{' => self.object(),
            b'[' => self.array(),
            b'n' if self.eat("null") => Ok(Value::Null),
            b't' if self.eat("true") => Ok(Value::Bool(true)),
            b'f' if self.eat("false") => Ok(Value::Bool(false)),
            b'N' if self.eat("NaN") => Ok(Value::Float(f64::NAN)),
            b'I' if self.eat("Infinity") => Ok(Value::Float(f64::INFINITY)),
            b'-' if self.eat("-Infinity") => Ok(Value::Float(f64::NEG_INFINITY)),
            b'-' | b'0'..=b'9' => self.number(),
            _ => Err(ParseError),
        }
    }

    fn nest(&mut self) -> Result<(), ParseError> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(ParseError);
        }
        Ok(())
    }

    fn object(&mut self) -> Result<Value, ParseError> {
        self.nest()?;
        self.pos += 1;
        let mut map = Map::new();
        self.skip_ws();
        if self.bytes.get(self.pos) == Some(&b'}') {
            self.pos += 1;
            self.depth -= 1;
            return Ok(Value::Object(map));
        }
        loop {
            if self.bytes.get(self.pos) != Some(&b'"') {
                return Err(ParseError);
            }
            self.pos += 1;
            let key = self.string()?;
            self.skip_ws();
            if self.bytes.get(self.pos) != Some(&b':') {
                return Err(ParseError);
            }
            self.pos += 1;
            self.skip_ws();
            let value = self.value()?;
            // dict assignment: a repeated key keeps its first position.
            map.insert(key, value);
            self.skip_ws();
            match self.bytes.get(self.pos) {
                Some(b',') => {
                    self.pos += 1;
                    self.skip_ws();
                }
                Some(b'}') => {
                    self.pos += 1;
                    self.depth -= 1;
                    return Ok(Value::Object(map));
                }
                _ => return Err(ParseError),
            }
        }
    }

    fn array(&mut self) -> Result<Value, ParseError> {
        self.nest()?;
        self.pos += 1;
        let mut items = Vec::new();
        self.skip_ws();
        if self.bytes.get(self.pos) == Some(&b']') {
            self.pos += 1;
            self.depth -= 1;
            return Ok(Value::Array(items));
        }
        loop {
            items.push(self.value()?);
            self.skip_ws();
            match self.bytes.get(self.pos) {
                Some(b',') => {
                    self.pos += 1;
                    self.skip_ws();
                }
                Some(b']') => {
                    self.pos += 1;
                    self.depth -= 1;
                    return Ok(Value::Array(items));
                }
                _ => return Err(ParseError),
            }
        }
    }

    fn number(&mut self) -> Result<Value, ParseError> {
        let start = self.pos;
        if self.bytes[self.pos] == b'-' {
            self.pos += 1;
        }
        let digits = |p: &mut Self| {
            let from = p.pos;
            while p.bytes.get(p.pos).is_some_and(u8::is_ascii_digit) {
                p.pos += 1;
            }
            p.pos - from
        };
        match self.bytes.get(self.pos) {
            Some(b'0') => self.pos += 1,
            Some(b'1'..=b'9') => {
                digits(self);
            }
            _ => return Err(ParseError),
        }
        let mut float = false;
        if self.bytes.get(self.pos) == Some(&b'.')
            && self.bytes.get(self.pos + 1).is_some_and(u8::is_ascii_digit)
        {
            self.pos += 1;
            digits(self);
            float = true;
        }
        if let Some(b'e' | b'E') = self.bytes.get(self.pos) {
            let save = self.pos;
            self.pos += 1;
            if let Some(b'+' | b'-') = self.bytes.get(self.pos) {
                self.pos += 1;
            }
            if digits(self) == 0 {
                self.pos = save;
            } else {
                float = true;
            }
        }
        let literal = &self.text[start..self.pos];
        if float {
            literal
                .parse::<f64>()
                .map(Value::Float)
                .map_err(|_| ParseError)
        } else {
            let trimmed = literal.trim_start_matches('-');
            let negative = literal.starts_with('-') && trimmed != "0";
            Ok(Value::Int(if negative {
                literal.to_string()
            } else {
                trimmed.to_string()
            }))
        }
    }

    fn hex4(&self, at: usize) -> Option<u32> {
        let slice = self.bytes.get(at..at + 4)?;
        if !slice.iter().all(u8::is_ascii_hexdigit) {
            return None;
        }
        u32::from_str_radix(std::str::from_utf8(slice).ok()?, 16).ok()
    }

    fn string(&mut self) -> Result<String, ParseError> {
        let mut out = String::new();
        loop {
            let run_start = self.pos;
            while let Some(&b) = self.bytes.get(self.pos) {
                if b == b'"' || b == b'\\' || b < 0x20 {
                    break;
                }
                self.pos += 1;
            }
            out.push_str(&self.text[run_start..self.pos]);
            match self.bytes.get(self.pos).copied() {
                Some(b'"') => {
                    self.pos += 1;
                    return Ok(out);
                }
                Some(b'\\') => {
                    let escape = *self.bytes.get(self.pos + 1).ok_or(ParseError)?;
                    self.pos += 2;
                    match escape {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{08}'),
                        b'f' => out.push('\u{0c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let unit = self.hex4(self.pos).ok_or(ParseError)?;
                            self.pos += 4;
                            if (0xD800..=0xDBFF).contains(&unit)
                                && self.bytes.get(self.pos..self.pos + 2) == Some(b"\\u")
                            {
                                if let Some(low @ 0xDC00..=0xDFFF) = self.hex4(self.pos + 2) {
                                    let code = 0x10000 + ((unit - 0xD800) << 10) + (low - 0xDC00);
                                    out.push(char::from_u32(code).ok_or(ParseError)?);
                                    self.pos += 6;
                                    continue;
                                }
                            }
                            if (0xD800..=0xDFFF).contains(&unit) {
                                out.push(py::surrogate(unit));
                            } else {
                                out.push(char::from_u32(unit).ok_or(ParseError)?);
                            }
                        }
                        _ => return Err(ParseError),
                    }
                }
                // A raw control character, or the end: both errors under
                // Python's default strict=True.
                _ => return Err(ParseError),
            }
        }
    }
}

// -- writing -----------------------------------------------------------------

#[derive(Clone, Copy)]
pub struct Style {
    pub ensure_ascii: bool,
    pub item_sep: &'static str,
    pub key_sep: &'static str,
    pub sort_keys: bool,
    pub indent: Option<usize>,
}

/// `json.dumps(value)`.
pub const DEFAULT: Style = Style {
    ensure_ascii: true,
    item_sep: ", ",
    key_sep: ": ",
    sort_keys: false,
    indent: None,
};

/// `json.dumps(value, separators=(",", ":"), ensure_ascii=False)`.
pub const COMPACT_UNICODE: Style = Style {
    ensure_ascii: false,
    item_sep: ",",
    key_sep: ":",
    sort_keys: false,
    indent: None,
};

/// `json.dumps(value, sort_keys=True, separators=(",", ":"))`.
pub const COMPACT_SORTED: Style = Style {
    ensure_ascii: true,
    item_sep: ",",
    key_sep: ":",
    sort_keys: true,
    indent: None,
};

/// `json.dumps(value, ensure_ascii=False)`.
pub const UNICODE: Style = Style {
    ensure_ascii: false,
    ..DEFAULT
};

/// `json.dumps(value, indent=2)`, whose item separator is a bare comma.
pub const INDENTED: Style = Style {
    ensure_ascii: true,
    item_sep: ",",
    key_sep: ": ",
    sort_keys: false,
    indent: Some(2),
};

pub fn dumps(value: &Value, style: Style) -> String {
    let mut out = String::new();
    write(&mut out, value, style, 0);
    out
}

fn newline(out: &mut String, style: Style, level: usize) {
    if let Some(width) = style.indent {
        out.push('\n');
        out.push_str(&" ".repeat(width * level));
    }
}

fn write(out: &mut String, value: &Value, style: Style, level: usize) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Int(digits) => out.push_str(digits),
        Value::Float(f) => out.push_str(&float_repr(*f)),
        Value::Str(text) => write_str(out, text, style.ensure_ascii),
        Value::Array(items) => {
            if items.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(style.item_sep);
                }
                newline(out, style, level + 1);
                write(out, item, style, level + 1);
            }
            newline(out, style, level);
            out.push(']');
        }
        Value::Object(map) => {
            if map.is_empty() {
                out.push_str("{}");
                return;
            }
            out.push('{');
            let mut entries: Vec<(&String, &Value)> = map.iter().collect();
            if style.sort_keys {
                entries.sort_by(|a, b| py::cmp(a.0, b.0));
            }
            for (i, (key, item)) in entries.into_iter().enumerate() {
                if i > 0 {
                    out.push_str(style.item_sep);
                }
                newline(out, style, level + 1);
                write_str(out, key, style.ensure_ascii);
                out.push_str(style.key_sep);
                write(out, item, style, level + 1);
            }
            newline(out, style, level);
            out.push('}');
        }
    }
}

/// Python's string escaping. With `ensure_ascii` everything outside printable
/// ASCII is `\uXXXX`; without it, only `"`, `\` and control characters are
/// escaped and a surrogate is written as itself — which Python then fails to
/// encode, so callers encode with `py::encode_strict` and treat that the same.
pub fn write_str(out: &mut String, text: &str, ensure_ascii: bool) {
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            '\u{00}'..='\u{1f}' => out.push_str(&format!("\\u{:04x}", c as u32)),
            ' '..='~' => out.push(c),
            _ if !ensure_ascii => out.push(c),
            _ => match py::as_surrogate(c) {
                Some(unit) => out.push_str(&format!("\\u{:04x}", unit)),
                None => {
                    let mut units = [0u16; 2];
                    for unit in c.encode_utf16(&mut units) {
                        out.push_str(&format!("\\u{:04x}", unit));
                    }
                }
            },
        }
    }
    out.push('"');
}

/// The shortest round-trip digits of a finite, positive double, and the
/// decimal exponent of the first: 0.00123 is ("123", -3).
fn shortest_digits(value: f64) -> (String, i32) {
    let scientific = format!("{:e}", value);
    let (mantissa, exponent) = scientific.split_once('e').expect("{:e} has an e");
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    let digits = digits.trim_end_matches('0');
    let digits = if digits.is_empty() { "0" } else { digits };
    (
        digits.to_string(),
        exponent.parse().expect("integer exponent"),
    )
}

/// Python's `float.__repr__`, which `json.dumps` writes: positional when the
/// exponent is in [-4, 16), `d.ddde±XX` outside.
pub fn float_repr(value: f64) -> String {
    if value.is_nan() {
        return "NaN".into();
    }
    if value.is_infinite() {
        return if value > 0.0 { "Infinity" } else { "-Infinity" }.into();
    }
    let sign = if value.is_sign_negative() { "-" } else { "" };
    if value == 0.0 {
        return format!("{}0.0", sign);
    }
    let (digits, exponent) = shortest_digits(value.abs());
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

// -- RFC 8785 -------------------------------------------------------------------

#[derive(Debug)]
pub struct NotJson;

/// RFC 8785, exactly as `format.canonical` writes it: keys by UTF-16 code
/// unit, strings escaped as JSON.stringify does (a lone surrogate as
/// `\uXXXX`), numbers as ECMAScript prints them — except that an integer
/// prints exactly at any size, so a 63-bit id inside a tool result is kept.
pub fn canonical(value: &Value) -> Result<Vec<u8>, NotJson> {
    let mut out = String::new();
    write_canonical(&mut out, value)?;
    Ok(out.into_bytes())
}

fn canonical_str(out: &mut String, text: &str) {
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            '\u{00}'..='\u{1f}' => out.push_str(&format!("\\u{:04x}", c as u32)),
            _ => match py::as_surrogate(c) {
                Some(unit) => out.push_str(&format!("\\u{:04x}", unit)),
                None => out.push(c),
            },
        }
    }
    out.push('"');
}

fn utf16_units(text: &str) -> Vec<u16> {
    let mut units = Vec::with_capacity(text.len());
    let mut buffer = [0u16; 2];
    for c in text.chars() {
        match py::as_surrogate(c) {
            Some(unit) => units.push(unit as u16),
            None => units.extend_from_slice(c.encode_utf16(&mut buffer)),
        }
    }
    units
}

fn write_canonical(out: &mut String, value: &Value) -> Result<(), NotJson> {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Int(digits) => out.push_str(digits),
        Value::Float(f) => out.push_str(&ecma_number(*f)?),
        Value::Str(text) => canonical_str(out, text),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_canonical(out, item)?;
            }
            out.push(']');
        }
        Value::Object(map) => {
            let mut keys: Vec<(Vec<u16>, &String)> =
                map.keys().map(|key| (utf16_units(key), key)).collect();
            keys.sort_by(|a, b| a.0.cmp(&b.0));
            out.push('{');
            for (i, (_, key)) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                canonical_str(out, key);
                out.push(':');
                write_canonical(out, &map[key])?;
            }
            out.push('}');
        }
    }
    Ok(())
}

/// ECMAScript Number-to-String.
fn ecma_number(value: f64) -> Result<String, NotJson> {
    if !value.is_finite() {
        return Err(NotJson);
    }
    if value == 0.0 {
        return Ok("0".into());
    }
    let sign = if value < 0.0 { "-" } else { "" };
    let (digits, exponent) = shortest_digits(value.abs());
    let k = digits.len() as i32;
    let n = exponent + 1;
    Ok(if k <= n && n <= 21 {
        format!("{}{}{}", sign, digits, "0".repeat((n - k) as usize))
    } else if 0 < n && n <= 21 {
        format!(
            "{}{}.{}",
            sign,
            &digits[..n as usize],
            &digits[n as usize..]
        )
    } else if -6 < n && n <= 0 {
        format!("{}0.{}{}", sign, "0".repeat((-n) as usize), digits)
    } else {
        let mantissa = if k > 1 {
            format!("{}.{}", &digits[..1], &digits[1..])
        } else {
            digits.clone()
        };
        let power = n - 1;
        format!(
            "{}{}e{}{}",
            sign,
            mantissa,
            if power >= 0 { '+' } else { '-' },
            power.abs()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floats_match_python_repr() {
        for (value, expected) in [
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
            (-0.0, "-0.0"),
            (1e300, "1e+300"),
            (100.0, "100.0"),
        ] {
            assert_eq!(float_repr(value), expected, "{value}");
        }
    }

    #[test]
    fn numbers_match_ecmascript() {
        for (value, expected) in [
            (1e16, "10000000000000000"),
            (0.00001, "0.00001"),
            (1e21, "1e+21"),
            (1e-7, "1e-7"),
            (1.5, "1.5"),
            (100.0, "100"),
            (-0.0, "0"),
        ] {
            assert_eq!(ecma_number(value).unwrap(), expected, "{value}");
        }
    }

    #[test]
    fn parses_what_python_parses() {
        let value = loads(r#" {"a": 1, "b": [1.5, -0, 123456789012345678901234567890], "a": NaN, "s": "😀\ud83d x"} "#).unwrap();
        let map = value.as_object().unwrap();
        assert_eq!(map.keys().collect::<Vec<_>>(), ["a", "b", "s"]);
        assert!(matches!(map["a"], Value::Float(f) if f.is_nan()));
        assert_eq!(
            dumps(&map["b"], DEFAULT),
            "[1.5, 0, 123456789012345678901234567890]"
        );
        assert_eq!(dumps(&map["s"], DEFAULT), r#""\ud83d\ude00\ud83d x""#);
        assert!(loads("[1] x").is_err());
        assert!(loads("\"a\nb\"").is_err());
        assert!(loads("01").is_err());
    }

    #[test]
    fn strings_escape_like_ensure_ascii() {
        let value = Value::Object(
            object! {"a" => "é😀\u{7f}\u{1}\"\\\n", "b" => Value::Array(vec![Value::int(1), Value::Bool(true), Value::Null])},
        );
        assert_eq!(
            dumps(&value, DEFAULT),
            "{\"a\": \"\\u00e9\\ud83d\\ude00\\u007f\\u0001\\\"\\\\\\n\", \"b\": [1, true, null]}"
        );
        assert_eq!(
            dumps(&value, COMPACT_UNICODE),
            "{\"a\":\"é😀\u{7f}\\u0001\\\"\\\\\\n\",\"b\":[1,true,null]}"
        );
    }

    #[test]
    fn indent_matches_python() {
        let value = Value::Object(
            object! {"a" => Value::Array(vec![]), "b" => Value::Object(object!{"c" => 1i64})},
        );
        assert_eq!(
            dumps(&value, INDENTED),
            "{\n  \"a\": [],\n  \"b\": {\n    \"c\": 1\n  }\n}"
        );
    }
}
