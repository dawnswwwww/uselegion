//! AWS Signature Version 4 request signing for the Bedrock provider.
//!
//! Pure functions only: no I/O, and the signing instant is passed in as a
//! parameter so tests can pin it to a known value.

use crate::types::ProviderError;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use std::fmt::Write as _;
use std::time::SystemTime;

/// AWS credentials used for SigV4 signing.
#[derive(Debug, Clone, PartialEq)]
pub struct AwsCreds {
    pub access_key: String,
    pub secret_key: String,
    pub session_token: Option<String>,
    pub region: String,
}

/// Sign a request and return the headers that must be attached to it:
/// `x-amz-date`, `authorization`, `x-amz-security-token` (when a session
/// token is present), and `x-amz-content-sha256` (required by Bedrock).
///
/// `url` is the full request URL, `headers` the headers the caller will send
/// (e.g. `content-type` / `accept`); `host` and `x-amz-date` are added here.
/// Only the URL path is used as the canonical URI and it is expected to
/// already be URI-safe (Bedrock model ids contain no characters that need
/// encoding). Query parameters, if any, are sorted lexicographically.
pub fn sign_request(
    method: &str,
    url: &str,
    headers: &[(String, String)],
    payload: &[u8],
    creds: &AwsCreds,
    service: &str,
    now: SystemTime,
) -> Result<Vec<(String, String)>, ProviderError> {
    let (host, path, query) = split_url(url)?;
    let (amz_date, date) = format_dates(now);
    let payload_hash = hex_encode(&Sha256::digest(payload));

    // Collect the headers that participate in the signature: caller headers
    // plus host / x-amz-date / x-amz-security-token, lowercased and sorted.
    let mut signed: Vec<(String, String)> = headers
        .iter()
        .map(|(k, v)| (k.to_lowercase(), v.trim().to_string()))
        .collect();
    signed.push(("host".to_string(), host));
    signed.push(("x-amz-date".to_string(), amz_date.clone()));
    if let Some(token) = &creds.session_token {
        signed.push(("x-amz-security-token".to_string(), token.clone()));
    }
    signed.sort_by(|a, b| a.0.cmp(&b.0));

    let (canonical_request, signed_headers) =
        build_canonical_request(method, &path, &query, &signed, &payload_hash);

    let scope = format!("{date}/{}/{service}/aws4_request", creds.region);
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        hex_encode(&Sha256::digest(canonical_request.as_bytes()))
    );

    let signing_key = derive_signing_key(&creds.secret_key, &date, &creds.region, service)?;
    let signature = hex_encode(&hmac_sha256(&signing_key, string_to_sign.as_bytes())?);

    let authorization = format!(
        "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
        creds.access_key
    );

    let mut out = vec![
        ("x-amz-date".to_string(), amz_date),
        ("authorization".to_string(), authorization),
    ];
    if let Some(token) = &creds.session_token {
        out.push(("x-amz-security-token".to_string(), token.clone()));
    }
    out.push(("x-amz-content-sha256".to_string(), payload_hash));
    Ok(out)
}

/// Split a full URL into (host, path, canonical query).
fn split_url(url: &str) -> Result<(String, String, String), ProviderError> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);
    if rest.is_empty() {
        return Err(ProviderError::InvalidAuth(
            "cannot sign request with empty URL".to_string(),
        ));
    }
    let (host, path_and_query) = rest.split_once('/').unwrap_or((rest, ""));
    let (path, query) = path_and_query
        .split_once('?')
        .map_or((path_and_query, ""), |(p, q)| (p, q));
    Ok((host.to_string(), format!("/{path}"), canonical_query(query)))
}

/// Sort query parameters lexicographically (keys are already URL-encoded in
/// the caller's URL).
fn canonical_query(raw: &str) -> String {
    if raw.is_empty() {
        return String::new();
    }
    let mut parts: Vec<&str> = raw.split('&').collect();
    parts.sort_unstable();
    parts.join("&")
}

/// Build the canonical request string and the signed-headers list.
///
/// `headers` must already be lowercased, trimmed, and sorted by name.
fn build_canonical_request(
    method: &str,
    path: &str,
    query: &str,
    headers: &[(String, String)],
    payload_hash: &str,
) -> (String, String) {
    let canonical_headers: String = headers.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
    let signed_headers = headers
        .iter()
        .map(|(k, _)| k.as_str())
        .collect::<Vec<_>>()
        .join(";");
    let canonical =
        format!("{method}\n{path}\n{query}\n{canonical_headers}\n{signed_headers}\n{payload_hash}");
    (canonical, signed_headers)
}

/// Derive the SigV4 signing key: HMAC chain date -> region -> service ->
/// "aws4_request" seeded with "AWS4" + secret key.
fn derive_signing_key(
    secret: &str,
    date: &str,
    region: &str,
    service: &str,
) -> Result<Vec<u8>, ProviderError> {
    let k_date = hmac_sha256(format!("AWS4{secret}").as_bytes(), date.as_bytes())?;
    let k_region = hmac_sha256(&k_date, region.as_bytes())?;
    let k_service = hmac_sha256(&k_region, service.as_bytes())?;
    hmac_sha256(&k_service, b"aws4_request")
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> Result<Vec<u8>, ProviderError> {
    // HMAC accepts keys of any length, so this cannot fail in practice; the
    // error is still mapped instead of unwrapped.
    let mut mac = Hmac::<Sha256>::new_from_slice(key)
        .map_err(|e| ProviderError::InvalidAuth(format!("hmac key rejected: {e}")))?;
    mac.update(data);
    Ok(mac.finalize().into_bytes().to_vec())
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// Format a `SystemTime` as (`YYYYMMDDTHHMMSSZ`, `YYYYMMDD`). Timestamps
/// before the Unix epoch are clamped to the epoch.
fn format_dates(now: SystemTime) -> (String, String) {
    let secs = now
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (year, month, day) = civil_from_days(days);
    let (hour, minute, second) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    (
        format!("{year:04}{month:02}{day:02}T{hour:02}{minute:02}{second:02}Z"),
        format!("{year:04}{month:02}{day:02}"),
    )
}

/// Convert days since the Unix epoch to a civil (year, month, day) date.
/// Howard Hinnant's `civil_from_days` algorithm, valid for all proleptic
/// Gregorian dates.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let year = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn at_epoch(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    fn example_creds() -> AwsCreds {
        AwsCreds {
            access_key: "AKIDEXAMPLE".to_string(),
            secret_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".to_string(),
            session_token: None,
            region: "us-east-1".to_string(),
        }
    }

    #[test]
    fn formats_known_epoch_timestamps() {
        assert_eq!(format_dates(at_epoch(0)).0, "19700101T000000Z");
        // 2015-08-30 12:36:00 UTC, the date used by AWS's own SigV4 examples.
        assert_eq!(format_dates(at_epoch(1_440_938_160)).0, "20150830T123600Z");
        // 2023-11-14 22:13:20 UTC.
        assert_eq!(format_dates(at_epoch(1_700_000_000)).0, "20231114T221320Z");
        assert_eq!(format_dates(at_epoch(1_700_000_000)).1, "20231114");
    }

    #[test]
    fn builds_canonical_request_exactly() {
        let headers = vec![
            (
                "accept".to_string(),
                "application/vnd.aws.eventstream".to_string(),
            ),
            ("content-type".to_string(), "application/json".to_string()),
            (
                "host".to_string(),
                "bedrock-runtime.us-east-1.amazonaws.com".to_string(),
            ),
            ("x-amz-date".to_string(), "20150830T123600Z".to_string()),
        ];
        let payload_hash = "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9";
        let (canonical, signed_headers) = build_canonical_request(
            "POST",
            "/model/anthropic.claude/converse-stream",
            "",
            &headers,
            payload_hash,
        );
        let expected = concat!(
            "POST\n",
            "/model/anthropic.claude/converse-stream\n",
            "\n",
            "accept:application/vnd.aws.eventstream\n",
            "content-type:application/json\n",
            "host:bedrock-runtime.us-east-1.amazonaws.com\n",
            "x-amz-date:20150830T123600Z\n",
            "\n",
            "accept;content-type;host;x-amz-date\n",
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9",
        );
        assert_eq!(canonical, expected);
        assert_eq!(signed_headers, "accept;content-type;host;x-amz-date");
    }

    #[test]
    fn produces_known_answer_signature() {
        // Expected values computed independently with Python's hashlib/hmac
        // for the canonical request asserted above (AWS example credentials,
        // 2015-08-30 12:36:00 UTC, payload "hello world").
        let headers = vec![
            ("content-type".to_string(), "application/json".to_string()),
            (
                "accept".to_string(),
                "application/vnd.aws.eventstream".to_string(),
            ),
        ];
        let signed = sign_request(
            "POST",
            "https://bedrock-runtime.us-east-1.amazonaws.com/model/anthropic.claude/converse-stream",
            &headers,
            b"hello world",
            &example_creds(),
            "bedrock",
            at_epoch(1_440_938_160),
        )
        .unwrap();

        let authorization = signed
            .iter()
            .find(|(k, _)| k == "authorization")
            .map(|(_, v)| v.clone())
            .unwrap();
        assert_eq!(
            authorization,
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/bedrock/aws4_request, \
             SignedHeaders=accept;content-type;host;x-amz-date, \
             Signature=cd979f97b6f99e273dc5536bc51089a45da7e59b757f7b1a8c665be8f33da037"
        );
        assert!(
            signed
                .iter()
                .any(|(k, v)| k == "x-amz-date" && v == "20150830T123600Z")
        );
        assert!(signed.iter().any(|(k, v)| {
            k == "x-amz-content-sha256"
                && v == "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        }));
        // No session token -> no security-token header.
        assert!(!signed.iter().any(|(k, _)| k == "x-amz-security-token"));
    }

    #[test]
    fn signing_is_deterministic() {
        let creds = example_creds();
        let first = sign_request(
            "POST",
            "https://h.example.com/x",
            &[],
            b"a",
            &creds,
            "bedrock",
            at_epoch(5),
        )
        .unwrap();
        let second = sign_request(
            "POST",
            "https://h.example.com/x",
            &[],
            b"a",
            &creds,
            "bedrock",
            at_epoch(5),
        )
        .unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn session_token_is_signed_and_attached() {
        let mut creds = example_creds();
        creds.session_token = Some("session-token".to_string());
        let signed = sign_request(
            "POST",
            "https://h.example.com/x",
            &[],
            b"",
            &creds,
            "bedrock",
            at_epoch(1_440_938_160),
        )
        .unwrap();
        assert!(
            signed
                .iter()
                .any(|(k, v)| k == "x-amz-security-token" && v == "session-token")
        );
        let authorization = signed
            .iter()
            .find(|(k, _)| k == "authorization")
            .map(|(_, v)| v.clone())
            .unwrap();
        assert!(authorization.contains("SignedHeaders=host;x-amz-date;x-amz-security-token"));
    }

    #[test]
    fn derives_signing_key_deterministically() {
        let first = derive_signing_key(
            "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            "20150830",
            "us-east-1",
            "bedrock",
        )
        .unwrap();
        let second = derive_signing_key(
            "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            "20150830",
            "us-east-1",
            "bedrock",
        )
        .unwrap();
        assert_eq!(first.len(), 32);
        assert_eq!(first, second);
        // The HMAC chain depends on every step: a different date changes the key.
        let other_date = derive_signing_key(
            "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            "20150831",
            "us-east-1",
            "bedrock",
        )
        .unwrap();
        assert_ne!(first, other_date);
    }

    #[test]
    fn sorts_query_parameters() {
        assert_eq!(canonical_query("b=2&a=1"), "a=1&b=2");
        assert_eq!(canonical_query(""), "");
    }
}
