//! The scrubber: what never reaches storage by default.
//!
//! `AGENTS.md` is blunt about it. "Never record secrets, credentials, request
//! bodies, claim values, or raw personal data by default." An error message is
//! where those arrive, because an error message is written by whoever wrote the
//! code that threw, and a connection string in an exception is the ordinary
//! case rather than the exotic one.
//!
//! # Where this runs, and why it runs twice
//!
//! The collector is the trust boundary and the place `AGENTS.md` says to
//! normalize. A driver may also scrub, and the reference application does, but
//! a driver's scrubbing is a courtesy: an application that does not use a
//! maintained driver still reaches the collector, so the collector cannot rely
//! on anything having happened before it.
//!
//! # What it replaces, and what it keeps
//!
//! A scrubbed value becomes a marker naming what it was, not an empty string. A
//! person reading `password=<removed>` knows the field was there and was
//! removed. An empty value looks like a defect in the producer.
//!
//! A file path in a stack frame is kept. A path is how somebody finds the line
//! that threw, and a path is not personal data. A **query string** on a path is
//! removed, because that is where a token ends up.
//!
//! # What it cannot do
//!
//! This is a pattern matcher. It catches the shapes that carry a secret and it
//! does not understand the text around them. A project that needs more names
//! removed configures them; `CollectionPolicy.redact_keys` carries that list.

/// What a removed value is replaced with. It names the removal rather than
/// hiding it, so nobody spends an afternoon on a producer that is working.
pub const REMOVED: &str = "<removed>";

/// Names an application cannot set.
///
/// The collector stamps these from its own configuration, refuses a client
/// value, and counts the refusal. It does not accept the value and hide the
/// conflict. See D38.
///
/// It lives here rather than in the collector because two components need the
/// same list: collector intake enforces it, and the head puts it in the policy
/// snapshot it distributes. Two copies of a list like this drift, and the drift
/// would show up as a property that one component refuses and the other keeps.
/// The first four are stamped by the collector. The last four are the row's
/// own correlation columns: a client property borrowing one of these names is
/// shadowed by the column and becomes silently unreachable by filter, which
/// is how the alpha lookups came to measure misses. Refusing at intake makes
/// that visible where an operator can act. L153, and the owner's decision of
/// 2026-08-10.
pub const PROTECTED_KEYS: &[&str] = &[
    "region",
    "env",
    "cell",
    "installation",
    "request_id",
    "session_id",
    "trace_id",
    "event_id",
];

/// Key names whose value never travels. A substring match, so
/// `authorization_header` and `x-api-key` are both caught.
const NEVER_STORED: &[&str] = &[
    "secret",
    "credential",
    "token",
    "password",
    "passphrase",
    "api_key",
    "apikey",
    "api-key",
    "private_key",
    "privatekey",
    "authorization",
    "cookie",
    "session_key",
    "access_key",
    "refresh_token",
    "claim",
];

/// Whether a key's value must not be stored.
pub fn is_protected(key: &str) -> bool {
    let key = key.to_lowercase();
    NEVER_STORED.iter().any(|banned| key.contains(banned))
}

/// Scrub free text, such as an error message or a breadcrumb.
///
/// Four shapes are removed:
///
/// - `key=value` and `key: value` where the key is one of the protected names;
/// - a bearer credential after the word that introduces it;
/// - the password inside a connection string;
/// - a query string on a path or a URL.
pub fn text(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let bytes: Vec<char> = input.chars().collect();
    let mut index = 0;

    while index < bytes.len() {
        // A `key=value` or `key: value` pair. Look back at the word that
        // precedes the separator rather than forward from a keyword, because
        // the key is what says whether the value is a secret.
        if bytes[index] == '=' || bytes[index] == ':' {
            let key_start = word_start(&bytes, index);
            let key: String = bytes[key_start..index].iter().collect();
            if is_protected(key.trim()) {
                out.push(bytes[index]);
                index += 1;
                // Take the separator's whitespace with it, so the marker sits
                // where the value was.
                while index < bytes.len() && bytes[index] == ' ' {
                    out.push(' ');
                    index += 1;
                }
                index = skip_value(&bytes, index);
                out.push_str(REMOVED);
                continue;
            }
        }

        // A query string. Everything after `?` up to whitespace goes, because
        // that is where a token ends up and nothing there helps a person find
        // the line that threw.
        if bytes[index] == '?' && index > 0 && !bytes[index - 1].is_whitespace() {
            let end = bytes[index..]
                .iter()
                .position(|c| c.is_whitespace())
                .map(|offset| index + offset)
                .unwrap_or(bytes.len());
            if end > index + 1 {
                out.push('?');
                out.push_str(REMOVED);
                index = end;
                continue;
            }
        }

        out.push(bytes[index]);
        index += 1;
    }

    scrub_bearer(&scrub_connection_string(&out))
}

/// Where the word before `at` starts.
fn word_start(bytes: &[char], at: usize) -> usize {
    let mut start = at;
    while start > 0 {
        let previous = bytes[start - 1];
        if previous.is_alphanumeric() || previous == '_' || previous == '-' {
            start -= 1;
        } else {
            break;
        }
    }
    start
}

/// Skip the value that follows a separator.
///
/// A quoted value ends at its quote; anything else ends at whitespace, a comma,
/// a semicolon, or a closing bracket. Ending at a comma matters: a message
/// that says `password=x, user=y` must keep the user.
fn skip_value(bytes: &[char], from: usize) -> usize {
    if from >= bytes.len() {
        return from;
    }
    let quote = bytes[from];
    if quote == '"' || quote == '\'' {
        let mut index = from + 1;
        while index < bytes.len() && bytes[index] != quote {
            index += 1;
        }
        return (index + 1).min(bytes.len());
    }
    let mut index = from;
    while index < bytes.len() {
        let c = bytes[index];
        if c.is_whitespace() || c == ',' || c == ';' || c == ')' || c == ']' || c == '}' {
            break;
        }
        index += 1;
    }
    index
}

/// `Bearer <credential>` and `Basic <credential>`.
fn scrub_bearer(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    loop {
        let found = ["Bearer ", "bearer ", "Basic ", "basic "]
            .iter()
            .filter_map(|word| rest.find(word).map(|at| (at, word.len())))
            .min_by_key(|(at, _)| *at);
        let Some((at, word_len)) = found else {
            out.push_str(rest);
            return out;
        };
        let value_start = at + word_len;
        out.push_str(&rest[..value_start]);
        let value_end = rest[value_start..]
            .find(char::is_whitespace)
            .map(|offset| value_start + offset)
            .unwrap_or(rest.len());
        if value_end > value_start {
            out.push_str(REMOVED);
        }
        rest = &rest[value_end..];
    }
}

/// The password inside `scheme://user:password@host`.
///
/// A connection string in an exception is the ordinary case, not the exotic
/// one, and the host is the part that helps somebody diagnose.
fn scrub_connection_string(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(at) = rest.find("://") {
        let after = at + 3;
        let authority_end = rest[after..]
            .find(|c: char| c.is_whitespace() || c == '/')
            .map(|offset| after + offset)
            .unwrap_or(rest.len());
        let authority = &rest[after..authority_end];
        match authority
            .find('@')
            .and_then(|host_at| authority[..host_at].find(':').map(|colon| (colon, host_at)))
        {
            Some((colon, host_at)) => {
                out.push_str(&rest[..after + colon + 1]);
                out.push_str(REMOVED);
                out.push_str(&authority[host_at..]);
                rest = &rest[authority_end..];
            }
            None => {
                out.push_str(&rest[..authority_end]);
                rest = &rest[authority_end..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Scrub a file path from a stack frame.
///
/// The path stays: it is how somebody finds the line that threw, and it is not
/// personal data. A query string on it does not.
pub fn path(input: &str) -> String {
    match input.split_once('?') {
        Some((before, _)) => format!("{before}?{REMOVED}"),
        None => input.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_protected_key_loses_its_value_and_keeps_its_name() {
        // Naming the removal is the point. An empty value looks like a defect
        // in the producer and costs somebody an afternoon.
        assert_eq!(
            text("connect failed: password=hunter2"),
            "connect failed: password=<removed>"
        );
        assert_eq!(text("api_key: sk-live-abc123"), "api_key: <removed>");
    }

    #[test]
    fn a_value_ends_where_the_next_field_starts() {
        // `password=x, user=y` must keep the user. A scrubber that ran to the
        // end of the line would remove the one piece that helps.
        assert_eq!(
            text("password=hunter2, user=alice"),
            "password=<removed>, user=alice"
        );
        assert_eq!(
            text("{token: \"abc def\", route: /pricing}"),
            "{token: <removed>, route: /pricing}"
        );
    }

    #[test]
    fn an_ordinary_key_that_contains_a_protected_word_as_a_word_is_caught() {
        assert!(is_protected("x-api-key"));
        assert!(is_protected("Authorization"));
        assert!(is_protected("refresh_token"));
        assert!(!is_protected("route"));
        assert!(!is_protected("release"));
        assert!(!is_protected("status"));
    }

    #[test]
    fn a_bearer_credential_goes_and_the_word_stays() {
        assert_eq!(
            text("401 for Bearer eyJhbGciOi.abc.def on /orders"),
            "401 for Bearer <removed> on /orders"
        );
    }

    #[test]
    fn a_connection_string_loses_its_password_and_keeps_its_host() {
        assert_eq!(
            text("dial postgres://app:s3cret@db.internal:5432/orders failed"),
            "dial postgres://app:<removed>@db.internal:5432/orders failed"
        );
    }

    #[test]
    fn a_connection_string_with_no_password_is_left_alone() {
        assert_eq!(
            text("GET https://api.example.com/orders timed out"),
            "GET https://api.example.com/orders timed out"
        );
    }

    #[test]
    fn a_query_string_goes_and_the_route_stays() {
        assert_eq!(
            text("GET /orders?token=abc failed"),
            "GET /orders?<removed> failed"
        );
        assert_eq!(path("/app/handlers/orders.rs"), "/app/handlers/orders.rs");
        assert_eq!(path("/app/orders.rs?v=2"), "/app/orders.rs?<removed>");
    }

    #[test]
    fn an_error_message_with_nothing_sensitive_is_unchanged() {
        // A scrubber that mangled ordinary messages would be turned off, and a
        // scrubber that is turned off protects nothing.
        for message in [
            "connection reset by peer",
            "index 5 out of range for length 3",
            "no route matches GET /pricing",
            "the checkout total was 19.99",
            "a: b",
        ] {
            assert_eq!(text(message), message, "{message}");
        }
    }

    #[test]
    fn a_lone_separator_at_the_end_does_not_run_past_the_text() {
        for message in ["password=", "token:", "=", ":", ""] {
            let _ = text(message);
        }
    }

    #[test]
    fn a_removed_value_is_removed_however_many_there_are() {
        assert_eq!(
            text("password=a token=b secret=c"),
            "password=<removed> token=<removed> secret=<removed>"
        );
    }
}
