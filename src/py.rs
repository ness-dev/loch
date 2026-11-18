//! The handful of Python string semantics loch's behaviour depends on.
//!
//! **Lone surrogates.** A Python `str` can hold an unpaired surrogate — a
//! `\ud83d` cut from its pair by truncation decodes to one, and
//! `surrogateescape` turns every undecodable byte into one — and real
//! transcripts contain both. A Rust `String` can't. So the port maps each
//! surrogate U+D800..U+DFFF onto U+10F800..U+10FFFF, the top of plane 16's
//! private-use area, and maps it back wherever Python would write it. The
//! cost is that a transcript holding those private-use code points literally
//! would be read as holding surrogates; the corpus check asserts no transcript
//! on this machine does, and `has_reserved` lets a caller find out.

/// `str.isspace()`, which is also what `\s` matches in a `str` pattern.
/// Broader than Rust's: it includes the file/group/record/unit separators.
pub fn is_space(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n'
            | '\u{0b}'
            | '\u{0c}'
            | '\r'
            | '\u{1c}'..='\u{1f}'
            | ' '
            | '\u{85}'
            | '\u{a0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200a}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202f}'
            | '\u{205f}'
            | '\u{3000}'
    )
}

/// The same set as a regex character-class body, for patterns ported from
/// Python that say `\s`.
pub const SPACE_CLASS: &str = r"\t\n\x0b\x0c\r\x1c-\x1f \x{85}\x{a0}\x{1680}\x{2000}-\x{200a}\x{2028}\x{2029}\x{202f}\x{205f}\x{3000}";

/// `str.split()` with no argument.
pub fn split(text: &str) -> Vec<&str> {
    text.split(is_space)
        .filter(|part| !part.is_empty())
        .collect()
}

/// `str.strip()` with no argument.
pub fn strip(text: &str) -> &str {
    text.trim_matches(is_space)
}

const SURROGATE_BASE: u32 = 0x10F800;

pub fn surrogate(unit: u32) -> char {
    debug_assert!((0xD800..=0xDFFF).contains(&unit));
    char::from_u32(SURROGATE_BASE + (unit - 0xD800)).expect("plane 16 is valid")
}

/// The surrogate a mapped character stands for, if it is one.
pub fn as_surrogate(c: char) -> Option<u32> {
    let code = c as u32;
    (code >= SURROGATE_BASE).then(|| code - SURROGATE_BASE + 0xD800)
}

/// The code point Python would see: the surrogate for a mapped character.
pub fn code_point(c: char) -> u32 {
    as_surrogate(c).unwrap_or(c as u32)
}

pub fn has_reserved(text: &str) -> bool {
    text.chars().any(|c| as_surrogate(c).is_some())
}

/// `bytes.decode("utf-8", "surrogateescape")`: each byte of an invalid
/// sequence becomes U+DC80..U+DCFF. Python and Rust agree on where an invalid
/// sequence ends (the Unicode "maximal subpart" rule), and since every byte is
/// escaped on its own, that is all that has to agree.
pub fn decode_surrogateescape(mut bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    loop {
        match std::str::from_utf8(bytes) {
            Ok(valid) => {
                out.push_str(valid);
                return out;
            }
            Err(error) => {
                let (valid, rest) = bytes.split_at(error.valid_up_to());
                out.push_str(std::str::from_utf8(valid).expect("checked valid"));
                let bad = error.error_len().unwrap_or(rest.len());
                for &byte in &rest[..bad] {
                    out.push(surrogate(0xDC00 + byte as u32));
                }
                bytes = &rest[bad..];
            }
        }
    }
}

/// `bytes.decode("utf-8", "replace")`, which also follows maximal subparts.
pub fn decode_replace(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// `str.encode("utf-8")`: fails, as Python does, on any surrogate.
pub fn encode_strict(text: &str) -> Result<Vec<u8>, char> {
    match text.chars().find(|&c| as_surrogate(c).is_some()) {
        Some(c) => Err(c),
        None => Ok(text.as_bytes().to_vec()),
    }
}

/// `str.encode("utf-8", "surrogateescape")`: an escaped byte goes back to
/// itself; any other surrogate is unencodable, as in Python.
pub fn encode_surrogateescape(text: &str) -> Result<Vec<u8>, char> {
    if !has_reserved(text) {
        return Ok(text.as_bytes().to_vec());
    }
    let mut out = Vec::with_capacity(text.len());
    let mut buffer = [0u8; 4];
    for c in text.chars() {
        match as_surrogate(c) {
            Some(unit @ 0xDC80..=0xDCFF) => out.push((unit - 0xDC00) as u8),
            Some(_) => return Err(c),
            None => out.extend_from_slice(c.encode_utf8(&mut buffer).as_bytes()),
        }
    }
    Ok(out)
}

/// Python's `str` ordering: by code point, with a mapped surrogate in the
/// place of the surrogate it stands for.
pub fn cmp(a: &str, b: &str) -> std::cmp::Ordering {
    if !has_reserved(a) && !has_reserved(b) {
        return a.cmp(b);
    }
    a.chars().map(code_point).cmp(b.chars().map(code_point))
}

/// `len(text)` in code points.
pub fn len(text: &str) -> usize {
    text.chars().count()
}

/// `text[:limit]` in code points.
pub fn truncate(text: &str, limit: usize) -> &str {
    match text.char_indices().nth(limit) {
        Some((index, _)) => &text[..index],
        None => text,
    }
}

/// `str.isdigit()` for the ASCII digits loch's callers can actually pass.
pub fn is_digits(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn surrogateescape_round_trips() {
        let bytes = b"ok \xff\xfe \xed\xa0\x80 \xe2\x9c\x93 \xe2\x9c";
        let text = decode_surrogateescape(bytes);
        assert_eq!(
            text.chars().filter(|&c| as_surrogate(c).is_some()).count(),
            7
        );
        assert_eq!(encode_surrogateescape(&text).unwrap(), bytes);
        assert!(encode_strict(&text).is_err());
    }

    #[test]
    fn ordering_places_surrogates_where_python_does() {
        let lone = surrogate(0xD83D).to_string();
        assert_eq!(cmp(&lone, "\u{E000}"), std::cmp::Ordering::Less);
        assert_eq!(cmp(&lone, "\u{D7FF}"), std::cmp::Ordering::Greater);
    }
}
