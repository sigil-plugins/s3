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

use bindings::exports::sigil::s3::client::{Error, ErrorClass, GetOptions, Guest};
use bindings::sigil::host::net;
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
        HttpErrorKind::Limit => ErrorClass::Limit,
        HttpErrorKind::Unsupported => ErrorClass::Unsupported,
    };
    Error {
        class,
        status: error.status,
        message: error.message.to_owned(),
    }
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
fn get_object(options: &GetOptions) -> Result<Vec<u8>, Error> {
    let max_bytes = usize::try_from(options.max_bytes).map_err(|_error| Error {
        class: ErrorClass::Limit,
        status: None,
        message: "requested object byte limit is not representable".to_owned(),
    })?;
    let request = http::build_get_request(
        &options.endpoint,
        &options.bucket,
        &options.key,
        options.presigned_query.as_deref(),
        max_bytes,
    )
    .map_err(|error| client_error(&error))?;
    let wire_limit = max_bytes
        .checked_add(http::MAX_WIRE_OVERHEAD_BYTES)
        .ok_or_else(|| Error {
            class: ErrorClass::Limit,
            status: None,
            message: "S3 response byte limit overflowed".to_owned(),
        })?;

    let stream = net::connect(&options.endpoint).map_err(net_error)?;
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

impl Guest for S3 {
    fn get_object(options: GetOptions) -> Result<Vec<u8>, Error> {
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
}
