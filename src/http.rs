use std::fmt::Write as _;

pub const MAX_OBJECT_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_HEADER_BYTES: usize = 32 * 1024;
pub const MAX_WIRE_OVERHEAD_BYTES: usize = 64 * 1024;
const MAX_QUERY_BYTES: usize = 8 * 1024;
const MAX_CHUNKS: usize = 4_096;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HttpErrorKind {
    InvalidRequest,
    Protocol,
    Denied,
    NotFound,
    Server,
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

fn valid_presigned_query(query: &str) -> bool {
    !query.is_empty()
        && query.len() <= MAX_QUERY_BYTES
        && !query.starts_with('?')
        && query
            .bytes()
            .all(|byte| byte.is_ascii_graphic() && byte != b'#')
}

#[inline(never)]
pub fn build_get_request(
    endpoint: &str,
    bucket: &str,
    key: &str,
    presigned_query: Option<&str>,
    max_bytes: usize,
) -> Result<Vec<u8>, HttpError> {
    if !valid_endpoint(endpoint) || !valid_bucket(bucket) || key.is_empty() || key.len() > 1_024 {
        return Err(HttpError::new(
            HttpErrorKind::InvalidRequest,
            "invalid S3 endpoint, bucket, or key",
        ));
    }
    if max_bytes == 0 || max_bytes > MAX_OBJECT_BYTES {
        return Err(HttpError::new(
            HttpErrorKind::Limit,
            "requested object byte limit is outside the supported range",
        ));
    }
    if presigned_query.is_some_and(|query| !valid_presigned_query(query)) {
        return Err(HttpError::new(
            HttpErrorKind::InvalidRequest,
            "invalid presigned query",
        ));
    }

    let mut target = String::with_capacity(bucket.len() + key.len() * 3 + MAX_QUERY_BYTES + 2);
    target.push('/');
    target.push_str(bucket);
    target.push('/');
    push_encoded_key(&mut target, key);
    if let Some(query) = presigned_query {
        target.push('?');
        target.push_str(query);
    }

    let mut request = String::with_capacity(target.len() + endpoint.len() + 96);
    write!(
        request,
        "GET {target} HTTP/1.1\r\nHost: {endpoint}\r\nAccept-Encoding: identity\r\nConnection: close\r\n\r\n"
    )
    .map_err(|_error| {
        HttpError::new(
            HttpErrorKind::Limit,
            "HTTP request construction exceeded a fixed limit",
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
    content_length: Option<usize>,
    chunked: bool,
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
            content_length = Some(value.parse::<usize>().map_err(|_error| {
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
        }
    }
    if content_length.is_some() && transfer_encoding.is_some() {
        return Err(HttpError::new(
            HttpErrorKind::Protocol,
            "S3 response mixes Content-Length and Transfer-Encoding",
        ));
    }
    if content_encoding.is_some_and(|value| !value.eq_ignore_ascii_case("identity")) {
        return Err(HttpError::new(
            HttpErrorKind::Unsupported,
            "compressed HTTP content encoding is unsupported",
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
    })
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
        if body.get(data_end..data_end + 2) != Some(b"\r\n") {
            return Err(HttpError::new(
                HttpErrorKind::Protocol,
                "S3 response has malformed HTTP chunk framing",
            ));
        }
        output.extend_from_slice(data);
        cursor = data_end + 2;
    }
}

#[inline(never)]
pub fn decode_response(raw: &[u8], max_bytes: usize) -> Result<Vec<u8>, HttpError> {
    let end = header_end(raw)?;
    let headers = parse_headers(raw, end)?;
    if let Some(error) = status_error(headers.status) {
        return Err(error);
    }
    if headers
        .content_length
        .is_some_and(|length| length > max_bytes)
    {
        return Err(HttpError::new(
            HttpErrorKind::Limit,
            "S3 object exceeds the requested byte limit",
        ));
    }
    let body = &raw[end..];
    if headers.chunked {
        return decode_chunked(body, max_bytes);
    }
    if let Some(expected) = headers.content_length
        && expected != body.len()
    {
        return Err(HttpError::new(
            HttpErrorKind::Protocol,
            "S3 response body does not match Content-Length",
        ));
    }
    if body.len() > max_bytes {
        return Err(HttpError::new(
            HttpErrorKind::Limit,
            "S3 object exceeds the requested byte limit",
        ));
    }
    Ok(body.to_vec())
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
            1024,
        )
        .expect("valid request");
        let request = String::from_utf8(request).expect("ASCII request");
        assert!(request.starts_with("GET /results/folder/a%20b/%C3%A9.parquet?X-Amz-Algorithm="));
        assert!(request.contains("\r\nHost: minio\r\n"));
        assert!(build_get_request("minio", "results.test", "key", None, 1).is_ok());
        assert!(build_get_request("bad\r\nname", "results", "key", None, 1).is_err());
        assert!(build_get_request("minio", "results", "key", Some("x=1\r\ny=2"), 1).is_err());
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
}
