//! `loch blame --html`: the annotated file as one
//! self-contained page. `blame_template.html` was the Python module's
//! `TEMPLATE`, extracted verbatim, so it is filled the way `str.format` would
//! fill it: `{{`/`}}` are literal braces and `{name}` a field.

use crate::json::{self, Value};
use crate::object;
use crate::py;

const PALETTE: [&str; 8] = [
    "#4f8cc9", "#d08a3a", "#8e6cc2", "#4a9e6b", "#c75b7a", "#3a9ea5", "#a8913a", "#6b7fd1",
];
const NONE_COLOUR: &str = "#9aa0a6";
const TEMPLATE: &str = include_str!("blame_template.html");

/// `html.escape(text)`, quotes included.
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#x27;"),
            c => out.push(c),
        }
    }
    out
}

fn short(text: &str, limit: usize) -> String {
    let flat = py::split(text).join(" ");
    if py::len(&flat) <= limit {
        flat
    } else {
        format!("{}…", py::truncate(&flat, limit - 1))
    }
}

fn format_template(fields: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(TEMPLATE.len() * 2);
    let mut rest = TEMPLATE;
    while let Some(i) = rest.find(['{', '}']) {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        if tail.starts_with("{{") || tail.starts_with("}}") {
            out.push_str(&tail[..1]);
            rest = &tail[2..];
        } else if let Some(end) = tail.find('}').filter(|_| tail.starts_with('{')) {
            let name = &tail[1..end];
            let value = fields
                .iter()
                .find(|(field, _)| *field == name)
                .map(|(_, value)| *value)
                .expect("every template field is supplied");
            out.push_str(value);
            rest = &tail[end + 1..];
        } else {
            out.push_str(&tail[..1]);
            rest = &tail[1..];
        }
    }
    out.push_str(rest);
    out
}

/// Conversations and hunks as `cli::command_blame` builds them, in JSON form.
pub fn render(
    file: &str,
    conversations: &[Value],
    hunks: &[Value],
    lines: &std::collections::HashMap<usize, String>,
    visible: Option<&[(i64, i64)]>,
) -> String {
    let mark_of = |v: &Value| {
        v.get("mark")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    let order: std::collections::HashMap<String, usize> = conversations
        .iter()
        .enumerate()
        .map(|(i, c)| (mark_of(c), i))
        .collect();
    let by_mark: std::collections::HashMap<String, &Value> =
        conversations.iter().map(|c| (mark_of(c), c)).collect();
    let colour = |mark: &str| match order.get(mark) {
        Some(index) => PALETTE[index % PALETTE.len()],
        None => NONE_COLOUR,
    };
    let shown = |number: i64| {
        visible.is_none_or(|ranges| ranges.iter().any(|&(a, b)| a <= number && number <= b))
    };

    let mut rows: Vec<String> = Vec::new();
    let mut previous_mark: Option<String> = None;
    let mut previous_number: Option<i64> = None;
    for hunk in hunks {
        let mark = mark_of(hunk);
        let colour = colour(&mark);
        let start = hunk.get("start").and_then(Value::as_i64).unwrap_or(0);
        let end = hunk.get("end").and_then(Value::as_i64).unwrap_or(0);
        for number in start..=end {
            if !shown(number) {
                continue;
            }
            if previous_number.is_some_and(|p| number != p + 1) {
                rows.push("<tr class=\"gap\"><td colspan=\"4\">⋯</td></tr>".into());
                previous_mark = None;
            }
            let mut label = String::new();
            if previous_mark.as_deref() != Some(mark.as_str()) {
                label = match by_mark.get(&mark) {
                    Some(entry) => {
                        let asked = match entry.get("prompt") {
                            Some(Value::Str(p)) if !p.is_empty() => p.clone(),
                            _ => "(no prompt recorded)".into(),
                        };
                        format!(
                            "<span class=\"label\">{} · {}</span>",
                            escape(&mark),
                            escape(&short(&asked, 70))
                        )
                    }
                    None if mark == "+" => "<span class=\"label\">not committed yet</span>".into(),
                    None => "<span class=\"label\">no conversation recorded</span>".into(),
                };
            }
            let text = escape(
                lines
                    .get(&(number as usize))
                    .map(String::as_str)
                    .unwrap_or(""),
            );
            let text = if text.is_empty() {
                "&nbsp;".to_string()
            } else {
                text
            };
            rows.push(format!(
                "<tr data-mark=\"{}\" data-line=\"{}\" style=\"--c:{}\"><td class=\"bar\"></td><td class=\"num\">{}</td><td class=\"code\">{}</td><td class=\"note\">{}</td></tr>",
                escape(&mark),
                number,
                colour,
                number,
                text,
                label
            ));
            previous_mark = Some(mark.clone());
            previous_number = Some(number);
        }
    }

    let mut key = String::new();
    for entry in conversations {
        let mark = mark_of(entry);
        let asked = match entry.get("prompt") {
            Some(Value::Str(p)) if !p.is_empty() => p.clone(),
            _ if !entry.get("available").is_some_and(Value::truthy) => {
                "(transcript not available)".into()
            }
            _ => "(no prompt recorded)".into(),
        };
        let commits: Vec<String> = entry
            .get("commits")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(|c| py::truncate(c, 7).to_string())
            .collect();
        let session = entry.get("session").and_then(Value::as_str).unwrap_or("");
        key.push_str(&format!(
            "<li data-mark=\"{}\" style=\"--c:{}\"><span class=\"chip\">{}</span><span class=\"asked\">“{}”</span><span class=\"meta\">session {} · {}</span></li>",
            escape(&mark),
            colour(&mark),
            escape(&mark),
            escape(&asked),
            escape(py::truncate(session, 8)),
            escape(&commits.join(", "))
        ));
    }

    let data = json::dumps(
        &Value::Object(object! {"file" => file, "conversations" => conversations.to_vec()}),
        json::UNICODE,
    )
    .replace("</", "<\\/");
    let count = conversations.len();
    let title = format!(
        "{} — {} conversation{}",
        file,
        count,
        if count != 1 { "s" } else { "" }
    );
    format_template(&[
        ("title", &escape(&title)),
        ("file", &escape(file)),
        ("key", &key),
        ("rows", &rows.concat()),
        ("data", &data),
    ])
}

/// Python's `int()` on a string: surrounding whitespace, a sign, and
/// underscores between digits.
pub fn py_int(text: &str) -> Option<i64> {
    let text = py::strip(text);
    let (negative, digits) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text.strip_prefix('+').unwrap_or(text)),
    };
    if digits.is_empty()
        || digits.starts_with('_')
        || digits.ends_with('_')
        || digits.contains("__")
    {
        return None;
    }
    let cleaned: String = digits.chars().filter(|&c| c != '_').collect();
    if !cleaned.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let value: i64 = cleaned.parse().ok()?;
    Some(if negative { -value } else { value })
}

/// `1-12,99-110` → [(1, 12), (99, 110)]; a bare number is one line.
pub fn parse_ranges(spec: &str) -> Option<Vec<(i64, i64)>> {
    let mut ranges = Vec::new();
    for part in spec.split(',') {
        let part = py::strip(part);
        if part.is_empty() {
            continue;
        }
        let (start, end) = part.split_once('-').unwrap_or((part, ""));
        let first = py_int(start)?;
        let last = if end.is_empty() { first } else { py_int(end)? };
        ranges.push((first, last));
    }
    Some(ranges)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_braces_unescape() {
        let page = format_template(&[
            ("title", "T"),
            ("file", "F"),
            ("key", ""),
            ("rows", ""),
            ("data", "{}"),
        ]);
        assert!(page.contains(".loch-blame {\n"));
        assert!(page.contains("${line}"));
        assert!(page.contains("const data = {};"));
    }
}
