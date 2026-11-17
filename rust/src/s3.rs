//! The S3-compatible backend, signed with SigV4 by hand.
//!
//! An SDK would be the whole AWS credential chain in exchange for a static
//! binary several megabytes larger and an async runtime; loch needs four
//! signed requests. So the chain is the part people actually use against R2,
//! B2, MinIO, Garage and plain S3: the `AWS_*` variables, then the shared
//! credentials and config files for `AWS_PROFILE` (or `default`). SSO,
//! `credential_process`, assumed roles and instance metadata are not read.
//!
//! Failures read the way botocore words them, since that text reaches the
//! `error` field: `An error occurred (Code) when calling the Op operation:
//! Message`.

use crate::claude::home_dir;
use crate::store::{Store, StoreError};
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// A hook has to come back before Claude Code gives up on it, so a store that
/// doesn't answer fails rather than hanging the turn.
const TIMEOUT: Duration = Duration::from_secs(20);

pub struct S3Store {
    bucket: String,
    prefix: String,
}

struct Response {
    status: u16,
    body: Vec<u8>,
}

impl S3Store {
    pub fn new(bucket: &str, prefix: &str) -> Self {
        S3Store {
            bucket: bucket.into(),
            prefix: prefix.into(),
        }
    }

    fn key_for(&self, key: &str) -> String {
        if self.prefix.is_empty() {
            key.into()
        } else {
            format!("{}/{}", self.prefix, key)
        }
    }

    fn relative_key(&self, key: &str) -> String {
        if self.prefix.is_empty() {
            key.into()
        } else {
            key.get(self.prefix.len() + 1..).unwrap_or_default().into()
        }
    }

    fn request(
        &self,
        method: &str,
        key: Option<&str>,
        query: &[(&str, &str)],
        body: &[u8],
        operation: &str,
    ) -> Result<Response, StoreError> {
        let credentials = Credentials::load()
            .ok_or_else(|| StoreError::Unreachable("Unable to locate credentials".into()))?;
        let region = region();
        let target = Target::new(&self.bucket, key, &region);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let extra: &[(&str, &str)] = if method == "PUT" {
            &[("content-type", "application/gzip")]
        } else {
            &[]
        };
        let query_string = canonical_query(query);
        let headers = sign(
            method,
            &target,
            &query_string,
            extra,
            body,
            &credentials,
            &region,
            now,
        );
        let url = if query_string.is_empty() {
            target.url()
        } else {
            format!("{}?{}", target.url(), query_string)
        };
        let mut request = ureq::AgentBuilder::new()
            .timeout(TIMEOUT)
            .build()
            .request(method, &url);
        for (name, value) in &headers {
            if name != "host" {
                request = request.set(name, value);
            }
        }
        let result = if method == "PUT" {
            request.send_bytes(body)
        } else {
            request.call()
        };
        let response = match result {
            Ok(response) => response,
            Err(ureq::Error::Status(_, response)) => response,
            Err(ureq::Error::Transport(_)) => {
                return Err(StoreError::Unreachable(format!(
                    "Could not connect to the endpoint URL: \"{}\"",
                    url
                )))
            }
        };
        let status = response.status();
        let mut data = Vec::new();
        let _ = response.into_reader().read_to_end(&mut data);
        if status >= 300 {
            let text = String::from_utf8_lossy(&data);
            let code = xml_values(&text, "Code")
                .into_iter()
                .next()
                .unwrap_or_else(|| status.to_string());
            if code == "NoSuchKey"
                || (status == 404 && operation == "GetObject" && !text.contains("NoSuchBucket"))
            {
                return Err(StoreError::Missing(format!(
                    "no object at {}",
                    key.map(|k| self.relative_key(k)).unwrap_or_default()
                )));
            }
            let message = xml_values(&text, "Message")
                .into_iter()
                .next()
                .unwrap_or_else(|| reason(status).into());
            return Err(StoreError::Unreachable(format!(
                "An error occurred ({}) when calling the {} operation: {}",
                code, operation, message
            )));
        }
        Ok(Response { status, body: data })
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        403 => "Forbidden",
        404 => "Not Found",
        _ => "Unknown",
    }
}

impl Store for S3Store {
    fn describe(&self) -> String {
        format!("s3://{}/{}", self.bucket, self.prefix)
    }

    fn put(&self, key: &str, data: &[u8]) -> Result<(), StoreError> {
        self.request("PUT", Some(&self.key_for(key)), &[], data, "PutObject")
            .map(|_| ())
    }

    fn get(&self, key: &str) -> Result<Vec<u8>, StoreError> {
        let full = self.key_for(key);
        match self.request("GET", Some(&full), &[], b"", "GetObject") {
            Err(StoreError::Missing(_)) => {
                Err(StoreError::Missing(format!("no object at {}", key)))
            }
            other => other.map(|r| r.body),
        }
    }

    /// Paginated: ListObjectsV2 truncates at 1000 keys.
    fn list(&self, prefix: &str) -> Result<Vec<String>, StoreError> {
        let full = self.key_for(prefix);
        let mut keys = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let mut query = vec![("list-type", "2"), ("prefix", full.as_str())];
            if let Some(token) = &token {
                query.push(("continuation-token", token.as_str()));
            }
            let response = self.request("GET", None, &query, b"", "ListObjectsV2")?;
            let text = String::from_utf8_lossy(&response.body).into_owned();
            let _ = response.status;
            for contents in xml_blocks(&text, "Contents") {
                for key in xml_values(contents, "Key") {
                    keys.push(self.relative_key(&key));
                }
            }
            let truncated =
                xml_values(&text, "IsTruncated").first().map(String::as_str) == Some("true");
            token = xml_values(&text, "NextContinuationToken")
                .into_iter()
                .next();
            if !truncated || token.is_none() {
                break;
            }
        }
        keys.sort_by(|a, b| crate::py::cmp(a, b));
        Ok(keys)
    }

    fn delete(&self, key: &str) -> Result<(), StoreError> {
        match self.request("DELETE", Some(&self.key_for(key)), &[], b"", "DeleteObject") {
            Ok(_) | Err(StoreError::Missing(_)) => Ok(()),
            Err(error) => Err(error),
        }
    }
}

/// The text of every `<tag>` element, entities decoded.
fn xml_values(text: &str, tag: &str) -> Vec<String> {
    xml_blocks(text, tag)
        .into_iter()
        .map(xml_unescape)
        .collect()
}

fn xml_blocks<'a>(text: &'a str, tag: &str) -> Vec<&'a str> {
    let open = format!("<{}>", tag);
    let close = format!("</{}>", tag);
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find(&open) {
        let after = &rest[start + open.len()..];
        let Some(end) = after.find(&close) else { break };
        out.push(&after[..end]);
        rest = &after[end + close.len()..];
    }
    out
}

fn xml_unescape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after = &rest[amp..];
        let Some(semi) = after.find(';') else {
            out.push_str(after);
            return out;
        };
        let entity = &after[1..semi];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => entity
                .strip_prefix("#x")
                .and_then(|h| u32::from_str_radix(h, 16).ok())
                .or_else(|| entity.strip_prefix('#').and_then(|d| d.parse().ok()))
                .and_then(char::from_u32),
        };
        match decoded {
            Some(c) => out.push(c),
            None => out.push_str(&after[..=semi]),
        }
        rest = &after[semi + 1..];
    }
    out.push_str(rest);
    out
}

struct Target {
    scheme: String,
    host: String,
    path: String,
}

impl Target {
    /// Path style under `LOCH_S3_ENDPOINT` — MinIO, R2 and Garage need it —
    /// and virtual-hosted against AWS, as boto3 addresses them. A bucket with
    /// a dot can't be a TLS hostname, so it goes path style there too.
    fn new(bucket: &str, key: Option<&str>, region: &str) -> Target {
        let encoded = key.map(|k| format!("/{}", uri_encode(k, false)));
        match std::env::var("LOCH_S3_ENDPOINT")
            .ok()
            .filter(|e| !e.is_empty())
        {
            Some(endpoint) => {
                let (scheme, rest) = endpoint.split_once("://").unwrap_or(("https", &endpoint));
                let (host, base) = match rest.find('/') {
                    Some(slash) => (&rest[..slash], rest[slash..].trim_end_matches('/')),
                    None => (rest, ""),
                };
                Target {
                    scheme: scheme.into(),
                    host: host.into(),
                    path: format!(
                        "{}/{}{}",
                        base,
                        uri_encode(bucket, true),
                        encoded.unwrap_or_default()
                    ),
                }
            }
            None if bucket.contains('.') => Target {
                scheme: "https".into(),
                host: format!("s3.{}.amazonaws.com", region),
                path: format!("/{}{}", bucket, encoded.unwrap_or_default()),
            },
            None => Target {
                scheme: "https".into(),
                host: format!("{}.s3.{}.amazonaws.com", bucket, region),
                path: encoded.unwrap_or_else(|| "/".into()),
            },
        }
    }

    fn url(&self) -> String {
        format!("{}://{}{}", self.scheme, self.host, self.path)
    }
}

/// RFC 3986 unreserved characters pass and everything else is
/// percent-encoded; `/` passes in a path and is encoded in a query value.
fn uri_encode(text: &str, encode_slash: bool) -> String {
    let mut out = String::new();
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            b'/' if !encode_slash => out.push('/'),
            _ => out.push_str(&format!("%{:02X}", byte)),
        }
    }
    out
}

fn canonical_query(query: &[(&str, &str)]) -> String {
    let mut pairs: Vec<(String, String)> = query
        .iter()
        .map(|(k, v)| (uri_encode(k, true), uri_encode(v, true)))
        .collect();
    pairs.sort();
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", k, v))
        .collect::<Vec<_>>()
        .join("&")
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

fn hmac(key: &[u8], data: &str) -> Vec<u8> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC takes any key length");
    mac.update(data.as_bytes());
    mac.finalize().into_bytes().to_vec()
}

/// The headers of a SigV4-signed request, `authorization` among them.
#[allow(clippy::too_many_arguments)]
fn sign(
    method: &str,
    target: &Target,
    query: &str,
    extra: &[(&str, &str)],
    body: &[u8],
    credentials: &Credentials,
    region: &str,
    now: u64,
) -> Vec<(String, String)> {
    let stamp = amz_date(now);
    let day = &stamp[..8];
    let payload = hex(&Sha256::digest(body));
    let mut headers: Vec<(String, String)> = vec![
        ("host".into(), target.host.clone()),
        ("x-amz-content-sha256".into(), payload.clone()),
        ("x-amz-date".into(), stamp.clone()),
    ];
    for (name, value) in extra {
        headers.push((name.to_string(), value.to_string()));
    }
    if let Some(token) = &credentials.session_token {
        headers.push(("x-amz-security-token".into(), token.clone()));
    }
    headers.sort();
    let canonical_headers: String = headers
        .iter()
        .map(|(name, value)| format!("{}:{}\n", name, value.trim()))
        .collect();
    let signed: String = headers
        .iter()
        .map(|(name, _)| name.as_str())
        .collect::<Vec<_>>()
        .join(";");
    let canonical = format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        method, target.path, query, canonical_headers, signed, payload
    );
    let scope = format!("{}/{}/s3/aws4_request", day, region);
    let to_sign = format!(
        "AWS4-HMAC-SHA256\n{}\n{}\n{}",
        stamp,
        scope,
        hex(&Sha256::digest(canonical.as_bytes()))
    );
    let mut key = hmac(format!("AWS4{}", credentials.secret_key).as_bytes(), day);
    for part in [region, "s3", "aws4_request"] {
        key = hmac(&key, part);
    }
    let signature = hex(&hmac(&key, &to_sign));
    headers.push((
        "authorization".into(),
        format!(
            "AWS4-HMAC-SHA256 Credential={}/{}, SignedHeaders={}, Signature={}",
            credentials.access_key, scope, signed, signature
        ),
    ));
    headers
}

/// `YYYYMMDDTHHMMSSZ` from Unix seconds.
fn amz_date(seconds: u64) -> String {
    let c = crate::sys::civil(seconds as i64);
    format!(
        "{:04}{:02}{:02}T{:02}{:02}{:02}Z",
        c.year, c.month, c.day, c.hour, c.minute, c.second
    )
}

struct Credentials {
    access_key: String,
    secret_key: String,
    session_token: Option<String>,
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

fn profile() -> String {
    env("AWS_PROFILE")
        .or_else(|| env("AWS_DEFAULT_PROFILE"))
        .unwrap_or_else(|| "default".into())
}

fn aws_file(variable: &str, name: &str) -> PathBuf {
    env(variable)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(home_dir()).join(".aws").join(name))
}

/// One section of an AWS INI file. The config file names non-default profiles
/// `[profile name]`; the credentials file names them `[name]`.
fn ini_section(path: &PathBuf, section: &str) -> HashMap<String, String> {
    let mut values = HashMap::new();
    let Ok(text) = fs::read_to_string(path) else {
        return values;
    };
    let mut inside = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') || line.starts_with(';') || line.is_empty() {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            inside = name.trim() == section;
            continue;
        }
        if inside {
            if let Some((key, value)) = line.split_once('=') {
                values.insert(key.trim().to_lowercase(), value.trim().to_string());
            }
        }
    }
    values
}

fn config_section() -> HashMap<String, String> {
    let name = profile();
    let section = if name == "default" {
        name
    } else {
        format!("profile {}", name)
    };
    ini_section(&aws_file("AWS_CONFIG_FILE", "config"), &section)
}

impl Credentials {
    fn load() -> Option<Credentials> {
        if let (Some(access_key), Some(secret_key)) =
            (env("AWS_ACCESS_KEY_ID"), env("AWS_SECRET_ACCESS_KEY"))
        {
            return Some(Credentials {
                access_key,
                secret_key,
                session_token: env("AWS_SESSION_TOKEN"),
            });
        }
        let shared = ini_section(
            &aws_file("AWS_SHARED_CREDENTIALS_FILE", "credentials"),
            &profile(),
        );
        for section in [shared, config_section()] {
            if let (Some(access_key), Some(secret_key)) = (
                section.get("aws_access_key_id"),
                section.get("aws_secret_access_key"),
            ) {
                return Some(Credentials {
                    access_key: access_key.clone(),
                    secret_key: secret_key.clone(),
                    session_token: section.get("aws_session_token").cloned(),
                });
            }
        }
        None
    }
}

/// `LOCH_S3_REGION`, then what boto3 would read, then us-east-1.
fn region() -> String {
    env("LOCH_S3_REGION")
        .or_else(|| env("AWS_REGION"))
        .or_else(|| env("AWS_DEFAULT_REGION"))
        .or_else(|| config_section().get("region").cloned())
        .unwrap_or_else(|| "us-east-1".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_are_civil() {
        assert_eq!(amz_date(0), "19700101T000000Z");
        assert_eq!(amz_date(951_782_400), "20000229T000000Z");
        assert_eq!(amz_date(1_794_947_157), "20261117T202557Z");
    }

    /// The worked example from AWS's SigV4 documentation for S3: a GET of
    /// /test.txt from examplebucket, with its published signature.
    #[test]
    fn signature_matches_the_aws_example() {
        let target = Target {
            scheme: "https".into(),
            host: "examplebucket.s3.amazonaws.com".into(),
            path: "/test.txt".into(),
        };
        let credentials = Credentials {
            access_key: "AKIAIOSFODNN7EXAMPLE".into(),
            secret_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".into(),
            session_token: None,
        };
        let headers = sign(
            "GET",
            &target,
            "",
            &[("range", "bytes=0-9")],
            b"",
            &credentials,
            "us-east-1",
            1_369_353_600,
        );
        let authorization = &headers
            .iter()
            .find(|(n, _)| n == "authorization")
            .unwrap()
            .1;
        assert_eq!(
            authorization,
            "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request, \
             SignedHeaders=host;range;x-amz-content-sha256;x-amz-date, \
             Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
        );
    }

    /// AWS's published example for a signed query string: a ListObjects of
    /// examplebucket with max-keys and prefix.
    #[test]
    fn query_signature_matches_the_aws_example() {
        let target = Target {
            scheme: "https".into(),
            host: "examplebucket.s3.amazonaws.com".into(),
            path: "/".into(),
        };
        let credentials = Credentials {
            access_key: "AKIAIOSFODNN7EXAMPLE".into(),
            secret_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".into(),
            session_token: None,
        };
        let query = canonical_query(&[("prefix", "J"), ("max-keys", "2")]);
        assert_eq!(query, "max-keys=2&prefix=J");
        let headers = sign(
            "GET",
            &target,
            &query,
            &[],
            b"",
            &credentials,
            "us-east-1",
            1_369_353_600,
        );
        let authorization = &headers
            .iter()
            .find(|(n, _)| n == "authorization")
            .unwrap()
            .1;
        assert!(authorization.ends_with(
            "Signature=34b48302e7b5fa45bde8084f4b7868a86f0a534bc59db6670ed5711ef69dc6f7"
        ));
    }

    #[test]
    fn keys_are_encoded_once() {
        assert_eq!(
            uri_encode("transcripts/a b/c+d.jsonl", false),
            "transcripts/a%20b/c%2Bd.jsonl"
        );
        assert_eq!(
            xml_unescape("a&amp;b&#x41;&#66;&bogus;"),
            "a&bA B&bogus;".replace(' ', "")
        );
    }
}
