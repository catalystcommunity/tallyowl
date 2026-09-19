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

/// Build the canonical CBOR value tree for a ExpressionNode.
fn csil_enc_expression_node(csil_v: &ExpressionNode) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(11);
    if let Some(csil_inner) = &csil_v.arith {
        csil_entries.push((cbor_text("arith"), csil_enc_arith_expr(csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.field {
        csil_entries.push((cbor_text("field"), csil_enc_field_ref(csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.compare {
        csil_entries.push((cbor_text("compare"), csil_enc_compare_expr(csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.convert {
        csil_entries.push((cbor_text("convert"), csil_enc_convert_expr(csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.literal {
        csil_entries.push((cbor_text("literal"), csil_enc_typed_value(csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.logical {
        csil_entries.push((cbor_text("logical"), csil_enc_logical_expr(csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.set_expr {
        csil_entries.push((cbor_text("set_expr"), csil_enc_set_expr(csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.null_expr {
        csil_entries.push((cbor_text("null_expr"), csil_enc_null_expr(csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.text_expr {
        csil_entries.push((cbor_text("text_expr"), csil_enc_text_expr(csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.time_expr {
        csil_entries.push((cbor_text("time_expr"), csil_enc_time_expr(csil_inner)));
    }
    csil_entries.push((
        cbor_text("expression"),
        csil_enc_expression_kind(&csil_v.expression),
    ));
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a ExpressionNode from a decoded CBOR value tree.
fn csil_dec_expression_node(csil_root: &CsilCborValue) -> Result<ExpressionNode, CsilCborError> {
    let expression = {
        let csil_field = cbor_require(csil_root, "expression")?;
        let csil_decode = csil_dec_expression_kind;
        csil_decode(csil_field)?
    };
    let literal = match cbor_map_get(csil_root, "literal") {
        Some(csil_field) => {
            let csil_decode = csil_dec_typed_value;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let field = match cbor_map_get(csil_root, "field") {
        Some(csil_field) => {
            let csil_decode = csil_dec_field_ref;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let compare = match cbor_map_get(csil_root, "compare") {
        Some(csil_field) => {
            let csil_decode = csil_dec_compare_expr;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let set_expr = match cbor_map_get(csil_root, "set_expr") {
        Some(csil_field) => {
            let csil_decode = csil_dec_set_expr;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let text_expr = match cbor_map_get(csil_root, "text_expr") {
        Some(csil_field) => {
            let csil_decode = csil_dec_text_expr;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let null_expr = match cbor_map_get(csil_root, "null_expr") {
        Some(csil_field) => {
            let csil_decode = csil_dec_null_expr;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let logical = match cbor_map_get(csil_root, "logical") {
        Some(csil_field) => {
            let csil_decode = csil_dec_logical_expr;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let arith = match cbor_map_get(csil_root, "arith") {
        Some(csil_field) => {
            let csil_decode = csil_dec_arith_expr;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let time_expr = match cbor_map_get(csil_root, "time_expr") {
        Some(csil_field) => {
            let csil_decode = csil_dec_time_expr;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let convert = match cbor_map_get(csil_root, "convert") {
        Some(csil_field) => {
            let csil_decode = csil_dec_convert_expr;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    Ok(ExpressionNode {
        expression,
        literal,
        field,
        compare,
        set_expr,
        text_expr,
        null_expr,
        logical,
        arith,
        time_expr,
        convert,
    })
}

/// Encode a ExpressionNode to canonical CSIL CBOR bytes.
pub fn encode_expression_node(csil_v: &ExpressionNode) -> Vec<u8> {
    cbor_encode(&csil_enc_expression_node(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a ExpressionNode.
pub fn decode_expression_node(csil_data: &[u8]) -> Result<ExpressionNode, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_expression_node(&csil_root)
}

/// Build the canonical CBOR value tree for a QueryNodeBox.
fn csil_enc_query_node_box(csil_v: &QueryNodeBox) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(9);
    if let Some(csil_inner) = &csil_v.join {
        csil_entries.push((cbor_text("join"), csil_enc_join_node(csil_inner)));
    }
    csil_entries.push((cbor_text("node"), csil_enc_query_node_kind(&csil_v.node)));
    if let Some(csil_inner) = &csil_v.scan {
        csil_entries.push((cbor_text("scan"), csil_enc_scan_node(csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.sort {
        csil_entries.push((cbor_text("sort"), csil_enc_sort_node(csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.limit {
        csil_entries.push((cbor_text("limit"), csil_enc_limit_node(csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.union {
        csil_entries.push((cbor_text("union"), csil_enc_union_node(csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.filter {
        csil_entries.push((cbor_text("filter"), csil_enc_filter_node(csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.project {
        csil_entries.push((cbor_text("project"), csil_enc_project_node(csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.aggregate {
        csil_entries.push((cbor_text("aggregate"), csil_enc_aggregate_node(csil_inner)));
    }
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a QueryNodeBox from a decoded CBOR value tree.
fn csil_dec_query_node_box(csil_root: &CsilCborValue) -> Result<QueryNodeBox, CsilCborError> {
    let node = {
        let csil_field = cbor_require(csil_root, "node")?;
        let csil_decode = csil_dec_query_node_kind;
        csil_decode(csil_field)?
    };
    let scan = match cbor_map_get(csil_root, "scan") {
        Some(csil_field) => {
            let csil_decode = csil_dec_scan_node;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let filter = match cbor_map_get(csil_root, "filter") {
        Some(csil_field) => {
            let csil_decode = csil_dec_filter_node;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let project = match cbor_map_get(csil_root, "project") {
        Some(csil_field) => {
            let csil_decode = csil_dec_project_node;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let aggregate = match cbor_map_get(csil_root, "aggregate") {
        Some(csil_field) => {
            let csil_decode = csil_dec_aggregate_node;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let sort = match cbor_map_get(csil_root, "sort") {
        Some(csil_field) => {
            let csil_decode = csil_dec_sort_node;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let limit = match cbor_map_get(csil_root, "limit") {
        Some(csil_field) => {
            let csil_decode = csil_dec_limit_node;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let join = match cbor_map_get(csil_root, "join") {
        Some(csil_field) => {
            let csil_decode = csil_dec_join_node;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let union = match cbor_map_get(csil_root, "union") {
        Some(csil_field) => {
            let csil_decode = csil_dec_union_node;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    Ok(QueryNodeBox {
        node,
        scan,
        filter,
        project,
        aggregate,
        sort,
        limit,
        join,
        union,
    })
}

/// Encode a QueryNodeBox to canonical CSIL CBOR bytes.
pub fn encode_query_node_box(csil_v: &QueryNodeBox) -> Vec<u8> {
    cbor_encode(&csil_enc_query_node_box(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a QueryNodeBox.
pub fn decode_query_node_box(csil_data: &[u8]) -> Result<QueryNodeBox, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_query_node_box(&csil_root)
}

/// Build the canonical CBOR value tree for a FieldRef.
fn csil_enc_field_ref(csil_v: &FieldRef) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(3);
    csil_entries.push((cbor_text("name"), cbor_text(&csil_v.name)));
    if let Some(csil_inner) = &csil_v.origin {
        csil_entries.push((cbor_text("origin"), csil_enc_property_origin(csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.value_type {
        csil_entries.push((cbor_text("value_type"), cbor_text(csil_inner)));
    }
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a FieldRef from a decoded CBOR value tree.
fn csil_dec_field_ref(csil_root: &CsilCborValue) -> Result<FieldRef, CsilCborError> {
    let name = {
        let csil_field = cbor_require(csil_root, "name")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let value_type = match cbor_map_get(csil_root, "value_type") {
        Some(csil_field) => {
            let csil_decode = cbor_as_text;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let origin = match cbor_map_get(csil_root, "origin") {
        Some(csil_field) => {
            let csil_decode = csil_dec_property_origin;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    Ok(FieldRef {
        name,
        value_type,
        origin,
    })
}

/// Encode a FieldRef to canonical CSIL CBOR bytes.
pub fn encode_field_ref(csil_v: &FieldRef) -> Vec<u8> {
    cbor_encode(&csil_enc_field_ref(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a FieldRef.
pub fn decode_field_ref(csil_data: &[u8]) -> Result<FieldRef, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_field_ref(&csil_root)
}

/// Build the canonical CBOR value tree for a CompareExpr.
fn csil_enc_compare_expr(csil_v: &CompareExpr) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(3);
    csil_entries.push((cbor_text("left"), cbor_bytes(&csil_v.left)));
    csil_entries.push((cbor_text("right"), cbor_bytes(&csil_v.right)));
    csil_entries.push((cbor_text("compare"), csil_enc_compare_op(&csil_v.compare)));
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a CompareExpr from a decoded CBOR value tree.
fn csil_dec_compare_expr(csil_root: &CsilCborValue) -> Result<CompareExpr, CsilCborError> {
    let compare = {
        let csil_field = cbor_require(csil_root, "compare")?;
        let csil_decode = csil_dec_compare_op;
        csil_decode(csil_field)?
    };
    let left = {
        let csil_field = cbor_require(csil_root, "left")?;
        let csil_decode = cbor_as_bytes;
        csil_decode(csil_field)?
    };
    let right = {
        let csil_field = cbor_require(csil_root, "right")?;
        let csil_decode = cbor_as_bytes;
        csil_decode(csil_field)?
    };
    Ok(CompareExpr {
        compare,
        left,
        right,
    })
}

/// Encode a CompareExpr to canonical CSIL CBOR bytes.
pub fn encode_compare_expr(csil_v: &CompareExpr) -> Vec<u8> {
    cbor_encode(&csil_enc_compare_expr(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a CompareExpr.
pub fn decode_compare_expr(csil_data: &[u8]) -> Result<CompareExpr, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_compare_expr(&csil_root)
}

/// Build the canonical CBOR value tree for a SetExpr.
fn csil_enc_set_expr(csil_v: &SetExpr) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(3);
    csil_entries.push((cbor_text("left"), cbor_bytes(&csil_v.left)));
    csil_entries.push((
        cbor_text("values"),
        cbor_enc_array(&csil_v.values, csil_enc_typed_value),
    ));
    csil_entries.push((
        cbor_text("set_test"),
        csil_enc_set_expr_set_test(&csil_v.set_test),
    ));
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a SetExpr from a decoded CBOR value tree.
fn csil_dec_set_expr(csil_root: &CsilCborValue) -> Result<SetExpr, CsilCborError> {
    let set_test = {
        let csil_field = cbor_require(csil_root, "set_test")?;
        let csil_decode = csil_dec_set_expr_set_test;
        csil_decode(csil_field)?
    };
    let left = {
        let csil_field = cbor_require(csil_root, "left")?;
        let csil_decode = cbor_as_bytes;
        csil_decode(csil_field)?
    };
    let values = {
        let csil_field = cbor_require(csil_root, "values")?;
        let csil_decode = |csil_v| cbor_dec_array(csil_v, csil_dec_typed_value);
        csil_decode(csil_field)?
    };
    Ok(SetExpr {
        set_test,
        left,
        values,
    })
}

/// Encode a SetExpr to canonical CSIL CBOR bytes.
pub fn encode_set_expr(csil_v: &SetExpr) -> Vec<u8> {
    cbor_encode(&csil_enc_set_expr(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a SetExpr.
pub fn decode_set_expr(csil_data: &[u8]) -> Result<SetExpr, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_set_expr(&csil_root)
}

/// Build the canonical CBOR value tree for a TextExpr.
fn csil_enc_text_expr(csil_v: &TextExpr) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(3);
    csil_entries.push((cbor_text("left"), cbor_bytes(&csil_v.left)));
    csil_entries.push((cbor_text("pattern"), cbor_text(&csil_v.pattern)));
    csil_entries.push((
        cbor_text("text_test"),
        csil_enc_text_expr_text_test(&csil_v.text_test),
    ));
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a TextExpr from a decoded CBOR value tree.
fn csil_dec_text_expr(csil_root: &CsilCborValue) -> Result<TextExpr, CsilCborError> {
    let text_test = {
        let csil_field = cbor_require(csil_root, "text_test")?;
        let csil_decode = csil_dec_text_expr_text_test;
        csil_decode(csil_field)?
    };
    let left = {
        let csil_field = cbor_require(csil_root, "left")?;
        let csil_decode = cbor_as_bytes;
        csil_decode(csil_field)?
    };
    let pattern = {
        let csil_field = cbor_require(csil_root, "pattern")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    Ok(TextExpr {
        text_test,
        left,
        pattern,
    })
}

/// Encode a TextExpr to canonical CSIL CBOR bytes.
pub fn encode_text_expr(csil_v: &TextExpr) -> Vec<u8> {
    cbor_encode(&csil_enc_text_expr(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a TextExpr.
pub fn decode_text_expr(csil_data: &[u8]) -> Result<TextExpr, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_text_expr(&csil_root)
}

/// Build the canonical CBOR value tree for a NullExpr.
fn csil_enc_null_expr(csil_v: &NullExpr) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(2);
    csil_entries.push((cbor_text("operand"), cbor_bytes(&csil_v.operand)));
    csil_entries.push((
        cbor_text("null_test"),
        csil_enc_null_expr_null_test(&csil_v.null_test),
    ));
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a NullExpr from a decoded CBOR value tree.
fn csil_dec_null_expr(csil_root: &CsilCborValue) -> Result<NullExpr, CsilCborError> {
    let null_test = {
        let csil_field = cbor_require(csil_root, "null_test")?;
        let csil_decode = csil_dec_null_expr_null_test;
        csil_decode(csil_field)?
    };
    let operand = {
        let csil_field = cbor_require(csil_root, "operand")?;
        let csil_decode = cbor_as_bytes;
        csil_decode(csil_field)?
    };
    Ok(NullExpr { null_test, operand })
}

/// Encode a NullExpr to canonical CSIL CBOR bytes.
pub fn encode_null_expr(csil_v: &NullExpr) -> Vec<u8> {
    cbor_encode(&csil_enc_null_expr(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a NullExpr.
pub fn decode_null_expr(csil_data: &[u8]) -> Result<NullExpr, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_null_expr(&csil_root)
}

/// Build the canonical CBOR value tree for a LogicalExpr.
fn csil_enc_logical_expr(csil_v: &LogicalExpr) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(2);
    csil_entries.push((cbor_text("logical"), csil_enc_logical_op(&csil_v.logical)));
    csil_entries.push((
        cbor_text("operands"),
        cbor_enc_array(&csil_v.operands, |csil_elem| cbor_bytes(csil_elem)),
    ));
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a LogicalExpr from a decoded CBOR value tree.
fn csil_dec_logical_expr(csil_root: &CsilCborValue) -> Result<LogicalExpr, CsilCborError> {
    let logical = {
        let csil_field = cbor_require(csil_root, "logical")?;
        let csil_decode = csil_dec_logical_op;
        csil_decode(csil_field)?
    };
    let operands = {
        let csil_field = cbor_require(csil_root, "operands")?;
        let csil_decode = |csil_v| cbor_dec_array(csil_v, cbor_as_bytes);
        csil_decode(csil_field)?
    };
    Ok(LogicalExpr { logical, operands })
}

/// Encode a LogicalExpr to canonical CSIL CBOR bytes.
pub fn encode_logical_expr(csil_v: &LogicalExpr) -> Vec<u8> {
    cbor_encode(&csil_enc_logical_expr(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a LogicalExpr.
pub fn decode_logical_expr(csil_data: &[u8]) -> Result<LogicalExpr, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_logical_expr(&csil_root)
}

/// Build the canonical CBOR value tree for a ArithExpr.
fn csil_enc_arith_expr(csil_v: &ArithExpr) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(3);
    csil_entries.push((cbor_text("left"), cbor_bytes(&csil_v.left)));
    csil_entries.push((cbor_text("arith"), csil_enc_arith_op(&csil_v.arith)));
    csil_entries.push((cbor_text("right"), cbor_bytes(&csil_v.right)));
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a ArithExpr from a decoded CBOR value tree.
fn csil_dec_arith_expr(csil_root: &CsilCborValue) -> Result<ArithExpr, CsilCborError> {
    let arith = {
        let csil_field = cbor_require(csil_root, "arith")?;
        let csil_decode = csil_dec_arith_op;
        csil_decode(csil_field)?
    };
    let left = {
        let csil_field = cbor_require(csil_root, "left")?;
        let csil_decode = cbor_as_bytes;
        csil_decode(csil_field)?
    };
    let right = {
        let csil_field = cbor_require(csil_root, "right")?;
        let csil_decode = cbor_as_bytes;
        csil_decode(csil_field)?
    };
    Ok(ArithExpr { arith, left, right })
}

/// Encode a ArithExpr to canonical CSIL CBOR bytes.
pub fn encode_arith_expr(csil_v: &ArithExpr) -> Vec<u8> {
    cbor_encode(&csil_enc_arith_expr(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a ArithExpr.
pub fn decode_arith_expr(csil_data: &[u8]) -> Result<ArithExpr, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_arith_expr(&csil_root)
}

/// Build the canonical CBOR value tree for a TimeExpr.
fn csil_enc_time_expr(csil_v: &TimeExpr) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(5);
    if let Some(csil_inner) = &csil_v.part {
        csil_entries.push((cbor_text("part"), cbor_text(csil_inner)));
    }
    csil_entries.push((cbor_text("operand"), cbor_bytes(&csil_v.operand)));
    csil_entries.push((
        cbor_text("time_fn"),
        csil_enc_time_expr_time_fn(&csil_v.time_fn),
    ));
    if let Some(csil_inner) = &csil_v.interval {
        csil_entries.push((cbor_text("interval"), csil_enc_interval(csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.shift_ms {
        csil_entries.push((cbor_text("shift_ms"), cbor_int(*csil_inner)));
    }
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a TimeExpr from a decoded CBOR value tree.
fn csil_dec_time_expr(csil_root: &CsilCborValue) -> Result<TimeExpr, CsilCborError> {
    let time_fn = {
        let csil_field = cbor_require(csil_root, "time_fn")?;
        let csil_decode = csil_dec_time_expr_time_fn;
        csil_decode(csil_field)?
    };
    let operand = {
        let csil_field = cbor_require(csil_root, "operand")?;
        let csil_decode = cbor_as_bytes;
        csil_decode(csil_field)?
    };
    let interval = match cbor_map_get(csil_root, "interval") {
        Some(csil_field) => {
            let csil_decode = csil_dec_interval;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let part = match cbor_map_get(csil_root, "part") {
        Some(csil_field) => {
            let csil_decode = cbor_as_text;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let shift_ms = match cbor_map_get(csil_root, "shift_ms") {
        Some(csil_field) => {
            let csil_decode = cbor_as_i64;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    Ok(TimeExpr {
        time_fn,
        operand,
        interval,
        part,
        shift_ms,
    })
}

/// Encode a TimeExpr to canonical CSIL CBOR bytes.
pub fn encode_time_expr(csil_v: &TimeExpr) -> Vec<u8> {
    cbor_encode(&csil_enc_time_expr(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a TimeExpr.
pub fn decode_time_expr(csil_data: &[u8]) -> Result<TimeExpr, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_time_expr(&csil_root)
}

/// Build the canonical CBOR value tree for a ConvertExpr.
fn csil_enc_convert_expr(csil_v: &ConvertExpr) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(2);
    csil_entries.push((cbor_text("operand"), cbor_bytes(&csil_v.operand)));
    csil_entries.push((cbor_text("convert_to"), cbor_text(&csil_v.convert_to)));
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a ConvertExpr from a decoded CBOR value tree.
fn csil_dec_convert_expr(csil_root: &CsilCborValue) -> Result<ConvertExpr, CsilCborError> {
    let convert_to = {
        let csil_field = cbor_require(csil_root, "convert_to")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let operand = {
        let csil_field = cbor_require(csil_root, "operand")?;
        let csil_decode = cbor_as_bytes;
        csil_decode(csil_field)?
    };
    Ok(ConvertExpr {
        convert_to,
        operand,
    })
}

/// Encode a ConvertExpr to canonical CSIL CBOR bytes.
pub fn encode_convert_expr(csil_v: &ConvertExpr) -> Vec<u8> {
    cbor_encode(&csil_enc_convert_expr(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a ConvertExpr.
pub fn decode_convert_expr(csil_data: &[u8]) -> Result<ConvertExpr, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_convert_expr(&csil_root)
}

/// Build the canonical CBOR value tree for a Interval.
fn csil_enc_interval(csil_v: &Interval) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(2);
    if let Some(csil_inner) = &csil_v.calendar {
        csil_entries.push((
            cbor_text("calendar"),
            csil_enc_interval_calendar(csil_inner),
        ));
    }
    if let Some(csil_inner) = &csil_v.fixed_ms {
        csil_entries.push((cbor_text("fixed_ms"), cbor_int(*csil_inner)));
    }
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a Interval from a decoded CBOR value tree.
fn csil_dec_interval(csil_root: &CsilCborValue) -> Result<Interval, CsilCborError> {
    let fixed_ms = match cbor_map_get(csil_root, "fixed_ms") {
        Some(csil_field) => {
            let csil_decode = cbor_as_i64;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let calendar = match cbor_map_get(csil_root, "calendar") {
        Some(csil_field) => {
            let csil_decode = csil_dec_interval_calendar;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    Ok(Interval { fixed_ms, calendar })
}

/// Encode a Interval to canonical CSIL CBOR bytes.
pub fn encode_interval(csil_v: &Interval) -> Vec<u8> {
    cbor_encode(&csil_enc_interval(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a Interval.
pub fn decode_interval(csil_data: &[u8]) -> Result<Interval, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_interval(&csil_root)
}

/// Build the canonical CBOR value tree for a TimeRange.
fn csil_enc_time_range(csil_v: &TimeRange) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(4);
    csil_entries.push((cbor_text("basis"), csil_enc_time_basis(&csil_v.basis)));
    if let Some(csil_inner) = &csil_v.timezone {
        csil_entries.push((cbor_text("timezone"), cbor_text(csil_inner)));
    }
    csil_entries.push((cbor_text("range_end"), cbor_int(csil_v.range_end)));
    csil_entries.push((cbor_text("range_start"), cbor_int(csil_v.range_start)));
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a TimeRange from a decoded CBOR value tree.
fn csil_dec_time_range(csil_root: &CsilCborValue) -> Result<TimeRange, CsilCborError> {
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
        let csil_decode = csil_dec_time_basis;
        csil_decode(csil_field)?
    };
    let timezone = match cbor_map_get(csil_root, "timezone") {
        Some(csil_field) => {
            let csil_decode = cbor_as_text;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    Ok(TimeRange {
        range_start,
        range_end,
        basis,
        timezone,
    })
}

/// Encode a TimeRange to canonical CSIL CBOR bytes.
pub fn encode_time_range(csil_v: &TimeRange) -> Vec<u8> {
    cbor_encode(&csil_enc_time_range(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a TimeRange.
pub fn decode_time_range(csil_data: &[u8]) -> Result<TimeRange, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_time_range(&csil_root)
}

/// Build the canonical CBOR value tree for a Measure.
fn csil_enc_measure(csil_v: &Measure) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(5);
    if let Some(csil_inner) = &csil_v.k {
        csil_entries.push((cbor_text("k"), cbor_uint(*csil_inner)));
    }
    csil_entries.push((cbor_text("kind"), csil_enc_measure_kind(&csil_v.kind)));
    csil_entries.push((cbor_text("alias"), cbor_text(&csil_v.alias)));
    if let Some(csil_inner) = &csil_v.field {
        csil_entries.push((cbor_text("field"), csil_enc_field_ref(csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.quantile {
        csil_entries.push((cbor_text("quantile"), cbor_float(*csil_inner)));
    }
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a Measure from a decoded CBOR value tree.
fn csil_dec_measure(csil_root: &CsilCborValue) -> Result<Measure, CsilCborError> {
    let kind = {
        let csil_field = cbor_require(csil_root, "kind")?;
        let csil_decode = csil_dec_measure_kind;
        csil_decode(csil_field)?
    };
    let field = match cbor_map_get(csil_root, "field") {
        Some(csil_field) => {
            let csil_decode = csil_dec_field_ref;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let quantile = match cbor_map_get(csil_root, "quantile") {
        Some(csil_field) => {
            let csil_decode = cbor_as_f64;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let k = match cbor_map_get(csil_root, "k") {
        Some(csil_field) => {
            let csil_decode = cbor_as_u64;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let alias = {
        let csil_field = cbor_require(csil_root, "alias")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    Ok(Measure {
        kind,
        field,
        quantile,
        k,
        alias,
    })
}

/// Encode a Measure to canonical CSIL CBOR bytes.
pub fn encode_measure(csil_v: &Measure) -> Vec<u8> {
    cbor_encode(&csil_enc_measure(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a Measure.
pub fn decode_measure(csil_data: &[u8]) -> Result<Measure, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_measure(&csil_root)
}

/// Build the canonical CBOR value tree for a Dimension.
fn csil_enc_dimension(csil_v: &Dimension) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(2);
    csil_entries.push((cbor_text("alias"), cbor_text(&csil_v.alias)));
    csil_entries.push((cbor_text("field"), csil_enc_field_ref(&csil_v.field)));
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a Dimension from a decoded CBOR value tree.
fn csil_dec_dimension(csil_root: &CsilCborValue) -> Result<Dimension, CsilCborError> {
    let field = {
        let csil_field = cbor_require(csil_root, "field")?;
        let csil_decode = csil_dec_field_ref;
        csil_decode(csil_field)?
    };
    let alias = {
        let csil_field = cbor_require(csil_root, "alias")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    Ok(Dimension { field, alias })
}

/// Encode a Dimension to canonical CSIL CBOR bytes.
pub fn encode_dimension(csil_v: &Dimension) -> Vec<u8> {
    cbor_encode(&csil_enc_dimension(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a Dimension.
pub fn decode_dimension(csil_data: &[u8]) -> Result<Dimension, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_dimension(&csil_root)
}

/// Build the canonical CBOR value tree for a SortKey.
fn csil_enc_sort_key(csil_v: &SortKey) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(2);
    csil_entries.push((cbor_text("alias"), cbor_text(&csil_v.alias)));
    csil_entries.push((
        cbor_text("direction"),
        csil_enc_sort_key_direction(&csil_v.direction),
    ));
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a SortKey from a decoded CBOR value tree.
fn csil_dec_sort_key(csil_root: &CsilCborValue) -> Result<SortKey, CsilCborError> {
    let alias = {
        let csil_field = cbor_require(csil_root, "alias")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let direction = {
        let csil_field = cbor_require(csil_root, "direction")?;
        let csil_decode = csil_dec_sort_key_direction;
        csil_decode(csil_field)?
    };
    Ok(SortKey { alias, direction })
}

/// Encode a SortKey to canonical CSIL CBOR bytes.
pub fn encode_sort_key(csil_v: &SortKey) -> Vec<u8> {
    cbor_encode(&csil_enc_sort_key(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a SortKey.
pub fn decode_sort_key(csil_data: &[u8]) -> Result<SortKey, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_sort_key(&csil_root)
}

/// Build the canonical CBOR value tree for a ScanNode.
fn csil_enc_scan_node(csil_v: &ScanNode) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(3);
    csil_entries.push((cbor_text("scan"), csil_enc_dataset(&csil_v.scan)));
    csil_entries.push((cbor_text("range"), csil_enc_time_range(&csil_v.range)));
    csil_entries.push((cbor_text("project_id"), cbor_bytes(&csil_v.project_id)));
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a ScanNode from a decoded CBOR value tree.
fn csil_dec_scan_node(csil_root: &CsilCborValue) -> Result<ScanNode, CsilCborError> {
    let scan = {
        let csil_field = cbor_require(csil_root, "scan")?;
        let csil_decode = csil_dec_dataset;
        csil_decode(csil_field)?
    };
    let project_id = {
        let csil_field = cbor_require(csil_root, "project_id")?;
        let csil_decode = cbor_as_bytes;
        csil_decode(csil_field)?
    };
    let range = {
        let csil_field = cbor_require(csil_root, "range")?;
        let csil_decode = csil_dec_time_range;
        csil_decode(csil_field)?
    };
    Ok(ScanNode {
        scan,
        project_id,
        range,
    })
}

/// Encode a ScanNode to canonical CSIL CBOR bytes.
pub fn encode_scan_node(csil_v: &ScanNode) -> Vec<u8> {
    cbor_encode(&csil_enc_scan_node(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a ScanNode.
pub fn decode_scan_node(csil_data: &[u8]) -> Result<ScanNode, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_scan_node(&csil_root)
}

/// Build the canonical CBOR value tree for a FilterNode.
fn csil_enc_filter_node(csil_v: &FilterNode) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(2);
    csil_entries.push((cbor_text("input"), cbor_bytes(&csil_v.input)));
    csil_entries.push((cbor_text("filter"), cbor_bytes(&csil_v.filter)));
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a FilterNode from a decoded CBOR value tree.
fn csil_dec_filter_node(csil_root: &CsilCborValue) -> Result<FilterNode, CsilCborError> {
    let filter = {
        let csil_field = cbor_require(csil_root, "filter")?;
        let csil_decode = cbor_as_bytes;
        csil_decode(csil_field)?
    };
    let input = {
        let csil_field = cbor_require(csil_root, "input")?;
        let csil_decode = cbor_as_bytes;
        csil_decode(csil_field)?
    };
    Ok(FilterNode { filter, input })
}

/// Encode a FilterNode to canonical CSIL CBOR bytes.
pub fn encode_filter_node(csil_v: &FilterNode) -> Vec<u8> {
    cbor_encode(&csil_enc_filter_node(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a FilterNode.
pub fn decode_filter_node(csil_data: &[u8]) -> Result<FilterNode, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_filter_node(&csil_root)
}

/// Build the canonical CBOR value tree for a ProjectNode.
fn csil_enc_project_node(csil_v: &ProjectNode) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(2);
    csil_entries.push((cbor_text("input"), cbor_bytes(&csil_v.input)));
    csil_entries.push((
        cbor_text("project_fields"),
        cbor_enc_array(&csil_v.project_fields, csil_enc_dimension),
    ));
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a ProjectNode from a decoded CBOR value tree.
fn csil_dec_project_node(csil_root: &CsilCborValue) -> Result<ProjectNode, CsilCborError> {
    let project_fields = {
        let csil_field = cbor_require(csil_root, "project_fields")?;
        let csil_decode = |csil_v| cbor_dec_array(csil_v, csil_dec_dimension);
        csil_decode(csil_field)?
    };
    let input = {
        let csil_field = cbor_require(csil_root, "input")?;
        let csil_decode = cbor_as_bytes;
        csil_decode(csil_field)?
    };
    Ok(ProjectNode {
        project_fields,
        input,
    })
}

/// Encode a ProjectNode to canonical CSIL CBOR bytes.
pub fn encode_project_node(csil_v: &ProjectNode) -> Vec<u8> {
    cbor_encode(&csil_enc_project_node(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a ProjectNode.
pub fn decode_project_node(csil_data: &[u8]) -> Result<ProjectNode, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_project_node(&csil_root)
}

/// Build the canonical CBOR value tree for a AggregateNode.
fn csil_enc_aggregate_node(csil_v: &AggregateNode) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(4);
    csil_entries.push((cbor_text("input"), cbor_bytes(&csil_v.input)));
    if let Some(csil_inner) = &csil_v.interval {
        csil_entries.push((cbor_text("interval"), csil_enc_interval(csil_inner)));
    }
    csil_entries.push((
        cbor_text("measures"),
        cbor_enc_array(&csil_v.measures, csil_enc_measure),
    ));
    csil_entries.push((
        cbor_text("dimensions"),
        cbor_enc_array(&csil_v.dimensions, csil_enc_dimension),
    ));
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a AggregateNode from a decoded CBOR value tree.
fn csil_dec_aggregate_node(csil_root: &CsilCborValue) -> Result<AggregateNode, CsilCborError> {
    let dimensions = {
        let csil_field = cbor_require(csil_root, "dimensions")?;
        let csil_decode = |csil_v| cbor_dec_array(csil_v, csil_dec_dimension);
        csil_decode(csil_field)?
    };
    let measures = {
        let csil_field = cbor_require(csil_root, "measures")?;
        let csil_decode = |csil_v| cbor_dec_array(csil_v, csil_dec_measure);
        csil_decode(csil_field)?
    };
    let interval = match cbor_map_get(csil_root, "interval") {
        Some(csil_field) => {
            let csil_decode = csil_dec_interval;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let input = {
        let csil_field = cbor_require(csil_root, "input")?;
        let csil_decode = cbor_as_bytes;
        csil_decode(csil_field)?
    };
    Ok(AggregateNode {
        dimensions,
        measures,
        interval,
        input,
    })
}

/// Encode a AggregateNode to canonical CSIL CBOR bytes.
pub fn encode_aggregate_node(csil_v: &AggregateNode) -> Vec<u8> {
    cbor_encode(&csil_enc_aggregate_node(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a AggregateNode.
pub fn decode_aggregate_node(csil_data: &[u8]) -> Result<AggregateNode, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_aggregate_node(&csil_root)
}

/// Build the canonical CBOR value tree for a SortNode.
fn csil_enc_sort_node(csil_v: &SortNode) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(2);
    csil_entries.push((
        cbor_text("sort"),
        cbor_enc_array(&csil_v.sort, csil_enc_sort_key),
    ));
    csil_entries.push((cbor_text("input"), cbor_bytes(&csil_v.input)));
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a SortNode from a decoded CBOR value tree.
fn csil_dec_sort_node(csil_root: &CsilCborValue) -> Result<SortNode, CsilCborError> {
    let sort = {
        let csil_field = cbor_require(csil_root, "sort")?;
        let csil_decode = |csil_v| cbor_dec_array(csil_v, csil_dec_sort_key);
        csil_decode(csil_field)?
    };
    let input = {
        let csil_field = cbor_require(csil_root, "input")?;
        let csil_decode = cbor_as_bytes;
        csil_decode(csil_field)?
    };
    Ok(SortNode { sort, input })
}

/// Encode a SortNode to canonical CSIL CBOR bytes.
pub fn encode_sort_node(csil_v: &SortNode) -> Vec<u8> {
    cbor_encode(&csil_enc_sort_node(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a SortNode.
pub fn decode_sort_node(csil_data: &[u8]) -> Result<SortNode, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_sort_node(&csil_root)
}

/// Build the canonical CBOR value tree for a LimitNode.
fn csil_enc_limit_node(csil_v: &LimitNode) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(4);
    csil_entries.push((cbor_text("input"), cbor_bytes(&csil_v.input)));
    csil_entries.push((cbor_text("limit"), cbor_uint(csil_v.limit)));
    if let Some(csil_inner) = &csil_v.cursor {
        csil_entries.push((cbor_text("cursor"), cbor_bytes(csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.offset {
        csil_entries.push((cbor_text("offset"), cbor_uint(*csil_inner)));
    }
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a LimitNode from a decoded CBOR value tree.
fn csil_dec_limit_node(csil_root: &CsilCborValue) -> Result<LimitNode, CsilCborError> {
    let limit = {
        let csil_field = cbor_require(csil_root, "limit")?;
        let csil_decode = cbor_as_u64;
        csil_decode(csil_field)?
    };
    let offset = match cbor_map_get(csil_root, "offset") {
        Some(csil_field) => {
            let csil_decode = cbor_as_u64;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let cursor = match cbor_map_get(csil_root, "cursor") {
        Some(csil_field) => {
            let csil_decode = cbor_as_bytes;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let input = {
        let csil_field = cbor_require(csil_root, "input")?;
        let csil_decode = cbor_as_bytes;
        csil_decode(csil_field)?
    };
    Ok(LimitNode {
        limit,
        offset,
        cursor,
        input,
    })
}

/// Encode a LimitNode to canonical CSIL CBOR bytes.
pub fn encode_limit_node(csil_v: &LimitNode) -> Vec<u8> {
    cbor_encode(&csil_enc_limit_node(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a LimitNode.
pub fn decode_limit_node(csil_data: &[u8]) -> Result<LimitNode, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_limit_node(&csil_root)
}

/// Build the canonical CBOR value tree for a JoinNode.
fn csil_enc_join_node(csil_v: &JoinNode) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(4);
    csil_entries.push((cbor_text("left"), cbor_bytes(&csil_v.left)));
    csil_entries.push((cbor_text("right"), cbor_bytes(&csil_v.right)));
    csil_entries.push((cbor_text("join_key"), csil_enc_field_ref(&csil_v.join_key)));
    csil_entries.push((
        cbor_text("max_rows_each_side"),
        cbor_uint(csil_v.max_rows_each_side),
    ));
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a JoinNode from a decoded CBOR value tree.
fn csil_dec_join_node(csil_root: &CsilCborValue) -> Result<JoinNode, CsilCborError> {
    let join_key = {
        let csil_field = cbor_require(csil_root, "join_key")?;
        let csil_decode = csil_dec_field_ref;
        csil_decode(csil_field)?
    };
    let max_rows_each_side = {
        let csil_field = cbor_require(csil_root, "max_rows_each_side")?;
        let csil_decode = cbor_as_u64;
        csil_decode(csil_field)?
    };
    let left = {
        let csil_field = cbor_require(csil_root, "left")?;
        let csil_decode = cbor_as_bytes;
        csil_decode(csil_field)?
    };
    let right = {
        let csil_field = cbor_require(csil_root, "right")?;
        let csil_decode = cbor_as_bytes;
        csil_decode(csil_field)?
    };
    Ok(JoinNode {
        join_key,
        max_rows_each_side,
        left,
        right,
    })
}

/// Encode a JoinNode to canonical CSIL CBOR bytes.
pub fn encode_join_node(csil_v: &JoinNode) -> Vec<u8> {
    cbor_encode(&csil_enc_join_node(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a JoinNode.
pub fn decode_join_node(csil_data: &[u8]) -> Result<JoinNode, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_join_node(&csil_root)
}

/// Build the canonical CBOR value tree for a UnionNode.
fn csil_enc_union_node(csil_v: &UnionNode) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(1);
    csil_entries.push((
        cbor_text("union"),
        cbor_enc_array(&csil_v.union, |csil_elem| cbor_bytes(csil_elem)),
    ));
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a UnionNode from a decoded CBOR value tree.
fn csil_dec_union_node(csil_root: &CsilCborValue) -> Result<UnionNode, CsilCborError> {
    let union = {
        let csil_field = cbor_require(csil_root, "union")?;
        let csil_decode = |csil_v| cbor_dec_array(csil_v, cbor_as_bytes);
        csil_decode(csil_field)?
    };
    Ok(UnionNode { union })
}

/// Encode a UnionNode to canonical CSIL CBOR bytes.
pub fn encode_union_node(csil_v: &UnionNode) -> Vec<u8> {
    cbor_encode(&csil_enc_union_node(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a UnionNode.
pub fn decode_union_node(csil_data: &[u8]) -> Result<UnionNode, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_union_node(&csil_root)
}

/// Build the canonical CBOR value tree for a FunnelStep.
fn csil_enc_funnel_step(csil_v: &FunnelStep) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(3);
    csil_entries.push((cbor_text("name"), cbor_text(&csil_v.name)));
    csil_entries.push((cbor_text("match"), cbor_bytes(&csil_v.r#match)));
    if let Some(csil_inner) = &csil_v.exclusion {
        csil_entries.push((cbor_text("exclusion"), cbor_bool(*csil_inner)));
    }
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a FunnelStep from a decoded CBOR value tree.
fn csil_dec_funnel_step(csil_root: &CsilCborValue) -> Result<FunnelStep, CsilCborError> {
    let name = {
        let csil_field = cbor_require(csil_root, "name")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let r#match = {
        let csil_field = cbor_require(csil_root, "match")?;
        let csil_decode = cbor_as_bytes;
        csil_decode(csil_field)?
    };
    let exclusion = match cbor_map_get(csil_root, "exclusion") {
        Some(csil_field) => {
            let csil_decode = cbor_as_bool;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    Ok(FunnelStep {
        name,
        r#match,
        exclusion,
    })
}

/// Encode a FunnelStep to canonical CSIL CBOR bytes.
pub fn encode_funnel_step(csil_v: &FunnelStep) -> Vec<u8> {
    cbor_encode(&csil_enc_funnel_step(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a FunnelStep.
pub fn decode_funnel_step(csil_data: &[u8]) -> Result<FunnelStep, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_funnel_step(&csil_root)
}

/// Build the canonical CBOR value tree for a FunnelQuery.
fn csil_enc_funnel_query(csil_v: &FunnelQuery) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(8);
    csil_entries.push((
        cbor_text("basis"),
        csil_enc_correlation_basis(&csil_v.basis),
    ));
    csil_entries.push((cbor_text("range"), csil_enc_time_range(&csil_v.range)));
    csil_entries.push((
        cbor_text("steps"),
        cbor_enc_array(&csil_v.steps, csil_enc_funnel_step),
    ));
    csil_entries.push((cbor_text("ordered"), cbor_bool(csil_v.ordered)));
    if let Some(csil_inner) = &csil_v.breakdown {
        csil_entries.push((cbor_text("breakdown"), csil_enc_dimension(csil_inner)));
    }
    csil_entries.push((cbor_text("window_ms"), cbor_int(csil_v.window_ms)));
    csil_entries.push((cbor_text("project_id"), cbor_bytes(&csil_v.project_id)));
    if let Some(csil_inner) = &csil_v.resolution {
        csil_entries.push((
            cbor_text("resolution"),
            csil_enc_identity_resolution(csil_inner),
        ));
    }
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a FunnelQuery from a decoded CBOR value tree.
fn csil_dec_funnel_query(csil_root: &CsilCborValue) -> Result<FunnelQuery, CsilCborError> {
    let project_id = {
        let csil_field = cbor_require(csil_root, "project_id")?;
        let csil_decode = cbor_as_bytes;
        csil_decode(csil_field)?
    };
    let range = {
        let csil_field = cbor_require(csil_root, "range")?;
        let csil_decode = csil_dec_time_range;
        csil_decode(csil_field)?
    };
    let steps = {
        let csil_field = cbor_require(csil_root, "steps")?;
        let csil_decode = |csil_v| cbor_dec_array(csil_v, csil_dec_funnel_step);
        csil_decode(csil_field)?
    };
    let window_ms = {
        let csil_field = cbor_require(csil_root, "window_ms")?;
        let csil_decode = cbor_as_i64;
        csil_decode(csil_field)?
    };
    let basis = {
        let csil_field = cbor_require(csil_root, "basis")?;
        let csil_decode = csil_dec_correlation_basis;
        csil_decode(csil_field)?
    };
    let ordered = {
        let csil_field = cbor_require(csil_root, "ordered")?;
        let csil_decode = cbor_as_bool;
        csil_decode(csil_field)?
    };
    let breakdown = match cbor_map_get(csil_root, "breakdown") {
        Some(csil_field) => {
            let csil_decode = csil_dec_dimension;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let resolution = match cbor_map_get(csil_root, "resolution") {
        Some(csil_field) => {
            let csil_decode = csil_dec_identity_resolution;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    Ok(FunnelQuery {
        project_id,
        range,
        steps,
        window_ms,
        basis,
        ordered,
        breakdown,
        resolution,
    })
}

/// Encode a FunnelQuery to canonical CSIL CBOR bytes.
pub fn encode_funnel_query(csil_v: &FunnelQuery) -> Vec<u8> {
    cbor_encode(&csil_enc_funnel_query(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a FunnelQuery.
pub fn decode_funnel_query(csil_data: &[u8]) -> Result<FunnelQuery, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_funnel_query(&csil_root)
}

/// Build the canonical CBOR value tree for a RetentionQuery.
fn csil_enc_retention_query(csil_v: &RetentionQuery) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(8);
    csil_entries.push((cbor_text("range"), csil_enc_time_range(&csil_v.range)));
    csil_entries.push((
        cbor_text("period"),
        csil_enc_retention_query_period(&csil_v.period),
    ));
    csil_entries.push((cbor_text("initial"), cbor_bytes(&csil_v.initial)));
    csil_entries.push((cbor_text("periods"), cbor_uint(csil_v.periods)));
    csil_entries.push((cbor_text("returning"), cbor_bytes(&csil_v.returning)));
    csil_entries.push((cbor_text("project_id"), cbor_bytes(&csil_v.project_id)));
    if let Some(csil_inner) = &csil_v.resolution {
        csil_entries.push((
            cbor_text("resolution"),
            csil_enc_identity_resolution(csil_inner),
        ));
    }
    csil_entries.push((
        cbor_text("first_time_only"),
        cbor_bool(csil_v.first_time_only),
    ));
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a RetentionQuery from a decoded CBOR value tree.
fn csil_dec_retention_query(csil_root: &CsilCborValue) -> Result<RetentionQuery, CsilCborError> {
    let project_id = {
        let csil_field = cbor_require(csil_root, "project_id")?;
        let csil_decode = cbor_as_bytes;
        csil_decode(csil_field)?
    };
    let range = {
        let csil_field = cbor_require(csil_root, "range")?;
        let csil_decode = csil_dec_time_range;
        csil_decode(csil_field)?
    };
    let initial = {
        let csil_field = cbor_require(csil_root, "initial")?;
        let csil_decode = cbor_as_bytes;
        csil_decode(csil_field)?
    };
    let returning = {
        let csil_field = cbor_require(csil_root, "returning")?;
        let csil_decode = cbor_as_bytes;
        csil_decode(csil_field)?
    };
    let period = {
        let csil_field = cbor_require(csil_root, "period")?;
        let csil_decode = csil_dec_retention_query_period;
        csil_decode(csil_field)?
    };
    let periods = {
        let csil_field = cbor_require(csil_root, "periods")?;
        let csil_decode = cbor_as_u64;
        csil_decode(csil_field)?
    };
    let first_time_only = {
        let csil_field = cbor_require(csil_root, "first_time_only")?;
        let csil_decode = cbor_as_bool;
        csil_decode(csil_field)?
    };
    let resolution = match cbor_map_get(csil_root, "resolution") {
        Some(csil_field) => {
            let csil_decode = csil_dec_identity_resolution;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    Ok(RetentionQuery {
        project_id,
        range,
        initial,
        returning,
        period,
        periods,
        first_time_only,
        resolution,
    })
}

/// Encode a RetentionQuery to canonical CSIL CBOR bytes.
pub fn encode_retention_query(csil_v: &RetentionQuery) -> Vec<u8> {
    cbor_encode(&csil_enc_retention_query(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a RetentionQuery.
pub fn decode_retention_query(csil_data: &[u8]) -> Result<RetentionQuery, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_retention_query(&csil_root)
}

/// Build the canonical CBOR value tree for a PathQuery.
fn csil_enc_path_query(csil_v: &PathQuery) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(8);
    csil_entries.push((cbor_text("depth"), cbor_uint(csil_v.depth)));
    csil_entries.push((cbor_text("range"), csil_enc_time_range(&csil_v.range)));
    csil_entries.push((cbor_text("anchor"), cbor_bytes(&csil_v.anchor)));
    csil_entries.push((
        cbor_text("direction"),
        csil_enc_path_query_direction(&csil_v.direction),
    ));
    csil_entries.push((cbor_text("project_id"), cbor_bytes(&csil_v.project_id)));
    if let Some(csil_inner) = &csil_v.resolution {
        csil_entries.push((
            cbor_text("resolution"),
            csil_enc_identity_resolution(csil_inner),
        ));
    }
    csil_entries.push((cbor_text("min_frequency"), cbor_uint(csil_v.min_frequency)));
    csil_entries.push((
        cbor_text("collapse_loops"),
        cbor_bool(csil_v.collapse_loops),
    ));
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a PathQuery from a decoded CBOR value tree.
fn csil_dec_path_query(csil_root: &CsilCborValue) -> Result<PathQuery, CsilCborError> {
    let project_id = {
        let csil_field = cbor_require(csil_root, "project_id")?;
        let csil_decode = cbor_as_bytes;
        csil_decode(csil_field)?
    };
    let range = {
        let csil_field = cbor_require(csil_root, "range")?;
        let csil_decode = csil_dec_time_range;
        csil_decode(csil_field)?
    };
    let anchor = {
        let csil_field = cbor_require(csil_root, "anchor")?;
        let csil_decode = cbor_as_bytes;
        csil_decode(csil_field)?
    };
    let direction = {
        let csil_field = cbor_require(csil_root, "direction")?;
        let csil_decode = csil_dec_path_query_direction;
        csil_decode(csil_field)?
    };
    let depth = {
        let csil_field = cbor_require(csil_root, "depth")?;
        let csil_decode = cbor_as_u64;
        csil_decode(csil_field)?
    };
    let min_frequency = {
        let csil_field = cbor_require(csil_root, "min_frequency")?;
        let csil_decode = cbor_as_u64;
        csil_decode(csil_field)?
    };
    let collapse_loops = {
        let csil_field = cbor_require(csil_root, "collapse_loops")?;
        let csil_decode = cbor_as_bool;
        csil_decode(csil_field)?
    };
    let resolution = match cbor_map_get(csil_root, "resolution") {
        Some(csil_field) => {
            let csil_decode = csil_dec_identity_resolution;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    Ok(PathQuery {
        project_id,
        range,
        anchor,
        direction,
        depth,
        min_frequency,
        collapse_loops,
        resolution,
    })
}

/// Encode a PathQuery to canonical CSIL CBOR bytes.
pub fn encode_path_query(csil_v: &PathQuery) -> Vec<u8> {
    cbor_encode(&csil_enc_path_query(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a PathQuery.
pub fn decode_path_query(csil_data: &[u8]) -> Result<PathQuery, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_path_query(&csil_root)
}

/// Build the canonical CBOR value tree for a TraceQuery.
fn csil_enc_trace_query(csil_v: &TraceQuery) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(2);
    csil_entries.push((cbor_text("trace_id"), cbor_bytes(&csil_v.trace_id)));
    csil_entries.push((cbor_text("project_id"), cbor_bytes(&csil_v.project_id)));
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a TraceQuery from a decoded CBOR value tree.
fn csil_dec_trace_query(csil_root: &CsilCborValue) -> Result<TraceQuery, CsilCborError> {
    let project_id = {
        let csil_field = cbor_require(csil_root, "project_id")?;
        let csil_decode = cbor_as_bytes;
        csil_decode(csil_field)?
    };
    let trace_id = {
        let csil_field = cbor_require(csil_root, "trace_id")?;
        let csil_decode = cbor_as_bytes;
        csil_decode(csil_field)?
    };
    Ok(TraceQuery {
        project_id,
        trace_id,
    })
}

/// Encode a TraceQuery to canonical CSIL CBOR bytes.
pub fn encode_trace_query(csil_v: &TraceQuery) -> Vec<u8> {
    cbor_encode(&csil_enc_trace_query(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a TraceQuery.
pub fn decode_trace_query(csil_data: &[u8]) -> Result<TraceQuery, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_trace_query(&csil_root)
}

/// Build the canonical CBOR value tree for a TimelineQuery.
fn csil_enc_timeline_query(csil_v: &TimelineQuery) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(8);
    csil_entries.push((
        cbor_text("kinds"),
        cbor_enc_array(&csil_v.kinds, csil_enc_telemetry_kind),
    ));
    csil_entries.push((cbor_text("limit"), cbor_uint(csil_v.limit)));
    csil_entries.push((cbor_text("range"), csil_enc_time_range(&csil_v.range)));
    if let Some(csil_inner) = &csil_v.cursor {
        csil_entries.push((cbor_text("cursor"), cbor_bytes(csil_inner)));
    }
    csil_entries.push((cbor_text("project_id"), cbor_bytes(&csil_v.project_id)));
    if let Some(csil_inner) = &csil_v.resolution {
        csil_entries.push((
            cbor_text("resolution"),
            csil_enc_identity_resolution(csil_inner),
        ));
    }
    if let Some(csil_inner) = &csil_v.session_id {
        csil_entries.push((cbor_text("session_id"), cbor_text(csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.end_user_id {
        csil_entries.push((cbor_text("end_user_id"), cbor_text(csil_inner)));
    }
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a TimelineQuery from a decoded CBOR value tree.
fn csil_dec_timeline_query(csil_root: &CsilCborValue) -> Result<TimelineQuery, CsilCborError> {
    let project_id = {
        let csil_field = cbor_require(csil_root, "project_id")?;
        let csil_decode = cbor_as_bytes;
        csil_decode(csil_field)?
    };
    let range = {
        let csil_field = cbor_require(csil_root, "range")?;
        let csil_decode = csil_dec_time_range;
        csil_decode(csil_field)?
    };
    let end_user_id = match cbor_map_get(csil_root, "end_user_id") {
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
    let kinds = {
        let csil_field = cbor_require(csil_root, "kinds")?;
        let csil_decode = |csil_v| cbor_dec_array(csil_v, csil_dec_telemetry_kind);
        csil_decode(csil_field)?
    };
    let limit = {
        let csil_field = cbor_require(csil_root, "limit")?;
        let csil_decode = cbor_as_u64;
        csil_decode(csil_field)?
    };
    let cursor = match cbor_map_get(csil_root, "cursor") {
        Some(csil_field) => {
            let csil_decode = cbor_as_bytes;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let resolution = match cbor_map_get(csil_root, "resolution") {
        Some(csil_field) => {
            let csil_decode = csil_dec_identity_resolution;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    Ok(TimelineQuery {
        project_id,
        range,
        end_user_id,
        session_id,
        kinds,
        limit,
        cursor,
        resolution,
    })
}

/// Encode a TimelineQuery to canonical CSIL CBOR bytes.
pub fn encode_timeline_query(csil_v: &TimelineQuery) -> Vec<u8> {
    cbor_encode(&csil_enc_timeline_query(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a TimelineQuery.
pub fn decode_timeline_query(csil_data: &[u8]) -> Result<TimelineQuery, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_timeline_query(&csil_root)
}

/// Build the canonical CBOR value tree for a AttributionQuery.
fn csil_enc_attribution_query(csil_v: &AttributionQuery) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(8);
    csil_entries.push((
        cbor_text("model"),
        csil_enc_attribution_model(&csil_v.model),
    ));
    csil_entries.push((cbor_text("range"), csil_enc_time_range(&csil_v.range)));
    if let Some(csil_inner) = &csil_v.breakdown {
        csil_entries.push((cbor_text("breakdown"), csil_enc_dimension(csil_inner)));
    }
    csil_entries.push((cbor_text("project_id"), cbor_bytes(&csil_v.project_id)));
    if let Some(csil_inner) = &csil_v.resolution {
        csil_entries.push((
            cbor_text("resolution"),
            csil_enc_identity_resolution(csil_inner),
        ));
    }
    csil_entries.push((cbor_text("lookback_ms"), cbor_int(csil_v.lookback_ms)));
    if let Some(csil_inner) = &csil_v.touch_filter {
        csil_entries.push((cbor_text("touch_filter"), cbor_bytes(csil_inner)));
    }
    csil_entries.push((
        cbor_text("conversion_goal"),
        cbor_text(&csil_v.conversion_goal),
    ));
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a AttributionQuery from a decoded CBOR value tree.
fn csil_dec_attribution_query(
    csil_root: &CsilCborValue,
) -> Result<AttributionQuery, CsilCborError> {
    let project_id = {
        let csil_field = cbor_require(csil_root, "project_id")?;
        let csil_decode = cbor_as_bytes;
        csil_decode(csil_field)?
    };
    let range = {
        let csil_field = cbor_require(csil_root, "range")?;
        let csil_decode = csil_dec_time_range;
        csil_decode(csil_field)?
    };
    let conversion_goal = {
        let csil_field = cbor_require(csil_root, "conversion_goal")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let model = {
        let csil_field = cbor_require(csil_root, "model")?;
        let csil_decode = csil_dec_attribution_model;
        csil_decode(csil_field)?
    };
    let lookback_ms = {
        let csil_field = cbor_require(csil_root, "lookback_ms")?;
        let csil_decode = cbor_as_i64;
        csil_decode(csil_field)?
    };
    let touch_filter = match cbor_map_get(csil_root, "touch_filter") {
        Some(csil_field) => {
            let csil_decode = cbor_as_bytes;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let breakdown = match cbor_map_get(csil_root, "breakdown") {
        Some(csil_field) => {
            let csil_decode = csil_dec_dimension;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let resolution = match cbor_map_get(csil_root, "resolution") {
        Some(csil_field) => {
            let csil_decode = csil_dec_identity_resolution;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    Ok(AttributionQuery {
        project_id,
        range,
        conversion_goal,
        model,
        lookback_ms,
        touch_filter,
        breakdown,
        resolution,
    })
}

/// Encode a AttributionQuery to canonical CSIL CBOR bytes.
pub fn encode_attribution_query(csil_v: &AttributionQuery) -> Vec<u8> {
    cbor_encode(&csil_enc_attribution_query(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a AttributionQuery.
pub fn decode_attribution_query(csil_data: &[u8]) -> Result<AttributionQuery, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_attribution_query(&csil_root)
}

/// Build the canonical CBOR value tree for a CampaignSummaryQuery.
fn csil_enc_campaign_summary_query(csil_v: &CampaignSummaryQuery) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(8);
    csil_entries.push((
        cbor_text("model"),
        csil_enc_attribution_model(&csil_v.model),
    ));
    csil_entries.push((cbor_text("range"), csil_enc_time_range(&csil_v.range)));
    if let Some(csil_inner) = &csil_v.dimension {
        csil_entries.push((
            cbor_text("dimension"),
            csil_enc_campaign_summary_query_dimension(csil_inner),
        ));
    }
    csil_entries.push((cbor_text("project_id"), cbor_bytes(&csil_v.project_id)));
    if let Some(csil_inner) = &csil_v.resolution {
        csil_entries.push((
            cbor_text("resolution"),
            csil_enc_identity_resolution(csil_inner),
        ));
    }
    csil_entries.push((cbor_text("lookback_ms"), cbor_int(csil_v.lookback_ms)));
    if let Some(csil_inner) = &csil_v.touch_filter {
        csil_entries.push((cbor_text("touch_filter"), cbor_bytes(csil_inner)));
    }
    csil_entries.push((
        cbor_text("conversion_goal"),
        cbor_text(&csil_v.conversion_goal),
    ));
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a CampaignSummaryQuery from a decoded CBOR value tree.
fn csil_dec_campaign_summary_query(
    csil_root: &CsilCborValue,
) -> Result<CampaignSummaryQuery, CsilCborError> {
    let project_id = {
        let csil_field = cbor_require(csil_root, "project_id")?;
        let csil_decode = cbor_as_bytes;
        csil_decode(csil_field)?
    };
    let range = {
        let csil_field = cbor_require(csil_root, "range")?;
        let csil_decode = csil_dec_time_range;
        csil_decode(csil_field)?
    };
    let conversion_goal = {
        let csil_field = cbor_require(csil_root, "conversion_goal")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let model = {
        let csil_field = cbor_require(csil_root, "model")?;
        let csil_decode = csil_dec_attribution_model;
        csil_decode(csil_field)?
    };
    let lookback_ms = {
        let csil_field = cbor_require(csil_root, "lookback_ms")?;
        let csil_decode = cbor_as_i64;
        csil_decode(csil_field)?
    };
    let dimension = match cbor_map_get(csil_root, "dimension") {
        Some(csil_field) => {
            let csil_decode = csil_dec_campaign_summary_query_dimension;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let touch_filter = match cbor_map_get(csil_root, "touch_filter") {
        Some(csil_field) => {
            let csil_decode = cbor_as_bytes;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let resolution = match cbor_map_get(csil_root, "resolution") {
        Some(csil_field) => {
            let csil_decode = csil_dec_identity_resolution;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    Ok(CampaignSummaryQuery {
        project_id,
        range,
        conversion_goal,
        model,
        lookback_ms,
        dimension,
        touch_filter,
        resolution,
    })
}

/// Encode a CampaignSummaryQuery to canonical CSIL CBOR bytes.
pub fn encode_campaign_summary_query(csil_v: &CampaignSummaryQuery) -> Vec<u8> {
    cbor_encode(&csil_enc_campaign_summary_query(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a CampaignSummaryQuery.
pub fn decode_campaign_summary_query(
    csil_data: &[u8],
) -> Result<CampaignSummaryQuery, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_campaign_summary_query(&csil_root)
}

/// Build the canonical CBOR value tree for a QueryBudget.
fn csil_enc_query_budget(csil_v: &QueryBudget) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(4);
    if let Some(csil_inner) = &csil_v.max_rows {
        csil_entries.push((cbor_text("max_rows"), cbor_uint(*csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.deadline_ms {
        csil_entries.push((cbor_text("deadline_ms"), cbor_int(*csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.max_scanned_bytes {
        csil_entries.push((cbor_text("max_scanned_bytes"), cbor_uint(*csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.max_scanned_segments {
        csil_entries.push((cbor_text("max_scanned_segments"), cbor_uint(*csil_inner)));
    }
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a QueryBudget from a decoded CBOR value tree.
fn csil_dec_query_budget(csil_root: &CsilCborValue) -> Result<QueryBudget, CsilCborError> {
    let deadline_ms = match cbor_map_get(csil_root, "deadline_ms") {
        Some(csil_field) => {
            let csil_decode = cbor_as_i64;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let max_scanned_bytes = match cbor_map_get(csil_root, "max_scanned_bytes") {
        Some(csil_field) => {
            let csil_decode = cbor_as_u64;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let max_scanned_segments = match cbor_map_get(csil_root, "max_scanned_segments") {
        Some(csil_field) => {
            let csil_decode = cbor_as_u64;
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
    Ok(QueryBudget {
        deadline_ms,
        max_scanned_bytes,
        max_scanned_segments,
        max_rows,
    })
}

/// Encode a QueryBudget to canonical CSIL CBOR bytes.
pub fn encode_query_budget(csil_v: &QueryBudget) -> Vec<u8> {
    cbor_encode(&csil_enc_query_budget(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a QueryBudget.
pub fn decode_query_budget(csil_data: &[u8]) -> Result<QueryBudget, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_query_budget(&csil_root)
}

/// Build the canonical CBOR value tree for a QueryRequest.
fn csil_enc_query_request(csil_v: &QueryRequest) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(15);
    csil_entries.push((cbor_text("form"), csil_enc_query_form(&csil_v.form)));
    if let Some(csil_inner) = &csil_v.node {
        csil_entries.push((cbor_text("node"), cbor_bytes(csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.path {
        csil_entries.push((cbor_text("path"), csil_enc_path_query(csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.trace {
        csil_entries.push((cbor_text("trace"), csil_enc_trace_query(csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.budget {
        csil_entries.push((cbor_text("budget"), csil_enc_query_budget(csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.funnel {
        csil_entries.push((cbor_text("funnel"), csil_enc_funnel_query(csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.timeline {
        csil_entries.push((cbor_text("timeline"), csil_enc_timeline_query(csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.retention {
        csil_entries.push((cbor_text("retention"), csil_enc_retention_query(csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.attribution {
        csil_entries.push((
            cbor_text("attribution"),
            csil_enc_attribution_query(csil_inner),
        ));
    }
    csil_entries.push((
        cbor_text("consistency"),
        csil_enc_consistency(&csil_v.consistency),
    ));
    csil_entries.push((cbor_text("allow_partial"), cbor_bool(csil_v.allow_partial)));
    csil_entries.push((
        cbor_text("algebra_version"),
        cbor_uint(csil_v.algebra_version),
    ));
    if let Some(csil_inner) = &csil_v.campaign_summary {
        csil_entries.push((
            cbor_text("campaign_summary"),
            csil_enc_campaign_summary_query(csil_inner),
        ));
    }
    if let Some(csil_inner) = &csil_v.comparison_range {
        csil_entries.push((
            cbor_text("comparison_range"),
            csil_enc_time_range(csil_inner),
        ));
    }
    if let Some(csil_inner) = &csil_v.max_staleness_ms {
        csil_entries.push((cbor_text("max_staleness_ms"), cbor_int(*csil_inner)));
    }
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a QueryRequest from a decoded CBOR value tree.
fn csil_dec_query_request(csil_root: &CsilCborValue) -> Result<QueryRequest, CsilCborError> {
    let algebra_version = {
        let csil_field = cbor_require(csil_root, "algebra_version")?;
        let csil_decode = cbor_as_u64;
        csil_decode(csil_field)?
    };
    let consistency = {
        let csil_field = cbor_require(csil_root, "consistency")?;
        let csil_decode = csil_dec_consistency;
        csil_decode(csil_field)?
    };
    let max_staleness_ms = match cbor_map_get(csil_root, "max_staleness_ms") {
        Some(csil_field) => {
            let csil_decode = cbor_as_i64;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let budget = match cbor_map_get(csil_root, "budget") {
        Some(csil_field) => {
            let csil_decode = csil_dec_query_budget;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let allow_partial = {
        let csil_field = cbor_require(csil_root, "allow_partial")?;
        let csil_decode = cbor_as_bool;
        csil_decode(csil_field)?
    };
    let comparison_range = match cbor_map_get(csil_root, "comparison_range") {
        Some(csil_field) => {
            let csil_decode = csil_dec_time_range;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let form = {
        let csil_field = cbor_require(csil_root, "form")?;
        let csil_decode = csil_dec_query_form;
        csil_decode(csil_field)?
    };
    let node = match cbor_map_get(csil_root, "node") {
        Some(csil_field) => {
            let csil_decode = cbor_as_bytes;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let funnel = match cbor_map_get(csil_root, "funnel") {
        Some(csil_field) => {
            let csil_decode = csil_dec_funnel_query;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let retention = match cbor_map_get(csil_root, "retention") {
        Some(csil_field) => {
            let csil_decode = csil_dec_retention_query;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let path = match cbor_map_get(csil_root, "path") {
        Some(csil_field) => {
            let csil_decode = csil_dec_path_query;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let trace = match cbor_map_get(csil_root, "trace") {
        Some(csil_field) => {
            let csil_decode = csil_dec_trace_query;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let timeline = match cbor_map_get(csil_root, "timeline") {
        Some(csil_field) => {
            let csil_decode = csil_dec_timeline_query;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let attribution = match cbor_map_get(csil_root, "attribution") {
        Some(csil_field) => {
            let csil_decode = csil_dec_attribution_query;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let campaign_summary = match cbor_map_get(csil_root, "campaign_summary") {
        Some(csil_field) => {
            let csil_decode = csil_dec_campaign_summary_query;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    Ok(QueryRequest {
        algebra_version,
        consistency,
        max_staleness_ms,
        budget,
        allow_partial,
        comparison_range,
        form,
        node,
        funnel,
        retention,
        path,
        trace,
        timeline,
        attribution,
        campaign_summary,
    })
}

/// Encode a QueryRequest to canonical CSIL CBOR bytes.
pub fn encode_query_request(csil_v: &QueryRequest) -> Vec<u8> {
    cbor_encode(&csil_enc_query_request(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a QueryRequest.
pub fn decode_query_request(csil_data: &[u8]) -> Result<QueryRequest, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_query_request(&csil_root)
}

/// Build the canonical CBOR value tree for a Exactness.
fn csil_enc_exactness(csil_v: &Exactness) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(4);
    csil_entries.push((cbor_text("alias"), cbor_text(&csil_v.alias)));
    csil_entries.push((cbor_text("exact"), cbor_bool(csil_v.exact)));
    if let Some(csil_inner) = &csil_v.method {
        csil_entries.push((cbor_text("method"), cbor_text(csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.error_bound {
        csil_entries.push((cbor_text("error_bound"), cbor_float(*csil_inner)));
    }
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a Exactness from a decoded CBOR value tree.
fn csil_dec_exactness(csil_root: &CsilCborValue) -> Result<Exactness, CsilCborError> {
    let alias = {
        let csil_field = cbor_require(csil_root, "alias")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let exact = {
        let csil_field = cbor_require(csil_root, "exact")?;
        let csil_decode = cbor_as_bool;
        csil_decode(csil_field)?
    };
    let method = match cbor_map_get(csil_root, "method") {
        Some(csil_field) => {
            let csil_decode = cbor_as_text;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let error_bound = match cbor_map_get(csil_root, "error_bound") {
        Some(csil_field) => {
            let csil_decode = cbor_as_f64;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    Ok(Exactness {
        alias,
        exact,
        method,
        error_bound,
    })
}

/// Encode a Exactness to canonical CSIL CBOR bytes.
pub fn encode_exactness(csil_v: &Exactness) -> Vec<u8> {
    cbor_encode(&csil_enc_exactness(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a Exactness.
pub fn decode_exactness(csil_data: &[u8]) -> Result<Exactness, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_exactness(&csil_root)
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

/// Build the canonical CBOR value tree for a ResultMetadata.
fn csil_enc_result_metadata(csil_v: &ResultMetadata) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(13);
    if let Some(csil_inner) = &csil_v.missing {
        csil_entries.push((
            cbor_text("missing"),
            cbor_enc_array(csil_inner, csil_enc_missing_range),
        ));
    }
    csil_entries.push((cbor_text("complete"), cbor_bool(csil_v.complete)));
    if let Some(csil_inner) = &csil_v.warnings {
        csil_entries.push((
            cbor_text("warnings"),
            cbor_enc_array(csil_inner, |csil_elem| cbor_text(csil_elem)),
        ));
    }
    csil_entries.push((
        cbor_text("exactness"),
        cbor_enc_array(&csil_v.exactness, csil_enc_exactness),
    ));
    if let Some(csil_inner) = &csil_v.cold_bytes {
        csil_entries.push((cbor_text("cold_bytes"), cbor_uint(*csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.next_cursor {
        csil_entries.push((cbor_text("next_cursor"), cbor_bytes(csil_inner)));
    }
    csil_entries.push((cbor_text("freshness_ms"), cbor_int(csil_v.freshness_ms)));
    csil_entries.push((cbor_text("scanned_bytes"), cbor_uint(csil_v.scanned_bytes)));
    csil_entries.push((
        cbor_text("algebra_version"),
        cbor_uint(csil_v.algebra_version),
    ));
    csil_entries.push((
        cbor_text("commit_watermark"),
        cbor_uint(csil_v.commit_watermark),
    ));
    csil_entries.push((
        cbor_text("scanned_segments"),
        cbor_uint(csil_v.scanned_segments),
    ));
    csil_entries.push((
        cbor_text("tombstone_generation"),
        cbor_uint(csil_v.tombstone_generation),
    ));
    if let Some(csil_inner) = &csil_v.applied_retention_class {
        csil_entries.push((
            cbor_text("applied_retention_class"),
            csil_enc_retention_class(csil_inner),
        ));
    }
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a ResultMetadata from a decoded CBOR value tree.
fn csil_dec_result_metadata(csil_root: &CsilCborValue) -> Result<ResultMetadata, CsilCborError> {
    let algebra_version = {
        let csil_field = cbor_require(csil_root, "algebra_version")?;
        let csil_decode = cbor_as_u64;
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
    let exactness = {
        let csil_field = cbor_require(csil_root, "exactness")?;
        let csil_decode = |csil_v| cbor_dec_array(csil_v, csil_dec_exactness);
        csil_decode(csil_field)?
    };
    let scanned_bytes = {
        let csil_field = cbor_require(csil_root, "scanned_bytes")?;
        let csil_decode = cbor_as_u64;
        csil_decode(csil_field)?
    };
    let scanned_segments = {
        let csil_field = cbor_require(csil_root, "scanned_segments")?;
        let csil_decode = cbor_as_u64;
        csil_decode(csil_field)?
    };
    let cold_bytes = match cbor_map_get(csil_root, "cold_bytes") {
        Some(csil_field) => {
            let csil_decode = cbor_as_u64;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let tombstone_generation = {
        let csil_field = cbor_require(csil_root, "tombstone_generation")?;
        let csil_decode = cbor_as_u64;
        csil_decode(csil_field)?
    };
    let applied_retention_class = match cbor_map_get(csil_root, "applied_retention_class") {
        Some(csil_field) => {
            let csil_decode = csil_dec_retention_class;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let warnings = match cbor_map_get(csil_root, "warnings") {
        Some(csil_field) => {
            let csil_decode = |csil_v| cbor_dec_array(csil_v, cbor_as_text);
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let next_cursor = match cbor_map_get(csil_root, "next_cursor") {
        Some(csil_field) => {
            let csil_decode = cbor_as_bytes;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    Ok(ResultMetadata {
        algebra_version,
        commit_watermark,
        freshness_ms,
        complete,
        missing,
        exactness,
        scanned_bytes,
        scanned_segments,
        cold_bytes,
        tombstone_generation,
        applied_retention_class,
        warnings,
        next_cursor,
    })
}

/// Encode a ResultMetadata to canonical CSIL CBOR bytes.
pub fn encode_result_metadata(csil_v: &ResultMetadata) -> Vec<u8> {
    cbor_encode(&csil_enc_result_metadata(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a ResultMetadata.
pub fn decode_result_metadata(csil_data: &[u8]) -> Result<ResultMetadata, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_result_metadata(&csil_root)
}

/// Build the canonical CBOR value tree for a ResultRow.
fn csil_enc_result_row(csil_v: &ResultRow) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(1);
    csil_entries.push((
        cbor_text("values"),
        cbor_enc_array(&csil_v.values, csil_enc_typed_value),
    ));
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a ResultRow from a decoded CBOR value tree.
fn csil_dec_result_row(csil_root: &CsilCborValue) -> Result<ResultRow, CsilCborError> {
    let values = {
        let csil_field = cbor_require(csil_root, "values")?;
        let csil_decode = |csil_v| cbor_dec_array(csil_v, csil_dec_typed_value);
        csil_decode(csil_field)?
    };
    Ok(ResultRow { values })
}

/// Encode a ResultRow to canonical CSIL CBOR bytes.
pub fn encode_result_row(csil_v: &ResultRow) -> Vec<u8> {
    cbor_encode(&csil_enc_result_row(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a ResultRow.
pub fn decode_result_row(csil_data: &[u8]) -> Result<ResultRow, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_result_row(&csil_root)
}

/// Build the canonical CBOR value tree for a QueryResponse.
fn csil_enc_query_response(csil_v: &QueryResponse) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(3);
    csil_entries.push((
        cbor_text("rows"),
        cbor_enc_array(&csil_v.rows, csil_enc_result_row),
    ));
    csil_entries.push((
        cbor_text("columns"),
        cbor_enc_array(&csil_v.columns, |csil_elem| cbor_text(csil_elem)),
    ));
    csil_entries.push((
        cbor_text("metadata"),
        csil_enc_result_metadata(&csil_v.metadata),
    ));
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a QueryResponse from a decoded CBOR value tree.
fn csil_dec_query_response(csil_root: &CsilCborValue) -> Result<QueryResponse, CsilCborError> {
    let columns = {
        let csil_field = cbor_require(csil_root, "columns")?;
        let csil_decode = |csil_v| cbor_dec_array(csil_v, cbor_as_text);
        csil_decode(csil_field)?
    };
    let rows = {
        let csil_field = cbor_require(csil_root, "rows")?;
        let csil_decode = |csil_v| cbor_dec_array(csil_v, csil_dec_result_row);
        csil_decode(csil_field)?
    };
    let metadata = {
        let csil_field = cbor_require(csil_root, "metadata")?;
        let csil_decode = csil_dec_result_metadata;
        csil_decode(csil_field)?
    };
    Ok(QueryResponse {
        columns,
        rows,
        metadata,
    })
}

/// Encode a QueryResponse to canonical CSIL CBOR bytes.
pub fn encode_query_response(csil_v: &QueryResponse) -> Vec<u8> {
    cbor_encode(&csil_enc_query_response(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a QueryResponse.
pub fn decode_query_response(csil_data: &[u8]) -> Result<QueryResponse, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_query_response(&csil_root)
}

/// Build the canonical CBOR value tree for a ThresholdCondition.
fn csil_enc_threshold_condition(csil_v: &ThresholdCondition) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(4);
    csil_entries.push((cbor_text("alias"), cbor_text(&csil_v.alias)));
    csil_entries.push((cbor_text("value"), cbor_float(csil_v.value)));
    csil_entries.push((cbor_text("compare"), csil_enc_compare_op(&csil_v.compare)));
    if let Some(csil_inner) = &csil_v.sustained_ms {
        csil_entries.push((cbor_text("sustained_ms"), cbor_int(*csil_inner)));
    }
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a ThresholdCondition from a decoded CBOR value tree.
fn csil_dec_threshold_condition(
    csil_root: &CsilCborValue,
) -> Result<ThresholdCondition, CsilCborError> {
    let alias = {
        let csil_field = cbor_require(csil_root, "alias")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let compare = {
        let csil_field = cbor_require(csil_root, "compare")?;
        let csil_decode = csil_dec_compare_op;
        csil_decode(csil_field)?
    };
    let value = {
        let csil_field = cbor_require(csil_root, "value")?;
        let csil_decode = cbor_as_f64;
        csil_decode(csil_field)?
    };
    let sustained_ms = match cbor_map_get(csil_root, "sustained_ms") {
        Some(csil_field) => {
            let csil_decode = cbor_as_i64;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    Ok(ThresholdCondition {
        alias,
        compare,
        value,
        sustained_ms,
    })
}

/// Encode a ThresholdCondition to canonical CSIL CBOR bytes.
pub fn encode_threshold_condition(csil_v: &ThresholdCondition) -> Vec<u8> {
    cbor_encode(&csil_enc_threshold_condition(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a ThresholdCondition.
pub fn decode_threshold_condition(csil_data: &[u8]) -> Result<ThresholdCondition, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_threshold_condition(&csil_root)
}

/// Build the canonical CBOR value tree for a AbsenceCondition.
fn csil_enc_absence_condition(csil_v: &AbsenceCondition) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(1);
    csil_entries.push((cbor_text("for_ms"), cbor_int(csil_v.for_ms)));
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a AbsenceCondition from a decoded CBOR value tree.
fn csil_dec_absence_condition(
    csil_root: &CsilCborValue,
) -> Result<AbsenceCondition, CsilCborError> {
    let for_ms = {
        let csil_field = cbor_require(csil_root, "for_ms")?;
        let csil_decode = cbor_as_i64;
        csil_decode(csil_field)?
    };
    Ok(AbsenceCondition { for_ms })
}

/// Encode a AbsenceCondition to canonical CSIL CBOR bytes.
pub fn encode_absence_condition(csil_v: &AbsenceCondition) -> Vec<u8> {
    cbor_encode(&csil_enc_absence_condition(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a AbsenceCondition.
pub fn decode_absence_condition(csil_data: &[u8]) -> Result<AbsenceCondition, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_absence_condition(&csil_root)
}

/// Build the canonical CBOR value tree for a AlertRule.
fn csil_enc_alert_rule(csil_v: &AlertRule) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(15);
    csil_entries.push((cbor_text("name"), cbor_text(&csil_v.name)));
    csil_entries.push((cbor_text("query"), csil_enc_query_request(&csil_v.query)));
    csil_entries.push((
        cbor_text("notify"),
        cbor_enc_array(&csil_v.notify, csil_enc_notification_target),
    ));
    if let Some(csil_inner) = &csil_v.absence {
        csil_entries.push((cbor_text("absence"), csil_enc_absence_condition(csil_inner)));
    }
    csil_entries.push((cbor_text("enabled"), cbor_bool(csil_v.enabled)));
    csil_entries.push((cbor_text("rule_id"), cbor_text(&csil_v.rule_id)));
    if let Some(csil_inner) = &csil_v.threshold {
        csil_entries.push((
            cbor_text("threshold"),
            csil_enc_threshold_condition(csil_inner),
        ));
    }
    csil_entries.push((cbor_text("project_id"), cbor_bytes(&csil_v.project_id)));
    if let Some(csil_inner) = &csil_v.updated_at {
        csil_entries.push((cbor_text("updated_at"), cbor_int(*csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.updated_by {
        csil_entries.push((cbor_text("updated_by"), cbor_text(csil_inner)));
    }
    csil_entries.push((cbor_text("interval_ms"), cbor_int(csil_v.interval_ms)));
    if let Some(csil_inner) = &csil_v.silence_reason {
        csil_entries.push((cbor_text("silence_reason"), cbor_text(csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.silenced_until {
        csil_entries.push((cbor_text("silenced_until"), cbor_int(*csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.disabled_reason {
        csil_entries.push((cbor_text("disabled_reason"), cbor_text(csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.escalate_after_ms {
        csil_entries.push((cbor_text("escalate_after_ms"), cbor_int(*csil_inner)));
    }
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a AlertRule from a decoded CBOR value tree.
fn csil_dec_alert_rule(csil_root: &CsilCborValue) -> Result<AlertRule, CsilCborError> {
    let rule_id = {
        let csil_field = cbor_require(csil_root, "rule_id")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let name = {
        let csil_field = cbor_require(csil_root, "name")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let project_id = {
        let csil_field = cbor_require(csil_root, "project_id")?;
        let csil_decode = cbor_as_bytes;
        csil_decode(csil_field)?
    };
    let query = {
        let csil_field = cbor_require(csil_root, "query")?;
        let csil_decode = csil_dec_query_request;
        csil_decode(csil_field)?
    };
    let interval_ms = {
        let csil_field = cbor_require(csil_root, "interval_ms")?;
        let csil_decode = cbor_as_i64;
        csil_decode(csil_field)?
    };
    let threshold = match cbor_map_get(csil_root, "threshold") {
        Some(csil_field) => {
            let csil_decode = csil_dec_threshold_condition;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let absence = match cbor_map_get(csil_root, "absence") {
        Some(csil_field) => {
            let csil_decode = csil_dec_absence_condition;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let notify = {
        let csil_field = cbor_require(csil_root, "notify")?;
        let csil_decode = |csil_v| cbor_dec_array(csil_v, csil_dec_notification_target);
        csil_decode(csil_field)?
    };
    let enabled = {
        let csil_field = cbor_require(csil_root, "enabled")?;
        let csil_decode = cbor_as_bool;
        csil_decode(csil_field)?
    };
    let escalate_after_ms = match cbor_map_get(csil_root, "escalate_after_ms") {
        Some(csil_field) => {
            let csil_decode = cbor_as_i64;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let silenced_until = match cbor_map_get(csil_root, "silenced_until") {
        Some(csil_field) => {
            let csil_decode = cbor_as_i64;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let silence_reason = match cbor_map_get(csil_root, "silence_reason") {
        Some(csil_field) => {
            let csil_decode = cbor_as_text;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let disabled_reason = match cbor_map_get(csil_root, "disabled_reason") {
        Some(csil_field) => {
            let csil_decode = cbor_as_text;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let updated_at = match cbor_map_get(csil_root, "updated_at") {
        Some(csil_field) => {
            let csil_decode = cbor_as_i64;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let updated_by = match cbor_map_get(csil_root, "updated_by") {
        Some(csil_field) => {
            let csil_decode = cbor_as_text;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    Ok(AlertRule {
        rule_id,
        name,
        project_id,
        query,
        interval_ms,
        threshold,
        absence,
        notify,
        enabled,
        escalate_after_ms,
        silenced_until,
        silence_reason,
        disabled_reason,
        updated_at,
        updated_by,
    })
}

/// Encode a AlertRule to canonical CSIL CBOR bytes.
pub fn encode_alert_rule(csil_v: &AlertRule) -> Vec<u8> {
    cbor_encode(&csil_enc_alert_rule(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a AlertRule.
pub fn decode_alert_rule(csil_data: &[u8]) -> Result<AlertRule, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_alert_rule(&csil_root)
}

/// Build the canonical CBOR value tree for a NotificationTarget.
fn csil_enc_notification_target(csil_v: &NotificationTarget) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(3);
    if let Some(csil_inner) = &csil_v.url {
        csil_entries.push((cbor_text("url"), cbor_text(csil_inner)));
    }
    csil_entries.push((
        cbor_text("kind"),
        csil_enc_notification_target_kind(&csil_v.kind),
    ));
    if let Some(csil_inner) = &csil_v.secret_ref {
        csil_entries.push((cbor_text("secret_ref"), cbor_text(csil_inner)));
    }
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a NotificationTarget from a decoded CBOR value tree.
fn csil_dec_notification_target(
    csil_root: &CsilCborValue,
) -> Result<NotificationTarget, CsilCborError> {
    let kind = {
        let csil_field = cbor_require(csil_root, "kind")?;
        let csil_decode = csil_dec_notification_target_kind;
        csil_decode(csil_field)?
    };
    let url = match cbor_map_get(csil_root, "url") {
        Some(csil_field) => {
            let csil_decode = cbor_as_text;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let secret_ref = match cbor_map_get(csil_root, "secret_ref") {
        Some(csil_field) => {
            let csil_decode = cbor_as_text;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    Ok(NotificationTarget {
        kind,
        url,
        secret_ref,
    })
}

/// Encode a NotificationTarget to canonical CSIL CBOR bytes.
pub fn encode_notification_target(csil_v: &NotificationTarget) -> Vec<u8> {
    cbor_encode(&csil_enc_notification_target(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a NotificationTarget.
pub fn decode_notification_target(csil_data: &[u8]) -> Result<NotificationTarget, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_notification_target(&csil_root)
}

/// Build the canonical CBOR value tree for a AlertInstance.
fn csil_enc_alert_instance(csil_v: &AlertInstance) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(11);
    csil_entries.push((cbor_text("since"), cbor_int(csil_v.since)));
    csil_entries.push((cbor_text("state"), csil_enc_alert_state(&csil_v.state)));
    if let Some(csil_inner) = &csil_v.reason {
        csil_entries.push((cbor_text("reason"), cbor_text(csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.outcome {
        csil_entries.push((cbor_text("outcome"), csil_enc_alert_outcome(csil_inner)));
    }
    csil_entries.push((cbor_text("rule_id"), cbor_text(&csil_v.rule_id)));
    if let Some(csil_inner) = &csil_v.holding_ms {
        csil_entries.push((cbor_text("holding_ms"), cbor_int(*csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.observed_value {
        csil_entries.push((cbor_text("observed_value"), cbor_float(*csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.commit_watermark {
        csil_entries.push((cbor_text("commit_watermark"), cbor_uint(*csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.last_notified_at {
        csil_entries.push((cbor_text("last_notified_at"), cbor_int(*csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.last_evaluated_at {
        csil_entries.push((cbor_text("last_evaluated_at"), cbor_int(*csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.notifications_sent {
        csil_entries.push((cbor_text("notifications_sent"), cbor_uint(*csil_inner)));
    }
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a AlertInstance from a decoded CBOR value tree.
fn csil_dec_alert_instance(csil_root: &CsilCborValue) -> Result<AlertInstance, CsilCborError> {
    let rule_id = {
        let csil_field = cbor_require(csil_root, "rule_id")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let state = {
        let csil_field = cbor_require(csil_root, "state")?;
        let csil_decode = csil_dec_alert_state;
        csil_decode(csil_field)?
    };
    let since = {
        let csil_field = cbor_require(csil_root, "since")?;
        let csil_decode = cbor_as_i64;
        csil_decode(csil_field)?
    };
    let observed_value = match cbor_map_get(csil_root, "observed_value") {
        Some(csil_field) => {
            let csil_decode = cbor_as_f64;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let reason = match cbor_map_get(csil_root, "reason") {
        Some(csil_field) => {
            let csil_decode = cbor_as_text;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let outcome = match cbor_map_get(csil_root, "outcome") {
        Some(csil_field) => {
            let csil_decode = csil_dec_alert_outcome;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let last_evaluated_at = match cbor_map_get(csil_root, "last_evaluated_at") {
        Some(csil_field) => {
            let csil_decode = cbor_as_i64;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let notifications_sent = match cbor_map_get(csil_root, "notifications_sent") {
        Some(csil_field) => {
            let csil_decode = cbor_as_u64;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let last_notified_at = match cbor_map_get(csil_root, "last_notified_at") {
        Some(csil_field) => {
            let csil_decode = cbor_as_i64;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let holding_ms = match cbor_map_get(csil_root, "holding_ms") {
        Some(csil_field) => {
            let csil_decode = cbor_as_i64;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let commit_watermark = match cbor_map_get(csil_root, "commit_watermark") {
        Some(csil_field) => {
            let csil_decode = cbor_as_u64;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    Ok(AlertInstance {
        rule_id,
        state,
        since,
        observed_value,
        reason,
        outcome,
        last_evaluated_at,
        notifications_sent,
        last_notified_at,
        holding_ms,
        commit_watermark,
    })
}

/// Encode a AlertInstance to canonical CSIL CBOR bytes.
pub fn encode_alert_instance(csil_v: &AlertInstance) -> Vec<u8> {
    cbor_encode(&csil_enc_alert_instance(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a AlertInstance.
pub fn decode_alert_instance(csil_data: &[u8]) -> Result<AlertInstance, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_alert_instance(&csil_root)
}

/// Build the canonical CBOR value tree for a SilenceRequest.
fn csil_enc_silence_request(csil_v: &SilenceRequest) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(4);
    csil_entries.push((cbor_text("until"), cbor_int(csil_v.until)));
    if let Some(csil_inner) = &csil_v.reason {
        csil_entries.push((cbor_text("reason"), cbor_text(csil_inner)));
    }
    csil_entries.push((cbor_text("rule_id"), cbor_text(&csil_v.rule_id)));
    csil_entries.push((cbor_text("project_id"), cbor_bytes(&csil_v.project_id)));
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a SilenceRequest from a decoded CBOR value tree.
fn csil_dec_silence_request(csil_root: &CsilCborValue) -> Result<SilenceRequest, CsilCborError> {
    let rule_id = {
        let csil_field = cbor_require(csil_root, "rule_id")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let project_id = {
        let csil_field = cbor_require(csil_root, "project_id")?;
        let csil_decode = cbor_as_bytes;
        csil_decode(csil_field)?
    };
    let until = {
        let csil_field = cbor_require(csil_root, "until")?;
        let csil_decode = cbor_as_i64;
        csil_decode(csil_field)?
    };
    let reason = match cbor_map_get(csil_root, "reason") {
        Some(csil_field) => {
            let csil_decode = cbor_as_text;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    Ok(SilenceRequest {
        rule_id,
        project_id,
        until,
        reason,
    })
}

/// Encode a SilenceRequest to canonical CSIL CBOR bytes.
pub fn encode_silence_request(csil_v: &SilenceRequest) -> Vec<u8> {
    cbor_encode(&csil_enc_silence_request(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a SilenceRequest.
pub fn decode_silence_request(csil_data: &[u8]) -> Result<SilenceRequest, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_silence_request(&csil_root)
}

/// Build the canonical CBOR value tree for a ResolveRequest.
fn csil_enc_resolve_request(csil_v: &ResolveRequest) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(3);
    if let Some(csil_inner) = &csil_v.reason {
        csil_entries.push((cbor_text("reason"), cbor_text(csil_inner)));
    }
    csil_entries.push((cbor_text("rule_id"), cbor_text(&csil_v.rule_id)));
    csil_entries.push((cbor_text("project_id"), cbor_bytes(&csil_v.project_id)));
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a ResolveRequest from a decoded CBOR value tree.
fn csil_dec_resolve_request(csil_root: &CsilCborValue) -> Result<ResolveRequest, CsilCborError> {
    let rule_id = {
        let csil_field = cbor_require(csil_root, "rule_id")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let project_id = {
        let csil_field = cbor_require(csil_root, "project_id")?;
        let csil_decode = cbor_as_bytes;
        csil_decode(csil_field)?
    };
    let reason = match cbor_map_get(csil_root, "reason") {
        Some(csil_field) => {
            let csil_decode = cbor_as_text;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    Ok(ResolveRequest {
        rule_id,
        project_id,
        reason,
    })
}

/// Encode a ResolveRequest to canonical CSIL CBOR bytes.
pub fn encode_resolve_request(csil_v: &ResolveRequest) -> Vec<u8> {
    cbor_encode(&csil_enc_resolve_request(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a ResolveRequest.
pub fn decode_resolve_request(csil_data: &[u8]) -> Result<ResolveRequest, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_resolve_request(&csil_root)
}

/// Build the canonical CBOR value tree for a DeleteAlertRuleRequest.
fn csil_enc_delete_alert_rule_request(csil_v: &DeleteAlertRuleRequest) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(2);
    csil_entries.push((cbor_text("rule_id"), cbor_text(&csil_v.rule_id)));
    csil_entries.push((cbor_text("project_id"), cbor_bytes(&csil_v.project_id)));
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a DeleteAlertRuleRequest from a decoded CBOR value tree.
fn csil_dec_delete_alert_rule_request(
    csil_root: &CsilCborValue,
) -> Result<DeleteAlertRuleRequest, CsilCborError> {
    let rule_id = {
        let csil_field = cbor_require(csil_root, "rule_id")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let project_id = {
        let csil_field = cbor_require(csil_root, "project_id")?;
        let csil_decode = cbor_as_bytes;
        csil_decode(csil_field)?
    };
    Ok(DeleteAlertRuleRequest {
        rule_id,
        project_id,
    })
}

/// Encode a DeleteAlertRuleRequest to canonical CSIL CBOR bytes.
pub fn encode_delete_alert_rule_request(csil_v: &DeleteAlertRuleRequest) -> Vec<u8> {
    cbor_encode(&csil_enc_delete_alert_rule_request(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a DeleteAlertRuleRequest.
pub fn decode_delete_alert_rule_request(
    csil_data: &[u8],
) -> Result<DeleteAlertRuleRequest, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_delete_alert_rule_request(&csil_root)
}

/// Build the canonical CBOR value tree for a NotificationDelivery.
fn csil_enc_notification_delivery(csil_v: &NotificationDelivery) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(8);
    csil_entries.push((cbor_text("at"), cbor_int(csil_v.at)));
    csil_entries.push((cbor_text("state"), csil_enc_alert_state(&csil_v.state)));
    csil_entries.push((cbor_text("target"), cbor_text(&csil_v.target)));
    csil_entries.push((cbor_text("rule_id"), cbor_text(&csil_v.rule_id)));
    csil_entries.push((cbor_text("attempts"), cbor_uint(csil_v.attempts)));
    csil_entries.push((cbor_text("delivered"), cbor_bool(csil_v.delivered)));
    if let Some(csil_inner) = &csil_v.last_failure {
        csil_entries.push((cbor_text("last_failure"), cbor_text(csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.next_attempt_at {
        csil_entries.push((cbor_text("next_attempt_at"), cbor_int(*csil_inner)));
    }
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a NotificationDelivery from a decoded CBOR value tree.
fn csil_dec_notification_delivery(
    csil_root: &CsilCborValue,
) -> Result<NotificationDelivery, CsilCborError> {
    let rule_id = {
        let csil_field = cbor_require(csil_root, "rule_id")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let target = {
        let csil_field = cbor_require(csil_root, "target")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let state = {
        let csil_field = cbor_require(csil_root, "state")?;
        let csil_decode = csil_dec_alert_state;
        csil_decode(csil_field)?
    };
    let attempts = {
        let csil_field = cbor_require(csil_root, "attempts")?;
        let csil_decode = cbor_as_u64;
        csil_decode(csil_field)?
    };
    let delivered = {
        let csil_field = cbor_require(csil_root, "delivered")?;
        let csil_decode = cbor_as_bool;
        csil_decode(csil_field)?
    };
    let last_failure = match cbor_map_get(csil_root, "last_failure") {
        Some(csil_field) => {
            let csil_decode = cbor_as_text;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let next_attempt_at = match cbor_map_get(csil_root, "next_attempt_at") {
        Some(csil_field) => {
            let csil_decode = cbor_as_i64;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let at = {
        let csil_field = cbor_require(csil_root, "at")?;
        let csil_decode = cbor_as_i64;
        csil_decode(csil_field)?
    };
    Ok(NotificationDelivery {
        rule_id,
        target,
        state,
        attempts,
        delivered,
        last_failure,
        next_attempt_at,
        at,
    })
}

/// Encode a NotificationDelivery to canonical CSIL CBOR bytes.
pub fn encode_notification_delivery(csil_v: &NotificationDelivery) -> Vec<u8> {
    cbor_encode(&csil_enc_notification_delivery(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a NotificationDelivery.
pub fn decode_notification_delivery(
    csil_data: &[u8],
) -> Result<NotificationDelivery, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_notification_delivery(&csil_root)
}

/// Build the canonical CBOR value tree for a NotificationDeliveryList.
fn csil_enc_notification_delivery_list(csil_v: &NotificationDeliveryList) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(2);
    csil_entries.push((
        cbor_text("deliveries"),
        cbor_enc_array(&csil_v.deliveries, csil_enc_notification_delivery),
    ));
    if let Some(csil_inner) = &csil_v.next_cursor {
        csil_entries.push((cbor_text("next_cursor"), cbor_bytes(csil_inner)));
    }
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a NotificationDeliveryList from a decoded CBOR value tree.
fn csil_dec_notification_delivery_list(
    csil_root: &CsilCborValue,
) -> Result<NotificationDeliveryList, CsilCborError> {
    let deliveries = {
        let csil_field = cbor_require(csil_root, "deliveries")?;
        let csil_decode = |csil_v| cbor_dec_array(csil_v, csil_dec_notification_delivery);
        csil_decode(csil_field)?
    };
    let next_cursor = match cbor_map_get(csil_root, "next_cursor") {
        Some(csil_field) => {
            let csil_decode = cbor_as_bytes;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    Ok(NotificationDeliveryList {
        deliveries,
        next_cursor,
    })
}

/// Encode a NotificationDeliveryList to canonical CSIL CBOR bytes.
pub fn encode_notification_delivery_list(csil_v: &NotificationDeliveryList) -> Vec<u8> {
    cbor_encode(&csil_enc_notification_delivery_list(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a NotificationDeliveryList.
pub fn decode_notification_delivery_list(
    csil_data: &[u8],
) -> Result<NotificationDeliveryList, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_notification_delivery_list(&csil_root)
}

/// Build the canonical CBOR value tree for a WorkflowStatus.
fn csil_enc_workflow_status(csil_v: &WorkflowStatus) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(9);
    csil_entries.push((cbor_text("kind"), csil_enc_workflow_kind(&csil_v.kind)));
    csil_entries.push((cbor_text("queue"), cbor_text(&csil_v.queue)));
    csil_entries.push((cbor_text("pending"), cbor_uint(csil_v.pending)));
    csil_entries.push((cbor_text("failures"), cbor_uint(csil_v.failures)));
    csil_entries.push((cbor_text("in_flight"), cbor_uint(csil_v.in_flight)));
    csil_entries.push((cbor_text("quarantined"), cbor_uint(csil_v.quarantined)));
    if let Some(csil_inner) = &csil_v.last_failure {
        csil_entries.push((cbor_text("last_failure"), cbor_text(csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.last_success_at {
        csil_entries.push((cbor_text("last_success_at"), cbor_int(*csil_inner)));
    }
    csil_entries.push((
        cbor_text("oldest_pending_age_ms"),
        cbor_int(csil_v.oldest_pending_age_ms),
    ));
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a WorkflowStatus from a decoded CBOR value tree.
fn csil_dec_workflow_status(csil_root: &CsilCborValue) -> Result<WorkflowStatus, CsilCborError> {
    let kind = {
        let csil_field = cbor_require(csil_root, "kind")?;
        let csil_decode = csil_dec_workflow_kind;
        csil_decode(csil_field)?
    };
    let queue = {
        let csil_field = cbor_require(csil_root, "queue")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let pending = {
        let csil_field = cbor_require(csil_root, "pending")?;
        let csil_decode = cbor_as_u64;
        csil_decode(csil_field)?
    };
    let in_flight = {
        let csil_field = cbor_require(csil_root, "in_flight")?;
        let csil_decode = cbor_as_u64;
        csil_decode(csil_field)?
    };
    let quarantined = {
        let csil_field = cbor_require(csil_root, "quarantined")?;
        let csil_decode = cbor_as_u64;
        csil_decode(csil_field)?
    };
    let oldest_pending_age_ms = {
        let csil_field = cbor_require(csil_root, "oldest_pending_age_ms")?;
        let csil_decode = cbor_as_i64;
        csil_decode(csil_field)?
    };
    let failures = {
        let csil_field = cbor_require(csil_root, "failures")?;
        let csil_decode = cbor_as_u64;
        csil_decode(csil_field)?
    };
    let last_success_at = match cbor_map_get(csil_root, "last_success_at") {
        Some(csil_field) => {
            let csil_decode = cbor_as_i64;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let last_failure = match cbor_map_get(csil_root, "last_failure") {
        Some(csil_field) => {
            let csil_decode = cbor_as_text;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    Ok(WorkflowStatus {
        kind,
        queue,
        pending,
        in_flight,
        quarantined,
        oldest_pending_age_ms,
        failures,
        last_success_at,
        last_failure,
    })
}

/// Encode a WorkflowStatus to canonical CSIL CBOR bytes.
pub fn encode_workflow_status(csil_v: &WorkflowStatus) -> Vec<u8> {
    cbor_encode(&csil_enc_workflow_status(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a WorkflowStatus.
pub fn decode_workflow_status(csil_data: &[u8]) -> Result<WorkflowStatus, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_workflow_status(&csil_root)
}

/// Build the canonical CBOR value tree for a WorkflowList.
fn csil_enc_workflow_list(csil_v: &WorkflowList) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(1);
    csil_entries.push((
        cbor_text("workflows"),
        cbor_enc_array(&csil_v.workflows, csil_enc_workflow_status),
    ));
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a WorkflowList from a decoded CBOR value tree.
fn csil_dec_workflow_list(csil_root: &CsilCborValue) -> Result<WorkflowList, CsilCborError> {
    let workflows = {
        let csil_field = cbor_require(csil_root, "workflows")?;
        let csil_decode = |csil_v| cbor_dec_array(csil_v, csil_dec_workflow_status);
        csil_decode(csil_field)?
    };
    Ok(WorkflowList { workflows })
}

/// Encode a WorkflowList to canonical CSIL CBOR bytes.
pub fn encode_workflow_list(csil_v: &WorkflowList) -> Vec<u8> {
    cbor_encode(&csil_enc_workflow_list(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a WorkflowList.
pub fn decode_workflow_list(csil_data: &[u8]) -> Result<WorkflowList, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_workflow_list(&csil_root)
}

/// Build the canonical CBOR value tree for a RunWorkflowRequest.
fn csil_enc_run_workflow_request(csil_v: &RunWorkflowRequest) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(4);
    csil_entries.push((cbor_text("kind"), csil_enc_workflow_kind(&csil_v.kind)));
    if let Some(csil_inner) = &csil_v.range {
        csil_entries.push((cbor_text("range"), csil_enc_time_range(csil_inner)));
    }
    csil_entries.push((cbor_text("project_id"), cbor_bytes(&csil_v.project_id)));
    if let Some(csil_inner) = &csil_v.destination {
        csil_entries.push((cbor_text("destination"), cbor_text(csil_inner)));
    }
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a RunWorkflowRequest from a decoded CBOR value tree.
fn csil_dec_run_workflow_request(
    csil_root: &CsilCborValue,
) -> Result<RunWorkflowRequest, CsilCborError> {
    let kind = {
        let csil_field = cbor_require(csil_root, "kind")?;
        let csil_decode = csil_dec_workflow_kind;
        csil_decode(csil_field)?
    };
    let project_id = {
        let csil_field = cbor_require(csil_root, "project_id")?;
        let csil_decode = cbor_as_bytes;
        csil_decode(csil_field)?
    };
    let range = match cbor_map_get(csil_root, "range") {
        Some(csil_field) => {
            let csil_decode = csil_dec_time_range;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let destination = match cbor_map_get(csil_root, "destination") {
        Some(csil_field) => {
            let csil_decode = cbor_as_text;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    Ok(RunWorkflowRequest {
        kind,
        project_id,
        range,
        destination,
    })
}

/// Encode a RunWorkflowRequest to canonical CSIL CBOR bytes.
pub fn encode_run_workflow_request(csil_v: &RunWorkflowRequest) -> Vec<u8> {
    cbor_encode(&csil_enc_run_workflow_request(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a RunWorkflowRequest.
pub fn decode_run_workflow_request(csil_data: &[u8]) -> Result<RunWorkflowRequest, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_run_workflow_request(&csil_root)
}

/// Build the canonical CBOR value tree for a RunWorkflowResponse.
fn csil_enc_run_workflow_response(csil_v: &RunWorkflowResponse) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(4);
    csil_entries.push((cbor_text("kind"), csil_enc_workflow_kind(&csil_v.kind)));
    if let Some(csil_inner) = &csil_v.reason {
        csil_entries.push((cbor_text("reason"), cbor_text(csil_inner)));
    }
    csil_entries.push((cbor_text("task_id"), cbor_text(&csil_v.task_id)));
    csil_entries.push((cbor_text("accepted"), cbor_bool(csil_v.accepted)));
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a RunWorkflowResponse from a decoded CBOR value tree.
fn csil_dec_run_workflow_response(
    csil_root: &CsilCborValue,
) -> Result<RunWorkflowResponse, CsilCborError> {
    let task_id = {
        let csil_field = cbor_require(csil_root, "task_id")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let kind = {
        let csil_field = cbor_require(csil_root, "kind")?;
        let csil_decode = csil_dec_workflow_kind;
        csil_decode(csil_field)?
    };
    let accepted = {
        let csil_field = cbor_require(csil_root, "accepted")?;
        let csil_decode = cbor_as_bool;
        csil_decode(csil_field)?
    };
    let reason = match cbor_map_get(csil_root, "reason") {
        Some(csil_field) => {
            let csil_decode = cbor_as_text;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    Ok(RunWorkflowResponse {
        task_id,
        kind,
        accepted,
        reason,
    })
}

/// Encode a RunWorkflowResponse to canonical CSIL CBOR bytes.
pub fn encode_run_workflow_response(csil_v: &RunWorkflowResponse) -> Vec<u8> {
    cbor_encode(&csil_enc_run_workflow_response(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a RunWorkflowResponse.
pub fn decode_run_workflow_response(
    csil_data: &[u8],
) -> Result<RunWorkflowResponse, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_run_workflow_response(&csil_root)
}

/// Build the canonical CBOR value tree for a BeginLoginRequest.
fn csil_enc_begin_login_request(csil_v: &BeginLoginRequest) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(2);
    csil_entries.push((cbor_text("user_domain"), cbor_text(&csil_v.user_domain)));
    csil_entries.push((cbor_text("callback_url"), cbor_text(&csil_v.callback_url)));
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a BeginLoginRequest from a decoded CBOR value tree.
fn csil_dec_begin_login_request(
    csil_root: &CsilCborValue,
) -> Result<BeginLoginRequest, CsilCborError> {
    let user_domain = {
        let csil_field = cbor_require(csil_root, "user_domain")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let callback_url = {
        let csil_field = cbor_require(csil_root, "callback_url")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    Ok(BeginLoginRequest {
        user_domain,
        callback_url,
    })
}

/// Encode a BeginLoginRequest to canonical CSIL CBOR bytes.
pub fn encode_begin_login_request(csil_v: &BeginLoginRequest) -> Vec<u8> {
    cbor_encode(&csil_enc_begin_login_request(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a BeginLoginRequest.
pub fn decode_begin_login_request(csil_data: &[u8]) -> Result<BeginLoginRequest, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_begin_login_request(&csil_root)
}

/// Build the canonical CBOR value tree for a BeginLoginResponse.
fn csil_enc_begin_login_response(csil_v: &BeginLoginResponse) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(3);
    csil_entries.push((cbor_text("login_id"), cbor_text(&csil_v.login_id)));
    csil_entries.push((cbor_text("expires_at"), cbor_int(csil_v.expires_at)));
    csil_entries.push((cbor_text("redirect_url"), cbor_text(&csil_v.redirect_url)));
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a BeginLoginResponse from a decoded CBOR value tree.
fn csil_dec_begin_login_response(
    csil_root: &CsilCborValue,
) -> Result<BeginLoginResponse, CsilCborError> {
    let redirect_url = {
        let csil_field = cbor_require(csil_root, "redirect_url")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let login_id = {
        let csil_field = cbor_require(csil_root, "login_id")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let expires_at = {
        let csil_field = cbor_require(csil_root, "expires_at")?;
        let csil_decode = cbor_as_i64;
        csil_decode(csil_field)?
    };
    Ok(BeginLoginResponse {
        redirect_url,
        login_id,
        expires_at,
    })
}

/// Encode a BeginLoginResponse to canonical CSIL CBOR bytes.
pub fn encode_begin_login_response(csil_v: &BeginLoginResponse) -> Vec<u8> {
    cbor_encode(&csil_enc_begin_login_response(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a BeginLoginResponse.
pub fn decode_begin_login_response(csil_data: &[u8]) -> Result<BeginLoginResponse, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_begin_login_response(&csil_root)
}

/// Build the canonical CBOR value tree for a CompleteLoginRequest.
fn csil_enc_complete_login_request(csil_v: &CompleteLoginRequest) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(3);
    csil_entries.push((cbor_text("login_id"), cbor_text(&csil_v.login_id)));
    csil_entries.push((cbor_text("arrived_url"), cbor_text(&csil_v.arrived_url)));
    csil_entries.push((
        cbor_text("encrypted_token"),
        cbor_text(&csil_v.encrypted_token),
    ));
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a CompleteLoginRequest from a decoded CBOR value tree.
fn csil_dec_complete_login_request(
    csil_root: &CsilCborValue,
) -> Result<CompleteLoginRequest, CsilCborError> {
    let login_id = {
        let csil_field = cbor_require(csil_root, "login_id")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let encrypted_token = {
        let csil_field = cbor_require(csil_root, "encrypted_token")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let arrived_url = {
        let csil_field = cbor_require(csil_root, "arrived_url")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    Ok(CompleteLoginRequest {
        login_id,
        encrypted_token,
        arrived_url,
    })
}

/// Encode a CompleteLoginRequest to canonical CSIL CBOR bytes.
pub fn encode_complete_login_request(csil_v: &CompleteLoginRequest) -> Vec<u8> {
    cbor_encode(&csil_enc_complete_login_request(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a CompleteLoginRequest.
pub fn decode_complete_login_request(
    csil_data: &[u8],
) -> Result<CompleteLoginRequest, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_complete_login_request(&csil_root)
}

/// Build the canonical CBOR value tree for a CompleteLoginResponse.
fn csil_enc_complete_login_response(csil_v: &CompleteLoginResponse) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(4);
    csil_entries.push((cbor_text("subject"), cbor_text(&csil_v.subject)));
    csil_entries.push((cbor_text("expires_at"), cbor_int(csil_v.expires_at)));
    csil_entries.push((
        cbor_text("memberships"),
        cbor_enc_array(&csil_v.memberships, csil_enc_membership),
    ));
    csil_entries.push((cbor_text("session_token"), cbor_text(&csil_v.session_token)));
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a CompleteLoginResponse from a decoded CBOR value tree.
fn csil_dec_complete_login_response(
    csil_root: &CsilCborValue,
) -> Result<CompleteLoginResponse, CsilCborError> {
    let session_token = {
        let csil_field = cbor_require(csil_root, "session_token")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let subject = {
        let csil_field = cbor_require(csil_root, "subject")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let expires_at = {
        let csil_field = cbor_require(csil_root, "expires_at")?;
        let csil_decode = cbor_as_i64;
        csil_decode(csil_field)?
    };
    let memberships = {
        let csil_field = cbor_require(csil_root, "memberships")?;
        let csil_decode = |csil_v| cbor_dec_array(csil_v, csil_dec_membership);
        csil_decode(csil_field)?
    };
    Ok(CompleteLoginResponse {
        session_token,
        subject,
        expires_at,
        memberships,
    })
}

/// Encode a CompleteLoginResponse to canonical CSIL CBOR bytes.
pub fn encode_complete_login_response(csil_v: &CompleteLoginResponse) -> Vec<u8> {
    cbor_encode(&csil_enc_complete_login_response(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a CompleteLoginResponse.
pub fn decode_complete_login_response(
    csil_data: &[u8],
) -> Result<CompleteLoginResponse, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_complete_login_response(&csil_root)
}

/// Build the canonical CBOR value tree for a Membership.
fn csil_enc_membership(csil_v: &Membership) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(2);
    csil_entries.push((cbor_text("role"), csil_enc_membership_role(&csil_v.role)));
    csil_entries.push((cbor_text("workspace_id"), cbor_bytes(&csil_v.workspace_id)));
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a Membership from a decoded CBOR value tree.
fn csil_dec_membership(csil_root: &CsilCborValue) -> Result<Membership, CsilCborError> {
    let workspace_id = {
        let csil_field = cbor_require(csil_root, "workspace_id")?;
        let csil_decode = cbor_as_bytes;
        csil_decode(csil_field)?
    };
    let role = {
        let csil_field = cbor_require(csil_root, "role")?;
        let csil_decode = csil_dec_membership_role;
        csil_decode(csil_field)?
    };
    Ok(Membership { workspace_id, role })
}

/// Encode a Membership to canonical CSIL CBOR bytes.
pub fn encode_membership(csil_v: &Membership) -> Vec<u8> {
    cbor_encode(&csil_enc_membership(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a Membership.
pub fn decode_membership(csil_data: &[u8]) -> Result<Membership, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_membership(&csil_root)
}

/// Build the canonical CBOR value tree for a Project.
fn csil_enc_project(csil_v: &Project) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(4);
    csil_entries.push((cbor_text("name"), cbor_text(&csil_v.name)));
    csil_entries.push((cbor_text("project_id"), cbor_bytes(&csil_v.project_id)));
    if let Some(csil_inner) = &csil_v.description {
        csil_entries.push((cbor_text("description"), cbor_text(csil_inner)));
    }
    csil_entries.push((cbor_text("workspace_id"), cbor_bytes(&csil_v.workspace_id)));
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a Project from a decoded CBOR value tree.
fn csil_dec_project(csil_root: &CsilCborValue) -> Result<Project, CsilCborError> {
    let project_id = {
        let csil_field = cbor_require(csil_root, "project_id")?;
        let csil_decode = cbor_as_bytes;
        csil_decode(csil_field)?
    };
    let workspace_id = {
        let csil_field = cbor_require(csil_root, "workspace_id")?;
        let csil_decode = cbor_as_bytes;
        csil_decode(csil_field)?
    };
    let name = {
        let csil_field = cbor_require(csil_root, "name")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let description = match cbor_map_get(csil_root, "description") {
        Some(csil_field) => {
            let csil_decode = cbor_as_text;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    Ok(Project {
        project_id,
        workspace_id,
        name,
        description,
    })
}

/// Encode a Project to canonical CSIL CBOR bytes.
pub fn encode_project(csil_v: &Project) -> Vec<u8> {
    cbor_encode(&csil_enc_project(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a Project.
pub fn decode_project(csil_data: &[u8]) -> Result<Project, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_project(&csil_root)
}

/// Build the canonical CBOR value tree for a Workspace.
fn csil_enc_workspace(csil_v: &Workspace) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(2);
    csil_entries.push((cbor_text("name"), cbor_text(&csil_v.name)));
    csil_entries.push((cbor_text("workspace_id"), cbor_bytes(&csil_v.workspace_id)));
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a Workspace from a decoded CBOR value tree.
fn csil_dec_workspace(csil_root: &CsilCborValue) -> Result<Workspace, CsilCborError> {
    let workspace_id = {
        let csil_field = cbor_require(csil_root, "workspace_id")?;
        let csil_decode = cbor_as_bytes;
        csil_decode(csil_field)?
    };
    let name = {
        let csil_field = cbor_require(csil_root, "name")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    Ok(Workspace { workspace_id, name })
}

/// Encode a Workspace to canonical CSIL CBOR bytes.
pub fn encode_workspace(csil_v: &Workspace) -> Vec<u8> {
    cbor_encode(&csil_enc_workspace(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a Workspace.
pub fn decode_workspace(csil_data: &[u8]) -> Result<Workspace, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_workspace(&csil_root)
}

/// Build the canonical CBOR value tree for a ApiKeySummary.
fn csil_enc_api_key_summary(csil_v: &ApiKeySummary) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(6);
    csil_entries.push((cbor_text("key_id"), cbor_text(&csil_v.key_id)));
    csil_entries.push((cbor_text("revoked"), cbor_bool(csil_v.revoked)));
    csil_entries.push((cbor_text("created_at"), cbor_int(csil_v.created_at)));
    if let Some(csil_inner) = &csil_v.expires_at {
        csil_entries.push((cbor_text("expires_at"), cbor_int(*csil_inner)));
    }
    csil_entries.push((cbor_text("project_id"), cbor_bytes(&csil_v.project_id)));
    if let Some(csil_inner) = &csil_v.last_used_at {
        csil_entries.push((cbor_text("last_used_at"), cbor_int(*csil_inner)));
    }
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a ApiKeySummary from a decoded CBOR value tree.
fn csil_dec_api_key_summary(csil_root: &CsilCborValue) -> Result<ApiKeySummary, CsilCborError> {
    let key_id = {
        let csil_field = cbor_require(csil_root, "key_id")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let project_id = {
        let csil_field = cbor_require(csil_root, "project_id")?;
        let csil_decode = cbor_as_bytes;
        csil_decode(csil_field)?
    };
    let created_at = {
        let csil_field = cbor_require(csil_root, "created_at")?;
        let csil_decode = cbor_as_i64;
        csil_decode(csil_field)?
    };
    let expires_at = match cbor_map_get(csil_root, "expires_at") {
        Some(csil_field) => {
            let csil_decode = cbor_as_i64;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let last_used_at = match cbor_map_get(csil_root, "last_used_at") {
        Some(csil_field) => {
            let csil_decode = cbor_as_i64;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let revoked = {
        let csil_field = cbor_require(csil_root, "revoked")?;
        let csil_decode = cbor_as_bool;
        csil_decode(csil_field)?
    };
    Ok(ApiKeySummary {
        key_id,
        project_id,
        created_at,
        expires_at,
        last_used_at,
        revoked,
    })
}

/// Encode a ApiKeySummary to canonical CSIL CBOR bytes.
pub fn encode_api_key_summary(csil_v: &ApiKeySummary) -> Vec<u8> {
    cbor_encode(&csil_enc_api_key_summary(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a ApiKeySummary.
pub fn decode_api_key_summary(csil_data: &[u8]) -> Result<ApiKeySummary, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_api_key_summary(&csil_root)
}

/// Build the canonical CBOR value tree for a DeletionTarget.
fn csil_enc_deletion_target(csil_v: &DeletionTarget) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(4);
    if let Some(csil_inner) = &csil_v.range {
        csil_entries.push((cbor_text("range"), csil_enc_time_range(csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.event_ids {
        csil_entries.push((
            cbor_text("event_ids"),
            cbor_enc_array(csil_inner, |csil_elem| cbor_bytes(csil_elem)),
        ));
    }
    csil_entries.push((cbor_text("project_id"), cbor_bytes(&csil_v.project_id)));
    if let Some(csil_inner) = &csil_v.end_user_id {
        csil_entries.push((cbor_text("end_user_id"), cbor_text(csil_inner)));
    }
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a DeletionTarget from a decoded CBOR value tree.
fn csil_dec_deletion_target(csil_root: &CsilCborValue) -> Result<DeletionTarget, CsilCborError> {
    let project_id = {
        let csil_field = cbor_require(csil_root, "project_id")?;
        let csil_decode = cbor_as_bytes;
        csil_decode(csil_field)?
    };
    let end_user_id = match cbor_map_get(csil_root, "end_user_id") {
        Some(csil_field) => {
            let csil_decode = cbor_as_text;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let event_ids = match cbor_map_get(csil_root, "event_ids") {
        Some(csil_field) => {
            let csil_decode = |csil_v| cbor_dec_array(csil_v, cbor_as_bytes);
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let range = match cbor_map_get(csil_root, "range") {
        Some(csil_field) => {
            let csil_decode = csil_dec_time_range;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    Ok(DeletionTarget {
        project_id,
        end_user_id,
        event_ids,
        range,
    })
}

/// Encode a DeletionTarget to canonical CSIL CBOR bytes.
pub fn encode_deletion_target(csil_v: &DeletionTarget) -> Vec<u8> {
    cbor_encode(&csil_enc_deletion_target(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a DeletionTarget.
pub fn decode_deletion_target(csil_data: &[u8]) -> Result<DeletionTarget, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_deletion_target(&csil_root)
}

/// Build the canonical CBOR value tree for a DeletionRequest.
fn csil_enc_deletion_request(csil_v: &DeletionRequest) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(2);
    csil_entries.push((cbor_text("reason"), cbor_text(&csil_v.reason)));
    csil_entries.push((
        cbor_text("target"),
        csil_enc_deletion_target(&csil_v.target),
    ));
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a DeletionRequest from a decoded CBOR value tree.
fn csil_dec_deletion_request(csil_root: &CsilCborValue) -> Result<DeletionRequest, CsilCborError> {
    let target = {
        let csil_field = cbor_require(csil_root, "target")?;
        let csil_decode = csil_dec_deletion_target;
        csil_decode(csil_field)?
    };
    let reason = {
        let csil_field = cbor_require(csil_root, "reason")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    Ok(DeletionRequest { target, reason })
}

/// Encode a DeletionRequest to canonical CSIL CBOR bytes.
pub fn encode_deletion_request(csil_v: &DeletionRequest) -> Vec<u8> {
    cbor_encode(&csil_enc_deletion_request(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a DeletionRequest.
pub fn decode_deletion_request(csil_data: &[u8]) -> Result<DeletionRequest, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_deletion_request(&csil_root)
}

/// Build the canonical CBOR value tree for a DeletionResponse.
fn csil_enc_deletion_response(csil_v: &DeletionResponse) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(5);
    if let Some(csil_inner) = &csil_v.predicates {
        csil_entries.push((cbor_text("predicates"), cbor_uint(*csil_inner)));
    }
    csil_entries.push((cbor_text("request_id"), cbor_text(&csil_v.request_id)));
    csil_entries.push((cbor_text("accepted_at"), cbor_int(csil_v.accepted_at)));
    if let Some(csil_inner) = &csil_v.identifiers {
        csil_entries.push((
            cbor_text("identifiers"),
            cbor_enc_array(csil_inner, |csil_elem| cbor_text(csil_elem)),
        ));
    }
    csil_entries.push((
        cbor_text("tombstone_generation"),
        cbor_uint(csil_v.tombstone_generation),
    ));
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a DeletionResponse from a decoded CBOR value tree.
fn csil_dec_deletion_response(
    csil_root: &CsilCborValue,
) -> Result<DeletionResponse, CsilCborError> {
    let request_id = {
        let csil_field = cbor_require(csil_root, "request_id")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let tombstone_generation = {
        let csil_field = cbor_require(csil_root, "tombstone_generation")?;
        let csil_decode = cbor_as_u64;
        csil_decode(csil_field)?
    };
    let accepted_at = {
        let csil_field = cbor_require(csil_root, "accepted_at")?;
        let csil_decode = cbor_as_i64;
        csil_decode(csil_field)?
    };
    let predicates = match cbor_map_get(csil_root, "predicates") {
        Some(csil_field) => {
            let csil_decode = cbor_as_u64;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let identifiers = match cbor_map_get(csil_root, "identifiers") {
        Some(csil_field) => {
            let csil_decode = |csil_v| cbor_dec_array(csil_v, cbor_as_text);
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    Ok(DeletionResponse {
        request_id,
        tombstone_generation,
        accepted_at,
        predicates,
        identifiers,
    })
}

/// Encode a DeletionResponse to canonical CSIL CBOR bytes.
pub fn encode_deletion_response(csil_v: &DeletionResponse) -> Vec<u8> {
    cbor_encode(&csil_enc_deletion_response(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a DeletionResponse.
pub fn decode_deletion_response(csil_data: &[u8]) -> Result<DeletionResponse, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_deletion_response(&csil_root)
}

/// Build the canonical CBOR value tree for a PolicyDocument.
fn csil_enc_policy_document(csil_v: &PolicyDocument) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(13);
    csil_entries.push((cbor_text("scope"), csil_enc_policy_scope(&csil_v.scope)));
    if let Some(csil_inner) = &csil_v.scope_id {
        csil_entries.push((cbor_text("scope_id"), cbor_text(csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.kill_switch {
        csil_entries.push((cbor_text("kill_switch"), cbor_bool(*csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.enabled_kinds {
        csil_entries.push((
            cbor_text("enabled_kinds"),
            cbor_enc_array(csil_inner, csil_enc_telemetry_kind),
        ));
    }
    if let Some(csil_inner) = &csil_v.max_properties {
        csil_entries.push((cbor_text("max_properties"), cbor_uint(*csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.max_event_bytes {
        csil_entries.push((cbor_text("max_event_bytes"), cbor_uint(*csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.campaign_linking {
        csil_entries.push((
            cbor_text("campaign_linking"),
            csil_enc_campaign_linking(csil_inner),
        ));
    }
    if let Some(csil_inner) = &csil_v.head_sample_rate {
        csil_entries.push((cbor_text("head_sample_rate"), cbor_float(*csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.blocked_event_names {
        csil_entries.push((
            cbor_text("blocked_event_names"),
            cbor_enc_array(csil_inner, |csil_elem| cbor_text(csil_elem)),
        ));
    }
    if let Some(csil_inner) = &csil_v.redact_property_keys {
        csil_entries.push((
            cbor_text("redact_property_keys"),
            cbor_enc_array(csil_inner, |csil_elem| cbor_text(csil_elem)),
        ));
    }
    if let Some(csil_inner) = &csil_v.blocked_property_keys {
        csil_entries.push((
            cbor_text("blocked_property_keys"),
            cbor_enc_array(csil_inner, |csil_elem| cbor_text(csil_elem)),
        ));
    }
    if let Some(csil_inner) = &csil_v.session_max_lifetime_ms {
        csil_entries.push((cbor_text("session_max_lifetime_ms"), cbor_int(*csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.attribution_needs_consent {
        csil_entries.push((
            cbor_text("attribution_needs_consent"),
            cbor_bool(*csil_inner),
        ));
    }
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a PolicyDocument from a decoded CBOR value tree.
fn csil_dec_policy_document(csil_root: &CsilCborValue) -> Result<PolicyDocument, CsilCborError> {
    let scope = {
        let csil_field = cbor_require(csil_root, "scope")?;
        let csil_decode = csil_dec_policy_scope;
        csil_decode(csil_field)?
    };
    let scope_id = match cbor_map_get(csil_root, "scope_id") {
        Some(csil_field) => {
            let csil_decode = cbor_as_text;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let enabled_kinds = match cbor_map_get(csil_root, "enabled_kinds") {
        Some(csil_field) => {
            let csil_decode = |csil_v| cbor_dec_array(csil_v, csil_dec_telemetry_kind);
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let head_sample_rate = match cbor_map_get(csil_root, "head_sample_rate") {
        Some(csil_field) => {
            let csil_decode = cbor_as_f64;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let session_max_lifetime_ms = match cbor_map_get(csil_root, "session_max_lifetime_ms") {
        Some(csil_field) => {
            let csil_decode = cbor_as_i64;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let max_event_bytes = match cbor_map_get(csil_root, "max_event_bytes") {
        Some(csil_field) => {
            let csil_decode = cbor_as_u64;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let max_properties = match cbor_map_get(csil_root, "max_properties") {
        Some(csil_field) => {
            let csil_decode = cbor_as_u64;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let blocked_event_names = match cbor_map_get(csil_root, "blocked_event_names") {
        Some(csil_field) => {
            let csil_decode = |csil_v| cbor_dec_array(csil_v, cbor_as_text);
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let blocked_property_keys = match cbor_map_get(csil_root, "blocked_property_keys") {
        Some(csil_field) => {
            let csil_decode = |csil_v| cbor_dec_array(csil_v, cbor_as_text);
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let redact_property_keys = match cbor_map_get(csil_root, "redact_property_keys") {
        Some(csil_field) => {
            let csil_decode = |csil_v| cbor_dec_array(csil_v, cbor_as_text);
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let campaign_linking = match cbor_map_get(csil_root, "campaign_linking") {
        Some(csil_field) => {
            let csil_decode = csil_dec_campaign_linking;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let attribution_needs_consent = match cbor_map_get(csil_root, "attribution_needs_consent") {
        Some(csil_field) => {
            let csil_decode = cbor_as_bool;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let kill_switch = match cbor_map_get(csil_root, "kill_switch") {
        Some(csil_field) => {
            let csil_decode = cbor_as_bool;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    Ok(PolicyDocument {
        scope,
        scope_id,
        enabled_kinds,
        head_sample_rate,
        session_max_lifetime_ms,
        max_event_bytes,
        max_properties,
        blocked_event_names,
        blocked_property_keys,
        redact_property_keys,
        campaign_linking,
        attribution_needs_consent,
        kill_switch,
    })
}

/// Encode a PolicyDocument to canonical CSIL CBOR bytes.
pub fn encode_policy_document(csil_v: &PolicyDocument) -> Vec<u8> {
    cbor_encode(&csil_enc_policy_document(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a PolicyDocument.
pub fn decode_policy_document(csil_data: &[u8]) -> Result<PolicyDocument, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_policy_document(&csil_root)
}

/// Build the canonical CBOR value tree for a CompiledPolicy.
fn csil_enc_compiled_policy(csil_v: &CompiledPolicy) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(13);
    csil_entries.push((
        cbor_text("from_levels"),
        cbor_enc_array(&csil_v.from_levels, |csil_elem| cbor_text(csil_elem)),
    ));
    csil_entries.push((cbor_text("kill_switch"), cbor_bool(csil_v.kill_switch)));
    csil_entries.push((
        cbor_text("enabled_kinds"),
        cbor_enc_array(&csil_v.enabled_kinds, csil_enc_telemetry_kind),
    ));
    csil_entries.push((
        cbor_text("max_properties"),
        cbor_uint(csil_v.max_properties),
    ));
    csil_entries.push((
        cbor_text("policy_version"),
        cbor_uint(csil_v.policy_version),
    ));
    csil_entries.push((
        cbor_text("max_event_bytes"),
        cbor_uint(csil_v.max_event_bytes),
    ));
    csil_entries.push((
        cbor_text("campaign_linking"),
        csil_enc_campaign_linking(&csil_v.campaign_linking),
    ));
    csil_entries.push((
        cbor_text("head_sample_rate"),
        cbor_float(csil_v.head_sample_rate),
    ));
    csil_entries.push((
        cbor_text("blocked_event_names"),
        cbor_enc_array(&csil_v.blocked_event_names, |csil_elem| {
            cbor_text(csil_elem)
        }),
    ));
    csil_entries.push((
        cbor_text("redact_property_keys"),
        cbor_enc_array(&csil_v.redact_property_keys, |csil_elem| {
            cbor_text(csil_elem)
        }),
    ));
    csil_entries.push((
        cbor_text("blocked_property_keys"),
        cbor_enc_array(&csil_v.blocked_property_keys, |csil_elem| {
            cbor_text(csil_elem)
        }),
    ));
    csil_entries.push((
        cbor_text("session_max_lifetime_ms"),
        cbor_int(csil_v.session_max_lifetime_ms),
    ));
    csil_entries.push((
        cbor_text("attribution_needs_consent"),
        cbor_bool(csil_v.attribution_needs_consent),
    ));
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a CompiledPolicy from a decoded CBOR value tree.
fn csil_dec_compiled_policy(csil_root: &CsilCborValue) -> Result<CompiledPolicy, CsilCborError> {
    let policy_version = {
        let csil_field = cbor_require(csil_root, "policy_version")?;
        let csil_decode = cbor_as_u64;
        csil_decode(csil_field)?
    };
    let enabled_kinds = {
        let csil_field = cbor_require(csil_root, "enabled_kinds")?;
        let csil_decode = |csil_v| cbor_dec_array(csil_v, csil_dec_telemetry_kind);
        csil_decode(csil_field)?
    };
    let head_sample_rate = {
        let csil_field = cbor_require(csil_root, "head_sample_rate")?;
        let csil_decode = cbor_as_f64;
        csil_decode(csil_field)?
    };
    let session_max_lifetime_ms = {
        let csil_field = cbor_require(csil_root, "session_max_lifetime_ms")?;
        let csil_decode = cbor_as_i64;
        csil_decode(csil_field)?
    };
    let max_event_bytes = {
        let csil_field = cbor_require(csil_root, "max_event_bytes")?;
        let csil_decode = cbor_as_u64;
        csil_decode(csil_field)?
    };
    let max_properties = {
        let csil_field = cbor_require(csil_root, "max_properties")?;
        let csil_decode = cbor_as_u64;
        csil_decode(csil_field)?
    };
    let blocked_event_names = {
        let csil_field = cbor_require(csil_root, "blocked_event_names")?;
        let csil_decode = |csil_v| cbor_dec_array(csil_v, cbor_as_text);
        csil_decode(csil_field)?
    };
    let blocked_property_keys = {
        let csil_field = cbor_require(csil_root, "blocked_property_keys")?;
        let csil_decode = |csil_v| cbor_dec_array(csil_v, cbor_as_text);
        csil_decode(csil_field)?
    };
    let redact_property_keys = {
        let csil_field = cbor_require(csil_root, "redact_property_keys")?;
        let csil_decode = |csil_v| cbor_dec_array(csil_v, cbor_as_text);
        csil_decode(csil_field)?
    };
    let campaign_linking = {
        let csil_field = cbor_require(csil_root, "campaign_linking")?;
        let csil_decode = csil_dec_campaign_linking;
        csil_decode(csil_field)?
    };
    let attribution_needs_consent = {
        let csil_field = cbor_require(csil_root, "attribution_needs_consent")?;
        let csil_decode = cbor_as_bool;
        csil_decode(csil_field)?
    };
    let kill_switch = {
        let csil_field = cbor_require(csil_root, "kill_switch")?;
        let csil_decode = cbor_as_bool;
        csil_decode(csil_field)?
    };
    let from_levels = {
        let csil_field = cbor_require(csil_root, "from_levels")?;
        let csil_decode = |csil_v| cbor_dec_array(csil_v, cbor_as_text);
        csil_decode(csil_field)?
    };
    Ok(CompiledPolicy {
        policy_version,
        enabled_kinds,
        head_sample_rate,
        session_max_lifetime_ms,
        max_event_bytes,
        max_properties,
        blocked_event_names,
        blocked_property_keys,
        redact_property_keys,
        campaign_linking,
        attribution_needs_consent,
        kill_switch,
        from_levels,
    })
}

/// Encode a CompiledPolicy to canonical CSIL CBOR bytes.
pub fn encode_compiled_policy(csil_v: &CompiledPolicy) -> Vec<u8> {
    cbor_encode(&csil_enc_compiled_policy(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a CompiledPolicy.
pub fn decode_compiled_policy(csil_data: &[u8]) -> Result<CompiledPolicy, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_compiled_policy(&csil_root)
}

/// Build the canonical CBOR value tree for a PolicyRequest.
fn csil_enc_policy_request(csil_v: &PolicyRequest) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(4);
    if let Some(csil_inner) = &csil_v.source_id {
        csil_entries.push((cbor_text("source_id"), cbor_bytes(csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.project_id {
        csil_entries.push((cbor_text("project_id"), cbor_bytes(csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.environment {
        csil_entries.push((cbor_text("environment"), cbor_text(csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.workspace_id {
        csil_entries.push((cbor_text("workspace_id"), cbor_bytes(csil_inner)));
    }
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a PolicyRequest from a decoded CBOR value tree.
fn csil_dec_policy_request(csil_root: &CsilCborValue) -> Result<PolicyRequest, CsilCborError> {
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
    let environment = match cbor_map_get(csil_root, "environment") {
        Some(csil_field) => {
            let csil_decode = cbor_as_text;
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
    Ok(PolicyRequest {
        workspace_id,
        project_id,
        environment,
        source_id,
    })
}

/// Encode a PolicyRequest to canonical CSIL CBOR bytes.
pub fn encode_policy_request(csil_v: &PolicyRequest) -> Vec<u8> {
    cbor_encode(&csil_enc_policy_request(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a PolicyRequest.
pub fn decode_policy_request(csil_data: &[u8]) -> Result<PolicyRequest, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_policy_request(&csil_root)
}

/// Build the canonical CBOR value tree for a AttributionSettings.
fn csil_enc_attribution_settings(csil_v: &AttributionSettings) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(10);
    csil_entries.push((cbor_text("project_id"), cbor_bytes(&csil_v.project_id)));
    if let Some(csil_inner) = &csil_v.updated_at {
        csil_entries.push((cbor_text("updated_at"), cbor_int(*csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.updated_by {
        csil_entries.push((cbor_text("updated_by"), cbor_text(csil_inner)));
    }
    csil_entries.push((cbor_text("lookback_ms"), cbor_int(csil_v.lookback_ms)));
    csil_entries.push((
        cbor_text("enabled_models"),
        cbor_enc_array(&csil_v.enabled_models, csil_enc_attribution_model),
    ));
    if let Some(csil_inner) = &csil_v.settings_version {
        csil_entries.push((cbor_text("settings_version"), cbor_uint(*csil_inner)));
    }
    csil_entries.push((
        cbor_text("decay_half_life_ms"),
        cbor_int(csil_v.decay_half_life_ms),
    ));
    csil_entries.push((
        cbor_text("touch_retention_ms"),
        cbor_int(csil_v.touch_retention_ms),
    ));
    csil_entries.push((
        cbor_text("position_last_weight"),
        cbor_float(csil_v.position_last_weight),
    ));
    csil_entries.push((
        cbor_text("position_first_weight"),
        cbor_float(csil_v.position_first_weight),
    ));
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a AttributionSettings from a decoded CBOR value tree.
fn csil_dec_attribution_settings(
    csil_root: &CsilCborValue,
) -> Result<AttributionSettings, CsilCborError> {
    let project_id = {
        let csil_field = cbor_require(csil_root, "project_id")?;
        let csil_decode = cbor_as_bytes;
        csil_decode(csil_field)?
    };
    let position_first_weight = {
        let csil_field = cbor_require(csil_root, "position_first_weight")?;
        let csil_decode = cbor_as_f64;
        csil_decode(csil_field)?
    };
    let position_last_weight = {
        let csil_field = cbor_require(csil_root, "position_last_weight")?;
        let csil_decode = cbor_as_f64;
        csil_decode(csil_field)?
    };
    let decay_half_life_ms = {
        let csil_field = cbor_require(csil_root, "decay_half_life_ms")?;
        let csil_decode = cbor_as_i64;
        csil_decode(csil_field)?
    };
    let lookback_ms = {
        let csil_field = cbor_require(csil_root, "lookback_ms")?;
        let csil_decode = cbor_as_i64;
        csil_decode(csil_field)?
    };
    let enabled_models = {
        let csil_field = cbor_require(csil_root, "enabled_models")?;
        let csil_decode = |csil_v| cbor_dec_array(csil_v, csil_dec_attribution_model);
        csil_decode(csil_field)?
    };
    let touch_retention_ms = {
        let csil_field = cbor_require(csil_root, "touch_retention_ms")?;
        let csil_decode = cbor_as_i64;
        csil_decode(csil_field)?
    };
    let settings_version = match cbor_map_get(csil_root, "settings_version") {
        Some(csil_field) => {
            let csil_decode = cbor_as_u64;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let updated_at = match cbor_map_get(csil_root, "updated_at") {
        Some(csil_field) => {
            let csil_decode = cbor_as_i64;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let updated_by = match cbor_map_get(csil_root, "updated_by") {
        Some(csil_field) => {
            let csil_decode = cbor_as_text;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    Ok(AttributionSettings {
        project_id,
        position_first_weight,
        position_last_weight,
        decay_half_life_ms,
        lookback_ms,
        enabled_models,
        touch_retention_ms,
        settings_version,
        updated_at,
        updated_by,
    })
}

/// Encode a AttributionSettings to canonical CSIL CBOR bytes.
pub fn encode_attribution_settings(csil_v: &AttributionSettings) -> Vec<u8> {
    cbor_encode(&csil_enc_attribution_settings(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a AttributionSettings.
pub fn decode_attribution_settings(csil_data: &[u8]) -> Result<AttributionSettings, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_attribution_settings(&csil_root)
}

/// Build the canonical CBOR value tree for a SavedAnalysis.
fn csil_enc_saved_analysis(csil_v: &SavedAnalysis) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(9);
    csil_entries.push((cbor_text("form"), csil_enc_query_form(&csil_v.form)));
    csil_entries.push((cbor_text("name"), cbor_text(&csil_v.name)));
    csil_entries.push((cbor_text("request"), cbor_bytes(&csil_v.request)));
    if let Some(csil_inner) = &csil_v.created_at {
        csil_entries.push((cbor_text("created_at"), cbor_int(*csil_inner)));
    }
    csil_entries.push((cbor_text("project_id"), cbor_bytes(&csil_v.project_id)));
    if let Some(csil_inner) = &csil_v.updated_at {
        csil_entries.push((cbor_text("updated_at"), cbor_int(*csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.updated_by {
        csil_entries.push((cbor_text("updated_by"), cbor_text(csil_inner)));
    }
    csil_entries.push((cbor_text("analysis_id"), cbor_text(&csil_v.analysis_id)));
    csil_entries.push((
        cbor_text("algebra_version"),
        cbor_uint(csil_v.algebra_version),
    ));
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a SavedAnalysis from a decoded CBOR value tree.
fn csil_dec_saved_analysis(csil_root: &CsilCborValue) -> Result<SavedAnalysis, CsilCborError> {
    let analysis_id = {
        let csil_field = cbor_require(csil_root, "analysis_id")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let project_id = {
        let csil_field = cbor_require(csil_root, "project_id")?;
        let csil_decode = cbor_as_bytes;
        csil_decode(csil_field)?
    };
    let name = {
        let csil_field = cbor_require(csil_root, "name")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let form = {
        let csil_field = cbor_require(csil_root, "form")?;
        let csil_decode = csil_dec_query_form;
        csil_decode(csil_field)?
    };
    let request = {
        let csil_field = cbor_require(csil_root, "request")?;
        let csil_decode = cbor_as_bytes;
        csil_decode(csil_field)?
    };
    let algebra_version = {
        let csil_field = cbor_require(csil_root, "algebra_version")?;
        let csil_decode = cbor_as_u64;
        csil_decode(csil_field)?
    };
    let created_at = match cbor_map_get(csil_root, "created_at") {
        Some(csil_field) => {
            let csil_decode = cbor_as_i64;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let updated_at = match cbor_map_get(csil_root, "updated_at") {
        Some(csil_field) => {
            let csil_decode = cbor_as_i64;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let updated_by = match cbor_map_get(csil_root, "updated_by") {
        Some(csil_field) => {
            let csil_decode = cbor_as_text;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    Ok(SavedAnalysis {
        analysis_id,
        project_id,
        name,
        form,
        request,
        algebra_version,
        created_at,
        updated_at,
        updated_by,
    })
}

/// Encode a SavedAnalysis to canonical CSIL CBOR bytes.
pub fn encode_saved_analysis(csil_v: &SavedAnalysis) -> Vec<u8> {
    cbor_encode(&csil_enc_saved_analysis(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a SavedAnalysis.
pub fn decode_saved_analysis(csil_data: &[u8]) -> Result<SavedAnalysis, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_saved_analysis(&csil_root)
}

/// Build the canonical CBOR value tree for a SavedAnalysisList.
fn csil_enc_saved_analysis_list(csil_v: &SavedAnalysisList) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(2);
    csil_entries.push((
        cbor_text("analyses"),
        cbor_enc_array(&csil_v.analyses, csil_enc_saved_analysis),
    ));
    if let Some(csil_inner) = &csil_v.next_cursor {
        csil_entries.push((cbor_text("next_cursor"), cbor_bytes(csil_inner)));
    }
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a SavedAnalysisList from a decoded CBOR value tree.
fn csil_dec_saved_analysis_list(
    csil_root: &CsilCborValue,
) -> Result<SavedAnalysisList, CsilCborError> {
    let analyses = {
        let csil_field = cbor_require(csil_root, "analyses")?;
        let csil_decode = |csil_v| cbor_dec_array(csil_v, csil_dec_saved_analysis);
        csil_decode(csil_field)?
    };
    let next_cursor = match cbor_map_get(csil_root, "next_cursor") {
        Some(csil_field) => {
            let csil_decode = cbor_as_bytes;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    Ok(SavedAnalysisList {
        analyses,
        next_cursor,
    })
}

/// Encode a SavedAnalysisList to canonical CSIL CBOR bytes.
pub fn encode_saved_analysis_list(csil_v: &SavedAnalysisList) -> Vec<u8> {
    cbor_encode(&csil_enc_saved_analysis_list(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a SavedAnalysisList.
pub fn decode_saved_analysis_list(csil_data: &[u8]) -> Result<SavedAnalysisList, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_saved_analysis_list(&csil_root)
}

/// Build the canonical CBOR value tree for a DashboardPanel.
fn csil_enc_dashboard_panel(csil_v: &DashboardPanel) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(6);
    csil_entries.push((cbor_text("row"), cbor_uint(csil_v.row)));
    if let Some(csil_inner) = &csil_v.title {
        csil_entries.push((cbor_text("title"), cbor_text(csil_inner)));
    }
    csil_entries.push((cbor_text("width"), cbor_uint(csil_v.width)));
    csil_entries.push((cbor_text("column"), cbor_uint(csil_v.column)));
    csil_entries.push((cbor_text("height"), cbor_uint(csil_v.height)));
    csil_entries.push((cbor_text("analysis_id"), cbor_text(&csil_v.analysis_id)));
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a DashboardPanel from a decoded CBOR value tree.
fn csil_dec_dashboard_panel(csil_root: &CsilCborValue) -> Result<DashboardPanel, CsilCborError> {
    let analysis_id = {
        let csil_field = cbor_require(csil_root, "analysis_id")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let title = match cbor_map_get(csil_root, "title") {
        Some(csil_field) => {
            let csil_decode = cbor_as_text;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let column = {
        let csil_field = cbor_require(csil_root, "column")?;
        let csil_decode = cbor_as_u64;
        csil_decode(csil_field)?
    };
    let row = {
        let csil_field = cbor_require(csil_root, "row")?;
        let csil_decode = cbor_as_u64;
        csil_decode(csil_field)?
    };
    let width = {
        let csil_field = cbor_require(csil_root, "width")?;
        let csil_decode = cbor_as_u64;
        csil_decode(csil_field)?
    };
    let height = {
        let csil_field = cbor_require(csil_root, "height")?;
        let csil_decode = cbor_as_u64;
        csil_decode(csil_field)?
    };
    Ok(DashboardPanel {
        analysis_id,
        title,
        column,
        row,
        width,
        height,
    })
}

/// Encode a DashboardPanel to canonical CSIL CBOR bytes.
pub fn encode_dashboard_panel(csil_v: &DashboardPanel) -> Vec<u8> {
    cbor_encode(&csil_enc_dashboard_panel(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a DashboardPanel.
pub fn decode_dashboard_panel(csil_data: &[u8]) -> Result<DashboardPanel, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_dashboard_panel(&csil_root)
}

/// Build the canonical CBOR value tree for a SavedDashboard.
fn csil_enc_saved_dashboard(csil_v: &SavedDashboard) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(6);
    csil_entries.push((cbor_text("name"), cbor_text(&csil_v.name)));
    csil_entries.push((
        cbor_text("panels"),
        cbor_enc_array(&csil_v.panels, csil_enc_dashboard_panel),
    ));
    csil_entries.push((cbor_text("project_id"), cbor_bytes(&csil_v.project_id)));
    if let Some(csil_inner) = &csil_v.updated_at {
        csil_entries.push((cbor_text("updated_at"), cbor_int(*csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.updated_by {
        csil_entries.push((cbor_text("updated_by"), cbor_text(csil_inner)));
    }
    csil_entries.push((cbor_text("dashboard_id"), cbor_text(&csil_v.dashboard_id)));
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a SavedDashboard from a decoded CBOR value tree.
fn csil_dec_saved_dashboard(csil_root: &CsilCborValue) -> Result<SavedDashboard, CsilCborError> {
    let dashboard_id = {
        let csil_field = cbor_require(csil_root, "dashboard_id")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let project_id = {
        let csil_field = cbor_require(csil_root, "project_id")?;
        let csil_decode = cbor_as_bytes;
        csil_decode(csil_field)?
    };
    let name = {
        let csil_field = cbor_require(csil_root, "name")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let panels = {
        let csil_field = cbor_require(csil_root, "panels")?;
        let csil_decode = |csil_v| cbor_dec_array(csil_v, csil_dec_dashboard_panel);
        csil_decode(csil_field)?
    };
    let updated_at = match cbor_map_get(csil_root, "updated_at") {
        Some(csil_field) => {
            let csil_decode = cbor_as_i64;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let updated_by = match cbor_map_get(csil_root, "updated_by") {
        Some(csil_field) => {
            let csil_decode = cbor_as_text;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    Ok(SavedDashboard {
        dashboard_id,
        project_id,
        name,
        panels,
        updated_at,
        updated_by,
    })
}

/// Encode a SavedDashboard to canonical CSIL CBOR bytes.
pub fn encode_saved_dashboard(csil_v: &SavedDashboard) -> Vec<u8> {
    cbor_encode(&csil_enc_saved_dashboard(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a SavedDashboard.
pub fn decode_saved_dashboard(csil_data: &[u8]) -> Result<SavedDashboard, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_saved_dashboard(&csil_root)
}

/// Build the canonical CBOR value tree for a SavedDashboardList.
fn csil_enc_saved_dashboard_list(csil_v: &SavedDashboardList) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(2);
    csil_entries.push((
        cbor_text("dashboards"),
        cbor_enc_array(&csil_v.dashboards, csil_enc_saved_dashboard),
    ));
    if let Some(csil_inner) = &csil_v.next_cursor {
        csil_entries.push((cbor_text("next_cursor"), cbor_bytes(csil_inner)));
    }
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a SavedDashboardList from a decoded CBOR value tree.
fn csil_dec_saved_dashboard_list(
    csil_root: &CsilCborValue,
) -> Result<SavedDashboardList, CsilCborError> {
    let dashboards = {
        let csil_field = cbor_require(csil_root, "dashboards")?;
        let csil_decode = |csil_v| cbor_dec_array(csil_v, csil_dec_saved_dashboard);
        csil_decode(csil_field)?
    };
    let next_cursor = match cbor_map_get(csil_root, "next_cursor") {
        Some(csil_field) => {
            let csil_decode = cbor_as_bytes;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    Ok(SavedDashboardList {
        dashboards,
        next_cursor,
    })
}

/// Encode a SavedDashboardList to canonical CSIL CBOR bytes.
pub fn encode_saved_dashboard_list(csil_v: &SavedDashboardList) -> Vec<u8> {
    cbor_encode(&csil_enc_saved_dashboard_list(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a SavedDashboardList.
pub fn decode_saved_dashboard_list(csil_data: &[u8]) -> Result<SavedDashboardList, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_saved_dashboard_list(&csil_root)
}

/// Build the canonical CBOR value tree for a SavedRequest.
fn csil_enc_saved_request(csil_v: &SavedRequest) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(4);
    if let Some(csil_inner) = &csil_v.id {
        csil_entries.push((cbor_text("id"), cbor_text(csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.limit {
        csil_entries.push((cbor_text("limit"), cbor_uint(*csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.cursor {
        csil_entries.push((cbor_text("cursor"), cbor_bytes(csil_inner)));
    }
    csil_entries.push((cbor_text("project_id"), cbor_bytes(&csil_v.project_id)));
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a SavedRequest from a decoded CBOR value tree.
fn csil_dec_saved_request(csil_root: &CsilCborValue) -> Result<SavedRequest, CsilCborError> {
    let project_id = {
        let csil_field = cbor_require(csil_root, "project_id")?;
        let csil_decode = cbor_as_bytes;
        csil_decode(csil_field)?
    };
    let id = match cbor_map_get(csil_root, "id") {
        Some(csil_field) => {
            let csil_decode = cbor_as_text;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let cursor = match cbor_map_get(csil_root, "cursor") {
        Some(csil_field) => {
            let csil_decode = cbor_as_bytes;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let limit = match cbor_map_get(csil_root, "limit") {
        Some(csil_field) => {
            let csil_decode = cbor_as_u64;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    Ok(SavedRequest {
        project_id,
        id,
        cursor,
        limit,
    })
}

/// Encode a SavedRequest to canonical CSIL CBOR bytes.
pub fn encode_saved_request(csil_v: &SavedRequest) -> Vec<u8> {
    cbor_encode(&csil_enc_saved_request(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a SavedRequest.
pub fn decode_saved_request(csil_data: &[u8]) -> Result<SavedRequest, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_saved_request(&csil_root)
}

/// Build the canonical CBOR value tree for a RoleTokenPolicy.
fn csil_enc_role_token_policy(csil_v: &RoleTokenPolicy) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(11);
    if let Some(csil_inner) = &csil_v.cells {
        csil_entries.push((
            cbor_text("cells"),
            cbor_enc_array(csil_inner, |csil_elem| cbor_text(csil_elem)),
        ));
    }
    csil_entries.push((
        cbor_text("roles"),
        cbor_enc_array(&csil_v.roles, csil_enc_node_role),
    ));
    if let Some(csil_inner) = &csil_v.regions {
        csil_entries.push((
            cbor_text("regions"),
            cbor_enc_array(csil_inner, |csil_elem| cbor_text(csil_elem)),
        ));
    }
    if let Some(csil_inner) = &csil_v.max_uses {
        csil_entries.push((cbor_text("max_uses"), cbor_uint(*csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.projects {
        csil_entries.push((
            cbor_text("projects"),
            cbor_enc_array(csil_inner, |csil_elem| cbor_bytes(csil_elem)),
        ));
    }
    if let Some(csil_inner) = &csil_v.expires_at {
        csil_entries.push((cbor_text("expires_at"), cbor_int(*csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.workspaces {
        csil_entries.push((
            cbor_text("workspaces"),
            cbor_enc_array(csil_inner, |csil_elem| cbor_bytes(csil_elem)),
        ));
    }
    if let Some(csil_inner) = &csil_v.audit_labels {
        csil_entries.push((
            cbor_text("audit_labels"),
            cbor_enc_array(csil_inner, |csil_elem| cbor_text(csil_elem)),
        ));
    }
    if let Some(csil_inner) = &csil_v.max_active_nodes {
        csil_entries.push((cbor_text("max_active_nodes"), cbor_uint(*csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.enrollments_each_hour {
        csil_entries.push((cbor_text("enrollments_each_hour"), cbor_uint(*csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.certificate_lifetime_ms {
        csil_entries.push((cbor_text("certificate_lifetime_ms"), cbor_uint(*csil_inner)));
    }
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a RoleTokenPolicy from a decoded CBOR value tree.
fn csil_dec_role_token_policy(csil_root: &CsilCborValue) -> Result<RoleTokenPolicy, CsilCborError> {
    let roles = {
        let csil_field = cbor_require(csil_root, "roles")?;
        let csil_decode = |csil_v| cbor_dec_array(csil_v, csil_dec_node_role);
        csil_decode(csil_field)?
    };
    let cells = match cbor_map_get(csil_root, "cells") {
        Some(csil_field) => {
            let csil_decode = |csil_v| cbor_dec_array(csil_v, cbor_as_text);
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let regions = match cbor_map_get(csil_root, "regions") {
        Some(csil_field) => {
            let csil_decode = |csil_v| cbor_dec_array(csil_v, cbor_as_text);
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let workspaces = match cbor_map_get(csil_root, "workspaces") {
        Some(csil_field) => {
            let csil_decode = |csil_v| cbor_dec_array(csil_v, cbor_as_bytes);
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let projects = match cbor_map_get(csil_root, "projects") {
        Some(csil_field) => {
            let csil_decode = |csil_v| cbor_dec_array(csil_v, cbor_as_bytes);
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let expires_at = match cbor_map_get(csil_root, "expires_at") {
        Some(csil_field) => {
            let csil_decode = cbor_as_i64;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let max_uses = match cbor_map_get(csil_root, "max_uses") {
        Some(csil_field) => {
            let csil_decode = cbor_as_u64;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let max_active_nodes = match cbor_map_get(csil_root, "max_active_nodes") {
        Some(csil_field) => {
            let csil_decode = cbor_as_u64;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let certificate_lifetime_ms = match cbor_map_get(csil_root, "certificate_lifetime_ms") {
        Some(csil_field) => {
            let csil_decode = cbor_as_u64;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let enrollments_each_hour = match cbor_map_get(csil_root, "enrollments_each_hour") {
        Some(csil_field) => {
            let csil_decode = cbor_as_u64;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let audit_labels = match cbor_map_get(csil_root, "audit_labels") {
        Some(csil_field) => {
            let csil_decode = |csil_v| cbor_dec_array(csil_v, cbor_as_text);
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    Ok(RoleTokenPolicy {
        roles,
        cells,
        regions,
        workspaces,
        projects,
        expires_at,
        max_uses,
        max_active_nodes,
        certificate_lifetime_ms,
        enrollments_each_hour,
        audit_labels,
    })
}

/// Encode a RoleTokenPolicy to canonical CSIL CBOR bytes.
pub fn encode_role_token_policy(csil_v: &RoleTokenPolicy) -> Vec<u8> {
    cbor_encode(&csil_enc_role_token_policy(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a RoleTokenPolicy.
pub fn decode_role_token_policy(csil_data: &[u8]) -> Result<RoleTokenPolicy, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_role_token_policy(&csil_root)
}

/// Build the canonical CBOR value tree for a CreateRoleTokenRequest.
fn csil_enc_create_role_token_request(csil_v: &CreateRoleTokenRequest) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(2);
    csil_entries.push((cbor_text("label"), cbor_text(&csil_v.label)));
    csil_entries.push((
        cbor_text("policy"),
        csil_enc_role_token_policy(&csil_v.policy),
    ));
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a CreateRoleTokenRequest from a decoded CBOR value tree.
fn csil_dec_create_role_token_request(
    csil_root: &CsilCborValue,
) -> Result<CreateRoleTokenRequest, CsilCborError> {
    let label = {
        let csil_field = cbor_require(csil_root, "label")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let policy = {
        let csil_field = cbor_require(csil_root, "policy")?;
        let csil_decode = csil_dec_role_token_policy;
        csil_decode(csil_field)?
    };
    Ok(CreateRoleTokenRequest { label, policy })
}

/// Encode a CreateRoleTokenRequest to canonical CSIL CBOR bytes.
pub fn encode_create_role_token_request(csil_v: &CreateRoleTokenRequest) -> Vec<u8> {
    cbor_encode(&csil_enc_create_role_token_request(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a CreateRoleTokenRequest.
pub fn decode_create_role_token_request(
    csil_data: &[u8],
) -> Result<CreateRoleTokenRequest, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_create_role_token_request(&csil_root)
}

/// Build the canonical CBOR value tree for a CreateRoleTokenResponse.
fn csil_enc_create_role_token_response(csil_v: &CreateRoleTokenResponse) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(4);
    csil_entries.push((cbor_text("token"), cbor_text(&csil_v.token)));
    csil_entries.push((
        cbor_text("policy"),
        csil_enc_role_token_policy(&csil_v.policy),
    ));
    csil_entries.push((cbor_text("token_id"), cbor_text(&csil_v.token_id)));
    csil_entries.push((cbor_text("created_at"), cbor_int(csil_v.created_at)));
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a CreateRoleTokenResponse from a decoded CBOR value tree.
fn csil_dec_create_role_token_response(
    csil_root: &CsilCborValue,
) -> Result<CreateRoleTokenResponse, CsilCborError> {
    let token_id = {
        let csil_field = cbor_require(csil_root, "token_id")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let token = {
        let csil_field = cbor_require(csil_root, "token")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let policy = {
        let csil_field = cbor_require(csil_root, "policy")?;
        let csil_decode = csil_dec_role_token_policy;
        csil_decode(csil_field)?
    };
    let created_at = {
        let csil_field = cbor_require(csil_root, "created_at")?;
        let csil_decode = cbor_as_i64;
        csil_decode(csil_field)?
    };
    Ok(CreateRoleTokenResponse {
        token_id,
        token,
        policy,
        created_at,
    })
}

/// Encode a CreateRoleTokenResponse to canonical CSIL CBOR bytes.
pub fn encode_create_role_token_response(csil_v: &CreateRoleTokenResponse) -> Vec<u8> {
    cbor_encode(&csil_enc_create_role_token_response(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a CreateRoleTokenResponse.
pub fn decode_create_role_token_response(
    csil_data: &[u8],
) -> Result<CreateRoleTokenResponse, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_create_role_token_response(&csil_root)
}

/// Build the canonical CBOR value tree for a RoleTokenSummary.
fn csil_enc_role_token_summary(csil_v: &RoleTokenSummary) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(8);
    csil_entries.push((cbor_text("uses"), cbor_uint(csil_v.uses)));
    csil_entries.push((cbor_text("label"), cbor_text(&csil_v.label)));
    csil_entries.push((
        cbor_text("policy"),
        csil_enc_role_token_policy(&csil_v.policy),
    ));
    csil_entries.push((cbor_text("revoked"), cbor_bool(csil_v.revoked)));
    csil_entries.push((cbor_text("token_id"), cbor_text(&csil_v.token_id)));
    csil_entries.push((cbor_text("created_at"), cbor_int(csil_v.created_at)));
    csil_entries.push((cbor_text("active_nodes"), cbor_uint(csil_v.active_nodes)));
    if let Some(csil_inner) = &csil_v.last_used_at {
        csil_entries.push((cbor_text("last_used_at"), cbor_int(*csil_inner)));
    }
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a RoleTokenSummary from a decoded CBOR value tree.
fn csil_dec_role_token_summary(
    csil_root: &CsilCborValue,
) -> Result<RoleTokenSummary, CsilCborError> {
    let token_id = {
        let csil_field = cbor_require(csil_root, "token_id")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let label = {
        let csil_field = cbor_require(csil_root, "label")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let policy = {
        let csil_field = cbor_require(csil_root, "policy")?;
        let csil_decode = csil_dec_role_token_policy;
        csil_decode(csil_field)?
    };
    let created_at = {
        let csil_field = cbor_require(csil_root, "created_at")?;
        let csil_decode = cbor_as_i64;
        csil_decode(csil_field)?
    };
    let uses = {
        let csil_field = cbor_require(csil_root, "uses")?;
        let csil_decode = cbor_as_u64;
        csil_decode(csil_field)?
    };
    let active_nodes = {
        let csil_field = cbor_require(csil_root, "active_nodes")?;
        let csil_decode = cbor_as_u64;
        csil_decode(csil_field)?
    };
    let last_used_at = match cbor_map_get(csil_root, "last_used_at") {
        Some(csil_field) => {
            let csil_decode = cbor_as_i64;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let revoked = {
        let csil_field = cbor_require(csil_root, "revoked")?;
        let csil_decode = cbor_as_bool;
        csil_decode(csil_field)?
    };
    Ok(RoleTokenSummary {
        token_id,
        label,
        policy,
        created_at,
        uses,
        active_nodes,
        last_used_at,
        revoked,
    })
}

/// Encode a RoleTokenSummary to canonical CSIL CBOR bytes.
pub fn encode_role_token_summary(csil_v: &RoleTokenSummary) -> Vec<u8> {
    cbor_encode(&csil_enc_role_token_summary(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a RoleTokenSummary.
pub fn decode_role_token_summary(csil_data: &[u8]) -> Result<RoleTokenSummary, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_role_token_summary(&csil_root)
}

/// Build the canonical CBOR value tree for a RoleTokenList.
fn csil_enc_role_token_list(csil_v: &RoleTokenList) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(2);
    csil_entries.push((
        cbor_text("tokens"),
        cbor_enc_array(&csil_v.tokens, csil_enc_role_token_summary),
    ));
    if let Some(csil_inner) = &csil_v.next_cursor {
        csil_entries.push((cbor_text("next_cursor"), cbor_bytes(csil_inner)));
    }
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a RoleTokenList from a decoded CBOR value tree.
fn csil_dec_role_token_list(csil_root: &CsilCborValue) -> Result<RoleTokenList, CsilCborError> {
    let tokens = {
        let csil_field = cbor_require(csil_root, "tokens")?;
        let csil_decode = |csil_v| cbor_dec_array(csil_v, csil_dec_role_token_summary);
        csil_decode(csil_field)?
    };
    let next_cursor = match cbor_map_get(csil_root, "next_cursor") {
        Some(csil_field) => {
            let csil_decode = cbor_as_bytes;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    Ok(RoleTokenList {
        tokens,
        next_cursor,
    })
}

/// Encode a RoleTokenList to canonical CSIL CBOR bytes.
pub fn encode_role_token_list(csil_v: &RoleTokenList) -> Vec<u8> {
    cbor_encode(&csil_enc_role_token_list(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a RoleTokenList.
pub fn decode_role_token_list(csil_data: &[u8]) -> Result<RoleTokenList, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_role_token_list(&csil_root)
}

/// Build the canonical CBOR value tree for a RevokeRoleTokenRequest.
fn csil_enc_revoke_role_token_request(csil_v: &RevokeRoleTokenRequest) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(2);
    if let Some(csil_inner) = &csil_v.cascade {
        csil_entries.push((cbor_text("cascade"), cbor_bool(*csil_inner)));
    }
    csil_entries.push((cbor_text("token_id"), cbor_text(&csil_v.token_id)));
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a RevokeRoleTokenRequest from a decoded CBOR value tree.
fn csil_dec_revoke_role_token_request(
    csil_root: &CsilCborValue,
) -> Result<RevokeRoleTokenRequest, CsilCborError> {
    let token_id = {
        let csil_field = cbor_require(csil_root, "token_id")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let cascade = match cbor_map_get(csil_root, "cascade") {
        Some(csil_field) => {
            let csil_decode = cbor_as_bool;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    Ok(RevokeRoleTokenRequest { token_id, cascade })
}

/// Encode a RevokeRoleTokenRequest to canonical CSIL CBOR bytes.
pub fn encode_revoke_role_token_request(csil_v: &RevokeRoleTokenRequest) -> Vec<u8> {
    cbor_encode(&csil_enc_revoke_role_token_request(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a RevokeRoleTokenRequest.
pub fn decode_revoke_role_token_request(
    csil_data: &[u8],
) -> Result<RevokeRoleTokenRequest, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_revoke_role_token_request(&csil_root)
}

/// Build the canonical CBOR value tree for a NodeCapabilities.
fn csil_enc_node_capabilities(csil_v: &NodeCapabilities) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(6);
    if let Some(csil_inner) = &csil_v.storage_bytes {
        csil_entries.push((cbor_text("storage_bytes"), cbor_uint(*csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.segment_versions {
        csil_entries.push((
            cbor_text("segment_versions"),
            cbor_enc_array(csil_inner, |csil_elem| cbor_uint(*csil_elem)),
        ));
    }
    csil_entries.push((
        cbor_text("software_version"),
        cbor_text(&csil_v.software_version),
    ));
    if let Some(csil_inner) = &csil_v.policy_generation {
        csil_entries.push((cbor_text("policy_generation"), cbor_uint(*csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.protocol_versions {
        csil_entries.push((
            cbor_text("protocol_versions"),
            cbor_enc_array(csil_inner, |csil_elem| cbor_uint(*csil_elem)),
        ));
    }
    if let Some(csil_inner) = &csil_v.compression_codecs {
        csil_entries.push((
            cbor_text("compression_codecs"),
            cbor_enc_array(csil_inner, |csil_elem| cbor_text(csil_elem)),
        ));
    }
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a NodeCapabilities from a decoded CBOR value tree.
fn csil_dec_node_capabilities(
    csil_root: &CsilCborValue,
) -> Result<NodeCapabilities, CsilCborError> {
    let software_version = {
        let csil_field = cbor_require(csil_root, "software_version")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let protocol_versions = match cbor_map_get(csil_root, "protocol_versions") {
        Some(csil_field) => {
            let csil_decode = |csil_v| cbor_dec_array(csil_v, cbor_as_u64);
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let segment_versions = match cbor_map_get(csil_root, "segment_versions") {
        Some(csil_field) => {
            let csil_decode = |csil_v| cbor_dec_array(csil_v, cbor_as_u64);
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let compression_codecs = match cbor_map_get(csil_root, "compression_codecs") {
        Some(csil_field) => {
            let csil_decode = |csil_v| cbor_dec_array(csil_v, cbor_as_text);
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let storage_bytes = match cbor_map_get(csil_root, "storage_bytes") {
        Some(csil_field) => {
            let csil_decode = cbor_as_u64;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let policy_generation = match cbor_map_get(csil_root, "policy_generation") {
        Some(csil_field) => {
            let csil_decode = cbor_as_u64;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    Ok(NodeCapabilities {
        software_version,
        protocol_versions,
        segment_versions,
        compression_codecs,
        storage_bytes,
        policy_generation,
    })
}

/// Encode a NodeCapabilities to canonical CSIL CBOR bytes.
pub fn encode_node_capabilities(csil_v: &NodeCapabilities) -> Vec<u8> {
    cbor_encode(&csil_enc_node_capabilities(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a NodeCapabilities.
pub fn decode_node_capabilities(csil_data: &[u8]) -> Result<NodeCapabilities, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_node_capabilities(&csil_root)
}

/// Build the canonical CBOR value tree for a EnrollNodeRequest.
fn csil_enc_enroll_node_request(csil_v: &EnrollNodeRequest) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(7);
    if let Some(csil_inner) = &csil_v.cell {
        csil_entries.push((cbor_text("cell"), cbor_text(csil_inner)));
    }
    csil_entries.push((cbor_text("token"), cbor_text(&csil_v.token)));
    if let Some(csil_inner) = &csil_v.region {
        csil_entries.push((cbor_text("region"), cbor_text(csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.node_id {
        csil_entries.push((cbor_text("node_id"), cbor_text(csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.capabilities {
        csil_entries.push((
            cbor_text("capabilities"),
            csil_enc_node_capabilities(csil_inner),
        ));
    }
    csil_entries.push((
        cbor_text("requested_role"),
        csil_enc_node_role(&csil_v.requested_role),
    ));
    csil_entries.push((
        cbor_text("certificate_request"),
        cbor_bytes(&csil_v.certificate_request),
    ));
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a EnrollNodeRequest from a decoded CBOR value tree.
fn csil_dec_enroll_node_request(
    csil_root: &CsilCborValue,
) -> Result<EnrollNodeRequest, CsilCborError> {
    let token = {
        let csil_field = cbor_require(csil_root, "token")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let certificate_request = {
        let csil_field = cbor_require(csil_root, "certificate_request")?;
        let csil_decode = cbor_as_bytes;
        csil_decode(csil_field)?
    };
    let requested_role = {
        let csil_field = cbor_require(csil_root, "requested_role")?;
        let csil_decode = csil_dec_node_role;
        csil_decode(csil_field)?
    };
    let cell = match cbor_map_get(csil_root, "cell") {
        Some(csil_field) => {
            let csil_decode = cbor_as_text;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let region = match cbor_map_get(csil_root, "region") {
        Some(csil_field) => {
            let csil_decode = cbor_as_text;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let node_id = match cbor_map_get(csil_root, "node_id") {
        Some(csil_field) => {
            let csil_decode = cbor_as_text;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let capabilities = match cbor_map_get(csil_root, "capabilities") {
        Some(csil_field) => {
            let csil_decode = csil_dec_node_capabilities;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    Ok(EnrollNodeRequest {
        token,
        certificate_request,
        requested_role,
        cell,
        region,
        node_id,
        capabilities,
    })
}

/// Encode a EnrollNodeRequest to canonical CSIL CBOR bytes.
pub fn encode_enroll_node_request(csil_v: &EnrollNodeRequest) -> Vec<u8> {
    cbor_encode(&csil_enc_enroll_node_request(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a EnrollNodeRequest.
pub fn decode_enroll_node_request(csil_data: &[u8]) -> Result<EnrollNodeRequest, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_enroll_node_request(&csil_root)
}

/// Build the canonical CBOR value tree for a EnrollNodeResponse.
fn csil_enc_enroll_node_response(csil_v: &EnrollNodeResponse) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(10);
    if let Some(csil_inner) = &csil_v.cell {
        csil_entries.push((cbor_text("cell"), cbor_text(csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.region {
        csil_entries.push((cbor_text("region"), cbor_text(csil_inner)));
    }
    csil_entries.push((cbor_text("node_id"), cbor_text(&csil_v.node_id)));
    csil_entries.push((cbor_text("issued_at"), cbor_int(csil_v.issued_at)));
    csil_entries.push((cbor_text("expires_at"), cbor_int(csil_v.expires_at)));
    csil_entries.push((cbor_text("renew_after"), cbor_int(csil_v.renew_after)));
    csil_entries.push((
        cbor_text("effective_role"),
        csil_enc_node_role(&csil_v.effective_role),
    ));
    csil_entries.push((
        cbor_text("certificate_chain"),
        cbor_enc_array(&csil_v.certificate_chain, |csil_elem| cbor_bytes(csil_elem)),
    ));
    csil_entries.push((
        cbor_text("certificate_serial"),
        cbor_text(&csil_v.certificate_serial),
    ));
    if let Some(csil_inner) = &csil_v.permitted_capabilities {
        csil_entries.push((
            cbor_text("permitted_capabilities"),
            csil_enc_node_capabilities(csil_inner),
        ));
    }
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a EnrollNodeResponse from a decoded CBOR value tree.
fn csil_dec_enroll_node_response(
    csil_root: &CsilCborValue,
) -> Result<EnrollNodeResponse, CsilCborError> {
    let node_id = {
        let csil_field = cbor_require(csil_root, "node_id")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let certificate_chain = {
        let csil_field = cbor_require(csil_root, "certificate_chain")?;
        let csil_decode = |csil_v| cbor_dec_array(csil_v, cbor_as_bytes);
        csil_decode(csil_field)?
    };
    let certificate_serial = {
        let csil_field = cbor_require(csil_root, "certificate_serial")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let effective_role = {
        let csil_field = cbor_require(csil_root, "effective_role")?;
        let csil_decode = csil_dec_node_role;
        csil_decode(csil_field)?
    };
    let cell = match cbor_map_get(csil_root, "cell") {
        Some(csil_field) => {
            let csil_decode = cbor_as_text;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let region = match cbor_map_get(csil_root, "region") {
        Some(csil_field) => {
            let csil_decode = cbor_as_text;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let issued_at = {
        let csil_field = cbor_require(csil_root, "issued_at")?;
        let csil_decode = cbor_as_i64;
        csil_decode(csil_field)?
    };
    let expires_at = {
        let csil_field = cbor_require(csil_root, "expires_at")?;
        let csil_decode = cbor_as_i64;
        csil_decode(csil_field)?
    };
    let renew_after = {
        let csil_field = cbor_require(csil_root, "renew_after")?;
        let csil_decode = cbor_as_i64;
        csil_decode(csil_field)?
    };
    let permitted_capabilities = match cbor_map_get(csil_root, "permitted_capabilities") {
        Some(csil_field) => {
            let csil_decode = csil_dec_node_capabilities;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    Ok(EnrollNodeResponse {
        node_id,
        certificate_chain,
        certificate_serial,
        effective_role,
        cell,
        region,
        issued_at,
        expires_at,
        renew_after,
        permitted_capabilities,
    })
}

/// Encode a EnrollNodeResponse to canonical CSIL CBOR bytes.
pub fn encode_enroll_node_response(csil_v: &EnrollNodeResponse) -> Vec<u8> {
    cbor_encode(&csil_enc_enroll_node_response(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a EnrollNodeResponse.
pub fn decode_enroll_node_response(csil_data: &[u8]) -> Result<EnrollNodeResponse, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_enroll_node_response(&csil_root)
}

/// Build the canonical CBOR value tree for a RenewNodeCertificateRequest.
fn csil_enc_renew_node_certificate_request(csil_v: &RenewNodeCertificateRequest) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(2);
    csil_entries.push((cbor_text("node_id"), cbor_text(&csil_v.node_id)));
    csil_entries.push((
        cbor_text("certificate_request"),
        cbor_bytes(&csil_v.certificate_request),
    ));
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a RenewNodeCertificateRequest from a decoded CBOR value tree.
fn csil_dec_renew_node_certificate_request(
    csil_root: &CsilCborValue,
) -> Result<RenewNodeCertificateRequest, CsilCborError> {
    let node_id = {
        let csil_field = cbor_require(csil_root, "node_id")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let certificate_request = {
        let csil_field = cbor_require(csil_root, "certificate_request")?;
        let csil_decode = cbor_as_bytes;
        csil_decode(csil_field)?
    };
    Ok(RenewNodeCertificateRequest {
        node_id,
        certificate_request,
    })
}

/// Encode a RenewNodeCertificateRequest to canonical CSIL CBOR bytes.
pub fn encode_renew_node_certificate_request(csil_v: &RenewNodeCertificateRequest) -> Vec<u8> {
    cbor_encode(&csil_enc_renew_node_certificate_request(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a RenewNodeCertificateRequest.
pub fn decode_renew_node_certificate_request(
    csil_data: &[u8],
) -> Result<RenewNodeCertificateRequest, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_renew_node_certificate_request(&csil_root)
}

/// Build the canonical CBOR value tree for a NodeSummary.
fn csil_enc_node_summary(csil_v: &NodeSummary) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(9);
    if let Some(csil_inner) = &csil_v.cell {
        csil_entries.push((cbor_text("cell"), cbor_text(csil_inner)));
    }
    csil_entries.push((cbor_text("role"), csil_enc_node_role(&csil_v.role)));
    if let Some(csil_inner) = &csil_v.region {
        csil_entries.push((cbor_text("region"), cbor_text(csil_inner)));
    }
    csil_entries.push((cbor_text("node_id"), cbor_text(&csil_v.node_id)));
    csil_entries.push((cbor_text("revoked"), cbor_bool(csil_v.revoked)));
    csil_entries.push((cbor_text("token_id"), cbor_text(&csil_v.token_id)));
    csil_entries.push((cbor_text("expires_at"), cbor_int(csil_v.expires_at)));
    csil_entries.push((cbor_text("enrolled_at"), cbor_int(csil_v.enrolled_at)));
    csil_entries.push((
        cbor_text("certificate_serial"),
        cbor_text(&csil_v.certificate_serial),
    ));
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a NodeSummary from a decoded CBOR value tree.
fn csil_dec_node_summary(csil_root: &CsilCborValue) -> Result<NodeSummary, CsilCborError> {
    let node_id = {
        let csil_field = cbor_require(csil_root, "node_id")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let token_id = {
        let csil_field = cbor_require(csil_root, "token_id")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let role = {
        let csil_field = cbor_require(csil_root, "role")?;
        let csil_decode = csil_dec_node_role;
        csil_decode(csil_field)?
    };
    let cell = match cbor_map_get(csil_root, "cell") {
        Some(csil_field) => {
            let csil_decode = cbor_as_text;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let region = match cbor_map_get(csil_root, "region") {
        Some(csil_field) => {
            let csil_decode = cbor_as_text;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let certificate_serial = {
        let csil_field = cbor_require(csil_root, "certificate_serial")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let enrolled_at = {
        let csil_field = cbor_require(csil_root, "enrolled_at")?;
        let csil_decode = cbor_as_i64;
        csil_decode(csil_field)?
    };
    let expires_at = {
        let csil_field = cbor_require(csil_root, "expires_at")?;
        let csil_decode = cbor_as_i64;
        csil_decode(csil_field)?
    };
    let revoked = {
        let csil_field = cbor_require(csil_root, "revoked")?;
        let csil_decode = cbor_as_bool;
        csil_decode(csil_field)?
    };
    Ok(NodeSummary {
        node_id,
        token_id,
        role,
        cell,
        region,
        certificate_serial,
        enrolled_at,
        expires_at,
        revoked,
    })
}

/// Encode a NodeSummary to canonical CSIL CBOR bytes.
pub fn encode_node_summary(csil_v: &NodeSummary) -> Vec<u8> {
    cbor_encode(&csil_enc_node_summary(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a NodeSummary.
pub fn decode_node_summary(csil_data: &[u8]) -> Result<NodeSummary, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_node_summary(&csil_root)
}

/// Build the canonical CBOR value tree for a NodeList.
fn csil_enc_node_list(csil_v: &NodeList) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(2);
    csil_entries.push((
        cbor_text("nodes"),
        cbor_enc_array(&csil_v.nodes, csil_enc_node_summary),
    ));
    if let Some(csil_inner) = &csil_v.next_cursor {
        csil_entries.push((cbor_text("next_cursor"), cbor_bytes(csil_inner)));
    }
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a NodeList from a decoded CBOR value tree.
fn csil_dec_node_list(csil_root: &CsilCborValue) -> Result<NodeList, CsilCborError> {
    let nodes = {
        let csil_field = cbor_require(csil_root, "nodes")?;
        let csil_decode = |csil_v| cbor_dec_array(csil_v, csil_dec_node_summary);
        csil_decode(csil_field)?
    };
    let next_cursor = match cbor_map_get(csil_root, "next_cursor") {
        Some(csil_field) => {
            let csil_decode = cbor_as_bytes;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    Ok(NodeList { nodes, next_cursor })
}

/// Encode a NodeList to canonical CSIL CBOR bytes.
pub fn encode_node_list(csil_v: &NodeList) -> Vec<u8> {
    cbor_encode(&csil_enc_node_list(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a NodeList.
pub fn decode_node_list(csil_data: &[u8]) -> Result<NodeList, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_node_list(&csil_root)
}

/// Build the canonical CBOR value tree for a ListRequest.
fn csil_enc_list_request(csil_v: &ListRequest) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(2);
    if let Some(csil_inner) = &csil_v.limit {
        csil_entries.push((cbor_text("limit"), cbor_uint(*csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.cursor {
        csil_entries.push((cbor_text("cursor"), cbor_bytes(csil_inner)));
    }
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a ListRequest from a decoded CBOR value tree.
fn csil_dec_list_request(csil_root: &CsilCborValue) -> Result<ListRequest, CsilCborError> {
    let cursor = match cbor_map_get(csil_root, "cursor") {
        Some(csil_field) => {
            let csil_decode = cbor_as_bytes;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let limit = match cbor_map_get(csil_root, "limit") {
        Some(csil_field) => {
            let csil_decode = cbor_as_u64;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    Ok(ListRequest { cursor, limit })
}

/// Encode a ListRequest to canonical CSIL CBOR bytes.
pub fn encode_list_request(csil_v: &ListRequest) -> Vec<u8> {
    cbor_encode(&csil_enc_list_request(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a ListRequest.
pub fn decode_list_request(csil_data: &[u8]) -> Result<ListRequest, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_list_request(&csil_root)
}

/// Build the canonical CBOR value tree for a WorkspaceList.
fn csil_enc_workspace_list(csil_v: &WorkspaceList) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(2);
    csil_entries.push((
        cbor_text("workspaces"),
        cbor_enc_array(&csil_v.workspaces, csil_enc_workspace),
    ));
    if let Some(csil_inner) = &csil_v.next_cursor {
        csil_entries.push((cbor_text("next_cursor"), cbor_bytes(csil_inner)));
    }
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a WorkspaceList from a decoded CBOR value tree.
fn csil_dec_workspace_list(csil_root: &CsilCborValue) -> Result<WorkspaceList, CsilCborError> {
    let workspaces = {
        let csil_field = cbor_require(csil_root, "workspaces")?;
        let csil_decode = |csil_v| cbor_dec_array(csil_v, csil_dec_workspace);
        csil_decode(csil_field)?
    };
    let next_cursor = match cbor_map_get(csil_root, "next_cursor") {
        Some(csil_field) => {
            let csil_decode = cbor_as_bytes;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    Ok(WorkspaceList {
        workspaces,
        next_cursor,
    })
}

/// Encode a WorkspaceList to canonical CSIL CBOR bytes.
pub fn encode_workspace_list(csil_v: &WorkspaceList) -> Vec<u8> {
    cbor_encode(&csil_enc_workspace_list(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a WorkspaceList.
pub fn decode_workspace_list(csil_data: &[u8]) -> Result<WorkspaceList, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_workspace_list(&csil_root)
}

/// Build the canonical CBOR value tree for a ProjectList.
fn csil_enc_project_list(csil_v: &ProjectList) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(2);
    csil_entries.push((
        cbor_text("projects"),
        cbor_enc_array(&csil_v.projects, csil_enc_project),
    ));
    if let Some(csil_inner) = &csil_v.next_cursor {
        csil_entries.push((cbor_text("next_cursor"), cbor_bytes(csil_inner)));
    }
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a ProjectList from a decoded CBOR value tree.
fn csil_dec_project_list(csil_root: &CsilCborValue) -> Result<ProjectList, CsilCborError> {
    let projects = {
        let csil_field = cbor_require(csil_root, "projects")?;
        let csil_decode = |csil_v| cbor_dec_array(csil_v, csil_dec_project);
        csil_decode(csil_field)?
    };
    let next_cursor = match cbor_map_get(csil_root, "next_cursor") {
        Some(csil_field) => {
            let csil_decode = cbor_as_bytes;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    Ok(ProjectList {
        projects,
        next_cursor,
    })
}

/// Encode a ProjectList to canonical CSIL CBOR bytes.
pub fn encode_project_list(csil_v: &ProjectList) -> Vec<u8> {
    cbor_encode(&csil_enc_project_list(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a ProjectList.
pub fn decode_project_list(csil_data: &[u8]) -> Result<ProjectList, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_project_list(&csil_root)
}

/// Build the canonical CBOR value tree for a ApiKeyList.
fn csil_enc_api_key_list(csil_v: &ApiKeyList) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(2);
    csil_entries.push((
        cbor_text("keys"),
        cbor_enc_array(&csil_v.keys, csil_enc_api_key_summary),
    ));
    if let Some(csil_inner) = &csil_v.next_cursor {
        csil_entries.push((cbor_text("next_cursor"), cbor_bytes(csil_inner)));
    }
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a ApiKeyList from a decoded CBOR value tree.
fn csil_dec_api_key_list(csil_root: &CsilCborValue) -> Result<ApiKeyList, CsilCborError> {
    let keys = {
        let csil_field = cbor_require(csil_root, "keys")?;
        let csil_decode = |csil_v| cbor_dec_array(csil_v, csil_dec_api_key_summary);
        csil_decode(csil_field)?
    };
    let next_cursor = match cbor_map_get(csil_root, "next_cursor") {
        Some(csil_field) => {
            let csil_decode = cbor_as_bytes;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    Ok(ApiKeyList { keys, next_cursor })
}

/// Encode a ApiKeyList to canonical CSIL CBOR bytes.
pub fn encode_api_key_list(csil_v: &ApiKeyList) -> Vec<u8> {
    cbor_encode(&csil_enc_api_key_list(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a ApiKeyList.
pub fn decode_api_key_list(csil_data: &[u8]) -> Result<ApiKeyList, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_api_key_list(&csil_root)
}

/// Build the canonical CBOR value tree for a AlertListRequest.
fn csil_enc_alert_list_request(csil_v: &AlertListRequest) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(3);
    if let Some(csil_inner) = &csil_v.limit {
        csil_entries.push((cbor_text("limit"), cbor_uint(*csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.cursor {
        csil_entries.push((cbor_text("cursor"), cbor_bytes(csil_inner)));
    }
    csil_entries.push((cbor_text("project_id"), cbor_bytes(&csil_v.project_id)));
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a AlertListRequest from a decoded CBOR value tree.
fn csil_dec_alert_list_request(
    csil_root: &CsilCborValue,
) -> Result<AlertListRequest, CsilCborError> {
    let project_id = {
        let csil_field = cbor_require(csil_root, "project_id")?;
        let csil_decode = cbor_as_bytes;
        csil_decode(csil_field)?
    };
    let cursor = match cbor_map_get(csil_root, "cursor") {
        Some(csil_field) => {
            let csil_decode = cbor_as_bytes;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let limit = match cbor_map_get(csil_root, "limit") {
        Some(csil_field) => {
            let csil_decode = cbor_as_u64;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    Ok(AlertListRequest {
        project_id,
        cursor,
        limit,
    })
}

/// Encode a AlertListRequest to canonical CSIL CBOR bytes.
pub fn encode_alert_list_request(csil_v: &AlertListRequest) -> Vec<u8> {
    cbor_encode(&csil_enc_alert_list_request(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a AlertListRequest.
pub fn decode_alert_list_request(csil_data: &[u8]) -> Result<AlertListRequest, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_alert_list_request(&csil_root)
}

/// Build the canonical CBOR value tree for a AlertRuleList.
fn csil_enc_alert_rule_list(csil_v: &AlertRuleList) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(2);
    csil_entries.push((
        cbor_text("rules"),
        cbor_enc_array(&csil_v.rules, csil_enc_alert_rule),
    ));
    if let Some(csil_inner) = &csil_v.next_cursor {
        csil_entries.push((cbor_text("next_cursor"), cbor_bytes(csil_inner)));
    }
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a AlertRuleList from a decoded CBOR value tree.
fn csil_dec_alert_rule_list(csil_root: &CsilCborValue) -> Result<AlertRuleList, CsilCborError> {
    let rules = {
        let csil_field = cbor_require(csil_root, "rules")?;
        let csil_decode = |csil_v| cbor_dec_array(csil_v, csil_dec_alert_rule);
        csil_decode(csil_field)?
    };
    let next_cursor = match cbor_map_get(csil_root, "next_cursor") {
        Some(csil_field) => {
            let csil_decode = cbor_as_bytes;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    Ok(AlertRuleList { rules, next_cursor })
}

/// Encode a AlertRuleList to canonical CSIL CBOR bytes.
pub fn encode_alert_rule_list(csil_v: &AlertRuleList) -> Vec<u8> {
    cbor_encode(&csil_enc_alert_rule_list(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a AlertRuleList.
pub fn decode_alert_rule_list(csil_data: &[u8]) -> Result<AlertRuleList, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_alert_rule_list(&csil_root)
}

/// Build the canonical CBOR value tree for a AlertInstanceList.
fn csil_enc_alert_instance_list(csil_v: &AlertInstanceList) -> CsilCborValue {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(2);
    csil_entries.push((
        cbor_text("instances"),
        cbor_enc_array(&csil_v.instances, csil_enc_alert_instance),
    ));
    if let Some(csil_inner) = &csil_v.next_cursor {
        csil_entries.push((cbor_text("next_cursor"), cbor_bytes(csil_inner)));
    }
    CsilCborValue::Map(csil_entries)
}

/// Reconstruct a AlertInstanceList from a decoded CBOR value tree.
fn csil_dec_alert_instance_list(
    csil_root: &CsilCborValue,
) -> Result<AlertInstanceList, CsilCborError> {
    let instances = {
        let csil_field = cbor_require(csil_root, "instances")?;
        let csil_decode = |csil_v| cbor_dec_array(csil_v, csil_dec_alert_instance);
        csil_decode(csil_field)?
    };
    let next_cursor = match cbor_map_get(csil_root, "next_cursor") {
        Some(csil_field) => {
            let csil_decode = cbor_as_bytes;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    Ok(AlertInstanceList {
        instances,
        next_cursor,
    })
}

/// Encode a AlertInstanceList to canonical CSIL CBOR bytes.
pub fn encode_alert_instance_list(csil_v: &AlertInstanceList) -> Vec<u8> {
    cbor_encode(&csil_enc_alert_instance_list(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a AlertInstanceList.
pub fn decode_alert_instance_list(csil_data: &[u8]) -> Result<AlertInstanceList, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_alert_instance_list(&csil_root)
}

/// Build the canonical CBOR value tree for a Empty.
fn csil_enc_empty(_csil_v: &Empty) -> CsilCborValue {
    CsilCborValue::Map(Vec::new())
}

/// Reconstruct a Empty from a decoded CBOR value tree.
fn csil_dec_empty(_csil_root: &CsilCborValue) -> Result<Empty, CsilCborError> {
    Ok(Empty {})
}

/// Encode a Empty to canonical CSIL CBOR bytes.
pub fn encode_empty(csil_v: &Empty) -> Vec<u8> {
    cbor_encode(&csil_enc_empty(csil_v))
}

/// Decode canonical CSIL CBOR bytes into a Empty.
pub fn decode_empty(csil_data: &[u8]) -> Result<Empty, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_empty(&csil_root)
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

/// Encode a ExpressionKind enum as its bare literal value.
fn csil_enc_expression_kind(csil_v: &ExpressionKind) -> CsilCborValue {
    match csil_v {
        ExpressionKind::Literal => cbor_text("literal"),
        ExpressionKind::Field => cbor_text("field"),
        ExpressionKind::Compare => cbor_text("compare"),
        ExpressionKind::Set => cbor_text("set"),
        ExpressionKind::Text => cbor_text("text"),
        ExpressionKind::Null => cbor_text("null"),
        ExpressionKind::Logical => cbor_text("logical"),
        ExpressionKind::Arith => cbor_text("arith"),
        ExpressionKind::Time => cbor_text("time"),
        ExpressionKind::Convert => cbor_text("convert"),
    }
}

/// Decode a bare literal value into a ExpressionKind enum.
fn csil_dec_expression_kind(csil_v: &CsilCborValue) -> Result<ExpressionKind, CsilCborError> {
    let csil_val = cbor_as_text(csil_v)?;
    match csil_val.as_str() {
        "literal" => Ok(ExpressionKind::Literal),
        "field" => Ok(ExpressionKind::Field),
        "compare" => Ok(ExpressionKind::Compare),
        "set" => Ok(ExpressionKind::Set),
        "text" => Ok(ExpressionKind::Text),
        "null" => Ok(ExpressionKind::Null),
        "logical" => Ok(ExpressionKind::Logical),
        "arith" => Ok(ExpressionKind::Arith),
        "time" => Ok(ExpressionKind::Time),
        "convert" => Ok(ExpressionKind::Convert),
        csil_other => Err(CsilCborError(format!(
            "csil cbor: unknown ExpressionKind value {csil_other:?}"
        ))),
    }
}

/// Encode a QueryNodeKind enum as its bare literal value.
fn csil_enc_query_node_kind(csil_v: &QueryNodeKind) -> CsilCborValue {
    match csil_v {
        QueryNodeKind::Scan => cbor_text("scan"),
        QueryNodeKind::Filter => cbor_text("filter"),
        QueryNodeKind::Project => cbor_text("project"),
        QueryNodeKind::Aggregate => cbor_text("aggregate"),
        QueryNodeKind::Sort => cbor_text("sort"),
        QueryNodeKind::Limit => cbor_text("limit"),
        QueryNodeKind::Join => cbor_text("join"),
        QueryNodeKind::Union => cbor_text("union"),
    }
}

/// Decode a bare literal value into a QueryNodeKind enum.
fn csil_dec_query_node_kind(csil_v: &CsilCborValue) -> Result<QueryNodeKind, CsilCborError> {
    let csil_val = cbor_as_text(csil_v)?;
    match csil_val.as_str() {
        "scan" => Ok(QueryNodeKind::Scan),
        "filter" => Ok(QueryNodeKind::Filter),
        "project" => Ok(QueryNodeKind::Project),
        "aggregate" => Ok(QueryNodeKind::Aggregate),
        "sort" => Ok(QueryNodeKind::Sort),
        "limit" => Ok(QueryNodeKind::Limit),
        "join" => Ok(QueryNodeKind::Join),
        "union" => Ok(QueryNodeKind::Union),
        csil_other => Err(CsilCborError(format!(
            "csil cbor: unknown QueryNodeKind value {csil_other:?}"
        ))),
    }
}

/// Encode a CompareOp enum as its bare literal value.
fn csil_enc_compare_op(csil_v: &CompareOp) -> CsilCborValue {
    match csil_v {
        CompareOp::Eq => cbor_text("eq"),
        CompareOp::Ne => cbor_text("ne"),
        CompareOp::Lt => cbor_text("lt"),
        CompareOp::Le => cbor_text("le"),
        CompareOp::Gt => cbor_text("gt"),
        CompareOp::Ge => cbor_text("ge"),
    }
}

/// Decode a bare literal value into a CompareOp enum.
fn csil_dec_compare_op(csil_v: &CsilCborValue) -> Result<CompareOp, CsilCborError> {
    let csil_val = cbor_as_text(csil_v)?;
    match csil_val.as_str() {
        "eq" => Ok(CompareOp::Eq),
        "ne" => Ok(CompareOp::Ne),
        "lt" => Ok(CompareOp::Lt),
        "le" => Ok(CompareOp::Le),
        "gt" => Ok(CompareOp::Gt),
        "ge" => Ok(CompareOp::Ge),
        csil_other => Err(CsilCborError(format!(
            "csil cbor: unknown CompareOp value {csil_other:?}"
        ))),
    }
}

/// Encode a LogicalOp enum as its bare literal value.
fn csil_enc_logical_op(csil_v: &LogicalOp) -> CsilCborValue {
    match csil_v {
        LogicalOp::And => cbor_text("and"),
        LogicalOp::Or => cbor_text("or"),
        LogicalOp::Not => cbor_text("not"),
    }
}

/// Decode a bare literal value into a LogicalOp enum.
fn csil_dec_logical_op(csil_v: &CsilCborValue) -> Result<LogicalOp, CsilCborError> {
    let csil_val = cbor_as_text(csil_v)?;
    match csil_val.as_str() {
        "and" => Ok(LogicalOp::And),
        "or" => Ok(LogicalOp::Or),
        "not" => Ok(LogicalOp::Not),
        csil_other => Err(CsilCborError(format!(
            "csil cbor: unknown LogicalOp value {csil_other:?}"
        ))),
    }
}

/// Encode a ArithOp enum as its bare literal value.
fn csil_enc_arith_op(csil_v: &ArithOp) -> CsilCborValue {
    match csil_v {
        ArithOp::Add => cbor_text("add"),
        ArithOp::Sub => cbor_text("sub"),
        ArithOp::Mul => cbor_text("mul"),
        ArithOp::Div => cbor_text("div"),
    }
}

/// Decode a bare literal value into a ArithOp enum.
fn csil_dec_arith_op(csil_v: &CsilCborValue) -> Result<ArithOp, CsilCborError> {
    let csil_val = cbor_as_text(csil_v)?;
    match csil_val.as_str() {
        "add" => Ok(ArithOp::Add),
        "sub" => Ok(ArithOp::Sub),
        "mul" => Ok(ArithOp::Mul),
        "div" => Ok(ArithOp::Div),
        csil_other => Err(CsilCborError(format!(
            "csil cbor: unknown ArithOp value {csil_other:?}"
        ))),
    }
}

/// Encode a TimeBasis enum as its bare literal value.
fn csil_enc_time_basis(csil_v: &TimeBasis) -> CsilCborValue {
    match csil_v {
        TimeBasis::OccurredAt => cbor_text("occurred_at"),
        TimeBasis::ReceivedAt => cbor_text("received_at"),
        TimeBasis::CommittedAt => cbor_text("committed_at"),
    }
}

/// Decode a bare literal value into a TimeBasis enum.
fn csil_dec_time_basis(csil_v: &CsilCborValue) -> Result<TimeBasis, CsilCborError> {
    let csil_val = cbor_as_text(csil_v)?;
    match csil_val.as_str() {
        "occurred_at" => Ok(TimeBasis::OccurredAt),
        "received_at" => Ok(TimeBasis::ReceivedAt),
        "committed_at" => Ok(TimeBasis::CommittedAt),
        csil_other => Err(CsilCborError(format!(
            "csil cbor: unknown TimeBasis value {csil_other:?}"
        ))),
    }
}

/// Encode a Dataset enum as its bare literal value.
fn csil_enc_dataset(csil_v: &Dataset) -> CsilCborValue {
    match csil_v {
        Dataset::Events => cbor_text("events"),
        Dataset::ErrorOccurrences => cbor_text("error_occurrences"),
        Dataset::ErrorGroups => cbor_text("error_groups"),
        Dataset::Spans => cbor_text("spans"),
        Dataset::MetricPoints => cbor_text("metric_points"),
        Dataset::IdentityEdges => cbor_text("identity_edges"),
        Dataset::CampaignTouches => cbor_text("campaign_touches"),
        Dataset::Conversions => cbor_text("conversions"),
        Dataset::CampaignCosts => cbor_text("campaign_costs"),
    }
}

/// Decode a bare literal value into a Dataset enum.
fn csil_dec_dataset(csil_v: &CsilCborValue) -> Result<Dataset, CsilCborError> {
    let csil_val = cbor_as_text(csil_v)?;
    match csil_val.as_str() {
        "events" => Ok(Dataset::Events),
        "error_occurrences" => Ok(Dataset::ErrorOccurrences),
        "error_groups" => Ok(Dataset::ErrorGroups),
        "spans" => Ok(Dataset::Spans),
        "metric_points" => Ok(Dataset::MetricPoints),
        "identity_edges" => Ok(Dataset::IdentityEdges),
        "campaign_touches" => Ok(Dataset::CampaignTouches),
        "conversions" => Ok(Dataset::Conversions),
        "campaign_costs" => Ok(Dataset::CampaignCosts),
        csil_other => Err(CsilCborError(format!(
            "csil cbor: unknown Dataset value {csil_other:?}"
        ))),
    }
}

/// Encode a MeasureKind enum as its bare literal value.
fn csil_enc_measure_kind(csil_v: &MeasureKind) -> CsilCborValue {
    match csil_v {
        MeasureKind::Count => cbor_text("count"),
        MeasureKind::Sum => cbor_text("sum"),
        MeasureKind::Min => cbor_text("min"),
        MeasureKind::Max => cbor_text("max"),
        MeasureKind::Avg => cbor_text("avg"),
        MeasureKind::CountDistinct => cbor_text("count_distinct"),
        MeasureKind::CountDistinctApprox => cbor_text("count_distinct_approx"),
        MeasureKind::Quantile => cbor_text("quantile"),
        MeasureKind::QuantileApprox => cbor_text("quantile_approx"),
        MeasureKind::HistogramMerge => cbor_text("histogram_merge"),
        MeasureKind::TopK => cbor_text("top_k"),
        MeasureKind::TopKApprox => cbor_text("top_k_approx"),
        MeasureKind::Rate => cbor_text("rate"),
        MeasureKind::Increase => cbor_text("increase"),
    }
}

/// Decode a bare literal value into a MeasureKind enum.
fn csil_dec_measure_kind(csil_v: &CsilCborValue) -> Result<MeasureKind, CsilCborError> {
    let csil_val = cbor_as_text(csil_v)?;
    match csil_val.as_str() {
        "count" => Ok(MeasureKind::Count),
        "sum" => Ok(MeasureKind::Sum),
        "min" => Ok(MeasureKind::Min),
        "max" => Ok(MeasureKind::Max),
        "avg" => Ok(MeasureKind::Avg),
        "count_distinct" => Ok(MeasureKind::CountDistinct),
        "count_distinct_approx" => Ok(MeasureKind::CountDistinctApprox),
        "quantile" => Ok(MeasureKind::Quantile),
        "quantile_approx" => Ok(MeasureKind::QuantileApprox),
        "histogram_merge" => Ok(MeasureKind::HistogramMerge),
        "top_k" => Ok(MeasureKind::TopK),
        "top_k_approx" => Ok(MeasureKind::TopKApprox),
        "rate" => Ok(MeasureKind::Rate),
        "increase" => Ok(MeasureKind::Increase),
        csil_other => Err(CsilCborError(format!(
            "csil cbor: unknown MeasureKind value {csil_other:?}"
        ))),
    }
}

/// Encode a CorrelationBasis enum as its bare literal value.
fn csil_enc_correlation_basis(csil_v: &CorrelationBasis) -> CsilCborValue {
    match csil_v {
        CorrelationBasis::EndUser => cbor_text("end-user"),
        CorrelationBasis::Session => cbor_text("session"),
        CorrelationBasis::Group => cbor_text("group"),
    }
}

/// Decode a bare literal value into a CorrelationBasis enum.
fn csil_dec_correlation_basis(csil_v: &CsilCborValue) -> Result<CorrelationBasis, CsilCborError> {
    let csil_val = cbor_as_text(csil_v)?;
    match csil_val.as_str() {
        "end-user" => Ok(CorrelationBasis::EndUser),
        "session" => Ok(CorrelationBasis::Session),
        "group" => Ok(CorrelationBasis::Group),
        csil_other => Err(CsilCborError(format!(
            "csil cbor: unknown CorrelationBasis value {csil_other:?}"
        ))),
    }
}

/// Encode a IdentityResolution enum as its bare literal value.
fn csil_enc_identity_resolution(csil_v: &IdentityResolution) -> CsilCborValue {
    match csil_v {
        IdentityResolution::EventTime => cbor_text("event-time"),
        IdentityResolution::LatestKnown => cbor_text("latest-known"),
    }
}

/// Decode a bare literal value into a IdentityResolution enum.
fn csil_dec_identity_resolution(
    csil_v: &CsilCborValue,
) -> Result<IdentityResolution, CsilCborError> {
    let csil_val = cbor_as_text(csil_v)?;
    match csil_val.as_str() {
        "event-time" => Ok(IdentityResolution::EventTime),
        "latest-known" => Ok(IdentityResolution::LatestKnown),
        csil_other => Err(CsilCborError(format!(
            "csil cbor: unknown IdentityResolution value {csil_other:?}"
        ))),
    }
}

/// Encode a AttributionModel enum as its bare literal value.
fn csil_enc_attribution_model(csil_v: &AttributionModel) -> CsilCborValue {
    match csil_v {
        AttributionModel::FirstTouch => cbor_text("first-touch"),
        AttributionModel::LastTouch => cbor_text("last-touch"),
        AttributionModel::LastNonDirect => cbor_text("last-non-direct"),
        AttributionModel::Linear => cbor_text("linear"),
        AttributionModel::Position => cbor_text("position"),
        AttributionModel::Decay => cbor_text("decay"),
    }
}

/// Decode a bare literal value into a AttributionModel enum.
fn csil_dec_attribution_model(csil_v: &CsilCborValue) -> Result<AttributionModel, CsilCborError> {
    let csil_val = cbor_as_text(csil_v)?;
    match csil_val.as_str() {
        "first-touch" => Ok(AttributionModel::FirstTouch),
        "last-touch" => Ok(AttributionModel::LastTouch),
        "last-non-direct" => Ok(AttributionModel::LastNonDirect),
        "linear" => Ok(AttributionModel::Linear),
        "position" => Ok(AttributionModel::Position),
        "decay" => Ok(AttributionModel::Decay),
        csil_other => Err(CsilCborError(format!(
            "csil cbor: unknown AttributionModel value {csil_other:?}"
        ))),
    }
}

/// Encode a Consistency enum as its bare literal value.
fn csil_enc_consistency(csil_v: &Consistency) -> CsilCborValue {
    match csil_v {
        Consistency::Committed => cbor_text("committed"),
        Consistency::BoundedStale => cbor_text("bounded-stale"),
    }
}

/// Decode a bare literal value into a Consistency enum.
fn csil_dec_consistency(csil_v: &CsilCborValue) -> Result<Consistency, CsilCborError> {
    let csil_val = cbor_as_text(csil_v)?;
    match csil_val.as_str() {
        "committed" => Ok(Consistency::Committed),
        "bounded-stale" => Ok(Consistency::BoundedStale),
        csil_other => Err(CsilCborError(format!(
            "csil cbor: unknown Consistency value {csil_other:?}"
        ))),
    }
}

/// Encode a QueryForm enum as its bare literal value.
fn csil_enc_query_form(csil_v: &QueryForm) -> CsilCborValue {
    match csil_v {
        QueryForm::Node => cbor_text("node"),
        QueryForm::Funnel => cbor_text("funnel"),
        QueryForm::Retention => cbor_text("retention"),
        QueryForm::Path => cbor_text("path"),
        QueryForm::Trace => cbor_text("trace"),
        QueryForm::Timeline => cbor_text("timeline"),
        QueryForm::Attribution => cbor_text("attribution"),
        QueryForm::CampaignSummary => cbor_text("campaign-summary"),
    }
}

/// Decode a bare literal value into a QueryForm enum.
fn csil_dec_query_form(csil_v: &CsilCborValue) -> Result<QueryForm, CsilCborError> {
    let csil_val = cbor_as_text(csil_v)?;
    match csil_val.as_str() {
        "node" => Ok(QueryForm::Node),
        "funnel" => Ok(QueryForm::Funnel),
        "retention" => Ok(QueryForm::Retention),
        "path" => Ok(QueryForm::Path),
        "trace" => Ok(QueryForm::Trace),
        "timeline" => Ok(QueryForm::Timeline),
        "attribution" => Ok(QueryForm::Attribution),
        "campaign-summary" => Ok(QueryForm::CampaignSummary),
        csil_other => Err(CsilCborError(format!(
            "csil cbor: unknown QueryForm value {csil_other:?}"
        ))),
    }
}

/// Encode a RetentionClass enum as its bare literal value.
fn csil_enc_retention_class(csil_v: &RetentionClass) -> CsilCborValue {
    match csil_v {
        RetentionClass::Provisional => cbor_text("provisional"),
        RetentionClass::Raw => cbor_text("raw"),
        RetentionClass::Detailed => cbor_text("detailed"),
        RetentionClass::Rollup => cbor_text("rollup"),
        RetentionClass::Audit => cbor_text("audit"),
    }
}

/// Decode a bare literal value into a RetentionClass enum.
fn csil_dec_retention_class(csil_v: &CsilCborValue) -> Result<RetentionClass, CsilCborError> {
    let csil_val = cbor_as_text(csil_v)?;
    match csil_val.as_str() {
        "provisional" => Ok(RetentionClass::Provisional),
        "raw" => Ok(RetentionClass::Raw),
        "detailed" => Ok(RetentionClass::Detailed),
        "rollup" => Ok(RetentionClass::Rollup),
        "audit" => Ok(RetentionClass::Audit),
        csil_other => Err(CsilCborError(format!(
            "csil cbor: unknown RetentionClass value {csil_other:?}"
        ))),
    }
}

/// Encode a AlertState enum as its bare literal value.
fn csil_enc_alert_state(csil_v: &AlertState) -> CsilCborValue {
    match csil_v {
        AlertState::Ok => cbor_text("ok"),
        AlertState::Firing => cbor_text("firing"),
        AlertState::NoData => cbor_text("no-data"),
        AlertState::Unknown => cbor_text("unknown"),
        AlertState::Silenced => cbor_text("silenced"),
    }
}

/// Decode a bare literal value into a AlertState enum.
fn csil_dec_alert_state(csil_v: &CsilCborValue) -> Result<AlertState, CsilCborError> {
    let csil_val = cbor_as_text(csil_v)?;
    match csil_val.as_str() {
        "ok" => Ok(AlertState::Ok),
        "firing" => Ok(AlertState::Firing),
        "no-data" => Ok(AlertState::NoData),
        "unknown" => Ok(AlertState::Unknown),
        "silenced" => Ok(AlertState::Silenced),
        csil_other => Err(CsilCborError(format!(
            "csil cbor: unknown AlertState value {csil_other:?}"
        ))),
    }
}

/// Encode a AlertOutcome enum as its bare literal value.
fn csil_enc_alert_outcome(csil_v: &AlertOutcome) -> CsilCborValue {
    match csil_v {
        AlertOutcome::Value => cbor_text("value"),
        AlertOutcome::NoData => cbor_text("no-data"),
        AlertOutcome::Error => cbor_text("error"),
    }
}

/// Decode a bare literal value into a AlertOutcome enum.
fn csil_dec_alert_outcome(csil_v: &CsilCborValue) -> Result<AlertOutcome, CsilCborError> {
    let csil_val = cbor_as_text(csil_v)?;
    match csil_val.as_str() {
        "value" => Ok(AlertOutcome::Value),
        "no-data" => Ok(AlertOutcome::NoData),
        "error" => Ok(AlertOutcome::Error),
        csil_other => Err(CsilCborError(format!(
            "csil cbor: unknown AlertOutcome value {csil_other:?}"
        ))),
    }
}

/// Encode a WorkflowKind enum as its bare literal value.
fn csil_enc_workflow_kind(csil_v: &WorkflowKind) -> CsilCborValue {
    match csil_v {
        WorkflowKind::AlertEvaluation => cbor_text("alert-evaluation"),
        WorkflowKind::Notification => cbor_text("notification"),
        WorkflowKind::ProjectorRebuild => cbor_text("projector-rebuild"),
        WorkflowKind::Retention => cbor_text("retention"),
        WorkflowKind::Deletion => cbor_text("deletion"),
        WorkflowKind::Export => cbor_text("export"),
    }
}

/// Decode a bare literal value into a WorkflowKind enum.
fn csil_dec_workflow_kind(csil_v: &CsilCborValue) -> Result<WorkflowKind, CsilCborError> {
    let csil_val = cbor_as_text(csil_v)?;
    match csil_val.as_str() {
        "alert-evaluation" => Ok(WorkflowKind::AlertEvaluation),
        "notification" => Ok(WorkflowKind::Notification),
        "projector-rebuild" => Ok(WorkflowKind::ProjectorRebuild),
        "retention" => Ok(WorkflowKind::Retention),
        "deletion" => Ok(WorkflowKind::Deletion),
        "export" => Ok(WorkflowKind::Export),
        csil_other => Err(CsilCborError(format!(
            "csil cbor: unknown WorkflowKind value {csil_other:?}"
        ))),
    }
}

/// Encode a PolicyScope enum as its bare literal value.
fn csil_enc_policy_scope(csil_v: &PolicyScope) -> CsilCborValue {
    match csil_v {
        PolicyScope::Installation => cbor_text("installation"),
        PolicyScope::Workspace => cbor_text("workspace"),
        PolicyScope::Project => cbor_text("project"),
        PolicyScope::Environment => cbor_text("environment"),
        PolicyScope::Source => cbor_text("source"),
    }
}

/// Decode a bare literal value into a PolicyScope enum.
fn csil_dec_policy_scope(csil_v: &CsilCborValue) -> Result<PolicyScope, CsilCborError> {
    let csil_val = cbor_as_text(csil_v)?;
    match csil_val.as_str() {
        "installation" => Ok(PolicyScope::Installation),
        "workspace" => Ok(PolicyScope::Workspace),
        "project" => Ok(PolicyScope::Project),
        "environment" => Ok(PolicyScope::Environment),
        "source" => Ok(PolicyScope::Source),
        csil_other => Err(CsilCborError(format!(
            "csil cbor: unknown PolicyScope value {csil_other:?}"
        ))),
    }
}

/// Encode a CampaignLinking enum as its bare literal value.
fn csil_enc_campaign_linking(csil_v: &CampaignLinking) -> CsilCborValue {
    match csil_v {
        CampaignLinking::Linked => cbor_text("linked"),
        CampaignLinking::Unlinked => cbor_text("unlinked"),
        CampaignLinking::None => cbor_text("none"),
    }
}

/// Decode a bare literal value into a CampaignLinking enum.
fn csil_dec_campaign_linking(csil_v: &CsilCborValue) -> Result<CampaignLinking, CsilCborError> {
    let csil_val = cbor_as_text(csil_v)?;
    match csil_val.as_str() {
        "linked" => Ok(CampaignLinking::Linked),
        "unlinked" => Ok(CampaignLinking::Unlinked),
        "none" => Ok(CampaignLinking::None),
        csil_other => Err(CsilCborError(format!(
            "csil cbor: unknown CampaignLinking value {csil_other:?}"
        ))),
    }
}

/// Encode a NodeRole enum as its bare literal value.
fn csil_enc_node_role(csil_v: &NodeRole) -> CsilCborValue {
    match csil_v {
        NodeRole::CollectorIntake => cbor_text("collector-intake"),
        NodeRole::CollectorForwarder => cbor_text("collector-forwarder"),
        NodeRole::CompatibilityReceiver => cbor_text("compatibility-receiver"),
        NodeRole::IngestGateway => cbor_text("ingest-gateway"),
        NodeRole::QueryCoordinator => cbor_text("query-coordinator"),
        NodeRole::Projector => cbor_text("projector"),
        NodeRole::WorkflowWorker => cbor_text("workflow-worker"),
        NodeRole::ReadReplica => cbor_text("read-replica"),
        NodeRole::ExportReplica => cbor_text("export-replica"),
        NodeRole::StorageProcess => cbor_text("storage-process"),
    }
}

/// Decode a bare literal value into a NodeRole enum.
fn csil_dec_node_role(csil_v: &CsilCborValue) -> Result<NodeRole, CsilCborError> {
    let csil_val = cbor_as_text(csil_v)?;
    match csil_val.as_str() {
        "collector-intake" => Ok(NodeRole::CollectorIntake),
        "collector-forwarder" => Ok(NodeRole::CollectorForwarder),
        "compatibility-receiver" => Ok(NodeRole::CompatibilityReceiver),
        "ingest-gateway" => Ok(NodeRole::IngestGateway),
        "query-coordinator" => Ok(NodeRole::QueryCoordinator),
        "projector" => Ok(NodeRole::Projector),
        "workflow-worker" => Ok(NodeRole::WorkflowWorker),
        "read-replica" => Ok(NodeRole::ReadReplica),
        "export-replica" => Ok(NodeRole::ExportReplica),
        "storage-process" => Ok(NodeRole::StorageProcess),
        csil_other => Err(CsilCborError(format!(
            "csil cbor: unknown NodeRole value {csil_other:?}"
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

/// Encode a SetExpr_set_test enum as its bare literal value.
fn csil_enc_set_expr_set_test(csil_v: &SetExpr_set_test) -> CsilCborValue {
    match csil_v {
        SetExpr_set_test::In => cbor_text("in"),
        SetExpr_set_test::NotIn => cbor_text("not-in"),
    }
}

/// Decode a bare literal value into a SetExpr_set_test enum.
fn csil_dec_set_expr_set_test(csil_v: &CsilCborValue) -> Result<SetExpr_set_test, CsilCborError> {
    let csil_val = cbor_as_text(csil_v)?;
    match csil_val.as_str() {
        "in" => Ok(SetExpr_set_test::In),
        "not-in" => Ok(SetExpr_set_test::NotIn),
        csil_other => Err(CsilCborError(format!(
            "csil cbor: unknown SetExpr_set_test value {csil_other:?}"
        ))),
    }
}

/// Encode a TextExpr_text_test enum as its bare literal value.
fn csil_enc_text_expr_text_test(csil_v: &TextExpr_text_test) -> CsilCborValue {
    match csil_v {
        TextExpr_text_test::StartsWith => cbor_text("starts-with"),
        TextExpr_text_test::EndsWith => cbor_text("ends-with"),
        TextExpr_text_test::Contains => cbor_text("contains"),
    }
}

/// Decode a bare literal value into a TextExpr_text_test enum.
fn csil_dec_text_expr_text_test(
    csil_v: &CsilCborValue,
) -> Result<TextExpr_text_test, CsilCborError> {
    let csil_val = cbor_as_text(csil_v)?;
    match csil_val.as_str() {
        "starts-with" => Ok(TextExpr_text_test::StartsWith),
        "ends-with" => Ok(TextExpr_text_test::EndsWith),
        "contains" => Ok(TextExpr_text_test::Contains),
        csil_other => Err(CsilCborError(format!(
            "csil cbor: unknown TextExpr_text_test value {csil_other:?}"
        ))),
    }
}

/// Encode a NullExpr_null_test enum as its bare literal value.
fn csil_enc_null_expr_null_test(csil_v: &NullExpr_null_test) -> CsilCborValue {
    match csil_v {
        NullExpr_null_test::IsNull => cbor_text("is-null"),
        NullExpr_null_test::IsNotNull => cbor_text("is-not-null"),
    }
}

/// Decode a bare literal value into a NullExpr_null_test enum.
fn csil_dec_null_expr_null_test(
    csil_v: &CsilCborValue,
) -> Result<NullExpr_null_test, CsilCborError> {
    let csil_val = cbor_as_text(csil_v)?;
    match csil_val.as_str() {
        "is-null" => Ok(NullExpr_null_test::IsNull),
        "is-not-null" => Ok(NullExpr_null_test::IsNotNull),
        csil_other => Err(CsilCborError(format!(
            "csil cbor: unknown NullExpr_null_test value {csil_other:?}"
        ))),
    }
}

/// Encode a TimeExpr_time_fn enum as its bare literal value.
fn csil_enc_time_expr_time_fn(csil_v: &TimeExpr_time_fn) -> CsilCborValue {
    match csil_v {
        TimeExpr_time_fn::Truncate => cbor_text("truncate"),
        TimeExpr_time_fn::Extract => cbor_text("extract"),
        TimeExpr_time_fn::Shift => cbor_text("shift"),
    }
}

/// Decode a bare literal value into a TimeExpr_time_fn enum.
fn csil_dec_time_expr_time_fn(csil_v: &CsilCborValue) -> Result<TimeExpr_time_fn, CsilCborError> {
    let csil_val = cbor_as_text(csil_v)?;
    match csil_val.as_str() {
        "truncate" => Ok(TimeExpr_time_fn::Truncate),
        "extract" => Ok(TimeExpr_time_fn::Extract),
        "shift" => Ok(TimeExpr_time_fn::Shift),
        csil_other => Err(CsilCborError(format!(
            "csil cbor: unknown TimeExpr_time_fn value {csil_other:?}"
        ))),
    }
}

/// Encode a Interval_calendar enum as its bare literal value.
fn csil_enc_interval_calendar(csil_v: &Interval_calendar) -> CsilCborValue {
    match csil_v {
        Interval_calendar::Hour => cbor_text("hour"),
        Interval_calendar::Day => cbor_text("day"),
        Interval_calendar::Week => cbor_text("week"),
        Interval_calendar::Month => cbor_text("month"),
    }
}

/// Decode a bare literal value into a Interval_calendar enum.
fn csil_dec_interval_calendar(csil_v: &CsilCborValue) -> Result<Interval_calendar, CsilCborError> {
    let csil_val = cbor_as_text(csil_v)?;
    match csil_val.as_str() {
        "hour" => Ok(Interval_calendar::Hour),
        "day" => Ok(Interval_calendar::Day),
        "week" => Ok(Interval_calendar::Week),
        "month" => Ok(Interval_calendar::Month),
        csil_other => Err(CsilCborError(format!(
            "csil cbor: unknown Interval_calendar value {csil_other:?}"
        ))),
    }
}

/// Encode a SortKey_direction enum as its bare literal value.
fn csil_enc_sort_key_direction(csil_v: &SortKey_direction) -> CsilCborValue {
    match csil_v {
        SortKey_direction::Asc => cbor_text("asc"),
        SortKey_direction::Desc => cbor_text("desc"),
    }
}

/// Decode a bare literal value into a SortKey_direction enum.
fn csil_dec_sort_key_direction(csil_v: &CsilCborValue) -> Result<SortKey_direction, CsilCborError> {
    let csil_val = cbor_as_text(csil_v)?;
    match csil_val.as_str() {
        "asc" => Ok(SortKey_direction::Asc),
        "desc" => Ok(SortKey_direction::Desc),
        csil_other => Err(CsilCborError(format!(
            "csil cbor: unknown SortKey_direction value {csil_other:?}"
        ))),
    }
}

/// Encode a RetentionQuery_period enum as its bare literal value.
fn csil_enc_retention_query_period(csil_v: &RetentionQuery_period) -> CsilCborValue {
    match csil_v {
        RetentionQuery_period::Day => cbor_text("day"),
        RetentionQuery_period::Week => cbor_text("week"),
        RetentionQuery_period::Month => cbor_text("month"),
    }
}

/// Decode a bare literal value into a RetentionQuery_period enum.
fn csil_dec_retention_query_period(
    csil_v: &CsilCborValue,
) -> Result<RetentionQuery_period, CsilCborError> {
    let csil_val = cbor_as_text(csil_v)?;
    match csil_val.as_str() {
        "day" => Ok(RetentionQuery_period::Day),
        "week" => Ok(RetentionQuery_period::Week),
        "month" => Ok(RetentionQuery_period::Month),
        csil_other => Err(CsilCborError(format!(
            "csil cbor: unknown RetentionQuery_period value {csil_other:?}"
        ))),
    }
}

/// Encode a PathQuery_direction enum as its bare literal value.
fn csil_enc_path_query_direction(csil_v: &PathQuery_direction) -> CsilCborValue {
    match csil_v {
        PathQuery_direction::Previous => cbor_text("previous"),
        PathQuery_direction::Next => cbor_text("next"),
    }
}

/// Decode a bare literal value into a PathQuery_direction enum.
fn csil_dec_path_query_direction(
    csil_v: &CsilCborValue,
) -> Result<PathQuery_direction, CsilCborError> {
    let csil_val = cbor_as_text(csil_v)?;
    match csil_val.as_str() {
        "previous" => Ok(PathQuery_direction::Previous),
        "next" => Ok(PathQuery_direction::Next),
        csil_other => Err(CsilCborError(format!(
            "csil cbor: unknown PathQuery_direction value {csil_other:?}"
        ))),
    }
}

/// Encode a CampaignSummaryQuery_dimension enum as its bare literal value.
fn csil_enc_campaign_summary_query_dimension(
    csil_v: &CampaignSummaryQuery_dimension,
) -> CsilCborValue {
    match csil_v {
        CampaignSummaryQuery_dimension::Campaign => cbor_text("campaign"),
        CampaignSummaryQuery_dimension::Channel => cbor_text("channel"),
        CampaignSummaryQuery_dimension::Source => cbor_text("source"),
        CampaignSummaryQuery_dimension::Medium => cbor_text("medium"),
        CampaignSummaryQuery_dimension::Content => cbor_text("content"),
    }
}

/// Decode a bare literal value into a CampaignSummaryQuery_dimension enum.
fn csil_dec_campaign_summary_query_dimension(
    csil_v: &CsilCborValue,
) -> Result<CampaignSummaryQuery_dimension, CsilCborError> {
    let csil_val = cbor_as_text(csil_v)?;
    match csil_val.as_str() {
        "campaign" => Ok(CampaignSummaryQuery_dimension::Campaign),
        "channel" => Ok(CampaignSummaryQuery_dimension::Channel),
        "source" => Ok(CampaignSummaryQuery_dimension::Source),
        "medium" => Ok(CampaignSummaryQuery_dimension::Medium),
        "content" => Ok(CampaignSummaryQuery_dimension::Content),
        csil_other => Err(CsilCborError(format!(
            "csil cbor: unknown CampaignSummaryQuery_dimension value {csil_other:?}"
        ))),
    }
}

/// Encode a NotificationTarget_kind enum as its bare literal value.
fn csil_enc_notification_target_kind(csil_v: &NotificationTarget_kind) -> CsilCborValue {
    match csil_v {
        NotificationTarget_kind::Webhook => cbor_text("webhook"),
        NotificationTarget_kind::CsilCallback => cbor_text("csil-callback"),
    }
}

/// Decode a bare literal value into a NotificationTarget_kind enum.
fn csil_dec_notification_target_kind(
    csil_v: &CsilCborValue,
) -> Result<NotificationTarget_kind, CsilCborError> {
    let csil_val = cbor_as_text(csil_v)?;
    match csil_val.as_str() {
        "webhook" => Ok(NotificationTarget_kind::Webhook),
        "csil-callback" => Ok(NotificationTarget_kind::CsilCallback),
        csil_other => Err(CsilCborError(format!(
            "csil cbor: unknown NotificationTarget_kind value {csil_other:?}"
        ))),
    }
}

/// Encode a Membership_role enum as its bare literal value.
fn csil_enc_membership_role(csil_v: &Membership_role) -> CsilCborValue {
    match csil_v {
        Membership_role::Viewer => cbor_text("viewer"),
        Membership_role::Admin => cbor_text("admin"),
        Membership_role::Owner => cbor_text("owner"),
    }
}

/// Decode a bare literal value into a Membership_role enum.
fn csil_dec_membership_role(csil_v: &CsilCborValue) -> Result<Membership_role, CsilCborError> {
    let csil_val = cbor_as_text(csil_v)?;
    match csil_val.as_str() {
        "viewer" => Ok(Membership_role::Viewer),
        "admin" => Ok(Membership_role::Admin),
        "owner" => Ok(Membership_role::Owner),
        csil_other => Err(CsilCborError(format!(
            "csil cbor: unknown Membership_role value {csil_other:?}"
        ))),
    }
}
