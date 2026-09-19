//! The error taxonomy from `docs/CONVENTIONS.md` section 2.
//!
//! Three rules hold this shape together:
//!
//! - a code is a short phrase, not an abbreviation, because a person reads it;
//! - a message says what happened and then what to do, and never carries a
//!   secret, a payload, a stack trace, or personal data;
//! - `retryable` is a fact, not advice. A caller builds automation on it, so a
//!   wrong value produces either a retry storm or lost data.

use std::fmt;

/// The complete set of error codes. `csil/types/common.csil` holds the normative
/// wire type; this enum must stay identical to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorCode {
    /// Something in the request was wrong.
    InvalidArgument,
    /// We do not know who you are.
    Unauthenticated,
    /// We know who you are, and you cannot do this.
    PermissionDenied,
    /// The thing you asked for does not exist.
    NotFound,
    /// You tried to create something that is already there.
    AlreadyExists,
    /// You have used up an allowance.
    ResourceExhausted,
    /// The system is not in a state where this works.
    FailedPrecondition,
    /// We could not reach part of the system.
    Unavailable,
    /// Your software is too old or too new for ours.
    SchemaUnsupported,
    /// The work cost more than the limit allowed.
    BudgetExceeded,
    /// We could not see all of the data.
    IncompleteResult,
    /// We made a mistake.
    Internal,
}

impl ErrorCode {
    /// The wire form, which is also what a person reads.
    pub fn as_str(&self) -> &'static str {
        match self {
            ErrorCode::InvalidArgument => "invalid-argument",
            ErrorCode::Unauthenticated => "unauthenticated",
            ErrorCode::PermissionDenied => "permission-denied",
            ErrorCode::NotFound => "not-found",
            ErrorCode::AlreadyExists => "already-exists",
            ErrorCode::ResourceExhausted => "resource-exhausted",
            ErrorCode::FailedPrecondition => "failed-precondition",
            ErrorCode::Unavailable => "unavailable",
            ErrorCode::SchemaUnsupported => "schema-unsupported",
            ErrorCode::BudgetExceeded => "budget-exceeded",
            ErrorCode::IncompleteResult => "incomplete-result",
            ErrorCode::Internal => "internal",
        }
    }

    pub fn parse(text: &str) -> Option<ErrorCode> {
        Some(match text {
            "invalid-argument" => ErrorCode::InvalidArgument,
            "unauthenticated" => ErrorCode::Unauthenticated,
            "permission-denied" => ErrorCode::PermissionDenied,
            "not-found" => ErrorCode::NotFound,
            "already-exists" => ErrorCode::AlreadyExists,
            "resource-exhausted" => ErrorCode::ResourceExhausted,
            "failed-precondition" => ErrorCode::FailedPrecondition,
            "unavailable" => ErrorCode::Unavailable,
            "schema-unsupported" => ErrorCode::SchemaUnsupported,
            "budget-exceeded" => ErrorCode::BudgetExceeded,
            "incomplete-result" => ErrorCode::IncompleteResult,
            "internal" => ErrorCode::Internal,
            _ => return None,
        })
    }

    /// Whether the same request can succeed later without a change.
    ///
    /// This is the default for the code. A construction site can still override
    /// it, because a code alone does not always decide the fact: an
    /// `invalid-argument` is never retryable, while a `resource-exhausted` is
    /// retryable when an allowance refills and permanent when it cannot.
    pub fn retryable_by_default(&self) -> bool {
        match self {
            ErrorCode::Unavailable | ErrorCode::ResourceExhausted | ErrorCode::Internal => true,
            ErrorCode::InvalidArgument
            | ErrorCode::Unauthenticated
            | ErrorCode::PermissionDenied
            | ErrorCode::NotFound
            | ErrorCode::AlreadyExists
            | ErrorCode::FailedPrecondition
            | ErrorCode::SchemaUnsupported
            | ErrorCode::BudgetExceeded
            | ErrorCode::IncompleteResult => false,
        }
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One error, in the shape that reaches a person and a caller at the same time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TallyOwlError {
    pub code: ErrorCode,
    pub message: String,
    pub retryable: bool,
    /// Safe key and value pairs an engineer reads. Never a secret, a payload, or
    /// personal data.
    pub detail: Vec<(String, String)>,
}

pub type Result<T> = std::result::Result<T, TallyOwlError>;

impl TallyOwlError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            retryable: code.retryable_by_default(),
            detail: Vec::new(),
        }
    }

    /// Override the retry fact. Use this when the code alone does not decide it.
    pub fn retryable(mut self, retryable: bool) -> Self {
        self.retryable = retryable;
        self
    }

    pub fn with_detail(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.detail.push((key.into(), value.into()));
        self
    }

    pub fn invalid_argument(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::InvalidArgument, message)
    }

    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Unavailable, message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Internal, message)
    }

    pub fn resource_exhausted(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::ResourceExhausted, message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::NotFound, message)
    }

    pub fn failed_precondition(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::FailedPrecondition, message)
    }

    /// The message a limit breach must produce: what happened, the limit, the
    /// observed value, and what to do. See CONVENTIONS.md section 2.
    pub fn over_limit(subject: &str, observed: &str, limit: &str, remedy: &str) -> Self {
        Self::new(
            ErrorCode::ResourceExhausted,
            format!("{subject} rejected. It holds {observed} and the limit is {limit}. {remedy}"),
        )
        .retryable(false)
        .with_detail("observed", observed)
        .with_detail("limit", limit)
    }
}

impl fmt::Display for TallyOwlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for TallyOwlError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_code_round_trips_through_its_wire_form() {
        let all = [
            ErrorCode::InvalidArgument,
            ErrorCode::Unauthenticated,
            ErrorCode::PermissionDenied,
            ErrorCode::NotFound,
            ErrorCode::AlreadyExists,
            ErrorCode::ResourceExhausted,
            ErrorCode::FailedPrecondition,
            ErrorCode::Unavailable,
            ErrorCode::SchemaUnsupported,
            ErrorCode::BudgetExceeded,
            ErrorCode::IncompleteResult,
            ErrorCode::Internal,
        ];
        for code in all {
            assert_eq!(ErrorCode::parse(code.as_str()), Some(code));
        }
    }

    #[test]
    fn an_unknown_code_does_not_parse() {
        assert_eq!(ErrorCode::parse("bad-request"), None);
        assert_eq!(ErrorCode::parse(""), None);
    }

    #[test]
    fn a_code_is_a_phrase_and_never_an_abbreviation() {
        // CONVENTIONS.md section 2: a code is a short phrase. Lower case, words
        // separated by a hyphen, no underscore, and no shouting.
        for code in [ErrorCode::InvalidArgument, ErrorCode::IncompleteResult] {
            let text = code.as_str();
            assert_eq!(text, text.to_lowercase());
            assert!(!text.contains('_'));
            assert!(text.contains('-'));
        }
    }

    #[test]
    fn retryable_defaults_follow_the_code() {
        assert!(ErrorCode::Unavailable.retryable_by_default());
        assert!(!ErrorCode::InvalidArgument.retryable_by_default());
        // The default is a starting point, and a site can state the fact itself.
        let permanent = TallyOwlError::unavailable("gone for good").retryable(false);
        assert!(!permanent.retryable);
    }

    #[test]
    fn a_limit_message_names_the_limit_and_the_observed_value() {
        let e = TallyOwlError::over_limit(
            "Batch",
            "640 KiB",
            "512 KiB",
            "Reduce the batch size or raise the limit for this project.",
        );
        assert!(e.message.contains("640 KiB"));
        assert!(e.message.contains("512 KiB"));
        assert!(!e.retryable);
        assert_eq!(e.code, ErrorCode::ResourceExhausted);
    }
}
