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

/// Build the canonical CBOR value tree for a GroupRef.
fn csil_enc_group_ref(csil_v: &GroupRef) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(2);
    csil_entries.push((cbor_text("kind"), csil_enc_group_kind(&csil_v.kind)));
    if let Some(csil_inner) = &csil_v.name {
        csil_entries.push((cbor_text("name"), cbor_text(csil_inner)));
    }
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a GroupRef from a decoded CBOR value tree.
fn csil_dec_group_ref(csil_root: &CsilCborValue) -> Result<GroupRef, CsilCborError> {
    let kind = {
        let csil_field = cbor_require(csil_root, "kind")?;
        let csil_decode = csil_dec_group_kind;
        csil_decode(csil_field)?
    };
    let name = match cbor_map_get(csil_root, "name") {
        Some(csil_field) => {
            let csil_decode = cbor_as_text;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    Ok(GroupRef { kind, name })
}

/// Encode a GroupRef to canonical CSIL CBOR bytes.
pub fn encode_group_ref(csil_v: &GroupRef) -> Vec<u8> {
    cbor_encode(&csil_enc_group_ref(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a GroupRef.
pub fn decode_group_ref(csil_data: &[u8]) -> Result<GroupRef, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_group_ref(&csil_root)
}

/// Build the canonical CBOR value tree for a GroupMember.
fn csil_enc_group_member(csil_v: &GroupMember) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(5);
    csil_entries.push((cbor_text("node"), cbor_text(&csil_v.node)));
    csil_entries.push((cbor_text("role"), csil_enc_member_role(&csil_v.role)));
    if let Some(csil_inner) = &csil_v.domain {
        csil_entries.push((cbor_text("domain"), cbor_text(csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.region {
        csil_entries.push((cbor_text("region"), cbor_text(csil_inner)));
    }
    csil_entries.push((cbor_text("address"), cbor_text(&csil_v.address)));
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a GroupMember from a decoded CBOR value tree.
fn csil_dec_group_member(csil_root: &CsilCborValue) -> Result<GroupMember, CsilCborError> {
    let node = {
        let csil_field = cbor_require(csil_root, "node")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let role = {
        let csil_field = cbor_require(csil_root, "role")?;
        let csil_decode = csil_dec_member_role;
        csil_decode(csil_field)?
    };
    let address = {
        let csil_field = cbor_require(csil_root, "address")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let region = match cbor_map_get(csil_root, "region") {
        Some(csil_field) => {
            let csil_decode = cbor_as_text;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let domain = match cbor_map_get(csil_root, "domain") {
        Some(csil_field) => {
            let csil_decode = cbor_as_text;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    Ok(GroupMember {
        node,
        role,
        address,
        region,
        domain,
    })
}

/// Encode a GroupMember to canonical CSIL CBOR bytes.
pub fn encode_group_member(csil_v: &GroupMember) -> Vec<u8> {
    cbor_encode(&csil_enc_group_member(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a GroupMember.
pub fn decode_group_member(csil_data: &[u8]) -> Result<GroupMember, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_group_member(&csil_root)
}

/// Build the canonical CBOR value tree for a ConsensusMessage.
fn csil_enc_consensus_message(csil_v: &ConsensusMessage) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(5);
    csil_entries.push((cbor_text("kind"), csil_enc_consensus_kind(&csil_v.kind)));
    csil_entries.push((cbor_text("group"), csil_enc_group_ref(&csil_v.group)));
    csil_entries.push((cbor_text("sender"), cbor_text(&csil_v.sender)));
    csil_entries.push((cbor_text("payload"), cbor_bytes(&csil_v.payload)));
    csil_entries.push((cbor_text("generation"), cbor_uint(csil_v.generation)));
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a ConsensusMessage from a decoded CBOR value tree.
fn csil_dec_consensus_message(
    csil_root: &CsilCborValue,
) -> Result<ConsensusMessage, CsilCborError> {
    let group = {
        let csil_field = cbor_require(csil_root, "group")?;
        let csil_decode = csil_dec_group_ref;
        csil_decode(csil_field)?
    };
    let kind = {
        let csil_field = cbor_require(csil_root, "kind")?;
        let csil_decode = csil_dec_consensus_kind;
        csil_decode(csil_field)?
    };
    let sender = {
        let csil_field = cbor_require(csil_root, "sender")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let generation = {
        let csil_field = cbor_require(csil_root, "generation")?;
        let csil_decode = cbor_as_u64;
        csil_decode(csil_field)?
    };
    let payload = {
        let csil_field = cbor_require(csil_root, "payload")?;
        let csil_decode = cbor_as_bytes;
        csil_decode(csil_field)?
    };
    Ok(ConsensusMessage {
        group,
        kind,
        sender,
        generation,
        payload,
    })
}

/// Encode a ConsensusMessage to canonical CSIL CBOR bytes.
pub fn encode_consensus_message(csil_v: &ConsensusMessage) -> Vec<u8> {
    cbor_encode(&csil_enc_consensus_message(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a ConsensusMessage.
pub fn decode_consensus_message(csil_data: &[u8]) -> Result<ConsensusMessage, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_consensus_message(&csil_root)
}

/// Build the canonical CBOR value tree for a ConsensusReply.
fn csil_enc_consensus_reply(csil_v: &ConsensusReply) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(4);
    if let Some(csil_inner) = &csil_v.payload {
        csil_entries.push((cbor_text("payload"), cbor_bytes(csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.refusal {
        csil_entries.push((cbor_text("refusal"), cbor_text(csil_inner)));
    }
    csil_entries.push((cbor_text("accepted"), cbor_bool(csil_v.accepted)));
    if let Some(csil_inner) = &csil_v.current_generation {
        csil_entries.push((cbor_text("current_generation"), cbor_uint(*csil_inner)));
    }
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a ConsensusReply from a decoded CBOR value tree.
fn csil_dec_consensus_reply(csil_root: &CsilCborValue) -> Result<ConsensusReply, CsilCborError> {
    let accepted = {
        let csil_field = cbor_require(csil_root, "accepted")?;
        let csil_decode = cbor_as_bool;
        csil_decode(csil_field)?
    };
    let payload = match cbor_map_get(csil_root, "payload") {
        Some(csil_field) => {
            let csil_decode = cbor_as_bytes;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let current_generation = match cbor_map_get(csil_root, "current_generation") {
        Some(csil_field) => {
            let csil_decode = cbor_as_u64;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let refusal = match cbor_map_get(csil_root, "refusal") {
        Some(csil_field) => {
            let csil_decode = cbor_as_text;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    Ok(ConsensusReply {
        accepted,
        payload,
        current_generation,
        refusal,
    })
}

/// Encode a ConsensusReply to canonical CSIL CBOR bytes.
pub fn encode_consensus_reply(csil_v: &ConsensusReply) -> Vec<u8> {
    cbor_encode(&csil_enc_consensus_reply(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a ConsensusReply.
pub fn decode_consensus_reply(csil_data: &[u8]) -> Result<ConsensusReply, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_consensus_reply(&csil_root)
}

/// Build the canonical CBOR value tree for a SnapshotChunkRequest.
fn csil_enc_snapshot_chunk_request(csil_v: &SnapshotChunkRequest) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(4);
    csil_entries.push((cbor_text("group"), csil_enc_group_ref(&csil_v.group)));
    csil_entries.push((cbor_text("offset"), cbor_uint(csil_v.offset)));
    csil_entries.push((cbor_text("max_bytes"), cbor_uint(csil_v.max_bytes)));
    if let Some(csil_inner) = &csil_v.snapshot_id {
        csil_entries.push((cbor_text("snapshot_id"), cbor_text(csil_inner)));
    }
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a SnapshotChunkRequest from a decoded CBOR value tree.
fn csil_dec_snapshot_chunk_request(
    csil_root: &CsilCborValue,
) -> Result<SnapshotChunkRequest, CsilCborError> {
    let group = {
        let csil_field = cbor_require(csil_root, "group")?;
        let csil_decode = csil_dec_group_ref;
        csil_decode(csil_field)?
    };
    let snapshot_id = match cbor_map_get(csil_root, "snapshot_id") {
        Some(csil_field) => {
            let csil_decode = cbor_as_text;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let offset = {
        let csil_field = cbor_require(csil_root, "offset")?;
        let csil_decode = cbor_as_u64;
        csil_decode(csil_field)?
    };
    let max_bytes = {
        let csil_field = cbor_require(csil_root, "max_bytes")?;
        let csil_decode = cbor_as_u64;
        csil_decode(csil_field)?
    };
    Ok(SnapshotChunkRequest {
        group,
        snapshot_id,
        offset,
        max_bytes,
    })
}

/// Encode a SnapshotChunkRequest to canonical CSIL CBOR bytes.
pub fn encode_snapshot_chunk_request(csil_v: &SnapshotChunkRequest) -> Vec<u8> {
    cbor_encode(&csil_enc_snapshot_chunk_request(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a SnapshotChunkRequest.
pub fn decode_snapshot_chunk_request(
    csil_data: &[u8],
) -> Result<SnapshotChunkRequest, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_snapshot_chunk_request(&csil_root)
}

/// Build the canonical CBOR value tree for a SnapshotChunk.
fn csil_enc_snapshot_chunk(csil_v: &SnapshotChunk) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(6);
    csil_entries.push((cbor_text("data"), cbor_bytes(&csil_v.data)));
    csil_entries.push((cbor_text("last"), cbor_bool(csil_v.last)));
    csil_entries.push((cbor_text("offset"), cbor_uint(csil_v.offset)));
    csil_entries.push((cbor_text("snapshot_id"), cbor_text(&csil_v.snapshot_id)));
    csil_entries.push((cbor_text("total_bytes"), cbor_uint(csil_v.total_bytes)));
    if let Some(csil_inner) = &csil_v.whole_digest {
        csil_entries.push((cbor_text("whole_digest"), cbor_bytes(csil_inner)));
    }
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a SnapshotChunk from a decoded CBOR value tree.
fn csil_dec_snapshot_chunk(csil_root: &CsilCborValue) -> Result<SnapshotChunk, CsilCborError> {
    let snapshot_id = {
        let csil_field = cbor_require(csil_root, "snapshot_id")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let offset = {
        let csil_field = cbor_require(csil_root, "offset")?;
        let csil_decode = cbor_as_u64;
        csil_decode(csil_field)?
    };
    let data = {
        let csil_field = cbor_require(csil_root, "data")?;
        let csil_decode = cbor_as_bytes;
        csil_decode(csil_field)?
    };
    let total_bytes = {
        let csil_field = cbor_require(csil_root, "total_bytes")?;
        let csil_decode = cbor_as_u64;
        csil_decode(csil_field)?
    };
    let last = {
        let csil_field = cbor_require(csil_root, "last")?;
        let csil_decode = cbor_as_bool;
        csil_decode(csil_field)?
    };
    let whole_digest = match cbor_map_get(csil_root, "whole_digest") {
        Some(csil_field) => {
            let csil_decode = cbor_as_bytes;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    Ok(SnapshotChunk {
        snapshot_id,
        offset,
        data,
        total_bytes,
        last,
        whole_digest,
    })
}

/// Encode a SnapshotChunk to canonical CSIL CBOR bytes.
pub fn encode_snapshot_chunk(csil_v: &SnapshotChunk) -> Vec<u8> {
    cbor_encode(&csil_enc_snapshot_chunk(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a SnapshotChunk.
pub fn decode_snapshot_chunk(csil_data: &[u8]) -> Result<SnapshotChunk, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_snapshot_chunk(&csil_root)
}

/// Build the canonical CBOR value tree for a SegmentListRequest.
fn csil_enc_segment_list_request(csil_v: &SegmentListRequest) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(1);
    csil_entries.push((cbor_text("tablet"), cbor_text(&csil_v.tablet)));
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a SegmentListRequest from a decoded CBOR value tree.
fn csil_dec_segment_list_request(
    csil_root: &CsilCborValue,
) -> Result<SegmentListRequest, CsilCborError> {
    let tablet = {
        let csil_field = cbor_require(csil_root, "tablet")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    Ok(SegmentListRequest { tablet })
}

/// Encode a SegmentListRequest to canonical CSIL CBOR bytes.
pub fn encode_segment_list_request(csil_v: &SegmentListRequest) -> Vec<u8> {
    cbor_encode(&csil_enc_segment_list_request(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a SegmentListRequest.
pub fn decode_segment_list_request(csil_data: &[u8]) -> Result<SegmentListRequest, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_segment_list_request(&csil_root)
}

/// Build the canonical CBOR value tree for a SegmentSummary.
fn csil_enc_segment_summary(csil_v: &SegmentSummary) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(6);
    csil_entries.push((cbor_text("digest"), cbor_bytes(&csil_v.digest)));
    csil_entries.push((cbor_text("row_count"), cbor_uint(csil_v.row_count)));
    csil_entries.push((cbor_text("segment_id"), cbor_text(&csil_v.segment_id)));
    csil_entries.push((cbor_text("total_bytes"), cbor_uint(csil_v.total_bytes)));
    csil_entries.push((cbor_text("occurred_end"), cbor_int(csil_v.occurred_end)));
    csil_entries.push((cbor_text("occurred_start"), cbor_int(csil_v.occurred_start)));
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a SegmentSummary from a decoded CBOR value tree.
fn csil_dec_segment_summary(csil_root: &CsilCborValue) -> Result<SegmentSummary, CsilCborError> {
    let segment_id = {
        let csil_field = cbor_require(csil_root, "segment_id")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let total_bytes = {
        let csil_field = cbor_require(csil_root, "total_bytes")?;
        let csil_decode = cbor_as_u64;
        csil_decode(csil_field)?
    };
    let digest = {
        let csil_field = cbor_require(csil_root, "digest")?;
        let csil_decode = cbor_as_bytes;
        csil_decode(csil_field)?
    };
    let row_count = {
        let csil_field = cbor_require(csil_root, "row_count")?;
        let csil_decode = cbor_as_u64;
        csil_decode(csil_field)?
    };
    let occurred_start = {
        let csil_field = cbor_require(csil_root, "occurred_start")?;
        let csil_decode = cbor_as_i64;
        csil_decode(csil_field)?
    };
    let occurred_end = {
        let csil_field = cbor_require(csil_root, "occurred_end")?;
        let csil_decode = cbor_as_i64;
        csil_decode(csil_field)?
    };
    Ok(SegmentSummary {
        segment_id,
        total_bytes,
        digest,
        row_count,
        occurred_start,
        occurred_end,
    })
}

/// Encode a SegmentSummary to canonical CSIL CBOR bytes.
pub fn encode_segment_summary(csil_v: &SegmentSummary) -> Vec<u8> {
    cbor_encode(&csil_enc_segment_summary(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a SegmentSummary.
pub fn decode_segment_summary(csil_data: &[u8]) -> Result<SegmentSummary, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_segment_summary(&csil_root)
}

/// Build the canonical CBOR value tree for a SegmentList.
fn csil_enc_segment_list(csil_v: &SegmentList) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(4);
    csil_entries.push((cbor_text("tablet"), cbor_text(&csil_v.tablet)));
    csil_entries.push((
        cbor_text("segments"),
        cbor_enc_array(&csil_v.segments, csil_enc_segment_summary),
    ));
    csil_entries.push((cbor_text("generation"), cbor_uint(csil_v.generation)));
    csil_entries.push((cbor_text("applied_index"), cbor_uint(csil_v.applied_index)));
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a SegmentList from a decoded CBOR value tree.
fn csil_dec_segment_list(csil_root: &CsilCborValue) -> Result<SegmentList, CsilCborError> {
    let tablet = {
        let csil_field = cbor_require(csil_root, "tablet")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let segments = {
        let csil_field = cbor_require(csil_root, "segments")?;
        let csil_decode = |csil_v| cbor_dec_array(csil_v, csil_dec_segment_summary);
        csil_decode(csil_field)?
    };
    let generation = {
        let csil_field = cbor_require(csil_root, "generation")?;
        let csil_decode = cbor_as_u64;
        csil_decode(csil_field)?
    };
    let applied_index = {
        let csil_field = cbor_require(csil_root, "applied_index")?;
        let csil_decode = cbor_as_u64;
        csil_decode(csil_field)?
    };
    Ok(SegmentList {
        tablet,
        segments,
        generation,
        applied_index,
    })
}

/// Encode a SegmentList to canonical CSIL CBOR bytes.
pub fn encode_segment_list(csil_v: &SegmentList) -> Vec<u8> {
    cbor_encode(&csil_enc_segment_list(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a SegmentList.
pub fn decode_segment_list(csil_data: &[u8]) -> Result<SegmentList, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_segment_list(&csil_root)
}

/// Build the canonical CBOR value tree for a SegmentTransferRequest.
fn csil_enc_segment_transfer_request(csil_v: &SegmentTransferRequest) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(4);
    csil_entries.push((cbor_text("offset"), cbor_uint(csil_v.offset)));
    csil_entries.push((cbor_text("tablet"), cbor_text(&csil_v.tablet)));
    csil_entries.push((cbor_text("max_bytes"), cbor_uint(csil_v.max_bytes)));
    csil_entries.push((cbor_text("segment_id"), cbor_text(&csil_v.segment_id)));
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a SegmentTransferRequest from a decoded CBOR value tree.
fn csil_dec_segment_transfer_request(
    csil_root: &CsilCborValue,
) -> Result<SegmentTransferRequest, CsilCborError> {
    let tablet = {
        let csil_field = cbor_require(csil_root, "tablet")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let segment_id = {
        let csil_field = cbor_require(csil_root, "segment_id")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let offset = {
        let csil_field = cbor_require(csil_root, "offset")?;
        let csil_decode = cbor_as_u64;
        csil_decode(csil_field)?
    };
    let max_bytes = {
        let csil_field = cbor_require(csil_root, "max_bytes")?;
        let csil_decode = cbor_as_u64;
        csil_decode(csil_field)?
    };
    Ok(SegmentTransferRequest {
        tablet,
        segment_id,
        offset,
        max_bytes,
    })
}

/// Encode a SegmentTransferRequest to canonical CSIL CBOR bytes.
pub fn encode_segment_transfer_request(csil_v: &SegmentTransferRequest) -> Vec<u8> {
    cbor_encode(&csil_enc_segment_transfer_request(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a SegmentTransferRequest.
pub fn decode_segment_transfer_request(
    csil_data: &[u8],
) -> Result<SegmentTransferRequest, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_segment_transfer_request(&csil_root)
}

/// Build the canonical CBOR value tree for a SegmentTransfer.
fn csil_enc_segment_transfer(csil_v: &SegmentTransfer) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(6);
    csil_entries.push((cbor_text("data"), cbor_bytes(&csil_v.data)));
    csil_entries.push((cbor_text("last"), cbor_bool(csil_v.last)));
    csil_entries.push((cbor_text("offset"), cbor_uint(csil_v.offset)));
    csil_entries.push((cbor_text("segment_id"), cbor_text(&csil_v.segment_id)));
    csil_entries.push((cbor_text("total_bytes"), cbor_uint(csil_v.total_bytes)));
    if let Some(csil_inner) = &csil_v.whole_digest {
        csil_entries.push((cbor_text("whole_digest"), cbor_bytes(csil_inner)));
    }
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a SegmentTransfer from a decoded CBOR value tree.
fn csil_dec_segment_transfer(csil_root: &CsilCborValue) -> Result<SegmentTransfer, CsilCborError> {
    let segment_id = {
        let csil_field = cbor_require(csil_root, "segment_id")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let offset = {
        let csil_field = cbor_require(csil_root, "offset")?;
        let csil_decode = cbor_as_u64;
        csil_decode(csil_field)?
    };
    let data = {
        let csil_field = cbor_require(csil_root, "data")?;
        let csil_decode = cbor_as_bytes;
        csil_decode(csil_field)?
    };
    let total_bytes = {
        let csil_field = cbor_require(csil_root, "total_bytes")?;
        let csil_decode = cbor_as_u64;
        csil_decode(csil_field)?
    };
    let last = {
        let csil_field = cbor_require(csil_root, "last")?;
        let csil_decode = cbor_as_bool;
        csil_decode(csil_field)?
    };
    let whole_digest = match cbor_map_get(csil_root, "whole_digest") {
        Some(csil_field) => {
            let csil_decode = cbor_as_bytes;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    Ok(SegmentTransfer {
        segment_id,
        offset,
        data,
        total_bytes,
        last,
        whole_digest,
    })
}

/// Encode a SegmentTransfer to canonical CSIL CBOR bytes.
pub fn encode_segment_transfer(csil_v: &SegmentTransfer) -> Vec<u8> {
    cbor_encode(&csil_enc_segment_transfer(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a SegmentTransfer.
pub fn decode_segment_transfer(csil_data: &[u8]) -> Result<SegmentTransfer, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_segment_transfer(&csil_root)
}

/// Build the canonical CBOR value tree for a PartialQueryRequest.
fn csil_enc_partial_query_request(csil_v: &PartialQueryRequest) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(15);
    csil_entries.push((cbor_text("kind"), csil_enc_partial_kind(&csil_v.kind)));
    csil_entries.push((cbor_text("basis"), cbor_text(&csil_v.basis)));
    if let Some(csil_inner) = &csil_v.value {
        csil_entries.push((cbor_text("value"), cbor_bytes(csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.column {
        csil_entries.push((cbor_text("column"), cbor_text(csil_inner)));
    }
    csil_entries.push((cbor_text("tablet"), cbor_text(&csil_v.tablet)));
    if let Some(csil_inner) = &csil_v.max_rows {
        csil_entries.push((cbor_text("max_rows"), cbor_uint(*csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.bucket_ms {
        csil_entries.push((cbor_text("bucket_ms"), cbor_int(*csil_inner)));
    }
    csil_entries.push((cbor_text("range_end"), cbor_int(csil_v.range_end)));
    if let Some(csil_inner) = &csil_v.event_name {
        csil_entries.push((cbor_text("event_name"), cbor_text(csil_inner)));
    }
    csil_entries.push((cbor_text("generation"), cbor_uint(csil_v.generation)));
    csil_entries.push((cbor_text("project_id"), cbor_bytes(&csil_v.project_id)));
    csil_entries.push((cbor_text("range_start"), cbor_int(csil_v.range_start)));
    if let Some(csil_inner) = &csil_v.aggregate_plan {
        csil_entries.push((cbor_text("aggregate_plan"), cbor_bytes(csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.max_staleness_ms {
        csil_entries.push((cbor_text("max_staleness_ms"), cbor_int(*csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.require_watermark {
        csil_entries.push((cbor_text("require_watermark"), cbor_uint(*csil_inner)));
    }
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a PartialQueryRequest from a decoded CBOR value tree.
fn csil_dec_partial_query_request(
    csil_root: &CsilCborValue,
) -> Result<PartialQueryRequest, CsilCborError> {
    let tablet = {
        let csil_field = cbor_require(csil_root, "tablet")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let generation = {
        let csil_field = cbor_require(csil_root, "generation")?;
        let csil_decode = cbor_as_u64;
        csil_decode(csil_field)?
    };
    let kind = {
        let csil_field = cbor_require(csil_root, "kind")?;
        let csil_decode = csil_dec_partial_kind;
        csil_decode(csil_field)?
    };
    let project_id = {
        let csil_field = cbor_require(csil_root, "project_id")?;
        let csil_decode = cbor_as_bytes;
        csil_decode(csil_field)?
    };
    let range_start = {
        let csil_field = cbor_require(csil_root, "range_start")?;
        let csil_decode = cbor_as_i64;
        csil_decode(csil_field)?
    };
    let range_end = {
        let csil_field = cbor_require(csil_root, "range_end")?;
        let csil_decode = cbor_as_i64;
        csil_decode(csil_field)?
    };
    let basis = {
        let csil_field = cbor_require(csil_root, "basis")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let bucket_ms = match cbor_map_get(csil_root, "bucket_ms") {
        Some(csil_field) => {
            let csil_decode = cbor_as_i64;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let event_name = match cbor_map_get(csil_root, "event_name") {
        Some(csil_field) => {
            let csil_decode = cbor_as_text;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let column = match cbor_map_get(csil_root, "column") {
        Some(csil_field) => {
            let csil_decode = cbor_as_text;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let value = match cbor_map_get(csil_root, "value") {
        Some(csil_field) => {
            let csil_decode = cbor_as_bytes;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let max_rows = match cbor_map_get(csil_root, "max_rows") {
        Some(csil_field) => {
            let csil_decode = cbor_as_u64;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let aggregate_plan = match cbor_map_get(csil_root, "aggregate_plan") {
        Some(csil_field) => {
            let csil_decode = cbor_as_bytes;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let require_watermark = match cbor_map_get(csil_root, "require_watermark") {
        Some(csil_field) => {
            let csil_decode = cbor_as_u64;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let max_staleness_ms = match cbor_map_get(csil_root, "max_staleness_ms") {
        Some(csil_field) => {
            let csil_decode = cbor_as_i64;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    Ok(PartialQueryRequest {
        tablet,
        generation,
        kind,
        project_id,
        range_start,
        range_end,
        basis,
        bucket_ms,
        event_name,
        column,
        value,
        max_rows,
        aggregate_plan,
        require_watermark,
        max_staleness_ms,
    })
}

/// Encode a PartialQueryRequest to canonical CSIL CBOR bytes.
pub fn encode_partial_query_request(csil_v: &PartialQueryRequest) -> Vec<u8> {
    cbor_encode(&csil_enc_partial_query_request(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a PartialQueryRequest.
pub fn decode_partial_query_request(
    csil_data: &[u8],
) -> Result<PartialQueryRequest, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_partial_query_request(&csil_root)
}

/// Build the canonical CBOR value tree for a TrendBucket.
fn csil_enc_trend_bucket(csil_v: &TrendBucket) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(2);
    csil_entries.push((cbor_text("count"), cbor_uint(csil_v.count)));
    csil_entries.push((cbor_text("bucket_start"), cbor_int(csil_v.bucket_start)));
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a TrendBucket from a decoded CBOR value tree.
fn csil_dec_trend_bucket(csil_root: &CsilCborValue) -> Result<TrendBucket, CsilCborError> {
    let bucket_start = {
        let csil_field = cbor_require(csil_root, "bucket_start")?;
        let csil_decode = cbor_as_i64;
        csil_decode(csil_field)?
    };
    let count = {
        let csil_field = cbor_require(csil_root, "count")?;
        let csil_decode = cbor_as_u64;
        csil_decode(csil_field)?
    };
    Ok(TrendBucket {
        bucket_start,
        count,
    })
}

/// Encode a TrendBucket to canonical CSIL CBOR bytes.
pub fn encode_trend_bucket(csil_v: &TrendBucket) -> Vec<u8> {
    cbor_encode(&csil_enc_trend_bucket(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a TrendBucket.
pub fn decode_trend_bucket(csil_data: &[u8]) -> Result<TrendBucket, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_trend_bucket(&csil_root)
}

/// Build the canonical CBOR value tree for a PartialQueryResponse.
fn csil_enc_partial_query_response(csil_v: &PartialQueryResponse) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(12);
    if let Some(csil_inner) = &csil_v.rows {
        csil_entries.push((
            cbor_text("rows"),
            cbor_enc_array(csil_inner, |csil_elem| cbor_bytes(csil_elem)),
        ));
    }
    csil_entries.push((cbor_text("count"), cbor_uint(csil_v.count)));
    csil_entries.push((cbor_text("tablet"), cbor_text(&csil_v.tablet)));
    if let Some(csil_inner) = &csil_v.buckets {
        csil_entries.push((
            cbor_text("buckets"),
            cbor_enc_array(csil_inner, csil_enc_trend_bucket),
        ));
    }
    if let Some(csil_inner) = &csil_v.missing {
        csil_entries.push((
            cbor_text("missing"),
            cbor_enc_array(csil_inner, csil_enc_missing_range),
        ));
    }
    csil_entries.push((cbor_text("complete"), cbor_bool(csil_v.complete)));
    if let Some(csil_inner) = &csil_v.degraded {
        csil_entries.push((cbor_text("degraded"), cbor_bool(*csil_inner)));
    }
    csil_entries.push((cbor_text("freshness_ms"), cbor_int(csil_v.freshness_ms)));
    csil_entries.push((cbor_text("scanned_bytes"), cbor_uint(csil_v.scanned_bytes)));
    if let Some(csil_inner) = &csil_v.aggregate_state {
        csil_entries.push((cbor_text("aggregate_state"), cbor_bytes(csil_inner)));
    }
    csil_entries.push((
        cbor_text("commit_watermark"),
        cbor_uint(csil_v.commit_watermark),
    ));
    csil_entries.push((
        cbor_text("scanned_segments"),
        cbor_uint(csil_v.scanned_segments),
    ));
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a PartialQueryResponse from a decoded CBOR value tree.
fn csil_dec_partial_query_response(
    csil_root: &CsilCborValue,
) -> Result<PartialQueryResponse, CsilCborError> {
    let tablet = {
        let csil_field = cbor_require(csil_root, "tablet")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let commit_watermark = {
        let csil_field = cbor_require(csil_root, "commit_watermark")?;
        let csil_decode = cbor_as_u64;
        csil_decode(csil_field)?
    };
    let freshness_ms = {
        let csil_field = cbor_require(csil_root, "freshness_ms")?;
        let csil_decode = cbor_as_i64;
        csil_decode(csil_field)?
    };
    let complete = {
        let csil_field = cbor_require(csil_root, "complete")?;
        let csil_decode = cbor_as_bool;
        csil_decode(csil_field)?
    };
    let missing = match cbor_map_get(csil_root, "missing") {
        Some(csil_field) => {
            let csil_decode = |csil_v| cbor_dec_array(csil_v, csil_dec_missing_range);
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let count = {
        let csil_field = cbor_require(csil_root, "count")?;
        let csil_decode = cbor_as_u64;
        csil_decode(csil_field)?
    };
    let buckets = match cbor_map_get(csil_root, "buckets") {
        Some(csil_field) => {
            let csil_decode = |csil_v| cbor_dec_array(csil_v, csil_dec_trend_bucket);
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let rows = match cbor_map_get(csil_root, "rows") {
        Some(csil_field) => {
            let csil_decode = |csil_v| cbor_dec_array(csil_v, cbor_as_bytes);
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let scanned_segments = {
        let csil_field = cbor_require(csil_root, "scanned_segments")?;
        let csil_decode = cbor_as_u64;
        csil_decode(csil_field)?
    };
    let scanned_bytes = {
        let csil_field = cbor_require(csil_root, "scanned_bytes")?;
        let csil_decode = cbor_as_u64;
        csil_decode(csil_field)?
    };
    let aggregate_state = match cbor_map_get(csil_root, "aggregate_state") {
        Some(csil_field) => {
            let csil_decode = cbor_as_bytes;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let degraded = match cbor_map_get(csil_root, "degraded") {
        Some(csil_field) => {
            let csil_decode = cbor_as_bool;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    Ok(PartialQueryResponse {
        tablet,
        commit_watermark,
        freshness_ms,
        complete,
        missing,
        count,
        buckets,
        rows,
        scanned_segments,
        scanned_bytes,
        aggregate_state,
        degraded,
    })
}

/// Encode a PartialQueryResponse to canonical CSIL CBOR bytes.
pub fn encode_partial_query_response(csil_v: &PartialQueryResponse) -> Vec<u8> {
    cbor_encode(&csil_enc_partial_query_response(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a PartialQueryResponse.
pub fn decode_partial_query_response(
    csil_data: &[u8],
) -> Result<PartialQueryResponse, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_partial_query_response(&csil_root)
}

/// Build the canonical CBOR value tree for a MissingRange.
fn csil_enc_missing_range(csil_v: &MissingRange) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(3);
    csil_entries.push((cbor_text("range_end"), cbor_int(csil_v.range_end)));
    csil_entries.push((cbor_text("tablet_id"), cbor_text(&csil_v.tablet_id)));
    csil_entries.push((cbor_text("range_start"), cbor_int(csil_v.range_start)));
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a MissingRange from a decoded CBOR value tree.
fn csil_dec_missing_range(csil_root: &CsilCborValue) -> Result<MissingRange, CsilCborError> {
    let tablet_id = {
        let csil_field = cbor_require(csil_root, "tablet_id")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let range_start = {
        let csil_field = cbor_require(csil_root, "range_start")?;
        let csil_decode = cbor_as_i64;
        csil_decode(csil_field)?
    };
    let range_end = {
        let csil_field = cbor_require(csil_root, "range_end")?;
        let csil_decode = cbor_as_i64;
        csil_decode(csil_field)?
    };
    Ok(MissingRange {
        tablet_id,
        range_start,
        range_end,
    })
}

/// Encode a MissingRange to canonical CSIL CBOR bytes.
pub fn encode_missing_range(csil_v: &MissingRange) -> Vec<u8> {
    cbor_encode(&csil_enc_missing_range(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a MissingRange.
pub fn decode_missing_range(csil_data: &[u8]) -> Result<MissingRange, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_missing_range(&csil_root)
}

/// Build the canonical CBOR value tree for a NodeHealthReport.
fn csil_enc_node_health_report(csil_v: &NodeHealthReport) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(14);
    csil_entries.push((cbor_text("node"), cbor_text(&csil_v.node)));
    csil_entries.push((cbor_text("writable"), cbor_bool(csil_v.writable)));
    csil_entries.push((cbor_text("free_bytes"), cbor_uint(csil_v.free_bytes)));
    if let Some(csil_inner) = &csil_v.self_cause {
        csil_entries.push((cbor_text("self_cause"), csil_enc_slow_cause(csil_inner)));
    }
    csil_entries.push((cbor_text("queue_depth"), cbor_uint(csil_v.queue_depth)));
    csil_entries.push((cbor_text("reported_at"), cbor_int(csil_v.reported_at)));
    csil_entries.push((cbor_text("device_errors"), cbor_uint(csil_v.device_errors)));
    csil_entries.push((
        cbor_text("fsync_latency_us"),
        cbor_uint(csil_v.fsync_latency_us),
    ));
    csil_entries.push((
        cbor_text("append_latency_us"),
        cbor_uint(csil_v.append_latency_us),
    ));
    csil_entries.push((
        cbor_text("peer_round_trip_us"),
        cbor_uint(csil_v.peer_round_trip_us),
    ));
    csil_entries.push((
        cbor_text("memory_reclaim_events"),
        cbor_uint(csil_v.memory_reclaim_events),
    ));
    csil_entries.push((
        cbor_text("device_service_time_us"),
        cbor_uint(csil_v.device_service_time_us),
    ));
    csil_entries.push((
        cbor_text("compaction_backlog_bytes"),
        cbor_uint(csil_v.compaction_backlog_bytes),
    ));
    csil_entries.push((
        cbor_text("accepted_bytes_each_second"),
        cbor_uint(csil_v.accepted_bytes_each_second),
    ));
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a NodeHealthReport from a decoded CBOR value tree.
fn csil_dec_node_health_report(
    csil_root: &CsilCborValue,
) -> Result<NodeHealthReport, CsilCborError> {
    let node = {
        let csil_field = cbor_require(csil_root, "node")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let reported_at = {
        let csil_field = cbor_require(csil_root, "reported_at")?;
        let csil_decode = cbor_as_i64;
        csil_decode(csil_field)?
    };
    let append_latency_us = {
        let csil_field = cbor_require(csil_root, "append_latency_us")?;
        let csil_decode = cbor_as_u64;
        csil_decode(csil_field)?
    };
    let fsync_latency_us = {
        let csil_field = cbor_require(csil_root, "fsync_latency_us")?;
        let csil_decode = cbor_as_u64;
        csil_decode(csil_field)?
    };
    let queue_depth = {
        let csil_field = cbor_require(csil_root, "queue_depth")?;
        let csil_decode = cbor_as_u64;
        csil_decode(csil_field)?
    };
    let accepted_bytes_each_second = {
        let csil_field = cbor_require(csil_root, "accepted_bytes_each_second")?;
        let csil_decode = cbor_as_u64;
        csil_decode(csil_field)?
    };
    let device_errors = {
        let csil_field = cbor_require(csil_root, "device_errors")?;
        let csil_decode = cbor_as_u64;
        csil_decode(csil_field)?
    };
    let device_service_time_us = {
        let csil_field = cbor_require(csil_root, "device_service_time_us")?;
        let csil_decode = cbor_as_u64;
        csil_decode(csil_field)?
    };
    let compaction_backlog_bytes = {
        let csil_field = cbor_require(csil_root, "compaction_backlog_bytes")?;
        let csil_decode = cbor_as_u64;
        csil_decode(csil_field)?
    };
    let memory_reclaim_events = {
        let csil_field = cbor_require(csil_root, "memory_reclaim_events")?;
        let csil_decode = cbor_as_u64;
        csil_decode(csil_field)?
    };
    let peer_round_trip_us = {
        let csil_field = cbor_require(csil_root, "peer_round_trip_us")?;
        let csil_decode = cbor_as_u64;
        csil_decode(csil_field)?
    };
    let self_cause = match cbor_map_get(csil_root, "self_cause") {
        Some(csil_field) => {
            let csil_decode = csil_dec_slow_cause;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let writable = {
        let csil_field = cbor_require(csil_root, "writable")?;
        let csil_decode = cbor_as_bool;
        csil_decode(csil_field)?
    };
    let free_bytes = {
        let csil_field = cbor_require(csil_root, "free_bytes")?;
        let csil_decode = cbor_as_u64;
        csil_decode(csil_field)?
    };
    Ok(NodeHealthReport {
        node,
        reported_at,
        append_latency_us,
        fsync_latency_us,
        queue_depth,
        accepted_bytes_each_second,
        device_errors,
        device_service_time_us,
        compaction_backlog_bytes,
        memory_reclaim_events,
        peer_round_trip_us,
        self_cause,
        writable,
        free_bytes,
    })
}

/// Encode a NodeHealthReport to canonical CSIL CBOR bytes.
pub fn encode_node_health_report(csil_v: &NodeHealthReport) -> Vec<u8> {
    cbor_encode(&csil_enc_node_health_report(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a NodeHealthReport.
pub fn decode_node_health_report(csil_data: &[u8]) -> Result<NodeHealthReport, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_node_health_report(&csil_root)
}

/// Build the canonical CBOR value tree for a NodeCondition.
fn csil_enc_node_condition(csil_v: &NodeCondition) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(11);
    csil_entries.push((cbor_text("node"), cbor_text(&csil_v.node)));
    if let Some(csil_inner) = &csil_v.cause {
        csil_entries.push((cbor_text("cause"), csil_enc_slow_cause(csil_inner)));
    }
    csil_entries.push((cbor_text("since"), cbor_int(csil_v.since)));
    csil_entries.push((cbor_text("state"), csil_enc_node_state(&csil_v.state)));
    csil_entries.push((cbor_text("queue_depth"), cbor_uint(csil_v.queue_depth)));
    if let Some(csil_inner) = &csil_v.action_taken {
        csil_entries.push((cbor_text("action_taken"), cbor_text(csil_inner)));
    }
    csil_entries.push((
        cbor_text("fsync_latency_us"),
        cbor_uint(csil_v.fsync_latency_us),
    ));
    csil_entries.push((
        cbor_text("append_latency_us"),
        cbor_uint(csil_v.append_latency_us),
    ));
    csil_entries.push((
        cbor_text("accepted_bytes_each_second"),
        cbor_uint(csil_v.accepted_bytes_each_second),
    ));
    csil_entries.push((
        cbor_text("group_median_fsync_latency_us"),
        cbor_uint(csil_v.group_median_fsync_latency_us),
    ));
    csil_entries.push((
        cbor_text("group_median_append_latency_us"),
        cbor_uint(csil_v.group_median_append_latency_us),
    ));
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a NodeCondition from a decoded CBOR value tree.
fn csil_dec_node_condition(csil_root: &CsilCborValue) -> Result<NodeCondition, CsilCborError> {
    let node = {
        let csil_field = cbor_require(csil_root, "node")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let state = {
        let csil_field = cbor_require(csil_root, "state")?;
        let csil_decode = csil_dec_node_state;
        csil_decode(csil_field)?
    };
    let cause = match cbor_map_get(csil_root, "cause") {
        Some(csil_field) => {
            let csil_decode = csil_dec_slow_cause;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let append_latency_us = {
        let csil_field = cbor_require(csil_root, "append_latency_us")?;
        let csil_decode = cbor_as_u64;
        csil_decode(csil_field)?
    };
    let group_median_append_latency_us = {
        let csil_field = cbor_require(csil_root, "group_median_append_latency_us")?;
        let csil_decode = cbor_as_u64;
        csil_decode(csil_field)?
    };
    let fsync_latency_us = {
        let csil_field = cbor_require(csil_root, "fsync_latency_us")?;
        let csil_decode = cbor_as_u64;
        csil_decode(csil_field)?
    };
    let group_median_fsync_latency_us = {
        let csil_field = cbor_require(csil_root, "group_median_fsync_latency_us")?;
        let csil_decode = cbor_as_u64;
        csil_decode(csil_field)?
    };
    let queue_depth = {
        let csil_field = cbor_require(csil_root, "queue_depth")?;
        let csil_decode = cbor_as_u64;
        csil_decode(csil_field)?
    };
    let accepted_bytes_each_second = {
        let csil_field = cbor_require(csil_root, "accepted_bytes_each_second")?;
        let csil_decode = cbor_as_u64;
        csil_decode(csil_field)?
    };
    let since = {
        let csil_field = cbor_require(csil_root, "since")?;
        let csil_decode = cbor_as_i64;
        csil_decode(csil_field)?
    };
    let action_taken = match cbor_map_get(csil_root, "action_taken") {
        Some(csil_field) => {
            let csil_decode = cbor_as_text;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    Ok(NodeCondition {
        node,
        state,
        cause,
        append_latency_us,
        group_median_append_latency_us,
        fsync_latency_us,
        group_median_fsync_latency_us,
        queue_depth,
        accepted_bytes_each_second,
        since,
        action_taken,
    })
}

/// Encode a NodeCondition to canonical CSIL CBOR bytes.
pub fn encode_node_condition(csil_v: &NodeCondition) -> Vec<u8> {
    cbor_encode(&csil_enc_node_condition(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a NodeCondition.
pub fn decode_node_condition(csil_data: &[u8]) -> Result<NodeCondition, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_node_condition(&csil_root)
}

/// Build the canonical CBOR value tree for a TabletInfo.
fn csil_enc_tablet_info(csil_v: &TabletInfo) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(16);
    csil_entries.push((cbor_text("cell"), cbor_text(&csil_v.cell)));
    csil_entries.push((cbor_text("epoch"), cbor_uint(csil_v.epoch)));
    csil_entries.push((cbor_text("state"), csil_enc_tablet_state(&csil_v.state)));
    if let Some(csil_inner) = &csil_v.leader {
        csil_entries.push((cbor_text("leader"), cbor_text(csil_inner)));
    }
    csil_entries.push((cbor_text("tablet"), cbor_text(&csil_v.tablet)));
    csil_entries.push((
        cbor_text("members"),
        cbor_enc_array(&csil_v.members, csil_enc_group_member),
    ));
    csil_entries.push((cbor_text("shard_end"), cbor_uint(csil_v.shard_end)));
    csil_entries.push((cbor_text("generation"), cbor_uint(csil_v.generation)));
    csil_entries.push((cbor_text("shard_start"), cbor_uint(csil_v.shard_start)));
    csil_entries.push((cbor_text("stored_bytes"), cbor_uint(csil_v.stored_bytes)));
    csil_entries.push((cbor_text("write_region"), cbor_text(&csil_v.write_region)));
    if let Some(csil_inner) = &csil_v.degraded_since {
        csil_entries.push((cbor_text("degraded_since"), cbor_int(*csil_inner)));
    }
    csil_entries.push((
        cbor_text("receipt_policy"),
        cbor_text(&csil_v.receipt_policy),
    ));
    csil_entries.push((
        cbor_text("commit_watermark"),
        cbor_uint(csil_v.commit_watermark),
    ));
    if let Some(csil_inner) = &csil_v.degraded_range_end {
        csil_entries.push((cbor_text("degraded_range_end"), cbor_int(*csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.degraded_range_start {
        csil_entries.push((cbor_text("degraded_range_start"), cbor_int(*csil_inner)));
    }
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a TabletInfo from a decoded CBOR value tree.
fn csil_dec_tablet_info(csil_root: &CsilCborValue) -> Result<TabletInfo, CsilCborError> {
    let tablet = {
        let csil_field = cbor_require(csil_root, "tablet")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let cell = {
        let csil_field = cbor_require(csil_root, "cell")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let write_region = {
        let csil_field = cbor_require(csil_root, "write_region")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let epoch = {
        let csil_field = cbor_require(csil_root, "epoch")?;
        let csil_decode = cbor_as_u64;
        csil_decode(csil_field)?
    };
    let generation = {
        let csil_field = cbor_require(csil_root, "generation")?;
        let csil_decode = cbor_as_u64;
        csil_decode(csil_field)?
    };
    let state = {
        let csil_field = cbor_require(csil_root, "state")?;
        let csil_decode = csil_dec_tablet_state;
        csil_decode(csil_field)?
    };
    let shard_start = {
        let csil_field = cbor_require(csil_root, "shard_start")?;
        let csil_decode = cbor_as_u64;
        csil_decode(csil_field)?
    };
    let shard_end = {
        let csil_field = cbor_require(csil_root, "shard_end")?;
        let csil_decode = cbor_as_u64;
        csil_decode(csil_field)?
    };
    let members = {
        let csil_field = cbor_require(csil_root, "members")?;
        let csil_decode = |csil_v| cbor_dec_array(csil_v, csil_dec_group_member);
        csil_decode(csil_field)?
    };
    let receipt_policy = {
        let csil_field = cbor_require(csil_root, "receipt_policy")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let leader = match cbor_map_get(csil_root, "leader") {
        Some(csil_field) => {
            let csil_decode = cbor_as_text;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let commit_watermark = {
        let csil_field = cbor_require(csil_root, "commit_watermark")?;
        let csil_decode = cbor_as_u64;
        csil_decode(csil_field)?
    };
    let stored_bytes = {
        let csil_field = cbor_require(csil_root, "stored_bytes")?;
        let csil_decode = cbor_as_u64;
        csil_decode(csil_field)?
    };
    let degraded_since = match cbor_map_get(csil_root, "degraded_since") {
        Some(csil_field) => {
            let csil_decode = cbor_as_i64;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let degraded_range_start = match cbor_map_get(csil_root, "degraded_range_start") {
        Some(csil_field) => {
            let csil_decode = cbor_as_i64;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let degraded_range_end = match cbor_map_get(csil_root, "degraded_range_end") {
        Some(csil_field) => {
            let csil_decode = cbor_as_i64;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    Ok(TabletInfo {
        tablet,
        cell,
        write_region,
        epoch,
        generation,
        state,
        shard_start,
        shard_end,
        members,
        receipt_policy,
        leader,
        commit_watermark,
        stored_bytes,
        degraded_since,
        degraded_range_start,
        degraded_range_end,
    })
}

/// Encode a TabletInfo to canonical CSIL CBOR bytes.
pub fn encode_tablet_info(csil_v: &TabletInfo) -> Vec<u8> {
    cbor_encode(&csil_enc_tablet_info(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a TabletInfo.
pub fn decode_tablet_info(csil_data: &[u8]) -> Result<TabletInfo, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_tablet_info(&csil_root)
}

/// Build the canonical CBOR value tree for a CellInfo.
fn csil_enc_cell_info(csil_v: &CellInfo) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(5);
    csil_entries.push((cbor_text("cell"), cbor_text(&csil_v.cell)));
    csil_entries.push((cbor_text("region"), cbor_text(&csil_v.region)));
    csil_entries.push((cbor_text("generation"), cbor_uint(csil_v.generation)));
    csil_entries.push((
        cbor_text("controllers"),
        cbor_enc_array(&csil_v.controllers, csil_enc_group_member),
    ));
    csil_entries.push((
        cbor_text("quorum_available"),
        cbor_bool(csil_v.quorum_available),
    ));
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a CellInfo from a decoded CBOR value tree.
fn csil_dec_cell_info(csil_root: &CsilCborValue) -> Result<CellInfo, CsilCborError> {
    let cell = {
        let csil_field = cbor_require(csil_root, "cell")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let region = {
        let csil_field = cbor_require(csil_root, "region")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let controllers = {
        let csil_field = cbor_require(csil_root, "controllers")?;
        let csil_decode = |csil_v| cbor_dec_array(csil_v, csil_dec_group_member);
        csil_decode(csil_field)?
    };
    let generation = {
        let csil_field = cbor_require(csil_root, "generation")?;
        let csil_decode = cbor_as_u64;
        csil_decode(csil_field)?
    };
    let quorum_available = {
        let csil_field = cbor_require(csil_root, "quorum_available")?;
        let csil_decode = cbor_as_bool;
        csil_decode(csil_field)?
    };
    Ok(CellInfo {
        cell,
        region,
        controllers,
        generation,
        quorum_available,
    })
}

/// Encode a CellInfo to canonical CSIL CBOR bytes.
pub fn encode_cell_info(csil_v: &CellInfo) -> Vec<u8> {
    cbor_encode(&csil_enc_cell_info(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a CellInfo.
pub fn decode_cell_info(csil_data: &[u8]) -> Result<CellInfo, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_cell_info(&csil_root)
}

/// Build the canonical CBOR value tree for a ProjectPlacement.
fn csil_enc_project_placement(csil_v: &ProjectPlacement) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(4);
    csil_entries.push((
        cbor_text("cells"),
        cbor_enc_array(&csil_v.cells, |csil_elem| cbor_text(csil_elem)),
    ));
    if let Some(csil_inner) = &csil_v.moving_to {
        csil_entries.push((cbor_text("moving_to"), cbor_text(csil_inner)));
    }
    csil_entries.push((cbor_text("project_id"), cbor_bytes(&csil_v.project_id)));
    csil_entries.push((
        cbor_text("policy_generation"),
        cbor_uint(csil_v.policy_generation),
    ));
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a ProjectPlacement from a decoded CBOR value tree.
fn csil_dec_project_placement(
    csil_root: &CsilCborValue,
) -> Result<ProjectPlacement, CsilCborError> {
    let project_id = {
        let csil_field = cbor_require(csil_root, "project_id")?;
        let csil_decode = cbor_as_bytes;
        csil_decode(csil_field)?
    };
    let cells = {
        let csil_field = cbor_require(csil_root, "cells")?;
        let csil_decode = |csil_v| cbor_dec_array(csil_v, cbor_as_text);
        csil_decode(csil_field)?
    };
    let moving_to = match cbor_map_get(csil_root, "moving_to") {
        Some(csil_field) => {
            let csil_decode = cbor_as_text;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let policy_generation = {
        let csil_field = cbor_require(csil_root, "policy_generation")?;
        let csil_decode = cbor_as_u64;
        csil_decode(csil_field)?
    };
    Ok(ProjectPlacement {
        project_id,
        cells,
        moving_to,
        policy_generation,
    })
}

/// Encode a ProjectPlacement to canonical CSIL CBOR bytes.
pub fn encode_project_placement(csil_v: &ProjectPlacement) -> Vec<u8> {
    cbor_encode(&csil_enc_project_placement(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a ProjectPlacement.
pub fn decode_project_placement(csil_data: &[u8]) -> Result<ProjectPlacement, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_project_placement(&csil_root)
}

/// Build the canonical CBOR value tree for a DescribeTopologyRequest.
fn csil_enc_describe_topology_request(csil_v: &DescribeTopologyRequest) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(1);
    if let Some(csil_inner) = &csil_v.cell {
        csil_entries.push((cbor_text("cell"), cbor_text(csil_inner)));
    }
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a DescribeTopologyRequest from a decoded CBOR value tree.
fn csil_dec_describe_topology_request(
    csil_root: &CsilCborValue,
) -> Result<DescribeTopologyRequest, CsilCborError> {
    let cell = match cbor_map_get(csil_root, "cell") {
        Some(csil_field) => {
            let csil_decode = cbor_as_text;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    Ok(DescribeTopologyRequest { cell })
}

/// Encode a DescribeTopologyRequest to canonical CSIL CBOR bytes.
pub fn encode_describe_topology_request(csil_v: &DescribeTopologyRequest) -> Vec<u8> {
    cbor_encode(&csil_enc_describe_topology_request(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a DescribeTopologyRequest.
pub fn decode_describe_topology_request(
    csil_data: &[u8],
) -> Result<DescribeTopologyRequest, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_describe_topology_request(&csil_root)
}

/// Build the canonical CBOR value tree for a TopologyResponse.
fn csil_enc_topology_response(csil_v: &TopologyResponse) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(5);
    csil_entries.push((
        cbor_text("cells"),
        cbor_enc_array(&csil_v.cells, csil_enc_cell_info),
    ));
    csil_entries.push((
        cbor_text("nodes"),
        cbor_enc_array(&csil_v.nodes, csil_enc_node_condition),
    ));
    csil_entries.push((
        cbor_text("tablets"),
        cbor_enc_array(&csil_v.tablets, csil_enc_tablet_info),
    ));
    csil_entries.push((cbor_text("generation"), cbor_uint(csil_v.generation)));
    csil_entries.push((
        cbor_text("directory_available"),
        cbor_bool(csil_v.directory_available),
    ));
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a TopologyResponse from a decoded CBOR value tree.
fn csil_dec_topology_response(
    csil_root: &CsilCborValue,
) -> Result<TopologyResponse, CsilCborError> {
    let cells = {
        let csil_field = cbor_require(csil_root, "cells")?;
        let csil_decode = |csil_v| cbor_dec_array(csil_v, csil_dec_cell_info);
        csil_decode(csil_field)?
    };
    let tablets = {
        let csil_field = cbor_require(csil_root, "tablets")?;
        let csil_decode = |csil_v| cbor_dec_array(csil_v, csil_dec_tablet_info);
        csil_decode(csil_field)?
    };
    let nodes = {
        let csil_field = cbor_require(csil_root, "nodes")?;
        let csil_decode = |csil_v| cbor_dec_array(csil_v, csil_dec_node_condition);
        csil_decode(csil_field)?
    };
    let generation = {
        let csil_field = cbor_require(csil_root, "generation")?;
        let csil_decode = cbor_as_u64;
        csil_decode(csil_field)?
    };
    let directory_available = {
        let csil_field = cbor_require(csil_root, "directory_available")?;
        let csil_decode = cbor_as_bool;
        csil_decode(csil_field)?
    };
    Ok(TopologyResponse {
        cells,
        tablets,
        nodes,
        generation,
        directory_available,
    })
}

/// Encode a TopologyResponse to canonical CSIL CBOR bytes.
pub fn encode_topology_response(csil_v: &TopologyResponse) -> Vec<u8> {
    cbor_encode(&csil_enc_topology_response(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a TopologyResponse.
pub fn decode_topology_response(csil_data: &[u8]) -> Result<TopologyResponse, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_topology_response(&csil_root)
}

/// Build the canonical CBOR value tree for a SplitTabletRequest.
fn csil_enc_split_tablet_request(csil_v: &SplitTabletRequest) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(2);
    csil_entries.push((cbor_text("tablet"), cbor_text(&csil_v.tablet)));
    if let Some(csil_inner) = &csil_v.at_shard {
        csil_entries.push((cbor_text("at_shard"), cbor_uint(*csil_inner)));
    }
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a SplitTabletRequest from a decoded CBOR value tree.
fn csil_dec_split_tablet_request(
    csil_root: &CsilCborValue,
) -> Result<SplitTabletRequest, CsilCborError> {
    let tablet = {
        let csil_field = cbor_require(csil_root, "tablet")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let at_shard = match cbor_map_get(csil_root, "at_shard") {
        Some(csil_field) => {
            let csil_decode = cbor_as_u64;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    Ok(SplitTabletRequest { tablet, at_shard })
}

/// Encode a SplitTabletRequest to canonical CSIL CBOR bytes.
pub fn encode_split_tablet_request(csil_v: &SplitTabletRequest) -> Vec<u8> {
    cbor_encode(&csil_enc_split_tablet_request(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a SplitTabletRequest.
pub fn decode_split_tablet_request(csil_data: &[u8]) -> Result<SplitTabletRequest, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_split_tablet_request(&csil_root)
}

/// Build the canonical CBOR value tree for a MergeTabletsRequest.
fn csil_enc_merge_tablets_request(csil_v: &MergeTabletsRequest) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(2);
    csil_entries.push((cbor_text("left"), cbor_text(&csil_v.left)));
    csil_entries.push((cbor_text("right"), cbor_text(&csil_v.right)));
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a MergeTabletsRequest from a decoded CBOR value tree.
fn csil_dec_merge_tablets_request(
    csil_root: &CsilCborValue,
) -> Result<MergeTabletsRequest, CsilCborError> {
    let left = {
        let csil_field = cbor_require(csil_root, "left")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let right = {
        let csil_field = cbor_require(csil_root, "right")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    Ok(MergeTabletsRequest { left, right })
}

/// Encode a MergeTabletsRequest to canonical CSIL CBOR bytes.
pub fn encode_merge_tablets_request(csil_v: &MergeTabletsRequest) -> Vec<u8> {
    cbor_encode(&csil_enc_merge_tablets_request(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a MergeTabletsRequest.
pub fn decode_merge_tablets_request(
    csil_data: &[u8],
) -> Result<MergeTabletsRequest, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_merge_tablets_request(&csil_root)
}

/// Build the canonical CBOR value tree for a MoveTabletRequest.
fn csil_enc_move_tablet_request(csil_v: &MoveTabletRequest) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(3);
    csil_entries.push((cbor_text("onto"), cbor_text(&csil_v.onto)));
    csil_entries.push((cbor_text("tablet"), cbor_text(&csil_v.tablet)));
    csil_entries.push((cbor_text("away_from"), cbor_text(&csil_v.away_from)));
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a MoveTabletRequest from a decoded CBOR value tree.
fn csil_dec_move_tablet_request(
    csil_root: &CsilCborValue,
) -> Result<MoveTabletRequest, CsilCborError> {
    let tablet = {
        let csil_field = cbor_require(csil_root, "tablet")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let away_from = {
        let csil_field = cbor_require(csil_root, "away_from")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let onto = {
        let csil_field = cbor_require(csil_root, "onto")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    Ok(MoveTabletRequest {
        tablet,
        away_from,
        onto,
    })
}

/// Encode a MoveTabletRequest to canonical CSIL CBOR bytes.
pub fn encode_move_tablet_request(csil_v: &MoveTabletRequest) -> Vec<u8> {
    cbor_encode(&csil_enc_move_tablet_request(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a MoveTabletRequest.
pub fn decode_move_tablet_request(csil_data: &[u8]) -> Result<MoveTabletRequest, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_move_tablet_request(&csil_root)
}

/// Build the canonical CBOR value tree for a ChangeReplicaRequest.
fn csil_enc_change_replica_request(csil_v: &ChangeReplicaRequest) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(6);
    csil_entries.push((cbor_text("node"), cbor_text(&csil_v.node)));
    csil_entries.push((cbor_text("role"), csil_enc_member_role(&csil_v.role)));
    if let Some(csil_inner) = &csil_v.domain {
        csil_entries.push((cbor_text("domain"), cbor_text(csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.region {
        csil_entries.push((cbor_text("region"), cbor_text(csil_inner)));
    }
    csil_entries.push((cbor_text("tablet"), cbor_text(&csil_v.tablet)));
    csil_entries.push((cbor_text("address"), cbor_text(&csil_v.address)));
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a ChangeReplicaRequest from a decoded CBOR value tree.
fn csil_dec_change_replica_request(
    csil_root: &CsilCborValue,
) -> Result<ChangeReplicaRequest, CsilCborError> {
    let tablet = {
        let csil_field = cbor_require(csil_root, "tablet")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let node = {
        let csil_field = cbor_require(csil_root, "node")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let address = {
        let csil_field = cbor_require(csil_root, "address")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let role = {
        let csil_field = cbor_require(csil_root, "role")?;
        let csil_decode = csil_dec_member_role;
        csil_decode(csil_field)?
    };
    let region = match cbor_map_get(csil_root, "region") {
        Some(csil_field) => {
            let csil_decode = cbor_as_text;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let domain = match cbor_map_get(csil_root, "domain") {
        Some(csil_field) => {
            let csil_decode = cbor_as_text;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    Ok(ChangeReplicaRequest {
        tablet,
        node,
        address,
        role,
        region,
        domain,
    })
}

/// Encode a ChangeReplicaRequest to canonical CSIL CBOR bytes.
pub fn encode_change_replica_request(csil_v: &ChangeReplicaRequest) -> Vec<u8> {
    cbor_encode(&csil_enc_change_replica_request(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a ChangeReplicaRequest.
pub fn decode_change_replica_request(
    csil_data: &[u8],
) -> Result<ChangeReplicaRequest, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_change_replica_request(&csil_root)
}

/// Build the canonical CBOR value tree for a RemoveReplicaRequest.
fn csil_enc_remove_replica_request(csil_v: &RemoveReplicaRequest) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(2);
    csil_entries.push((cbor_text("node"), cbor_text(&csil_v.node)));
    csil_entries.push((cbor_text("tablet"), cbor_text(&csil_v.tablet)));
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a RemoveReplicaRequest from a decoded CBOR value tree.
fn csil_dec_remove_replica_request(
    csil_root: &CsilCborValue,
) -> Result<RemoveReplicaRequest, CsilCborError> {
    let tablet = {
        let csil_field = cbor_require(csil_root, "tablet")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let node = {
        let csil_field = cbor_require(csil_root, "node")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    Ok(RemoveReplicaRequest { tablet, node })
}

/// Encode a RemoveReplicaRequest to canonical CSIL CBOR bytes.
pub fn encode_remove_replica_request(csil_v: &RemoveReplicaRequest) -> Vec<u8> {
    cbor_encode(&csil_enc_remove_replica_request(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a RemoveReplicaRequest.
pub fn decode_remove_replica_request(
    csil_data: &[u8],
) -> Result<RemoveReplicaRequest, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_remove_replica_request(&csil_root)
}

/// Build the canonical CBOR value tree for a SetReceiptPolicyRequest.
fn csil_enc_set_receipt_policy_request(csil_v: &SetReceiptPolicyRequest) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(2);
    csil_entries.push((cbor_text("policy"), cbor_text(&csil_v.policy)));
    csil_entries.push((cbor_text("tablet"), cbor_text(&csil_v.tablet)));
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a SetReceiptPolicyRequest from a decoded CBOR value tree.
fn csil_dec_set_receipt_policy_request(
    csil_root: &CsilCborValue,
) -> Result<SetReceiptPolicyRequest, CsilCborError> {
    let tablet = {
        let csil_field = cbor_require(csil_root, "tablet")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let policy = {
        let csil_field = cbor_require(csil_root, "policy")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    Ok(SetReceiptPolicyRequest { tablet, policy })
}

/// Encode a SetReceiptPolicyRequest to canonical CSIL CBOR bytes.
pub fn encode_set_receipt_policy_request(csil_v: &SetReceiptPolicyRequest) -> Vec<u8> {
    cbor_encode(&csil_enc_set_receipt_policy_request(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a SetReceiptPolicyRequest.
pub fn decode_set_receipt_policy_request(
    csil_data: &[u8],
) -> Result<SetReceiptPolicyRequest, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_set_receipt_policy_request(&csil_root)
}

/// Build the canonical CBOR value tree for a FailOverRegionRequest.
fn csil_enc_fail_over_region_request(csil_v: &FailOverRegionRequest) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(3);
    csil_entries.push((cbor_text("tablet"), cbor_text(&csil_v.tablet)));
    csil_entries.push((cbor_text("onto_region"), cbor_text(&csil_v.onto_region)));
    if let Some(csil_inner) = &csil_v.accept_data_loss {
        csil_entries.push((cbor_text("accept_data_loss"), cbor_bool(*csil_inner)));
    }
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a FailOverRegionRequest from a decoded CBOR value tree.
fn csil_dec_fail_over_region_request(
    csil_root: &CsilCborValue,
) -> Result<FailOverRegionRequest, CsilCborError> {
    let tablet = {
        let csil_field = cbor_require(csil_root, "tablet")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let onto_region = {
        let csil_field = cbor_require(csil_root, "onto_region")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let accept_data_loss = match cbor_map_get(csil_root, "accept_data_loss") {
        Some(csil_field) => {
            let csil_decode = cbor_as_bool;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    Ok(FailOverRegionRequest {
        tablet,
        onto_region,
        accept_data_loss,
    })
}

/// Encode a FailOverRegionRequest to canonical CSIL CBOR bytes.
pub fn encode_fail_over_region_request(csil_v: &FailOverRegionRequest) -> Vec<u8> {
    cbor_encode(&csil_enc_fail_over_region_request(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a FailOverRegionRequest.
pub fn decode_fail_over_region_request(
    csil_data: &[u8],
) -> Result<FailOverRegionRequest, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_fail_over_region_request(&csil_root)
}

/// Build the canonical CBOR value tree for a UnsafeRecoverRequest.
fn csil_enc_unsafe_recover_request(csil_v: &UnsafeRecoverRequest) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(4);
    csil_entries.push((cbor_text("reason"), cbor_text(&csil_v.reason)));
    csil_entries.push((cbor_text("tablet"), cbor_text(&csil_v.tablet)));
    csil_entries.push((cbor_text("survivor"), cbor_text(&csil_v.survivor)));
    csil_entries.push((
        cbor_text("confirm_tablet"),
        cbor_text(&csil_v.confirm_tablet),
    ));
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a UnsafeRecoverRequest from a decoded CBOR value tree.
fn csil_dec_unsafe_recover_request(
    csil_root: &CsilCborValue,
) -> Result<UnsafeRecoverRequest, CsilCborError> {
    let tablet = {
        let csil_field = cbor_require(csil_root, "tablet")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let confirm_tablet = {
        let csil_field = cbor_require(csil_root, "confirm_tablet")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let survivor = {
        let csil_field = cbor_require(csil_root, "survivor")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let reason = {
        let csil_field = cbor_require(csil_root, "reason")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    Ok(UnsafeRecoverRequest {
        tablet,
        confirm_tablet,
        survivor,
        reason,
    })
}

/// Encode a UnsafeRecoverRequest to canonical CSIL CBOR bytes.
pub fn encode_unsafe_recover_request(csil_v: &UnsafeRecoverRequest) -> Vec<u8> {
    cbor_encode(&csil_enc_unsafe_recover_request(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a UnsafeRecoverRequest.
pub fn decode_unsafe_recover_request(
    csil_data: &[u8],
) -> Result<UnsafeRecoverRequest, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_unsafe_recover_request(&csil_root)
}

/// Build the canonical CBOR value tree for a UnsafeRecoverResponse.
fn csil_enc_unsafe_recover_response(csil_v: &UnsafeRecoverResponse) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(6);
    csil_entries.push((cbor_text("tablet"), cbor_text(&csil_v.tablet)));
    csil_entries.push((cbor_text("audit_id"), cbor_text(&csil_v.audit_id)));
    csil_entries.push((cbor_text("performed_at"), cbor_int(csil_v.performed_at)));
    csil_entries.push((
        cbor_text("degraded_range_end"),
        cbor_int(csil_v.degraded_range_end),
    ));
    csil_entries.push((
        cbor_text("survivor_watermark"),
        cbor_uint(csil_v.survivor_watermark),
    ));
    csil_entries.push((
        cbor_text("degraded_range_start"),
        cbor_int(csil_v.degraded_range_start),
    ));
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a UnsafeRecoverResponse from a decoded CBOR value tree.
fn csil_dec_unsafe_recover_response(
    csil_root: &CsilCborValue,
) -> Result<UnsafeRecoverResponse, CsilCborError> {
    let tablet = {
        let csil_field = cbor_require(csil_root, "tablet")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let audit_id = {
        let csil_field = cbor_require(csil_root, "audit_id")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let degraded_range_start = {
        let csil_field = cbor_require(csil_root, "degraded_range_start")?;
        let csil_decode = cbor_as_i64;
        csil_decode(csil_field)?
    };
    let degraded_range_end = {
        let csil_field = cbor_require(csil_root, "degraded_range_end")?;
        let csil_decode = cbor_as_i64;
        csil_decode(csil_field)?
    };
    let survivor_watermark = {
        let csil_field = cbor_require(csil_root, "survivor_watermark")?;
        let csil_decode = cbor_as_u64;
        csil_decode(csil_field)?
    };
    let performed_at = {
        let csil_field = cbor_require(csil_root, "performed_at")?;
        let csil_decode = cbor_as_i64;
        csil_decode(csil_field)?
    };
    Ok(UnsafeRecoverResponse {
        tablet,
        audit_id,
        degraded_range_start,
        degraded_range_end,
        survivor_watermark,
        performed_at,
    })
}

/// Encode a UnsafeRecoverResponse to canonical CSIL CBOR bytes.
pub fn encode_unsafe_recover_response(csil_v: &UnsafeRecoverResponse) -> Vec<u8> {
    cbor_encode(&csil_enc_unsafe_recover_response(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a UnsafeRecoverResponse.
pub fn decode_unsafe_recover_response(
    csil_data: &[u8],
) -> Result<UnsafeRecoverResponse, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_unsafe_recover_response(&csil_root)
}

/// Build the canonical CBOR value tree for a ClearDegradedRequest.
fn csil_enc_clear_degraded_request(csil_v: &ClearDegradedRequest) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(3);
    csil_entries.push((cbor_text("reason"), cbor_text(&csil_v.reason)));
    csil_entries.push((cbor_text("tablet"), cbor_text(&csil_v.tablet)));
    csil_entries.push((cbor_text("accepted_by"), cbor_text(&csil_v.accepted_by)));
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a ClearDegradedRequest from a decoded CBOR value tree.
fn csil_dec_clear_degraded_request(
    csil_root: &CsilCborValue,
) -> Result<ClearDegradedRequest, CsilCborError> {
    let tablet = {
        let csil_field = cbor_require(csil_root, "tablet")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let accepted_by = {
        let csil_field = cbor_require(csil_root, "accepted_by")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let reason = {
        let csil_field = cbor_require(csil_root, "reason")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    Ok(ClearDegradedRequest {
        tablet,
        accepted_by,
        reason,
    })
}

/// Encode a ClearDegradedRequest to canonical CSIL CBOR bytes.
pub fn encode_clear_degraded_request(csil_v: &ClearDegradedRequest) -> Vec<u8> {
    cbor_encode(&csil_enc_clear_degraded_request(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a ClearDegradedRequest.
pub fn decode_clear_degraded_request(
    csil_data: &[u8],
) -> Result<ClearDegradedRequest, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_clear_degraded_request(&csil_root)
}

/// Build the canonical CBOR value tree for a ClusterSnapshotRequest.
fn csil_enc_cluster_snapshot_request(csil_v: &ClusterSnapshotRequest) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(1);
    if let Some(csil_inner) = &csil_v.tablet {
        csil_entries.push((cbor_text("tablet"), cbor_text(csil_inner)));
    }
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a ClusterSnapshotRequest from a decoded CBOR value tree.
fn csil_dec_cluster_snapshot_request(
    csil_root: &CsilCborValue,
) -> Result<ClusterSnapshotRequest, CsilCborError> {
    let tablet = match cbor_map_get(csil_root, "tablet") {
        Some(csil_field) => {
            let csil_decode = cbor_as_text;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    Ok(ClusterSnapshotRequest { tablet })
}

/// Encode a ClusterSnapshotRequest to canonical CSIL CBOR bytes.
pub fn encode_cluster_snapshot_request(csil_v: &ClusterSnapshotRequest) -> Vec<u8> {
    cbor_encode(&csil_enc_cluster_snapshot_request(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a ClusterSnapshotRequest.
pub fn decode_cluster_snapshot_request(
    csil_data: &[u8],
) -> Result<ClusterSnapshotRequest, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_cluster_snapshot_request(&csil_root)
}

/// Build the canonical CBOR value tree for a ClusterSnapshotResponse.
fn csil_enc_cluster_snapshot_response(csil_v: &ClusterSnapshotResponse) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(5);
    csil_entries.push((
        cbor_text("tablets"),
        cbor_enc_array(&csil_v.tablets, |csil_elem| cbor_text(csil_elem)),
    ));
    csil_entries.push((cbor_text("taken_at"), cbor_int(csil_v.taken_at)));
    csil_entries.push((cbor_text("snapshot_id"), cbor_text(&csil_v.snapshot_id)));
    csil_entries.push((cbor_text("total_bytes"), cbor_uint(csil_v.total_bytes)));
    csil_entries.push((cbor_text("whole_digest"), cbor_bytes(&csil_v.whole_digest)));
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a ClusterSnapshotResponse from a decoded CBOR value tree.
fn csil_dec_cluster_snapshot_response(
    csil_root: &CsilCborValue,
) -> Result<ClusterSnapshotResponse, CsilCborError> {
    let snapshot_id = {
        let csil_field = cbor_require(csil_root, "snapshot_id")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let tablets = {
        let csil_field = cbor_require(csil_root, "tablets")?;
        let csil_decode = |csil_v| cbor_dec_array(csil_v, cbor_as_text);
        csil_decode(csil_field)?
    };
    let taken_at = {
        let csil_field = cbor_require(csil_root, "taken_at")?;
        let csil_decode = cbor_as_i64;
        csil_decode(csil_field)?
    };
    let total_bytes = {
        let csil_field = cbor_require(csil_root, "total_bytes")?;
        let csil_decode = cbor_as_u64;
        csil_decode(csil_field)?
    };
    let whole_digest = {
        let csil_field = cbor_require(csil_root, "whole_digest")?;
        let csil_decode = cbor_as_bytes;
        csil_decode(csil_field)?
    };
    Ok(ClusterSnapshotResponse {
        snapshot_id,
        tablets,
        taken_at,
        total_bytes,
        whole_digest,
    })
}

/// Encode a ClusterSnapshotResponse to canonical CSIL CBOR bytes.
pub fn encode_cluster_snapshot_response(csil_v: &ClusterSnapshotResponse) -> Vec<u8> {
    cbor_encode(&csil_enc_cluster_snapshot_response(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a ClusterSnapshotResponse.
pub fn decode_cluster_snapshot_response(
    csil_data: &[u8],
) -> Result<ClusterSnapshotResponse, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_cluster_snapshot_response(&csil_root)
}

/// Build the canonical CBOR value tree for a RestoreRequest.
fn csil_enc_restore_request(csil_v: &RestoreRequest) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(2);
    if let Some(csil_inner) = &csil_v.onto_tablet {
        csil_entries.push((cbor_text("onto_tablet"), cbor_text(csil_inner)));
    }
    csil_entries.push((cbor_text("snapshot_id"), cbor_text(&csil_v.snapshot_id)));
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a RestoreRequest from a decoded CBOR value tree.
fn csil_dec_restore_request(csil_root: &CsilCborValue) -> Result<RestoreRequest, CsilCborError> {
    let snapshot_id = {
        let csil_field = cbor_require(csil_root, "snapshot_id")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let onto_tablet = match cbor_map_get(csil_root, "onto_tablet") {
        Some(csil_field) => {
            let csil_decode = cbor_as_text;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    Ok(RestoreRequest {
        snapshot_id,
        onto_tablet,
    })
}

/// Encode a RestoreRequest to canonical CSIL CBOR bytes.
pub fn encode_restore_request(csil_v: &RestoreRequest) -> Vec<u8> {
    cbor_encode(&csil_enc_restore_request(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a RestoreRequest.
pub fn decode_restore_request(csil_data: &[u8]) -> Result<RestoreRequest, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_restore_request(&csil_root)
}

/// Build the canonical CBOR value tree for a BootstrapGroupRequest.
fn csil_enc_bootstrap_group_request(csil_v: &BootstrapGroupRequest) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(2);
    csil_entries.push((cbor_text("group"), csil_enc_group_ref(&csil_v.group)));
    csil_entries.push((
        cbor_text("members"),
        cbor_enc_array(&csil_v.members, csil_enc_group_member),
    ));
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a BootstrapGroupRequest from a decoded CBOR value tree.
fn csil_dec_bootstrap_group_request(
    csil_root: &CsilCborValue,
) -> Result<BootstrapGroupRequest, CsilCborError> {
    let group = {
        let csil_field = cbor_require(csil_root, "group")?;
        let csil_decode = csil_dec_group_ref;
        csil_decode(csil_field)?
    };
    let members = {
        let csil_field = cbor_require(csil_root, "members")?;
        let csil_decode = |csil_v| cbor_dec_array(csil_v, csil_dec_group_member);
        csil_decode(csil_field)?
    };
    Ok(BootstrapGroupRequest { group, members })
}

/// Encode a BootstrapGroupRequest to canonical CSIL CBOR bytes.
pub fn encode_bootstrap_group_request(csil_v: &BootstrapGroupRequest) -> Vec<u8> {
    cbor_encode(&csil_enc_bootstrap_group_request(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a BootstrapGroupRequest.
pub fn decode_bootstrap_group_request(
    csil_data: &[u8],
) -> Result<BootstrapGroupRequest, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_bootstrap_group_request(&csil_root)
}

/// Build the canonical CBOR value tree for a ClusterAck.
fn csil_enc_cluster_ack(csil_v: &ClusterAck) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(3);
    if let Some(csil_inner) = &csil_v.message {
        csil_entries.push((cbor_text("message"), cbor_text(csil_inner)));
    }
    csil_entries.push((cbor_text("accepted"), cbor_bool(csil_v.accepted)));
    csil_entries.push((cbor_text("generation"), cbor_uint(csil_v.generation)));
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a ClusterAck from a decoded CBOR value tree.
fn csil_dec_cluster_ack(csil_root: &CsilCborValue) -> Result<ClusterAck, CsilCborError> {
    let accepted = {
        let csil_field = cbor_require(csil_root, "accepted")?;
        let csil_decode = cbor_as_bool;
        csil_decode(csil_field)?
    };
    let generation = {
        let csil_field = cbor_require(csil_root, "generation")?;
        let csil_decode = cbor_as_u64;
        csil_decode(csil_field)?
    };
    let message = match cbor_map_get(csil_root, "message") {
        Some(csil_field) => {
            let csil_decode = cbor_as_text;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    Ok(ClusterAck {
        accepted,
        generation,
        message,
    })
}

/// Encode a ClusterAck to canonical CSIL CBOR bytes.
pub fn encode_cluster_ack(csil_v: &ClusterAck) -> Vec<u8> {
    cbor_encode(&csil_enc_cluster_ack(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a ClusterAck.
pub fn decode_cluster_ack(csil_data: &[u8]) -> Result<ClusterAck, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_cluster_ack(&csil_root)
}

/// Build the canonical CBOR value tree for a AssignProjectRequest.
fn csil_enc_assign_project_request(csil_v: &AssignProjectRequest) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(2);
    csil_entries.push((
        cbor_text("cells"),
        cbor_enc_array(&csil_v.cells, |csil_elem| cbor_text(csil_elem)),
    ));
    csil_entries.push((cbor_text("project_id"), cbor_bytes(&csil_v.project_id)));
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a AssignProjectRequest from a decoded CBOR value tree.
fn csil_dec_assign_project_request(
    csil_root: &CsilCborValue,
) -> Result<AssignProjectRequest, CsilCborError> {
    let project_id = {
        let csil_field = cbor_require(csil_root, "project_id")?;
        let csil_decode = cbor_as_bytes;
        csil_decode(csil_field)?
    };
    let cells = {
        let csil_field = cbor_require(csil_root, "cells")?;
        let csil_decode = |csil_v| cbor_dec_array(csil_v, cbor_as_text);
        csil_decode(csil_field)?
    };
    Ok(AssignProjectRequest { project_id, cells })
}

/// Encode a AssignProjectRequest to canonical CSIL CBOR bytes.
pub fn encode_assign_project_request(csil_v: &AssignProjectRequest) -> Vec<u8> {
    cbor_encode(&csil_enc_assign_project_request(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a AssignProjectRequest.
pub fn decode_assign_project_request(
    csil_data: &[u8],
) -> Result<AssignProjectRequest, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_assign_project_request(&csil_root)
}

/// Build the canonical CBOR value tree for a DirectoryRequest.
fn csil_enc_directory_request(csil_v: &DirectoryRequest) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(1);
    if let Some(csil_inner) = &csil_v.project_id {
        csil_entries.push((cbor_text("project_id"), cbor_bytes(csil_inner)));
    }
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a DirectoryRequest from a decoded CBOR value tree.
fn csil_dec_directory_request(
    csil_root: &CsilCborValue,
) -> Result<DirectoryRequest, CsilCborError> {
    let project_id = match cbor_map_get(csil_root, "project_id") {
        Some(csil_field) => {
            let csil_decode = cbor_as_bytes;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    Ok(DirectoryRequest { project_id })
}

/// Encode a DirectoryRequest to canonical CSIL CBOR bytes.
pub fn encode_directory_request(csil_v: &DirectoryRequest) -> Vec<u8> {
    cbor_encode(&csil_enc_directory_request(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a DirectoryRequest.
pub fn decode_directory_request(csil_data: &[u8]) -> Result<DirectoryRequest, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_directory_request(&csil_root)
}

/// Build the canonical CBOR value tree for a DirectoryResponse.
fn csil_enc_directory_response(csil_v: &DirectoryResponse) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(2);
    csil_entries.push((cbor_text("generation"), cbor_uint(csil_v.generation)));
    csil_entries.push((
        cbor_text("placements"),
        cbor_enc_array(&csil_v.placements, csil_enc_project_placement),
    ));
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a DirectoryResponse from a decoded CBOR value tree.
fn csil_dec_directory_response(
    csil_root: &CsilCborValue,
) -> Result<DirectoryResponse, CsilCborError> {
    let placements = {
        let csil_field = cbor_require(csil_root, "placements")?;
        let csil_decode = |csil_v| cbor_dec_array(csil_v, csil_dec_project_placement);
        csil_decode(csil_field)?
    };
    let generation = {
        let csil_field = cbor_require(csil_root, "generation")?;
        let csil_decode = cbor_as_u64;
        csil_decode(csil_field)?
    };
    Ok(DirectoryResponse {
        placements,
        generation,
    })
}

/// Encode a DirectoryResponse to canonical CSIL CBOR bytes.
pub fn encode_directory_response(csil_v: &DirectoryResponse) -> Vec<u8> {
    cbor_encode(&csil_enc_directory_response(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a DirectoryResponse.
pub fn decode_directory_response(csil_data: &[u8]) -> Result<DirectoryResponse, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_directory_response(&csil_root)
}

/// Build the canonical CBOR value tree for a RouteRequest.
fn csil_enc_route_request(csil_v: &RouteRequest) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(2);
    csil_entries.push((cbor_text("project_id"), cbor_bytes(&csil_v.project_id)));
    csil_entries.push((cbor_text("affinity_key"), cbor_bytes(&csil_v.affinity_key)));
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a RouteRequest from a decoded CBOR value tree.
fn csil_dec_route_request(csil_root: &CsilCborValue) -> Result<RouteRequest, CsilCborError> {
    let project_id = {
        let csil_field = cbor_require(csil_root, "project_id")?;
        let csil_decode = cbor_as_bytes;
        csil_decode(csil_field)?
    };
    let affinity_key = {
        let csil_field = cbor_require(csil_root, "affinity_key")?;
        let csil_decode = cbor_as_bytes;
        csil_decode(csil_field)?
    };
    Ok(RouteRequest {
        project_id,
        affinity_key,
    })
}

/// Encode a RouteRequest to canonical CSIL CBOR bytes.
pub fn encode_route_request(csil_v: &RouteRequest) -> Vec<u8> {
    cbor_encode(&csil_enc_route_request(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a RouteRequest.
pub fn decode_route_request(csil_data: &[u8]) -> Result<RouteRequest, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_route_request(&csil_root)
}

/// Build the canonical CBOR value tree for a RouteResponse.
fn csil_enc_route_response(csil_v: &RouteResponse) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(8);
    csil_entries.push((cbor_text("cell"), cbor_text(&csil_v.cell)));
    csil_entries.push((cbor_text("epoch"), cbor_uint(csil_v.epoch)));
    csil_entries.push((cbor_text("shard"), cbor_uint(csil_v.shard)));
    if let Some(csil_inner) = &csil_v.leader {
        csil_entries.push((cbor_text("leader"), cbor_text(csil_inner)));
    }
    csil_entries.push((cbor_text("tablet"), cbor_text(&csil_v.tablet)));
    csil_entries.push((cbor_text("generation"), cbor_uint(csil_v.generation)));
    if let Some(csil_inner) = &csil_v.leader_address {
        csil_entries.push((cbor_text("leader_address"), cbor_text(csil_inner)));
    }
    csil_entries.push((
        cbor_text("receipt_policy"),
        cbor_text(&csil_v.receipt_policy),
    ));
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a RouteResponse from a decoded CBOR value tree.
fn csil_dec_route_response(csil_root: &CsilCborValue) -> Result<RouteResponse, CsilCborError> {
    let cell = {
        let csil_field = cbor_require(csil_root, "cell")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let shard = {
        let csil_field = cbor_require(csil_root, "shard")?;
        let csil_decode = cbor_as_u64;
        csil_decode(csil_field)?
    };
    let tablet = {
        let csil_field = cbor_require(csil_root, "tablet")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let leader = match cbor_map_get(csil_root, "leader") {
        Some(csil_field) => {
            let csil_decode = cbor_as_text;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let leader_address = match cbor_map_get(csil_root, "leader_address") {
        Some(csil_field) => {
            let csil_decode = cbor_as_text;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let generation = {
        let csil_field = cbor_require(csil_root, "generation")?;
        let csil_decode = cbor_as_u64;
        csil_decode(csil_field)?
    };
    let epoch = {
        let csil_field = cbor_require(csil_root, "epoch")?;
        let csil_decode = cbor_as_u64;
        csil_decode(csil_field)?
    };
    let receipt_policy = {
        let csil_field = cbor_require(csil_root, "receipt_policy")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    Ok(RouteResponse {
        cell,
        shard,
        tablet,
        leader,
        leader_address,
        generation,
        epoch,
        receipt_policy,
    })
}

/// Encode a RouteResponse to canonical CSIL CBOR bytes.
pub fn encode_route_response(csil_v: &RouteResponse) -> Vec<u8> {
    cbor_encode(&csil_enc_route_response(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a RouteResponse.
pub fn decode_route_response(csil_data: &[u8]) -> Result<RouteResponse, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_route_response(&csil_root)
}

/// Build the canonical CBOR value tree for a ReplicaStatusRequest.
fn csil_enc_replica_status_request(csil_v: &ReplicaStatusRequest) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(1);
    if let Some(csil_inner) = &csil_v.tablet {
        csil_entries.push((cbor_text("tablet"), cbor_text(csil_inner)));
    }
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a ReplicaStatusRequest from a decoded CBOR value tree.
fn csil_dec_replica_status_request(
    csil_root: &CsilCborValue,
) -> Result<ReplicaStatusRequest, CsilCborError> {
    let tablet = match cbor_map_get(csil_root, "tablet") {
        Some(csil_field) => {
            let csil_decode = cbor_as_text;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    Ok(ReplicaStatusRequest { tablet })
}

/// Encode a ReplicaStatusRequest to canonical CSIL CBOR bytes.
pub fn encode_replica_status_request(csil_v: &ReplicaStatusRequest) -> Vec<u8> {
    cbor_encode(&csil_enc_replica_status_request(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a ReplicaStatusRequest.
pub fn decode_replica_status_request(
    csil_data: &[u8],
) -> Result<ReplicaStatusRequest, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_replica_status_request(&csil_root)
}

/// Build the canonical CBOR value tree for a ReplicaStatus.
fn csil_enc_replica_status(csil_v: &ReplicaStatus) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(5);
    csil_entries.push((cbor_text("node"), cbor_text(&csil_v.node)));
    if let Some(csil_inner) = &csil_v.lag_ms {
        csil_entries.push((cbor_text("lag_ms"), cbor_int(*csil_inner)));
    }
    csil_entries.push((
        cbor_text("tablets"),
        cbor_enc_array(&csil_v.tablets, csil_enc_tablet_info),
    ));
    csil_entries.push((cbor_text("writable"), cbor_bool(csil_v.writable)));
    csil_entries.push((
        cbor_text("applied_watermark"),
        cbor_uint(csil_v.applied_watermark),
    ));
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a ReplicaStatus from a decoded CBOR value tree.
fn csil_dec_replica_status(csil_root: &CsilCborValue) -> Result<ReplicaStatus, CsilCborError> {
    let node = {
        let csil_field = cbor_require(csil_root, "node")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let tablets = {
        let csil_field = cbor_require(csil_root, "tablets")?;
        let csil_decode = |csil_v| cbor_dec_array(csil_v, csil_dec_tablet_info);
        csil_decode(csil_field)?
    };
    let applied_watermark = {
        let csil_field = cbor_require(csil_root, "applied_watermark")?;
        let csil_decode = cbor_as_u64;
        csil_decode(csil_field)?
    };
    let lag_ms = match cbor_map_get(csil_root, "lag_ms") {
        Some(csil_field) => {
            let csil_decode = cbor_as_i64;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let writable = {
        let csil_field = cbor_require(csil_root, "writable")?;
        let csil_decode = cbor_as_bool;
        csil_decode(csil_field)?
    };
    Ok(ReplicaStatus {
        node,
        tablets,
        applied_watermark,
        lag_ms,
        writable,
    })
}

/// Encode a ReplicaStatus to canonical CSIL CBOR bytes.
pub fn encode_replica_status(csil_v: &ReplicaStatus) -> Vec<u8> {
    cbor_encode(&csil_enc_replica_status(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a ReplicaStatus.
pub fn decode_replica_status(csil_data: &[u8]) -> Result<ReplicaStatus, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_replica_status(&csil_root)
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

/// Encode a GroupKind enum as its bare literal value.
fn csil_enc_group_kind(csil_v: &GroupKind) -> CsilCborValue {
    match csil_v {
        GroupKind::GlobalDirectory => cbor_text("global-directory"),
        GroupKind::CellController => cbor_text("cell-controller"),
        GroupKind::Tablet => cbor_text("tablet"),
    }
}

/// Decode a bare literal value into a GroupKind enum.
fn csil_dec_group_kind(csil_v: &CsilCborValue) -> Result<GroupKind, CsilCborError> {
    let csil_val = cbor_as_text(csil_v)?;
    match csil_val.as_str() {
        "global-directory" => Ok(GroupKind::GlobalDirectory),
        "cell-controller" => Ok(GroupKind::CellController),
        "tablet" => Ok(GroupKind::Tablet),
        csil_other => Err(CsilCborError(format!(
            "csil cbor: unknown GroupKind value {csil_other:?}"
        ))),
    }
}

/// Encode a MemberRole enum as its bare literal value.
fn csil_enc_member_role(csil_v: &MemberRole) -> CsilCborValue {
    match csil_v {
        MemberRole::Voter => cbor_text("voter"),
        MemberRole::Learner => cbor_text("learner"),
    }
}

/// Decode a bare literal value into a MemberRole enum.
fn csil_dec_member_role(csil_v: &CsilCborValue) -> Result<MemberRole, CsilCborError> {
    let csil_val = cbor_as_text(csil_v)?;
    match csil_val.as_str() {
        "voter" => Ok(MemberRole::Voter),
        "learner" => Ok(MemberRole::Learner),
        csil_other => Err(CsilCborError(format!(
            "csil cbor: unknown MemberRole value {csil_other:?}"
        ))),
    }
}

/// Encode a ConsensusKind enum as its bare literal value.
fn csil_enc_consensus_kind(csil_v: &ConsensusKind) -> CsilCborValue {
    match csil_v {
        ConsensusKind::AppendEntries => cbor_text("append-entries"),
        ConsensusKind::Vote => cbor_text("vote"),
        ConsensusKind::InstallSnapshot => cbor_text("install-snapshot"),
        ConsensusKind::Proposal => cbor_text("proposal"),
    }
}

/// Decode a bare literal value into a ConsensusKind enum.
fn csil_dec_consensus_kind(csil_v: &CsilCborValue) -> Result<ConsensusKind, CsilCborError> {
    let csil_val = cbor_as_text(csil_v)?;
    match csil_val.as_str() {
        "append-entries" => Ok(ConsensusKind::AppendEntries),
        "vote" => Ok(ConsensusKind::Vote),
        "install-snapshot" => Ok(ConsensusKind::InstallSnapshot),
        "proposal" => Ok(ConsensusKind::Proposal),
        csil_other => Err(CsilCborError(format!(
            "csil cbor: unknown ConsensusKind value {csil_other:?}"
        ))),
    }
}

/// Encode a PartialKind enum as its bare literal value.
fn csil_enc_partial_kind(csil_v: &PartialKind) -> CsilCborValue {
    match csil_v {
        PartialKind::Count => cbor_text("count"),
        PartialKind::Trend => cbor_text("trend"),
        PartialKind::Rows => cbor_text("rows"),
        PartialKind::Lookup => cbor_text("lookup"),
        PartialKind::Aggregate => cbor_text("aggregate"),
    }
}

/// Decode a bare literal value into a PartialKind enum.
fn csil_dec_partial_kind(csil_v: &CsilCborValue) -> Result<PartialKind, CsilCborError> {
    let csil_val = cbor_as_text(csil_v)?;
    match csil_val.as_str() {
        "count" => Ok(PartialKind::Count),
        "trend" => Ok(PartialKind::Trend),
        "rows" => Ok(PartialKind::Rows),
        "lookup" => Ok(PartialKind::Lookup),
        "aggregate" => Ok(PartialKind::Aggregate),
        csil_other => Err(CsilCborError(format!(
            "csil cbor: unknown PartialKind value {csil_other:?}"
        ))),
    }
}

/// Encode a SlowCause enum as its bare literal value.
fn csil_enc_slow_cause(csil_v: &SlowCause) -> CsilCborValue {
    match csil_v {
        SlowCause::StorageErrors => cbor_text("storage-errors"),
        SlowCause::StorageSaturated => cbor_text("storage-saturated"),
        SlowCause::StorageSlow => cbor_text("storage-slow"),
        SlowCause::WriteVolume => cbor_text("write-volume"),
        SlowCause::CompactionPressure => cbor_text("compaction-pressure"),
        SlowCause::MemoryPressure => cbor_text("memory-pressure"),
        SlowCause::NetworkLatency => cbor_text("network-latency"),
        SlowCause::Unknown => cbor_text("unknown"),
    }
}

/// Decode a bare literal value into a SlowCause enum.
fn csil_dec_slow_cause(csil_v: &CsilCborValue) -> Result<SlowCause, CsilCborError> {
    let csil_val = cbor_as_text(csil_v)?;
    match csil_val.as_str() {
        "storage-errors" => Ok(SlowCause::StorageErrors),
        "storage-saturated" => Ok(SlowCause::StorageSaturated),
        "storage-slow" => Ok(SlowCause::StorageSlow),
        "write-volume" => Ok(SlowCause::WriteVolume),
        "compaction-pressure" => Ok(SlowCause::CompactionPressure),
        "memory-pressure" => Ok(SlowCause::MemoryPressure),
        "network-latency" => Ok(SlowCause::NetworkLatency),
        "unknown" => Ok(SlowCause::Unknown),
        csil_other => Err(CsilCborError(format!(
            "csil cbor: unknown SlowCause value {csil_other:?}"
        ))),
    }
}

/// Encode a NodeState enum as its bare literal value.
fn csil_enc_node_state(csil_v: &NodeState) -> CsilCborValue {
    match csil_v {
        NodeState::Healthy => cbor_text("healthy"),
        NodeState::Slow => cbor_text("slow"),
        NodeState::Unreachable => cbor_text("unreachable"),
        NodeState::ReadOnly => cbor_text("read-only"),
        NodeState::Draining => cbor_text("draining"),
    }
}

/// Decode a bare literal value into a NodeState enum.
fn csil_dec_node_state(csil_v: &CsilCborValue) -> Result<NodeState, CsilCborError> {
    let csil_val = cbor_as_text(csil_v)?;
    match csil_val.as_str() {
        "healthy" => Ok(NodeState::Healthy),
        "slow" => Ok(NodeState::Slow),
        "unreachable" => Ok(NodeState::Unreachable),
        "read-only" => Ok(NodeState::ReadOnly),
        "draining" => Ok(NodeState::Draining),
        csil_other => Err(CsilCborError(format!(
            "csil cbor: unknown NodeState value {csil_other:?}"
        ))),
    }
}

/// Encode a TabletState enum as its bare literal value.
fn csil_enc_tablet_state(csil_v: &TabletState) -> CsilCborValue {
    match csil_v {
        TabletState::Active => cbor_text("active"),
        TabletState::Splitting => cbor_text("splitting"),
        TabletState::Merging => cbor_text("merging"),
        TabletState::Moving => cbor_text("moving"),
        TabletState::Draining => cbor_text("draining"),
        TabletState::Retired => cbor_text("retired"),
    }
}

/// Decode a bare literal value into a TabletState enum.
fn csil_dec_tablet_state(csil_v: &CsilCborValue) -> Result<TabletState, CsilCborError> {
    let csil_val = cbor_as_text(csil_v)?;
    match csil_val.as_str() {
        "active" => Ok(TabletState::Active),
        "splitting" => Ok(TabletState::Splitting),
        "merging" => Ok(TabletState::Merging),
        "moving" => Ok(TabletState::Moving),
        "draining" => Ok(TabletState::Draining),
        "retired" => Ok(TabletState::Retired),
        csil_other => Err(CsilCborError(format!(
            "csil cbor: unknown TabletState value {csil_other:?}"
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
