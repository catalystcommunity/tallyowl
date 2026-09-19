//! Typed configuration values, and the text forms a person writes.
//!
//! A configuration file, an environment variable, and a command-line flag all
//! carry text. This module turns that text into a typed value and refuses a
//! value that does not fit, so a service never starts on a setting it will
//! misread later.

use std::fmt;

/// What a setting holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    Text,
    Integer,
    Boolean,
    /// One of a fixed set of words. The set is part of the error message,
    /// because a person who typed the wrong word wants to see the right ones.
    Enum(&'static [&'static str]),
    /// A span of time, written as `500ms`, `30s`, `5m`, `2h`, or `7d`.
    Duration,
    /// A count of bytes, written as `512KiB`, `16MiB`, or `1GiB`.
    Bytes,
    /// A list of words, written as a YAML sequence or a comma-separated text.
    TextList,
    /// A reference to a secret, never a secret itself. See `secret.rs`.
    Secret,
}

impl Kind {
    pub fn name(&self) -> &'static str {
        match self {
            Kind::Text => "text",
            Kind::Integer => "whole number",
            Kind::Boolean => "true or false",
            Kind::Enum(_) => "one of a fixed set of words",
            Kind::Duration => "a span of time",
            Kind::Bytes => "a count of bytes",
            Kind::TextList => "a list of words",
            Kind::Secret => "a secret reference",
        }
    }
}

/// A resolved value.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Text(String),
    Integer(i64),
    Boolean(bool),
    /// Milliseconds. A duration is stored in the unit CONVENTIONS.md section 7
    /// requires, so nothing downstream has to parse `7d` again.
    DurationMs(i64),
    Bytes(i64),
    TextList(Vec<String>),
    /// The reference, never the value. Resolution happens at startup and the
    /// result never enters this type.
    SecretRef(String),
}

impl Value {
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Value::Text(v) | Value::SecretRef(v) => Some(v),
            _ => None,
        }
    }

    pub fn as_integer(&self) -> Option<i64> {
        match self {
            Value::Integer(v) | Value::DurationMs(v) | Value::Bytes(v) => Some(*v),
            _ => None,
        }
    }

    pub fn as_boolean(&self) -> Option<bool> {
        match self {
            Value::Boolean(v) => Some(*v),
            _ => None,
        }
    }

    pub fn as_list(&self) -> Option<&[String]> {
        match self {
            Value::TextList(v) => Some(v),
            _ => None,
        }
    }

    /// The form that goes back into a configuration file or a report.
    pub fn to_display(&self) -> String {
        match self {
            Value::Text(v) => v.clone(),
            Value::Integer(v) => v.to_string(),
            Value::Boolean(v) => v.to_string(),
            Value::DurationMs(v) => format_duration(*v),
            Value::Bytes(v) => format_bytes(*v),
            Value::TextList(v) => v.join(","),
            // A reference is safe to print. The resolved value never is, and it
            // is never held here. See `secret.rs`.
            Value::SecretRef(v) => v.clone(),
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_display())
    }
}

/// Why a text did not fit its kind. The caller adds the setting name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseFailure {
    pub reason: String,
}

fn fail(reason: impl Into<String>) -> ParseFailure {
    ParseFailure {
        reason: reason.into(),
    }
}

pub fn parse(kind: &Kind, text: &str) -> Result<Value, ParseFailure> {
    let trimmed = text.trim();
    match kind {
        Kind::Text => Ok(Value::Text(trimmed.to_string())),
        Kind::Secret => Ok(Value::SecretRef(trimmed.to_string())),
        Kind::Integer => trimmed
            .parse::<i64>()
            .map(Value::Integer)
            .map_err(|_| fail("it is not a whole number")),
        Kind::Boolean => match trimmed.to_lowercase().as_str() {
            "true" | "yes" | "on" | "1" => Ok(Value::Boolean(true)),
            "false" | "no" | "off" | "0" => Ok(Value::Boolean(false)),
            _ => Err(fail("it is not true or false")),
        },
        Kind::Enum(permitted) => {
            if permitted.contains(&trimmed) {
                Ok(Value::Text(trimmed.to_string()))
            } else {
                Err(fail(format!("it is not one of {}", permitted.join(", "))))
            }
        }
        Kind::Duration => parse_duration(trimmed).map(Value::DurationMs),
        Kind::Bytes => parse_bytes(trimmed).map(Value::Bytes),
        Kind::TextList => {
            if trimmed.is_empty() {
                return Ok(Value::TextList(Vec::new()));
            }
            Ok(Value::TextList(
                trimmed
                    .split(',')
                    .map(|p| p.trim().to_string())
                    .filter(|p| !p.is_empty())
                    .collect(),
            ))
        }
    }
}

/// `500ms`, `30s`, `5m`, `2h`, `7d`. A bare number is refused, because a reader
/// cannot tell whether `5` means seconds or days.
pub fn parse_duration(text: &str) -> Result<i64, ParseFailure> {
    let text = text.trim();
    let (digits, unit) = split_unit(text);
    if digits.is_empty() {
        return Err(fail("it has no number"));
    }
    let amount: i64 = digits
        .parse()
        .map_err(|_| fail("its number is too large or is not whole"))?;
    let multiplier = match unit {
        "ms" => 1,
        "s" => 1_000,
        "m" => 60_000,
        "h" => 3_600_000,
        "d" => 86_400_000,
        "" => return Err(fail("it has no unit. Use ms, s, m, h, or d")),
        other => {
            return Err(fail(format!(
                "`{other}` is not a unit of time. Use ms, s, m, h, or d"
            )))
        }
    };
    amount
        .checked_mul(multiplier)
        .ok_or_else(|| fail("it is too large to hold in milliseconds"))
}

/// `512KiB`, `16MiB`, `1GiB`, or a bare byte count. Binary units only, because a
/// storage limit that means 1,000,000 when a person wrote 1 MB causes an
/// argument nobody wants to have twice.
pub fn parse_bytes(text: &str) -> Result<i64, ParseFailure> {
    let text = text.trim();
    let (digits, unit) = split_unit(text);
    if digits.is_empty() {
        return Err(fail("it has no number"));
    }
    let amount: i64 = digits
        .parse()
        .map_err(|_| fail("its number is too large or is not whole"))?;
    let multiplier: i64 = match unit.to_lowercase().as_str() {
        "" | "b" => 1,
        "kib" => 1024,
        "mib" => 1024 * 1024,
        "gib" => 1024 * 1024 * 1024,
        "tib" => 1024_i64 * 1024 * 1024 * 1024,
        other => {
            return Err(fail(format!(
                "`{other}` is not a unit of bytes. Use B, KiB, MiB, GiB, or TiB"
            )))
        }
    };
    amount
        .checked_mul(multiplier)
        .ok_or_else(|| fail("it is too large to hold in bytes"))
}

fn split_unit(text: &str) -> (&str, &str) {
    let boundary = text
        .find(|c: char| !c.is_ascii_digit() && c != '-')
        .unwrap_or(text.len());
    (&text[..boundary], text[boundary..].trim())
}

pub fn format_duration(ms: i64) -> String {
    for (unit, size) in [
        ("d", 86_400_000_i64),
        ("h", 3_600_000),
        ("m", 60_000),
        ("s", 1_000),
    ] {
        if ms != 0 && ms % size == 0 {
            return format!("{}{unit}", ms / size);
        }
    }
    format!("{ms}ms")
}

pub fn format_bytes(bytes: i64) -> String {
    for (unit, size) in [
        ("TiB", 1024_i64 * 1024 * 1024 * 1024),
        ("GiB", 1024 * 1024 * 1024),
        ("MiB", 1024 * 1024),
        ("KiB", 1024),
    ] {
        if bytes != 0 && bytes % size == 0 {
            return format!("{}{unit}", bytes / size);
        }
    }
    format!("{bytes}B")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_duration_needs_a_unit() {
        assert_eq!(parse_duration("30s").unwrap(), 30_000);
        assert_eq!(parse_duration("7d").unwrap(), 604_800_000);
        assert_eq!(parse_duration("500ms").unwrap(), 500);
        assert_eq!(parse_duration("2h").unwrap(), 7_200_000);
        assert!(parse_duration("5").is_err(), "a bare number is ambiguous");
        assert!(parse_duration("5 fortnights").is_err());
        assert!(parse_duration("s").is_err());
    }

    #[test]
    fn bytes_use_binary_units() {
        assert_eq!(parse_bytes("512KiB").unwrap(), 524_288);
        assert_eq!(parse_bytes("16MiB").unwrap(), 16_777_216);
        assert_eq!(parse_bytes("1GiB").unwrap(), 1_073_741_824);
        assert_eq!(parse_bytes("1024").unwrap(), 1024);
        assert!(parse_bytes("1MB").is_err(), "a decimal unit is refused");
    }

    #[test]
    fn a_duration_and_a_byte_count_round_trip_through_their_text() {
        for text in ["30s", "7d", "500ms", "2h"] {
            let ms = parse_duration(text).unwrap();
            assert_eq!(format_duration(ms), text);
        }
        for text in ["512KiB", "16MiB", "1GiB"] {
            let bytes = parse_bytes(text).unwrap();
            assert_eq!(format_bytes(bytes), text);
        }
    }

    #[test]
    fn an_enum_refusal_names_the_permitted_words() {
        let kind = Kind::Enum(&["none", "verify-on-read", "scrub"]);
        let failure = parse(&kind, "paranoid").unwrap_err();
        assert!(failure.reason.contains("verify-on-read"));
        assert!(parse(&kind, "scrub").is_ok());
    }

    #[test]
    fn a_boolean_accepts_the_spellings_a_person_writes() {
        for yes in ["true", "TRUE", "yes", "on", "1"] {
            assert_eq!(parse(&Kind::Boolean, yes).unwrap(), Value::Boolean(true));
        }
        for no in ["false", "no", "off", "0"] {
            assert_eq!(parse(&Kind::Boolean, no).unwrap(), Value::Boolean(false));
        }
        assert!(parse(&Kind::Boolean, "maybe").is_err());
    }

    #[test]
    fn a_list_comes_from_a_comma_separated_text() {
        let value = parse(&Kind::TextList, "intake, forwarder ,").unwrap();
        assert_eq!(
            value.as_list().unwrap(),
            &["intake".to_string(), "forwarder".to_string()]
        );
        assert!(parse(&Kind::TextList, "")
            .unwrap()
            .as_list()
            .unwrap()
            .is_empty());
    }

    #[test]
    fn a_number_that_overflows_is_refused_rather_than_wrapped() {
        assert!(parse_bytes("9999999999999TiB").is_err());
        assert!(parse_duration("999999999999999999d").is_err());
    }
}
