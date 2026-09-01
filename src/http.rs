use std::{collections::BTreeSet, fmt::Write as _};

pub const MAX_OBJECT_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_HEADER_BYTES: usize = 32 * 1024;
pub const MAX_WIRE_OVERHEAD_BYTES: usize = 64 * 1024;
const MAX_S3_ERROR_BODY_BYTES: usize = 32 * 1024;
const MAX_QUERY_BYTES: usize = 8 * 1024;
const MAX_CHUNKS: usize = 4_096;
pub const MAX_LIST_KEYS: u32 = 1_000;
pub const MAX_LIST_PREFIX_BYTES: usize = 1_024;
pub const MAX_CONTINUATION_TOKEN_BYTES: usize = 2_048;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HttpErrorKind {
    InvalidRequest,
    Protocol,
    Denied,
    NotFound,
    Server,
    ClockSkew,
    Limit,
    Unsupported,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HttpError {
    pub kind: HttpErrorKind,
    pub status: Option<u16>,
    pub message: &'static str,
}

impl HttpError {
    const fn new(kind: HttpErrorKind, message: &'static str) -> Self {
        Self {
            kind,
            status: None,
            message,
        }
    }

    const fn with_status(kind: HttpErrorKind, status: u16, message: &'static str) -> Self {
        Self {
            kind,
            status: Some(status),
            message,
        }
    }
}

fn valid_endpoint(endpoint: &str) -> bool {
    !endpoint.is_empty()
        && endpoint.len() <= 128
        && endpoint
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn valid_bucket(bucket: &str) -> bool {
    (3..=63).contains(&bucket.len())
        && bucket.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'.')
        })
        && bucket
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
        && bucket
            .as_bytes()
            .last()
            .is_some_and(u8::is_ascii_alphanumeric)
        && !bucket.contains("..")
        && !bucket.contains(".-")
        && !bucket.contains("-.")
}

fn push_encoded_key(output: &mut String, key: &str) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    for byte in key.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~' | b'/') {
            output.push(char::from(byte));
        } else {
            output.push('%');
            output.push(char::from(HEX[usize::from(byte >> 4)]));
            output.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
    }
}

fn push_encoded_query_value(output: &mut String, value: &str) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            output.push(char::from(byte));
        } else {
            output.push('%');
            output.push(char::from(HEX[usize::from(byte >> 4)]));
            output.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
    }
}

/// Build the exact path-style S3 canonical URI supplied to the host signer.
///
/// The host validates but does not normalize these bytes. Object-key slashes
/// remain path separators; every other non-unreserved byte uses uppercase
/// RFC 3986 percent encoding.
fn build_canonical_uri_unbounded(bucket: &str, key: &str) -> Result<String, HttpError> {
    if !valid_bucket(bucket) || key.is_empty() || key.len() > 1_024 {
        return Err(HttpError::new(
            HttpErrorKind::InvalidRequest,
            "invalid S3 bucket or key",
        ));
    }

    let mut uri = String::with_capacity(bucket.len() + key.len() * 3 + 2);
    uri.push('/');
    uri.push_str(bucket);
    uri.push('/');
    push_encoded_key(&mut uri, key);
    Ok(uri)
}

pub fn build_canonical_uri(bucket: &str, key: &str, max_bytes: usize) -> Result<String, HttpError> {
    if max_bytes == 0 || max_bytes > MAX_OBJECT_BYTES {
        return Err(HttpError::new(
            HttpErrorKind::Limit,
            "requested object byte limit is outside the supported range",
        ));
    }
    build_canonical_uri_unbounded(bucket, key)
}

pub fn build_head_canonical_uri(bucket: &str, key: &str) -> Result<String, HttpError> {
    build_canonical_uri_unbounded(bucket, key)
}

/// Build the deliberately narrower canonical URI accepted by `SigV4` grants.
///
/// The host rejects encoded percent signs and backslashes as well as dot
/// segments. Reject them here too, before requesting signing authority. The
/// legacy anonymous and presigned paths retain their broader object-key
/// behavior through [`build_canonical_uri`].
pub fn build_sigv4_canonical_uri(
    bucket: &str,
    key: &str,
    max_bytes: usize,
) -> Result<String, HttpError> {
    if key.bytes().any(|byte| matches!(byte, b'%' | b'\\'))
        || key.split('/').any(|segment| matches!(segment, "." | ".."))
    {
        return Err(HttpError::new(
            HttpErrorKind::InvalidRequest,
            "object key is outside the supported SigV4 path shape",
        ));
    }
    build_canonical_uri(bucket, key, max_bytes)
}

pub fn build_sigv4_head_canonical_uri(bucket: &str, key: &str) -> Result<String, HttpError> {
    if key.bytes().any(|byte| matches!(byte, b'%' | b'\\'))
        || key.split('/').any(|segment| matches!(segment, "." | ".."))
    {
        return Err(HttpError::new(
            HttpErrorKind::InvalidRequest,
            "object key is outside the supported SigV4 path shape",
        ));
    }
    build_head_canonical_uri(bucket, key)
}

/// Build the exact path-style bucket URI used by `ListObjectsV2`.
pub fn build_list_canonical_uri(bucket: &str) -> Result<String, HttpError> {
    if !valid_bucket(bucket) {
        return Err(HttpError::new(
            HttpErrorKind::InvalidRequest,
            "invalid S3 bucket for object listing",
        ));
    }
    Ok(format!("/{bucket}/"))
}

fn validate_list_inputs(
    prefix: &str,
    max_keys: u32,
    continuation_token: Option<&str>,
) -> Result<(), HttpError> {
    if prefix.is_empty() || prefix.len() > MAX_LIST_PREFIX_BYTES {
        return Err(HttpError::new(
            HttpErrorKind::InvalidRequest,
            "S3 list prefix is outside the supported range",
        ));
    }
    if !(1..=MAX_LIST_KEYS).contains(&max_keys) {
        return Err(HttpError::new(
            HttpErrorKind::Limit,
            "S3 list max-keys is outside the supported range",
        ));
    }
    if continuation_token
        .is_some_and(|token| token.is_empty() || token.len() > MAX_CONTINUATION_TOKEN_BYTES)
    {
        return Err(HttpError::new(
            HttpErrorKind::Limit,
            "S3 continuation token is outside the supported range",
        ));
    }
    Ok(())
}

/// Build a strict RFC 3986 canonical `ListObjectsV2` query. Fields are emitted
/// in byte-sorted name order, including the caller's optional continuation.
pub fn build_list_query(
    prefix: &str,
    max_keys: u32,
    continuation_token: Option<&str>,
) -> Result<String, HttpError> {
    validate_list_inputs(prefix, max_keys, continuation_token)?;
    let mut query = String::with_capacity(prefix.len().saturating_mul(3).saturating_add(96));
    if let Some(token) = continuation_token {
        query.push_str("continuation-token=");
        push_encoded_query_value(&mut query, token);
        query.push('&');
    }
    write!(query, "list-type=2&max-keys={max_keys}&prefix=").map_err(|_error| {
        HttpError::new(
            HttpErrorKind::Limit,
            "S3 list query construction exceeded a fixed limit",
        )
    })?;
    push_encoded_query_value(&mut query, prefix);
    if query.len() > MAX_QUERY_BYTES {
        return Err(HttpError::new(
            HttpErrorKind::Limit,
            "S3 list query exceeds the byte limit",
        ));
    }
    Ok(query)
}

fn valid_presigned_query(query: &str) -> bool {
    !query.is_empty()
        && query.len() <= MAX_QUERY_BYTES
        && !query.starts_with('?')
        && query
            .bytes()
            .all(|byte| byte.is_ascii_graphic() && byte != b'#')
}

fn presigned_list_query_matches(
    query: &str,
    prefix: &str,
    max_keys: u32,
    continuation_token: Option<&str>,
) -> bool {
    if !valid_presigned_query(query)
        || validate_list_inputs(prefix, max_keys, continuation_token).is_err()
    {
        return false;
    }
    let mut fields = Vec::new();
    let mut names = BTreeSet::new();
    for pair in query.split('&') {
        let Some((name, value)) = pair.split_once('=') else {
            return false;
        };
        if name.is_empty() || !names.insert(name) {
            return false;
        }
        fields.push((name, value));
    }
    let lookup = |name: &str| {
        fields
            .iter()
            .find_map(|(field, value)| (*field == name).then_some(*value))
    };

    let mut encoded_prefix = String::new();
    push_encoded_query_value(&mut encoded_prefix, prefix);
    let expected_max_keys = max_keys.to_string();
    if lookup("list-type") != Some("2")
        || lookup("max-keys") != Some(expected_max_keys.as_str())
        || lookup("prefix") != Some(encoded_prefix.as_str())
    {
        return false;
    }
    continuation_token.map_or_else(
        || lookup("continuation-token").is_none(),
        |token| {
            let mut encoded = String::new();
            push_encoded_query_value(&mut encoded, token);
            lookup("continuation-token") == Some(encoded.as_str())
        },
    )
}

fn valid_port(port: &str) -> bool {
    !port.is_empty()
        && port.bytes().all(|byte| byte.is_ascii_digit())
        && port.parse::<u16>().is_ok_and(|value| value != 0)
}

fn valid_presigned_authority(authority: &str) -> bool {
    if authority.is_empty() || authority.len() > 255 || !authority.is_ascii() {
        return false;
    }

    if let Some(bracketed) = authority.strip_prefix('[') {
        let Some((address, suffix)) = bracketed.split_once(']') else {
            return false;
        };
        return address.parse::<std::net::Ipv6Addr>().is_ok()
            && (suffix.is_empty() || suffix.strip_prefix(':').is_some_and(valid_port));
    }

    let mut parts = authority.split(':');
    let host = parts.next().unwrap_or_default();
    let port = parts.next();
    if parts.next().is_some()
        || host.is_empty()
        || !host
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-'))
    {
        return false;
    }
    port.is_none_or(valid_port)
}

fn build_request(
    method: &str,
    endpoint: &str,
    bucket: &str,
    key: &str,
    presigned_query: Option<&str>,
    presigned_authority: Option<&str>,
    max_bytes: Option<usize>,
) -> Result<Vec<u8>, HttpError> {
    if !valid_endpoint(endpoint) {
        return Err(HttpError::new(
            HttpErrorKind::InvalidRequest,
            "invalid S3 endpoint, bucket, or key",
        ));
    }
    if presigned_query.is_some_and(|query| !valid_presigned_query(query)) {
        return Err(HttpError::new(
            HttpErrorKind::InvalidRequest,
            "invalid presigned query",
        ));
    }
    if presigned_authority.is_some_and(|authority| !valid_presigned_authority(authority))
        || (presigned_authority.is_some() && presigned_query.is_none())
    {
        return Err(HttpError::new(
            HttpErrorKind::InvalidRequest,
            "invalid presigned authority",
        ));
    }

    let mut target = max_bytes.map_or_else(
        || build_head_canonical_uri(bucket, key),
        |limit| build_canonical_uri(bucket, key, limit),
    )?;
    if let Some(query) = presigned_query {
        target.push('?');
        target.push_str(query);
    }

    let authority = presigned_authority.unwrap_or(endpoint);
    let mut request = String::with_capacity(target.len() + authority.len() + 96);
    write!(
        request,
        "{method} {target} HTTP/1.1\r\nHost: {authority}\r\nAccept-Encoding: identity\r\nConnection: close\r\n\r\n"
    )
    .map_err(|_error| {
        HttpError::new(
            HttpErrorKind::Limit,
            "HTTP request construction exceeded a fixed limit",
        )
    })?;
    Ok(request.into_bytes())
}

#[inline(never)]
pub fn build_get_request(
    endpoint: &str,
    bucket: &str,
    key: &str,
    presigned_query: Option<&str>,
    presigned_authority: Option<&str>,
    max_bytes: usize,
) -> Result<Vec<u8>, HttpError> {
    build_request(
        "GET",
        endpoint,
        bucket,
        key,
        presigned_query,
        presigned_authority,
        Some(max_bytes),
    )
}

#[inline(never)]
pub fn build_head_request(
    endpoint: &str,
    bucket: &str,
    key: &str,
    presigned_query: Option<&str>,
    presigned_authority: Option<&str>,
) -> Result<Vec<u8>, HttpError> {
    build_request(
        "HEAD",
        endpoint,
        bucket,
        key,
        presigned_query,
        presigned_authority,
        None,
    )
}

#[inline(never)]
pub fn build_list_request(
    endpoint: &str,
    bucket: &str,
    prefix: &str,
    max_keys: u32,
    continuation_token: Option<&str>,
    presigned_query: Option<&str>,
    presigned_authority: Option<&str>,
) -> Result<Vec<u8>, HttpError> {
    if !valid_endpoint(endpoint) {
        return Err(HttpError::new(
            HttpErrorKind::InvalidRequest,
            "invalid S3 list endpoint",
        ));
    }
    if presigned_authority.is_some_and(|authority| !valid_presigned_authority(authority))
        || presigned_authority.is_some() != presigned_query.is_some()
    {
        return Err(HttpError::new(
            HttpErrorKind::InvalidRequest,
            "invalid presigned list authority",
        ));
    }
    let canonical_query = build_list_query(prefix, max_keys, continuation_token)?;
    let query = if let Some(query) = presigned_query {
        if !presigned_list_query_matches(query, prefix, max_keys, continuation_token) {
            return Err(HttpError::new(
                HttpErrorKind::InvalidRequest,
                "presigned query does not match S3 list options",
            ));
        }
        query
    } else {
        &canonical_query
    };
    let target = build_list_canonical_uri(bucket)?;
    let authority = presigned_authority.unwrap_or(endpoint);
    let mut request = String::with_capacity(target.len() + query.len() + authority.len() + 96);
    write!(
        request,
        "GET {target}?{query} HTTP/1.1\r\nHost: {authority}\r\nAccept-Encoding: identity\r\nConnection: close\r\n\r\n"
    )
    .map_err(|_error| {
        HttpError::new(
            HttpErrorKind::Limit,
            "S3 list request construction exceeded a fixed limit",
        )
    })?;
    Ok(request.into_bytes())
}

fn header_end(raw: &[u8]) -> Result<usize, HttpError> {
    let Some(offset) = raw.windows(4).position(|window| window == b"\r\n\r\n") else {
        return Err(HttpError::new(
            HttpErrorKind::Protocol,
            "S3 response has no complete HTTP header",
        ));
    };
    let end = offset + 4;
    if end > MAX_HEADER_BYTES {
        return Err(HttpError::new(
            HttpErrorKind::Limit,
            "S3 response header exceeds the byte limit",
        ));
    }
    Ok(end)
}

fn valid_header_name(name: &str) -> bool {
    !name.is_empty()
        && name.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'|'
                        | b'~'
                )
        })
}

fn valid_header_value(value: &str) -> bool {
    value
        .bytes()
        .all(|byte| byte == b'\t' || (byte >= b' ' && byte != 0x7f))
}

fn parse_status(line: &str) -> Result<u16, HttpError> {
    let mut parts = line.splitn(3, ' ');
    let version = parts.next();
    let status = parts.next();
    if !matches!(version, Some("HTTP/1.1" | "HTTP/1.0")) {
        return Err(HttpError::new(
            HttpErrorKind::Protocol,
            "S3 response uses an unsupported HTTP version",
        ));
    }
    let status = status
        .filter(|value| value.len() == 3 && value.bytes().all(|byte| byte.is_ascii_digit()))
        .and_then(|value| value.parse::<u16>().ok())
        .filter(|value| (100..=599).contains(value))
        .ok_or_else(|| {
            HttpError::new(
                HttpErrorKind::Protocol,
                "S3 response has an invalid HTTP status",
            )
        })?;
    Ok(status)
}

struct ResponseHeaders {
    status: u16,
    content_length: Option<u64>,
    chunked: bool,
    content_encoding: Option<String>,
    etag: Option<String>,
    last_modified: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObjectMetadata {
    pub content_length: Option<u64>,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
}

fn set_unique_metadata_header(
    target: &mut Option<String>,
    value: &str,
    repeated_message: &'static str,
) -> Result<(), HttpError> {
    if target.replace(value.to_owned()).is_some() {
        return Err(HttpError::new(HttpErrorKind::Protocol, repeated_message));
    }
    Ok(())
}

enum ChunkedProgress {
    Complete,
    Need(usize),
}

#[inline(never)]
fn parse_headers(raw: &[u8], end: usize) -> Result<ResponseHeaders, HttpError> {
    let text = std::str::from_utf8(&raw[..end - 2]).map_err(|_error| {
        HttpError::new(
            HttpErrorKind::Protocol,
            "S3 response header is not valid ASCII",
        )
    })?;
    if !text.is_ascii() {
        return Err(HttpError::new(
            HttpErrorKind::Protocol,
            "S3 response header is not valid ASCII",
        ));
    }
    let mut lines = text.split("\r\n");
    let status = parse_status(lines.next().unwrap_or_default())?;
    let mut content_length = None;
    let mut transfer_encoding = None::<&str>;
    let mut content_encoding = None::<&str>;
    let mut etag = None;
    let mut last_modified = None;
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let (name, value) = line.split_once(':').ok_or_else(|| {
            HttpError::new(
                HttpErrorKind::Protocol,
                "S3 response contains a malformed HTTP header",
            )
        })?;
        let value = value.trim_matches([' ', '\t']);
        if !valid_header_name(name) || !valid_header_value(value) {
            return Err(HttpError::new(
                HttpErrorKind::Protocol,
                "S3 response contains a malformed HTTP header",
            ));
        }
        if name.eq_ignore_ascii_case("content-length") {
            if content_length.is_some() {
                return Err(HttpError::new(
                    HttpErrorKind::Protocol,
                    "S3 response repeats Content-Length",
                ));
            }
            content_length = Some(value.parse::<u64>().map_err(|_error| {
                HttpError::new(
                    HttpErrorKind::Protocol,
                    "S3 response has an invalid Content-Length",
                )
            })?);
        } else if name.eq_ignore_ascii_case("transfer-encoding") {
            if transfer_encoding.replace(value).is_some() {
                return Err(HttpError::new(
                    HttpErrorKind::Protocol,
                    "S3 response repeats Transfer-Encoding",
                ));
            }
        } else if name.eq_ignore_ascii_case("content-encoding")
            && content_encoding.replace(value).is_some()
        {
            return Err(HttpError::new(
                HttpErrorKind::Protocol,
                "S3 response repeats Content-Encoding",
            ));
        } else if name.eq_ignore_ascii_case("etag") {
            set_unique_metadata_header(&mut etag, value, "S3 response repeats ETag")?;
        } else if name.eq_ignore_ascii_case("last-modified") {
            set_unique_metadata_header(
                &mut last_modified,
                value,
                "S3 response repeats Last-Modified",
            )?;
        }
    }
    if content_length.is_some() && transfer_encoding.is_some() {
        return Err(HttpError::new(
            HttpErrorKind::Protocol,
            "S3 response mixes Content-Length and Transfer-Encoding",
        ));
    }
    let chunked = match transfer_encoding {
        None => false,
        Some(value) if value.eq_ignore_ascii_case("chunked") => true,
        Some(_) => {
            return Err(HttpError::new(
                HttpErrorKind::Unsupported,
                "unsupported HTTP transfer encoding",
            ));
        }
    };
    Ok(ResponseHeaders {
        status,
        content_length,
        chunked,
        content_encoding: content_encoding.map(str::to_owned),
        etag,
        last_modified,
    })
}

fn validate_get_content_encoding(headers: &ResponseHeaders) -> Result<(), HttpError> {
    if headers
        .content_encoding
        .as_deref()
        .is_some_and(|value| !value.eq_ignore_ascii_case("identity"))
    {
        return Err(HttpError::new(
            HttpErrorKind::Unsupported,
            "compressed HTTP content encoding is unsupported",
        ));
    }
    Ok(())
}

const fn status_error(status: u16) -> Option<HttpError> {
    match status {
        200 => None,
        401 | 403 => Some(HttpError::with_status(
            HttpErrorKind::Denied,
            status,
            "S3 request was denied",
        )),
        404 => Some(HttpError::with_status(
            HttpErrorKind::NotFound,
            status,
            "S3 object was not found",
        )),
        300..=399 => Some(HttpError::with_status(
            HttpErrorKind::Unsupported,
            status,
            "S3 redirect responses are unsupported",
        )),
        500..=599 => Some(HttpError::with_status(
            HttpErrorKind::Server,
            status,
            "S3 server returned an error",
        )),
        _ => Some(HttpError::with_status(
            HttpErrorKind::Protocol,
            status,
            "S3 server returned an unexpected status",
        )),
    }
}

fn find_crlf(raw: &[u8], start: usize) -> Option<usize> {
    raw.get(start..)?
        .windows(2)
        .position(|window| window == b"\r\n")
        .map(|offset| start + offset)
}

fn chunked_progress(body: &[u8], max_bytes: usize) -> Result<ChunkedProgress, HttpError> {
    let mut cursor = 0_usize;
    let mut decoded_bytes = 0_usize;
    let mut chunks = 0_usize;
    loop {
        chunks = chunks
            .checked_add(1)
            .ok_or_else(|| HttpError::new(HttpErrorKind::Limit, "S3 chunk count overflowed"))?;
        if chunks > MAX_CHUNKS {
            return Err(HttpError::new(
                HttpErrorKind::Limit,
                "S3 response contains too many HTTP chunks",
            ));
        }
        let Some(line_end) = find_crlf(body, cursor) else {
            let available = body.len().saturating_sub(cursor);
            if available >= 10 {
                return Err(HttpError::new(
                    HttpErrorKind::Protocol,
                    "S3 response has an invalid HTTP chunk size",
                ));
            }
            return Ok(ChunkedProgress::Need(10 - available));
        };
        let size_bytes = &body[cursor..line_end];
        if size_bytes.is_empty()
            || size_bytes.len() > 8
            || !size_bytes.iter().all(u8::is_ascii_hexdigit)
        {
            return Err(HttpError::new(
                HttpErrorKind::Protocol,
                "S3 response has an invalid HTTP chunk size",
            ));
        }
        let size_text = std::str::from_utf8(size_bytes).map_err(|_error| {
            HttpError::new(
                HttpErrorKind::Protocol,
                "S3 response has an invalid HTTP chunk size",
            )
        })?;
        let size = usize::from_str_radix(size_text, 16).map_err(|_error| {
            HttpError::new(
                HttpErrorKind::Protocol,
                "S3 response has an invalid HTTP chunk size",
            )
        })?;
        cursor = line_end + 2;
        if size == 0 {
            let available = body.len().saturating_sub(cursor);
            return if available < 2 {
                Ok(ChunkedProgress::Need(2 - available))
            } else {
                Ok(ChunkedProgress::Complete)
            };
        }
        decoded_bytes = decoded_bytes.checked_add(size).ok_or_else(|| {
            HttpError::new(HttpErrorKind::Limit, "S3 object byte count overflowed")
        })?;
        if decoded_bytes > max_bytes {
            return Err(HttpError::new(
                HttpErrorKind::Limit,
                "S3 object exceeds the requested byte limit",
            ));
        }
        let data_end = cursor
            .checked_add(size)
            .ok_or_else(|| HttpError::new(HttpErrorKind::Limit, "S3 chunk offset overflowed"))?;
        let framed_end = data_end
            .checked_add(2)
            .ok_or_else(|| HttpError::new(HttpErrorKind::Limit, "S3 chunk offset overflowed"))?;
        if body.len() < framed_end {
            return Ok(ChunkedProgress::Need(framed_end - body.len()));
        }
        if body.get(data_end..framed_end) != Some(b"\r\n") {
            return Err(HttpError::new(
                HttpErrorKind::Protocol,
                "S3 response has malformed HTTP chunk framing",
            ));
        }
        cursor = framed_end;
    }
}

/// Return the largest useful next host read for the response received so far.
///
/// Framed responses request only bytes that their HTTP framing proves remain.
/// This matters because Sigil reserves the requested amount against the named
/// endpoint quota before performing I/O.
pub fn next_read_size(raw: &[u8], max_bytes: usize) -> Result<Option<usize>, HttpError> {
    next_read_size_impl(raw, max_bytes, false)
}

/// Return the next bounded read for a host-signed response.
///
/// Unlike the legacy raw-network path, this reads a bounded, framed 4xx body
/// so exact S3 clock-skew codes can be classified without exposing the body.
pub fn next_signed_read_size(raw: &[u8], max_bytes: usize) -> Result<Option<usize>, HttpError> {
    next_read_size_impl(raw, max_bytes, true)
}

/// Return the next bounded read needed for a HEAD response.
///
/// Once the complete header arrives this returns `None`; it never requests an
/// object body. Any body bytes already delivered with the header fail closed.
pub fn next_head_read_size(raw: &[u8]) -> Result<Option<usize>, HttpError> {
    let Some(offset) = raw.windows(4).position(|window| window == b"\r\n\r\n") else {
        if raw.len() >= MAX_HEADER_BYTES {
            return Err(HttpError::new(
                HttpErrorKind::Limit,
                "S3 response header exceeds the byte limit",
            ));
        }
        return Ok(Some(MAX_HEADER_BYTES - raw.len()));
    };
    let end = offset + 4;
    if end > MAX_HEADER_BYTES {
        return Err(HttpError::new(
            HttpErrorKind::Limit,
            "S3 response header exceeds the byte limit",
        ));
    }
    decode_head_response(raw).map(|_metadata| None)
}

fn next_read_size_impl(
    raw: &[u8],
    max_bytes: usize,
    read_s3_error: bool,
) -> Result<Option<usize>, HttpError> {
    let Some(offset) = raw.windows(4).position(|window| window == b"\r\n\r\n") else {
        if raw.len() >= MAX_HEADER_BYTES {
            return Err(HttpError::new(
                HttpErrorKind::Limit,
                "S3 response header exceeds the byte limit",
            ));
        }
        return Ok(Some(MAX_HEADER_BYTES - raw.len()));
    };
    let end = offset + 4;
    if end > MAX_HEADER_BYTES {
        return Err(HttpError::new(
            HttpErrorKind::Limit,
            "S3 response header exceeds the byte limit",
        ));
    }
    let headers = parse_headers(raw, end)?;
    validate_get_content_encoding(&headers)?;
    let body_limit = response_body_limit(headers.status, max_bytes, read_s3_error)?;
    if headers
        .content_length
        .is_some_and(|length| length > body_limit as u64)
    {
        return Err(HttpError::new(
            HttpErrorKind::Limit,
            "S3 object exceeds the requested byte limit",
        ));
    }
    if headers.chunked {
        return match chunked_progress(&raw[end..], body_limit)? {
            ChunkedProgress::Complete => Ok(None),
            ChunkedProgress::Need(bytes) => Ok(Some(bytes)),
        };
    }
    if let Some(content_length) = headers.content_length {
        let content_length = usize::try_from(content_length).map_err(|_error| {
            HttpError::new(
                HttpErrorKind::Limit,
                "S3 object byte count is not representable",
            )
        })?;
        let expected = end.checked_add(content_length).ok_or_else(|| {
            HttpError::new(HttpErrorKind::Limit, "S3 response byte count overflowed")
        })?;
        return Ok((raw.len() < expected).then_some(expected - raw.len()));
    }
    let body_bytes = raw.len() - end;
    if body_bytes > body_limit {
        return Err(HttpError::new(
            HttpErrorKind::Limit,
            "S3 object exceeds the requested byte limit",
        ));
    }
    Ok(Some(body_limit - body_bytes + 1))
}

fn response_body_limit(
    status: u16,
    object_limit: usize,
    read_s3_error: bool,
) -> Result<usize, HttpError> {
    if status == 200 {
        return Ok(object_limit);
    }
    if read_s3_error && (400..=499).contains(&status) {
        return Ok(MAX_S3_ERROR_BODY_BYTES);
    }
    Err(status_error(status).unwrap_or_else(|| {
        HttpError::new(
            HttpErrorKind::Protocol,
            "S3 server returned an unexpected status",
        )
    }))
}

#[inline(never)]
fn decode_chunked(body: &[u8], max_bytes: usize) -> Result<Vec<u8>, HttpError> {
    let mut cursor = 0_usize;
    let mut output = Vec::new();
    let mut chunks = 0_usize;
    loop {
        chunks = chunks
            .checked_add(1)
            .ok_or_else(|| HttpError::new(HttpErrorKind::Limit, "S3 chunk count overflowed"))?;
        if chunks > MAX_CHUNKS {
            return Err(HttpError::new(
                HttpErrorKind::Limit,
                "S3 response contains too many HTTP chunks",
            ));
        }
        let line_end = find_crlf(body, cursor).ok_or_else(|| {
            HttpError::new(
                HttpErrorKind::Protocol,
                "S3 response has a truncated HTTP chunk size",
            )
        })?;
        let size_bytes = &body[cursor..line_end];
        if size_bytes.is_empty()
            || size_bytes.len() > 8
            || !size_bytes.iter().all(u8::is_ascii_hexdigit)
        {
            return Err(HttpError::new(
                HttpErrorKind::Protocol,
                "S3 response has an invalid HTTP chunk size",
            ));
        }
        let size_text = std::str::from_utf8(size_bytes).map_err(|_error| {
            HttpError::new(
                HttpErrorKind::Protocol,
                "S3 response has an invalid HTTP chunk size",
            )
        })?;
        let size = usize::from_str_radix(size_text, 16).map_err(|_error| {
            HttpError::new(
                HttpErrorKind::Protocol,
                "S3 response has an invalid HTTP chunk size",
            )
        })?;
        cursor = line_end + 2;
        if size == 0 {
            if body.get(cursor..) != Some(b"\r\n") {
                return Err(HttpError::new(
                    HttpErrorKind::Unsupported,
                    "S3 response trailers are unsupported",
                ));
            }
            return Ok(output);
        }
        let next_len = output.len().checked_add(size).ok_or_else(|| {
            HttpError::new(HttpErrorKind::Limit, "S3 object byte count overflowed")
        })?;
        if next_len > max_bytes {
            return Err(HttpError::new(
                HttpErrorKind::Limit,
                "S3 object exceeds the requested byte limit",
            ));
        }
        let data_end = cursor
            .checked_add(size)
            .ok_or_else(|| HttpError::new(HttpErrorKind::Limit, "S3 chunk offset overflowed"))?;
        let data = body.get(cursor..data_end).ok_or_else(|| {
            HttpError::new(
                HttpErrorKind::Protocol,
                "S3 response has a truncated HTTP chunk",
            )
        })?;
        let framed_end = data_end
            .checked_add(2)
            .ok_or_else(|| HttpError::new(HttpErrorKind::Limit, "S3 chunk offset overflowed"))?;
        if body.get(data_end..framed_end) != Some(b"\r\n") {
            return Err(HttpError::new(
                HttpErrorKind::Protocol,
                "S3 response has malformed HTTP chunk framing",
            ));
        }
        output.extend_from_slice(data);
        cursor = framed_end;
    }
}

#[inline(never)]
pub fn decode_response(raw: &[u8], max_bytes: usize) -> Result<Vec<u8>, HttpError> {
    decode_response_impl(raw, max_bytes, false)
}

/// Decode a host-signed response and classify only exact structured S3
/// clock-skew codes. The raw XML and all other upstream fields are discarded.
pub fn decode_signed_response(raw: &[u8], max_bytes: usize) -> Result<Vec<u8>, HttpError> {
    decode_response_impl(raw, max_bytes, true)
}

/// Decode a successful HEAD response without reading or synthesizing a body.
///
/// Metadata strings are returned after HTTP optional whitespace is removed,
/// exactly as supplied otherwise. Missing values remain distinguishable.
pub fn decode_head_response(raw: &[u8]) -> Result<ObjectMetadata, HttpError> {
    let end = header_end(raw)?;
    let headers = parse_headers(raw, end)?;
    if raw.len() != end {
        return Err(HttpError::new(
            HttpErrorKind::Protocol,
            "S3 HEAD response unexpectedly contains body bytes",
        ));
    }
    if headers.chunked {
        return Err(HttpError::new(
            HttpErrorKind::Protocol,
            "S3 HEAD response uses transfer framing",
        ));
    }
    if let Some(error) = status_error(headers.status) {
        return Err(error);
    }
    Ok(ObjectMetadata {
        content_length: headers.content_length,
        etag: headers.etag,
        last_modified: headers.last_modified,
    })
}

fn decode_response_impl(
    raw: &[u8],
    max_bytes: usize,
    read_s3_error: bool,
) -> Result<Vec<u8>, HttpError> {
    let end = header_end(raw)?;
    let headers = parse_headers(raw, end)?;
    validate_get_content_encoding(&headers)?;
    let body_limit = response_body_limit(headers.status, max_bytes, read_s3_error)?;
    if headers
        .content_length
        .is_some_and(|length| length > body_limit as u64)
    {
        return Err(HttpError::new(
            HttpErrorKind::Limit,
            "S3 object exceeds the requested byte limit",
        ));
    }
    let body = &raw[end..];
    let decoded = if headers.chunked {
        decode_chunked(body, body_limit)?
    } else {
        if let Some(expected) = headers.content_length
            && expected != body.len() as u64
        {
            return Err(HttpError::new(
                HttpErrorKind::Protocol,
                "S3 response body does not match Content-Length",
            ));
        }
        if body.len() > body_limit {
            return Err(HttpError::new(
                HttpErrorKind::Limit,
                "S3 object exceeds the requested byte limit",
            ));
        }
        body.to_vec()
    };

    if headers.status == 200 {
        return Ok(decoded);
    }
    let error = if read_s3_error && structured_s3_clock_skew_code(&decoded).is_some() {
        HttpError::with_status(
            HttpErrorKind::ClockSkew,
            headers.status,
            "S3 rejected the request signing time",
        )
    } else {
        status_error(headers.status).unwrap_or_else(|| {
            HttpError::new(
                HttpErrorKind::Protocol,
                "S3 server returned an unexpected status",
            )
        })
    };
    Err(error)
}

fn structured_s3_clock_skew_code(body: &[u8]) -> Option<&str> {
    let text = std::str::from_utf8(body).ok()?.trim_start();
    let text = if let Some(after_open) = text.strip_prefix("<?xml") {
        after_open.split_once("?>")?.1.trim_start()
    } else {
        text
    };
    let error = text.strip_prefix("<Error>")?.trim_start();
    let code = error.strip_prefix("<Code>")?.split_once("</Code>")?.0;
    if code.bytes().all(|byte| byte.is_ascii_alphanumeric())
        && matches!(code, "RequestTimeTooSkewed" | "RequestExpired")
    {
        Some(code)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_is_path_style_binary_safe_and_injection_resistant() {
        let request = build_get_request(
            "minio",
            "results",
            "folder/a b/é.parquet",
            Some("X-Amz-Algorithm=AWS4-HMAC-SHA256&X-Amz-Signature=abc%2Fdef"),
            Some("127.0.0.1:9000"),
            1024,
        )
        .expect("valid request");
        let request = String::from_utf8(request).expect("ASCII request");
        assert!(request.starts_with("GET /results/folder/a%20b/%C3%A9.parquet?X-Amz-Algorithm="));
        assert!(request.contains("X-Amz-Signature=abc%2Fdef HTTP/1.1\r\n"));
        assert!(request.contains("\r\nHost: 127.0.0.1:9000\r\n"));
        assert!(build_get_request("minio", "results.test", "key", None, None, 1).is_ok());
        assert!(build_get_request("bad\r\nname", "results", "key", None, None, 1).is_err());
        assert!(build_get_request("minio", "results", "key", Some("x=1\r\ny=2"), None, 1).is_err());
    }

    #[test]
    fn head_request_preserves_auth_authority_and_has_no_body() {
        let request = build_head_request(
            "operator-route",
            "results",
            "exports/capi.parquet",
            Some("X-Amz-Credential=key%2Fscope&X-Amz-Signature=abc"),
            Some("127.0.0.1:9000"),
        )
        .expect("valid presigned HEAD request");
        let request = String::from_utf8(request).expect("ASCII request");
        assert_eq!(
            request,
            "HEAD /results/exports/capi.parquet?X-Amz-Credential=key%2Fscope&X-Amz-Signature=abc HTTP/1.1\r\nHost: 127.0.0.1:9000\r\nAccept-Encoding: identity\r\nConnection: close\r\n\r\n"
        );
        assert!(!request.contains("Host: operator-route"));
        assert!(!request.contains("GET "));
    }

    #[test]
    fn list_query_is_canonical_bounded_and_caller_driven() {
        assert_eq!(
            build_list_query("exports/space %/é+", 17, None).expect("first page"),
            "list-type=2&max-keys=17&prefix=exports%2Fspace%20%25%2F%C3%A9%2B"
        );
        assert_eq!(
            build_list_query("exports/", 17, Some("opaque/next+2")).expect("later page"),
            "continuation-token=opaque%2Fnext%2B2&list-type=2&max-keys=17&prefix=exports%2F"
        );
        for (prefix, max_keys, token) in [
            ("", 1, None),
            ("exports/", 0, None),
            ("exports/", MAX_LIST_KEYS + 1, None),
            ("exports/", 1, Some("")),
        ] {
            assert!(build_list_query(prefix, max_keys, token).is_err());
        }
    }

    #[test]
    fn deterministic_list_query_corpus_is_ascii_and_canonical() {
        for byte in 0_u8..=127 {
            let character = char::from(byte);
            let prefix = format!("exports/{character}suffix");
            let token = format!("opaque{character}token");
            let query = build_list_query(&prefix, 1000, Some(&token)).expect("bounded query");
            assert!(query.is_ascii());
            assert!(!query.contains(' '));
            for (index, encoded) in query.bytes().enumerate() {
                if encoded == b'%' {
                    let escape = query
                        .as_bytes()
                        .get(index + 1..index + 3)
                        .expect("complete percent escape");
                    assert!(escape.iter().all(u8::is_ascii_hexdigit));
                    assert!(!escape.iter().any(u8::is_ascii_lowercase));
                }
            }
        }
    }

    #[test]
    fn raw_and_presigned_list_requests_bind_the_exact_options() {
        let anonymous = build_list_request(
            "object-store",
            "results",
            "exports/",
            2,
            Some("next/+"),
            None,
            None,
        )
        .expect("anonymous list");
        assert_eq!(
            String::from_utf8(anonymous).expect("ASCII"),
            "GET /results/?continuation-token=next%2F%2B&list-type=2&max-keys=2&prefix=exports%2F HTTP/1.1\r\nHost: object-store\r\nAccept-Encoding: identity\r\nConnection: close\r\n\r\n"
        );

        let presigned_query = "X-Amz-Algorithm=AWS4-HMAC-SHA256&X-Amz-Signature=abc%2Fdef&list-type=2&max-keys=2&prefix=exports%2F";
        let presigned = build_list_request(
            "operator-route",
            "results",
            "exports/",
            2,
            None,
            Some(presigned_query),
            Some("127.0.0.1:9000"),
        )
        .expect("presigned list");
        let presigned = String::from_utf8(presigned).expect("ASCII");
        assert!(presigned.starts_with(&format!("GET /results/?{presigned_query} HTTP/1.1\r\n")));
        assert!(presigned.contains("\r\nHost: 127.0.0.1:9000\r\n"));

        for mismatch in [
            "X-Amz-Signature=x&list-type=1&max-keys=2&prefix=exports%2F",
            "X-Amz-Signature=x&list-type=2&max-keys=3&prefix=exports%2F",
            "X-Amz-Signature=x&list-type=2&max-keys=2&prefix=other%2F",
            "X-Amz-Signature=x&list-type=2&list-type=2&max-keys=2&prefix=exports%2F",
            "X-Amz-Signature=x&list-type=2&max-keys=2&prefix=exports%2F&continuation-token=unexpected",
        ] {
            assert!(
                build_list_request(
                    "operator-route",
                    "results",
                    "exports/",
                    2,
                    None,
                    Some(mismatch),
                    Some("127.0.0.1:9000")
                )
                .is_err(),
                "accepted mismatch: {mismatch}"
            );
        }
    }

    #[test]
    fn presigned_authority_is_bounded_and_cannot_select_the_socket_route() {
        for authority in [
            "minio.example:9000",
            "127.0.0.1:9000",
            "[2001:db8::1]:9000",
            "localhost",
        ] {
            let request = build_get_request(
                "operator-route",
                "results",
                "key",
                Some("X-Amz-Signature=abc"),
                Some(authority),
                1,
            )
            .expect("valid authority");
            let request = String::from_utf8(request).expect("ASCII request");
            assert!(request.contains(&format!("\r\nHost: {authority}\r\n")));
            assert!(!request.contains("Host: operator-route\r\n"));
        }

        for authority in [
            "",
            "example.com:0",
            "example.com:+80",
            "example.com:-1",
            "example.com:65536",
            "user@example.com",
            "example.com/path",
            "example.com\r\nX-Injected: yes",
            "2001:db8::1",
            "[not-ipv6]:9000",
        ] {
            assert!(
                build_get_request(
                    "operator-route",
                    "results",
                    "key",
                    Some("X-Amz-Signature=abc"),
                    Some(authority),
                    1,
                )
                .is_err(),
                "authority {authority:?} must be rejected"
            );
        }
        assert!(
            build_get_request(
                "operator-route",
                "results",
                "key",
                None,
                Some("example.com"),
                1,
            )
            .is_err(),
            "an authority without a presigned query has no defined meaning"
        );
    }

    #[test]
    fn fixed_and_chunked_binary_bodies_decode_exactly() {
        let fixed = b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\n\0\xffAB".to_vec();
        assert_eq!(decode_response(&fixed, 4).expect("fixed body"), b"\0\xffAB");

        let chunked = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\n\0\xff\r\n2\r\nAB\r\n0\r\n\r\n".to_vec();
        assert_eq!(
            decode_response(&chunked, 4).expect("chunked body"),
            b"\0\xffAB"
        );
    }

    #[test]
    fn head_metadata_is_optional_exact_and_unnormalized() {
        let full = b"HTTP/1.1 200 OK\r\nContent-Length: 18446744073709551615\r\nETag:\t\"abc-2\" \r\nLast-Modified: Sun, 30 Aug 2026 23:59:01 GMT\r\nContent-Encoding: gzip\r\n\r\n";
        assert_eq!(
            decode_head_response(full).expect("complete metadata"),
            ObjectMetadata {
                content_length: Some(u64::MAX),
                etag: Some("\"abc-2\"".to_owned()),
                last_modified: Some("Sun, 30 Aug 2026 23:59:01 GMT".to_owned()),
            }
        );

        let absent = b"HTTP/1.1 200 OK\r\nDate: Sun, 30 Aug 2026 23:59:01 GMT\r\n\r\n";
        assert_eq!(
            decode_head_response(absent).expect("missing metadata remains explicit"),
            ObjectMetadata {
                content_length: None,
                etag: None,
                last_modified: None,
            }
        );

        let zero = b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n";
        assert_eq!(
            decode_head_response(zero)
                .expect("present zero content length remains distinguishable")
                .content_length,
            Some(0)
        );

        let empty = b"HTTP/1.1 200 OK\r\nETag:\r\nLast-Modified:\r\n\r\n";
        assert_eq!(
            decode_head_response(empty).expect("present empty values remain present"),
            ObjectMetadata {
                content_length: None,
                etag: Some(String::new()),
                last_modified: Some(String::new()),
            }
        );
    }

    #[test]
    fn head_rejects_ambiguous_metadata_framing_and_body_bytes() {
        for raw in [
            b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\nContent-Length: 1\r\n\r\n".as_slice(),
            b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\nContent-Length: 2\r\n\r\n".as_slice(),
            b"HTTP/1.1 200 OK\r\nETag: one\r\nETag: two\r\n\r\n".as_slice(),
            b"HTTP/1.1 200 OK\r\nLast-Modified: one\r\nLast-Modified: two\r\n\r\n".as_slice(),
            b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\nTransfer-Encoding: chunked\r\n\r\n"
                .as_slice(),
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n".as_slice(),
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: identity\r\n\r\n".as_slice(),
            b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\n\r\nX".as_slice(),
            b"HTTP/1.1 200 OK\r\nContent-Length: 18446744073709551616\r\n\r\n".as_slice(),
            b"HTTP/1.1 200 OK\r\nETag: \xff\r\n\r\n".as_slice(),
            b"HTTP/1.1 200 OK\r\nETag: one\r\n two\r\n\r\n".as_slice(),
            b"HTTP/1.1 200 OK\r\nETag: one\x01two\r\n\r\n".as_slice(),
            b"HTTP/1.1 200 OK\r\nBad Name: value\r\n\r\n".as_slice(),
        ] {
            assert!(
                decode_head_response(raw).is_err(),
                "hostile HEAD response must fail: {raw:?}"
            );
        }
    }

    #[test]
    fn head_statuses_are_typed_without_metadata_or_payload_fallback() {
        for (status, kind) in [
            (100, HttpErrorKind::Protocol),
            (204, HttpErrorKind::Protocol),
            (301, HttpErrorKind::Unsupported),
            (401, HttpErrorKind::Denied),
            (403, HttpErrorKind::Denied),
            (404, HttpErrorKind::NotFound),
            (500, HttpErrorKind::Server),
            (599, HttpErrorKind::Server),
        ] {
            let raw = format!(
                "HTTP/1.1 {status} Status\r\nContent-Length: 7\r\nETag: do-not-return\r\n\r\n"
            );
            let error = decode_head_response(raw.as_bytes()).expect_err("status must fail");
            assert_eq!(error.kind, kind);
            assert_eq!(error.status, Some(status));
            assert!(!error.message.contains("do-not-return"));
        }

        let with_error_body = b"HTTP/1.1 403 Forbidden\r\nContent-Length: 6\r\n\r\nsecret";
        let error = decode_head_response(with_error_body)
            .expect_err("HEAD must not consume or expose an upstream body");
        assert_eq!(error.kind, HttpErrorKind::Protocol);
        assert!(!error.message.contains("secret"));
    }

    #[test]
    fn head_reads_only_a_bounded_complete_header() {
        assert_eq!(
            next_head_read_size(b"").expect("empty response"),
            Some(MAX_HEADER_BYTES)
        );
        let partial = b"HTTP/1.1 200 OK\r\nContent-Length: 7\r\n";
        assert_eq!(
            next_head_read_size(partial).expect("partial header"),
            Some(MAX_HEADER_BYTES - partial.len())
        );
        let complete = b"HTTP/1.1 200 OK\r\nContent-Length: 7\r\n\r\n";
        assert_eq!(
            next_head_read_size(complete).expect("complete header"),
            None
        );
        let mut body = complete.to_vec();
        body.push(b'X');
        assert_eq!(
            next_head_read_size(&body)
                .expect_err("coalesced body byte must fail")
                .kind,
            HttpErrorKind::Protocol
        );

        let no_terminator = vec![b'A'; MAX_HEADER_BYTES];
        assert_eq!(
            next_head_read_size(&no_terminator)
                .expect_err("aggregate header limit must fail")
                .kind,
            HttpErrorKind::Limit
        );
        let oversized = format!(
            "HTTP/1.1 200 OK\r\nX-Fill: {}\r\n\r\n",
            "a".repeat(MAX_HEADER_BYTES)
        );
        assert_eq!(
            decode_head_response(oversized.as_bytes())
                .expect_err("terminated oversized header must fail")
                .kind,
            HttpErrorKind::Limit
        );

        let fixed_prefix = "HTTP/1.1 200 OK\r\nX-Fill: ";
        let fixed_suffix = "\r\n\r\n";
        let exact = format!(
            "{fixed_prefix}{}{fixed_suffix}",
            "a".repeat(MAX_HEADER_BYTES - fixed_prefix.len() - fixed_suffix.len())
        );
        assert_eq!(exact.len(), MAX_HEADER_BYTES);
        assert_eq!(
            next_head_read_size(exact.as_bytes()).expect("exact cap is accepted"),
            None
        );
        assert!(decode_head_response(exact.as_bytes()).is_ok());

        let over_by_one = format!(
            "{fixed_prefix}{}{fixed_suffix}",
            "a".repeat(MAX_HEADER_BYTES + 1 - fixed_prefix.len() - fixed_suffix.len())
        );
        assert_eq!(over_by_one.len(), MAX_HEADER_BYTES + 1);
        assert_eq!(
            next_head_read_size(over_by_one.as_bytes())
                .expect_err("cap plus one must fail")
                .kind,
            HttpErrorKind::Limit
        );
    }

    #[test]
    fn limits_and_ambiguous_framing_fail_closed() {
        let oversized = b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\nABCD".to_vec();
        assert_eq!(
            decode_response(&oversized, 3)
                .expect_err("must reject limit")
                .kind,
            HttpErrorKind::Limit
        );
        let declared_oversized = b"HTTP/1.1 200 OK\r\nContent-Length: 1000000\r\n\r\n".to_vec();
        assert_eq!(
            decode_response(&declared_oversized, 3)
                .expect_err("declared size must reject before body decoding")
                .kind,
            HttpErrorKind::Limit
        );
        let ambiguous =
            b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n"
                .to_vec();
        assert_eq!(
            decode_response(&ambiguous, 3)
                .expect_err("must reject ambiguous framing")
                .kind,
            HttpErrorKind::Protocol
        );
    }

    #[test]
    fn statuses_are_typed_without_returning_server_payloads() {
        for (status, kind) in [
            (403, HttpErrorKind::Denied),
            (404, HttpErrorKind::NotFound),
            (503, HttpErrorKind::Server),
            (307, HttpErrorKind::Unsupported),
        ] {
            let raw =
                format!("HTTP/1.1 {status} Error\r\nContent-Length: 14\r\n\r\nsecret payload")
                    .into_bytes();
            let error = decode_response(&raw, 100).expect_err("status must fail");
            assert_eq!(error.kind, kind);
            assert_eq!(error.status, Some(status));
            assert!(!error.message.contains("secret"));
        }
    }

    #[test]
    fn signed_errors_classify_only_exact_structured_clock_skew_codes() {
        for code in ["RequestTimeTooSkewed", "RequestExpired"] {
            let body = format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?><Error><Code>{code}</Code><Message>do not return me</Message><RequestId>sensitive</RequestId></Error>"
            );
            let raw = format!(
                "HTTP/1.1 403 Forbidden\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
            let error = decode_signed_response(raw.as_bytes(), 1)
                .expect_err("structured signing-time rejection must fail");
            assert_eq!(error.kind, HttpErrorKind::ClockSkew);
            assert_eq!(error.status, Some(403));
            assert!(!error.message.contains(code));
            assert!(!error.message.contains("sensitive"));
        }

        for body in [
            "<Error><Code>SignatureDoesNotMatch</Code><Message>RequestExpired</Message></Error>",
            "<Error><Message>RequestExpired</Message><Code>RequestExpired</Code></Error>",
            "free text RequestTimeTooSkewed",
        ] {
            let raw = format!(
                "HTTP/1.1 403 Forbidden\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
            let error = decode_signed_response(raw.as_bytes(), 1)
                .expect_err("noncanonical error shape must fail");
            assert_eq!(error.kind, HttpErrorKind::Denied);
            assert_eq!(error.status, Some(403));
        }
    }

    #[test]
    fn signed_error_body_reads_are_framed_and_bounded() {
        let body = "<Error><Code>RequestExpired</Code></Error>";
        let header = format!(
            "HTTP/1.1 400 Bad Request\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        assert_eq!(
            next_read_size(header.as_bytes(), 1)
                .expect_err("legacy path preserves immediate status mapping")
                .kind,
            HttpErrorKind::Protocol
        );
        assert_eq!(
            next_signed_read_size(header.as_bytes(), 1).expect("signed error body is readable"),
            Some(body.len())
        );
        let complete = format!("{header}{body}");
        assert_eq!(
            next_signed_read_size(complete.as_bytes(), 1).expect("signed body is complete"),
            None
        );

        let oversized = format!(
            "HTTP/1.1 403 Forbidden\r\nContent-Length: {}\r\n\r\n",
            MAX_S3_ERROR_BODY_BYTES + 1
        );
        assert_eq!(
            next_signed_read_size(oversized.as_bytes(), 1)
                .expect_err("oversized error body must fail before reading")
                .kind,
            HttpErrorKind::Limit
        );
    }

    #[test]
    fn signed_canonical_uri_matches_aws_encoding_rules_and_rejects_escapes() {
        assert_eq!(
            build_sigv4_canonical_uri("examplebucket", "photos/Jan/sample.jpg", 1)
                .expect("AWS object-key example"),
            "/examplebucket/photos/Jan/sample.jpg"
        );
        assert_eq!(
            build_sigv4_canonical_uri("examplebucket", "snow man/é+.txt", 1)
                .expect("UTF-8 and reserved bytes"),
            "/examplebucket/snow%20man/%C3%A9%2B.txt"
        );
        for key in [".", "..", "x/../y", r"x\y", "x%2Fy"] {
            assert!(
                build_sigv4_canonical_uri("examplebucket", key, 1).is_err(),
                "signed key {key:?} must be rejected"
            );
            assert!(
                build_canonical_uri("examplebucket", key, 1).is_ok(),
                "legacy raw key {key:?} must remain accepted"
            );
        }
    }

    #[test]
    fn malformed_headers_chunks_and_lengths_fail_closed() {
        for raw in [
            b"HTTP/2 200 OK\r\nContent-Length: 0\r\n\r\n".as_slice(),
            b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\n\r\n".as_slice(),
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n1;x=1\r\nA\r\n0\r\n\r\n"
                .as_slice(),
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n1\r\nA\n0\r\n\r\n".as_slice(),
        ] {
            assert!(decode_response(raw, 1024).is_err());
        }
    }

    #[test]
    fn read_sizes_follow_framing_without_reserving_a_full_chunk_for_the_tail() {
        assert_eq!(
            next_read_size(b"", 4).expect("empty response"),
            Some(32_768)
        );

        let fixed_header = b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\n";
        assert_eq!(
            next_read_size(fixed_header, 4).expect("fixed header"),
            Some(4)
        );
        let mut fixed_partial = fixed_header.to_vec();
        fixed_partial.extend_from_slice(b"ABC");
        assert_eq!(
            next_read_size(&fixed_partial, 4).expect("fixed tail"),
            Some(1)
        );
        fixed_partial.push(b'D');
        assert_eq!(
            next_read_size(&fixed_partial, 4).expect("fixed complete"),
            None
        );

        let chunked_header = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n";
        let mut chunked_partial = chunked_header.to_vec();
        chunked_partial.extend_from_slice(b"4\r\nABC");
        assert_eq!(
            next_read_size(&chunked_partial, 4).expect("chunked tail"),
            Some(3)
        );
        chunked_partial.extend_from_slice(b"D\r\n0\r\n\r\n");
        assert_eq!(
            next_read_size(&chunked_partial, 4).expect("chunked complete"),
            None
        );

        let eof_header = b"HTTP/1.1 200 OK\r\n\r\n";
        let mut eof_full = eof_header.to_vec();
        eof_full.extend_from_slice(b"ABCD");
        assert_eq!(next_read_size(&eof_full, 4).expect("EOF probe"), Some(1));
    }
}
