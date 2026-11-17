//! The S3-compatible backend, signed with SigV4 by hand.
//!
//! An SDK would be the whole AWS credential chain in exchange for a static
//! binary several megabytes larger and an async runtime; the hook needs one
//! signed PUT. So the chain is the part people actually use against R2, B2,
//! MinIO, Garage and plain S3: the `AWS_*` variables, then the shared
//! credentials and config files for `AWS_PROFILE` (or `default`). SSO,
//! `credential_process`, assumed roles and instance metadata are not read;
//! boto3 reads them, so a machine relying on one of those needs the Python
//! build until they are.

use crate::claude::home_dir;
use crate::store::{Store, StoreError};
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// A hook has to come back before Claude Code gives up on it, so a store
/// that doesn't answer fails rather than hanging the turn.
const TIMEOUT: Duration = Duration::from_secs(20);

pub struct S3Store {
    bucket: String,
    prefix: String,
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
}

impl Store for S3Store {
    fn put(&self, key: &str, data: &[u8]) -> Result<(), StoreError> {
        let credentials = Credentials::load()
            .ok_or_else(|| StoreError::Unreachable("no AWS credentials found".into()))?;
        let region = region();
        let target = Target::new(&self.bucket, &self.key_for(key), &region);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let headers = sign(
            "PUT",
            &target,
            &[("content-type", "application/gzip")],
            data,
            &credentials,
            &region,
            now,
        );
        let mut request = ureq::AgentBuilder::new()
            .timeout(TIMEOUT)
            .build()
            .put(&target.url());
        for (name, value) in &headers {
            if name != "host" {
                request = request.set(name, value);
            }
        }
        request
            .send_bytes(data)
            .map(|_| ())
            .map_err(|error| StoreError::Unreachable(error.to_string()))
    }
}

struct Target {
    scheme: String,
    host: String,
    path: String,
}

impl Target {
    /// Path style under `LOCH_S3_ENDPOINT` — MinIO, R2 and Garage need it —
    /// and virtual-hosted against AWS, as boto3 addresses them. A bucket
    /// with a dot can't be a TLS hostname, so it goes path style there too.
    fn new(bucket: &str, key: &str, region: &str) -> Target {
        let encoded = uri_encode_path(key);
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
                    path: format!("{}/{}/{}", base, uri_encode_path(bucket), encoded),
                }
            }
            None if bucket.contains('.') => Target {
                scheme: "https".into(),
                host: format!("s3.{}.amazonaws.com", region),
                path: format!("/{}/{}", bucket, encoded),
            },
            None => Target {
                scheme: "https".into(),
                host: format!("{}.s3.{}.amazonaws.com", bucket, region),
                path: format!("/{}", encoded),
            },
        }
    }

    fn url(&self) -> String {
        format!("{}://{}{}", self.scheme, self.host, self.path)
    }
}

/// RFC 3986 unreserved characters pass, `/` separates segments, and
/// everything else is percent-encoded. S3 signs the path encoded once.
fn uri_encode_path(text: &str) -> String {
    let mut out = String::new();
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{:02X}", byte)),
        }
    }
    out
}

fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect::<Vec<_>>()
        .concat()
}

fn hmac(key: &[u8], data: &str) -> Vec<u8> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC takes any key length");
    mac.update(data.as_bytes());
    mac.finalize().into_bytes().to_vec()
}

/// The headers of a SigV4-signed request, `authorization` among them.
fn sign(
    method: &str,
    target: &Target,
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
        "{}\n{}\n\n{}\n{}\n{}",
        method, target.path, canonical_headers, signed, payload
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

/// `YYYYMMDDTHHMMSSZ` from Unix seconds, without a date library.
fn amz_date(seconds: u64) -> String {
    let days = (seconds / 86_400) as i64;
    let rest = seconds % 86_400;
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{:04}{:02}{:02}T{:02}{:02}{:02}Z",
        year,
        month,
        day,
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
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

/// One section of an AWS INI file. The config file names non-default
/// profiles `[profile name]`; the credentials file names them `[name]`.
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
        // 2013-05-24T00:00:00Z
        let headers = sign(
            "GET",
            &target,
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

    #[test]
    fn keys_are_encoded_once() {
        assert_eq!(
            uri_encode_path("transcripts/a b/c+d.jsonl"),
            "transcripts/a%20b/c%2Bd.jsonl"
        );
    }
}
