//! Generated self-contained canonical-CBOR codec from CSIL specification.
//!
//! CSIL is the CBOR Service Interface Language; this codec owns the payload
//! wire (a CBOR map keyed by the verbatim CSIL field name in canonical RFC
//! 8949 order) so the generated types need no serde derive. One
//! `encode_`/`decode_` pair is emitted per record type.
#![allow(dead_code, clippy::vec_init_then_push, clippy::let_unit_value)]

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
            )));
        }
    };
    let bytes = pos
        .checked_add(1)
        .and_then(|start| b.get(start..start.checked_add(width)?))
        .ok_or_else(|| CsilCborError("csil cbor: truncated argument".to_string()))?;
    let mut v = 0u64;
    for &byte in bytes {
        v = (v << 8) | byte as u64;
    }
    *pos += 1 + width;
    Ok(v)
}

/// Whether the item head at `at` uses the shortest argument for its value or
/// length (RFC 8949 section 4.2.1). A map key with a longer head has a second byte
/// form, and the canonical order check could then accept a map that no canonical
/// encoder writes. Floats and simple values (major type 7) are not checked here,
/// and a truncated head is left to the decoder to report.
fn cbor_head_is_shortest(b: &[u8], at: usize) -> bool {
    let Some(&ib) = b.get(at) else {
        return true;
    };
    let (major, low) = (ib >> 5, ib & 0x1f);
    let width = match low {
        24 => 1usize,
        25 => 2,
        26 => 4,
        27 => 8,
        _ => return true,
    };
    if major == 7 {
        return true;
    }
    let Some(bytes) = b.get(at + 1..at + 1 + width) else {
        return true;
    };
    let value = bytes.iter().fold(0u64, |v, &byte| (v << 8) | byte as u64);
    match width {
        1 => value >= 24,
        2 => value > 0xff,
        4 => value > 0xffff,
        _ => value > 0xffff_ffff,
    }
}

/// The most elements a decoded array or map reserves before it reads them. The
/// declared length is checked against the remaining input, but one input byte can
/// become a much larger value, so reserving the full declared length lets a small
/// frame reserve a large multiple of its size at every nesting level. Past this
/// bound, the vector grows only as elements are actually read.
const CSIL_CBOR_PREALLOC_LIMIT: usize = 1024;

/// The `len` bytes at `start`, or `None` when the input is shorter. `get` is
/// used instead of indexing so a bad length can never panic.
fn cbor_take(b: &[u8], start: usize, len: u64) -> Option<&[u8]> {
    let end = start.checked_add(usize::try_from(len).ok()?)?;
    b.get(start..end)
}

/// Widen an IEEE 754 half-precision value (RFC 8949 Appendix D) exactly to binary64.
fn cbor_half_to_f64(half: u16) -> f64 {
    let exp = (half >> 10) & 0x1f;
    let mant = f64::from(half & 0x3ff);
    let magnitude = match exp {
        0 => mant * 2f64.powi(-24),
        31 if mant == 0.0 => f64::INFINITY,
        31 => f64::NAN,
        _ => (mant + 1024.0) * 2f64.powi(i32::from(exp) - 25),
    };
    if half & 0x8000 != 0 {
        -magnitude
    } else {
        magnitude
    }
}

fn cbor_dec(b: &[u8], pos: &mut usize, depth: usize) -> Result<CsilCborValue, CsilCborError> {
    if depth > 64 {
        return Err(CsilCborError(
            "csil cbor: nesting limit exceeded".to_string(),
        ));
    }
    let Some(&ib) = b.get(*pos) else {
        return Err(CsilCborError(
            "csil cbor: unexpected end of input".to_string(),
        ));
    };
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
            25 => {
                let bits = cbor_read_arg(b, pos, low)?;
                Ok(CsilCborValue::Float(cbor_half_to_f64(bits as u16)))
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
            let slice = cbor_take(b, *pos, arg)
                .ok_or_else(|| CsilCborError("csil cbor: truncated byte string".to_string()))?;
            *pos += slice.len();
            Ok(CsilCborValue::Bytes(slice.to_vec()))
        }
        3 => {
            let slice = cbor_take(b, *pos, arg)
                .ok_or_else(|| CsilCborError("csil cbor: truncated text string".to_string()))?;
            let s = std::str::from_utf8(slice)
                .map_err(|e| CsilCborError(format!("csil cbor: invalid utf-8: {e}")))?
                .to_string();
            *pos += slice.len();
            Ok(CsilCborValue::Text(s))
        }
        4 => {
            if arg > b.len().saturating_sub(*pos) as u64 {
                return Err(CsilCborError(
                    "csil cbor: array length exceeds remaining input".to_string(),
                ));
            }
            let n = arg as usize;
            let mut items = Vec::with_capacity(n.min(CSIL_CBOR_PREALLOC_LIMIT));
            for _ in 0..n {
                items.push(cbor_dec(b, pos, depth + 1)?);
            }
            Ok(CsilCborValue::Array(items))
        }
        5 => {
            if arg > b.len().saturating_sub(*pos) as u64 {
                return Err(CsilCborError(
                    "csil cbor: map length exceeds remaining input".to_string(),
                ));
            }
            let n = arg as usize;
            let mut entries = Vec::with_capacity(n.min(CSIL_CBOR_PREALLOC_LIMIT));
            for _ in 0..n {
                if !cbor_head_is_shortest(b, *pos) {
                    return Err(CsilCborError(
                        "csil cbor: map key is not in shortest form".to_string(),
                    ));
                }
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
fn cbor_enc_array<E>(
    xs: &[E],
    f: impl Fn(&E) -> Result<CsilCborValue, CsilCborError>,
) -> Result<CsilCborValue, CsilCborError> {
    xs.iter()
        .map(f)
        .collect::<Result<Vec<_>, _>>()
        .map(CsilCborValue::Array)
}

/// Map a typed map to a CBOR map with its entries in canonical order: the bytewise
/// order of each encoded key (RFC 8949 section 4.2.1). `HashMap` iteration order is
/// arbitrary, and a peer rejects a map whose keys are not in that order.
fn cbor_enc_map<K, V>(
    m: &std::collections::HashMap<K, V>,
    kf: impl Fn(&K) -> Result<CsilCborValue, CsilCborError>,
    vf: impl Fn(&V) -> Result<CsilCborValue, CsilCborError>,
) -> Result<CsilCborValue, CsilCborError> {
    let mut entries = m
        .iter()
        .map(|(k, v)| Ok((kf(k)?, vf(v)?)))
        .collect::<Result<Vec<_>, CsilCborError>>()?;
    entries.sort_by_cached_key(|(k, _)| cbor_encode(k));
    Ok(CsilCborValue::Map(entries))
}

fn cbor_dec_array<E>(
    v: &CsilCborValue,
    f: impl Fn(&CsilCborValue) -> Result<E, CsilCborError>,
) -> Result<Vec<E>, CsilCborError> {
    cbor_as_array(v)?.iter().map(f).collect()
}

/// Decode a dynamic map. Its keys must be in strictly ascending canonical order, so
/// each key is compared with the one before it only: one pass, no sort, and a key
/// that comes twice is refused too.
fn cbor_dec_map<K: std::cmp::Eq + std::hash::Hash, V>(
    v: &CsilCborValue,
    kf: impl Fn(&CsilCborValue) -> Result<K, CsilCborError>,
    vf: impl Fn(&CsilCborValue) -> Result<V, CsilCborError>,
) -> Result<std::collections::HashMap<K, V>, CsilCborError> {
    let entries = cbor_as_map(v)?;
    let mut out = std::collections::HashMap::with_capacity(entries.len());
    let mut previous: Option<Vec<u8>> = None;
    for (k, val) in entries {
        let key = cbor_encode(k);
        match previous.as_deref().map(|p| p.cmp(key.as_slice())) {
            Some(std::cmp::Ordering::Equal) => return Err(cbor_duplicate_key(k)),
            Some(std::cmp::Ordering::Greater) => {
                return Err(CsilCborError(
                    "csil cbor: map keys are not in canonical order".to_string(),
                ));
            }
            _ => {}
        }
        out.insert(kf(k)?, vf(val)?);
        previous = Some(key);
    }
    Ok(out)
}

fn cbor_duplicate_key(k: &CsilCborValue) -> CsilCborError {
    match k {
        CsilCborValue::Text(name) => CsilCborError(format!("csil cbor: duplicate key {name:?}")),
        other => CsilCborError(format!("csil cbor: duplicate key {other:?}")),
    }
}

/// Refuse a record map that names one key twice. Record fields may come in any
/// order, so a set of the encoded keys finds a repeat in one pass.
fn cbor_check_record(v: &CsilCborValue) -> Result<(), CsilCborError> {
    let entries = cbor_as_map(v)?;
    let mut seen = std::collections::HashSet::with_capacity(entries.len());
    for (k, _) in entries {
        if !seen.insert(cbor_encode(k)) {
            return Err(cbor_duplicate_key(k));
        }
    }
    Ok(())
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
fn csil_rt_enc_bigint(m: i128) -> CsilCborValue {
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

fn csil_rt_dec_bigint(v: &CsilCborValue) -> Result<i128, CsilCborError> {
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
fn csil_rt_enc_decimal(d: &CsilDecimal) -> Result<CsilCborValue, CsilCborError> {
    Ok(CsilCborValue::Tag(
        4,
        Box::new(CsilCborValue::Array(vec![
            CsilCborValue::Int(d.exponent),
            csil_rt_enc_bigint(d.mantissa),
        ])),
    ))
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
    let [exponent, mantissa] = arr.as_slice() else {
        return Err(CsilCborError(
            "csil cbor: tag 4 content must be [exponent, mantissa]".to_string(),
        ));
    };
    let exponent = cbor_as_i64(exponent)?;
    let mantissa = csil_rt_dec_bigint(mantissa)?;
    Ok(CsilDecimal { exponent, mantissa })
}

/// Build the canonical CBOR value tree for a Batch.
fn csil_enc_batch(csil_v: &Batch) -> Result<CsilCborValue, CsilCborError> {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(5);
    csil_entries.push((
        cbor_text("items"),
        cbor_enc_array(&csil_v.items, csil_enc_telemetry_item)?,
    ));
    csil_entries.push((cbor_text("batch_id"), cbor_bytes(&csil_v.batch_id)));
    csil_entries.push((cbor_text("sealed_at"), cbor_int(csil_v.sealed_at)));
    if let Some(csil_inner) = &csil_v.compression {
        csil_entries.push((cbor_text("compression"), csil_enc_compression(csil_inner)?));
    }
    if let Some(csil_inner) = &csil_v.common_properties {
        csil_entries.push((
            cbor_text("common_properties"),
            cbor_enc_array(csil_inner, csil_enc_property)?,
        ));
    }
    Ok(CsilCborValue::Map(csil_entries))
}

/// Reconstruct a Batch from a decoded CBOR value tree.
fn csil_dec_batch(csil_root: &CsilCborValue) -> Result<Batch, CsilCborError> {
    cbor_check_record(csil_root)?;
    let batch_id = {
        let csil_field = cbor_require(csil_root, "batch_id")?;
        let csil_decode = cbor_as_bytes;
        csil_decode(csil_field)?
    };
    let items = {
        let csil_field = cbor_require(csil_root, "items")?;
        let csil_decode = |csil_v| cbor_dec_array(csil_v, csil_dec_telemetry_item);
        csil_decode(csil_field)?
    };
    let common_properties = match cbor_map_get(csil_root, "common_properties") {
        Some(csil_field) => {
            let csil_decode = |csil_v| cbor_dec_array(csil_v, csil_dec_property);
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let sealed_at = {
        let csil_field = cbor_require(csil_root, "sealed_at")?;
        let csil_decode = cbor_as_i64;
        csil_decode(csil_field)?
    };
    let compression = match cbor_map_get(csil_root, "compression") {
        Some(csil_field) => {
            let csil_decode = csil_dec_compression;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    Ok(Batch {
        batch_id,
        items,
        common_properties,
        sealed_at,
        compression,
    })
}

/// Encode a Batch to canonical CSIL CBOR bytes.
pub fn encode_batch(csil_v: &Batch) -> Result<Vec<u8>, CsilCborError> {
    let csil_tree = csil_enc_batch(csil_v)?;
    Ok(cbor_encode(&csil_tree))
}

/// Decode canonical CSIL CBOR bytes into a Batch.
pub fn decode_batch(csil_data: &[u8]) -> Result<Batch, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_batch(&csil_root)
}

/// Build the canonical CBOR value tree for a SubmitBatchRequest.
fn csil_enc_submit_batch_request(
    csil_v: &SubmitBatchRequest,
) -> Result<CsilCborValue, CsilCborError> {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(3);
    csil_entries.push((cbor_text("batch"), csil_enc_batch(&csil_v.batch)?));
    if let Some(csil_inner) = &csil_v.policy_version {
        csil_entries.push((cbor_text("policy_version"), cbor_uint(*csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.protocol_version {
        csil_entries.push((cbor_text("protocol_version"), cbor_uint(*csil_inner)));
    }
    Ok(CsilCborValue::Map(csil_entries))
}

/// Reconstruct a SubmitBatchRequest from a decoded CBOR value tree.
fn csil_dec_submit_batch_request(
    csil_root: &CsilCborValue,
) -> Result<SubmitBatchRequest, CsilCborError> {
    cbor_check_record(csil_root)?;
    let batch = {
        let csil_field = cbor_require(csil_root, "batch")?;
        let csil_decode = csil_dec_batch;
        csil_decode(csil_field)?
    };
    let policy_version = match cbor_map_get(csil_root, "policy_version") {
        Some(csil_field) => {
            let csil_decode = cbor_as_u64;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let protocol_version = match cbor_map_get(csil_root, "protocol_version") {
        Some(csil_field) => {
            let csil_decode = cbor_as_u64;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    Ok(SubmitBatchRequest {
        batch,
        policy_version,
        protocol_version,
    })
}

/// Encode a SubmitBatchRequest to canonical CSIL CBOR bytes.
pub fn encode_submit_batch_request(csil_v: &SubmitBatchRequest) -> Result<Vec<u8>, CsilCborError> {
    let csil_tree = csil_enc_submit_batch_request(csil_v)?;
    Ok(cbor_encode(&csil_tree))
}

/// Decode canonical CSIL CBOR bytes into a SubmitBatchRequest.
pub fn decode_submit_batch_request(csil_data: &[u8]) -> Result<SubmitBatchRequest, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_submit_batch_request(&csil_root)
}

/// Build the canonical CBOR value tree for a SubmitBatchResponse.
fn csil_enc_submit_batch_response(
    csil_v: &SubmitBatchResponse,
) -> Result<CsilCborValue, CsilCborError> {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(6);
    csil_entries.push((cbor_text("accepted"), cbor_uint(csil_v.accepted)));
    csil_entries.push((cbor_text("batch_id"), cbor_bytes(&csil_v.batch_id)));
    if let Some(csil_inner) = &csil_v.rejected {
        csil_entries.push((
            cbor_text("rejected"),
            cbor_enc_array(csil_inner, csil_enc_rejected_item)?,
        ));
    }
    csil_entries.push((cbor_text("queued_at"), cbor_int(csil_v.queued_at)));
    csil_entries.push((
        cbor_text("durable_copies"),
        cbor_uint(csil_v.durable_copies),
    ));
    if let Some(csil_inner) = &csil_v.policy_version {
        csil_entries.push((cbor_text("policy_version"), cbor_uint(*csil_inner)));
    }
    Ok(CsilCborValue::Map(csil_entries))
}

/// Reconstruct a SubmitBatchResponse from a decoded CBOR value tree.
fn csil_dec_submit_batch_response(
    csil_root: &CsilCborValue,
) -> Result<SubmitBatchResponse, CsilCborError> {
    cbor_check_record(csil_root)?;
    let batch_id = {
        let csil_field = cbor_require(csil_root, "batch_id")?;
        let csil_decode = cbor_as_bytes;
        csil_decode(csil_field)?
    };
    let accepted = {
        let csil_field = cbor_require(csil_root, "accepted")?;
        let csil_decode = cbor_as_u64;
        csil_decode(csil_field)?
    };
    let durable_copies = {
        let csil_field = cbor_require(csil_root, "durable_copies")?;
        let csil_decode = cbor_as_u64;
        csil_decode(csil_field)?
    };
    let queued_at = {
        let csil_field = cbor_require(csil_root, "queued_at")?;
        let csil_decode = cbor_as_i64;
        csil_decode(csil_field)?
    };
    let rejected = match cbor_map_get(csil_root, "rejected") {
        Some(csil_field) => {
            let csil_decode = |csil_v| cbor_dec_array(csil_v, csil_dec_rejected_item);
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let policy_version = match cbor_map_get(csil_root, "policy_version") {
        Some(csil_field) => {
            let csil_decode = cbor_as_u64;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    Ok(SubmitBatchResponse {
        batch_id,
        accepted,
        durable_copies,
        queued_at,
        rejected,
        policy_version,
    })
}

/// Encode a SubmitBatchResponse to canonical CSIL CBOR bytes.
pub fn encode_submit_batch_response(
    csil_v: &SubmitBatchResponse,
) -> Result<Vec<u8>, CsilCborError> {
    let csil_tree = csil_enc_submit_batch_response(csil_v)?;
    Ok(cbor_encode(&csil_tree))
}

/// Decode canonical CSIL CBOR bytes into a SubmitBatchResponse.
pub fn decode_submit_batch_response(
    csil_data: &[u8],
) -> Result<SubmitBatchResponse, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_submit_batch_response(&csil_root)
}

/// Build the canonical CBOR value tree for a CommitBatchRequest.
fn csil_enc_commit_batch_request(
    csil_v: &CommitBatchRequest,
) -> Result<CsilCborValue, CsilCborError> {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(4);
    csil_entries.push((cbor_text("batch"), csil_enc_batch(&csil_v.batch)?));
    if let Some(csil_inner) = &csil_v.attempt {
        csil_entries.push((cbor_text("attempt"), cbor_uint(*csil_inner)));
    }
    csil_entries.push((cbor_text("source_id"), cbor_bytes(&csil_v.source_id)));
    if let Some(csil_inner) = &csil_v.protocol_version {
        csil_entries.push((cbor_text("protocol_version"), cbor_uint(*csil_inner)));
    }
    Ok(CsilCborValue::Map(csil_entries))
}

/// Reconstruct a CommitBatchRequest from a decoded CBOR value tree.
fn csil_dec_commit_batch_request(
    csil_root: &CsilCborValue,
) -> Result<CommitBatchRequest, CsilCborError> {
    cbor_check_record(csil_root)?;
    let batch = {
        let csil_field = cbor_require(csil_root, "batch")?;
        let csil_decode = csil_dec_batch;
        csil_decode(csil_field)?
    };
    let source_id = {
        let csil_field = cbor_require(csil_root, "source_id")?;
        let csil_decode = cbor_as_bytes;
        csil_decode(csil_field)?
    };
    let attempt = match cbor_map_get(csil_root, "attempt") {
        Some(csil_field) => {
            let csil_decode = cbor_as_u64;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let protocol_version = match cbor_map_get(csil_root, "protocol_version") {
        Some(csil_field) => {
            let csil_decode = cbor_as_u64;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    Ok(CommitBatchRequest {
        batch,
        source_id,
        attempt,
        protocol_version,
    })
}

/// Encode a CommitBatchRequest to canonical CSIL CBOR bytes.
pub fn encode_commit_batch_request(csil_v: &CommitBatchRequest) -> Result<Vec<u8>, CsilCborError> {
    let csil_tree = csil_enc_commit_batch_request(csil_v)?;
    Ok(cbor_encode(&csil_tree))
}

/// Decode canonical CSIL CBOR bytes into a CommitBatchRequest.
pub fn decode_commit_batch_request(csil_data: &[u8]) -> Result<CommitBatchRequest, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_commit_batch_request(&csil_root)
}

/// Build the canonical CBOR value tree for a CommitBatchResponse.
fn csil_enc_commit_batch_response(
    csil_v: &CommitBatchResponse,
) -> Result<CsilCborValue, CsilCborError> {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(9);
    csil_entries.push((cbor_text("accepted"), cbor_uint(csil_v.accepted)));
    csil_entries.push((cbor_text("batch_id"), cbor_bytes(&csil_v.batch_id)));
    if let Some(csil_inner) = &csil_v.rejected {
        csil_entries.push((
            cbor_text("rejected"),
            cbor_enc_array(csil_inner, csil_enc_rejected_item)?,
        ));
    }
    csil_entries.push((cbor_text("committed_at"), cbor_int(csil_v.committed_at)));
    if let Some(csil_inner) = &csil_v.deduplicated {
        csil_entries.push((cbor_text("deduplicated"), cbor_bool(*csil_inner)));
    }
    csil_entries.push((
        cbor_text("commit_watermark"),
        cbor_uint(csil_v.commit_watermark),
    ));
    csil_entries.push((
        cbor_text("protocol_version"),
        cbor_uint(csil_v.protocol_version),
    ));
    csil_entries.push((
        cbor_text("satisfied_policy"),
        csil_enc_receipt_policy(&csil_v.satisfied_policy)?,
    ));
    csil_entries.push((
        cbor_text("projector_version"),
        cbor_uint(csil_v.projector_version),
    ));
    Ok(CsilCborValue::Map(csil_entries))
}

/// Reconstruct a CommitBatchResponse from a decoded CBOR value tree.
fn csil_dec_commit_batch_response(
    csil_root: &CsilCborValue,
) -> Result<CommitBatchResponse, CsilCborError> {
    cbor_check_record(csil_root)?;
    let batch_id = {
        let csil_field = cbor_require(csil_root, "batch_id")?;
        let csil_decode = cbor_as_bytes;
        csil_decode(csil_field)?
    };
    let accepted = {
        let csil_field = cbor_require(csil_root, "accepted")?;
        let csil_decode = cbor_as_u64;
        csil_decode(csil_field)?
    };
    let committed_at = {
        let csil_field = cbor_require(csil_root, "committed_at")?;
        let csil_decode = cbor_as_i64;
        csil_decode(csil_field)?
    };
    let satisfied_policy = {
        let csil_field = cbor_require(csil_root, "satisfied_policy")?;
        let csil_decode = csil_dec_receipt_policy;
        csil_decode(csil_field)?
    };
    let commit_watermark = {
        let csil_field = cbor_require(csil_root, "commit_watermark")?;
        let csil_decode = cbor_as_u64;
        csil_decode(csil_field)?
    };
    let protocol_version = {
        let csil_field = cbor_require(csil_root, "protocol_version")?;
        let csil_decode = cbor_as_u64;
        csil_decode(csil_field)?
    };
    let projector_version = {
        let csil_field = cbor_require(csil_root, "projector_version")?;
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
    let deduplicated = match cbor_map_get(csil_root, "deduplicated") {
        Some(csil_field) => {
            let csil_decode = cbor_as_bool;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    Ok(CommitBatchResponse {
        batch_id,
        accepted,
        committed_at,
        satisfied_policy,
        commit_watermark,
        protocol_version,
        projector_version,
        rejected,
        deduplicated,
    })
}

/// Encode a CommitBatchResponse to canonical CSIL CBOR bytes.
pub fn encode_commit_batch_response(
    csil_v: &CommitBatchResponse,
) -> Result<Vec<u8>, CsilCborError> {
    let csil_tree = csil_enc_commit_batch_response(csil_v)?;
    Ok(cbor_encode(&csil_tree))
}

/// Decode canonical CSIL CBOR bytes into a CommitBatchResponse.
pub fn decode_commit_batch_response(
    csil_data: &[u8],
) -> Result<CommitBatchResponse, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_commit_batch_response(&csil_root)
}

/// Build the canonical CBOR value tree for a DeliveryTask.
fn csil_enc_delivery_task(csil_v: &DeliveryTask) -> Result<CsilCborValue, CsilCborError> {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(11);
    csil_entries.push((cbor_text("batch"), cbor_bytes(&csil_v.batch)));
    csil_entries.push((cbor_text("attempts"), cbor_uint(csil_v.attempts)));
    csil_entries.push((cbor_text("batch_id"), cbor_bytes(&csil_v.batch_id)));
    csil_entries.push((cbor_text("source_id"), cbor_bytes(&csil_v.source_id)));
    csil_entries.push((cbor_text("accepted_at"), cbor_int(csil_v.accepted_at)));
    csil_entries.push((
        cbor_text("compression"),
        csil_enc_compression(&csil_v.compression)?,
    ));
    if let Some(csil_inner) = &csil_v.last_failure {
        csil_entries.push((cbor_text("last_failure"), cbor_text(csil_inner)));
    }
    csil_entries.push((cbor_text("task_version"), cbor_uint(csil_v.task_version)));
    if let Some(csil_inner) = &csil_v.last_attempt_at {
        csil_entries.push((cbor_text("last_attempt_at"), cbor_int(*csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.next_attempt_at {
        csil_entries.push((cbor_text("next_attempt_at"), cbor_int(*csil_inner)));
    }
    csil_entries.push((
        cbor_text("uncompressed_bytes"),
        cbor_uint(csil_v.uncompressed_bytes),
    ));
    Ok(CsilCborValue::Map(csil_entries))
}

/// Reconstruct a DeliveryTask from a decoded CBOR value tree.
fn csil_dec_delivery_task(csil_root: &CsilCborValue) -> Result<DeliveryTask, CsilCborError> {
    cbor_check_record(csil_root)?;
    let task_version = {
        let csil_field = cbor_require(csil_root, "task_version")?;
        let csil_decode = cbor_as_u64;
        csil_decode(csil_field)?
    };
    let batch_id = {
        let csil_field = cbor_require(csil_root, "batch_id")?;
        let csil_decode = cbor_as_bytes;
        csil_decode(csil_field)?
    };
    let source_id = {
        let csil_field = cbor_require(csil_root, "source_id")?;
        let csil_decode = cbor_as_bytes;
        csil_decode(csil_field)?
    };
    let batch = {
        let csil_field = cbor_require(csil_root, "batch")?;
        let csil_decode = cbor_as_bytes;
        csil_decode(csil_field)?
    };
    let compression = {
        let csil_field = cbor_require(csil_root, "compression")?;
        let csil_decode = csil_dec_compression;
        csil_decode(csil_field)?
    };
    let uncompressed_bytes = {
        let csil_field = cbor_require(csil_root, "uncompressed_bytes")?;
        let csil_decode = cbor_as_u64;
        csil_decode(csil_field)?
    };
    let attempts = {
        let csil_field = cbor_require(csil_root, "attempts")?;
        let csil_decode = cbor_as_u64;
        csil_decode(csil_field)?
    };
    let accepted_at = {
        let csil_field = cbor_require(csil_root, "accepted_at")?;
        let csil_decode = cbor_as_i64;
        csil_decode(csil_field)?
    };
    let last_attempt_at = match cbor_map_get(csil_root, "last_attempt_at") {
        Some(csil_field) => {
            let csil_decode = cbor_as_i64;
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
    let last_failure = match cbor_map_get(csil_root, "last_failure") {
        Some(csil_field) => {
            let csil_decode = cbor_as_text;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    Ok(DeliveryTask {
        task_version,
        batch_id,
        source_id,
        batch,
        compression,
        uncompressed_bytes,
        attempts,
        accepted_at,
        last_attempt_at,
        next_attempt_at,
        last_failure,
    })
}

/// Encode a DeliveryTask to canonical CSIL CBOR bytes.
pub fn encode_delivery_task(csil_v: &DeliveryTask) -> Result<Vec<u8>, CsilCborError> {
    let csil_tree = csil_enc_delivery_task(csil_v)?;
    Ok(cbor_encode(&csil_tree))
}

/// Decode canonical CSIL CBOR bytes into a DeliveryTask.
pub fn decode_delivery_task(csil_data: &[u8]) -> Result<DeliveryTask, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_delivery_task(&csil_root)
}

/// Build the canonical CBOR value tree for a ResolveKeyRequest.
fn csil_enc_resolve_key_request(
    csil_v: &ResolveKeyRequest,
) -> Result<CsilCborValue, CsilCborError> {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(1);
    csil_entries.push((cbor_text("credential"), cbor_text(&csil_v.credential)));
    Ok(CsilCborValue::Map(csil_entries))
}

/// Reconstruct a ResolveKeyRequest from a decoded CBOR value tree.
fn csil_dec_resolve_key_request(
    csil_root: &CsilCborValue,
) -> Result<ResolveKeyRequest, CsilCborError> {
    cbor_check_record(csil_root)?;
    let credential = {
        let csil_field = cbor_require(csil_root, "credential")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    Ok(ResolveKeyRequest { credential })
}

/// Encode a ResolveKeyRequest to canonical CSIL CBOR bytes.
pub fn encode_resolve_key_request(csil_v: &ResolveKeyRequest) -> Result<Vec<u8>, CsilCborError> {
    let csil_tree = csil_enc_resolve_key_request(csil_v)?;
    Ok(cbor_encode(&csil_tree))
}

/// Decode canonical CSIL CBOR bytes into a ResolveKeyRequest.
pub fn decode_resolve_key_request(csil_data: &[u8]) -> Result<ResolveKeyRequest, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_resolve_key_request(&csil_root)
}

/// Build the canonical CBOR value tree for a ResolveKeyResponse.
fn csil_enc_resolve_key_response(
    csil_v: &ResolveKeyResponse,
) -> Result<CsilCborValue, CsilCborError> {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(6);
    csil_entries.push((cbor_text("key_id"), cbor_text(&csil_v.key_id)));
    csil_entries.push((cbor_text("source_id"), cbor_bytes(&csil_v.source_id)));
    if let Some(csil_inner) = &csil_v.expires_at {
        csil_entries.push((cbor_text("expires_at"), cbor_int(*csil_inner)));
    }
    csil_entries.push((cbor_text("project_id"), cbor_bytes(&csil_v.project_id)));
    csil_entries.push((cbor_text("cache_ttl_ms"), cbor_int(csil_v.cache_ttl_ms)));
    csil_entries.push((cbor_text("workspace_id"), cbor_bytes(&csil_v.workspace_id)));
    Ok(CsilCborValue::Map(csil_entries))
}

/// Reconstruct a ResolveKeyResponse from a decoded CBOR value tree.
fn csil_dec_resolve_key_response(
    csil_root: &CsilCborValue,
) -> Result<ResolveKeyResponse, CsilCborError> {
    cbor_check_record(csil_root)?;
    let key_id = {
        let csil_field = cbor_require(csil_root, "key_id")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let workspace_id = {
        let csil_field = cbor_require(csil_root, "workspace_id")?;
        let csil_decode = cbor_as_bytes;
        csil_decode(csil_field)?
    };
    let project_id = {
        let csil_field = cbor_require(csil_root, "project_id")?;
        let csil_decode = cbor_as_bytes;
        csil_decode(csil_field)?
    };
    let source_id = {
        let csil_field = cbor_require(csil_root, "source_id")?;
        let csil_decode = cbor_as_bytes;
        csil_decode(csil_field)?
    };
    let cache_ttl_ms = {
        let csil_field = cbor_require(csil_root, "cache_ttl_ms")?;
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
    Ok(ResolveKeyResponse {
        key_id,
        workspace_id,
        project_id,
        source_id,
        cache_ttl_ms,
        expires_at,
    })
}

/// Encode a ResolveKeyResponse to canonical CSIL CBOR bytes.
pub fn encode_resolve_key_response(csil_v: &ResolveKeyResponse) -> Result<Vec<u8>, CsilCborError> {
    let csil_tree = csil_enc_resolve_key_response(csil_v)?;
    Ok(cbor_encode(&csil_tree))
}

/// Decode canonical CSIL CBOR bytes into a ResolveKeyResponse.
pub fn decode_resolve_key_response(csil_data: &[u8]) -> Result<ResolveKeyResponse, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_resolve_key_response(&csil_root)
}

/// Build the canonical CBOR value tree for a RetentionRule.
fn csil_enc_retention_rule(csil_v: &RetentionRule) -> Result<CsilCborValue, CsilCborError> {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(3);
    if let Some(csil_inner) = &csil_v.kind {
        csil_entries.push((cbor_text("kind"), csil_enc_telemetry_kind(csil_inner)?));
    }
    csil_entries.push((cbor_text("class"), csil_enc_retention_class(&csil_v.class)?));
    csil_entries.push((cbor_text("duration_ms"), cbor_int(csil_v.duration_ms)));
    Ok(CsilCborValue::Map(csil_entries))
}

/// Reconstruct a RetentionRule from a decoded CBOR value tree.
fn csil_dec_retention_rule(csil_root: &CsilCborValue) -> Result<RetentionRule, CsilCborError> {
    cbor_check_record(csil_root)?;
    let class = {
        let csil_field = cbor_require(csil_root, "class")?;
        let csil_decode = csil_dec_retention_class;
        csil_decode(csil_field)?
    };
    let duration_ms = {
        let csil_field = cbor_require(csil_root, "duration_ms")?;
        let csil_decode = cbor_as_i64;
        csil_decode(csil_field)?
    };
    let kind = match cbor_map_get(csil_root, "kind") {
        Some(csil_field) => {
            let csil_decode = csil_dec_telemetry_kind;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    Ok(RetentionRule {
        class,
        duration_ms,
        kind,
    })
}

/// Encode a RetentionRule to canonical CSIL CBOR bytes.
pub fn encode_retention_rule(csil_v: &RetentionRule) -> Result<Vec<u8>, CsilCborError> {
    let csil_tree = csil_enc_retention_rule(csil_v)?;
    Ok(cbor_encode(&csil_tree))
}

/// Decode canonical CSIL CBOR bytes into a RetentionRule.
pub fn decode_retention_rule(csil_data: &[u8]) -> Result<RetentionRule, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_retention_rule(&csil_root)
}

/// Build the canonical CBOR value tree for a SamplingClause.
fn csil_enc_sampling_clause(csil_v: &SamplingClause) -> Result<CsilCborValue, CsilCborError> {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(4);
    csil_entries.push((
        cbor_text("mode"),
        csil_enc_sampling_clause_mode(&csil_v.mode)?,
    ));
    if let Some(csil_inner) = &csil_v.count {
        csil_entries.push((cbor_text("count"), cbor_uint(*csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.percent {
        csil_entries.push((cbor_text("percent"), cbor_float(*csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.interval_ms {
        csil_entries.push((cbor_text("interval_ms"), cbor_int(*csil_inner)));
    }
    Ok(CsilCborValue::Map(csil_entries))
}

/// Reconstruct a SamplingClause from a decoded CBOR value tree.
fn csil_dec_sampling_clause(csil_root: &CsilCborValue) -> Result<SamplingClause, CsilCborError> {
    cbor_check_record(csil_root)?;
    let mode = {
        let csil_field = cbor_require(csil_root, "mode")?;
        let csil_decode = csil_dec_sampling_clause_mode;
        csil_decode(csil_field)?
    };
    let percent = match cbor_map_get(csil_root, "percent") {
        Some(csil_field) => {
            let csil_decode = cbor_as_f64;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let count = match cbor_map_get(csil_root, "count") {
        Some(csil_field) => {
            let csil_decode = cbor_as_u64;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let interval_ms = match cbor_map_get(csil_root, "interval_ms") {
        Some(csil_field) => {
            let csil_decode = cbor_as_i64;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    Ok(SamplingClause {
        mode,
        percent,
        count,
        interval_ms,
    })
}

/// Encode a SamplingClause to canonical CSIL CBOR bytes.
pub fn encode_sampling_clause(csil_v: &SamplingClause) -> Result<Vec<u8>, CsilCborError> {
    let csil_tree = csil_enc_sampling_clause(csil_v)?;
    Ok(cbor_encode(&csil_tree))
}

/// Decode canonical CSIL CBOR bytes into a SamplingClause.
pub fn decode_sampling_clause(csil_data: &[u8]) -> Result<SamplingClause, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_sampling_clause(&csil_root)
}

/// Build the canonical CBOR value tree for a TailRule.
fn csil_enc_tail_rule(csil_v: &TailRule) -> Result<CsilCborValue, CsilCborError> {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(3);
    csil_entries.push((cbor_text("name"), cbor_text(&csil_v.name)));
    csil_entries.push((
        cbor_text("sampling"),
        csil_enc_sampling_clause(&csil_v.sampling)?,
    ));
    csil_entries.push((cbor_text("expression"), cbor_bytes(&csil_v.expression)));
    Ok(CsilCborValue::Map(csil_entries))
}

/// Reconstruct a TailRule from a decoded CBOR value tree.
fn csil_dec_tail_rule(csil_root: &CsilCborValue) -> Result<TailRule, CsilCborError> {
    cbor_check_record(csil_root)?;
    let name = {
        let csil_field = cbor_require(csil_root, "name")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let expression = {
        let csil_field = cbor_require(csil_root, "expression")?;
        let csil_decode = cbor_as_bytes;
        csil_decode(csil_field)?
    };
    let sampling = {
        let csil_field = cbor_require(csil_root, "sampling")?;
        let csil_decode = csil_dec_sampling_clause;
        csil_decode(csil_field)?
    };
    Ok(TailRule {
        name,
        expression,
        sampling,
    })
}

/// Encode a TailRule to canonical CSIL CBOR bytes.
pub fn encode_tail_rule(csil_v: &TailRule) -> Result<Vec<u8>, CsilCborError> {
    let csil_tree = csil_enc_tail_rule(csil_v)?;
    Ok(cbor_encode(&csil_tree))
}

/// Decode canonical CSIL CBOR bytes into a TailRule.
pub fn decode_tail_rule(csil_data: &[u8]) -> Result<TailRule, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_tail_rule(&csil_root)
}

/// Build the canonical CBOR value tree for a ProtectedKey.
fn csil_enc_protected_key(csil_v: &ProtectedKey) -> Result<CsilCborValue, CsilCborError> {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(2);
    csil_entries.push((cbor_text("key"), cbor_text(&csil_v.key)));
    csil_entries.push((
        cbor_text("origin"),
        csil_enc_property_origin(&csil_v.origin)?,
    ));
    Ok(CsilCborValue::Map(csil_entries))
}

/// Reconstruct a ProtectedKey from a decoded CBOR value tree.
fn csil_dec_protected_key(csil_root: &CsilCborValue) -> Result<ProtectedKey, CsilCborError> {
    cbor_check_record(csil_root)?;
    let key = {
        let csil_field = cbor_require(csil_root, "key")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let origin = {
        let csil_field = cbor_require(csil_root, "origin")?;
        let csil_decode = csil_dec_property_origin;
        csil_decode(csil_field)?
    };
    Ok(ProtectedKey { key, origin })
}

/// Encode a ProtectedKey to canonical CSIL CBOR bytes.
pub fn encode_protected_key(csil_v: &ProtectedKey) -> Result<Vec<u8>, CsilCborError> {
    let csil_tree = csil_enc_protected_key(csil_v)?;
    Ok(cbor_encode(&csil_tree))
}

/// Decode canonical CSIL CBOR bytes into a ProtectedKey.
pub fn decode_protected_key(csil_data: &[u8]) -> Result<ProtectedKey, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_protected_key(&csil_root)
}

/// Build the canonical CBOR value tree for a CollectionPolicy.
fn csil_enc_collection_policy(csil_v: &CollectionPolicy) -> Result<CsilCborValue, CsilCborError> {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(19);
    csil_entries.push((
        cbor_text("retention"),
        cbor_enc_array(&csil_v.retention, csil_enc_retention_rule)?,
    ));
    if let Some(csil_inner) = &csil_v.tail_rules {
        csil_entries.push((
            cbor_text("tail_rules"),
            cbor_enc_array(csil_inner, csil_enc_tail_rule)?,
        ));
    }
    if let Some(csil_inner) = &csil_v.kill_switch {
        csil_entries.push((cbor_text("kill_switch"), cbor_bool(*csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.redact_keys {
        csil_entries.push((
            cbor_text("redact_keys"),
            cbor_enc_array(csil_inner, |csil_elem| Ok(cbor_text(csil_elem)))?,
        ));
    }
    csil_entries.push((
        cbor_text("enabled_kinds"),
        cbor_enc_array(&csil_v.enabled_kinds, csil_enc_telemetry_kind)?,
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
        cbor_text("protected_keys"),
        cbor_enc_array(&csil_v.protected_keys, csil_enc_protected_key)?,
    ));
    csil_entries.push((
        cbor_text("max_batch_bytes"),
        cbor_uint(csil_v.max_batch_bytes),
    ));
    csil_entries.push((
        cbor_text("max_event_bytes"),
        cbor_uint(csil_v.max_event_bytes),
    ));
    if let Some(csil_inner) = &csil_v.campaign_linking {
        csil_entries.push((
            cbor_text("campaign_linking"),
            csil_enc_campaign_linking(csil_inner)?,
        ));
    }
    csil_entries.push((
        cbor_text("head_sample_rate"),
        cbor_float(csil_v.head_sample_rate),
    ));
    if let Some(csil_inner) = &csil_v.late_span_grace_ms {
        csil_entries.push((cbor_text("late_span_grace_ms"), cbor_int(*csil_inner)));
    }
    csil_entries.push((
        cbor_text("stamped_properties"),
        cbor_enc_array(&csil_v.stamped_properties, csil_enc_property)?,
    ));
    if let Some(csil_inner) = &csil_v.blocked_event_names {
        csil_entries.push((
            cbor_text("blocked_event_names"),
            cbor_enc_array(csil_inner, |csil_elem| Ok(cbor_text(csil_elem)))?,
        ));
    }
    if let Some(csil_inner) = &csil_v.blocked_property_keys {
        csil_entries.push((
            cbor_text("blocked_property_keys"),
            cbor_enc_array(csil_inner, |csil_elem| Ok(cbor_text(csil_elem)))?,
        ));
    }
    if let Some(csil_inner) = &csil_v.always_keep_expressions {
        csil_entries.push((
            cbor_text("always_keep_expressions"),
            cbor_enc_array(csil_inner, |csil_elem| Ok(cbor_bytes(csil_elem)))?,
        ));
    }
    csil_entries.push((
        cbor_text("session_max_lifetime_ms"),
        cbor_int(csil_v.session_max_lifetime_ms),
    ));
    if let Some(csil_inner) = &csil_v.tail_decision_window_ms {
        csil_entries.push((cbor_text("tail_decision_window_ms"), cbor_int(*csil_inner)));
    }
    Ok(CsilCborValue::Map(csil_entries))
}

/// Reconstruct a CollectionPolicy from a decoded CBOR value tree.
fn csil_dec_collection_policy(
    csil_root: &CsilCborValue,
) -> Result<CollectionPolicy, CsilCborError> {
    cbor_check_record(csil_root)?;
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
    let tail_rules = match cbor_map_get(csil_root, "tail_rules") {
        Some(csil_field) => {
            let csil_decode = |csil_v| cbor_dec_array(csil_v, csil_dec_tail_rule);
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let tail_decision_window_ms = match cbor_map_get(csil_root, "tail_decision_window_ms") {
        Some(csil_field) => {
            let csil_decode = cbor_as_i64;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let late_span_grace_ms = match cbor_map_get(csil_root, "late_span_grace_ms") {
        Some(csil_field) => {
            let csil_decode = cbor_as_i64;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let always_keep_expressions = match cbor_map_get(csil_root, "always_keep_expressions") {
        Some(csil_field) => {
            let csil_decode = |csil_v| cbor_dec_array(csil_v, cbor_as_bytes);
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let retention = {
        let csil_field = cbor_require(csil_root, "retention")?;
        let csil_decode = |csil_v| cbor_dec_array(csil_v, csil_dec_retention_rule);
        csil_decode(csil_field)?
    };
    let protected_keys = {
        let csil_field = cbor_require(csil_root, "protected_keys")?;
        let csil_decode = |csil_v| cbor_dec_array(csil_v, csil_dec_protected_key);
        csil_decode(csil_field)?
    };
    let stamped_properties = {
        let csil_field = cbor_require(csil_root, "stamped_properties")?;
        let csil_decode = |csil_v| cbor_dec_array(csil_v, csil_dec_property);
        csil_decode(csil_field)?
    };
    let max_event_bytes = {
        let csil_field = cbor_require(csil_root, "max_event_bytes")?;
        let csil_decode = cbor_as_u64;
        csil_decode(csil_field)?
    };
    let max_batch_bytes = {
        let csil_field = cbor_require(csil_root, "max_batch_bytes")?;
        let csil_decode = cbor_as_u64;
        csil_decode(csil_field)?
    };
    let max_properties = {
        let csil_field = cbor_require(csil_root, "max_properties")?;
        let csil_decode = cbor_as_u64;
        csil_decode(csil_field)?
    };
    let session_max_lifetime_ms = {
        let csil_field = cbor_require(csil_root, "session_max_lifetime_ms")?;
        let csil_decode = cbor_as_i64;
        csil_decode(csil_field)?
    };
    let redact_keys = match cbor_map_get(csil_root, "redact_keys") {
        Some(csil_field) => {
            let csil_decode = |csil_v| cbor_dec_array(csil_v, cbor_as_text);
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
    let campaign_linking = match cbor_map_get(csil_root, "campaign_linking") {
        Some(csil_field) => {
            let csil_decode = csil_dec_campaign_linking;
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
    Ok(CollectionPolicy {
        policy_version,
        enabled_kinds,
        head_sample_rate,
        tail_rules,
        tail_decision_window_ms,
        late_span_grace_ms,
        always_keep_expressions,
        retention,
        protected_keys,
        stamped_properties,
        max_event_bytes,
        max_batch_bytes,
        max_properties,
        session_max_lifetime_ms,
        redact_keys,
        blocked_event_names,
        blocked_property_keys,
        campaign_linking,
        kill_switch,
    })
}

/// Encode a CollectionPolicy to canonical CSIL CBOR bytes.
pub fn encode_collection_policy(csil_v: &CollectionPolicy) -> Result<Vec<u8>, CsilCborError> {
    let csil_tree = csil_enc_collection_policy(csil_v)?;
    Ok(cbor_encode(&csil_tree))
}

/// Decode canonical CSIL CBOR bytes into a CollectionPolicy.
pub fn decode_collection_policy(csil_data: &[u8]) -> Result<CollectionPolicy, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_collection_policy(&csil_root)
}

/// Build the canonical CBOR value tree for a FetchPolicyRequest.
fn csil_enc_fetch_policy_request(
    csil_v: &FetchPolicyRequest,
) -> Result<CsilCborValue, CsilCborError> {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(2);
    csil_entries.push((cbor_text("source_id"), cbor_bytes(&csil_v.source_id)));
    if let Some(csil_inner) = &csil_v.known_version {
        csil_entries.push((cbor_text("known_version"), cbor_uint(*csil_inner)));
    }
    Ok(CsilCborValue::Map(csil_entries))
}

/// Reconstruct a FetchPolicyRequest from a decoded CBOR value tree.
fn csil_dec_fetch_policy_request(
    csil_root: &CsilCborValue,
) -> Result<FetchPolicyRequest, CsilCborError> {
    cbor_check_record(csil_root)?;
    let source_id = {
        let csil_field = cbor_require(csil_root, "source_id")?;
        let csil_decode = cbor_as_bytes;
        csil_decode(csil_field)?
    };
    let known_version = match cbor_map_get(csil_root, "known_version") {
        Some(csil_field) => {
            let csil_decode = cbor_as_u64;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    Ok(FetchPolicyRequest {
        source_id,
        known_version,
    })
}

/// Encode a FetchPolicyRequest to canonical CSIL CBOR bytes.
pub fn encode_fetch_policy_request(csil_v: &FetchPolicyRequest) -> Result<Vec<u8>, CsilCborError> {
    let csil_tree = csil_enc_fetch_policy_request(csil_v)?;
    Ok(cbor_encode(&csil_tree))
}

/// Decode canonical CSIL CBOR bytes into a FetchPolicyRequest.
pub fn decode_fetch_policy_request(csil_data: &[u8]) -> Result<FetchPolicyRequest, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_fetch_policy_request(&csil_root)
}

/// Build the canonical CBOR value tree for a FetchPolicyResponse.
fn csil_enc_fetch_policy_response(
    csil_v: &FetchPolicyResponse,
) -> Result<CsilCborValue, CsilCborError> {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(2);
    if let Some(csil_inner) = &csil_v.policy {
        csil_entries.push((cbor_text("policy"), csil_enc_collection_policy(csil_inner)?));
    }
    csil_entries.push((cbor_text("unchanged"), cbor_bool(csil_v.unchanged)));
    Ok(CsilCborValue::Map(csil_entries))
}

/// Reconstruct a FetchPolicyResponse from a decoded CBOR value tree.
fn csil_dec_fetch_policy_response(
    csil_root: &CsilCborValue,
) -> Result<FetchPolicyResponse, CsilCborError> {
    cbor_check_record(csil_root)?;
    let policy = match cbor_map_get(csil_root, "policy") {
        Some(csil_field) => {
            let csil_decode = csil_dec_collection_policy;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let unchanged = {
        let csil_field = cbor_require(csil_root, "unchanged")?;
        let csil_decode = cbor_as_bool;
        csil_decode(csil_field)?
    };
    Ok(FetchPolicyResponse { policy, unchanged })
}

/// Encode a FetchPolicyResponse to canonical CSIL CBOR bytes.
pub fn encode_fetch_policy_response(
    csil_v: &FetchPolicyResponse,
) -> Result<Vec<u8>, CsilCborError> {
    let csil_tree = csil_enc_fetch_policy_response(csil_v)?;
    Ok(cbor_encode(&csil_tree))
}

/// Decode canonical CSIL CBOR bytes into a FetchPolicyResponse.
pub fn decode_fetch_policy_response(
    csil_data: &[u8],
) -> Result<FetchPolicyResponse, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_fetch_policy_response(&csil_root)
}

/// Build the canonical CBOR value tree for a CollectorHealth.
fn csil_enc_collector_health(csil_v: &CollectorHealth) -> Result<CsilCborValue, CsilCborError> {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(8);
    csil_entries.push((
        cbor_text("role"),
        csil_enc_collector_health_role(&csil_v.role)?,
    ));
    csil_entries.push((cbor_text("ready"), cbor_bool(csil_v.ready)));
    csil_entries.push((cbor_text("queue_depth"), cbor_uint(csil_v.queue_depth)));
    if let Some(csil_inner) = &csil_v.last_sweep_at {
        csil_entries.push((cbor_text("last_sweep_at"), cbor_int(*csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.policy_age_ms {
        csil_entries.push((cbor_text("policy_age_ms"), cbor_int(*csil_inner)));
    }
    csil_entries.push((
        cbor_text("quarantine_count"),
        cbor_uint(csil_v.quarantine_count),
    ));
    csil_entries.push((
        cbor_text("oldest_task_age_ms"),
        cbor_int(csil_v.oldest_task_age_ms),
    ));
    csil_entries.push((
        cbor_text("applied_policy_version"),
        cbor_uint(csil_v.applied_policy_version),
    ));
    Ok(CsilCborValue::Map(csil_entries))
}

/// Reconstruct a CollectorHealth from a decoded CBOR value tree.
fn csil_dec_collector_health(csil_root: &CsilCborValue) -> Result<CollectorHealth, CsilCborError> {
    cbor_check_record(csil_root)?;
    let role = {
        let csil_field = cbor_require(csil_root, "role")?;
        let csil_decode = csil_dec_collector_health_role;
        csil_decode(csil_field)?
    };
    let ready = {
        let csil_field = cbor_require(csil_root, "ready")?;
        let csil_decode = cbor_as_bool;
        csil_decode(csil_field)?
    };
    let queue_depth = {
        let csil_field = cbor_require(csil_root, "queue_depth")?;
        let csil_decode = cbor_as_u64;
        csil_decode(csil_field)?
    };
    let oldest_task_age_ms = {
        let csil_field = cbor_require(csil_root, "oldest_task_age_ms")?;
        let csil_decode = cbor_as_i64;
        csil_decode(csil_field)?
    };
    let quarantine_count = {
        let csil_field = cbor_require(csil_root, "quarantine_count")?;
        let csil_decode = cbor_as_u64;
        csil_decode(csil_field)?
    };
    let applied_policy_version = {
        let csil_field = cbor_require(csil_root, "applied_policy_version")?;
        let csil_decode = cbor_as_u64;
        csil_decode(csil_field)?
    };
    let policy_age_ms = match cbor_map_get(csil_root, "policy_age_ms") {
        Some(csil_field) => {
            let csil_decode = cbor_as_i64;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    let last_sweep_at = match cbor_map_get(csil_root, "last_sweep_at") {
        Some(csil_field) => {
            let csil_decode = cbor_as_i64;
            Some(csil_decode(csil_field)?)
        }
        None => None,
    };
    Ok(CollectorHealth {
        role,
        ready,
        queue_depth,
        oldest_task_age_ms,
        quarantine_count,
        applied_policy_version,
        policy_age_ms,
        last_sweep_at,
    })
}

/// Encode a CollectorHealth to canonical CSIL CBOR bytes.
pub fn encode_collector_health(csil_v: &CollectorHealth) -> Result<Vec<u8>, CsilCborError> {
    let csil_tree = csil_enc_collector_health(csil_v)?;
    Ok(cbor_encode(&csil_tree))
}

/// Decode canonical CSIL CBOR bytes into a CollectorHealth.
pub fn decode_collector_health(csil_data: &[u8]) -> Result<CollectorHealth, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_collector_health(&csil_root)
}

/// Build the canonical CBOR value tree for a HealthRequest.
fn csil_enc_health_request(_csil_v: &HealthRequest) -> Result<CsilCborValue, CsilCborError> {
    Ok(CsilCborValue::Map(Vec::new()))
}

/// Reconstruct a HealthRequest from a decoded CBOR value tree.
fn csil_dec_health_request(csil_root: &CsilCborValue) -> Result<HealthRequest, CsilCborError> {
    cbor_check_record(csil_root)?;
    Ok(HealthRequest {})
}

/// Encode a HealthRequest to canonical CSIL CBOR bytes.
pub fn encode_health_request(csil_v: &HealthRequest) -> Result<Vec<u8>, CsilCborError> {
    let csil_tree = csil_enc_health_request(csil_v)?;
    Ok(cbor_encode(&csil_tree))
}

/// Decode canonical CSIL CBOR bytes into a HealthRequest.
pub fn decode_health_request(csil_data: &[u8]) -> Result<HealthRequest, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_health_request(&csil_root)
}

/// Build the canonical CBOR value tree for a TypedValue.
fn csil_enc_typed_value(csil_v: &TypedValue) -> Result<CsilCborValue, CsilCborError> {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(8);
    csil_entries.push((cbor_text("kind"), csil_enc_typed_value_kind(&csil_v.kind)?));
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
        csil_entries.push((cbor_text("decimal_value"), csil_rt_enc_decimal(csil_inner)?));
    }
    Ok(CsilCborValue::Map(csil_entries))
}

/// Reconstruct a TypedValue from a decoded CBOR value tree.
fn csil_dec_typed_value(csil_root: &CsilCborValue) -> Result<TypedValue, CsilCborError> {
    cbor_check_record(csil_root)?;
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
pub fn encode_typed_value(csil_v: &TypedValue) -> Result<Vec<u8>, CsilCborError> {
    let csil_tree = csil_enc_typed_value(csil_v)?;
    Ok(cbor_encode(&csil_tree))
}

/// Decode canonical CSIL CBOR bytes into a TypedValue.
pub fn decode_typed_value(csil_data: &[u8]) -> Result<TypedValue, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_typed_value(&csil_root)
}

/// Build the canonical CBOR value tree for a Property.
fn csil_enc_property(csil_v: &Property) -> Result<CsilCborValue, CsilCborError> {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(3);
    csil_entries.push((cbor_text("key"), cbor_text(&csil_v.key)));
    csil_entries.push((cbor_text("value"), csil_enc_typed_value(&csil_v.value)?));
    csil_entries.push((
        cbor_text("origin"),
        csil_enc_property_origin(&csil_v.origin)?,
    ));
    Ok(CsilCborValue::Map(csil_entries))
}

/// Reconstruct a Property from a decoded CBOR value tree.
fn csil_dec_property(csil_root: &CsilCborValue) -> Result<Property, CsilCborError> {
    cbor_check_record(csil_root)?;
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
pub fn encode_property(csil_v: &Property) -> Result<Vec<u8>, CsilCborError> {
    let csil_tree = csil_enc_property(csil_v)?;
    Ok(cbor_encode(&csil_tree))
}

/// Decode canonical CSIL CBOR bytes into a Property.
pub fn decode_property(csil_data: &[u8]) -> Result<Property, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_property(&csil_root)
}

/// Build the canonical CBOR value tree for a Measurement.
fn csil_enc_measurement(csil_v: &Measurement) -> Result<CsilCborValue, CsilCborError> {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(6);
    csil_entries.push((cbor_text("key"), cbor_text(&csil_v.key)));
    csil_entries.push((cbor_text("kind"), csil_enc_measurement_kind(&csil_v.kind)?));
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
        csil_entries.push((cbor_text("decimal_value"), csil_rt_enc_decimal(csil_inner)?));
    }
    Ok(CsilCborValue::Map(csil_entries))
}

/// Reconstruct a Measurement from a decoded CBOR value tree.
fn csil_dec_measurement(csil_root: &CsilCborValue) -> Result<Measurement, CsilCborError> {
    cbor_check_record(csil_root)?;
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
pub fn encode_measurement(csil_v: &Measurement) -> Result<Vec<u8>, CsilCborError> {
    let csil_tree = csil_enc_measurement(csil_v)?;
    Ok(cbor_encode(&csil_tree))
}

/// Decode canonical CSIL CBOR bytes into a Measurement.
pub fn decode_measurement(csil_data: &[u8]) -> Result<Measurement, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_measurement(&csil_root)
}

/// Build the canonical CBOR value tree for a Consent.
fn csil_enc_consent(csil_v: &Consent) -> Result<CsilCborValue, CsilCborError> {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(3);
    csil_entries.push((
        cbor_text("analytics"),
        csil_enc_consent_state(&csil_v.analytics)?,
    ));
    csil_entries.push((
        cbor_text("marketing"),
        csil_enc_consent_state(&csil_v.marketing)?,
    ));
    if let Some(csil_inner) = &csil_v.policy_version {
        csil_entries.push((cbor_text("policy_version"), cbor_text(csil_inner)));
    }
    Ok(CsilCborValue::Map(csil_entries))
}

/// Reconstruct a Consent from a decoded CBOR value tree.
fn csil_dec_consent(csil_root: &CsilCborValue) -> Result<Consent, CsilCborError> {
    cbor_check_record(csil_root)?;
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
pub fn encode_consent(csil_v: &Consent) -> Result<Vec<u8>, CsilCborError> {
    let csil_tree = csil_enc_consent(csil_v)?;
    Ok(cbor_encode(&csil_tree))
}

/// Decode canonical CSIL CBOR bytes into a Consent.
pub fn decode_consent(csil_data: &[u8]) -> Result<Consent, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_consent(&csil_root)
}

/// Build the canonical CBOR value tree for a Envelope.
fn csil_enc_envelope(csil_v: &Envelope) -> Result<CsilCborValue, CsilCborError> {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(23);
    csil_entries.push((cbor_text("kind"), csil_enc_telemetry_kind(&csil_v.kind)?));
    if let Some(csil_inner) = &csil_v.consent {
        csil_entries.push((cbor_text("consent"), csil_enc_consent(csil_inner)?));
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
        cbor_enc_array(&csil_v.properties, csil_enc_property)?,
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
            cbor_enc_array(csil_inner, csil_enc_measurement)?,
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
    Ok(CsilCborValue::Map(csil_entries))
}

/// Reconstruct a Envelope from a decoded CBOR value tree.
fn csil_dec_envelope(csil_root: &CsilCborValue) -> Result<Envelope, CsilCborError> {
    cbor_check_record(csil_root)?;
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
pub fn encode_envelope(csil_v: &Envelope) -> Result<Vec<u8>, CsilCborError> {
    let csil_tree = csil_enc_envelope(csil_v)?;
    Ok(cbor_encode(&csil_tree))
}

/// Decode canonical CSIL CBOR bytes into a Envelope.
pub fn decode_envelope(csil_data: &[u8]) -> Result<Envelope, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_envelope(&csil_root)
}

/// Build the canonical CBOR value tree for a ServiceError.
fn csil_enc_service_error(csil_v: &ServiceError) -> Result<CsilCborValue, CsilCborError> {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(4);
    csil_entries.push((cbor_text("code"), csil_enc_error_code(&csil_v.code)?));
    if let Some(csil_inner) = &csil_v.detail {
        csil_entries.push((
            cbor_text("detail"),
            cbor_enc_array(csil_inner, csil_enc_property)?,
        ));
    }
    csil_entries.push((cbor_text("message"), cbor_text(&csil_v.message)));
    csil_entries.push((cbor_text("retryable"), cbor_bool(csil_v.retryable)));
    Ok(CsilCborValue::Map(csil_entries))
}

/// Reconstruct a ServiceError from a decoded CBOR value tree.
fn csil_dec_service_error(csil_root: &CsilCborValue) -> Result<ServiceError, CsilCborError> {
    cbor_check_record(csil_root)?;
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
pub fn encode_service_error(csil_v: &ServiceError) -> Result<Vec<u8>, CsilCborError> {
    let csil_tree = csil_enc_service_error(csil_v)?;
    Ok(cbor_encode(&csil_tree))
}

/// Decode canonical CSIL CBOR bytes into a ServiceError.
pub fn decode_service_error(csil_data: &[u8]) -> Result<ServiceError, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_service_error(&csil_root)
}

/// Build the canonical CBOR value tree for a EventPayload.
fn csil_enc_event_payload(csil_v: &EventPayload) -> Result<CsilCborValue, CsilCborError> {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(3);
    csil_entries.push((cbor_text("name"), cbor_text(&csil_v.name)));
    if let Some(csil_inner) = &csil_v.route {
        csil_entries.push((cbor_text("route"), cbor_text(csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.page_title {
        csil_entries.push((cbor_text("page_title"), cbor_text(csil_inner)));
    }
    Ok(CsilCborValue::Map(csil_entries))
}

/// Reconstruct a EventPayload from a decoded CBOR value tree.
fn csil_dec_event_payload(csil_root: &CsilCborValue) -> Result<EventPayload, CsilCborError> {
    cbor_check_record(csil_root)?;
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
pub fn encode_event_payload(csil_v: &EventPayload) -> Result<Vec<u8>, CsilCborError> {
    let csil_tree = csil_enc_event_payload(csil_v)?;
    Ok(cbor_encode(&csil_tree))
}

/// Decode canonical CSIL CBOR bytes into a EventPayload.
pub fn decode_event_payload(csil_data: &[u8]) -> Result<EventPayload, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_event_payload(&csil_root)
}

/// Build the canonical CBOR value tree for a PageViewPayload.
fn csil_enc_page_view_payload(csil_v: &PageViewPayload) -> Result<CsilCborValue, CsilCborError> {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(4);
    csil_entries.push((cbor_text("route"), cbor_text(&csil_v.route)));
    if let Some(csil_inner) = &csil_v.campaign {
        csil_entries.push((
            cbor_text("campaign"),
            csil_enc_campaign_parameters(csil_inner)?,
        ));
    }
    if let Some(csil_inner) = &csil_v.referrer {
        csil_entries.push((cbor_text("referrer"), cbor_text(csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.page_title {
        csil_entries.push((cbor_text("page_title"), cbor_text(csil_inner)));
    }
    Ok(CsilCborValue::Map(csil_entries))
}

/// Reconstruct a PageViewPayload from a decoded CBOR value tree.
fn csil_dec_page_view_payload(csil_root: &CsilCborValue) -> Result<PageViewPayload, CsilCborError> {
    cbor_check_record(csil_root)?;
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
pub fn encode_page_view_payload(csil_v: &PageViewPayload) -> Result<Vec<u8>, CsilCborError> {
    let csil_tree = csil_enc_page_view_payload(csil_v)?;
    Ok(cbor_encode(&csil_tree))
}

/// Decode canonical CSIL CBOR bytes into a PageViewPayload.
pub fn decode_page_view_payload(csil_data: &[u8]) -> Result<PageViewPayload, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_page_view_payload(&csil_root)
}

/// Build the canonical CBOR value tree for a CampaignParameters.
fn csil_enc_campaign_parameters(
    csil_v: &CampaignParameters,
) -> Result<CsilCborValue, CsilCborError> {
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
    Ok(CsilCborValue::Map(csil_entries))
}

/// Reconstruct a CampaignParameters from a decoded CBOR value tree.
fn csil_dec_campaign_parameters(
    csil_root: &CsilCborValue,
) -> Result<CampaignParameters, CsilCborError> {
    cbor_check_record(csil_root)?;
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
pub fn encode_campaign_parameters(csil_v: &CampaignParameters) -> Result<Vec<u8>, CsilCborError> {
    let csil_tree = csil_enc_campaign_parameters(csil_v)?;
    Ok(cbor_encode(&csil_tree))
}

/// Decode canonical CSIL CBOR bytes into a CampaignParameters.
pub fn decode_campaign_parameters(csil_data: &[u8]) -> Result<CampaignParameters, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_campaign_parameters(&csil_root)
}

/// Build the canonical CBOR value tree for a SessionStartPayload.
fn csil_enc_session_start_payload(
    csil_v: &SessionStartPayload,
) -> Result<CsilCborValue, CsilCborError> {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(1);
    if let Some(csil_inner) = &csil_v.entry_route {
        csil_entries.push((cbor_text("entry_route"), cbor_text(csil_inner)));
    }
    Ok(CsilCborValue::Map(csil_entries))
}

/// Reconstruct a SessionStartPayload from a decoded CBOR value tree.
fn csil_dec_session_start_payload(
    csil_root: &CsilCborValue,
) -> Result<SessionStartPayload, CsilCborError> {
    cbor_check_record(csil_root)?;
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
pub fn encode_session_start_payload(
    csil_v: &SessionStartPayload,
) -> Result<Vec<u8>, CsilCborError> {
    let csil_tree = csil_enc_session_start_payload(csil_v)?;
    Ok(cbor_encode(&csil_tree))
}

/// Decode canonical CSIL CBOR bytes into a SessionStartPayload.
pub fn decode_session_start_payload(
    csil_data: &[u8],
) -> Result<SessionStartPayload, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_session_start_payload(&csil_root)
}

/// Build the canonical CBOR value tree for a SessionEndPayload.
fn csil_enc_session_end_payload(
    csil_v: &SessionEndPayload,
) -> Result<CsilCborValue, CsilCborError> {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(1);
    csil_entries.push((
        cbor_text("reason"),
        csil_enc_session_end_payload_reason(&csil_v.reason)?,
    ));
    Ok(CsilCborValue::Map(csil_entries))
}

/// Reconstruct a SessionEndPayload from a decoded CBOR value tree.
fn csil_dec_session_end_payload(
    csil_root: &CsilCborValue,
) -> Result<SessionEndPayload, CsilCborError> {
    cbor_check_record(csil_root)?;
    let reason = {
        let csil_field = cbor_require(csil_root, "reason")?;
        let csil_decode = csil_dec_session_end_payload_reason;
        csil_decode(csil_field)?
    };
    Ok(SessionEndPayload { reason })
}

/// Encode a SessionEndPayload to canonical CSIL CBOR bytes.
pub fn encode_session_end_payload(csil_v: &SessionEndPayload) -> Result<Vec<u8>, CsilCborError> {
    let csil_tree = csil_enc_session_end_payload(csil_v)?;
    Ok(cbor_encode(&csil_tree))
}

/// Decode canonical CSIL CBOR bytes into a SessionEndPayload.
pub fn decode_session_end_payload(csil_data: &[u8]) -> Result<SessionEndPayload, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_session_end_payload(&csil_root)
}

/// Build the canonical CBOR value tree for a InteractionPayload.
fn csil_enc_interaction_payload(
    csil_v: &InteractionPayload,
) -> Result<CsilCborValue, CsilCborError> {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(2);
    csil_entries.push((cbor_text("action"), cbor_text(&csil_v.action)));
    csil_entries.push((cbor_text("target"), cbor_text(&csil_v.target)));
    Ok(CsilCborValue::Map(csil_entries))
}

/// Reconstruct a InteractionPayload from a decoded CBOR value tree.
fn csil_dec_interaction_payload(
    csil_root: &CsilCborValue,
) -> Result<InteractionPayload, CsilCborError> {
    cbor_check_record(csil_root)?;
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
pub fn encode_interaction_payload(csil_v: &InteractionPayload) -> Result<Vec<u8>, CsilCborError> {
    let csil_tree = csil_enc_interaction_payload(csil_v)?;
    Ok(cbor_encode(&csil_tree))
}

/// Decode canonical CSIL CBOR bytes into a InteractionPayload.
pub fn decode_interaction_payload(csil_data: &[u8]) -> Result<InteractionPayload, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_interaction_payload(&csil_root)
}

/// Build the canonical CBOR value tree for a FeatureExposurePayload.
fn csil_enc_feature_exposure_payload(
    csil_v: &FeatureExposurePayload,
) -> Result<CsilCborValue, CsilCborError> {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(2);
    csil_entries.push((cbor_text("feature"), cbor_text(&csil_v.feature)));
    csil_entries.push((cbor_text("variant"), cbor_text(&csil_v.variant)));
    Ok(CsilCborValue::Map(csil_entries))
}

/// Reconstruct a FeatureExposurePayload from a decoded CBOR value tree.
fn csil_dec_feature_exposure_payload(
    csil_root: &CsilCborValue,
) -> Result<FeatureExposurePayload, CsilCborError> {
    cbor_check_record(csil_root)?;
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
pub fn encode_feature_exposure_payload(
    csil_v: &FeatureExposurePayload,
) -> Result<Vec<u8>, CsilCborError> {
    let csil_tree = csil_enc_feature_exposure_payload(csil_v)?;
    Ok(cbor_encode(&csil_tree))
}

/// Decode canonical CSIL CBOR bytes into a FeatureExposurePayload.
pub fn decode_feature_exposure_payload(
    csil_data: &[u8],
) -> Result<FeatureExposurePayload, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_feature_exposure_payload(&csil_root)
}

/// Build the canonical CBOR value tree for a IdentifyPayload.
fn csil_enc_identify_payload(csil_v: &IdentifyPayload) -> Result<CsilCborValue, CsilCborError> {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(1);
    csil_entries.push((cbor_text("end_user_id"), cbor_text(&csil_v.end_user_id)));
    Ok(CsilCborValue::Map(csil_entries))
}

/// Reconstruct a IdentifyPayload from a decoded CBOR value tree.
fn csil_dec_identify_payload(csil_root: &CsilCborValue) -> Result<IdentifyPayload, CsilCborError> {
    cbor_check_record(csil_root)?;
    let end_user_id = {
        let csil_field = cbor_require(csil_root, "end_user_id")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    Ok(IdentifyPayload { end_user_id })
}

/// Encode a IdentifyPayload to canonical CSIL CBOR bytes.
pub fn encode_identify_payload(csil_v: &IdentifyPayload) -> Result<Vec<u8>, CsilCborError> {
    let csil_tree = csil_enc_identify_payload(csil_v)?;
    Ok(cbor_encode(&csil_tree))
}

/// Decode canonical CSIL CBOR bytes into a IdentifyPayload.
pub fn decode_identify_payload(csil_data: &[u8]) -> Result<IdentifyPayload, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_identify_payload(&csil_root)
}

/// Build the canonical CBOR value tree for a AliasPayload.
fn csil_enc_alias_payload(csil_v: &AliasPayload) -> Result<CsilCborValue, CsilCborError> {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(2);
    csil_entries.push((cbor_text("to_id"), cbor_text(&csil_v.to_id)));
    csil_entries.push((cbor_text("from_id"), cbor_text(&csil_v.from_id)));
    Ok(CsilCborValue::Map(csil_entries))
}

/// Reconstruct a AliasPayload from a decoded CBOR value tree.
fn csil_dec_alias_payload(csil_root: &CsilCborValue) -> Result<AliasPayload, CsilCborError> {
    cbor_check_record(csil_root)?;
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
pub fn encode_alias_payload(csil_v: &AliasPayload) -> Result<Vec<u8>, CsilCborError> {
    let csil_tree = csil_enc_alias_payload(csil_v)?;
    Ok(cbor_encode(&csil_tree))
}

/// Decode canonical CSIL CBOR bytes into a AliasPayload.
pub fn decode_alias_payload(csil_data: &[u8]) -> Result<AliasPayload, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_alias_payload(&csil_root)
}

/// Build the canonical CBOR value tree for a GroupPayload.
fn csil_enc_group_payload(csil_v: &GroupPayload) -> Result<CsilCborValue, CsilCborError> {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(2);
    csil_entries.push((cbor_text("group_id"), cbor_text(&csil_v.group_id)));
    if let Some(csil_inner) = &csil_v.group_kind {
        csil_entries.push((cbor_text("group_kind"), cbor_text(csil_inner)));
    }
    Ok(CsilCborValue::Map(csil_entries))
}

/// Reconstruct a GroupPayload from a decoded CBOR value tree.
fn csil_dec_group_payload(csil_root: &CsilCborValue) -> Result<GroupPayload, CsilCborError> {
    cbor_check_record(csil_root)?;
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
pub fn encode_group_payload(csil_v: &GroupPayload) -> Result<Vec<u8>, CsilCborError> {
    let csil_tree = csil_enc_group_payload(csil_v)?;
    Ok(cbor_encode(&csil_tree))
}

/// Decode canonical CSIL CBOR bytes into a GroupPayload.
pub fn decode_group_payload(csil_data: &[u8]) -> Result<GroupPayload, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_group_payload(&csil_root)
}

/// Build the canonical CBOR value tree for a ConversionPayload.
fn csil_enc_conversion_payload(csil_v: &ConversionPayload) -> Result<CsilCborValue, CsilCborError> {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(6);
    csil_entries.push((cbor_text("goal"), cbor_text(&csil_v.goal)));
    if let Some(csil_inner) = &csil_v.value {
        csil_entries.push((cbor_text("value"), csil_rt_enc_decimal(csil_inner)?));
    }
    if let Some(csil_inner) = &csil_v.campaign {
        csil_entries.push((
            cbor_text("campaign"),
            csil_enc_campaign_parameters(csil_inner)?,
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
    Ok(CsilCborValue::Map(csil_entries))
}

/// Reconstruct a ConversionPayload from a decoded CBOR value tree.
fn csil_dec_conversion_payload(
    csil_root: &CsilCborValue,
) -> Result<ConversionPayload, CsilCborError> {
    cbor_check_record(csil_root)?;
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
pub fn encode_conversion_payload(csil_v: &ConversionPayload) -> Result<Vec<u8>, CsilCborError> {
    let csil_tree = csil_enc_conversion_payload(csil_v)?;
    Ok(cbor_encode(&csil_tree))
}

/// Decode canonical CSIL CBOR bytes into a ConversionPayload.
pub fn decode_conversion_payload(csil_data: &[u8]) -> Result<ConversionPayload, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_conversion_payload(&csil_root)
}

/// Build the canonical CBOR value tree for a StackFrame.
fn csil_enc_stack_frame(csil_v: &StackFrame) -> Result<CsilCborValue, CsilCborError> {
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
    Ok(CsilCborValue::Map(csil_entries))
}

/// Reconstruct a StackFrame from a decoded CBOR value tree.
fn csil_dec_stack_frame(csil_root: &CsilCborValue) -> Result<StackFrame, CsilCborError> {
    cbor_check_record(csil_root)?;
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
pub fn encode_stack_frame(csil_v: &StackFrame) -> Result<Vec<u8>, CsilCborError> {
    let csil_tree = csil_enc_stack_frame(csil_v)?;
    Ok(cbor_encode(&csil_tree))
}

/// Decode canonical CSIL CBOR bytes into a StackFrame.
pub fn decode_stack_frame(csil_data: &[u8]) -> Result<StackFrame, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_stack_frame(&csil_root)
}

/// Build the canonical CBOR value tree for a ErrorPayload.
fn csil_enc_error_payload(csil_v: &ErrorPayload) -> Result<CsilCborValue, CsilCborError> {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(8);
    if let Some(csil_inner) = &csil_v.frames {
        csil_entries.push((
            cbor_text("frames"),
            cbor_enc_array(csil_inner, csil_enc_stack_frame)?,
        ));
    }
    csil_entries.push((cbor_text("handled"), cbor_bool(csil_v.handled)));
    csil_entries.push((cbor_text("message"), cbor_text(&csil_v.message)));
    if let Some(csil_inner) = &csil_v.runtime {
        csil_entries.push((cbor_text("runtime"), cbor_text(csil_inner)));
    }
    csil_entries.push((
        cbor_text("severity"),
        csil_enc_error_payload_severity(&csil_v.severity)?,
    ));
    if let Some(csil_inner) = &csil_v.mechanism {
        csil_entries.push((cbor_text("mechanism"), cbor_text(csil_inner)));
    }
    csil_entries.push((cbor_text("error_type"), cbor_text(&csil_v.error_type)));
    if let Some(csil_inner) = &csil_v.breadcrumbs {
        csil_entries.push((
            cbor_text("breadcrumbs"),
            cbor_enc_array(csil_inner, |csil_elem| Ok(cbor_bytes(csil_elem)))?,
        ));
    }
    Ok(CsilCborValue::Map(csil_entries))
}

/// Reconstruct a ErrorPayload from a decoded CBOR value tree.
fn csil_dec_error_payload(csil_root: &CsilCborValue) -> Result<ErrorPayload, CsilCborError> {
    cbor_check_record(csil_root)?;
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
pub fn encode_error_payload(csil_v: &ErrorPayload) -> Result<Vec<u8>, CsilCborError> {
    let csil_tree = csil_enc_error_payload(csil_v)?;
    Ok(cbor_encode(&csil_tree))
}

/// Decode canonical CSIL CBOR bytes into a ErrorPayload.
pub fn decode_error_payload(csil_data: &[u8]) -> Result<ErrorPayload, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_error_payload(&csil_root)
}

/// Build the canonical CBOR value tree for a SpanLink.
fn csil_enc_span_link(csil_v: &SpanLink) -> Result<CsilCborValue, CsilCborError> {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(2);
    csil_entries.push((cbor_text("span_id"), cbor_bytes(&csil_v.span_id)));
    csil_entries.push((cbor_text("trace_id"), cbor_bytes(&csil_v.trace_id)));
    Ok(CsilCborValue::Map(csil_entries))
}

/// Reconstruct a SpanLink from a decoded CBOR value tree.
fn csil_dec_span_link(csil_root: &CsilCborValue) -> Result<SpanLink, CsilCborError> {
    cbor_check_record(csil_root)?;
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
pub fn encode_span_link(csil_v: &SpanLink) -> Result<Vec<u8>, CsilCborError> {
    let csil_tree = csil_enc_span_link(csil_v)?;
    Ok(cbor_encode(&csil_tree))
}

/// Decode canonical CSIL CBOR bytes into a SpanLink.
pub fn decode_span_link(csil_data: &[u8]) -> Result<SpanLink, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_span_link(&csil_root)
}

/// Build the canonical CBOR value tree for a SpanPayload.
fn csil_enc_span_payload(csil_v: &SpanPayload) -> Result<CsilCborValue, CsilCborError> {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(10);
    csil_entries.push((cbor_text("kind"), csil_enc_span_kind(&csil_v.kind)?));
    if let Some(csil_inner) = &csil_v.links {
        csil_entries.push((
            cbor_text("links"),
            cbor_enc_array(csil_inner, csil_enc_span_link)?,
        ));
    }
    csil_entries.push((
        cbor_text("status"),
        csil_enc_span_payload_status(&csil_v.status)?,
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
    Ok(CsilCborValue::Map(csil_entries))
}

/// Reconstruct a SpanPayload from a decoded CBOR value tree.
fn csil_dec_span_payload(csil_root: &CsilCborValue) -> Result<SpanPayload, CsilCborError> {
    cbor_check_record(csil_root)?;
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
pub fn encode_span_payload(csil_v: &SpanPayload) -> Result<Vec<u8>, CsilCborError> {
    let csil_tree = csil_enc_span_payload(csil_v)?;
    Ok(cbor_encode(&csil_tree))
}

/// Decode canonical CSIL CBOR bytes into a SpanPayload.
pub fn decode_span_payload(csil_data: &[u8]) -> Result<SpanPayload, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_span_payload(&csil_root)
}

/// Build the canonical CBOR value tree for a HistogramValue.
fn csil_enc_histogram_value(csil_v: &HistogramValue) -> Result<CsilCborValue, CsilCborError> {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(4);
    csil_entries.push((cbor_text("sum"), cbor_float(csil_v.sum)));
    csil_entries.push((cbor_text("count"), cbor_uint(csil_v.count)));
    csil_entries.push((
        cbor_text("bounds"),
        cbor_enc_array(&csil_v.bounds, |csil_elem| Ok(cbor_float(*csil_elem)))?,
    ));
    csil_entries.push((
        cbor_text("counts"),
        cbor_enc_array(&csil_v.counts, |csil_elem| Ok(cbor_uint(*csil_elem)))?,
    ));
    Ok(CsilCborValue::Map(csil_entries))
}

/// Reconstruct a HistogramValue from a decoded CBOR value tree.
fn csil_dec_histogram_value(csil_root: &CsilCborValue) -> Result<HistogramValue, CsilCborError> {
    cbor_check_record(csil_root)?;
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
pub fn encode_histogram_value(csil_v: &HistogramValue) -> Result<Vec<u8>, CsilCborError> {
    let csil_tree = csil_enc_histogram_value(csil_v)?;
    Ok(cbor_encode(&csil_tree))
}

/// Decode canonical CSIL CBOR bytes into a HistogramValue.
pub fn decode_histogram_value(csil_data: &[u8]) -> Result<HistogramValue, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_histogram_value(&csil_root)
}

/// Build the canonical CBOR value tree for a MetricPointPayload.
fn csil_enc_metric_point_payload(
    csil_v: &MetricPointPayload,
) -> Result<CsilCborValue, CsilCborError> {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(12);
    if let Some(csil_inner) = &csil_v.unit {
        csil_entries.push((cbor_text("unit"), cbor_text(csil_inner)));
    }
    csil_entries.push((cbor_text("end_at"), cbor_int(csil_v.end_at)));
    csil_entries.push((
        cbor_text("labels"),
        cbor_enc_array(&csil_v.labels, csil_enc_property)?,
    ));
    csil_entries.push((cbor_text("start_at"), cbor_int(csil_v.start_at)));
    csil_entries.push((cbor_text("monotonic"), cbor_bool(csil_v.monotonic)));
    if let Some(csil_inner) = &csil_v.description {
        csil_entries.push((cbor_text("description"), cbor_text(csil_inner)));
    }
    csil_entries.push((
        cbor_text("metric_kind"),
        csil_enc_metric_kind(&csil_v.metric_kind)?,
    ));
    csil_entries.push((cbor_text("metric_name"), cbor_text(&csil_v.metric_name)));
    csil_entries.push((
        cbor_text("temporality"),
        csil_enc_metric_point_payload_temporality(&csil_v.temporality)?,
    ));
    if let Some(csil_inner) = &csil_v.number_value {
        csil_entries.push((cbor_text("number_value"), cbor_float(*csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.histogram_value {
        csil_entries.push((
            cbor_text("histogram_value"),
            csil_enc_histogram_value(csil_inner)?,
        ));
    }
    if let Some(csil_inner) = &csil_v.exemplar_trace_id {
        csil_entries.push((cbor_text("exemplar_trace_id"), cbor_bytes(csil_inner)));
    }
    Ok(CsilCborValue::Map(csil_entries))
}

/// Reconstruct a MetricPointPayload from a decoded CBOR value tree.
fn csil_dec_metric_point_payload(
    csil_root: &CsilCborValue,
) -> Result<MetricPointPayload, CsilCborError> {
    cbor_check_record(csil_root)?;
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
pub fn encode_metric_point_payload(csil_v: &MetricPointPayload) -> Result<Vec<u8>, CsilCborError> {
    let csil_tree = csil_enc_metric_point_payload(csil_v)?;
    Ok(cbor_encode(&csil_tree))
}

/// Decode canonical CSIL CBOR bytes into a MetricPointPayload.
pub fn decode_metric_point_payload(csil_data: &[u8]) -> Result<MetricPointPayload, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_metric_point_payload(&csil_root)
}

/// Build the canonical CBOR value tree for a CampaignTouchPayload.
fn csil_enc_campaign_touch_payload(
    csil_v: &CampaignTouchPayload,
) -> Result<CsilCborValue, CsilCborError> {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(4);
    csil_entries.push((
        cbor_text("campaign"),
        csil_enc_campaign_parameters(&csil_v.campaign)?,
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
    Ok(CsilCborValue::Map(csil_entries))
}

/// Reconstruct a CampaignTouchPayload from a decoded CBOR value tree.
fn csil_dec_campaign_touch_payload(
    csil_root: &CsilCborValue,
) -> Result<CampaignTouchPayload, CsilCborError> {
    cbor_check_record(csil_root)?;
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
pub fn encode_campaign_touch_payload(
    csil_v: &CampaignTouchPayload,
) -> Result<Vec<u8>, CsilCborError> {
    let csil_tree = csil_enc_campaign_touch_payload(csil_v)?;
    Ok(cbor_encode(&csil_tree))
}

/// Decode canonical CSIL CBOR bytes into a CampaignTouchPayload.
pub fn decode_campaign_touch_payload(
    csil_data: &[u8],
) -> Result<CampaignTouchPayload, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_campaign_touch_payload(&csil_root)
}

/// Build the canonical CBOR value tree for a CampaignCostPayload.
fn csil_enc_campaign_cost_payload(
    csil_v: &CampaignCostPayload,
) -> Result<CsilCborValue, CsilCborError> {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(6);
    csil_entries.push((cbor_text("cost"), csil_rt_enc_decimal(&csil_v.cost)?));
    csil_entries.push((cbor_text("campaign"), cbor_text(&csil_v.campaign)));
    csil_entries.push((cbor_text("currency"), cbor_text(&csil_v.currency)));
    if let Some(csil_inner) = &csil_v.platform {
        csil_entries.push((cbor_text("platform"), cbor_text(csil_inner)));
    }
    csil_entries.push((cbor_text("period_end"), cbor_int(csil_v.period_end)));
    csil_entries.push((cbor_text("period_start"), cbor_int(csil_v.period_start)));
    Ok(CsilCborValue::Map(csil_entries))
}

/// Reconstruct a CampaignCostPayload from a decoded CBOR value tree.
fn csil_dec_campaign_cost_payload(
    csil_root: &CsilCborValue,
) -> Result<CampaignCostPayload, CsilCborError> {
    cbor_check_record(csil_root)?;
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
pub fn encode_campaign_cost_payload(
    csil_v: &CampaignCostPayload,
) -> Result<Vec<u8>, CsilCborError> {
    let csil_tree = csil_enc_campaign_cost_payload(csil_v)?;
    Ok(cbor_encode(&csil_tree))
}

/// Decode canonical CSIL CBOR bytes into a CampaignCostPayload.
pub fn decode_campaign_cost_payload(
    csil_data: &[u8],
) -> Result<CampaignCostPayload, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_campaign_cost_payload(&csil_root)
}

/// Build the canonical CBOR value tree for a TelemetryItem.
fn csil_enc_telemetry_item(csil_v: &TelemetryItem) -> Result<CsilCborValue, CsilCborError> {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(16);
    if let Some(csil_inner) = &csil_v.span {
        csil_entries.push((cbor_text("span"), csil_enc_span_payload(csil_inner)?));
    }
    if let Some(csil_inner) = &csil_v.alias {
        csil_entries.push((cbor_text("alias"), csil_enc_alias_payload(csil_inner)?));
    }
    if let Some(csil_inner) = &csil_v.error {
        csil_entries.push((cbor_text("error"), csil_enc_error_payload(csil_inner)?));
    }
    if let Some(csil_inner) = &csil_v.event {
        csil_entries.push((cbor_text("event"), csil_enc_event_payload(csil_inner)?));
    }
    if let Some(csil_inner) = &csil_v.group {
        csil_entries.push((cbor_text("group"), csil_enc_group_payload(csil_inner)?));
    }
    csil_entries.push((cbor_text("envelope"), csil_enc_envelope(&csil_v.envelope)?));
    if let Some(csil_inner) = &csil_v.identify {
        csil_entries.push((
            cbor_text("identify"),
            csil_enc_identify_payload(csil_inner)?,
        ));
    }
    if let Some(csil_inner) = &csil_v.page_view {
        csil_entries.push((
            cbor_text("page_view"),
            csil_enc_page_view_payload(csil_inner)?,
        ));
    }
    if let Some(csil_inner) = &csil_v.conversion {
        csil_entries.push((
            cbor_text("conversion"),
            csil_enc_conversion_payload(csil_inner)?,
        ));
    }
    if let Some(csil_inner) = &csil_v.interaction {
        csil_entries.push((
            cbor_text("interaction"),
            csil_enc_interaction_payload(csil_inner)?,
        ));
    }
    if let Some(csil_inner) = &csil_v.session_end {
        csil_entries.push((
            cbor_text("session_end"),
            csil_enc_session_end_payload(csil_inner)?,
        ));
    }
    if let Some(csil_inner) = &csil_v.metric_point {
        csil_entries.push((
            cbor_text("metric_point"),
            csil_enc_metric_point_payload(csil_inner)?,
        ));
    }
    if let Some(csil_inner) = &csil_v.campaign_cost {
        csil_entries.push((
            cbor_text("campaign_cost"),
            csil_enc_campaign_cost_payload(csil_inner)?,
        ));
    }
    if let Some(csil_inner) = &csil_v.session_start {
        csil_entries.push((
            cbor_text("session_start"),
            csil_enc_session_start_payload(csil_inner)?,
        ));
    }
    if let Some(csil_inner) = &csil_v.campaign_touch {
        csil_entries.push((
            cbor_text("campaign_touch"),
            csil_enc_campaign_touch_payload(csil_inner)?,
        ));
    }
    if let Some(csil_inner) = &csil_v.feature_exposure {
        csil_entries.push((
            cbor_text("feature_exposure"),
            csil_enc_feature_exposure_payload(csil_inner)?,
        ));
    }
    Ok(CsilCborValue::Map(csil_entries))
}

/// Reconstruct a TelemetryItem from a decoded CBOR value tree.
fn csil_dec_telemetry_item(csil_root: &CsilCborValue) -> Result<TelemetryItem, CsilCborError> {
    cbor_check_record(csil_root)?;
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
pub fn encode_telemetry_item(csil_v: &TelemetryItem) -> Result<Vec<u8>, CsilCborError> {
    let csil_tree = csil_enc_telemetry_item(csil_v)?;
    Ok(cbor_encode(&csil_tree))
}

/// Decode canonical CSIL CBOR bytes into a TelemetryItem.
pub fn decode_telemetry_item(csil_data: &[u8]) -> Result<TelemetryItem, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_telemetry_item(&csil_root)
}

/// Build the canonical CBOR value tree for a CaptureRequest.
fn csil_enc_capture_request(csil_v: &CaptureRequest) -> Result<CsilCborValue, CsilCborError> {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(1);
    csil_entries.push((
        cbor_text("items"),
        cbor_enc_array(&csil_v.items, csil_enc_telemetry_item)?,
    ));
    Ok(CsilCborValue::Map(csil_entries))
}

/// Reconstruct a CaptureRequest from a decoded CBOR value tree.
fn csil_dec_capture_request(csil_root: &CsilCborValue) -> Result<CaptureRequest, CsilCborError> {
    cbor_check_record(csil_root)?;
    let items = {
        let csil_field = cbor_require(csil_root, "items")?;
        let csil_decode = |csil_v| cbor_dec_array(csil_v, csil_dec_telemetry_item);
        csil_decode(csil_field)?
    };
    Ok(CaptureRequest { items })
}

/// Encode a CaptureRequest to canonical CSIL CBOR bytes.
pub fn encode_capture_request(csil_v: &CaptureRequest) -> Result<Vec<u8>, CsilCborError> {
    let csil_tree = csil_enc_capture_request(csil_v)?;
    Ok(cbor_encode(&csil_tree))
}

/// Decode canonical CSIL CBOR bytes into a CaptureRequest.
pub fn decode_capture_request(csil_data: &[u8]) -> Result<CaptureRequest, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_capture_request(&csil_root)
}

/// Build the canonical CBOR value tree for a CaptureResponse.
fn csil_enc_capture_response(csil_v: &CaptureResponse) -> Result<CsilCborValue, CsilCborError> {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(2);
    csil_entries.push((cbor_text("accepted"), cbor_uint(csil_v.accepted)));
    if let Some(csil_inner) = &csil_v.rejected {
        csil_entries.push((
            cbor_text("rejected"),
            cbor_enc_array(csil_inner, csil_enc_rejected_item)?,
        ));
    }
    Ok(CsilCborValue::Map(csil_entries))
}

/// Reconstruct a CaptureResponse from a decoded CBOR value tree.
fn csil_dec_capture_response(csil_root: &CsilCborValue) -> Result<CaptureResponse, CsilCborError> {
    cbor_check_record(csil_root)?;
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
pub fn encode_capture_response(csil_v: &CaptureResponse) -> Result<Vec<u8>, CsilCborError> {
    let csil_tree = csil_enc_capture_response(csil_v)?;
    Ok(cbor_encode(&csil_tree))
}

/// Decode canonical CSIL CBOR bytes into a CaptureResponse.
pub fn decode_capture_response(csil_data: &[u8]) -> Result<CaptureResponse, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_capture_response(&csil_root)
}

/// Build the canonical CBOR value tree for a RejectedItem.
fn csil_enc_rejected_item(csil_v: &RejectedItem) -> Result<CsilCborValue, CsilCborError> {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(3);
    csil_entries.push((cbor_text("code"), csil_enc_error_code(&csil_v.code)?));
    csil_entries.push((cbor_text("message"), cbor_text(&csil_v.message)));
    csil_entries.push((cbor_text("event_id"), cbor_bytes(&csil_v.event_id)));
    Ok(CsilCborValue::Map(csil_entries))
}

/// Reconstruct a RejectedItem from a decoded CBOR value tree.
fn csil_dec_rejected_item(csil_root: &CsilCborValue) -> Result<RejectedItem, CsilCborError> {
    cbor_check_record(csil_root)?;
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
pub fn encode_rejected_item(csil_v: &RejectedItem) -> Result<Vec<u8>, CsilCborError> {
    let csil_tree = csil_enc_rejected_item(csil_v)?;
    Ok(cbor_encode(&csil_tree))
}

/// Decode canonical CSIL CBOR bytes into a RejectedItem.
pub fn decode_rejected_item(csil_data: &[u8]) -> Result<RejectedItem, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_rejected_item(&csil_root)
}

/// Build the canonical CBOR value tree for a CaptureCriticalRequest.
fn csil_enc_capture_critical_request(
    csil_v: &CaptureCriticalRequest,
) -> Result<CsilCborValue, CsilCborError> {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(1);
    csil_entries.push((
        cbor_text("items"),
        cbor_enc_array(&csil_v.items, csil_enc_telemetry_item)?,
    ));
    Ok(CsilCborValue::Map(csil_entries))
}

/// Reconstruct a CaptureCriticalRequest from a decoded CBOR value tree.
fn csil_dec_capture_critical_request(
    csil_root: &CsilCborValue,
) -> Result<CaptureCriticalRequest, CsilCborError> {
    cbor_check_record(csil_root)?;
    let items = {
        let csil_field = cbor_require(csil_root, "items")?;
        let csil_decode = |csil_v| cbor_dec_array(csil_v, csil_dec_telemetry_item);
        csil_decode(csil_field)?
    };
    Ok(CaptureCriticalRequest { items })
}

/// Encode a CaptureCriticalRequest to canonical CSIL CBOR bytes.
pub fn encode_capture_critical_request(
    csil_v: &CaptureCriticalRequest,
) -> Result<Vec<u8>, CsilCborError> {
    let csil_tree = csil_enc_capture_critical_request(csil_v)?;
    Ok(cbor_encode(&csil_tree))
}

/// Decode canonical CSIL CBOR bytes into a CaptureCriticalRequest.
pub fn decode_capture_critical_request(
    csil_data: &[u8],
) -> Result<CaptureCriticalRequest, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_capture_critical_request(&csil_root)
}

/// Build the canonical CBOR value tree for a CaptureCriticalResponse.
fn csil_enc_capture_critical_response(
    csil_v: &CaptureCriticalResponse,
) -> Result<CsilCborValue, CsilCborError> {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(4);
    csil_entries.push((cbor_text("durable"), cbor_bool(csil_v.durable)));
    csil_entries.push((cbor_text("accepted"), cbor_uint(csil_v.accepted)));
    if let Some(csil_inner) = &csil_v.batch_id {
        csil_entries.push((cbor_text("batch_id"), cbor_bytes(csil_inner)));
    }
    if let Some(csil_inner) = &csil_v.rejected {
        csil_entries.push((
            cbor_text("rejected"),
            cbor_enc_array(csil_inner, csil_enc_rejected_item)?,
        ));
    }
    Ok(CsilCborValue::Map(csil_entries))
}

/// Reconstruct a CaptureCriticalResponse from a decoded CBOR value tree.
fn csil_dec_capture_critical_response(
    csil_root: &CsilCborValue,
) -> Result<CaptureCriticalResponse, CsilCborError> {
    cbor_check_record(csil_root)?;
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
pub fn encode_capture_critical_response(
    csil_v: &CaptureCriticalResponse,
) -> Result<Vec<u8>, CsilCborError> {
    let csil_tree = csil_enc_capture_critical_response(csil_v)?;
    Ok(cbor_encode(&csil_tree))
}

/// Decode canonical CSIL CBOR bytes into a CaptureCriticalResponse.
pub fn decode_capture_critical_response(
    csil_data: &[u8],
) -> Result<CaptureCriticalResponse, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_capture_critical_response(&csil_root)
}

/// Build the canonical CBOR value tree for a PolicyVersionRequest.
fn csil_enc_policy_version_request(
    _csil_v: &PolicyVersionRequest,
) -> Result<CsilCborValue, CsilCborError> {
    Ok(CsilCborValue::Map(Vec::new()))
}

/// Reconstruct a PolicyVersionRequest from a decoded CBOR value tree.
fn csil_dec_policy_version_request(
    csil_root: &CsilCborValue,
) -> Result<PolicyVersionRequest, CsilCborError> {
    cbor_check_record(csil_root)?;
    Ok(PolicyVersionRequest {})
}

/// Encode a PolicyVersionRequest to canonical CSIL CBOR bytes.
pub fn encode_policy_version_request(
    csil_v: &PolicyVersionRequest,
) -> Result<Vec<u8>, CsilCborError> {
    let csil_tree = csil_enc_policy_version_request(csil_v)?;
    Ok(cbor_encode(&csil_tree))
}

/// Decode canonical CSIL CBOR bytes into a PolicyVersionRequest.
pub fn decode_policy_version_request(
    csil_data: &[u8],
) -> Result<PolicyVersionRequest, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_policy_version_request(&csil_root)
}

/// Build the canonical CBOR value tree for a PolicyVersionResponse.
fn csil_enc_policy_version_response(
    csil_v: &PolicyVersionResponse,
) -> Result<CsilCborValue, CsilCborError> {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(3);
    csil_entries.push((
        cbor_text("enabled_kinds"),
        cbor_enc_array(&csil_v.enabled_kinds, csil_enc_telemetry_kind)?,
    ));
    csil_entries.push((cbor_text("sampling_rate"), cbor_float(csil_v.sampling_rate)));
    csil_entries.push((
        cbor_text("policy_version"),
        cbor_uint(csil_v.policy_version),
    ));
    Ok(CsilCborValue::Map(csil_entries))
}

/// Reconstruct a PolicyVersionResponse from a decoded CBOR value tree.
fn csil_dec_policy_version_response(
    csil_root: &CsilCborValue,
) -> Result<PolicyVersionResponse, CsilCborError> {
    cbor_check_record(csil_root)?;
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
pub fn encode_policy_version_response(
    csil_v: &PolicyVersionResponse,
) -> Result<Vec<u8>, CsilCborError> {
    let csil_tree = csil_enc_policy_version_response(csil_v)?;
    Ok(cbor_encode(&csil_tree))
}

/// Decode canonical CSIL CBOR bytes into a PolicyVersionResponse.
pub fn decode_policy_version_response(
    csil_data: &[u8],
) -> Result<PolicyVersionResponse, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_policy_version_response(&csil_root)
}

/// Build the canonical CBOR value tree for a AlertNotifyRequest.
fn csil_enc_alert_notify_request(
    csil_v: &AlertNotifyRequest,
) -> Result<CsilCborValue, CsilCborError> {
    let mut csil_entries: Vec<(CsilCborValue, CsilCborValue)> = Vec::with_capacity(3);
    csil_entries.push((cbor_text("body"), cbor_bytes(&csil_v.body)));
    csil_entries.push((cbor_text("signature"), cbor_text(&csil_v.signature)));
    csil_entries.push((cbor_text("signed_at"), cbor_int(csil_v.signed_at)));
    Ok(CsilCborValue::Map(csil_entries))
}

/// Reconstruct a AlertNotifyRequest from a decoded CBOR value tree.
fn csil_dec_alert_notify_request(
    csil_root: &CsilCborValue,
) -> Result<AlertNotifyRequest, CsilCborError> {
    cbor_check_record(csil_root)?;
    let signed_at = {
        let csil_field = cbor_require(csil_root, "signed_at")?;
        let csil_decode = cbor_as_i64;
        csil_decode(csil_field)?
    };
    let signature = {
        let csil_field = cbor_require(csil_root, "signature")?;
        let csil_decode = cbor_as_text;
        csil_decode(csil_field)?
    };
    let body = {
        let csil_field = cbor_require(csil_root, "body")?;
        let csil_decode = cbor_as_bytes;
        csil_decode(csil_field)?
    };
    Ok(AlertNotifyRequest {
        signed_at,
        signature,
        body,
    })
}

/// Encode a AlertNotifyRequest to canonical CSIL CBOR bytes.
pub fn encode_alert_notify_request(csil_v: &AlertNotifyRequest) -> Result<Vec<u8>, CsilCborError> {
    let csil_tree = csil_enc_alert_notify_request(csil_v)?;
    Ok(cbor_encode(&csil_tree))
}

/// Decode canonical CSIL CBOR bytes into a AlertNotifyRequest.
pub fn decode_alert_notify_request(csil_data: &[u8]) -> Result<AlertNotifyRequest, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_alert_notify_request(&csil_root)
}

/// Build the canonical CBOR value tree for a AlertNotifyResponse.
fn csil_enc_alert_notify_response(
    _csil_v: &AlertNotifyResponse,
) -> Result<CsilCborValue, CsilCborError> {
    Ok(CsilCborValue::Map(Vec::new()))
}

/// Reconstruct a AlertNotifyResponse from a decoded CBOR value tree.
fn csil_dec_alert_notify_response(
    csil_root: &CsilCborValue,
) -> Result<AlertNotifyResponse, CsilCborError> {
    cbor_check_record(csil_root)?;
    Ok(AlertNotifyResponse {})
}

/// Encode a AlertNotifyResponse to canonical CSIL CBOR bytes.
pub fn encode_alert_notify_response(
    csil_v: &AlertNotifyResponse,
) -> Result<Vec<u8>, CsilCborError> {
    let csil_tree = csil_enc_alert_notify_response(csil_v)?;
    Ok(cbor_encode(&csil_tree))
}

/// Decode canonical CSIL CBOR bytes into a AlertNotifyResponse.
pub fn decode_alert_notify_response(
    csil_data: &[u8],
) -> Result<AlertNotifyResponse, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_alert_notify_response(&csil_root)
}

/// Encode a Compression enum as its CBOR value.
fn csil_enc_compression(csil_v: &Compression) -> Result<CsilCborValue, CsilCborError> {
    Ok(match csil_v {
        Compression::None => cbor_text("none"),
        Compression::Zstd => cbor_text("zstd"),
    })
}

/// Decode a bare literal value into a Compression enum.
fn csil_dec_compression(csil_v: &CsilCborValue) -> Result<Compression, CsilCborError> {
    let csil_val = cbor_as_text(csil_v)?;
    match csil_val.as_str() {
        "none" => Ok(Compression::None),
        "zstd" => Ok(Compression::Zstd),
        csil_other => Err(CsilCborError(format!(
            "csil cbor: unknown Compression value {csil_other:?}"
        ))),
    }
}

/// Encode a Compression to canonical CSIL CBOR bytes.
pub fn encode_compression(csil_v: &Compression) -> Result<Vec<u8>, CsilCborError> {
    let csil_tree = csil_enc_compression(csil_v)?;
    Ok(cbor_encode(&csil_tree))
}

/// Decode canonical CSIL CBOR bytes into a Compression.
pub fn decode_compression(csil_data: &[u8]) -> Result<Compression, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_compression(&csil_root)
}

/// Encode a ReceiptPolicy enum as its CBOR value.
fn csil_enc_receipt_policy(csil_v: &ReceiptPolicy) -> Result<CsilCborValue, CsilCborError> {
    Ok(match csil_v {
        ReceiptPolicy::LocalOne => cbor_text("local-one"),
        ReceiptPolicy::LocalQuorum => cbor_text("local-quorum"),
        ReceiptPolicy::RemoteOne => cbor_text("remote-one"),
        ReceiptPolicy::Custom => cbor_text("custom"),
    })
}

/// Decode a bare literal value into a ReceiptPolicy enum.
fn csil_dec_receipt_policy(csil_v: &CsilCborValue) -> Result<ReceiptPolicy, CsilCborError> {
    let csil_val = cbor_as_text(csil_v)?;
    match csil_val.as_str() {
        "local-one" => Ok(ReceiptPolicy::LocalOne),
        "local-quorum" => Ok(ReceiptPolicy::LocalQuorum),
        "remote-one" => Ok(ReceiptPolicy::RemoteOne),
        "custom" => Ok(ReceiptPolicy::Custom),
        csil_other => Err(CsilCborError(format!(
            "csil cbor: unknown ReceiptPolicy value {csil_other:?}"
        ))),
    }
}

/// Encode a ReceiptPolicy to canonical CSIL CBOR bytes.
pub fn encode_receipt_policy(csil_v: &ReceiptPolicy) -> Result<Vec<u8>, CsilCborError> {
    let csil_tree = csil_enc_receipt_policy(csil_v)?;
    Ok(cbor_encode(&csil_tree))
}

/// Decode canonical CSIL CBOR bytes into a ReceiptPolicy.
pub fn decode_receipt_policy(csil_data: &[u8]) -> Result<ReceiptPolicy, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_receipt_policy(&csil_root)
}

/// Encode a RetentionClass enum as its CBOR value.
fn csil_enc_retention_class(csil_v: &RetentionClass) -> Result<CsilCborValue, CsilCborError> {
    Ok(match csil_v {
        RetentionClass::Provisional => cbor_text("provisional"),
        RetentionClass::Raw => cbor_text("raw"),
        RetentionClass::Detailed => cbor_text("detailed"),
        RetentionClass::Rollup => cbor_text("rollup"),
        RetentionClass::Audit => cbor_text("audit"),
    })
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

/// Encode a RetentionClass to canonical CSIL CBOR bytes.
pub fn encode_retention_class(csil_v: &RetentionClass) -> Result<Vec<u8>, CsilCborError> {
    let csil_tree = csil_enc_retention_class(csil_v)?;
    Ok(cbor_encode(&csil_tree))
}

/// Decode canonical CSIL CBOR bytes into a RetentionClass.
pub fn decode_retention_class(csil_data: &[u8]) -> Result<RetentionClass, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_retention_class(&csil_root)
}

/// Encode a CampaignLinking enum as its CBOR value.
fn csil_enc_campaign_linking(csil_v: &CampaignLinking) -> Result<CsilCborValue, CsilCborError> {
    Ok(match csil_v {
        CampaignLinking::Linked => cbor_text("linked"),
        CampaignLinking::Unlinked => cbor_text("unlinked"),
        CampaignLinking::None => cbor_text("none"),
    })
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

/// Encode a CampaignLinking to canonical CSIL CBOR bytes.
pub fn encode_campaign_linking(csil_v: &CampaignLinking) -> Result<Vec<u8>, CsilCborError> {
    let csil_tree = csil_enc_campaign_linking(csil_v)?;
    Ok(cbor_encode(&csil_tree))
}

/// Decode canonical CSIL CBOR bytes into a CampaignLinking.
pub fn decode_campaign_linking(csil_data: &[u8]) -> Result<CampaignLinking, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_campaign_linking(&csil_root)
}

/// Encode a TypedValueKind enum as its CBOR value.
fn csil_enc_typed_value_kind(csil_v: &TypedValueKind) -> Result<CsilCborValue, CsilCborError> {
    Ok(match csil_v {
        TypedValueKind::Null => cbor_text("null"),
        TypedValueKind::Bool => cbor_text("bool"),
        TypedValueKind::Int => cbor_text("int"),
        TypedValueKind::Uint => cbor_text("uint"),
        TypedValueKind::Float => cbor_text("float"),
        TypedValueKind::Decimal => cbor_text("decimal"),
        TypedValueKind::Text => cbor_text("text"),
        TypedValueKind::Bytes => cbor_text("bytes"),
    })
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

/// Encode a TypedValueKind to canonical CSIL CBOR bytes.
pub fn encode_typed_value_kind(csil_v: &TypedValueKind) -> Result<Vec<u8>, CsilCborError> {
    let csil_tree = csil_enc_typed_value_kind(csil_v)?;
    Ok(cbor_encode(&csil_tree))
}

/// Decode canonical CSIL CBOR bytes into a TypedValueKind.
pub fn decode_typed_value_kind(csil_data: &[u8]) -> Result<TypedValueKind, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_typed_value_kind(&csil_root)
}

/// Encode a PropertyOrigin enum as its CBOR value.
fn csil_enc_property_origin(csil_v: &PropertyOrigin) -> Result<CsilCborValue, CsilCborError> {
    Ok(match csil_v {
        PropertyOrigin::Client => cbor_text("client"),
        PropertyOrigin::Driver => cbor_text("driver"),
        PropertyOrigin::Collector => cbor_text("collector"),
    })
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

/// Encode a PropertyOrigin to canonical CSIL CBOR bytes.
pub fn encode_property_origin(csil_v: &PropertyOrigin) -> Result<Vec<u8>, CsilCborError> {
    let csil_tree = csil_enc_property_origin(csil_v)?;
    Ok(cbor_encode(&csil_tree))
}

/// Decode canonical CSIL CBOR bytes into a PropertyOrigin.
pub fn decode_property_origin(csil_data: &[u8]) -> Result<PropertyOrigin, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_property_origin(&csil_root)
}

/// Encode a MeasurementKind enum as its CBOR value.
fn csil_enc_measurement_kind(csil_v: &MeasurementKind) -> Result<CsilCborValue, CsilCborError> {
    Ok(match csil_v {
        MeasurementKind::Float => cbor_text("float"),
        MeasurementKind::Decimal => cbor_text("decimal"),
        MeasurementKind::Int => cbor_text("int"),
    })
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

/// Encode a MeasurementKind to canonical CSIL CBOR bytes.
pub fn encode_measurement_kind(csil_v: &MeasurementKind) -> Result<Vec<u8>, CsilCborError> {
    let csil_tree = csil_enc_measurement_kind(csil_v)?;
    Ok(cbor_encode(&csil_tree))
}

/// Decode canonical CSIL CBOR bytes into a MeasurementKind.
pub fn decode_measurement_kind(csil_data: &[u8]) -> Result<MeasurementKind, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_measurement_kind(&csil_root)
}

/// Encode a ConsentState enum as its CBOR value.
fn csil_enc_consent_state(csil_v: &ConsentState) -> Result<CsilCborValue, CsilCborError> {
    Ok(match csil_v {
        ConsentState::Granted => cbor_text("granted"),
        ConsentState::Denied => cbor_text("denied"),
        ConsentState::Absent => cbor_text("absent"),
    })
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

/// Encode a ConsentState to canonical CSIL CBOR bytes.
pub fn encode_consent_state(csil_v: &ConsentState) -> Result<Vec<u8>, CsilCborError> {
    let csil_tree = csil_enc_consent_state(csil_v)?;
    Ok(cbor_encode(&csil_tree))
}

/// Decode canonical CSIL CBOR bytes into a ConsentState.
pub fn decode_consent_state(csil_data: &[u8]) -> Result<ConsentState, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_consent_state(&csil_root)
}

/// Encode a TelemetryKind enum as its CBOR value.
fn csil_enc_telemetry_kind(csil_v: &TelemetryKind) -> Result<CsilCborValue, CsilCborError> {
    Ok(match csil_v {
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
    })
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

/// Encode a TelemetryKind to canonical CSIL CBOR bytes.
pub fn encode_telemetry_kind(csil_v: &TelemetryKind) -> Result<Vec<u8>, CsilCborError> {
    let csil_tree = csil_enc_telemetry_kind(csil_v)?;
    Ok(cbor_encode(&csil_tree))
}

/// Decode canonical CSIL CBOR bytes into a TelemetryKind.
pub fn decode_telemetry_kind(csil_data: &[u8]) -> Result<TelemetryKind, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_telemetry_kind(&csil_root)
}

/// Encode a ErrorCode enum as its CBOR value.
fn csil_enc_error_code(csil_v: &ErrorCode) -> Result<CsilCborValue, CsilCborError> {
    Ok(match csil_v {
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
    })
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

/// Encode a ErrorCode to canonical CSIL CBOR bytes.
pub fn encode_error_code(csil_v: &ErrorCode) -> Result<Vec<u8>, CsilCborError> {
    let csil_tree = csil_enc_error_code(csil_v)?;
    Ok(cbor_encode(&csil_tree))
}

/// Decode canonical CSIL CBOR bytes into a ErrorCode.
pub fn decode_error_code(csil_data: &[u8]) -> Result<ErrorCode, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_error_code(&csil_root)
}

/// Encode a SpanKind enum as its CBOR value.
fn csil_enc_span_kind(csil_v: &SpanKind) -> Result<CsilCborValue, CsilCborError> {
    Ok(match csil_v {
        SpanKind::Internal => cbor_text("internal"),
        SpanKind::Server => cbor_text("server"),
        SpanKind::Client => cbor_text("client"),
        SpanKind::Producer => cbor_text("producer"),
        SpanKind::Consumer => cbor_text("consumer"),
    })
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

/// Encode a SpanKind to canonical CSIL CBOR bytes.
pub fn encode_span_kind(csil_v: &SpanKind) -> Result<Vec<u8>, CsilCborError> {
    let csil_tree = csil_enc_span_kind(csil_v)?;
    Ok(cbor_encode(&csil_tree))
}

/// Decode canonical CSIL CBOR bytes into a SpanKind.
pub fn decode_span_kind(csil_data: &[u8]) -> Result<SpanKind, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_span_kind(&csil_root)
}

/// Encode a MetricKind enum as its CBOR value.
fn csil_enc_metric_kind(csil_v: &MetricKind) -> Result<CsilCborValue, CsilCborError> {
    Ok(match csil_v {
        MetricKind::Counter => cbor_text("counter"),
        MetricKind::Gauge => cbor_text("gauge"),
        MetricKind::Histogram => cbor_text("histogram"),
    })
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

/// Encode a MetricKind to canonical CSIL CBOR bytes.
pub fn encode_metric_kind(csil_v: &MetricKind) -> Result<Vec<u8>, CsilCborError> {
    let csil_tree = csil_enc_metric_kind(csil_v)?;
    Ok(cbor_encode(&csil_tree))
}

/// Decode canonical CSIL CBOR bytes into a MetricKind.
pub fn decode_metric_kind(csil_data: &[u8]) -> Result<MetricKind, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_metric_kind(&csil_root)
}

/// Encode a SamplingClause_mode enum as its CBOR value.
fn csil_enc_sampling_clause_mode(
    csil_v: &SamplingClause_mode,
) -> Result<CsilCborValue, CsilCborError> {
    Ok(match csil_v {
        SamplingClause_mode::KeepAll => cbor_text("keep-all"),
        SamplingClause_mode::KeepPercent => cbor_text("keep-percent"),
        SamplingClause_mode::KeepFirstN => cbor_text("keep-first-n"),
    })
}

/// Decode a bare literal value into a SamplingClause_mode enum.
fn csil_dec_sampling_clause_mode(
    csil_v: &CsilCborValue,
) -> Result<SamplingClause_mode, CsilCborError> {
    let csil_val = cbor_as_text(csil_v)?;
    match csil_val.as_str() {
        "keep-all" => Ok(SamplingClause_mode::KeepAll),
        "keep-percent" => Ok(SamplingClause_mode::KeepPercent),
        "keep-first-n" => Ok(SamplingClause_mode::KeepFirstN),
        csil_other => Err(CsilCborError(format!(
            "csil cbor: unknown SamplingClause_mode value {csil_other:?}"
        ))),
    }
}

/// Encode a SamplingClause_mode to canonical CSIL CBOR bytes.
pub fn encode_sampling_clause_mode(csil_v: &SamplingClause_mode) -> Result<Vec<u8>, CsilCborError> {
    let csil_tree = csil_enc_sampling_clause_mode(csil_v)?;
    Ok(cbor_encode(&csil_tree))
}

/// Decode canonical CSIL CBOR bytes into a SamplingClause_mode.
pub fn decode_sampling_clause_mode(csil_data: &[u8]) -> Result<SamplingClause_mode, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_sampling_clause_mode(&csil_root)
}

/// Encode a CollectorHealth_role enum as its CBOR value.
fn csil_enc_collector_health_role(
    csil_v: &CollectorHealth_role,
) -> Result<CsilCborValue, CsilCborError> {
    Ok(match csil_v {
        CollectorHealth_role::Intake => cbor_text("intake"),
        CollectorHealth_role::Forwarder => cbor_text("forwarder"),
        CollectorHealth_role::CompatibilityReceiver => cbor_text("compatibility-receiver"),
    })
}

/// Decode a bare literal value into a CollectorHealth_role enum.
fn csil_dec_collector_health_role(
    csil_v: &CsilCborValue,
) -> Result<CollectorHealth_role, CsilCborError> {
    let csil_val = cbor_as_text(csil_v)?;
    match csil_val.as_str() {
        "intake" => Ok(CollectorHealth_role::Intake),
        "forwarder" => Ok(CollectorHealth_role::Forwarder),
        "compatibility-receiver" => Ok(CollectorHealth_role::CompatibilityReceiver),
        csil_other => Err(CsilCborError(format!(
            "csil cbor: unknown CollectorHealth_role value {csil_other:?}"
        ))),
    }
}

/// Encode a CollectorHealth_role to canonical CSIL CBOR bytes.
pub fn encode_collector_health_role(
    csil_v: &CollectorHealth_role,
) -> Result<Vec<u8>, CsilCborError> {
    let csil_tree = csil_enc_collector_health_role(csil_v)?;
    Ok(cbor_encode(&csil_tree))
}

/// Decode canonical CSIL CBOR bytes into a CollectorHealth_role.
pub fn decode_collector_health_role(
    csil_data: &[u8],
) -> Result<CollectorHealth_role, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_collector_health_role(&csil_root)
}

/// Encode a SessionEndPayload_reason enum as its CBOR value.
fn csil_enc_session_end_payload_reason(
    csil_v: &SessionEndPayload_reason,
) -> Result<CsilCborValue, CsilCborError> {
    Ok(match csil_v {
        SessionEndPayload_reason::Explicit => cbor_text("explicit"),
        SessionEndPayload_reason::Timeout => cbor_text("timeout"),
        SessionEndPayload_reason::MaximumLifetime => cbor_text("maximum-lifetime"),
    })
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

/// Encode a SessionEndPayload_reason to canonical CSIL CBOR bytes.
pub fn encode_session_end_payload_reason(
    csil_v: &SessionEndPayload_reason,
) -> Result<Vec<u8>, CsilCborError> {
    let csil_tree = csil_enc_session_end_payload_reason(csil_v)?;
    Ok(cbor_encode(&csil_tree))
}

/// Decode canonical CSIL CBOR bytes into a SessionEndPayload_reason.
pub fn decode_session_end_payload_reason(
    csil_data: &[u8],
) -> Result<SessionEndPayload_reason, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_session_end_payload_reason(&csil_root)
}

/// Encode a ErrorPayload_severity enum as its CBOR value.
fn csil_enc_error_payload_severity(
    csil_v: &ErrorPayload_severity,
) -> Result<CsilCborValue, CsilCborError> {
    Ok(match csil_v {
        ErrorPayload_severity::Fatal => cbor_text("fatal"),
        ErrorPayload_severity::Error => cbor_text("error"),
        ErrorPayload_severity::Warning => cbor_text("warning"),
        ErrorPayload_severity::Info => cbor_text("info"),
    })
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

/// Encode a ErrorPayload_severity to canonical CSIL CBOR bytes.
pub fn encode_error_payload_severity(
    csil_v: &ErrorPayload_severity,
) -> Result<Vec<u8>, CsilCborError> {
    let csil_tree = csil_enc_error_payload_severity(csil_v)?;
    Ok(cbor_encode(&csil_tree))
}

/// Decode canonical CSIL CBOR bytes into a ErrorPayload_severity.
pub fn decode_error_payload_severity(
    csil_data: &[u8],
) -> Result<ErrorPayload_severity, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_error_payload_severity(&csil_root)
}

/// Encode a SpanPayload_status enum as its CBOR value.
fn csil_enc_span_payload_status(
    csil_v: &SpanPayload_status,
) -> Result<CsilCborValue, CsilCborError> {
    Ok(match csil_v {
        SpanPayload_status::Ok => cbor_text("ok"),
        SpanPayload_status::Error => cbor_text("error"),
        SpanPayload_status::Unset => cbor_text("unset"),
    })
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

/// Encode a SpanPayload_status to canonical CSIL CBOR bytes.
pub fn encode_span_payload_status(csil_v: &SpanPayload_status) -> Result<Vec<u8>, CsilCborError> {
    let csil_tree = csil_enc_span_payload_status(csil_v)?;
    Ok(cbor_encode(&csil_tree))
}

/// Decode canonical CSIL CBOR bytes into a SpanPayload_status.
pub fn decode_span_payload_status(csil_data: &[u8]) -> Result<SpanPayload_status, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_span_payload_status(&csil_root)
}

/// Encode a MetricPointPayload_temporality enum as its CBOR value.
fn csil_enc_metric_point_payload_temporality(
    csil_v: &MetricPointPayload_temporality,
) -> Result<CsilCborValue, CsilCborError> {
    Ok(match csil_v {
        MetricPointPayload_temporality::Delta => cbor_text("delta"),
        MetricPointPayload_temporality::Cumulative => cbor_text("cumulative"),
    })
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

/// Encode a MetricPointPayload_temporality to canonical CSIL CBOR bytes.
pub fn encode_metric_point_payload_temporality(
    csil_v: &MetricPointPayload_temporality,
) -> Result<Vec<u8>, CsilCborError> {
    let csil_tree = csil_enc_metric_point_payload_temporality(csil_v)?;
    Ok(cbor_encode(&csil_tree))
}

/// Decode canonical CSIL CBOR bytes into a MetricPointPayload_temporality.
pub fn decode_metric_point_payload_temporality(
    csil_data: &[u8],
) -> Result<MetricPointPayload_temporality, CsilCborError> {
    let csil_root = cbor_decode(csil_data)?;
    csil_dec_metric_point_payload_temporality(&csil_root)
}

pub fn encode_tally_owl_collector_submit_batch_reply(
    reply: Result<SubmitBatchResponse, ServiceError>,
) -> Result<(&'static str, Vec<u8>), CsilCborError> {
    Ok(match reply {
        Ok(value) => ("SubmitBatchResponse", encode_submit_batch_response(&value)?),
        Err(value) => ("ServiceError", encode_service_error(&value)?),
    })
}

pub fn encode_tally_owl_collector_commit_batch_reply(
    reply: Result<CommitBatchResponse, ServiceError>,
) -> Result<(&'static str, Vec<u8>), CsilCborError> {
    Ok(match reply {
        Ok(value) => ("CommitBatchResponse", encode_commit_batch_response(&value)?),
        Err(value) => ("ServiceError", encode_service_error(&value)?),
    })
}

pub fn encode_tally_owl_collector_fetch_policy_reply(
    reply: Result<FetchPolicyResponse, ServiceError>,
) -> Result<(&'static str, Vec<u8>), CsilCborError> {
    Ok(match reply {
        Ok(value) => ("FetchPolicyResponse", encode_fetch_policy_response(&value)?),
        Err(value) => ("ServiceError", encode_service_error(&value)?),
    })
}

pub fn encode_tally_owl_collector_health_reply(
    reply: Result<CollectorHealth, ServiceError>,
) -> Result<(&'static str, Vec<u8>), CsilCborError> {
    Ok(match reply {
        Ok(value) => ("CollectorHealth", encode_collector_health(&value)?),
        Err(value) => ("ServiceError", encode_service_error(&value)?),
    })
}

pub fn encode_tally_owl_collector_resolve_key_reply(
    reply: Result<ResolveKeyResponse, ServiceError>,
) -> Result<(&'static str, Vec<u8>), CsilCborError> {
    Ok(match reply {
        Ok(value) => ("ResolveKeyResponse", encode_resolve_key_response(&value)?),
        Err(value) => ("ServiceError", encode_service_error(&value)?),
    })
}

pub fn encode_tally_owl_ingest_capture_reply(
    reply: Result<CaptureResponse, ServiceError>,
) -> Result<(&'static str, Vec<u8>), CsilCborError> {
    Ok(match reply {
        Ok(value) => ("CaptureResponse", encode_capture_response(&value)?),
        Err(value) => ("ServiceError", encode_service_error(&value)?),
    })
}

pub fn encode_tally_owl_ingest_capture_critical_reply(
    reply: Result<CaptureCriticalResponse, ServiceError>,
) -> Result<(&'static str, Vec<u8>), CsilCborError> {
    Ok(match reply {
        Ok(value) => (
            "CaptureCriticalResponse",
            encode_capture_critical_response(&value)?,
        ),
        Err(value) => ("ServiceError", encode_service_error(&value)?),
    })
}

pub fn encode_tally_owl_ingest_policy_version_reply(
    reply: Result<PolicyVersionResponse, ServiceError>,
) -> Result<(&'static str, Vec<u8>), CsilCborError> {
    Ok(match reply {
        Ok(value) => (
            "PolicyVersionResponse",
            encode_policy_version_response(&value)?,
        ),
        Err(value) => ("ServiceError", encode_service_error(&value)?),
    })
}

pub fn encode_tally_owl_alert_receiver_notify_reply(
    reply: Result<AlertNotifyResponse, ServiceError>,
) -> Result<(&'static str, Vec<u8>), CsilCborError> {
    Ok(match reply {
        Ok(value) => ("AlertNotifyResponse", encode_alert_notify_response(&value)?),
        Err(value) => ("ServiceError", encode_service_error(&value)?),
    })
}
