//! Thin wrapper over the `ipp` crate: parse a request body, build responses. Only this file touches
//! the crate's `BoundedString` newtypes and value constructors, so it can be swapped for a hand codec.

use std::collections::BTreeMap;
use std::io::Cursor;
use std::time::{SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use ipp::attribute::{IppAttribute, IppAttributeGroup, IppAttributes};
use ipp::model::IppVersion;
use ipp::parser::IppParser;
use ipp::request::IppRequestResponse;
use ipp::value::{IppDateTime, IppName, IppTextValue, IppValue};
use thiserror::Error;

pub use ipp::model::DelimiterTag as Group;
pub use ipp::model::StatusCode as Status;

#[derive(Debug, Error)]
pub enum CodecError {
    #[error("malformed IPP request: {0}")]
    Parse(String),
}

/// A parsed IPP request. `payload` = everything after the end-of-attributes tag (the document).
#[derive(Debug)]
pub struct Request {
    pub version: u16,
    pub op: u16,
    pub request_id: i32,
    pub attrs: IppAttributes,
    pub payload: Bytes,
}

impl Request {
    /// First value of `name` in `group` (or in any group when `group` is None).
    pub fn get(&self, group: Option<Group>, name: &str) -> Option<&IppValue> {
        let groups = self
            .attrs
            .groups()
            .iter()
            .filter(|g| group.is_none_or(|t| g.tag() == t));
        for g in groups {
            if let Some(a) = g
                .attributes()
                .iter()
                .find(|a| a.name().as_str().eq_ignore_ascii_case(name))
            {
                return Some(a.value());
            }
        }
        None
    }
    pub fn get_str(&self, group: Option<Group>, name: &str) -> Option<String> {
        self.get(group, name).and_then(value_to_string)
    }
    pub fn get_int(&self, group: Option<Group>, name: &str) -> Option<i32> {
        match self.get(group, name)? {
            IppValue::Integer(i) | IppValue::Enum(i) => Some(*i),
            IppValue::Array(v) => v.first().and_then(|x| match x {
                IppValue::Integer(i) | IppValue::Enum(i) => Some(*i),
                _ => None,
            }),
            _ => None,
        }
    }
    pub fn get_bool(&self, group: Option<Group>, name: &str) -> Option<bool> {
        match self.get(group, name)? {
            IppValue::Boolean(b) => Some(*b),
            _ => None,
        }
    }
    /// All string values (flattening arrays).
    pub fn get_strs(&self, group: Option<Group>, name: &str) -> Vec<String> {
        match self.get(group, name) {
            Some(IppValue::Array(v)) => v.iter().filter_map(value_to_string).collect(),
            Some(v) => value_to_string(v).into_iter().collect(),
            None => vec![],
        }
    }
    /// All integer values (flattening arrays).
    pub fn get_ints(&self, group: Option<Group>, name: &str) -> Vec<i32> {
        match self.get(group, name) {
            Some(IppValue::Array(v)) => v
                .iter()
                .filter_map(|x| match x {
                    IppValue::Integer(i) | IppValue::Enum(i) => Some(*i),
                    _ => None,
                })
                .collect(),
            Some(IppValue::Integer(i)) | Some(IppValue::Enum(i)) => vec![*i],
            _ => vec![],
        }
    }
    /// A collection's member (first collection if array).
    pub fn get_member(&self, group: Option<Group>, name: &str, member: &str) -> Option<&IppValue> {
        let v = self.get(group, name)?;
        let coll = match v {
            IppValue::Collection(c) => c,
            IppValue::Array(a) => match a.first()? {
                IppValue::Collection(c) => c,
                _ => return None,
            },
            _ => return None,
        };
        coll.iter()
            .find(|(k, _)| k.as_str().eq_ignore_ascii_case(member))
            .map(|(_, v)| v)
    }
    /// Names of the operation-attributes group in order (for RFC 8011 §4.1.4 order checks).
    pub fn operation_attr_names(&self) -> Vec<String> {
        self.attrs
            .groups()
            .iter()
            .find(|g| g.tag() == Group::OperationAttributes)
            .map(|g| {
                g.attributes()
                    .iter()
                    .map(|a| a.name().as_str().to_string())
                    .collect()
            })
            .unwrap_or_default()
    }
    pub fn first_group_tag(&self) -> Option<Group> {
        self.attrs.groups().first().map(|g| g.tag())
    }
    pub fn version_major(&self) -> u8 {
        (self.version >> 8) as u8
    }
}

pub fn value_to_string(v: &IppValue) -> Option<String> {
    match v {
        IppValue::Keyword(s) | IppValue::NameWithoutLanguage(s) | IppValue::MemberAttrName(s) => {
            Some(s.as_str().to_string())
        }
        IppValue::TextWithoutLanguage(t) => Some(t.to_string()),
        IppValue::TextWithLanguage { text, .. } => Some(text.to_string()),
        IppValue::NameWithLanguage { name, .. } => Some(name.as_str().to_string()),
        IppValue::Charset(s) | IppValue::NaturalLanguage(s) => Some(s.as_str().to_string()),
        IppValue::Uri(s) | IppValue::UriScheme(s) => Some(s.as_str().to_string()),
        IppValue::MimeMediaType(s) => Some(s.as_str().to_string()),
        IppValue::Integer(i) | IppValue::Enum(i) => Some(i.to_string()),
        IppValue::Boolean(b) => Some(b.to_string()),
        IppValue::Array(a) => a.first().and_then(value_to_string),
        _ => None,
    }
}

/// Parse a complete request body.
pub fn parse(body: Bytes) -> Result<Request, CodecError> {
    if body.len() < 8 {
        return Err(CodecError::Parse("shorter than an IPP header".into()));
    }
    let version = u16::from_be_bytes([body[0], body[1]]);
    let op = u16::from_be_bytes([body[2], body[3]]);
    let request_id = i32::from_be_bytes([body[4], body[5], body[6], body[7]]);
    let parser = IppParser::new(Cursor::new(body.clone()));
    let (_header, attrs, reader) = parser
        .parse_parts()
        .map_err(|e| CodecError::Parse(e.to_string()))?;
    let pos = reader.into_inner().position() as usize;
    let payload = body.slice(pos.min(body.len())..);
    Ok(Request {
        version,
        op,
        request_id,
        attrs,
        payload,
    })
}

/// Peek at the header without parsing attributes (for early rejects on oversize bodies).
pub fn peek_header(bytes: &[u8]) -> Option<(u16, u16, i32)> {
    if bytes.len() < 8 {
        return None;
    }
    Some((
        u16::from_be_bytes([bytes[0], bytes[1]]),
        u16::from_be_bytes([bytes[2], bytes[3]]),
        i32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
    ))
}

// ---------------------------------------------------------------------------------------------
// value constructors (infallible). Strings are truncated to the IPP limit on a UTF-8 char boundary:
// the crate's `new_truncated` is `String::truncate(MAX)`, which panics inside a multi-byte char, and
// several of these strings are client-controlled (job-name, document-format …) or config
// (--location), so a plain truncate would let a print job or an env line take the daemon down.

/// Longest prefix of `s` that is at most `max` bytes and ends on a char boundary.
pub fn utf8_prefix(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

fn bounded<const N: usize>(s: &str) -> ipp::value::BoundedString<N> {
    ipp::value::BoundedString::<N>::new_truncated(utf8_prefix(s, N))
}

fn name(s: &str) -> IppName {
    bounded(s)
}
pub fn v_kw(s: &str) -> IppValue {
    IppValue::Keyword(bounded(s))
}
pub fn v_name(s: &str) -> IppValue {
    IppValue::NameWithoutLanguage(bounded(s))
}
pub fn v_text(s: &str) -> IppValue {
    // ≤ 1023 bytes on a char boundary always constructs (Short ≤ 255, Long ≤ 1023).
    let t = IppTextValue::new(utf8_prefix(s, 1023))
        .unwrap_or_else(|_| IppTextValue::new("").expect("empty text is valid"));
    IppValue::TextWithoutLanguage(t)
}
pub fn v_int(i: i32) -> IppValue {
    IppValue::Integer(i)
}
pub fn v_enum(i: i32) -> IppValue {
    IppValue::Enum(i)
}
pub fn v_bool(b: bool) -> IppValue {
    IppValue::Boolean(b)
}
pub fn v_uri(s: &str) -> IppValue {
    IppValue::Uri(bounded(s))
}
pub fn v_mime(s: &str) -> IppValue {
    IppValue::MimeMediaType(bounded(s))
}
pub fn v_charset(s: &str) -> IppValue {
    IppValue::Charset(bounded(s))
}
pub fn v_lang(s: &str) -> IppValue {
    IppValue::NaturalLanguage(bounded(s))
}
pub fn v_range(min: i32, max: i32) -> IppValue {
    IppValue::RangeOfInteger { min, max }
}
pub fn v_res(dpi: i32) -> IppValue {
    IppValue::Resolution {
        cross_feed: dpi,
        feed: dpi,
        units: 3,
    }
}
pub fn v_octets(b: &[u8]) -> IppValue {
    IppValue::OctetString(Bytes::copy_from_slice(b))
}
/// out-of-band `unknown` (0x12)
pub fn v_unknown() -> IppValue {
    IppValue::Other {
        tag: 0x12,
        data: Bytes::new(),
    }
}
/// out-of-band `no-value` (0x13)
pub fn v_no_value() -> IppValue {
    IppValue::NoValue
}
pub fn v_array(vals: Vec<IppValue>) -> IppValue {
    match vals.len() {
        1 => vals.into_iter().next().unwrap(),
        _ => IppValue::Array(vals),
    }
}
pub fn v_kws(vals: &[&str]) -> IppValue {
    v_array(vals.iter().map(|s| v_kw(s)).collect())
}
pub fn v_ints(vals: &[i32]) -> IppValue {
    v_array(vals.iter().map(|i| v_int(*i)).collect())
}
pub fn v_enums(vals: &[i32]) -> IppValue {
    v_array(vals.iter().map(|i| v_enum(*i)).collect())
}
pub fn v_mimes(vals: &[&str]) -> IppValue {
    v_array(vals.iter().map(|s| v_mime(s)).collect())
}
pub fn v_uris(vals: &[&str]) -> IppValue {
    v_array(vals.iter().map(|s| v_uri(s)).collect())
}
pub fn v_datetime(t: SystemTime) -> IppValue {
    let secs = t
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let (y, m, d, hh, mm, ss) = civil_from_unix(secs);
    IppValue::DateTime(IppDateTime {
        year: y as u16,
        month: m,
        day: d,
        hour: hh,
        minutes: mm,
        seconds: ss,
        deci_seconds: 0,
        utc_dir: '+',
        utc_hours: 0,
        utc_mins: 0,
    })
}

/// Collection builder (member order is alphabetical on the wire — the crate uses a BTreeMap).
#[derive(Default)]
pub struct Coll(BTreeMap<IppName, IppValue>);

impl Coll {
    pub fn new() -> Self {
        Coll(BTreeMap::new())
    }
    pub fn add(mut self, member: &str, v: IppValue) -> Self {
        self.0.insert(name(member), v);
        self
    }
    pub fn build(self) -> IppValue {
        IppValue::Collection(self.0)
    }
}

/// Response builder.
pub struct Resp {
    inner: IppRequestResponse,
}

impl Resp {
    /// New response echoing the request version (clamped to 1.1/2.0 family) and id; adds charset + language.
    pub fn new(req_version: u16, status: Status, request_id: i32) -> Self {
        let version = IppVersion(req_version);
        let inner = IppRequestResponse::new_response(version, status, request_id)
            .expect("static charset/lang are valid");
        Resp { inner }
    }
    pub fn status_message(mut self, msg: &str) -> Self {
        self.add(Group::OperationAttributes, "status-message", v_text(msg));
        self
    }
    /// Append to the (single) group of this tag, creating it if needed.
    pub fn add(&mut self, group: Group, attr_name: &str, value: IppValue) {
        self.inner
            .attributes_mut()
            .add(group, IppAttribute::new(name(attr_name), value));
    }
    /// Start a NEW group of this tag (Get-Jobs needs one job-attributes group per job).
    pub fn push_group(&mut self, group: Group) -> GroupWriter<'_> {
        self.inner
            .attributes_mut()
            .groups_mut()
            .push(IppAttributeGroup::new(group));
        GroupWriter {
            g: self.inner.attributes_mut().groups_mut().last_mut().unwrap(),
        }
    }
    pub fn into_bytes(self) -> Bytes {
        self.inner.to_bytes()
    }
    pub fn attributes(&self) -> &IppAttributes {
        self.inner.attributes()
    }
}

pub struct GroupWriter<'a> {
    g: &'a mut IppAttributeGroup,
}

impl GroupWriter<'_> {
    pub fn add(&mut self, attr_name: &str, value: IppValue) -> &mut Self {
        self.g
            .attributes_mut()
            .push(IppAttribute::new(name(attr_name), value));
        self
    }
}

/// Days-to-civil (Howard Hinnant's algorithm), UTC.
pub fn civil_from_unix(secs: i64) -> (i32, u8, u8, u8, u8, u8) {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u8;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u8;
    let y = if m <= 2 { y + 1 } else { y } as i32;
    (
        y,
        m,
        d,
        (rem / 3600) as u8,
        ((rem % 3600) / 60) as u8,
        (rem % 60) as u8,
    )
}

/// Build an IPP *request* (used by the tiny CUPS client in `cupsq`).
pub struct ReqBuilder {
    inner: IppRequestResponse,
}

impl ReqBuilder {
    /// Fails only when `printer_uri` cannot be represented (e.g. longer than the IPP string limit).
    pub fn new(op: ipp::model::Operation, printer_uri: &str) -> Result<Self, CodecError> {
        let uri: Option<http::Uri> = printer_uri.parse().ok();
        let inner = IppRequestResponse::new(IppVersion::v2_0(), op, uri)
            .map_err(|e| CodecError::Parse(format!("request for {printer_uri}: {e}")))?;
        Ok(ReqBuilder { inner })
    }
    pub fn add(mut self, group: Group, attr_name: &str, value: IppValue) -> Self {
        self.inner
            .attributes_mut()
            .add(group, IppAttribute::new(name(attr_name), value));
        self
    }
    pub fn into_bytes(self) -> Bytes {
        self.inner.to_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_dates() {
        assert_eq!(civil_from_unix(0), (1970, 1, 1, 0, 0, 0));
        assert_eq!(civil_from_unix(951_782_400), (2000, 2, 29, 0, 0, 0));
        assert_eq!(civil_from_unix(1_786_752_000), (2026, 8, 15, 0, 0, 0));
    }

    #[test]
    fn response_roundtrip_with_collections_and_groups() {
        let mut r = Resp::new(0x0200, Status::SuccessfulOk, 42);
        r.add(
            Group::PrinterAttributes,
            "printer-name",
            v_name("CatPrinter"),
        );
        r.add(Group::PrinterAttributes, "copies-supported", v_range(1, 99));
        r.add(
            Group::PrinterAttributes,
            "printer-geo-location",
            v_unknown(),
        );
        r.add(
            Group::PrinterAttributes,
            "printer-resolution-supported",
            v_res(203),
        );
        r.add(
            Group::PrinterAttributes,
            "media-col-database",
            v_array(vec![
                Coll::new()
                    .add(
                        "media-size",
                        Coll::new()
                            .add("x-dimension", v_int(4800))
                            .add("y-dimension", v_int(29700))
                            .build(),
                    )
                    .add("media-source", v_kw("main"))
                    .build(),
                Coll::new()
                    .add(
                        "media-size",
                        Coll::new()
                            .add("x-dimension", v_int(21000))
                            .add("y-dimension", v_int(29700))
                            .build(),
                    )
                    .build(),
            ]),
        );
        {
            let mut g = r.push_group(Group::JobAttributes);
            g.add("job-id", v_int(1))
                .add("job-uri", v_uri("ipp://127.0.0.1:8095/ipp/print/1"));
        }
        {
            let mut g = r.push_group(Group::JobAttributes);
            g.add("job-id", v_int(2));
        }
        let bytes = r.into_bytes();
        let back = parse(bytes.clone()).unwrap();
        assert_eq!(back.version, 0x0200);
        assert_eq!(back.op, 0); // status ok
        assert_eq!(back.request_id, 42);
        assert_eq!(
            back.get_str(Some(Group::PrinterAttributes), "printer-name")
                .as_deref(),
            Some("CatPrinter")
        );
        assert!(matches!(
            back.get(Some(Group::PrinterAttributes), "copies-supported"),
            Some(IppValue::RangeOfInteger { min: 1, max: 99 })
        ));
        assert!(matches!(
            back.get(Some(Group::PrinterAttributes), "printer-geo-location"),
            Some(IppValue::Other { tag: 0x12, .. })
        ));
        let db = back
            .get(Some(Group::PrinterAttributes), "media-col-database")
            .unwrap();
        match db {
            IppValue::Array(a) => assert_eq!(a.len(), 2),
            other => panic!("expected array, got {other:?}"),
        }
        assert!(
            matches!(back.get_member(Some(Group::PrinterAttributes), "media-col-database", "media-source"), Some(IppValue::Keyword(k)) if k.as_str() == "main")
        );
        let job_groups = back
            .attrs
            .groups()
            .iter()
            .filter(|g| g.tag() == Group::JobAttributes)
            .count();
        assert_eq!(job_groups, 2);
        assert!(back.payload.is_empty());
    }

    #[test]
    fn parse_keeps_payload_and_operation_order() {
        // Hand-built Print-Job: version 1.1, op 2, id 7, operation group with charset, language, printer-uri, then payload
        let mut b = vec![1u8, 1, 0, 2, 0, 0, 0, 7, 0x01];
        fn attr(b: &mut Vec<u8>, tag: u8, name: &str, val: &str) {
            b.push(tag);
            b.extend((name.len() as u16).to_be_bytes());
            b.extend(name.as_bytes());
            b.extend((val.len() as u16).to_be_bytes());
            b.extend(val.as_bytes());
        }
        attr(&mut b, 0x47, "attributes-charset", "utf-8");
        attr(&mut b, 0x48, "attributes-natural-language", "en");
        attr(
            &mut b,
            0x45,
            "printer-uri",
            "ipp://127.0.0.1:8095/ipp/print",
        );
        b.push(0x03);
        b.extend(b"RaS2payload");
        let req = parse(Bytes::from(b)).unwrap();
        assert_eq!(req.op, 2);
        assert_eq!(req.request_id, 7);
        assert_eq!(
            req.operation_attr_names(),
            vec![
                "attributes-charset",
                "attributes-natural-language",
                "printer-uri"
            ]
        );
        assert_eq!(&req.payload[..], b"RaS2payload");
        assert_eq!(req.first_group_tag(), Some(Group::OperationAttributes));
    }

    #[test]
    fn truncation_is_char_boundary_safe() {
        // 2-byte chars: byte 255 / 1023 fall inside a char — the crate's own truncate would panic.
        let e = "é".repeat(600); // 1200 bytes
        for v in [
            v_name(&e),
            v_kw(&e),
            v_mime(&e),
            v_charset(&e),
            v_lang(&e),
            v_uri(&e),
        ] {
            let s = value_to_string(&v).unwrap();
            assert!(!s.is_empty());
            assert!(s.len() <= 1023, "{}", s.len());
            assert!(s.chars().all(|c| c == 'é'));
        }
        assert!(value_to_string(&v_name(&e)).unwrap().len() <= 255);
        // 3-byte chars through v_text: never more than 1023 bytes, always whole chars
        let dash = "—".repeat(500); // 1500 bytes
        let t = value_to_string(&v_text(&dash)).unwrap();
        assert!(t.len() <= 1023 && t.len() >= 1020, "{}", t.len());
        assert!(t.chars().all(|c| c == '—'));
        // short strings pass through untouched; the attribute name is bounded too
        assert_eq!(value_to_string(&v_text("héllo")).unwrap(), "héllo");
        let _ = name(&e);
        assert_eq!(utf8_prefix("aé", 2), "a");
        assert_eq!(utf8_prefix("aé", 3), "aé");
        assert_eq!(utf8_prefix("", 0), "");
    }

    #[test]
    fn req_builder_rejects_huge_uri() {
        let uri = format!("ipp://localhost/printers/{}", "q".repeat(2000));
        assert!(ReqBuilder::new(ipp::model::Operation::GetPrinterAttributes, &uri).is_err());
        assert!(ReqBuilder::new(
            ipp::model::Operation::GetPrinterAttributes,
            "ipp://localhost/p"
        )
        .is_ok());
    }
}
