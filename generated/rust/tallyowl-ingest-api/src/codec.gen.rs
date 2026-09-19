//! Generated self-contained canonical-CBOR codec from CSIL specification.
//!
//! CSIL is the CBOR Service Interface Language; this codec owns the payload
//! wire (a CBOR map keyed by the verbatim CSIL field name in canonical RFC
//! 8949 order) so the generated types need no serde derive. One
//! `encode_`/`decode_` pair is emitted per record type.
#![allow(dead_code, clippy::vec_init_then_push)]

use super::types::*;

/// A decode failure: the CBOR was malformed or did not match the expected shape.
#[derive(Debug, Clone, PartialEq)]
pub struct CsilCborError(pub String);

impl std::fmt::Display for CsilCborError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for CsilCborError {}

/// A minimal canonical-CBOR value tree: a closed set of variants the generated codec
/// builds and walks. A map is an ordered list of pairs, so the encoder controls the
/// wire order of a record's keys explicitly (laid down in canonical order).
#[derive(Debug, Clone, PartialEq)]
pub enum CsilCborValue {
    Uint(u64),
    Int(i64),
    Bool(bool),
    Float(f64),
    Null,
    Text(String),
    Bytes(Vec<u8>),
    Array(Vec<CsilCborValue>),
    Map(Vec<(CsilCborValue, CsilCborValue)>),
    Tag(u64, Box<CsilCborValue>),
}

fn cbor_int(x: i64) -> CsilCborValue {
    CsilCborValue::Int(x)
}
fn cbor_uint(x: u64) -> CsilCborValue {
    CsilCborValue::Uint(x)
}
fn cbor_float(x: f64) -> CsilCborValue {
    CsilCborValue::Float(x)
}
fn cbor_bool(x: bool) -> CsilCborValue {
    CsilCborValue::Bool(x)
}
fn cbor_text(x: &str) -> CsilCborValue {
    CsilCborValue::Text(x.to_string())
}
fn cbor_bytes(x: &[u8]) -> CsilCborValue {
    CsilCborValue::Bytes(x.to_vec())
}

/// Serialize a value tree to canonical CBOR bytes.
fn cbor_encode(v: &CsilCborValue) -> Vec<u8> {
    let mut out = Vec::new();
    cbor_enc(v, &mut out);
    out
}

fn cbor_head(major: u8, n: u64, out: &mut Vec<u8>) {
    let mt = major << 5;
    if n < 24 {
        out.push(mt | n as u8);
    } else if n < 0x100 {
        out.push(mt | 24);
        out.push(n as u8);
    } else if n < 0x10000 {
        out.push(mt | 25);
        out.extend_from_slice(&(n as u16).to_be_bytes());
    } else if n < 0x1_0000_0000 {
        out.push(mt | 26);
        out.extend_from_slice(&(n as u32).to_be_bytes());
    } else {
        out.push(mt | 27);
        out.extend_from_slice(&n.to_be_bytes());
    }
}

fn cbor_enc(v: &CsilCborValue, out: &mut Vec<u8>) {
    match v {
        CsilCborValue::Uint(x) => cbor_head(0, *x, out),
        // A non-negative `Int` rides major type 0 so it is byte-identical to a `Uint`
        // of the same magnitude; only a genuinely negative value uses major type 1.
        CsilCborValue::Int(x) => {
            if *x >= 0 {
                cbor_head(0, *x as u64, out);
            } else {
                cbor_head(1, (-(*x + 1)) as u64, out);
            }
        }
        CsilCborValue::Bool(x) => out.push(if *x { 0xf5 } else { 0xf4 }),
        CsilCborValue::Null => out.push(0xf6),
        CsilCborValue::Float(x) => {
            out.push(0xfb);
            out.extend_from_slice(&x.to_bits().to_be_bytes());
        }
        CsilCborValue::Text(s) => {
            let bytes = s.as_bytes();
            cbor_head(3, bytes.len() as u64, out);
            out.extend_from_slice(bytes);
        }
        CsilCborValue::Bytes(b) => {
            cbor_head(2, b.len() as u64, out);
            out.extend_from_slice(b);
        }
        CsilCborValue::Array(items) => {
            cbor_head(4, items.len() as u64, out);
            for item in items {
                cbor_enc(item, out);
            }
        }
        CsilCborValue::Map(entries) => {
            cbor_head(5, entries.len() as u64, out);
            for (k, val) in entries {
                cbor_enc(k, out);
                cbor_enc(val, out);
            }
        }
        CsilCborValue::Tag(num, inner) => {
            cbor_head(6, *num, out);
            cbor_enc(inner, out);
        }
    }
}

/// Parse a full CBOR item and reject trailing bytes, so a payload that is not
/// exactly one value is an error rather than a silently-truncated read.
fn cbor_decode(b: &[u8]) -> Result<CsilCborValue, CsilCborError> {
    let mut pos = 0usize;
    let v = cbor_dec(b, &mut pos, 0)?;
    if pos != b.len() {
        return Err(CsilCborError(format!(
            "csil cbor: {} trailing bytes",
            b.len() - pos
        )));
    }
    Ok(v)
}

fn cbor_read_arg(b: &[u8], pos: &mut usize, low: u8) -> Result<u64, CsilCborError> {
    if low < 24 {
        *pos += 1;
        return Ok(low as u64);
    }
    let width = match low {
        24 => 1usize,
        25 => 2,
        26 => 4,
        27 => 8,
        _ => {
            return Err(CsilCborError(format!(
                "csil cbor: reserved additional info {low}"
            )))
        }
    };
    if *pos >= b.len() || width > b.len() - *pos - 1 {
        return Err(CsilCborError("csil cbor: truncated argument".to_string()));
    }
    let mut v = 0u64;
    for &byte in &b[*pos + 1..*pos + 1 + width] {
        v = (v << 8) | byte as u64;
    }
    *pos += 1 + width;
    Ok(v)
}

fn cbor_dec(b: &[u8], pos: &mut usize, depth: usize) -> Result<CsilCborValue, CsilCborError> {
    if depth > 64 {
        return Err(CsilCborError(
            "csil cbor: nesting limit exceeded".to_string(),
        ));
    }
    if *pos >= b.len() {
        return Err(CsilCborError(
            "csil cbor: unexpected end of input".to_string(),
        ));
    }
    let ib = b[*pos];
    let major = ib >> 5;
    let low = ib & 0x1f;
    if major == 7 {
        return match low {
            20 => {
                *pos += 1;
                Ok(CsilCborValue::Bool(false))
            }
            21 => {
                *pos += 1;
                Ok(CsilCborValue::Bool(true))
            }
            22 | 23 => {
                *pos += 1;
                Ok(CsilCborValue::Null)
            }
            26 => {
                let bits = cbor_read_arg(b, pos, low)?;
                Ok(CsilCborValue::Float(f32::from_bits(bits as u32) as f64))
            }
            27 => {
                let bits = cbor_read_arg(b, pos, low)?;
                Ok(CsilCborValue::Float(f64::from_bits(bits)))
            }
            _ => Err(CsilCborError(format!(
                "csil cbor: unsupported simple value {low}"
            ))),
        };
    }
    let arg = cbor_read_arg(b, pos, low)?;
    match major {
        0 => Ok(CsilCborValue::Uint(arg)),
        1 => {
            if arg > i64::MAX as u64 {
                return Err(CsilCborError(
                    "csil cbor: negative integer out of range".to_string(),
                ));
            }
            Ok(CsilCborValue::Int(-1 - arg as i64))
        }
        2 => {
            if arg > (b.len() - *pos) as u64 {
                return Err(CsilCborError(
                    "csil cbor: truncated byte string".to_string(),
                ));
            }
            let n = arg as usize;
            let slice = b[*pos..*pos + n].to_vec();
            *pos += n;
            Ok(CsilCborValue::Bytes(slice))
        }
        3 => {
            if arg > (b.len() - *pos) as u64 {
                return Err(CsilCborError(
                    "csil cbor: truncated text string".to_string(),
                ));
            }
            let n = arg as usize;
            let s = std::str::from_utf8(&b[*pos..*pos + n])
                .map_err(|e| CsilCborError(format!("csil cbor: invalid utf-8: {e}")))?
                .to_string();
            *pos += n;
            Ok(CsilCborValue::Text(s))
        }
        4 => {
            if arg > (b.len() - *pos) as u64 {
                return Err(CsilCborError(
                    "csil cbor: array length exceeds remaining input".to_string(),
                ));
            }
            let n = arg as usize;
            let mut items = Vec::with_capacity(n);
            for _ in 0..n {
                items.push(cbor_dec(b, pos, depth + 1)?);
            }
            Ok(CsilCborValue::Array(items))
        }
        5 => {
            if arg > (b.len() - *pos) as u64 {
                return Err(CsilCborError(
                    "csil cbor: map length exceeds remaining input".to_string(),
                ));
            }
            let n = arg as usize;
            let mut entries = Vec::with_capacity(n);
            for _ in 0..n {
                let k = cbor_dec(b, pos, depth + 1)?;
                let val = cbor_dec(b, pos, depth + 1)?;
                entries.push((k, val));
            }
            Ok(CsilCborValue::Map(entries))
        }
        6 => {
            let inner = cbor_dec(b, pos, depth + 1)?;
            Ok(CsilCborValue::Tag(arg, Box::new(inner)))
        }
        _ => Err(CsilCborError(format!(
            "csil cbor: unexpected major type {major}"
        ))),
    }
}

/// Map a typed slice to a CBOR array via the per-element encoder.
fn cbor_enc_array<E>(xs: &[E], f: impl Fn(&E) -> CsilCborValue) -> CsilCborValue {
    CsilCborValue::Array(xs.iter().map(f).collect())
}

/// Map a typed map to a CBOR map. Rust `HashMap` iteration is unordered, so the inner
/// map's entry order is not canonicalized; the record's own keys (laid down at
/// generation time) are what the cross-language wire contract pins.
fn cbor_enc_map<K, V>(
    m: &std::collections::HashMap<K, V>,
    kf: impl Fn(&K) -> CsilCborValue,
    vf: impl Fn(&V) -> CsilCborValue,
) -> CsilCborValue {
    CsilCborValue::Map(m.iter().map(|(k, v)| (kf(k), vf(v))).collect())
}

fn cbor_dec_array<E>(
    v: &CsilCborValue,
    f: impl Fn(&CsilCborValue) -> Result<E, CsilCborError>,
) -> Result<Vec<E>, CsilCborError> {
    cbor_as_array(v)?.iter().map(f).collect()
}

fn cbor_dec_map<K: std::cmp::Eq + std::hash::Hash, V>(
    v: &CsilCborValue,
    kf: impl Fn(&CsilCborValue) -> Result<K, CsilCborError>,
    vf: impl Fn(&CsilCborValue) -> Result<V, CsilCborError>,
) -> Result<std::collections::HashMap<K, V>, CsilCborError> {
    let entries = cbor_as_map(v)?;
    let mut out = std::collections::HashMap::with_capacity(entries.len());
    for (k, val) in entries {
        out.insert(kf(k)?, vf(val)?);
    }
    Ok(out)
}

fn cbor_map_get<'a>(v: &'a CsilCborValue, key: &str) -> Option<&'a CsilCborValue> {
    if let CsilCborValue::Map(entries) = v {
        for (k, val) in entries {
            if matches!(k, CsilCborValue::Text(name) if name == key) {
                return Some(val);
            }
        }
    }
    None
}

fn cbor_expect_value(v: &CsilCborValue, expected: &CsilCborValue) -> Result<(), CsilCborError> {
    if v == expected {
        Ok(())
    } else {
        Err(CsilCborError(format!(
            "csil cbor: expected literal {expected:?}, got {v:?}"
        )))
    }
}

fn cbor_require<'a>(v: &'a CsilCborValue, key: &str) -> Result<&'a CsilCborValue, CsilCborError> {
    cbor_map_get(v, key).ok_or_else(|| CsilCborError(format!("csil cbor: missing field {key:?}")))
}

fn cbor_as_i64(v: &CsilCborValue) -> Result<i64, CsilCborError> {
    match v {
        CsilCborValue::Uint(x) => i64::try_from(*x)
            .map_err(|_| CsilCborError("csil cbor: integer overflows i64".to_string())),
        CsilCborValue::Int(x) => Ok(*x),
        _ => Err(CsilCborError("csil cbor: expected integer".to_string())),
    }
}

fn cbor_as_u64(v: &CsilCborValue) -> Result<u64, CsilCborError> {
    match v {
        CsilCborValue::Uint(x) => Ok(*x),
        CsilCborValue::Int(x) if *x >= 0 => Ok(*x as u64),
        CsilCborValue::Int(_) => Err(CsilCborError(
            "csil cbor: negative integer where unsigned expected".to_string(),
        )),
        _ => Err(CsilCborError(
            "csil cbor: expected unsigned integer".to_string(),
        )),
    }
}

fn cbor_as_f64(v: &CsilCborValue) -> Result<f64, CsilCborError> {
    match v {
        CsilCborValue::Float(x) => Ok(*x),
        CsilCborValue::Uint(x) => Ok(*x as f64),
        CsilCborValue::Int(x) => Ok(*x as f64),
        _ => Err(CsilCborError("csil cbor: expected float".to_string())),
    }
}

fn cbor_as_bool(v: &CsilCborValue) -> Result<bool, CsilCborError> {
    match v {
        CsilCborValue::Bool(b) => Ok(*b),
        _ => Err(CsilCborError("csil cbor: expected bool".to_string())),
    }
}

fn cbor_as_text(v: &CsilCborValue) -> Result<String, CsilCborError> {
    match v {
        CsilCborValue::Text(s) => Ok(s.clone()),
        _ => Err(CsilCborError("csil cbor: expected text".to_string())),
    }
}

fn cbor_as_bytes(v: &CsilCborValue) -> Result<Vec<u8>, CsilCborError> {
    match v {
        CsilCborValue::Bytes(b) => Ok(b.clone()),
        _ => Err(CsilCborError("csil cbor: expected byte string".to_string())),
    }
}

fn cbor_as_array(v: &CsilCborValue) -> Result<&[CsilCborValue], CsilCborError> {
    match v {
        CsilCborValue::Array(a) => Ok(a),
        _ => Err(CsilCborError("csil cbor: expected array".to_string())),
    }
}

fn cbor_as_map(v: &CsilCborValue) -> Result<&[(CsilCborValue, CsilCborValue)], CsilCborError> {
    match v {
        CsilCborValue::Map(m) => Ok(m),
        _ => Err(CsilCborError("csil cbor: expected map".to_string())),
    }
}

fn csil_bigint_be_bytes(mut n: u128) -> Vec<u8> {
    if n == 0 {
        return vec![0];
    }
    let mut out = Vec::new();
    while n > 0 {
        out.push((n & 0xff) as u8);
        n >>= 8;
    }
    out.reverse();
    out
}

fn csil_be_bytes_to_u128(bytes: &[u8]) -> Result<u128, CsilCborError> {
    if bytes.len() > 16 {
        return Err(CsilCborError(
            "csil cbor: bignum exceeds 128 bits".to_string(),
        ));
    }
    let mut n: u128 = 0;
    for &b in bytes {
        n = (n << 8) | b as u128;
    }
    Ok(n)
}

/// Encode an exact integer mantissa: a CBOR integer when it fits in 64 bits,
/// otherwise a bignum so the value stays exact across the wire.
fn csil_enc_bigint(m: i128) -> CsilCborValue {
    if let Ok(v) = i64::try_from(m) {
        CsilCborValue::Int(v)
    } else if let Ok(v) = u64::try_from(m) {
        CsilCborValue::Uint(v)
    } else if m >= 0 {
        CsilCborValue::Tag(
            2,
            Box::new(CsilCborValue::Bytes(csil_bigint_be_bytes(m as u128))),
        )
    } else {
        // A negative bignum encodes the magnitude of -1 - value.
        let mag = (-(m + 1)) as u128;
        CsilCborValue::Tag(3, Box::new(CsilCborValue::Bytes(csil_bigint_be_bytes(mag))))
    }
}

fn csil_dec_bigint(v: &CsilCborValue) -> Result<i128, CsilCborError> {
    match v {
        CsilCborValue::Uint(x) => Ok(*x as i128),
        CsilCborValue::Int(x) => Ok(*x as i128),
        CsilCborValue::Tag(num, inner) => {
            let CsilCborValue::Bytes(bytes) = inner.as_ref() else {
                return Err(CsilCborError(
                    "csil cbor: bignum content must be a byte string".to_string(),
                ));
            };
            let mag = csil_be_bytes_to_u128(bytes)?;
            match num {
                2 => i128::try_from(mag).map_err(|_| {
                    CsilCborError("csil cbor: decimal mantissa overflows i128".to_string())
                }),
                3 => {
                    let val = i128::try_from(mag).map_err(|_| {
                        CsilCborError("csil cbor: decimal mantissa overflows i128".to_string())
                    })?;
                    Ok(-1 - val)
                }
                _ => Err(CsilCborError(format!(
                    "csil cbor: unexpected bignum tag {num}"
                ))),
            }
        }
        _ => Err(CsilCborError(
            "csil cbor: expected integer mantissa".to_string(),
        )),
    }
}

/// Encode a `CsilDecimal` as CBOR tag 4: `[exponent, mantissa]`.
fn csil_enc_decimal(d: &CsilDecimal) -> CsilCborValue {
    CsilCborValue::Tag(
        4,
        Box::new(CsilCborValue::Array(vec![
            CsilCborValue::Int(d.exponent),
            csil_enc_bigint(d.mantissa),
        ])),
    )
}

/// Decode a CBOR tag 4 decimal fraction into an exact `CsilDecimal`.
fn csil_as_decimal(v: &CsilCborValue) -> Result<CsilDecimal, CsilCborError> {
    let CsilCborValue::Tag(4, inner) = v else {
        return Err(CsilCborError(
            "csil cbor: expected CBOR tag 4 decimal".to_string(),
        ));
    };
    let CsilCborValue::Array(arr) = inner.as_ref() else {
        return Err(CsilCborError(
            "csil cbor: tag 4 content must be [exponent, mantissa]".to_string(),
        ));
    };
    if arr.len() != 2 {
        return Err(CsilCborError(
            "csil cbor: tag 4 content must be [exponent, mantissa]".to_string(),
        ));
    }
    let exponent = cbor_as_i64(&arr[0])?;
    let mantissa = csil_dec_bigint(&arr[1])?;
    Ok(CsilDecimal { exponent, mantissa })
}

/// Build the canonical CBOR value tree for a EventPayload.
fn csil_enc_event_payload(csil_v: &EventPayload) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(3);
    csil_entries.push((cbor_text("name"), cbor_text(&csil_v.name)));
    if let Some(csil_inner) = &csil_v.route {
        csil_entries.push((cbor_text("route"), cbor_text(csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.page_title {
        csil_entries.push((cbor_text("page_title"), cbor_text(csil_inner)));
    }
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a EventPayload from a decoded CBOR value tree.
fn csil_dec_event_payload(csil_root: &CsilCborValue) -> Result<EventPayload, CsilCborError> {
    let name = {
        let csil_field = cbor_require(csil_root, "name")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let route = match cbor_map_get(csil_root, "route") {
        Some(csil_field) => {
            let csil_decode = cbor_as_text;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let page_title = match cbor_map_get(csil_root, "page_title") {
        Some(csil_field) => {
            let csil_decode = cbor_as_text;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    Ok(EventPayload {
        name,
        route,
        page_title,
    })
}

/// Encode a EventPayload to canonical CSIL CBOR bytes.
pub fn encode_event_payload(csil_v: &EventPayload) -> Vec<u8> {
    cbor_encode(&csil_enc_event_payload(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a EventPayload.
pub fn decode_event_payload(csil_data: &[u8]) -> Result<EventPayload, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_event_payload(&csil_root)
}

/// Build the canonical CBOR value tree for a PageViewPayload.
fn csil_enc_page_view_payload(csil_v: &PageViewPayload) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(4);
    csil_entries.push((cbor_text("route"), cbor_text(&csil_v.route)));
    if let Some(csil_inner) = &csil_v.campaign {
        csil_entries.push((
            cbor_text("campaign"),
            csil_enc_campaign_parameters(csil_inner),
        ));
    }
    if let Some(csil_inner) = &csil_v.referrer {
        csil_entries.push((cbor_text("referrer"), cbor_text(csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.page_title {
        csil_entries.push((cbor_text("page_title"), cbor_text(csil_inner)));
    }
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a PageViewPayload from a decoded CBOR value tree.
fn csil_dec_page_view_payload(csil_root: &CsilCborValue) -> Result<PageViewPayload, CsilCborError> {
    let route = {
        let csil_field = cbor_require(csil_root, "route")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let page_title = match cbor_map_get(csil_root, "page_title") {
        Some(csil_field) => {
            let csil_decode = cbor_as_text;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let referrer = match cbor_map_get(csil_root, "referrer") {
        Some(csil_field) => {
            let csil_decode = cbor_as_text;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let campaign = match cbor_map_get(csil_root, "campaign") {
        Some(csil_field) => {
            let csil_decode = csil_dec_campaign_parameters;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    Ok(PageViewPayload {
        route,
        page_title,
        referrer,
        campaign,
    })
}

/// Encode a PageViewPayload to canonical CSIL CBOR bytes.
pub fn encode_page_view_payload(csil_v: &PageViewPayload) -> Vec<u8> {
    cbor_encode(&csil_enc_page_view_payload(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a PageViewPayload.
pub fn decode_page_view_payload(csil_data: &[u8]) -> Result<PageViewPayload, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_page_view_payload(&csil_root)
}

/// Build the canonical CBOR value tree for a CampaignParameters.
fn csil_enc_campaign_parameters(csil_v: &CampaignParameters) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(6);
    if let Some(csil_inner) = &csil_v.term {
        csil_entries.push((cbor_text("term"), cbor_text(csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.medium {
        csil_entries.push((cbor_text("medium"), cbor_text(csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.source {
        csil_entries.push((cbor_text("source"), cbor_text(csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.content {
        csil_entries.push((cbor_text("content"), cbor_text(csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.campaign {
        csil_entries.push((cbor_text("campaign"), cbor_text(csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.click_id {
        csil_entries.push((cbor_text("click_id"), cbor_text(csil_inner)));
    }
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a CampaignParameters from a decoded CBOR value tree.
fn csil_dec_campaign_parameters(
    csil_root: &CsilCborValue,
) -> Result<CampaignParameters, CsilCborError> {
    let source = match cbor_map_get(csil_root, "source") {
        Some(csil_field) => {
            let csil_decode = cbor_as_text;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let medium = match cbor_map_get(csil_root, "medium") {
        Some(csil_field) => {
            let csil_decode = cbor_as_text;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let campaign = match cbor_map_get(csil_root, "campaign") {
        Some(csil_field) => {
            let csil_decode = cbor_as_text;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let term = match cbor_map_get(csil_root, "term") {
        Some(csil_field) => {
            let csil_decode = cbor_as_text;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let content = match cbor_map_get(csil_root, "content") {
        Some(csil_field) => {
            let csil_decode = cbor_as_text;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let click_id = match cbor_map_get(csil_root, "click_id") {
        Some(csil_field) => {
            let csil_decode = cbor_as_text;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    Ok(CampaignParameters {
        source,
        medium,
        campaign,
        term,
        content,
        click_id,
    })
}

/// Encode a CampaignParameters to canonical CSIL CBOR bytes.
pub fn encode_campaign_parameters(csil_v: &CampaignParameters) -> Vec<u8> {
    cbor_encode(&csil_enc_campaign_parameters(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a CampaignParameters.
pub fn decode_campaign_parameters(csil_data: &[u8]) -> Result<CampaignParameters, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_campaign_parameters(&csil_root)
}

/// Build the canonical CBOR value tree for a SessionStartPayload.
fn csil_enc_session_start_payload(csil_v: &SessionStartPayload) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(1);
    if let Some(csil_inner) = &csil_v.entry_route {
        csil_entries.push((cbor_text("entry_route"), cbor_text(csil_inner)));
    }
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a SessionStartPayload from a decoded CBOR value tree.
fn csil_dec_session_start_payload(
    csil_root: &CsilCborValue,
) -> Result<SessionStartPayload, CsilCborError> {
    let entry_route = match cbor_map_get(csil_root, "entry_route") {
        Some(csil_field) => {
            let csil_decode = cbor_as_text;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    Ok(SessionStartPayload { entry_route })
}

/// Encode a SessionStartPayload to canonical CSIL CBOR bytes.
pub fn encode_session_start_payload(csil_v: &SessionStartPayload) -> Vec<u8> {
    cbor_encode(&csil_enc_session_start_payload(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a SessionStartPayload.
pub fn decode_session_start_payload(
    csil_data: &[u8],
) -> Result<SessionStartPayload, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_session_start_payload(&csil_root)
}

/// Build the canonical CBOR value tree for a SessionEndPayload.
fn csil_enc_session_end_payload(csil_v: &SessionEndPayload) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(1);
    csil_entries.push((
        cbor_text("reason"),
        csil_enc_session_end_payload_reason(&csil_v.reason),
    ));
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a SessionEndPayload from a decoded CBOR value tree.
fn csil_dec_session_end_payload(
    csil_root: &CsilCborValue,
) -> Result<SessionEndPayload, CsilCborError> {
    let reason = {
        let csil_field = cbor_require(csil_root, "reason")?;
        let csil_decode = csil_dec_session_end_payload_reason;
        csil_decode(csil_field)?
    };
    Ok(SessionEndPayload { reason })
}

/// Encode a SessionEndPayload to canonical CSIL CBOR bytes.
pub fn encode_session_end_payload(csil_v: &SessionEndPayload) -> Vec<u8> {
    cbor_encode(&csil_enc_session_end_payload(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a SessionEndPayload.
pub fn decode_session_end_payload(csil_data: &[u8]) -> Result<SessionEndPayload, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_session_end_payload(&csil_root)
}

/// Build the canonical CBOR value tree for a InteractionPayload.
fn csil_enc_interaction_payload(csil_v: &InteractionPayload) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(2);
    csil_entries.push((cbor_text("action"), cbor_text(&csil_v.action)));
    csil_entries.push((cbor_text("target"), cbor_text(&csil_v.target)));
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a InteractionPayload from a decoded CBOR value tree.
fn csil_dec_interaction_payload(
    csil_root: &CsilCborValue,
) -> Result<InteractionPayload, CsilCborError> {
    let target = {
        let csil_field = cbor_require(csil_root, "target")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let action = {
        let csil_field = cbor_require(csil_root, "action")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    Ok(InteractionPayload { target, action })
}

/// Encode a InteractionPayload to canonical CSIL CBOR bytes.
pub fn encode_interaction_payload(csil_v: &InteractionPayload) -> Vec<u8> {
    cbor_encode(&csil_enc_interaction_payload(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a InteractionPayload.
pub fn decode_interaction_payload(csil_data: &[u8]) -> Result<InteractionPayload, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_interaction_payload(&csil_root)
}

/// Build the canonical CBOR value tree for a FeatureExposurePayload.
fn csil_enc_feature_exposure_payload(csil_v: &FeatureExposurePayload) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(2);
    csil_entries.push((cbor_text("feature"), cbor_text(&csil_v.feature)));
    csil_entries.push((cbor_text("variant"), cbor_text(&csil_v.variant)));
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a FeatureExposurePayload from a decoded CBOR value tree.
fn csil_dec_feature_exposure_payload(
    csil_root: &CsilCborValue,
) -> Result<FeatureExposurePayload, CsilCborError> {
    let feature = {
        let csil_field = cbor_require(csil_root, "feature")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let variant = {
        let csil_field = cbor_require(csil_root, "variant")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    Ok(FeatureExposurePayload { feature, variant })
}

/// Encode a FeatureExposurePayload to canonical CSIL CBOR bytes.
pub fn encode_feature_exposure_payload(csil_v: &FeatureExposurePayload) -> Vec<u8> {
    cbor_encode(&csil_enc_feature_exposure_payload(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a FeatureExposurePayload.
pub fn decode_feature_exposure_payload(
    csil_data: &[u8],
) -> Result<FeatureExposurePayload, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_feature_exposure_payload(&csil_root)
}

/// Build the canonical CBOR value tree for a IdentifyPayload.
fn csil_enc_identify_payload(csil_v: &IdentifyPayload) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(1);
    csil_entries.push((cbor_text("end_user_id"), cbor_text(&csil_v.end_user_id)));
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a IdentifyPayload from a decoded CBOR value tree.
fn csil_dec_identify_payload(csil_root: &CsilCborValue) -> Result<IdentifyPayload, CsilCborError> {
    let end_user_id = {
        let csil_field = cbor_require(csil_root, "end_user_id")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    Ok(IdentifyPayload { end_user_id })
}

/// Encode a IdentifyPayload to canonical CSIL CBOR bytes.
pub fn encode_identify_payload(csil_v: &IdentifyPayload) -> Vec<u8> {
    cbor_encode(&csil_enc_identify_payload(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a IdentifyPayload.
pub fn decode_identify_payload(csil_data: &[u8]) -> Result<IdentifyPayload, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_identify_payload(&csil_root)
}

/// Build the canonical CBOR value tree for a AliasPayload.
fn csil_enc_alias_payload(csil_v: &AliasPayload) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(2);
    csil_entries.push((cbor_text("to_id"), cbor_text(&csil_v.to_id)));
    csil_entries.push((cbor_text("from_id"), cbor_text(&csil_v.from_id)));
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a AliasPayload from a decoded CBOR value tree.
fn csil_dec_alias_payload(csil_root: &CsilCborValue) -> Result<AliasPayload, CsilCborError> {
    let from_id = {
        let csil_field = cbor_require(csil_root, "from_id")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let to_id = {
        let csil_field = cbor_require(csil_root, "to_id")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    Ok(AliasPayload { from_id, to_id })
}

/// Encode a AliasPayload to canonical CSIL CBOR bytes.
pub fn encode_alias_payload(csil_v: &AliasPayload) -> Vec<u8> {
    cbor_encode(&csil_enc_alias_payload(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a AliasPayload.
pub fn decode_alias_payload(csil_data: &[u8]) -> Result<AliasPayload, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_alias_payload(&csil_root)
}

/// Build the canonical CBOR value tree for a GroupPayload.
fn csil_enc_group_payload(csil_v: &GroupPayload) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(2);
    csil_entries.push((cbor_text("group_id"), cbor_text(&csil_v.group_id)));
    if let Some(csil_inner) = &csil_v.group_kind {
        csil_entries.push((cbor_text("group_kind"), cbor_text(csil_inner)));
    }
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a GroupPayload from a decoded CBOR value tree.
fn csil_dec_group_payload(csil_root: &CsilCborValue) -> Result<GroupPayload, CsilCborError> {
    let group_id = {
        let csil_field = cbor_require(csil_root, "group_id")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let group_kind = match cbor_map_get(csil_root, "group_kind") {
        Some(csil_field) => {
            let csil_decode = cbor_as_text;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    Ok(GroupPayload {
        group_id,
        group_kind,
    })
}

/// Encode a GroupPayload to canonical CSIL CBOR bytes.
pub fn encode_group_payload(csil_v: &GroupPayload) -> Vec<u8> {
    cbor_encode(&csil_enc_group_payload(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a GroupPayload.
pub fn decode_group_payload(csil_data: &[u8]) -> Result<GroupPayload, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_group_payload(&csil_root)
}

/// Build the canonical CBOR value tree for a ConversionPayload.
fn csil_enc_conversion_payload(csil_v: &ConversionPayload) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(6);
    csil_entries.push((cbor_text("goal"), cbor_text(&csil_v.goal)));
    if let Some(csil_inner) = &csil_v.value {
        csil_entries.push((cbor_text("value"), csil_enc_decimal(csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.campaign {
        csil_entries.push((
            cbor_text("campaign"),
            csil_enc_campaign_parameters(csil_inner),
        ));
    }
    if let Some(csil_inner) = &csil_v.currency {
        csil_entries.push((cbor_text("currency"), cbor_text(csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.order_id {
        csil_entries.push((cbor_text("order_id"), cbor_text(csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.touch_event_id {
        csil_entries.push((cbor_text("touch_event_id"), cbor_bytes(csil_inner)));
    }
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a ConversionPayload from a decoded CBOR value tree.
fn csil_dec_conversion_payload(
    csil_root: &CsilCborValue,
) -> Result<ConversionPayload, CsilCborError> {
    let goal = {
        let csil_field = cbor_require(csil_root, "goal")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let value = match cbor_map_get(csil_root, "value") {
        Some(csil_field) => {
            let csil_decode = csil_as_decimal;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let currency = match cbor_map_get(csil_root, "currency") {
        Some(csil_field) => {
            let csil_decode = cbor_as_text;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let order_id = match cbor_map_get(csil_root, "order_id") {
        Some(csil_field) => {
            let csil_decode = cbor_as_text;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let campaign = match cbor_map_get(csil_root, "campaign") {
        Some(csil_field) => {
            let csil_decode = csil_dec_campaign_parameters;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let touch_event_id = match cbor_map_get(csil_root, "touch_event_id") {
        Some(csil_field) => {
            let csil_decode = cbor_as_bytes;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    Ok(ConversionPayload {
        goal,
        value,
        currency,
        order_id,
        campaign,
        touch_event_id,
    })
}

/// Encode a ConversionPayload to canonical CSIL CBOR bytes.
pub fn encode_conversion_payload(csil_v: &ConversionPayload) -> Vec<u8> {
    cbor_encode(&csil_enc_conversion_payload(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a ConversionPayload.
pub fn decode_conversion_payload(csil_data: &[u8]) -> Result<ConversionPayload, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_conversion_payload(&csil_root)
}

/// Build the canonical CBOR value tree for a StackFrame.
fn csil_enc_stack_frame(csil_v: &StackFrame) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(5);
    if let Some(csil_inner) = &csil_v.file {
        csil_entries.push((cbor_text("file"), cbor_text(csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.line {
        csil_entries.push((cbor_text("line"), cbor_uint(*csil_inner)));
    }
    csil_entries.push((cbor_text("in_app"), cbor_bool(csil_v.in_app)));
    if let Some(csil_inner) = &csil_v.module {
        csil_entries.push((cbor_text("module"), cbor_text(csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.function {
        csil_entries.push((cbor_text("function"), cbor_text(csil_inner)));
    }
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a StackFrame from a decoded CBOR value tree.
fn csil_dec_stack_frame(csil_root: &CsilCborValue) -> Result<StackFrame, CsilCborError> {
    let module = match cbor_map_get(csil_root, "module") {
        Some(csil_field) => {
            let csil_decode = cbor_as_text;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let function = match cbor_map_get(csil_root, "function") {
        Some(csil_field) => {
            let csil_decode = cbor_as_text;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let file = match cbor_map_get(csil_root, "file") {
        Some(csil_field) => {
            let csil_decode = cbor_as_text;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let line = match cbor_map_get(csil_root, "line") {
        Some(csil_field) => {
            let csil_decode = cbor_as_u64;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let in_app = {
        let csil_field = cbor_require(csil_root, "in_app")?;
        let csil_decode = cbor_as_bool;
        csil_decode(csil_field)?
    };
    Ok(StackFrame {
        module,
        function,
        file,
        line,
        in_app,
    })
}

/// Encode a StackFrame to canonical CSIL CBOR bytes.
pub fn encode_stack_frame(csil_v: &StackFrame) -> Vec<u8> {
    cbor_encode(&csil_enc_stack_frame(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a StackFrame.
pub fn decode_stack_frame(csil_data: &[u8]) -> Result<StackFrame, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_stack_frame(&csil_root)
}

/// Build the canonical CBOR value tree for a ErrorPayload.
fn csil_enc_error_payload(csil_v: &ErrorPayload) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(8);
    if let Some(csil_inner) = &csil_v.frames {
        csil_entries.push((
            cbor_text("frames"),
            cbor_enc_array(csil_inner, csil_enc_stack_frame),
        ));
    }
    csil_entries.push((cbor_text("handled"), cbor_bool(csil_v.handled)));
    csil_entries.push((cbor_text("message"), cbor_text(&csil_v.message)));
    if let Some(csil_inner) = &csil_v.runtime {
        csil_entries.push((cbor_text("runtime"), cbor_text(csil_inner)));
    }
    csil_entries.push((
        cbor_text("severity"),
        csil_enc_error_payload_severity(&csil_v.severity),
    ));
    if let Some(csil_inner) = &csil_v.mechanism {
        csil_entries.push((cbor_text("mechanism"), cbor_text(csil_inner)));
    }
    csil_entries.push((cbor_text("error_type"), cbor_text(&csil_v.error_type)));
    if let Some(csil_inner) = &csil_v.breadcrumbs {
        csil_entries.push((
            cbor_text("breadcrumbs"),
            cbor_enc_array(csil_inner, |csil_elem| cbor_bytes(csil_elem)),
        ));
    }
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a ErrorPayload from a decoded CBOR value tree.
fn csil_dec_error_payload(csil_root: &CsilCborValue) -> Result<ErrorPayload, CsilCborError> {
    let error_type = {
        let csil_field = cbor_require(csil_root, "error_type")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let message = {
        let csil_field = cbor_require(csil_root, "message")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let handled = {
        let csil_field = cbor_require(csil_root, "handled")?;
        let csil_decode = cbor_as_bool;
        csil_decode(csil_field)?
    };
    let severity = {
        let csil_field = cbor_require(csil_root, "severity")?;
        let csil_decode = csil_dec_error_payload_severity;
        csil_decode(csil_field)?
    };
    let mechanism = match cbor_map_get(csil_root, "mechanism") {
        Some(csil_field) => {
            let csil_decode = cbor_as_text;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let runtime = match cbor_map_get(csil_root, "runtime") {
        Some(csil_field) => {
            let csil_decode = cbor_as_text;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let frames = match cbor_map_get(csil_root, "frames") {
        Some(csil_field) => {
            let csil_decode = |csil_v| cbor_dec_array(csil_v, csil_dec_stack_frame);
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let breadcrumbs = match cbor_map_get(csil_root, "breadcrumbs") {
        Some(csil_field) => {
            let csil_decode = |csil_v| cbor_dec_array(csil_v, cbor_as_bytes);
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    Ok(ErrorPayload {
        error_type,
        message,
        handled,
        severity,
        mechanism,
        runtime,
        frames,
        breadcrumbs,
    })
}

/// Encode a ErrorPayload to canonical CSIL CBOR bytes.
pub fn encode_error_payload(csil_v: &ErrorPayload) -> Vec<u8> {
    cbor_encode(&csil_enc_error_payload(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a ErrorPayload.
pub fn decode_error_payload(csil_data: &[u8]) -> Result<ErrorPayload, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_error_payload(&csil_root)
}

/// Build the canonical CBOR value tree for a SpanLink.
fn csil_enc_span_link(csil_v: &SpanLink) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(2);
    csil_entries.push((cbor_text("span_id"), cbor_bytes(&csil_v.span_id)));
    csil_entries.push((cbor_text("trace_id"), cbor_bytes(&csil_v.trace_id)));
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a SpanLink from a decoded CBOR value tree.
fn csil_dec_span_link(csil_root: &CsilCborValue) -> Result<SpanLink, CsilCborError> {
    let trace_id = {
        let csil_field = cbor_require(csil_root, "trace_id")?;
        let csil_decode = cbor_as_bytes;
        csil_decode(csil_field)?
    };
    let span_id = {
        let csil_field = cbor_require(csil_root, "span_id")?;
        let csil_decode = cbor_as_bytes;
        csil_decode(csil_field)?
    };
    Ok(SpanLink { trace_id, span_id })
}

/// Encode a SpanLink to canonical CSIL CBOR bytes.
pub fn encode_span_link(csil_v: &SpanLink) -> Vec<u8> {
    cbor_encode(&csil_enc_span_link(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a SpanLink.
pub fn decode_span_link(csil_data: &[u8]) -> Result<SpanLink, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_span_link(&csil_root)
}

/// Build the canonical CBOR value tree for a SpanPayload.
fn csil_enc_span_payload(csil_v: &SpanPayload) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(10);
    csil_entries.push((cbor_text("kind"), csil_enc_span_kind(&csil_v.kind)));
    if let Some(csil_inner) = &csil_v.links {
        csil_entries.push((
            cbor_text("links"),
            cbor_enc_array(csil_inner, csil_enc_span_link),
        ));
    }
    csil_entries.push((
        cbor_text("status"),
        csil_enc_span_payload_status(&csil_v.status),
    ));
    if let Some(csil_inner) = &csil_v.resource {
        csil_entries.push((cbor_text("resource"), cbor_text(csil_inner)));
    }
    csil_entries.push((cbor_text("start_at"), cbor_int(csil_v.start_at)));
    csil_entries.push((cbor_text("operation"), cbor_text(&csil_v.operation)));
    csil_entries.push((cbor_text("duration_ms"), cbor_int(csil_v.duration_ms)));
    if let Some(csil_inner) = &csil_v.error_event_id {
        csil_entries.push((cbor_text("error_event_id"), cbor_bytes(csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.parent_span_id {
        csil_entries.push((cbor_text("parent_span_id"), cbor_bytes(csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.sampling_reason {
        csil_entries.push((cbor_text("sampling_reason"), cbor_text(csil_inner)));
    }
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a SpanPayload from a decoded CBOR value tree.
fn csil_dec_span_payload(csil_root: &CsilCborValue) -> Result<SpanPayload, CsilCborError> {
    let operation = {
        let csil_field = cbor_require(csil_root, "operation")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let kind = {
        let csil_field = cbor_require(csil_root, "kind")?;
        let csil_decode = csil_dec_span_kind;
        csil_decode(csil_field)?
    };
    let start_at = {
        let csil_field = cbor_require(csil_root, "start_at")?;
        let csil_decode = cbor_as_i64;
        csil_decode(csil_field)?
    };
    let duration_ms = {
        let csil_field = cbor_require(csil_root, "duration_ms")?;
        let csil_decode = cbor_as_i64;
        csil_decode(csil_field)?
    };
    let status = {
        let csil_field = cbor_require(csil_root, "status")?;
        let csil_decode = csil_dec_span_payload_status;
        csil_decode(csil_field)?
    };
    let resource = match cbor_map_get(csil_root, "resource") {
        Some(csil_field) => {
            let csil_decode = cbor_as_text;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let parent_span_id = match cbor_map_get(csil_root, "parent_span_id") {
        Some(csil_field) => {
            let csil_decode = cbor_as_bytes;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let links = match cbor_map_get(csil_root, "links") {
        Some(csil_field) => {
            let csil_decode = |csil_v| cbor_dec_array(csil_v, csil_dec_span_link);
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let error_event_id = match cbor_map_get(csil_root, "error_event_id") {
        Some(csil_field) => {
            let csil_decode = cbor_as_bytes;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let sampling_reason = match cbor_map_get(csil_root, "sampling_reason") {
        Some(csil_field) => {
            let csil_decode = cbor_as_text;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    Ok(SpanPayload {
        operation,
        kind,
        start_at,
        duration_ms,
        status,
        resource,
        parent_span_id,
        links,
        error_event_id,
        sampling_reason,
    })
}

/// Encode a SpanPayload to canonical CSIL CBOR bytes.
pub fn encode_span_payload(csil_v: &SpanPayload) -> Vec<u8> {
    cbor_encode(&csil_enc_span_payload(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a SpanPayload.
pub fn decode_span_payload(csil_data: &[u8]) -> Result<SpanPayload, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_span_payload(&csil_root)
}

/// Build the canonical CBOR value tree for a HistogramValue.
fn csil_enc_histogram_value(csil_v: &HistogramValue) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(4);
    csil_entries.push((cbor_text("sum"), cbor_float(csil_v.sum)));
    csil_entries.push((cbor_text("count"), cbor_uint(csil_v.count)));
    csil_entries.push((
        cbor_text("bounds"),
        cbor_enc_array(&csil_v.bounds, |csil_elem| cbor_float(*csil_elem)),
    ));
    csil_entries.push((
        cbor_text("counts"),
        cbor_enc_array(&csil_v.counts, |csil_elem| cbor_uint(*csil_elem)),
    ));
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a HistogramValue from a decoded CBOR value tree.
fn csil_dec_histogram_value(csil_root: &CsilCborValue) -> Result<HistogramValue, CsilCborError> {
    let count = {
        let csil_field = cbor_require(csil_root, "count")?;
        let csil_decode = cbor_as_u64;
        csil_decode(csil_field)?
    };
    let sum = {
        let csil_field = cbor_require(csil_root, "sum")?;
        let csil_decode = cbor_as_f64;
        csil_decode(csil_field)?
    };
    let bounds = {
        let csil_field = cbor_require(csil_root, "bounds")?;
        let csil_decode = |csil_v| cbor_dec_array(csil_v, cbor_as_f64);
        csil_decode(csil_field)?
    };
    let counts = {
        let csil_field = cbor_require(csil_root, "counts")?;
        let csil_decode = |csil_v| cbor_dec_array(csil_v, cbor_as_u64);
        csil_decode(csil_field)?
    };
    Ok(HistogramValue {
        count,
        sum,
        bounds,
        counts,
    })
}

/// Encode a HistogramValue to canonical CSIL CBOR bytes.
pub fn encode_histogram_value(csil_v: &HistogramValue) -> Vec<u8> {
    cbor_encode(&csil_enc_histogram_value(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a HistogramValue.
pub fn decode_histogram_value(csil_data: &[u8]) -> Result<HistogramValue, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_histogram_value(&csil_root)
}

/// Build the canonical CBOR value tree for a MetricPointPayload.
fn csil_enc_metric_point_payload(csil_v: &MetricPointPayload) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(12);
    if let Some(csil_inner) = &csil_v.unit {
        csil_entries.push((cbor_text("unit"), cbor_text(csil_inner)));
    }
    csil_entries.push((cbor_text("end_at"), cbor_int(csil_v.end_at)));
    csil_entries.push((
        cbor_text("labels"),
        cbor_enc_array(&csil_v.labels, csil_enc_property),
    ));
    csil_entries.push((cbor_text("start_at"), cbor_int(csil_v.start_at)));
    csil_entries.push((cbor_text("monotonic"), cbor_bool(csil_v.monotonic)));
    if let Some(csil_inner) = &csil_v.description {
        csil_entries.push((cbor_text("description"), cbor_text(csil_inner)));
    }
    csil_entries.push((
        cbor_text("metric_kind"),
        csil_enc_metric_kind(&csil_v.metric_kind),
    ));
    csil_entries.push((cbor_text("metric_name"), cbor_text(&csil_v.metric_name)));
    csil_entries.push((
        cbor_text("temporality"),
        csil_enc_metric_point_payload_temporality(&csil_v.temporality),
    ));
    if let Some(csil_inner) = &csil_v.number_value {
        csil_entries.push((cbor_text("number_value"), cbor_float(*csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.histogram_value {
        csil_entries.push((
            cbor_text("histogram_value"),
            csil_enc_histogram_value(csil_inner),
        ));
    }
    if let Some(csil_inner) = &csil_v.exemplar_trace_id {
        csil_entries.push((cbor_text("exemplar_trace_id"), cbor_bytes(csil_inner)));
    }
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a MetricPointPayload from a decoded CBOR value tree.
fn csil_dec_metric_point_payload(
    csil_root: &CsilCborValue,
) -> Result<MetricPointPayload, CsilCborError> {
    let metric_name = {
        let csil_field = cbor_require(csil_root, "metric_name")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let metric_kind = {
        let csil_field = cbor_require(csil_root, "metric_kind")?;
        let csil_decode = csil_dec_metric_kind;
        csil_decode(csil_field)?
    };
    let unit = match cbor_map_get(csil_root, "unit") {
        Some(csil_field) => {
            let csil_decode = cbor_as_text;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let description = match cbor_map_get(csil_root, "description") {
        Some(csil_field) => {
            let csil_decode = cbor_as_text;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let monotonic = {
        let csil_field = cbor_require(csil_root, "monotonic")?;
        let csil_decode = cbor_as_bool;
        csil_decode(csil_field)?
    };
    let temporality = {
        let csil_field = cbor_require(csil_root, "temporality")?;
        let csil_decode = csil_dec_metric_point_payload_temporality;
        csil_decode(csil_field)?
    };
    let start_at = {
        let csil_field = cbor_require(csil_root, "start_at")?;
        let csil_decode = cbor_as_i64;
        csil_decode(csil_field)?
    };
    let end_at = {
        let csil_field = cbor_require(csil_root, "end_at")?;
        let csil_decode = cbor_as_i64;
        csil_decode(csil_field)?
    };
    let labels = {
        let csil_field = cbor_require(csil_root, "labels")?;
        let csil_decode = |csil_v| cbor_dec_array(csil_v, csil_dec_property);
        csil_decode(csil_field)?
    };
    let number_value = match cbor_map_get(csil_root, "number_value") {
        Some(csil_field) => {
            let csil_decode = cbor_as_f64;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let histogram_value = match cbor_map_get(csil_root, "histogram_value") {
        Some(csil_field) => {
            let csil_decode = csil_dec_histogram_value;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let exemplar_trace_id = match cbor_map_get(csil_root, "exemplar_trace_id") {
        Some(csil_field) => {
            let csil_decode = cbor_as_bytes;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    Ok(MetricPointPayload {
        metric_name,
        metric_kind,
        unit,
        description,
        monotonic,
        temporality,
        start_at,
        end_at,
        labels,
        number_value,
        histogram_value,
        exemplar_trace_id,
    })
}

/// Encode a MetricPointPayload to canonical CSIL CBOR bytes.
pub fn encode_metric_point_payload(csil_v: &MetricPointPayload) -> Vec<u8> {
    cbor_encode(&csil_enc_metric_point_payload(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a MetricPointPayload.
pub fn decode_metric_point_payload(csil_data: &[u8]) -> Result<MetricPointPayload, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_metric_point_payload(&csil_root)
}

/// Build the canonical CBOR value tree for a CampaignTouchPayload.
fn csil_enc_campaign_touch_payload(csil_v: &CampaignTouchPayload) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(4);
    csil_entries.push((
        cbor_text("campaign"),
        csil_enc_campaign_parameters(&csil_v.campaign),
    ));
    if let Some(csil_inner) = &csil_v.referrer {
        csil_entries.push((cbor_text("referrer"), cbor_text(csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.landing_route {
        csil_entries.push((cbor_text("landing_route"), cbor_text(csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.referrer_domain {
        csil_entries.push((cbor_text("referrer_domain"), cbor_text(csil_inner)));
    }
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a CampaignTouchPayload from a decoded CBOR value tree.
fn csil_dec_campaign_touch_payload(
    csil_root: &CsilCborValue,
) -> Result<CampaignTouchPayload, CsilCborError> {
    let campaign = {
        let csil_field = cbor_require(csil_root, "campaign")?;
        let csil_decode = csil_dec_campaign_parameters;
        csil_decode(csil_field)?
    };
    let referrer = match cbor_map_get(csil_root, "referrer") {
        Some(csil_field) => {
            let csil_decode = cbor_as_text;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let referrer_domain = match cbor_map_get(csil_root, "referrer_domain") {
        Some(csil_field) => {
            let csil_decode = cbor_as_text;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let landing_route = match cbor_map_get(csil_root, "landing_route") {
        Some(csil_field) => {
            let csil_decode = cbor_as_text;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    Ok(CampaignTouchPayload {
        campaign,
        referrer,
        referrer_domain,
        landing_route,
    })
}

/// Encode a CampaignTouchPayload to canonical CSIL CBOR bytes.
pub fn encode_campaign_touch_payload(csil_v: &CampaignTouchPayload) -> Vec<u8> {
    cbor_encode(&csil_enc_campaign_touch_payload(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a CampaignTouchPayload.
pub fn decode_campaign_touch_payload(
    csil_data: &[u8],
) -> Result<CampaignTouchPayload, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_campaign_touch_payload(&csil_root)
}

/// Build the canonical CBOR value tree for a CampaignCostPayload.
fn csil_enc_campaign_cost_payload(csil_v: &CampaignCostPayload) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(6);
    csil_entries.push((cbor_text("cost"), csil_enc_decimal(&csil_v.cost)));
    csil_entries.push((cbor_text("campaign"), cbor_text(&csil_v.campaign)));
    csil_entries.push((cbor_text("currency"), cbor_text(&csil_v.currency)));
    if let Some(csil_inner) = &csil_v.platform {
        csil_entries.push((cbor_text("platform"), cbor_text(csil_inner)));
    }
    csil_entries.push((cbor_text("period_end"), cbor_int(csil_v.period_end)));
    csil_entries.push((cbor_text("period_start"), cbor_int(csil_v.period_start)));
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a CampaignCostPayload from a decoded CBOR value tree.
fn csil_dec_campaign_cost_payload(
    csil_root: &CsilCborValue,
) -> Result<CampaignCostPayload, CsilCborError> {
    let campaign = {
        let csil_field = cbor_require(csil_root, "campaign")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let platform = match cbor_map_get(csil_root, "platform") {
        Some(csil_field) => {
            let csil_decode = cbor_as_text;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let cost = {
        let csil_field = cbor_require(csil_root, "cost")?;
        let csil_decode = csil_as_decimal;
        csil_decode(csil_field)?
    };
    let currency = {
        let csil_field = cbor_require(csil_root, "currency")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let period_start = {
        let csil_field = cbor_require(csil_root, "period_start")?;
        let csil_decode = cbor_as_i64;
        csil_decode(csil_field)?
    };
    let period_end = {
        let csil_field = cbor_require(csil_root, "period_end")?;
        let csil_decode = cbor_as_i64;
        csil_decode(csil_field)?
    };
    Ok(CampaignCostPayload {
        campaign,
        platform,
        cost,
        currency,
        period_start,
        period_end,
    })
}

/// Encode a CampaignCostPayload to canonical CSIL CBOR bytes.
pub fn encode_campaign_cost_payload(csil_v: &CampaignCostPayload) -> Vec<u8> {
    cbor_encode(&csil_enc_campaign_cost_payload(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a CampaignCostPayload.
pub fn decode_campaign_cost_payload(
    csil_data: &[u8],
) -> Result<CampaignCostPayload, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_campaign_cost_payload(&csil_root)
}

/// Build the canonical CBOR value tree for a TelemetryItem.
fn csil_enc_telemetry_item(csil_v: &TelemetryItem) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(16);
    if let Some(csil_inner) = &csil_v.span {
        csil_entries.push((cbor_text("span"), csil_enc_span_payload(csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.alias {
        csil_entries.push((cbor_text("alias"), csil_enc_alias_payload(csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.error {
        csil_entries.push((cbor_text("error"), csil_enc_error_payload(csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.event {
        csil_entries.push((cbor_text("event"), csil_enc_event_payload(csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.group {
        csil_entries.push((cbor_text("group"), csil_enc_group_payload(csil_inner)));
    }
    csil_entries.push((cbor_text("envelope"), csil_enc_envelope(&csil_v.envelope)));
    if let Some(csil_inner) = &csil_v.identify {
        csil_entries.push((cbor_text("identify"), csil_enc_identify_payload(csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.page_view {
        csil_entries.push((
            cbor_text("page_view"),
            csil_enc_page_view_payload(csil_inner),
        ));
    }
    if let Some(csil_inner) = &csil_v.conversion {
        csil_entries.push((
            cbor_text("conversion"),
            csil_enc_conversion_payload(csil_inner),
        ));
    }
    if let Some(csil_inner) = &csil_v.interaction {
        csil_entries.push((
            cbor_text("interaction"),
            csil_enc_interaction_payload(csil_inner),
        ));
    }
    if let Some(csil_inner) = &csil_v.session_end {
        csil_entries.push((
            cbor_text("session_end"),
            csil_enc_session_end_payload(csil_inner),
        ));
    }
    if let Some(csil_inner) = &csil_v.metric_point {
        csil_entries.push((
            cbor_text("metric_point"),
            csil_enc_metric_point_payload(csil_inner),
        ));
    }
    if let Some(csil_inner) = &csil_v.campaign_cost {
        csil_entries.push((
            cbor_text("campaign_cost"),
            csil_enc_campaign_cost_payload(csil_inner),
        ));
    }
    if let Some(csil_inner) = &csil_v.session_start {
        csil_entries.push((
            cbor_text("session_start"),
            csil_enc_session_start_payload(csil_inner),
        ));
    }
    if let Some(csil_inner) = &csil_v.campaign_touch {
        csil_entries.push((
            cbor_text("campaign_touch"),
            csil_enc_campaign_touch_payload(csil_inner),
        ));
    }
    if let Some(csil_inner) = &csil_v.feature_exposure {
        csil_entries.push((
            cbor_text("feature_exposure"),
            csil_enc_feature_exposure_payload(csil_inner),
        ));
    }
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a TelemetryItem from a decoded CBOR value tree.
fn csil_dec_telemetry_item(csil_root: &CsilCborValue) -> Result<TelemetryItem, CsilCborError> {
    let envelope = {
        let csil_field = cbor_require(csil_root, "envelope")?;
        let csil_decode = csil_dec_envelope;
        csil_decode(csil_field)?
    };
    let event = match cbor_map_get(csil_root, "event") {
        Some(csil_field) => {
            let csil_decode = csil_dec_event_payload;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let page_view = match cbor_map_get(csil_root, "page_view") {
        Some(csil_field) => {
            let csil_decode = csil_dec_page_view_payload;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let session_start = match cbor_map_get(csil_root, "session_start") {
        Some(csil_field) => {
            let csil_decode = csil_dec_session_start_payload;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let session_end = match cbor_map_get(csil_root, "session_end") {
        Some(csil_field) => {
            let csil_decode = csil_dec_session_end_payload;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let interaction = match cbor_map_get(csil_root, "interaction") {
        Some(csil_field) => {
            let csil_decode = csil_dec_interaction_payload;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let feature_exposure = match cbor_map_get(csil_root, "feature_exposure") {
        Some(csil_field) => {
            let csil_decode = csil_dec_feature_exposure_payload;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let identify = match cbor_map_get(csil_root, "identify") {
        Some(csil_field) => {
            let csil_decode = csil_dec_identify_payload;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let alias = match cbor_map_get(csil_root, "alias") {
        Some(csil_field) => {
            let csil_decode = csil_dec_alias_payload;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let group = match cbor_map_get(csil_root, "group") {
        Some(csil_field) => {
            let csil_decode = csil_dec_group_payload;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let conversion = match cbor_map_get(csil_root, "conversion") {
        Some(csil_field) => {
            let csil_decode = csil_dec_conversion_payload;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let error = match cbor_map_get(csil_root, "error") {
        Some(csil_field) => {
            let csil_decode = csil_dec_error_payload;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let span = match cbor_map_get(csil_root, "span") {
        Some(csil_field) => {
            let csil_decode = csil_dec_span_payload;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let metric_point = match cbor_map_get(csil_root, "metric_point") {
        Some(csil_field) => {
            let csil_decode = csil_dec_metric_point_payload;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let campaign_touch = match cbor_map_get(csil_root, "campaign_touch") {
        Some(csil_field) => {
            let csil_decode = csil_dec_campaign_touch_payload;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let campaign_cost = match cbor_map_get(csil_root, "campaign_cost") {
        Some(csil_field) => {
            let csil_decode = csil_dec_campaign_cost_payload;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    Ok(TelemetryItem {
        envelope,
        event,
        page_view,
        session_start,
        session_end,
        interaction,
        feature_exposure,
        identify,
        alias,
        group,
        conversion,
        error,
        span,
        metric_point,
        campaign_touch,
        campaign_cost,
    })
}

/// Encode a TelemetryItem to canonical CSIL CBOR bytes.
pub fn encode_telemetry_item(csil_v: &TelemetryItem) -> Vec<u8> {
    cbor_encode(&csil_enc_telemetry_item(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a TelemetryItem.
pub fn decode_telemetry_item(csil_data: &[u8]) -> Result<TelemetryItem, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_telemetry_item(&csil_root)
}

/// Build the canonical CBOR value tree for a CaptureRequest.
fn csil_enc_capture_request(csil_v: &CaptureRequest) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(1);
    csil_entries.push((
        cbor_text("items"),
        cbor_enc_array(&csil_v.items, csil_enc_telemetry_item),
    ));
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a CaptureRequest from a decoded CBOR value tree.
fn csil_dec_capture_request(csil_root: &CsilCborValue) -> Result<CaptureRequest, CsilCborError> {
    let items = {
        let csil_field = cbor_require(csil_root, "items")?;
        let csil_decode = |csil_v| cbor_dec_array(csil_v, csil_dec_telemetry_item);
        csil_decode(csil_field)?
    };
    Ok(CaptureRequest { items })
}

/// Encode a CaptureRequest to canonical CSIL CBOR bytes.
pub fn encode_capture_request(csil_v: &CaptureRequest) -> Vec<u8> {
    cbor_encode(&csil_enc_capture_request(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a CaptureRequest.
pub fn decode_capture_request(csil_data: &[u8]) -> Result<CaptureRequest, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_capture_request(&csil_root)
}

/// Build the canonical CBOR value tree for a CaptureResponse.
fn csil_enc_capture_response(csil_v: &CaptureResponse) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(2);
    csil_entries.push((cbor_text("accepted"), cbor_uint(csil_v.accepted)));
    if let Some(csil_inner) = &csil_v.rejected {
        csil_entries.push((
            cbor_text("rejected"),
            cbor_enc_array(csil_inner, csil_enc_rejected_item),
        ));
    }
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a CaptureResponse from a decoded CBOR value tree.
fn csil_dec_capture_response(csil_root: &CsilCborValue) -> Result<CaptureResponse, CsilCborError> {
    let accepted = {
        let csil_field = cbor_require(csil_root, "accepted")?;
        let csil_decode = cbor_as_u64;
        csil_decode(csil_field)?
    };
    let rejected = match cbor_map_get(csil_root, "rejected") {
        Some(csil_field) => {
            let csil_decode = |csil_v| cbor_dec_array(csil_v, csil_dec_rejected_item);
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    Ok(CaptureResponse { accepted, rejected })
}

/// Encode a CaptureResponse to canonical CSIL CBOR bytes.
pub fn encode_capture_response(csil_v: &CaptureResponse) -> Vec<u8> {
    cbor_encode(&csil_enc_capture_response(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a CaptureResponse.
pub fn decode_capture_response(csil_data: &[u8]) -> Result<CaptureResponse, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_capture_response(&csil_root)
}

/// Build the canonical CBOR value tree for a RejectedItem.
fn csil_enc_rejected_item(csil_v: &RejectedItem) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(3);
    csil_entries.push((cbor_text("code"), csil_enc_error_code(&csil_v.code)));
    csil_entries.push((cbor_text("message"), cbor_text(&csil_v.message)));
    csil_entries.push((cbor_text("event_id"), cbor_bytes(&csil_v.event_id)));
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a RejectedItem from a decoded CBOR value tree.
fn csil_dec_rejected_item(csil_root: &CsilCborValue) -> Result<RejectedItem, CsilCborError> {
    let event_id = {
        let csil_field = cbor_require(csil_root, "event_id")?;
        let csil_decode = cbor_as_bytes;
        csil_decode(csil_field)?
    };
    let code = {
        let csil_field = cbor_require(csil_root, "code")?;
        let csil_decode = csil_dec_error_code;
        csil_decode(csil_field)?
    };
    let message = {
        let csil_field = cbor_require(csil_root, "message")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    Ok(RejectedItem {
        event_id,
        code,
        message,
    })
}

/// Encode a RejectedItem to canonical CSIL CBOR bytes.
pub fn encode_rejected_item(csil_v: &RejectedItem) -> Vec<u8> {
    cbor_encode(&csil_enc_rejected_item(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a RejectedItem.
pub fn decode_rejected_item(csil_data: &[u8]) -> Result<RejectedItem, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_rejected_item(&csil_root)
}

/// Build the canonical CBOR value tree for a CaptureCriticalRequest.
fn csil_enc_capture_critical_request(csil_v: &CaptureCriticalRequest) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(1);
    csil_entries.push((
        cbor_text("items"),
        cbor_enc_array(&csil_v.items, csil_enc_telemetry_item),
    ));
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a CaptureCriticalRequest from a decoded CBOR value tree.
fn csil_dec_capture_critical_request(
    csil_root: &CsilCborValue,
) -> Result<CaptureCriticalRequest, CsilCborError> {
    let items = {
        let csil_field = cbor_require(csil_root, "items")?;
        let csil_decode = |csil_v| cbor_dec_array(csil_v, csil_dec_telemetry_item);
        csil_decode(csil_field)?
    };
    Ok(CaptureCriticalRequest { items })
}

/// Encode a CaptureCriticalRequest to canonical CSIL CBOR bytes.
pub fn encode_capture_critical_request(csil_v: &CaptureCriticalRequest) -> Vec<u8> {
    cbor_encode(&csil_enc_capture_critical_request(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a CaptureCriticalRequest.
pub fn decode_capture_critical_request(
    csil_data: &[u8],
) -> Result<CaptureCriticalRequest, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_capture_critical_request(&csil_root)
}

/// Build the canonical CBOR value tree for a CaptureCriticalResponse.
fn csil_enc_capture_critical_response(csil_v: &CaptureCriticalResponse) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(4);
    csil_entries.push((cbor_text("durable"), cbor_bool(csil_v.durable)));
    csil_entries.push((cbor_text("accepted"), cbor_uint(csil_v.accepted)));
    if let Some(csil_inner) = &csil_v.batch_id {
        csil_entries.push((cbor_text("batch_id"), cbor_bytes(csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.rejected {
        csil_entries.push((
            cbor_text("rejected"),
            cbor_enc_array(csil_inner, csil_enc_rejected_item),
        ));
    }
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a CaptureCriticalResponse from a decoded CBOR value tree.
fn csil_dec_capture_critical_response(
    csil_root: &CsilCborValue,
) -> Result<CaptureCriticalResponse, CsilCborError> {
    let accepted = {
        let csil_field = cbor_require(csil_root, "accepted")?;
        let csil_decode = cbor_as_u64;
        csil_decode(csil_field)?
    };
    let durable = {
        let csil_field = cbor_require(csil_root, "durable")?;
        let csil_decode = cbor_as_bool;
        csil_decode(csil_field)?
    };
    let batch_id = match cbor_map_get(csil_root, "batch_id") {
        Some(csil_field) => {
            let csil_decode = cbor_as_bytes;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let rejected = match cbor_map_get(csil_root, "rejected") {
        Some(csil_field) => {
            let csil_decode = |csil_v| cbor_dec_array(csil_v, csil_dec_rejected_item);
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    Ok(CaptureCriticalResponse {
        accepted,
        durable,
        batch_id,
        rejected,
    })
}

/// Encode a CaptureCriticalResponse to canonical CSIL CBOR bytes.
pub fn encode_capture_critical_response(csil_v: &CaptureCriticalResponse) -> Vec<u8> {
    cbor_encode(&csil_enc_capture_critical_response(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a CaptureCriticalResponse.
pub fn decode_capture_critical_response(
    csil_data: &[u8],
) -> Result<CaptureCriticalResponse, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_capture_critical_response(&csil_root)
}

/// Build the canonical CBOR value tree for a PolicyVersionRequest.
fn csil_enc_policy_version_request(_csil_v: &PolicyVersionRequest) -> CsilCborValue {
    CsilCborValue::Map(Vec::new())
}

/// Reconstruct a PolicyVersionRequest from a decoded CBOR value tree.
fn csil_dec_policy_version_request(
    _csil_root: &CsilCborValue,
) -> Result<PolicyVersionRequest, CsilCborError> {
    Ok(PolicyVersionRequest {})
}

/// Encode a PolicyVersionRequest to canonical CSIL CBOR bytes.
pub fn encode_policy_version_request(csil_v: &PolicyVersionRequest) -> Vec<u8> {
    cbor_encode(&csil_enc_policy_version_request(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a PolicyVersionRequest.
pub fn decode_policy_version_request(
    csil_data: &[u8],
) -> Result<PolicyVersionRequest, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_policy_version_request(&csil_root)
}

/// Build the canonical CBOR value tree for a PolicyVersionResponse.
fn csil_enc_policy_version_response(csil_v: &PolicyVersionResponse) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(3);
    csil_entries.push((
        cbor_text("enabled_kinds"),
        cbor_enc_array(&csil_v.enabled_kinds, csil_enc_telemetry_kind),
    ));
    csil_entries.push((cbor_text("sampling_rate"), cbor_float(csil_v.sampling_rate)));
    csil_entries.push((
        cbor_text("policy_version"),
        cbor_uint(csil_v.policy_version),
    ));
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a PolicyVersionResponse from a decoded CBOR value tree.
fn csil_dec_policy_version_response(
    csil_root: &CsilCborValue,
) -> Result<PolicyVersionResponse, CsilCborError> {
    let policy_version = {
        let csil_field = cbor_require(csil_root, "policy_version")?;
        let csil_decode = cbor_as_u64;
        csil_decode(csil_field)?
    };
    let sampling_rate = {
        let csil_field = cbor_require(csil_root, "sampling_rate")?;
        let csil_decode = cbor_as_f64;
        csil_decode(csil_field)?
    };
    let enabled_kinds = {
        let csil_field = cbor_require(csil_root, "enabled_kinds")?;
        let csil_decode = |csil_v| cbor_dec_array(csil_v, csil_dec_telemetry_kind);
        csil_decode(csil_field)?
    };
    Ok(PolicyVersionResponse {
        policy_version,
        sampling_rate,
        enabled_kinds,
    })
}

/// Encode a PolicyVersionResponse to canonical CSIL CBOR bytes.
pub fn encode_policy_version_response(csil_v: &PolicyVersionResponse) -> Vec<u8> {
    cbor_encode(&csil_enc_policy_version_response(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a PolicyVersionResponse.
pub fn decode_policy_version_response(
    csil_data: &[u8],
) -> Result<PolicyVersionResponse, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_policy_version_response(&csil_root)
}

/// Build the canonical CBOR value tree for a TypedValue.
fn csil_enc_typed_value(csil_v: &TypedValue) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(8);
    csil_entries.push((cbor_text("kind"), csil_enc_typed_value_kind(&csil_v.kind)));
    if let Some(csil_inner) = &csil_v.int_value {
        csil_entries.push((cbor_text("int_value"), cbor_int(*csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.bool_value {
        csil_entries.push((cbor_text("bool_value"), cbor_bool(*csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.text_value {
        csil_entries.push((cbor_text("text_value"), cbor_text(csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.uint_value {
        csil_entries.push((cbor_text("uint_value"), cbor_uint(*csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.bytes_value {
        csil_entries.push((cbor_text("bytes_value"), cbor_bytes(csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.float_value {
        csil_entries.push((cbor_text("float_value"), cbor_float(*csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.decimal_value {
        csil_entries.push((cbor_text("decimal_value"), csil_enc_decimal(csil_inner)));
    }
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a TypedValue from a decoded CBOR value tree.
fn csil_dec_typed_value(csil_root: &CsilCborValue) -> Result<TypedValue, CsilCborError> {
    let kind = {
        let csil_field = cbor_require(csil_root, "kind")?;
        let csil_decode = csil_dec_typed_value_kind;
        csil_decode(csil_field)?
    };
    let bool_value = match cbor_map_get(csil_root, "bool_value") {
        Some(csil_field) => {
            let csil_decode = cbor_as_bool;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let int_value = match cbor_map_get(csil_root, "int_value") {
        Some(csil_field) => {
            let csil_decode = cbor_as_i64;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let uint_value = match cbor_map_get(csil_root, "uint_value") {
        Some(csil_field) => {
            let csil_decode = cbor_as_u64;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let float_value = match cbor_map_get(csil_root, "float_value") {
        Some(csil_field) => {
            let csil_decode = cbor_as_f64;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let decimal_value = match cbor_map_get(csil_root, "decimal_value") {
        Some(csil_field) => {
            let csil_decode = csil_as_decimal;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let text_value = match cbor_map_get(csil_root, "text_value") {
        Some(csil_field) => {
            let csil_decode = cbor_as_text;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let bytes_value = match cbor_map_get(csil_root, "bytes_value") {
        Some(csil_field) => {
            let csil_decode = cbor_as_bytes;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    Ok(TypedValue {
        kind,
        bool_value,
        int_value,
        uint_value,
        float_value,
        decimal_value,
        text_value,
        bytes_value,
    })
}

/// Encode a TypedValue to canonical CSIL CBOR bytes.
pub fn encode_typed_value(csil_v: &TypedValue) -> Vec<u8> {
    cbor_encode(&csil_enc_typed_value(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a TypedValue.
pub fn decode_typed_value(csil_data: &[u8]) -> Result<TypedValue, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_typed_value(&csil_root)
}

/// Build the canonical CBOR value tree for a Property.
fn csil_enc_property(csil_v: &Property) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(3);
    csil_entries.push((cbor_text("key"), cbor_text(&csil_v.key)));
    csil_entries.push((cbor_text("value"), csil_enc_typed_value(&csil_v.value)));
    csil_entries.push((
        cbor_text("origin"),
        csil_enc_property_origin(&csil_v.origin),
    ));
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a Property from a decoded CBOR value tree.
fn csil_dec_property(csil_root: &CsilCborValue) -> Result<Property, CsilCborError> {
    let key = {
        let csil_field = cbor_require(csil_root, "key")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let value = {
        let csil_field = cbor_require(csil_root, "value")?;
        let csil_decode = csil_dec_typed_value;
        csil_decode(csil_field)?
    };
    let origin = {
        let csil_field = cbor_require(csil_root, "origin")?;
        let csil_decode = csil_dec_property_origin;
        csil_decode(csil_field)?
    };
    Ok(Property { key, value, origin })
}

/// Encode a Property to canonical CSIL CBOR bytes.
pub fn encode_property(csil_v: &Property) -> Vec<u8> {
    cbor_encode(&csil_enc_property(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a Property.
pub fn decode_property(csil_data: &[u8]) -> Result<Property, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_property(&csil_root)
}

/// Build the canonical CBOR value tree for a Measurement.
fn csil_enc_measurement(csil_v: &Measurement) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(6);
    csil_entries.push((cbor_text("key"), cbor_text(&csil_v.key)));
    csil_entries.push((cbor_text("kind"), csil_enc_measurement_kind(&csil_v.kind)));
    if let Some(csil_inner) = &csil_v.unit {
        csil_entries.push((cbor_text("unit"), cbor_text(csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.int_value {
        csil_entries.push((cbor_text("int_value"), cbor_int(*csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.float_value {
        csil_entries.push((cbor_text("float_value"), cbor_float(*csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.decimal_value {
        csil_entries.push((cbor_text("decimal_value"), csil_enc_decimal(csil_inner)));
    }
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a Measurement from a decoded CBOR value tree.
fn csil_dec_measurement(csil_root: &CsilCborValue) -> Result<Measurement, CsilCborError> {
    let key = {
        let csil_field = cbor_require(csil_root, "key")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let kind = {
        let csil_field = cbor_require(csil_root, "kind")?;
        let csil_decode = csil_dec_measurement_kind;
        csil_decode(csil_field)?
    };
    let float_value = match cbor_map_get(csil_root, "float_value") {
        Some(csil_field) => {
            let csil_decode = cbor_as_f64;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let decimal_value = match cbor_map_get(csil_root, "decimal_value") {
        Some(csil_field) => {
            let csil_decode = csil_as_decimal;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let int_value = match cbor_map_get(csil_root, "int_value") {
        Some(csil_field) => {
            let csil_decode = cbor_as_i64;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let unit = match cbor_map_get(csil_root, "unit") {
        Some(csil_field) => {
            let csil_decode = cbor_as_text;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    Ok(Measurement {
        key,
        kind,
        float_value,
        decimal_value,
        int_value,
        unit,
    })
}

/// Encode a Measurement to canonical CSIL CBOR bytes.
pub fn encode_measurement(csil_v: &Measurement) -> Vec<u8> {
    cbor_encode(&csil_enc_measurement(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a Measurement.
pub fn decode_measurement(csil_data: &[u8]) -> Result<Measurement, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_measurement(&csil_root)
}

/// Build the canonical CBOR value tree for a Consent.
fn csil_enc_consent(csil_v: &Consent) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(3);
    csil_entries.push((
        cbor_text("analytics"),
        csil_enc_consent_state(&csil_v.analytics),
    ));
    csil_entries.push((
        cbor_text("marketing"),
        csil_enc_consent_state(&csil_v.marketing),
    ));
    if let Some(csil_inner) = &csil_v.policy_version {
        csil_entries.push((cbor_text("policy_version"), cbor_text(csil_inner)));
    }
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a Consent from a decoded CBOR value tree.
fn csil_dec_consent(csil_root: &CsilCborValue) -> Result<Consent, CsilCborError> {
    let marketing = {
        let csil_field = cbor_require(csil_root, "marketing")?;
        let csil_decode = csil_dec_consent_state;
        csil_decode(csil_field)?
    };
    let analytics = {
        let csil_field = cbor_require(csil_root, "analytics")?;
        let csil_decode = csil_dec_consent_state;
        csil_decode(csil_field)?
    };
    let policy_version = match cbor_map_get(csil_root, "policy_version") {
        Some(csil_field) => {
            let csil_decode = cbor_as_text;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    Ok(Consent {
        marketing,
        analytics,
        policy_version,
    })
}

/// Encode a Consent to canonical CSIL CBOR bytes.
pub fn encode_consent(csil_v: &Consent) -> Vec<u8> {
    cbor_encode(&csil_enc_consent(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a Consent.
pub fn decode_consent(csil_data: &[u8]) -> Result<Consent, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_consent(&csil_root)
}

/// Build the canonical CBOR value tree for a Envelope.
fn csil_enc_envelope(csil_v: &Envelope) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(23);
    csil_entries.push((cbor_text("kind"), csil_enc_telemetry_kind(&csil_v.kind)));
    if let Some(csil_inner) = &csil_v.consent {
        csil_entries.push((cbor_text("consent"), csil_enc_consent(csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.release {
        csil_entries.push((cbor_text("release"), cbor_text(csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.span_id {
        csil_entries.push((cbor_text("span_id"), cbor_bytes(csil_inner)));
    }
    csil_entries.push((cbor_text("event_id"), cbor_bytes(&csil_v.event_id)));
    csil_entries.push((cbor_text("sdk_name"), cbor_text(&csil_v.sdk_name)));
    if let Some(csil_inner) = &csil_v.sequence {
        csil_entries.push((cbor_text("sequence"), cbor_uint(*csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.trace_id {
        csil_entries.push((cbor_text("trace_id"), cbor_bytes(csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.source_id {
        csil_entries.push((cbor_text("source_id"), cbor_bytes(csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.project_id {
        csil_entries.push((cbor_text("project_id"), cbor_bytes(csil_inner)));
    }
    csil_entries.push((
        cbor_text("properties"),
        cbor_enc_array(&csil_v.properties, csil_enc_property),
    ));
    if let Some(csil_inner) = &csil_v.request_id {
        csil_entries.push((cbor_text("request_id"), cbor_text(csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.session_id {
        csil_entries.push((cbor_text("session_id"), cbor_text(csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.end_user_id {
        csil_entries.push((cbor_text("end_user_id"), cbor_text(csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.observed_at {
        csil_entries.push((cbor_text("observed_at"), cbor_int(*csil_inner)));
    }
    csil_entries.push((cbor_text("occurred_at"), cbor_int(csil_v.occurred_at)));
    if let Some(csil_inner) = &csil_v.received_at {
        csil_entries.push((cbor_text("received_at"), cbor_int(*csil_inner)));
    }
    csil_entries.push((cbor_text("sdk_version"), cbor_text(&csil_v.sdk_version)));
    if let Some(csil_inner) = &csil_v.anonymous_id {
        csil_entries.push((cbor_text("anonymous_id"), cbor_text(csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.measurements {
        csil_entries.push((
            cbor_text("measurements"),
            cbor_enc_array(csil_inner, csil_enc_measurement),
        ));
    }
    if let Some(csil_inner) = &csil_v.service_name {
        csil_entries.push((cbor_text("service_name"), cbor_text(csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.workspace_id {
        csil_entries.push((cbor_text("workspace_id"), cbor_bytes(csil_inner)));
    }
    csil_entries.push((
        cbor_text("schema_version"),
        cbor_uint(csil_v.schema_version),
    ));
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a Envelope from a decoded CBOR value tree.
fn csil_dec_envelope(csil_root: &CsilCborValue) -> Result<Envelope, CsilCborError> {
    let event_id = {
        let csil_field = cbor_require(csil_root, "event_id")?;
        let csil_decode = cbor_as_bytes;
        csil_decode(csil_field)?
    };
    let kind = {
        let csil_field = cbor_require(csil_root, "kind")?;
        let csil_decode = csil_dec_telemetry_kind;
        csil_decode(csil_field)?
    };
    let schema_version = {
        let csil_field = cbor_require(csil_root, "schema_version")?;
        let csil_decode = cbor_as_u64;
        csil_decode(csil_field)?
    };
    let occurred_at = {
        let csil_field = cbor_require(csil_root, "occurred_at")?;
        let csil_decode = cbor_as_i64;
        csil_decode(csil_field)?
    };
    let observed_at = match cbor_map_get(csil_root, "observed_at") {
        Some(csil_field) => {
            let csil_decode = cbor_as_i64;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let received_at = match cbor_map_get(csil_root, "received_at") {
        Some(csil_field) => {
            let csil_decode = cbor_as_i64;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let workspace_id = match cbor_map_get(csil_root, "workspace_id") {
        Some(csil_field) => {
            let csil_decode = cbor_as_bytes;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let project_id = match cbor_map_get(csil_root, "project_id") {
        Some(csil_field) => {
            let csil_decode = cbor_as_bytes;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let source_id = match cbor_map_get(csil_root, "source_id") {
        Some(csil_field) => {
            let csil_decode = cbor_as_bytes;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let sequence = match cbor_map_get(csil_root, "sequence") {
        Some(csil_field) => {
            let csil_decode = cbor_as_u64;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let release = match cbor_map_get(csil_root, "release") {
        Some(csil_field) => {
            let csil_decode = cbor_as_text;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let service_name = match cbor_map_get(csil_root, "service_name") {
        Some(csil_field) => {
            let csil_decode = cbor_as_text;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let request_id = match cbor_map_get(csil_root, "request_id") {
        Some(csil_field) => {
            let csil_decode = cbor_as_text;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let session_id = match cbor_map_get(csil_root, "session_id") {
        Some(csil_field) => {
            let csil_decode = cbor_as_text;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let end_user_id = match cbor_map_get(csil_root, "end_user_id") {
        Some(csil_field) => {
            let csil_decode = cbor_as_text;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let anonymous_id = match cbor_map_get(csil_root, "anonymous_id") {
        Some(csil_field) => {
            let csil_decode = cbor_as_text;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let trace_id = match cbor_map_get(csil_root, "trace_id") {
        Some(csil_field) => {
            let csil_decode = cbor_as_bytes;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let span_id = match cbor_map_get(csil_root, "span_id") {
        Some(csil_field) => {
            let csil_decode = cbor_as_bytes;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let consent = match cbor_map_get(csil_root, "consent") {
        Some(csil_field) => {
            let csil_decode = csil_dec_consent;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let sdk_name = {
        let csil_field = cbor_require(csil_root, "sdk_name")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let sdk_version = {
        let csil_field = cbor_require(csil_root, "sdk_version")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let properties = {
        let csil_field = cbor_require(csil_root, "properties")?;
        let csil_decode = |csil_v| cbor_dec_array(csil_v, csil_dec_property);
        csil_decode(csil_field)?
    };
    let measurements = match cbor_map_get(csil_root, "measurements") {
        Some(csil_field) => {
            let csil_decode = |csil_v| cbor_dec_array(csil_v, csil_dec_measurement);
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    Ok(Envelope {
        event_id,
        kind,
        schema_version,
        occurred_at,
        observed_at,
        received_at,
        workspace_id,
        project_id,
        source_id,
        sequence,
        release,
        service_name,
        request_id,
        session_id,
        end_user_id,
        anonymous_id,
        trace_id,
        span_id,
        consent,
        sdk_name,
        sdk_version,
        properties,
        measurements,
    })
}

/// Encode a Envelope to canonical CSIL CBOR bytes.
pub fn encode_envelope(csil_v: &Envelope) -> Vec<u8> {
    cbor_encode(&csil_enc_envelope(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a Envelope.
pub fn decode_envelope(csil_data: &[u8]) -> Result<Envelope, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_envelope(&csil_root)
}

/// Build the canonical CBOR value tree for a ServiceError.
fn csil_enc_service_error(csil_v: &ServiceError) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(4);
    csil_entries.push((cbor_text("code"), csil_enc_error_code(&csil_v.code)));
    if let Some(csil_inner) = &csil_v.detail {
        csil_entries.push((
            cbor_text("detail"),
            cbor_enc_array(csil_inner, csil_enc_property),
        ));
    }
    csil_entries.push((cbor_text("message"), cbor_text(&csil_v.message)));
    csil_entries.push((cbor_text("retryable"), cbor_bool(csil_v.retryable)));
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a ServiceError from a decoded CBOR value tree.
fn csil_dec_service_error(csil_root: &CsilCborValue) -> Result<ServiceError, CsilCborError> {
    let code = {
        let csil_field = cbor_require(csil_root, "code")?;
        let csil_decode = csil_dec_error_code;
        csil_decode(csil_field)?
    };
    let message = {
        let csil_field = cbor_require(csil_root, "message")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let retryable = {
        let csil_field = cbor_require(csil_root, "retryable")?;
        let csil_decode = cbor_as_bool;
        csil_decode(csil_field)?
    };
    let detail = match cbor_map_get(csil_root, "detail") {
        Some(csil_field) => {
            let csil_decode = |csil_v| cbor_dec_array(csil_v, csil_dec_property);
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    Ok(ServiceError {
        code,
        message,
        retryable,
        detail,
    })
}

/// Encode a ServiceError to canonical CSIL CBOR bytes.
pub fn encode_service_error(csil_v: &ServiceError) -> Vec<u8> {
    cbor_encode(&csil_enc_service_error(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a ServiceError.
pub fn decode_service_error(csil_data: &[u8]) -> Result<ServiceError, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_service_error(&csil_root)
}

/// Encode a SpanKind enum as its bare literal value.
fn csil_enc_span_kind(csil_v: &SpanKind) -> CsilCborValue {
    match csil_v {
        SpanKind::Internal => cbor_text("internal"),
        SpanKind::Server => cbor_text("server"),
        SpanKind::Client => cbor_text("client"),
        SpanKind::Producer => cbor_text("producer"),
        SpanKind::Consumer => cbor_text("consumer"),
    }
}

/// Decode a bare literal value into a SpanKind enum.
fn csil_dec_span_kind(csil_v: &CsilCborValue) -> Result<SpanKind, CsilCborError> {
    let csil_val = cbor_as_text(csil_v)?;
    match csil_val.as_str() {
        "internal" => Ok(SpanKind::Internal),
        "server" => Ok(SpanKind::Server),
        "client" => Ok(SpanKind::Client),
        "producer" => Ok(SpanKind::Producer),
        "consumer" => Ok(SpanKind::Consumer),
        csil_other => Err(CsilCborError(format!(
            "csil cbor: unknown SpanKind value {csil_other:?}"
        ))),
    }
}

/// Encode a MetricKind enum as its bare literal value.
fn csil_enc_metric_kind(csil_v: &MetricKind) -> CsilCborValue {
    match csil_v {
        MetricKind::Counter => cbor_text("counter"),
        MetricKind::Gauge => cbor_text("gauge"),
        MetricKind::Histogram => cbor_text("histogram"),
    }
}

/// Decode a bare literal value into a MetricKind enum.
fn csil_dec_metric_kind(csil_v: &CsilCborValue) -> Result<MetricKind, CsilCborError> {
    let csil_val = cbor_as_text(csil_v)?;
    match csil_val.as_str() {
        "counter" => Ok(MetricKind::Counter),
        "gauge" => Ok(MetricKind::Gauge),
        "histogram" => Ok(MetricKind::Histogram),
        csil_other => Err(CsilCborError(format!(
            "csil cbor: unknown MetricKind value {csil_other:?}"
        ))),
    }
}

/// Encode a TypedValueKind enum as its bare literal value.
fn csil_enc_typed_value_kind(csil_v: &TypedValueKind) -> CsilCborValue {
    match csil_v {
        TypedValueKind::Null => cbor_text("null"),
        TypedValueKind::Bool => cbor_text("bool"),
        TypedValueKind::Int => cbor_text("int"),
        TypedValueKind::Uint => cbor_text("uint"),
        TypedValueKind::Float => cbor_text("float"),
        TypedValueKind::Decimal => cbor_text("decimal"),
        TypedValueKind::Text => cbor_text("text"),
        TypedValueKind::Bytes => cbor_text("bytes"),
    }
}

/// Decode a bare literal value into a TypedValueKind enum.
fn csil_dec_typed_value_kind(csil_v: &CsilCborValue) -> Result<TypedValueKind, CsilCborError> {
    let csil_val = cbor_as_text(csil_v)?;
    match csil_val.as_str() {
        "null" => Ok(TypedValueKind::Null),
        "bool" => Ok(TypedValueKind::Bool),
        "int" => Ok(TypedValueKind::Int),
        "uint" => Ok(TypedValueKind::Uint),
        "float" => Ok(TypedValueKind::Float),
        "decimal" => Ok(TypedValueKind::Decimal),
        "text" => Ok(TypedValueKind::Text),
        "bytes" => Ok(TypedValueKind::Bytes),
        csil_other => Err(CsilCborError(format!(
            "csil cbor: unknown TypedValueKind value {csil_other:?}"
        ))),
    }
}

/// Encode a PropertyOrigin enum as its bare literal value.
fn csil_enc_property_origin(csil_v: &PropertyOrigin) -> CsilCborValue {
    match csil_v {
        PropertyOrigin::Client => cbor_text("client"),
        PropertyOrigin::Driver => cbor_text("driver"),
        PropertyOrigin::Collector => cbor_text("collector"),
    }
}

/// Decode a bare literal value into a PropertyOrigin enum.
fn csil_dec_property_origin(csil_v: &CsilCborValue) -> Result<PropertyOrigin, CsilCborError> {
    let csil_val = cbor_as_text(csil_v)?;
    match csil_val.as_str() {
        "client" => Ok(PropertyOrigin::Client),
        "driver" => Ok(PropertyOrigin::Driver),
        "collector" => Ok(PropertyOrigin::Collector),
        csil_other => Err(CsilCborError(format!(
            "csil cbor: unknown PropertyOrigin value {csil_other:?}"
        ))),
    }
}

/// Encode a MeasurementKind enum as its bare literal value.
fn csil_enc_measurement_kind(csil_v: &MeasurementKind) -> CsilCborValue {
    match csil_v {
        MeasurementKind::Float => cbor_text("float"),
        MeasurementKind::Decimal => cbor_text("decimal"),
        MeasurementKind::Int => cbor_text("int"),
    }
}

/// Decode a bare literal value into a MeasurementKind enum.
fn csil_dec_measurement_kind(csil_v: &CsilCborValue) -> Result<MeasurementKind, CsilCborError> {
    let csil_val = cbor_as_text(csil_v)?;
    match csil_val.as_str() {
        "float" => Ok(MeasurementKind::Float),
        "decimal" => Ok(MeasurementKind::Decimal),
        "int" => Ok(MeasurementKind::Int),
        csil_other => Err(CsilCborError(format!(
            "csil cbor: unknown MeasurementKind value {csil_other:?}"
        ))),
    }
}

/// Encode a ConsentState enum as its bare literal value.
fn csil_enc_consent_state(csil_v: &ConsentState) -> CsilCborValue {
    match csil_v {
        ConsentState::Granted => cbor_text("granted"),
        ConsentState::Denied => cbor_text("denied"),
        ConsentState::Absent => cbor_text("absent"),
    }
}

/// Decode a bare literal value into a ConsentState enum.
fn csil_dec_consent_state(csil_v: &CsilCborValue) -> Result<ConsentState, CsilCborError> {
    let csil_val = cbor_as_text(csil_v)?;
    match csil_val.as_str() {
        "granted" => Ok(ConsentState::Granted),
        "denied" => Ok(ConsentState::Denied),
        "absent" => Ok(ConsentState::Absent),
        csil_other => Err(CsilCborError(format!(
            "csil cbor: unknown ConsentState value {csil_other:?}"
        ))),
    }
}

/// Encode a TelemetryKind enum as its bare literal value.
fn csil_enc_telemetry_kind(csil_v: &TelemetryKind) -> CsilCborValue {
    match csil_v {
        TelemetryKind::Event => cbor_text("event"),
        TelemetryKind::PageView => cbor_text("page-view"),
        TelemetryKind::SessionStart => cbor_text("session-start"),
        TelemetryKind::SessionEnd => cbor_text("session-end"),
        TelemetryKind::SessionHeartbeat => cbor_text("session-heartbeat"),
        TelemetryKind::Interaction => cbor_text("interaction"),
        TelemetryKind::FeatureExposure => cbor_text("feature-exposure"),
        TelemetryKind::Identify => cbor_text("identify"),
        TelemetryKind::Alias => cbor_text("alias"),
        TelemetryKind::Group => cbor_text("group"),
        TelemetryKind::Conversion => cbor_text("conversion"),
        TelemetryKind::Error => cbor_text("error"),
        TelemetryKind::Span => cbor_text("span"),
        TelemetryKind::MetricPoint => cbor_text("metric-point"),
        TelemetryKind::CampaignTouch => cbor_text("campaign-touch"),
        TelemetryKind::CampaignCost => cbor_text("campaign-cost"),
    }
}

/// Decode a bare literal value into a TelemetryKind enum.
fn csil_dec_telemetry_kind(csil_v: &CsilCborValue) -> Result<TelemetryKind, CsilCborError> {
    let csil_val = cbor_as_text(csil_v)?;
    match csil_val.as_str() {
        "event" => Ok(TelemetryKind::Event),
        "page-view" => Ok(TelemetryKind::PageView),
        "session-start" => Ok(TelemetryKind::SessionStart),
        "session-end" => Ok(TelemetryKind::SessionEnd),
        "session-heartbeat" => Ok(TelemetryKind::SessionHeartbeat),
        "interaction" => Ok(TelemetryKind::Interaction),
        "feature-exposure" => Ok(TelemetryKind::FeatureExposure),
        "identify" => Ok(TelemetryKind::Identify),
        "alias" => Ok(TelemetryKind::Alias),
        "group" => Ok(TelemetryKind::Group),
        "conversion" => Ok(TelemetryKind::Conversion),
        "error" => Ok(TelemetryKind::Error),
        "span" => Ok(TelemetryKind::Span),
        "metric-point" => Ok(TelemetryKind::MetricPoint),
        "campaign-touch" => Ok(TelemetryKind::CampaignTouch),
        "campaign-cost" => Ok(TelemetryKind::CampaignCost),
        csil_other => Err(CsilCborError(format!(
            "csil cbor: unknown TelemetryKind value {csil_other:?}"
        ))),
    }
}

/// Encode a ErrorCode enum as its bare literal value.
fn csil_enc_error_code(csil_v: &ErrorCode) -> CsilCborValue {
    match csil_v {
        ErrorCode::InvalidArgument => cbor_text("invalid-argument"),
        ErrorCode::Unauthenticated => cbor_text("unauthenticated"),
        ErrorCode::PermissionDenied => cbor_text("permission-denied"),
        ErrorCode::NotFound => cbor_text("not-found"),
        ErrorCode::AlreadyExists => cbor_text("already-exists"),
        ErrorCode::ResourceExhausted => cbor_text("resource-exhausted"),
        ErrorCode::FailedPrecondition => cbor_text("failed-precondition"),
        ErrorCode::Unavailable => cbor_text("unavailable"),
        ErrorCode::SchemaUnsupported => cbor_text("schema-unsupported"),
        ErrorCode::BudgetExceeded => cbor_text("budget-exceeded"),
        ErrorCode::IncompleteResult => cbor_text("incomplete-result"),
        ErrorCode::Internal => cbor_text("internal"),
    }
}

/// Decode a bare literal value into a ErrorCode enum.
fn csil_dec_error_code(csil_v: &CsilCborValue) -> Result<ErrorCode, CsilCborError> {
    let csil_val = cbor_as_text(csil_v)?;
    match csil_val.as_str() {
        "invalid-argument" => Ok(ErrorCode::InvalidArgument),
        "unauthenticated" => Ok(ErrorCode::Unauthenticated),
        "permission-denied" => Ok(ErrorCode::PermissionDenied),
        "not-found" => Ok(ErrorCode::NotFound),
        "already-exists" => Ok(ErrorCode::AlreadyExists),
        "resource-exhausted" => Ok(ErrorCode::ResourceExhausted),
        "failed-precondition" => Ok(ErrorCode::FailedPrecondition),
        "unavailable" => Ok(ErrorCode::Unavailable),
        "schema-unsupported" => Ok(ErrorCode::SchemaUnsupported),
        "budget-exceeded" => Ok(ErrorCode::BudgetExceeded),
        "incomplete-result" => Ok(ErrorCode::IncompleteResult),
        "internal" => Ok(ErrorCode::Internal),
        csil_other => Err(CsilCborError(format!(
            "csil cbor: unknown ErrorCode value {csil_other:?}"
        ))),
    }
}

/// Encode a SessionEndPayload_reason enum as its bare literal value.
fn csil_enc_session_end_payload_reason(csil_v: &SessionEndPayload_reason) -> CsilCborValue {
    match csil_v {
        SessionEndPayload_reason::Explicit => cbor_text("explicit"),
        SessionEndPayload_reason::Timeout => cbor_text("timeout"),
        SessionEndPayload_reason::MaximumLifetime => cbor_text("maximum-lifetime"),
    }
}

/// Decode a bare literal value into a SessionEndPayload_reason enum.
fn csil_dec_session_end_payload_reason(
    csil_v: &CsilCborValue,
) -> Result<SessionEndPayload_reason, CsilCborError> {
    let csil_val = cbor_as_text(csil_v)?;
    match csil_val.as_str() {
        "explicit" => Ok(SessionEndPayload_reason::Explicit),
        "timeout" => Ok(SessionEndPayload_reason::Timeout),
        "maximum-lifetime" => Ok(SessionEndPayload_reason::MaximumLifetime),
        csil_other => Err(CsilCborError(format!(
            "csil cbor: unknown SessionEndPayload_reason value {csil_other:?}"
        ))),
    }
}

/// Encode a ErrorPayload_severity enum as its bare literal value.
fn csil_enc_error_payload_severity(csil_v: &ErrorPayload_severity) -> CsilCborValue {
    match csil_v {
        ErrorPayload_severity::Fatal => cbor_text("fatal"),
        ErrorPayload_severity::Error => cbor_text("error"),
        ErrorPayload_severity::Warning => cbor_text("warning"),
        ErrorPayload_severity::Info => cbor_text("info"),
    }
}

/// Decode a bare literal value into a ErrorPayload_severity enum.
fn csil_dec_error_payload_severity(
    csil_v: &CsilCborValue,
) -> Result<ErrorPayload_severity, CsilCborError> {
    let csil_val = cbor_as_text(csil_v)?;
    match csil_val.as_str() {
        "fatal" => Ok(ErrorPayload_severity::Fatal),
        "error" => Ok(ErrorPayload_severity::Error),
        "warning" => Ok(ErrorPayload_severity::Warning),
        "info" => Ok(ErrorPayload_severity::Info),
        csil_other => Err(CsilCborError(format!(
            "csil cbor: unknown ErrorPayload_severity value {csil_other:?}"
        ))),
    }
}

/// Encode a SpanPayload_status enum as its bare literal value.
fn csil_enc_span_payload_status(csil_v: &SpanPayload_status) -> CsilCborValue {
    match csil_v {
        SpanPayload_status::Ok => cbor_text("ok"),
        SpanPayload_status::Error => cbor_text("error"),
        SpanPayload_status::Unset => cbor_text("unset"),
    }
}

/// Decode a bare literal value into a SpanPayload_status enum.
fn csil_dec_span_payload_status(
    csil_v: &CsilCborValue,
) -> Result<SpanPayload_status, CsilCborError> {
    let csil_val = cbor_as_text(csil_v)?;
    match csil_val.as_str() {
        "ok" => Ok(SpanPayload_status::Ok),
        "error" => Ok(SpanPayload_status::Error),
        "unset" => Ok(SpanPayload_status::Unset),
        csil_other => Err(CsilCborError(format!(
            "csil cbor: unknown SpanPayload_status value {csil_other:?}"
        ))),
    }
}

/// Encode a MetricPointPayload_temporality enum as its bare literal value.
fn csil_enc_metric_point_payload_temporality(
    csil_v: &MetricPointPayload_temporality,
) -> CsilCborValue {
    match csil_v {
        MetricPointPayload_temporality::Delta => cbor_text("delta"),
        MetricPointPayload_temporality::Cumulative => cbor_text("cumulative"),
    }
}

/// Decode a bare literal value into a MetricPointPayload_temporality enum.
fn csil_dec_metric_point_payload_temporality(
    csil_v: &CsilCborValue,
) -> Result<MetricPointPayload_temporality, CsilCborError> {
    let csil_val = cbor_as_text(csil_v)?;
    match csil_val.as_str() {
        "delta" => Ok(MetricPointPayload_temporality::Delta),
        "cumulative" => Ok(MetricPointPayload_temporality::Cumulative),
        csil_other => Err(CsilCborError(format!(
            "csil cbor: unknown MetricPointPayload_temporality value {csil_other:?}"
        ))),
    }
}
