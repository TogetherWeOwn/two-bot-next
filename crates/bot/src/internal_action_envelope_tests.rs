//! Acceptance: hold the internal-action pre-auth envelope under hostile
//! headers.
//!
//! BUILDER CONTRACT: this file is frozen. Build on the test card's branch,
//! keep every test here verbatim, and make the suite green by changing
//! production code only (`wire_headers` / `one_header` in
//! `internal_action_http.rs`). Do not edit these tests, the hostile samples,
//! or the assertions: they are the spec. Corrections return to the test card.
//!
//! The envelope under test is the pre-auth front door: before any signature
//! is checked, a request must carry at most [`MAX_HEADERS`] headers totalling
//! at most [`MAX_HEADER_BYTES`] name+value bytes, exactly one value for every
//! inspected header, no value over its per-header maximum, an exact JSON
//! media type with no content-encoding, and a canonical content-length within
//! the body limit. Every refusal uses the existing fixed vocabulary
//! (`malformed` / `unauthorized`, non-retryable); no refusal may echo header
//! content. Happy-path single-header requests pass byte-identical.
//!
//! Suite map (all offline, unit-level against `wire_headers`/`one_header`):
//!
//! - Happy paths (green now): canonical single headers pass byte-identical;
//!   media-type case/charset variants, content-length at the limit or absent,
//!   at-max single values, exactly [`MAX_HEADERS`] headers and exactly
//!   [`MAX_HEADER_BYTES`] bytes all pass.
//! - Refusal pins (green now): duplicated headers (including under a
//!   different name case, and duplicated content-length whose values agree),
//!   over-max single values, over-count (distinct names or repeated appends
//!   of one name), over-byte totals (one padding header or many small ones),
//!   wrong/missing media types, any present content-encoding, over-limit or
//!   non-numeric content-length, missing/empty auth headers, and non-ASCII
//!   values are each refused with the fixed status for their class.
//! - RED pins (fail now, the builder's work):
//!   - a content-length with a leading `+` is accepted by `usize` parsing
//!     today; the field grammar is digits only, so it must be refused;
//!   - a whitespace-only `x-two-*` auth header counts as present today; a
//!     blank value is a missing header and must share the existing
//!     empty-header refusal (`unauthorized`).

use super::*;

/// Synthetic signature at its 71-byte maximum: `sha256=` plus 64 fixed hex
/// digits. Low-entropy fixture text, never a credential.
fn signature() -> String {
    format!("sha256={}", "a".repeat(64))
}

/// The smallest complete pre-auth header set: one of everything inspected.
fn happy_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    headers.insert("x-two-key-id", HeaderValue::from_static("key-1"));
    headers.insert("x-two-timestamp", HeaderValue::from_static("1700000000000"));
    headers.insert(
        "x-two-nonce",
        HeaderValue::from_str(&"n".repeat(32)).unwrap(),
    );
    headers.insert(
        "x-two-signature",
        HeaderValue::from_str(&signature()).unwrap(),
    );
    headers
}

fn set(headers: &mut HeaderMap, name: &'static str, value: &str) {
    headers.insert(name, HeaderValue::from_str(value).unwrap());
}

/// The envelope's byte accounting: header name bytes plus value bytes,
/// counted once per stored value.
fn byte_total(headers: &HeaderMap) -> usize {
    headers
        .iter()
        .map(|(name, value)| name.as_str().len() + value.as_bytes().len())
        .sum()
}

/// Pads with one extra header so the byte total lands exactly on `target`.
fn pad_to_byte_total(headers: &mut HeaderMap, target: usize) {
    let name = "x-padding";
    let fill = target - byte_total(headers) - name.len();
    headers.insert(name, HeaderValue::from_str(&"p".repeat(fill)).unwrap());
    assert_eq!(byte_total(headers), target);
}

fn refused(headers: &HeaderMap, status: StatusCode, code: &'static str) {
    let failure = match wire_headers(headers) {
        Ok(_) => panic!("hostile headers were admitted, expected {status}"),
        Err(failure) => failure,
    };
    assert_eq!(failure.status, status);
    assert_eq!(failure.code, code);
    assert_eq!(failure.class_code, ErrorCode::Malformed);
    assert!(!failure.retryable);
}

fn admitted(headers: &HeaderMap) -> WireHeaders<'_> {
    match wire_headers(headers) {
        Ok(wire) => wire,
        Err(failure) => panic!(
            "happy headers were refused with {} {}",
            failure.status, failure.code
        ),
    }
}

#[test]
fn happy_path_single_headers_pass_byte_identical() {
    let mut headers = happy_headers();
    set(&mut headers, "idempotency-key", "intent-1");
    let wire = admitted(&headers);
    assert_eq!(wire.auth.key_id, "key-1");
    assert_eq!(wire.auth.timestamp, "1700000000000");
    assert_eq!(wire.auth.nonce, "n".repeat(32));
    assert_eq!(wire.auth.signature, signature());
    assert_eq!(wire.idempotency, Some("intent-1"));

    let headers = happy_headers();
    let wire = admitted(&headers);
    assert_eq!(wire.idempotency, None);
}

#[test]
fn media_type_case_and_charset_variants_pass() {
    for media in [
        "APPLICATION/JSON",
        "application/json; charset=utf-8",
        "Application/Json; Charset=UTF-8",
    ] {
        let mut headers = happy_headers();
        headers.insert(header::CONTENT_TYPE, HeaderValue::from_str(media).unwrap());
        assert!(wire_headers(&headers).is_ok(), "{media}");
    }
}

#[test]
fn content_length_at_limit_absent_or_zero_padded_passes() {
    for length in [
        MAX_BODY_BYTES.to_string(),
        "0".to_owned(),
        "000128".to_owned(),
    ] {
        let mut headers = happy_headers();
        headers.insert(
            header::CONTENT_LENGTH,
            HeaderValue::from_str(&length).unwrap(),
        );
        assert!(wire_headers(&headers).is_ok(), "{length}");
    }
    assert!(wire_headers(&happy_headers()).is_ok());
}

#[test]
fn at_max_single_values_pass() {
    let mut headers = happy_headers();
    set(&mut headers, "x-two-key-id", &"k".repeat(128));
    set(&mut headers, "x-two-timestamp", &"1".repeat(15));
    set(&mut headers, "x-two-nonce", &"n".repeat(32));
    set(&mut headers, "x-two-signature", &signature());
    set(&mut headers, "idempotency-key", &"i".repeat(200));
    assert!(wire_headers(&headers).is_ok());
}

#[test]
fn exactly_max_headers_passes() {
    let mut headers = happy_headers();
    let mut index = 0;
    while headers.len() < MAX_HEADERS {
        let name = format!("x-filler-{index}");
        headers.insert(
            axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
            HeaderValue::from_static("v"),
        );
        index += 1;
    }
    assert_eq!(headers.len(), MAX_HEADERS);
    assert!(wire_headers(&headers).is_ok());
}

#[test]
fn exactly_max_header_bytes_passes() {
    let mut headers = happy_headers();
    pad_to_byte_total(&mut headers, MAX_HEADER_BYTES);
    assert!(wire_headers(&headers).is_ok());
}

#[test]
fn duplicated_inspected_headers_are_refused() {
    for name in [
        "x-two-key-id",
        "x-two-timestamp",
        "x-two-nonce",
        "x-two-signature",
        "idempotency-key",
        "content-type",
        "content-length",
    ] {
        let mut headers = happy_headers();
        set(&mut headers, "idempotency-key", "intent-1");
        headers.insert(header::CONTENT_LENGTH, HeaderValue::from_static("128"));
        let value = headers[name].clone();
        headers.append(name, value);
        refused(&headers, StatusCode::BAD_REQUEST, "malformed");
    }
}

#[test]
fn duplicated_header_under_different_name_case_is_refused() {
    let mut headers = happy_headers();
    let value = headers["x-two-nonce"].clone();
    headers.append("X-Two-Nonce", value);
    refused(&headers, StatusCode::BAD_REQUEST, "malformed");
}

#[test]
fn duplicated_content_length_is_refused_even_when_values_agree() {
    for second in ["128", "129"] {
        let mut headers = happy_headers();
        headers.append(header::CONTENT_LENGTH, HeaderValue::from_static("128"));
        headers.append(
            header::CONTENT_LENGTH,
            HeaderValue::from_str(second).unwrap(),
        );
        refused(&headers, StatusCode::BAD_REQUEST, "malformed");
    }
}

#[test]
fn over_max_single_values_are_refused() {
    for (name, value) in [
        ("x-two-key-id", "k".repeat(129)),
        ("x-two-timestamp", "1".repeat(16)),
        ("x-two-nonce", "n".repeat(33)),
        ("x-two-signature", format!("{}a", signature())),
        ("idempotency-key", "i".repeat(201)),
        ("content-type", "a".repeat(129)),
        ("content-length", "1".repeat(21)),
    ] {
        let mut headers = happy_headers();
        set(&mut headers, name, &value);
        refused(&headers, StatusCode::BAD_REQUEST, "malformed");
    }
}

#[test]
fn over_count_headers_are_refused() {
    let mut headers = happy_headers();
    let mut index = 0;
    while headers.len() < MAX_HEADERS + 1 {
        let name = format!("x-filler-{index}");
        headers.insert(
            axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
            HeaderValue::from_static("v"),
        );
        index += 1;
    }
    assert_eq!(headers.len(), MAX_HEADERS + 1);
    refused(
        &headers,
        StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE,
        "malformed",
    );

    // Repeated appends of one filler name count toward the same ceiling.
    let mut headers = happy_headers();
    while headers.len() < MAX_HEADERS + 1 {
        headers.append("x-filler", HeaderValue::from_static("v"));
    }
    refused(
        &headers,
        StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE,
        "malformed",
    );
}

#[test]
fn over_byte_total_is_refused() {
    let mut headers = happy_headers();
    pad_to_byte_total(&mut headers, MAX_HEADER_BYTES + 1);
    refused(
        &headers,
        StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE,
        "malformed",
    );

    // Many small headers accumulate past the byte ceiling while the count
    // stays under its own ceiling.
    let mut headers = happy_headers();
    let mut index = 0;
    while byte_total(&headers) <= MAX_HEADER_BYTES {
        let name = format!("x-chunk-{index}");
        headers.insert(
            axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
            HeaderValue::from_str(&"c".repeat(200)).unwrap(),
        );
        index += 1;
    }
    assert!(headers.len() <= MAX_HEADERS);
    refused(
        &headers,
        StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE,
        "malformed",
    );
}

#[test]
fn wrong_or_missing_media_types_are_refused() {
    for media in [
        "text/plain",
        "application/json-patch",
        "application/json; charset=ascii",
        "application/json; boundary=x",
        "application/json; charset=utf-8; x=1",
        "application/json;charset=utf-8",
        " application/json",
        "application/json ",
        "",
    ] {
        let mut headers = happy_headers();
        headers.insert(header::CONTENT_TYPE, HeaderValue::from_str(media).unwrap());
        refused(&headers, StatusCode::UNSUPPORTED_MEDIA_TYPE, "malformed");
    }

    let mut headers = happy_headers();
    headers.remove(header::CONTENT_TYPE);
    refused(&headers, StatusCode::UNSUPPORTED_MEDIA_TYPE, "malformed");
}

#[test]
fn present_content_encoding_is_refused() {
    for encoding in ["gzip", "identity", ""] {
        let mut headers = happy_headers();
        headers.insert(
            header::CONTENT_ENCODING,
            HeaderValue::from_str(encoding).unwrap(),
        );
        refused(&headers, StatusCode::UNSUPPORTED_MEDIA_TYPE, "malformed");
    }
}

#[test]
fn over_limit_or_non_numeric_content_length_is_refused() {
    for length in [
        (MAX_BODY_BYTES + 1).to_string(),
        "abc".to_owned(),
        "12, 12".to_owned(),
        "-1".to_owned(),
        String::new(),
        "9".repeat(20),
    ] {
        let mut headers = happy_headers();
        headers.insert(
            header::CONTENT_LENGTH,
            HeaderValue::from_str(&length).unwrap(),
        );
        refused(&headers, StatusCode::PAYLOAD_TOO_LARGE, "malformed");
    }
}

#[test]
fn missing_or_empty_auth_headers_are_refused() {
    for name in [
        "x-two-key-id",
        "x-two-timestamp",
        "x-two-nonce",
        "x-two-signature",
    ] {
        let mut headers = happy_headers();
        headers.remove(name);
        let failure = match wire_headers(&headers) {
            Ok(_) => panic!("missing {name} was admitted"),
            Err(failure) => failure,
        };
        assert_eq!(failure.status, StatusCode::UNAUTHORIZED, "{name}");
        assert_eq!(failure.code, "unauthorized", "{name}");
        assert_eq!(failure.class_code, ErrorCode::Unauthorized, "{name}");
        assert!(!failure.retryable, "{name}");

        let mut headers = happy_headers();
        set(&mut headers, name, "");
        let failure = match wire_headers(&headers) {
            Ok(_) => panic!("empty {name} was admitted"),
            Err(failure) => failure,
        };
        assert_eq!(failure.status, StatusCode::UNAUTHORIZED, "{name}");
        assert_eq!(failure.code, "unauthorized", "{name}");
    }
}

#[test]
fn non_ascii_auth_value_is_refused_without_echo() {
    let mut headers = happy_headers();
    headers.insert(
        "x-two-nonce",
        HeaderValue::from_bytes(&[0x80, 0x81, 0x82]).unwrap(),
    );
    refused(&headers, StatusCode::BAD_REQUEST, "malformed");
}

#[test]
fn one_header_pins_single_value_semantics() {
    let headers = happy_headers();
    assert!(matches!(
        one_header(&headers, "x-two-key-id", 128),
        Ok(Some("key-1"))
    ));
    assert!(matches!(one_header(&headers, "x-absent", 8), Ok(None)));
    assert!(one_header(&headers, "x-two-key-id", 4).is_err());

    let mut headers = happy_headers();
    let value = headers["x-two-key-id"].clone();
    headers.append("x-two-key-id", value);
    assert!(one_header(&headers, "x-two-key-id", 128).is_err());
}

/// RED: `Content-Length` is digits only on the wire. A leading `+` is not a
/// digit, so the envelope must refuse it with the same fixed vocabulary as
/// every other unusable content-length, even when the numeric value it names
/// would fit the body limit.
#[test]
fn content_length_with_leading_plus_is_refused() {
    for length in [
        "+0".to_owned(),
        "+128".to_owned(),
        format!("+{MAX_BODY_BYTES}"),
    ] {
        let mut headers = happy_headers();
        headers.insert(
            header::CONTENT_LENGTH,
            HeaderValue::from_str(&length).unwrap(),
        );
        refused(&headers, StatusCode::PAYLOAD_TOO_LARGE, "malformed");
    }
}

/// RED: a whitespace-only auth header carries no credential material. It is
/// a missing header wearing padding, so it must share the empty-header
/// refusal (`unauthorized`, non-retryable) instead of reaching signature
/// verification as if present.
#[test]
fn whitespace_only_auth_headers_are_refused_as_missing() {
    for name in [
        "x-two-key-id",
        "x-two-timestamp",
        "x-two-nonce",
        "x-two-signature",
    ] {
        for blank in [" ", "\t", " \t "] {
            let mut headers = happy_headers();
            set(&mut headers, name, blank);
            let failure = match wire_headers(&headers) {
                Ok(_) => panic!("whitespace-only {name} was admitted"),
                Err(failure) => failure,
            };
            assert_eq!(failure.status, StatusCode::UNAUTHORIZED, "{name}");
            assert_eq!(failure.code, "unauthorized", "{name}");
            assert_eq!(failure.class_code, ErrorCode::Unauthorized, "{name}");
            assert!(!failure.retryable, "{name}");
        }
    }
}
