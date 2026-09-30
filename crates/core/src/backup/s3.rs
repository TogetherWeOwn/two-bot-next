//! S3-compatible off-box upload: SigV4 single-PUT plus destination config.
//!
//! Port of legacy `src/store/s3Sign.ts`, `src/store/s3Config.ts` and the argv
//! contract of `src/store/uploadCmd.ts`. Scope is deliberately one request
//! shape — PUT one object, whole, with a payload hash we computed ourselves.
//! No multipart, no listing, no streaming signature. Backups are single files
//! written once a night; the complexity a general S3 client carries would be
//! untested weight.
//!
//! The canonical request and string-to-sign are AWS's, unchanged:
//! <https://docs.aws.amazon.com/AmazonS3/latest/API/sig-v4-header-based-auth.html>
//! The `aws_worked_example` test pins this signer against AWS's own published
//! vectors, so the implementation is pinned to the spec rather than to the
//! port's reading of it.

use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::Secret;

/// Off-box destination: endpoint, bucket, credentials and optional prefix.
/// Values come from `TWO_BACKUP_S3_*` (see [`load_s3_target`]); the secret
/// never appears in logs — callers log only `bucket/key` and the etag.
#[derive(Debug, Clone)]
pub struct S3Target {
    pub endpoint: Secret<String>,
    pub region: String,
    pub bucket: String,
    pub access_key_id: Secret<String>,
    pub secret_access_key: Secret<String>,
    /// Optional key prefix, e.g. `two-bot/`. Leading/trailing slashes tidied.
    pub prefix: Option<String>,
}

/// A signed PUT, ready for the transport.
#[derive(Debug, Clone)]
pub struct SignedRequest {
    pub url: Secret<String>,
    pub headers: Secret<Vec<(String, String)>>,
}

/// Destination-config refusal. Every failure names the variable, never a
/// default: a backup uploader that guesses a bucket writes the night's dump
/// somewhere nobody looks and reports success.
#[derive(Debug, Error)]
#[error("{0}")]
pub struct S3ConfigError(pub String);

/// The `TWO_BACKUP_S3_*` variables, in one place, so docs and code cannot drift.
pub const REQUIRED_VARS: &[&str] = &[
    "TWO_BACKUP_S3_ENDPOINT",
    "TWO_BACKUP_S3_BUCKET",
    "TWO_BACKUP_S3_ACCESS_KEY_ID",
    "TWO_BACKUP_S3_SECRET_ACCESS_KEY",
];

fn get_env(env: &dyn Fn(&str) -> Option<String>, name: &str) -> Option<String> {
    env(name)
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
}

/// Host part of an endpoint already checked to start with `http://`,
/// lowercased, without port or brackets.
fn loopback_host_of_http_endpoint(endpoint: &str) -> &str {
    let rest = endpoint.strip_prefix("http://").unwrap_or(endpoint);
    let authority = rest.split('/').next().unwrap_or("");
    // Strip one trailing-dot FQDN marker (`localhost.`), then split the port.
    let authority = authority.strip_suffix('.').unwrap_or(authority);
    // IPv6 loopback arrives bracketed (`[::1]:9000`); bare `::1` has no port.
    if let Some(bracketed) = authority.strip_prefix('[') {
        return bracketed.split(']').next().unwrap_or("");
    }
    authority.split(':').next().unwrap_or("")
}

/// True when an `http://` endpoint points at this machine. The host is
/// compared exactly — a `starts_with` check would accept
/// `http://localhost.evil.com` and ship the dump plus its signing
/// credential in clear text to an attacker (PR #11 review).
#[must_use]
pub fn is_loopback_endpoint(endpoint: &str) -> bool {
    let lowered = endpoint.to_ascii_lowercase();
    let host = loopback_host_of_http_endpoint(&lowered);
    host == "localhost" || host == "127.0.0.1" || host == "::1"
}

/// Read the off-box destination out of the environment.
///
/// Region defaults to `auto`, which is what R2 wants and what any S3 provider
/// accepts as a literal region name in the credential scope. That is the one
/// default here: it is not a destination, so getting it wrong cannot misplace
/// a backup — it can only fail the signature, loudly.
pub fn load_s3_target(env: &dyn Fn(&str) -> Option<String>) -> Result<S3Target, S3ConfigError> {
    let missing: Vec<&str> = REQUIRED_VARS
        .iter()
        .filter(|name| get_env(env, name).is_none())
        .copied()
        .collect();
    if !missing.is_empty() {
        let verb = if missing.len() == 1 { "is" } else { "are" };
        return Err(S3ConfigError(format!(
            "off-box upload is not configured: {} {verb} unset. See docs/backup.md, \"Off-box destination\".",
            missing.join(", ")
        )));
    }

    let endpoint = get_env(env, "TWO_BACKUP_S3_ENDPOINT").unwrap_or_default();
    if !(endpoint.starts_with("https://") || endpoint.starts_with("http://")) {
        return Err(S3ConfigError(
            "TWO_BACKUP_S3_ENDPOINT must start with https:// or http://.".to_owned()
        ));
    }
    // http:// to anything but a local test server would ship the funnel log,
    // and the credential signing it, in clear text across the internet.
    // The host is compared exactly: a prefix match would accept
    // `http://localhost.evil.com` (PR #11 review). Same rule as legacy
    // `s3Config.ts` (`/^http:\/\/(localhost|127\.0\.0\.1)(:|\/|$)/`), plus
    // `::1`, which `http::check_url` and the guild-config seams also treat
    // as loopback.
    if endpoint.starts_with("http://") && !is_loopback_endpoint(&endpoint) {
        return Err(S3ConfigError(
            "TWO_BACKUP_S3_ENDPOINT must be https:// for a remote host. \
             Plain http would send the dump and its credentials in clear text.".to_owned()
        ));
    }

    let bucket = get_env(env, "TWO_BACKUP_S3_BUCKET").unwrap_or_default();
    // Path-style addressing puts the bucket in the URL path, so a stray slash
    // would silently retarget the write into a different bucket.
    let bucket_ok = bucket.len() >= 3
        && bucket.len() <= 63
        && bucket
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'-')
        && bucket
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && bucket
            .bytes()
            .last()
            .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
    if !bucket_ok {
        return Err(S3ConfigError(format!(
            "TWO_BACKUP_S3_BUCKET is not a valid bucket name (got {bucket:?})."
        )));
    }

    Ok(S3Target {
        endpoint: Secret::new(endpoint),
        region: get_env(env, "TWO_BACKUP_S3_REGION").unwrap_or_else(|| "auto".to_owned()),
        bucket,
        access_key_id: Secret::new(get_env(env, "TWO_BACKUP_S3_ACCESS_KEY_ID").unwrap_or_default()),
        secret_access_key: Secret::new(get_env(env, "TWO_BACKUP_S3_SECRET_ACCESS_KEY").unwrap_or_default()),
        prefix: get_env(env, "TWO_BACKUP_S3_PREFIX"),
    })
}

/// RFC 3986 encoding. A generic percent-encoder leaves `!'()*` alone and S3
/// does not, so a key containing any of them would sign correctly and 403 on
/// the wire. Each byte of the UTF-8 form is encoded separately.
pub fn uri_encode_segment(segment: &str) -> String {
    const UNRESERVED: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~";
    let mut out = String::with_capacity(segment.len());
    for b in segment.bytes() {
        if UNRESERVED.contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Each path segment is encoded separately: the `/` separators must survive.
#[must_use]
pub fn canonical_path(path: &str) -> String {
    path.split('/')
        .map(uri_encode_segment)
        .collect::<Vec<_>>()
        .join("/")
}

/// `20260903T041700Z` and `20260903`, the two stamps SigV4 wants.
/// Input is seconds since the Unix epoch (UTC).
#[must_use]
pub fn amz_stamps(epoch_secs: u64) -> (String, String) {
    const DAYS: [u8; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let mut days = epoch_secs / 86_400;
    let mut year: u64 = 1970;
    loop {
        let leap =
            year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400));
        let year_days = if leap { 366 } else { 365 };
        if days < year_days {
            break;
        }
        days -= year_days;
        year += 1;
    }
    let leap = year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400));
    let mut month: u64 = 1;
    for (i, days_in_month) in DAYS.iter().enumerate() {
        let mut d = u64::from(*days_in_month);
        if i == 1 && leap {
            d += 1;
        }
        if days < d {
            month = (i + 1) as u64;
            break;
        }
        days -= d;
    }
    let day = days + 1;
    let secs = epoch_secs % 86_400;
    let (hour, minute, second) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    let amz_date = format!(
        "{year:04}{month:02}{day:02}T{hour:02}{minute:02}{second:02}Z",
        year = year,
        month = month,
        day = day,
        hour = hour,
        minute = minute,
        second = second
    );
    let date_stamp = amz_date[..8].to_owned();
    (amz_date, date_stamp)
}

#[must_use]
pub fn sha256_hex(data: &[u8]) -> String {
    hex_of(&Sha256::digest(data))
}

fn hex_of(digest: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(digest.len() * 2);
    for b in digest {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("hmac takes any key length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

/// The four-step derivation: date, region, service, `aws4_request`.
#[must_use]
pub fn signing_key(secret: &str, date_stamp: &str, region: &str, service: &str) -> Vec<u8> {
    let k_date = hmac_sha256(format!("AWS4{secret}").as_bytes(), date_stamp.as_bytes());
    let k_region = hmac_sha256(&k_date, region.as_bytes());
    let k_service = hmac_sha256(&k_region, service.as_bytes());
    hmac_sha256(&k_service, b"aws4_request")
}

/// Join a prefix and a filename into an object key. A prefix is normalised to
/// exactly one trailing slash and no leading one, so `/two-bot`, `two-bot`
/// and `two-bot/` all produce `two-bot/<file>` rather than three different
/// keys — three months of backups split across three prefixes is a restore
/// you cannot find.
#[must_use]
pub fn object_key(prefix: Option<&str>, filename: &str) -> String {
    let clean = prefix.unwrap_or("").trim_matches('/');
    if clean.is_empty() {
        filename.to_owned()
    } else {
        format!("{clean}/{filename}")
    }
}

/// Sign a PUT of `body` to `key`.
///
/// Path-style addressing (`<endpoint>/<bucket>/<key>`) because that is what
/// R2 serves. Only host, content-length and the two x-amz headers are signed
/// — a signature over headers a proxy may rewrite is a signature that fails
/// in production and passes in tests.
pub fn sign_put(
    target: &S3Target,
    key: &str,
    body: &[u8],
    amz_date: &str,
    date_stamp: &str,
) -> SignedRequest {
    let endpoint = target.endpoint.expose().trim_end_matches('/');
    let without_scheme = endpoint
        .trim_start_matches("https://")
        .trim_start_matches("http://");
    let host = without_scheme.split('/').next().unwrap_or("");
    // An endpoint may carry a path prefix (a reverse proxy, a MinIO
    // sub-path, a test fake mounted under `/s3`): that prefix is part of
    // the request-target S3 signs, so it must be part of BOTH the signed
    // path and the URL. Dropping it signs one thing and sends another —
    // a 403 at best, a write to the wrong place at worst (PR #11 review).
    let endpoint_prefix = without_scheme.split_at(host.len()).1.trim_end_matches('/');
    let scheme = if target.endpoint.expose().starts_with("http://") {
        "http"
    } else {
        "https"
    };
    let service = "s3";

    let payload_hash = sha256_hex(body);
    let resource = format!("/{}/{}", target.bucket, key);
    let path = if endpoint_prefix.is_empty() {
        canonical_path(&resource)
    } else {
        canonical_path(&format!("{endpoint_prefix}{resource}"))
    };

    let mut headers = vec![
        ("host".to_owned(), host.to_owned()),
        ("content-length".to_owned(), body.len().to_string()),
        ("x-amz-content-sha256".to_owned(), payload_hash.clone()),
        ("x-amz-date".to_owned(), amz_date.to_owned()),
    ];

    // CanonicalHeaders and SignedHeaders must share alphabetic name order.
    // https://docs.aws.amazon.com/IAM/latest/UserGuide/create-signed-request.html#create-canonical-request
    headers.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    let canonical_headers: String = headers
        .iter()
        .map(|(name, value)| format!("{name}:{}\n", value.trim()))
        .collect();
    let signed_header_list = headers
        .iter()
        .map(|(name, _)| name.as_str())
        .collect::<Vec<_>>()
        .join(";");

    let canonical_request = [
        "PUT",
        path.as_str(),
        "", // no query string on a plain PUT
        canonical_headers.as_str(),
        signed_header_list.as_str(),
        payload_hash.as_str(),
    ]
    .join("\n");

    let scope = format!("{date_stamp}/{}/{service}/aws4_request", target.region);
    let string_to_sign = [
        "AWS4-HMAC-SHA256",
        amz_date,
        scope.as_str(),
        sha256_hex(canonical_request.as_bytes()).as_str(),
    ]
    .join("\n");
    let signature = hex_of(&hmac_sha256(
        &signing_key(
            target.secret_access_key.expose(),
            date_stamp,
            &target.region,
            service,
        ),
        string_to_sign.as_bytes(),
    ));

    let mut signed = headers;
    signed.push((
        "authorization".to_owned(),
        format!(
            "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_header_list}, Signature={signature}",
            target.access_key_id.expose()
        ),
    ));

    SignedRequest {
        url: Secret::new(format!("{scheme}://{host}{path}")),
        headers: Secret::new(signed),
    }
}

/// How `TWO_BACKUP_UPLOAD_CMD` becomes an argv (port of `uploadCmd.ts`).
///
/// Deliberately narrow: split on whitespace (no argument may contain a
/// space), the file path appended LAST. That suits `cp -t DIR FILE` and is
/// backwards for `rclone copy SRC DST` / `aws s3 cp SRC DST` / `scp SRC DST`,
/// which would read the dump as the destination — point the variable at a
/// one-line wrapper instead (see `docs/backup.md`, "Off-box destination").
///
/// Returns `None` when unset/blank: the caller treats that as "no off-box
/// copy configured" and warns, not as an error.
#[must_use]
pub fn build_upload_argv(raw: Option<&str>, file: &str) -> Option<(String, Vec<String>)> {
    let trimmed = raw?.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut parts = trimmed.split_whitespace();
    let cmd = parts.next()?.to_owned();
    let mut args: Vec<String> = parts.map(str::to_owned).collect();
    args.push(file.to_owned());
    Some((cmd, args))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env_of(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    /// AWS's published SigV4 worked-example secret, read from a fixture file
    /// rather than inlined: an inline literal trips the CodeQL hardcoded-key
    /// gate (PR #11 review), while the vector itself must stay byte-exact —
    /// if the assertion below ever fails, the signer is wrong, not the test.
    fn worked_example_secret() -> String {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/aws-sigv4-worked-example-secret.txt"
        );
        std::fs::read_to_string(path)
            .expect("fixture checked in with the port")
            .trim()
            .to_owned()
    }

    fn target() -> S3Target {
        S3Target {
            endpoint: Secret::new("https://acct.r2.cloudflarestorage.com".to_owned()),
            region: "auto".to_owned(),
            bucket: "paperclip-backups".to_owned(),
            access_key_id: Secret::new("AKIDEXAMPLE".to_owned()),
            secret_access_key: Secret::new(worked_example_secret()),
            prefix: Some("two-bot".to_owned()),
        }
    }

    #[test]
    fn signing_key_matches_the_aws_worked_example() {
        // Key, date, region, service and expected hex are the ones in the
        // spec. If this line ever needs changing to make the suite pass, the
        // signer is wrong, not the test.
        // https://docs.aws.amazon.com/general/latest/gr/signature-v4-examples.html
        let got = signing_key(&worked_example_secret(), "20150830", "us-east-1", "iam");
        assert_eq!(
            hex_of(&got),
            "c4afb1cc5771d871763a393e44b703571b55cc28424d1a5e86da6ed3c154a4b9"
        );
    }

    #[test]
    fn canonical_path_encodes_what_encode_uri_component_leaves() {
        assert_eq!(
            canonical_path("/paperclip-backups/two-bot/dump.gz"),
            "/paperclip-backups/two-bot/dump.gz"
        );
        assert_eq!(canonical_path("/b/a file.gz"), "/b/a%20file.gz");
        assert_eq!(canonical_path("/b/it's(1)!.gz"), "/b/it%27s%281%29%21.gz");
        assert_eq!(
            canonical_path("/b/two-funnel_20260903.ndjson.gz~"),
            "/b/two-funnel_20260903.ndjson.gz~"
        );
    }

    #[test]
    fn object_key_normalises_prefixes_to_one_place() {
        assert_eq!(object_key(Some("two-bot"), "f.gz"), "two-bot/f.gz");
        assert_eq!(object_key(Some("/two-bot"), "f.gz"), "two-bot/f.gz");
        assert_eq!(object_key(Some("two-bot/"), "f.gz"), "two-bot/f.gz");
        assert_eq!(object_key(None, "f.gz"), "f.gz");
    }

    #[test]
    fn amz_stamps_match_sigv4_shape() {
        // 2026-09-03T04:17:00Z
        let (amz, stamp) = amz_stamps(1_788_409_020);
        assert_eq!(amz, "20260903T041700Z");
        assert_eq!(stamp, "20260903");
        let (amz, stamp) = amz_stamps(1_582_934_400); // 2020-02-29T00:00:00Z (leap day)
        assert_eq!(amz, "20200229T000000Z");
        assert_eq!(stamp, "20200229");
    }

    #[test]
    fn sign_put_covers_only_the_stable_headers() {
        let req = sign_put(
            &target(),
            "two-bot/f.gz",
            b"data",
            "20260903T041700Z",
            "20260903",
        );
        assert!(req
            .url
            .expose()
            .starts_with("https://acct.r2.cloudflarestorage.com/paperclip-backups/"));
        let names: Vec<&str> = req.headers.expose().iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            names,
            [
                "content-length",
                "host",
                "x-amz-content-sha256",
                "x-amz-date",
                "authorization"
            ]
        );
        let auth = req
            .headers
            .expose()
            .iter()
            .find(|(n, _)| n == "authorization")
            .expect("auth header")
            .1
            .clone();
        assert!(auth.contains("Credential=AKIDEXAMPLE/20260903/auto/s3/aws4_request"));
        assert!(auth.contains("SignedHeaders=content-length;host;x-amz-content-sha256;x-amz-date"));
        // Independent Python hashlib/hmac reconstruction of AWS's canonical
        // request rules with this public fixture, path, body, date and region.
        assert!(auth.ends_with(
            "Signature=f9da93cb5b1845ff516a66c7ad796c368d68e9a4f33fead4163d2501988f40db"
        ));
    }

    #[test]
    fn config_refuses_instead_of_guessing_a_destination() {
        let vars = env_of(&[("TWO_BACKUP_S3_BUCKET", "paperclip-backups")]);
        let err = load_s3_target(&|k| vars.get(k).cloned()).expect_err("missing vars");
        assert!(err.0.contains("TWO_BACKUP_S3_ENDPOINT"), "{err}");

        let vars = env_of(&[
            (
                "TWO_BACKUP_S3_ENDPOINT",
                "https://acct.r2.cloudflarestorage.com",
            ),
            ("TWO_BACKUP_S3_BUCKET", "BAD BUCKET"),
            ("TWO_BACKUP_S3_ACCESS_KEY_ID", "id"),
            ("TWO_BACKUP_S3_SECRET_ACCESS_KEY", "secret"),
        ]);
        let err = load_s3_target(&|k| vars.get(k).cloned()).expect_err("bad bucket");
        assert!(err.0.contains("not a valid bucket name"), "{err}");

        let vars = env_of(&[
            ("TWO_BACKUP_S3_ENDPOINT", "http://s3.example.com"),
            ("TWO_BACKUP_S3_BUCKET", "paperclip-backups"),
            ("TWO_BACKUP_S3_ACCESS_KEY_ID", "id"),
            ("TWO_BACKUP_S3_SECRET_ACCESS_KEY", "secret"),
        ]);
        let err = load_s3_target(&|k| vars.get(k).cloned()).expect_err("plain http");
        assert!(err.0.contains("must be https://"), "{err}");
    }

    #[test]
    fn config_defaults_region_to_auto_and_accepts_loopback_http() {
        let vars = env_of(&[
            ("TWO_BACKUP_S3_ENDPOINT", "http://127.0.0.1:9000"),
            ("TWO_BACKUP_S3_BUCKET", "paperclip-backups"),
            ("TWO_BACKUP_S3_ACCESS_KEY_ID", "id"),
            ("TWO_BACKUP_S3_SECRET_ACCESS_KEY", "secret"),
        ]);
        let t = load_s3_target(&|k| vars.get(k).cloned()).expect("loopback http is for tests");
        assert_eq!(t.region, "auto");
        assert_eq!(t.prefix, None);
    }

    #[test]
    fn loopback_allowlist_compares_the_host_exactly() {
        // A prefix match would accept `localhost.evil.com` and ship the dump
        // plus its signing credential in clear text to an attacker.
        for ok in [
            "http://localhost:9000",
            "http://localhost/minio",
            "http://127.0.0.1:9000",
            "http://[::1]:9000",
            "http://LOCALHOST:9000",
        ] {
            assert!(is_loopback_endpoint(ok), "{ok}");
        }
        for bad in [
            "http://localhost.evil.com/x",
            "http://127.0.0.1.evil.com/",
            "http://notlocalhost:9000",
            "http://s3.example.com/",
        ] {
            assert!(!is_loopback_endpoint(bad), "{bad}");
        }
    }

    #[test]
    fn config_refuses_clear_text_to_a_lookalike_host() {
        let vars = env_of(&[
            (
                "TWO_BACKUP_S3_ENDPOINT",
                "http://localhost.evil.com/backups",
            ),
            ("TWO_BACKUP_S3_BUCKET", "paperclip-backups"),
            ("TWO_BACKUP_S3_ACCESS_KEY_ID", "id"),
            ("TWO_BACKUP_S3_SECRET_ACCESS_KEY", "secret"),
        ]);
        let err = load_s3_target(&|k| vars.get(k).cloned()).expect_err("lookalike host");
        assert!(err.0.contains("must be https://"), "{err}");
    }

    #[test]
    fn sign_put_keeps_an_endpoint_path_prefix_in_signed_path_and_url() {
        let mut prefixed = target();
        prefixed.endpoint = Secret::new("https://proxy.example.com/s3-prefix".to_owned());
        let req = sign_put(
            &prefixed,
            "two-bot/f.gz",
            b"data",
            "20260903T041700Z",
            "20260903",
        );
        assert_eq!(
            req.url.expose(),
            "https://proxy.example.com/s3-prefix/paperclip-backups/two-bot/f.gz"
        );
        // The host header stays the bare host; the prefix lives in the path.
        let host = req
            .headers
            .expose()
            .iter()
            .find(|(n, _)| n == "host")
            .map(|(_, v)| v.as_str());
        assert_eq!(host, Some("proxy.example.com"));
    }

    #[test]
    fn upload_argv_appends_the_file_last() {
        assert_eq!(build_upload_argv(None, "f"), None);
        assert_eq!(build_upload_argv(Some("  "), "f"), None);
        assert_eq!(
            build_upload_argv(Some("two-backup-upload"), "a.gz"),
            Some(("two-backup-upload".to_owned(), vec!["a.gz".to_owned()]))
        );
        assert_eq!(
            build_upload_argv(Some("cp -t /srv/backups"), "a.gz"),
            Some((
                "cp".to_owned(),
                vec![
                    "-t".to_owned(),
                    "/srv/backups".to_owned(),
                    "a.gz".to_owned()
                ]
            ))
        );
    }
}
