use std::collections::BTreeSet;

use crate::http::{
    HttpError, HttpErrorKind, MAX_CONTINUATION_TOKEN_BYTES, MAX_LIST_KEYS, MAX_LIST_PREFIX_BYTES,
};

pub const MAX_LIST_BODY_BYTES: usize = 4 * 1024 * 1024;
const MAX_LIST_XML_DEPTH: usize = 3;
const MAX_LIST_XML_EVENTS: usize = 32_768;
const MAX_LIST_XML_TEXT_BYTES: usize = 3 * 1024 * 1024;
const MAX_OBJECT_KEY_BYTES: usize = 1_024;
const MAX_ETAG_BYTES: usize = 1_024;
const MAX_LAST_MODIFIED_BYTES: usize = 128;
const MAX_STORAGE_CLASS_BYTES: usize = 64;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParsedListedObject {
    pub key: String,
    pub size: u64,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParsedListPage {
    pub objects: Vec<ParsedListedObject>,
    pub is_truncated: bool,
    pub next_continuation_token: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Element {
    Root,
    Name,
    Prefix,
    KeyCount,
    MaxKeys,
    IsTruncated,
    ContinuationToken,
    NextContinuationToken,
    Contents,
    Key,
    LastModified,
    Etag,
    Size,
    StorageClass,
}

impl Element {
    const fn name(self) -> &'static [u8] {
        match self {
            Self::Root => b"ListBucketResult",
            Self::Name => b"Name",
            Self::Prefix => b"Prefix",
            Self::KeyCount => b"KeyCount",
            Self::MaxKeys => b"MaxKeys",
            Self::IsTruncated => b"IsTruncated",
            Self::ContinuationToken => b"ContinuationToken",
            Self::NextContinuationToken => b"NextContinuationToken",
            Self::Contents => b"Contents",
            Self::Key => b"Key",
            Self::LastModified => b"LastModified",
            Self::Etag => b"ETag",
            Self::Size => b"Size",
            Self::StorageClass => b"StorageClass",
        }
    }

    const fn is_scalar(self) -> bool {
        !matches!(self, Self::Root | Self::Contents)
    }

    const fn text_limit(self) -> usize {
        match self {
            Self::Name => 63,
            Self::Prefix => MAX_LIST_PREFIX_BYTES,
            Self::ContinuationToken | Self::NextContinuationToken => MAX_CONTINUATION_TOKEN_BYTES,
            Self::Key => MAX_OBJECT_KEY_BYTES,
            Self::Etag => MAX_ETAG_BYTES,
            Self::LastModified => MAX_LAST_MODIFIED_BYTES,
            Self::StorageClass => MAX_STORAGE_CLASS_BYTES,
            Self::KeyCount | Self::MaxKeys | Self::Size => 20,
            Self::IsTruncated => 5,
            Self::Root | Self::Contents => 0,
        }
    }
}

#[derive(Default)]
struct ObjectBuilder {
    key: Option<String>,
    size: Option<u64>,
    etag: Option<String>,
    last_modified: Option<String>,
    storage_class_seen: bool,
}

#[derive(Default)]
struct PageBuilder {
    name: Option<String>,
    prefix: Option<String>,
    key_count: Option<u32>,
    max_keys: Option<u32>,
    is_truncated: Option<bool>,
    continuation_token: Option<String>,
    next_continuation_token: Option<String>,
    objects: Vec<ParsedListedObject>,
    object: Option<ObjectBuilder>,
    stack: Vec<Element>,
    text: String,
    total_text_bytes: usize,
    events: usize,
    root_closed: bool,
    declaration_seen: bool,
}

const fn protocol(message: &'static str) -> HttpError {
    HttpError {
        kind: HttpErrorKind::Protocol,
        status: None,
        message,
    }
}

const fn limit(message: &'static str) -> HttpError {
    HttpError {
        kind: HttpErrorKind::Limit,
        status: None,
        message,
    }
}

fn set_once<T>(slot: &mut Option<T>, value: T) -> Result<(), HttpError> {
    if slot.replace(value).is_some() {
        return Err(protocol("S3 list XML contains a duplicate field"));
    }
    Ok(())
}

fn parse_u32(value: &str) -> Result<u32, HttpError> {
    if value.is_empty()
        || !value.bytes().all(|byte| byte.is_ascii_digit())
        || (value.len() > 1 && value.starts_with('0'))
    {
        return Err(protocol("S3 list XML contains an invalid integer"));
    }
    value
        .parse::<u32>()
        .map_err(|_error| protocol("S3 list XML integer is out of range"))
}

fn parse_u64(value: &str) -> Result<u64, HttpError> {
    if value.is_empty()
        || !value.bytes().all(|byte| byte.is_ascii_digit())
        || (value.len() > 1 && value.starts_with('0'))
    {
        return Err(protocol("S3 list XML contains an invalid integer"));
    }
    value
        .parse::<u64>()
        .map_err(|_error| protocol("S3 list XML integer is out of range"))
}

fn child_element(parent: Option<Element>, name: &[u8]) -> Result<Element, HttpError> {
    let element = match parent {
        None if name == Element::Root.name() => Element::Root,
        Some(Element::Root) => match name {
            b"Name" => Element::Name,
            b"Prefix" => Element::Prefix,
            b"KeyCount" => Element::KeyCount,
            b"MaxKeys" => Element::MaxKeys,
            b"IsTruncated" => Element::IsTruncated,
            b"ContinuationToken" => Element::ContinuationToken,
            b"NextContinuationToken" => Element::NextContinuationToken,
            b"Contents" => Element::Contents,
            _ => return Err(protocol("S3 list XML contains an unexpected element")),
        },
        Some(Element::Contents) => match name {
            b"Key" => Element::Key,
            b"LastModified" => Element::LastModified,
            b"ETag" => Element::Etag,
            b"Size" => Element::Size,
            b"StorageClass" => Element::StorageClass,
            _ => return Err(protocol("S3 list XML contains an unexpected object field")),
        },
        _ => return Err(protocol("S3 list XML has an invalid nesting shape")),
    };
    Ok(element)
}

impl PageBuilder {
    fn event(&mut self) -> Result<(), HttpError> {
        self.events = self
            .events
            .checked_add(1)
            .ok_or_else(|| limit("S3 list XML event count overflowed"))?;
        if self.events > MAX_LIST_XML_EVENTS {
            return Err(limit("S3 list XML exceeds the event limit"));
        }
        Ok(())
    }

    fn start(&mut self, name: &[u8], attributes: usize) -> Result<(), HttpError> {
        if self.root_closed {
            return Err(protocol("S3 list XML contains data after the root"));
        }
        let element = child_element(self.stack.last().copied(), name)?;
        if attributes != 0 && element != Element::Root {
            return Err(protocol("S3 list XML contains unexpected attributes"));
        }
        if self.stack.len() >= MAX_LIST_XML_DEPTH {
            return Err(limit("S3 list XML exceeds the nesting limit"));
        }
        if element == Element::Contents && self.object.replace(ObjectBuilder::default()).is_some() {
            return Err(protocol("S3 list XML contains nested objects"));
        }
        self.stack.push(element);
        self.text.clear();
        Ok(())
    }

    fn append_text(&mut self, value: &str) -> Result<(), HttpError> {
        self.total_text_bytes = self
            .total_text_bytes
            .checked_add(value.len())
            .ok_or_else(|| limit("S3 list XML text count overflowed"))?;
        if self.total_text_bytes > MAX_LIST_XML_TEXT_BYTES {
            return Err(limit("S3 list XML exceeds the text byte limit"));
        }
        let Some(element) = self.stack.last().copied() else {
            if value.bytes().all(|byte| byte.is_ascii_whitespace()) {
                return Ok(());
            }
            return Err(protocol("S3 list XML contains text outside the root"));
        };
        if !element.is_scalar() {
            if value.bytes().all(|byte| byte.is_ascii_whitespace()) {
                return Ok(());
            }
            return Err(protocol("S3 list XML contains text in a container"));
        }
        if self.text.len().saturating_add(value.len()) > element.text_limit() {
            return Err(limit("S3 list XML field exceeds its byte limit"));
        }
        self.text.push_str(value);
        Ok(())
    }

    fn append_reference(&mut self, reference: &str) -> Result<(), HttpError> {
        let value = match reference {
            "amp" => '&',
            "lt" => '<',
            "gt" => '>',
            "apos" => '\'',
            "quot" => '"',
            _ => {
                reference.strip_prefix("#x").map_or_else(
                    || {
                        reference
                            .strip_prefix('#')
                            .and_then(|decimal| decimal.parse::<u32>().ok())
                    },
                    |hex| u32::from_str_radix(hex, 16).ok(),
                )
                .and_then(char::from_u32)
                .filter(|character| {
                    matches!(*character, '\u{9}' | '\n' | '\r')
                        || matches!(*character as u32, 0x20..=0xd7ff | 0xe000..=0xfffd | 0x0001_0000..=0x0010_ffff)
                })
                .ok_or_else(|| protocol("S3 list XML contains an unsupported entity reference"))?
            }
        };
        let mut encoded = [0_u8; 4];
        self.append_text(value.encode_utf8(&mut encoded))
    }

    fn end(&mut self, name: &[u8]) -> Result<(), HttpError> {
        let element = self
            .stack
            .pop()
            .ok_or_else(|| protocol("S3 list XML contains an unmatched end element"))?;
        if element.name() != name {
            return Err(protocol("S3 list XML end element does not match"));
        }
        if element.is_scalar() {
            self.finish_scalar(element)?;
        } else if element == Element::Contents {
            self.finish_object()?;
        } else {
            self.root_closed = true;
        }
        self.text.clear();
        Ok(())
    }

    #[inline(never)]
    fn finish_scalar(&mut self, element: Element) -> Result<(), HttpError> {
        let text = std::mem::take(&mut self.text);
        if matches!(
            element,
            Element::Name
                | Element::Prefix
                | Element::KeyCount
                | Element::MaxKeys
                | Element::IsTruncated
                | Element::ContinuationToken
                | Element::NextContinuationToken
        ) {
            return self.finish_page_scalar(element, text);
        }
        self.finish_object_scalar(element, text)
    }

    #[inline(never)]
    fn finish_page_scalar(&mut self, element: Element, text: String) -> Result<(), HttpError> {
        match element {
            Element::Name => set_once(&mut self.name, text),
            Element::Prefix => set_once(&mut self.prefix, text),
            Element::KeyCount => {
                let value = parse_u32(&text)?;
                set_once(&mut self.key_count, value)
            }
            Element::MaxKeys => {
                let value = parse_u32(&text)?;
                set_once(&mut self.max_keys, value)
            }
            Element::IsTruncated => {
                let value = match text.as_str() {
                    "true" => true,
                    "false" => false,
                    _ => return Err(protocol("S3 list XML has an invalid truncation flag")),
                };
                set_once(&mut self.is_truncated, value)
            }
            Element::ContinuationToken => set_once(&mut self.continuation_token, text),
            Element::NextContinuationToken => set_once(&mut self.next_continuation_token, text),
            Element::Root
            | Element::Contents
            | Element::Key
            | Element::Size
            | Element::Etag
            | Element::LastModified
            | Element::StorageClass => Err(protocol("S3 list XML field has the wrong parent")),
        }
    }

    #[inline(never)]
    fn finish_object_scalar(&mut self, element: Element, text: String) -> Result<(), HttpError> {
        let object = self
            .object
            .as_mut()
            .ok_or_else(|| protocol("S3 list XML object field is outside an object"))?;
        match element {
            Element::Key => set_once(&mut object.key, text),
            Element::Size => {
                let value = parse_u64(&text)?;
                set_once(&mut object.size, value)
            }
            Element::Etag => set_once(&mut object.etag, text),
            Element::LastModified => set_once(&mut object.last_modified, text),
            Element::StorageClass => {
                if object.storage_class_seen {
                    return Err(protocol("S3 list XML contains a duplicate field"));
                }
                object.storage_class_seen = true;
                if text.is_empty() {
                    return Err(protocol("S3 list XML contains an empty storage class"));
                }
                Ok(())
            }
            Element::Root
            | Element::Contents
            | Element::Name
            | Element::Prefix
            | Element::KeyCount
            | Element::MaxKeys
            | Element::IsTruncated
            | Element::ContinuationToken
            | Element::NextContinuationToken => {
                Err(protocol("S3 list XML field has the wrong parent"))
            }
        }
    }

    fn finish_object(&mut self) -> Result<(), HttpError> {
        let object = self
            .object
            .take()
            .ok_or_else(|| protocol("S3 list XML object is incomplete"))?;
        let key = object
            .key
            .filter(|key| !key.is_empty())
            .ok_or_else(|| protocol("S3 list XML object has no key"))?;
        let size = object
            .size
            .ok_or_else(|| protocol("S3 list XML object has no size"))?;
        self.objects.push(ParsedListedObject {
            key,
            size,
            etag: object.etag,
            last_modified: object.last_modified,
        });
        Ok(())
    }

    fn finish(
        self,
        bucket: &str,
        prefix: &str,
        requested_max_keys: u32,
        requested_token: Option<&str>,
    ) -> Result<ParsedListPage, HttpError> {
        if !self.root_closed || !self.stack.is_empty() || self.object.is_some() {
            return Err(protocol("S3 list XML has no complete root"));
        }
        if self.name.as_deref() != Some(bucket) || self.prefix.as_deref() != Some(prefix) {
            return Err(protocol(
                "S3 list XML does not match the requested bucket or prefix",
            ));
        }
        if self.max_keys != Some(requested_max_keys) {
            return Err(protocol(
                "S3 list XML does not match the requested max-keys",
            ));
        }
        if self.continuation_token.as_deref() != requested_token {
            return Err(protocol(
                "S3 list XML does not match the requested continuation token",
            ));
        }
        let object_count = u32::try_from(self.objects.len())
            .map_err(|_error| limit("S3 list object count is not representable"))?;
        if object_count > requested_max_keys || object_count > MAX_LIST_KEYS {
            return Err(limit("S3 list response exceeds the requested object limit"));
        }
        if self.key_count != Some(object_count) {
            return Err(protocol(
                "S3 list XML key count does not match the returned objects",
            ));
        }
        let mut keys = BTreeSet::new();
        if self
            .objects
            .iter()
            .any(|object| !object.key.starts_with(prefix) || !keys.insert(object.key.as_str()))
        {
            return Err(protocol(
                "S3 list XML contains an out-of-prefix or duplicate key",
            ));
        }
        let is_truncated = self
            .is_truncated
            .ok_or_else(|| protocol("S3 list XML has no truncation flag"))?;
        match (is_truncated, self.next_continuation_token.as_deref()) {
            (true, Some(token)) if !token.is_empty() => {}
            (false, None) => {}
            _ => {
                return Err(protocol(
                    "S3 list XML truncation and continuation token disagree",
                ));
            }
        }
        Ok(ParsedListPage {
            objects: self.objects,
            is_truncated,
            next_continuation_token: self.next_continuation_token,
        })
    }
}

fn consume_declaration(
    page: &mut PageBuilder,
    remaining: &str,
    cursor: usize,
) -> Result<usize, HttpError> {
    if page.declaration_seen || !page.stack.is_empty() || page.root_closed || cursor != 0 {
        return Err(protocol("S3 list XML declaration is misplaced"));
    }
    let end = remaining
        .find("?>")
        .filter(|offset| *offset <= 96)
        .ok_or_else(|| protocol("S3 list XML declaration is malformed"))?;
    let declaration = &remaining[..end + 2];
    if !matches!(
        declaration,
        "<?xml version=\"1.0\"?>"
            | "<?xml version=\"1.0\" encoding=\"UTF-8\"?>"
            | "<?xml version=\"1.0\" encoding=\"utf-8\"?>"
            | "<?xml version='1.0'?>"
            | "<?xml version='1.0' encoding='UTF-8'?>"
            | "<?xml version='1.0' encoding='utf-8'?>"
    ) {
        return Err(protocol("S3 list XML uses an unsupported declaration"));
    }
    page.declaration_seen = true;
    Ok(cursor + end + 2)
}

fn consume_end(page: &mut PageBuilder, remaining: &str, cursor: usize) -> Result<usize, HttpError> {
    let end = remaining
        .find('>')
        .filter(|offset| *offset <= 66)
        .ok_or_else(|| protocol("S3 list XML end element is malformed"))?;
    let name = &remaining.as_bytes()[2..end];
    if !valid_element_name(name) {
        return Err(protocol("S3 list XML end element is malformed"));
    }
    page.end(name)?;
    Ok(cursor + end + 1)
}

fn consume_start(
    page: &mut PageBuilder,
    remaining: &str,
    cursor: usize,
) -> Result<usize, HttpError> {
    let end = remaining
        .find('>')
        .filter(|offset| *offset <= 256)
        .ok_or_else(|| protocol("S3 list XML start element is malformed"))?;
    let mut content = &remaining[1..end];
    let self_closing = content.ends_with('/');
    if self_closing {
        content = content[..content.len() - 1].trim_end_matches([' ', '\t']);
    }
    let name_end = content
        .bytes()
        .position(|byte| byte.is_ascii_whitespace())
        .unwrap_or(content.len());
    let name = &content.as_bytes()[..name_end];
    if !valid_element_name(name) {
        return Err(protocol("S3 list XML start element is malformed"));
    }
    let attributes = &content[name_end..];
    let attribute_count = if attributes.is_empty() {
        0
    } else if name == Element::Root.name()
        && matches!(
            attributes,
            " xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\""
                | " xmlns='http://s3.amazonaws.com/doc/2006-03-01/'"
        )
    {
        1
    } else {
        return Err(protocol("S3 list XML contains unexpected attributes"));
    };
    page.start(name, attribute_count)?;
    if self_closing {
        page.event()?;
        page.end(name)?;
    }
    Ok(cursor + end + 1)
}

fn consume_reference(
    page: &mut PageBuilder,
    remaining: &str,
    cursor: usize,
) -> Result<usize, HttpError> {
    let end = remaining
        .find(';')
        .filter(|offset| (2..=16).contains(offset))
        .ok_or_else(|| protocol("S3 list XML entity reference is malformed"))?;
    page.append_reference(&remaining[1..end])?;
    Ok(cursor + end + 1)
}

fn consume_text(
    page: &mut PageBuilder,
    remaining: &str,
    cursor: usize,
) -> Result<usize, HttpError> {
    let next_markup = remaining.find(['<', '&']).unwrap_or(remaining.len());
    let text = &remaining[..next_markup];
    if text.contains("]]>") {
        return Err(protocol("S3 list XML contains unsupported markup"));
    }
    page.append_text(text)?;
    Ok(cursor + next_markup)
}

fn consume_event(page: &mut PageBuilder, source: &str, cursor: usize) -> Result<usize, HttpError> {
    page.event()?;
    let remaining = &source[cursor..];
    if remaining.starts_with("<?xml") {
        return consume_declaration(page, remaining, cursor);
    }
    if remaining.starts_with("<!") || remaining.starts_with("<?") {
        return Err(protocol("S3 list XML contains unsupported markup"));
    }
    if remaining.starts_with("</") {
        return consume_end(page, remaining, cursor);
    }
    if remaining.starts_with('<') {
        return consume_start(page, remaining, cursor);
    }
    if remaining.starts_with('&') {
        return consume_reference(page, remaining, cursor);
    }
    consume_text(page, remaining, cursor)
}

pub fn parse_list_page(
    body: &[u8],
    bucket: &str,
    prefix: &str,
    max_keys: u32,
    continuation_token: Option<&str>,
) -> Result<ParsedListPage, HttpError> {
    if body.len() > MAX_LIST_BODY_BYTES {
        return Err(limit("S3 list body exceeds the byte limit"));
    }
    let source = std::str::from_utf8(body)
        .map_err(|_error| protocol("S3 list XML text is not valid UTF-8"))?;
    if source.chars().any(|character| {
        !(matches!(character, '\u{9}' | '\n' | '\r')
            || matches!(character as u32, 0x20..=0xd7ff | 0xe000..=0xfffd | 0x0001_0000..=0x0010_ffff))
    }) {
        return Err(protocol("S3 list XML contains an invalid character"));
    }
    let mut page = PageBuilder::default();
    let mut cursor = 0_usize;
    while cursor < source.len() {
        cursor = consume_event(&mut page, source, cursor)?;
    }
    page.finish(bucket, prefix, max_keys, continuation_token)
}

fn valid_element_name(name: &[u8]) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;

    use super::*;

    fn response(contents: &str, tail: &str, key_count: usize, max_keys: u32) -> String {
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?><ListBucketResult><Name>results</Name><Prefix>exports/</Prefix><KeyCount>{key_count}</KeyCount><MaxKeys>{max_keys}</MaxKeys>{contents}<IsTruncated>{}</IsTruncated>{}</ListBucketResult>",
            if tail.is_empty() { "false" } else { "true" },
            tail
        )
    }

    #[test]
    fn parses_empty_and_exact_ordered_typed_pages() {
        let empty = response("", "", 0, 3);
        assert_eq!(
            parse_list_page(empty.as_bytes(), "results", "exports/", 3, None).expect("empty page"),
            ParsedListPage {
                objects: Vec::new(),
                is_truncated: false,
                next_continuation_token: None,
            }
        );

        let contents = concat!(
            "<Contents><Key>exports/space %25/é&amp;x.parquet</Key>",
            "<LastModified>2026-09-01T22:01:02.003Z</LastModified>",
            "<ETag>&quot;abc-2&quot;</ETag><Size>18446744073709551615</Size>",
            "<StorageClass>STANDARD</StorageClass></Contents>",
            "<Contents><Key>exports/z.parquet</Key><Size>0</Size></Contents>"
        );
        let page = parse_list_page(
            response(contents, "", 2, 3).as_bytes(),
            "results",
            "exports/",
            3,
            None,
        )
        .expect("typed page");
        assert_eq!(
            page.objects,
            vec![
                ParsedListedObject {
                    key: "exports/space %25/é&x.parquet".to_owned(),
                    size: u64::MAX,
                    etag: Some("\"abc-2\"".to_owned()),
                    last_modified: Some("2026-09-01T22:01:02.003Z".to_owned()),
                },
                ParsedListedObject {
                    key: "exports/z.parquet".to_owned(),
                    size: 0,
                    etag: None,
                    last_modified: None,
                },
            ]
        );
    }

    #[test]
    fn pagination_is_caller_driven_and_explicit() {
        let contents = "<Contents><Key>exports/a</Key><Size>1</Size></Contents>";
        let first = response(
            contents,
            "<NextContinuationToken>opaque/next+2</NextContinuationToken>",
            1,
            1,
        );
        let first =
            parse_list_page(first.as_bytes(), "results", "exports/", 1, None).expect("first page");
        assert!(first.is_truncated);
        assert_eq!(
            first.next_continuation_token.as_deref(),
            Some("opaque/next+2")
        );

        let second = "<ListBucketResult><Name>results</Name><Prefix>exports/</Prefix><KeyCount>0</KeyCount><MaxKeys>1</MaxKeys><ContinuationToken>opaque/next+2</ContinuationToken><IsTruncated>false</IsTruncated></ListBucketResult>";
        assert!(
            parse_list_page(
                second.as_bytes(),
                "results",
                "exports/",
                1,
                Some("opaque/next+2")
            )
            .is_ok()
        );
    }

    #[test]
    fn object_and_disclosure_limits_return_no_partial_page() {
        let one = "<Contents><Key>exports/a</Key><Size>1</Size></Contents>";
        let over = response(&format!("{one}{one}"), "", 2, 1);
        assert_eq!(
            parse_list_page(over.as_bytes(), "results", "exports/", 1, None)
                .expect_err("max plus one must fail")
                .kind,
            HttpErrorKind::Limit
        );
        for tail in ["<NextContinuationToken>token</NextContinuationToken>", ""] {
            let mut xml = response(one, tail, 1, 1);
            if tail.is_empty() {
                xml = xml.replace(
                    "<IsTruncated>false</IsTruncated>",
                    "<IsTruncated>true</IsTruncated>",
                );
            } else {
                xml = xml.replace(
                    "<IsTruncated>true</IsTruncated>",
                    "<IsTruncated>false</IsTruncated>",
                );
            }
            assert!(parse_list_page(xml.as_bytes(), "results", "exports/", 1, None).is_err());
        }
    }

    #[test]
    fn accepts_exactly_one_thousand_objects_and_rejects_one_more() {
        let contents = (0..MAX_LIST_KEYS).fold(String::new(), |mut output, index| {
            write!(
                output,
                "<Contents><Key>exports/{index:04}</Key><Size>{index}</Size></Contents>"
            )
            .expect("writing into a String is infallible");
            output
        });
        let exact = response(
            &contents,
            "<NextContinuationToken>opaque-next</NextContinuationToken>",
            MAX_LIST_KEYS as usize,
            MAX_LIST_KEYS,
        );
        let page = parse_list_page(exact.as_bytes(), "results", "exports/", MAX_LIST_KEYS, None)
            .expect("the documented maximum page");
        assert_eq!(page.objects.len(), MAX_LIST_KEYS as usize);
        assert!(page.is_truncated);

        let over_contents =
            format!("{contents}<Contents><Key>exports/1000</Key><Size>1000</Size></Contents>");
        let over = response(
            &over_contents,
            "<NextContinuationToken>opaque-next</NextContinuationToken>",
            MAX_LIST_KEYS as usize + 1,
            MAX_LIST_KEYS,
        );
        assert_eq!(
            parse_list_page(over.as_bytes(), "results", "exports/", MAX_LIST_KEYS, None,)
                .expect_err("max plus one must return no page")
                .kind,
            HttpErrorKind::Limit
        );
    }

    #[test]
    fn hostile_xml_is_bounded_and_rejected() {
        let fixtures = [
            "<!DOCTYPE x [<!ENTITY y SYSTEM \"file:///etc/passwd\">]><ListBucketResult></ListBucketResult>",
            "<ListBucketResult><Name>&external;</Name></ListBucketResult>",
            "<ListBucketResult><Name>results</Name><Name>results</Name></ListBucketResult>",
            "<ListBucketResult><Contents><Key><Nested>x</Nested></Key></Contents></ListBucketResult>",
            "<ListBucketResult><Unknown>x</Unknown></ListBucketResult>",
            "<ListBucketResult><Name>results</Prefix></ListBucketResult>",
        ];
        for fixture in fixtures {
            assert!(
                parse_list_page(fixture.as_bytes(), "results", "exports/", 1, None).is_err(),
                "accepted hostile fixture: {fixture}"
            );
        }
        assert_eq!(
            parse_list_page(&[0xff, 0xfe], "results", "exports/", 1, None)
                .expect_err("invalid UTF-8 must fail")
                .kind,
            HttpErrorKind::Protocol
        );
        let oversized = vec![b'x'; MAX_LIST_BODY_BYTES + 1];
        assert_eq!(
            parse_list_page(&oversized, "results", "exports/", 1, None)
                .expect_err("oversized body must fail")
                .kind,
            HttpErrorKind::Limit
        );
    }

    #[test]
    fn deterministic_xml_mutation_corpus_never_panics() {
        let seed = response(
            "<Contents><Key>exports/a</Key><Size>1</Size></Contents>",
            "",
            1,
            1,
        );
        let positions = [
            0,
            1,
            seed.len() / 4,
            seed.len() / 2,
            seed.len().saturating_sub(1),
            seed.len(),
        ];
        for position in positions {
            for mutation in [b'\0', b'&', b'<', b'>', b'%', 0xff] {
                let mut candidate = seed.as_bytes().to_vec();
                candidate.insert(position, mutation);
                let _bounded_result = parse_list_page(&candidate, "results", "exports/", 1, None);
            }
        }
    }
}
