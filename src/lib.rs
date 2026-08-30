#![deny(unsafe_code)]

#[allow(unsafe_code, clippy::all, clippy::nursery, clippy::pedantic)]
mod bindings {
    wit_bindgen::generate!({
        path: "wit",
        world: "s3",
        generate_all,
    });
}

mod http;

use bindings::exports::sigil::s3::client::{Auth, Error, ErrorClass, Guest, ObjectOptions};
use bindings::sigil::{host1_0_0::net, host1_1_0::sigv4};
use http::{HttpError, HttpErrorKind};

const HOST_READ_BYTES: u32 = 1024 * 1024;

struct S3;

fn client_error(error: &HttpError) -> Error {
    let class = match error.kind {
        HttpErrorKind::InvalidRequest => ErrorClass::InvalidRequest,
        HttpErrorKind::Protocol => ErrorClass::Protocol,
        HttpErrorKind::Denied => ErrorClass::Denied,
        HttpErrorKind::NotFound => ErrorClass::NotFound,
        HttpErrorKind::Server => ErrorClass::Server,
        HttpErrorKind::ClockSkew => ErrorClass::ClockSkew,
        HttpErrorKind::Limit => ErrorClass::Limit,
        HttpErrorKind::Unsupported => ErrorClass::Unsupported,
    };
    Error {
        class,
        status: error.status,
        message: error.message.to_owned(),
    }
}

const fn sigv4_error_class(error: sigv4::Error) -> ErrorClass {
    match error {
        sigv4::Error::Denied => ErrorClass::Denied,
        sigv4::Error::InvalidRequest => ErrorClass::InvalidRequest,
        sigv4::Error::Limit => ErrorClass::Limit,
        sigv4::Error::Unavailable
        | sigv4::Error::Timeout
        | sigv4::Error::Tls
        | sigv4::Error::Io
        | sigv4::Error::Expired
        | sigv4::Error::Internal => ErrorClass::Transport,
    }
}

fn sigv4_error(error: sigv4::Error) -> Error {
    let class = sigv4_error_class(error);
    let message = match error {
        sigv4::Error::Denied => "signed exchange was denied",
        sigv4::Error::InvalidRequest => "signed exchange request was invalid",
        sigv4::Error::Limit => "signed exchange byte limit was exceeded",
        sigv4::Error::Expired => "signed exchange expired",
        sigv4::Error::Unavailable
        | sigv4::Error::Timeout
        | sigv4::Error::Tls
        | sigv4::Error::Io
        | sigv4::Error::Internal => "signed exchange failed",
    };
    Error {
        class,
        status: None,
        message: message.to_owned(),
    }
}

fn object_limit(max_bytes: u32) -> Result<usize, Error> {
    usize::try_from(max_bytes).map_err(|_error| Error {
        class: ErrorClass::Limit,
        status: None,
        message: "requested object byte limit is not representable".to_owned(),
    })
}

fn wire_limit(max_bytes: usize) -> Result<usize, Error> {
    max_bytes
        .checked_add(http::MAX_WIRE_OVERHEAD_BYTES)
        .ok_or_else(|| Error {
            class: ErrorClass::Limit,
            status: None,
            message: "S3 response byte limit overflowed".to_owned(),
        })
}

#[allow(
    clippy::needless_pass_by_value,
    reason = "Result::map_err supplies the generated host error by value"
)]
fn net_error(error: net::Error) -> Error {
    let (class, message) = match error {
        net::Error::Denied(_) => (ErrorClass::Denied, "network endpoint access was denied"),
        net::Error::Limit(_) => (ErrorClass::Limit, "network byte limit was exceeded"),
        net::Error::Unavailable(_)
        | net::Error::Timeout(_)
        | net::Error::Tls(_)
        | net::Error::Io(_) => (ErrorClass::Transport, "network operation failed"),
    };
    Error {
        class,
        status: None,
        message: message.to_owned(),
    }
}

#[inline(never)]
fn raw_get_object(
    endpoint: &str,
    bucket: &str,
    key: &str,
    query: Option<&str>,
    authority: Option<&str>,
    max_bytes: usize,
) -> Result<Vec<u8>, Error> {
    let request = http::build_get_request(endpoint, bucket, key, query, authority, max_bytes)
        .map_err(|error| client_error(&error))?;
    let wire_limit = wire_limit(max_bytes)?;

    let stream = net::connect(endpoint).map_err(net_error)?;
    let result = (|| {
        stream.write_all(&request).map_err(net_error)?;
        stream.flush().map_err(net_error)?;
        let mut response = Vec::new();
        loop {
            let remaining = wire_limit.saturating_sub(response.len());
            if remaining == 0 {
                return Err(Error {
                    class: ErrorClass::Limit,
                    status: None,
                    message: "S3 response exceeds the wire byte limit".to_owned(),
                });
            }
            let Some(useful) =
                http::next_read_size(&response, max_bytes).map_err(|error| client_error(&error))?
            else {
                break;
            };
            let requested = u32::try_from(remaining.min(useful))
                .unwrap_or(u32::MAX)
                .min(HOST_READ_BYTES);
            let chunk = stream.read(requested).map_err(net_error)?;
            if chunk.is_empty() {
                break;
            }
            response.extend_from_slice(&chunk);
            if response.len() > wire_limit {
                return Err(Error {
                    class: ErrorClass::Limit,
                    status: None,
                    message: "S3 response exceeds the wire byte limit".to_owned(),
                });
            }
        }
        http::decode_response(&response, max_bytes).map_err(|error| client_error(&error))
    })();
    stream.close();
    result
}

fn valid_signing_grant(grant: &str) -> bool {
    (1..=64).contains(&grant.len())
        && grant.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
        && grant.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
        })
}

fn build_sigv4_request(
    signing_grant: &str,
    bucket: &str,
    key: &str,
    max_bytes: usize,
) -> Result<sigv4::Request, Error> {
    if !valid_signing_grant(signing_grant) {
        return Err(Error {
            class: ErrorClass::InvalidRequest,
            status: None,
            message: "invalid SigV4 signing grant".to_owned(),
        });
    }
    let canonical_uri = http::build_sigv4_canonical_uri(bucket, key, max_bytes)
        .map_err(|error| client_error(&error))?;
    let max_response_bytes = u64::try_from(wire_limit(max_bytes)?).map_err(|_error| Error {
        class: ErrorClass::Limit,
        status: None,
        message: "S3 response byte limit is not representable".to_owned(),
    })?;
    Ok(sigv4::Request {
        signing_grant: signing_grant.to_owned(),
        method: sigv4::Method::Get,
        canonical_uri,
        canonical_query: String::new(),
        headers: Vec::new(),
        max_response_bytes,
    })
}

fn signed_get_object(
    signing_grant: &str,
    bucket: &str,
    key: &str,
    max_bytes: usize,
) -> Result<Vec<u8>, Error> {
    let request = build_sigv4_request(signing_grant, bucket, key, max_bytes)?;
    let wire_limit = wire_limit(max_bytes)?;
    let response = sigv4::exchange(&request).map_err(sigv4_error)?;
    let result = (|| {
        let mut raw = Vec::new();
        loop {
            let remaining = wire_limit.saturating_sub(raw.len());
            if remaining == 0 {
                return Err(Error {
                    class: ErrorClass::Limit,
                    status: None,
                    message: "S3 response exceeds the wire byte limit".to_owned(),
                });
            }
            let Some(useful) = http::next_signed_read_size(&raw, max_bytes)
                .map_err(|error| client_error(&error))?
            else {
                break;
            };
            let requested = u32::try_from(remaining.min(useful))
                .unwrap_or(u32::MAX)
                .min(HOST_READ_BYTES);
            let chunk = response.read(requested).map_err(sigv4_error)?;
            if chunk.is_empty() {
                break;
            }
            raw.extend_from_slice(&chunk);
            if raw.len() > wire_limit {
                return Err(Error {
                    class: ErrorClass::Limit,
                    status: None,
                    message: "S3 response exceeds the wire byte limit".to_owned(),
                });
            }
        }
        http::decode_signed_response(&raw, max_bytes).map_err(|error| client_error(&error))
    })();
    response.close();
    result
}

#[inline(never)]
fn get_object(options: &ObjectOptions) -> Result<Vec<u8>, Error> {
    let max_bytes = object_limit(options.max_bytes)?;
    match &options.auth {
        Auth::Anonymous(auth) => raw_get_object(
            &auth.endpoint,
            &options.bucket,
            &options.key,
            None,
            None,
            max_bytes,
        ),
        Auth::Presigned(auth) => raw_get_object(
            &auth.endpoint,
            &options.bucket,
            &options.key,
            Some(&auth.query),
            Some(&auth.authority),
            max_bytes,
        ),
        Auth::Sigv4(signing_grant) => {
            signed_get_object(signing_grant, &options.bucket, &options.key, max_bytes)
        }
    }
}

impl Guest for S3 {
    fn get_object(options: ObjectOptions) -> Result<Vec<u8>, Error> {
        get_object(&options)
    }
}

#[allow(unsafe_code, clippy::all, clippy::nursery, clippy::pedantic)]
#[cfg(target_arch = "wasm32")]
mod export {
    use super::S3;

    crate::bindings::export!(S3 with_types_in crate::bindings);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_network_errors_are_typed_without_source_details() {
        for (source, expected) in [
            (
                net::Error::Denied("endpoint policy detail".to_owned()),
                ErrorClass::Denied,
            ),
            (
                net::Error::Limit("quota detail".to_owned()),
                ErrorClass::Limit,
            ),
            (
                net::Error::Unavailable("socket detail".to_owned()),
                ErrorClass::Transport,
            ),
            (
                net::Error::Timeout("deadline detail".to_owned()),
                ErrorClass::Transport,
            ),
            (
                net::Error::Tls("certificate detail".to_owned()),
                ErrorClass::Transport,
            ),
            (
                net::Error::Io("operating-system detail".to_owned()),
                ErrorClass::Transport,
            ),
        ] {
            let mapped = net_error(source);
            assert_eq!(mapped.class, expected);
            assert_eq!(mapped.status, None);
            assert!(!mapped.message.contains("detail"));
        }
    }

    #[test]
    fn host_sigv4_errors_are_closed_and_sanitized() {
        for (source, expected, message) in [
            (sigv4::Error::Denied, ErrorClass::Denied, "denied"),
            (
                sigv4::Error::InvalidRequest,
                ErrorClass::InvalidRequest,
                "invalid",
            ),
            (sigv4::Error::Limit, ErrorClass::Limit, "limit"),
            (sigv4::Error::Unavailable, ErrorClass::Transport, "failed"),
            (sigv4::Error::Timeout, ErrorClass::Transport, "failed"),
            (sigv4::Error::Tls, ErrorClass::Transport, "failed"),
            (sigv4::Error::Io, ErrorClass::Transport, "failed"),
            (sigv4::Error::Expired, ErrorClass::Transport, "expired"),
            (sigv4::Error::Internal, ErrorClass::Transport, "failed"),
        ] {
            let mapped = sigv4_error(source);
            assert_eq!(mapped.class, expected);
            assert_eq!(mapped.status, None);
            assert!(mapped.message.contains(message));
            assert!(!mapped.message.contains("credential"));
            assert!(!mapped.message.contains("signature"));
        }
    }

    #[test]
    fn sigv4_request_matches_the_aws_s3_path_style_canonical_shape() {
        let request = build_sigv4_request(
            "private-results",
            "examplebucket",
            "test.txt",
            4 * 1024 * 1024,
        )
        .expect("valid signed request");
        assert_eq!(request.signing_grant, "private-results");
        assert_eq!(request.method, sigv4::Method::Get);
        assert_eq!(request.canonical_uri, "/examplebucket/test.txt");
        assert!(request.canonical_query.is_empty());
        assert!(request.headers.is_empty());
        assert_eq!(
            request.max_response_bytes,
            4 * 1024 * 1024 + http::MAX_WIRE_OVERHEAD_BYTES as u64
        );
    }

    #[test]
    fn signed_request_has_no_endpoint_or_credential_input() {
        let request = build_sigv4_request("object-store", "results", "folder/a b/é+.parquet", 1)
            .expect("valid signed request");
        assert_eq!(
            request.canonical_uri,
            "/results/folder/a%20b/%C3%A9%2B.parquet"
        );
        assert!(format!("{request:?}").contains("object-store"));
        for forbidden in ["endpoint", "access-key", "secret-key", "timestamp"] {
            assert!(!format!("{request:?}").contains(forbidden));
        }
        for invalid in ["", "ObjectStore", "object.store", "object/store"] {
            assert!(build_sigv4_request(invalid, "results", "key", 1).is_err());
        }
        for key in ["../secret", "folder/./key", r"folder\key", "literal%2Fkey"] {
            assert!(build_sigv4_request("object-store", "results", key, 1).is_err());
        }
    }
}
