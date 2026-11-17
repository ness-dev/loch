//! Python's `os.path`, `datetime` and `OSError` behaviour, where loch's output
//! depends on the exact string they produce.

use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// `os.getcwd()`.
pub fn getcwd() -> String {
    std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// `posixpath.normpath`.
pub fn normpath(path: &str) -> String {
    if path.is_empty() {
        return ".".into();
    }
    let mut initial = usize::from(path.starts_with('/'));
    if path.starts_with("//") && !path.starts_with("///") {
        initial = 2;
    }
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        if part.is_empty() || part == "." {
            continue;
        }
        if part != ".." || (initial == 0 && parts.is_empty()) || parts.last() == Some(&"..") {
            parts.push(part);
        } else if !parts.is_empty() {
            parts.pop();
        }
    }
    let joined = format!("{}{}", "/".repeat(initial), parts.join("/"));
    if joined.is_empty() {
        ".".into()
    } else {
        joined
    }
}

/// `os.path.join(a, b)`.
pub fn join(a: &str, b: &str) -> String {
    if b.starts_with('/') || a.is_empty() {
        b.into()
    } else if a.ends_with('/') {
        format!("{}{}", a, b)
    } else {
        format!("{}/{}", a, b)
    }
}

/// `os.path.abspath`.
pub fn abspath(path: &str) -> String {
    if path.starts_with('/') {
        normpath(path)
    } else {
        normpath(&join(&getcwd(), path))
    }
}

/// `os.path.dirname`.
pub fn dirname(path: &str) -> String {
    let cut = path.rfind('/').map(|i| i + 1).unwrap_or(0);
    let head = &path[..cut];
    if !head.is_empty() && !head.chars().all(|c| c == '/') {
        head.trim_end_matches('/').into()
    } else {
        head.into()
    }
}

/// `os.path.basename`.
pub fn basename(path: &str) -> String {
    path[path.rfind('/').map(|i| i + 1).unwrap_or(0)..].into()
}

/// `os.path.relpath(path, start)`.
pub fn relpath(path: &str, start: &str) -> String {
    let path = abspath(path);
    let start = abspath(start);
    let path_parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    let start_parts: Vec<&str> = start.split('/').filter(|p| !p.is_empty()).collect();
    let common = path_parts
        .iter()
        .zip(&start_parts)
        .take_while(|(a, b)| a == b)
        .count();
    let mut parts: Vec<&str> = vec![".."; start_parts.len() - common];
    parts.extend(&path_parts[common..]);
    if parts.is_empty() {
        ".".into()
    } else {
        parts.join("/")
    }
}

/// `os.path.realpath(path, strict=True)`, or None where it would raise.
pub fn realpath_strict(path: &str) -> Option<String> {
    fs::canonicalize(path)
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
}

/// `str(OSError)` for an error on a path: `[Errno 2] No such file or
/// directory: '/x'`.
pub fn os_error(error: &std::io::Error, path: &str) -> String {
    match error.raw_os_error() {
        Some(code) => format!("[Errno {}] {}: {}", code, strerror(code), py_repr(path)),
        None => error.to_string(),
    }
}

pub fn strerror(code: i32) -> String {
    let text = std::io::Error::from_raw_os_error(code).to_string();
    // Rust appends " (os error N)"; Python's strerror is the bare message.
    match text.rfind(" (os error") {
        Some(cut) => text[..cut].into(),
        None => text,
    }
}

/// `repr()` of a str.
pub fn py_repr(text: &str) -> String {
    let quote = if text.contains('\'') && !text.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::new();
    out.push(quote);
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                out.push_str(&format!("\\x{:02x}", c as u32))
            }
            c => match crate::py::as_surrogate(c) {
                Some(unit) => out.push_str(&format!("\\u{:04x}", unit)),
                None => out.push(c),
            },
        }
    }
    out.push(quote);
    out
}

/// `repr()` of bytes.
pub fn bytes_repr(bytes: &[u8]) -> String {
    let quote = if bytes.contains(&b'\'') && !bytes.contains(&b'"') {
        '"'
    } else {
        '\''
    };
    let mut out = format!("b{}", quote);
    for &b in bytes {
        match b {
            b'\\' => out.push_str("\\\\"),
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            b'\t' => out.push_str("\\t"),
            b if b as char == quote => {
                out.push('\\');
                out.push(b as char);
            }
            0x20..=0x7e => out.push(b as char),
            _ => out.push_str(&format!("\\x{:02x}", b)),
        }
    }
    out.push(quote);
    out
}

/// `time.time()`.
pub fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// `os.stat(path).st_mtime`, computed as CPython computes it: the seconds
/// plus `nsec * 1e-9`, in double arithmetic.
pub fn mtime(meta: &fs::Metadata) -> f64 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        meta.mtime() as f64 + meta.mtime_nsec() as f64 * 1e-9
    }
    #[cfg(not(unix))]
    {
        meta.modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as f64 + d.subsec_nanos() as f64 * 1e-9)
            .unwrap_or(0.0)
    }
}

/// `_PyTime_ObjectToTimeval(..., _PyTime_ROUND_HALF_EVEN)`, which
/// `datetime.fromtimestamp` uses: whole seconds and microseconds.
pub fn split_timestamp(value: f64) -> (i64, i64) {
    let mut whole = value.trunc();
    let mut micros = (value - whole) * 1e6;
    let rounded = micros.round();
    micros = if (micros - rounded).abs() == 0.5 {
        2.0 * (micros / 2.0).round()
    } else {
        rounded
    };
    if micros >= 1e6 {
        micros -= 1e6;
        whole += 1.0;
    } else if micros < 0.0 {
        micros += 1e6;
        whole -= 1.0;
    }
    (whole as i64, micros as i64)
}

pub struct Civil {
    pub year: i64,
    pub month: i64,
    pub day: i64,
    pub hour: i64,
    pub minute: i64,
    pub second: i64,
}

/// UTC calendar fields of Unix seconds (Howard Hinnant's civil_from_days).
pub fn civil(seconds: i64) -> Civil {
    let days = seconds.div_euclid(86_400);
    let rest = seconds.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    Civil {
        year,
        month,
        day,
        hour: rest / 3600,
        minute: rest % 3600 / 60,
        second: rest % 60,
    }
}

/// Days since the epoch of a proleptic Gregorian date.
pub fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// `claude.iso_timestamp`: millisecond-precision UTC with a Z suffix.
pub fn iso_timestamp(seconds: f64) -> String {
    let (whole, micros) = split_timestamp(seconds);
    let c = civil(whole);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        c.year,
        c.month,
        c.day,
        c.hour,
        c.minute,
        c.second,
        micros / 1000
    )
}

/// `datetime.fromtimestamp(t, timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")`.
pub fn utc_seconds(seconds: i64) -> String {
    let c = civil(seconds);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        c.year, c.month, c.day, c.hour, c.minute, c.second
    )
}

/// `datetime.fromisoformat(stamp.replace("Z", "+00:00")).timestamp()` for the
/// shapes transcripts carry. A naive stamp is local time to Python; it is
/// read as UTC here, and no transcript writes one.
pub fn iso_seconds(stamp: &str) -> Option<f64> {
    let stamp = stamp.replace('Z', "+00:00");
    let b = stamp.as_bytes();
    let num = |from: usize, len: usize| -> Option<i64> {
        let part = stamp.get(from..from + len)?;
        part.bytes()
            .all(|c| c.is_ascii_digit())
            .then(|| part.parse().ok())?
    };
    if b.len() < 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let (year, month, day) = (num(0, 4)?, num(5, 2)?, num(8, 2)?);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let mut pos = 10;
    let (mut hour, mut minute, mut second, mut micros) = (0, 0, 0, 0i64);
    if pos < b.len() && (b[pos] == b'T' || b[pos] == b' ') {
        pos += 1;
        hour = num(pos, 2)?;
        pos += 2;
        if b.get(pos) == Some(&b':') {
            minute = num(pos + 1, 2)?;
            pos += 3;
            if b.get(pos) == Some(&b':') {
                second = num(pos + 1, 2)?;
                pos += 3;
                if let Some(b'.' | b',') = b.get(pos) {
                    pos += 1;
                    let start = pos;
                    while b.get(pos).is_some_and(u8::is_ascii_digit) {
                        pos += 1;
                    }
                    let digits = &stamp[start..pos];
                    if digits.is_empty() {
                        return None;
                    }
                    let six: String = digits.chars().chain("000000".chars()).take(6).collect();
                    micros = six.parse().ok()?;
                }
            }
        }
    }
    let mut offset = 0i64;
    if pos < b.len() {
        let sign = match b[pos] {
            b'+' => 1,
            b'-' => -1,
            _ => return None,
        };
        let hours = num(pos + 1, 2)?;
        let minutes = if b.get(pos + 3) == Some(&b':') {
            num(pos + 4, 2)?
        } else {
            num(pos + 3, 2)?
        };
        offset = sign * (hours * 3600 + minutes * 60);
    }
    if hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    let seconds =
        days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second - offset;
    Some((seconds * 1_000_000 + micros) as f64 / 1e6)
}

/// `socket.gethostname()`.
pub fn hostname() -> String {
    #[cfg(unix)]
    {
        let mut buffer = [0u8; 256];
        // SAFETY: gethostname writes at most len bytes into the buffer.
        let result = unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) };
        if result == 0 {
            let end = buffer.iter().position(|&b| b == 0).unwrap_or(buffer.len());
            return String::from_utf8_lossy(&buffer[..end]).into_owned();
        }
        String::new()
    }
    #[cfg(not(unix))]
    {
        std::env::var("COMPUTERNAME").unwrap_or_default()
    }
}

/// `cli._platform_name`.
pub fn platform_name() -> &'static str {
    if cfg!(target_os = "macos") {
        "darwin"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else if cfg!(windows) {
        "win32"
    } else {
        std::env::consts::OS
    }
}

/// `str(uuid.uuid4())`.
pub fn uuid4() -> String {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes).expect("the OS has a random source");
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|b| format!("{:02x}", b)).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// `tempfile.gettempdir()`: the first of TMPDIR, TEMP, TMP, then the
/// platform's usual places, made absolute.
pub fn gettempdir() -> String {
    for name in ["TMPDIR", "TEMP", "TMP"] {
        if let Ok(dir) = std::env::var(name) {
            if !dir.is_empty() && Path::new(&dir).is_dir() {
                return abspath(&dir);
            }
        }
    }
    for dir in ["/tmp", "/var/tmp", "/usr/tmp"] {
        if Path::new(dir).is_dir() {
            return dir.into();
        }
    }
    getcwd()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_follow_posixpath() {
        assert_eq!(normpath("//a//b/../c/."), "//a/c");
        assert_eq!(normpath("///a"), "/a");
        assert_eq!(normpath("../x/.."), "..");
        assert_eq!(dirname("/a/b/"), "/a/b");
        assert_eq!(dirname("///"), "///");
        assert_eq!(dirname("x"), "");
        assert_eq!(relpath("/a/b/c", "/a/d"), "../b/c");
        assert_eq!(relpath("/a", "/a"), ".");
    }

    #[test]
    fn timestamps_round_like_datetime() {
        assert_eq!(iso_timestamp(0.0), "1970-01-01T00:00:00.000Z");
        assert_eq!(
            iso_timestamp(1_794_947_157.999_6),
            "2026-11-17T20:25:57.999Z"
        );
        assert_eq!(
            iso_timestamp(1_794_947_157.999_999_6),
            "2026-11-17T20:25:58.000Z"
        );
        assert_eq!(iso_seconds("2026-01-01T00:00:10.5Z"), Some(1_767_225_610.5));
    }

    #[test]
    fn os_errors_read_like_python() {
        let error = std::fs::read("/nonexistent/x").unwrap_err();
        assert_eq!(
            os_error(&error, "/nonexistent/x"),
            "[Errno 2] No such file or directory: '/nonexistent/x'"
        );
    }
}
