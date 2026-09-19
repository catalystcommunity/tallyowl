//! Error grouping, from D39.
//!
//! **The projector computes the fingerprint. A producer never controls its
//! group.** A client that could name its own group could split one defect into
//! a thousand groups, or merge a thousand defects into one, and either makes
//! the error view useless.
//!
//! # `fingerprint_v1`
//!
//! The first rule that applies:
//!
//! 1. The occurrence has one or more in-app frames. Hash the exception type and
//!    the top five in-app frames, using module and function only, with repeated
//!    frames from recursion collapsed.
//! 2. The occurrence has frames and none are in-app. Hash the exception type
//!    and the top five frames.
//! 3. The occurrence has no frames. Hash the exception type and the message
//!    with literals replaced by placeholders.
//!
//! Rule 3 exists because a browser error often arrives with no usable stack. A
//! design that used frames alone would put every such error in one group.
//!
//! **The fingerprint excludes line numbers and addresses.** A reformatting
//! change or a line shift moves every line number in a file, and a grouping
//! that split on that would report a thousand new defects after a `gofmt`.
//!
//! # Rebuilding at a new version
//!
//! TallyOwl stores the fingerprint inputs, the rule that applied, and the
//! version. A later version rebuilds every group from retained raw data, which
//! is why the inputs are stored rather than only the digest.

use tallyowl_collector_api::types::{ErrorPayload, StackFrame};

/// The fingerprint version this projector computes. It travels with every
/// occurrence, so a rebuild at a new version can tell what it is replacing.
pub const FINGERPRINT_VERSION: u64 = 1;

/// How many frames the fingerprint reads. Deeper frames are the framework and
/// the runtime, which are the same for every defect in an application.
const FRAMES: usize = 5;

/// Which of D39's three rules produced a fingerprint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rule {
    /// The occurrence had in-app frames.
    InAppFrames,
    /// The occurrence had frames and none were in-app.
    AllFrames,
    /// The occurrence had no frames, so the message decided.
    Message,
}

impl Rule {
    pub fn as_str(self) -> &'static str {
        match self {
            Rule::InAppFrames => "in-app-frames",
            Rule::AllFrames => "all-frames",
            Rule::Message => "message",
        }
    }
}

/// One computed group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fingerprint {
    /// The digest, as 16 lowercase hexadecimal characters. Short enough to read
    /// in a dashboard and wide enough that a collision is not a concern at the
    /// number of groups an installation has.
    pub digest: String,
    pub rule: Rule,
    pub version: u64,
    /// Exactly what was hashed. Stored so a later version can rebuild, and so
    /// somebody can see why two occurrences grouped together.
    pub inputs: Vec<String>,
}

/// Compute the group for one error occurrence.
pub fn fingerprint(error: &ErrorPayload) -> Fingerprint {
    let frames = error.frames.as_deref().unwrap_or(&[]);
    let in_app: Vec<&StackFrame> = frames.iter().filter(|frame| frame.in_app).collect();

    let (rule, mut inputs) = if !in_app.is_empty() {
        (Rule::InAppFrames, frame_inputs(&in_app))
    } else if !frames.is_empty() {
        let all: Vec<&StackFrame> = frames.iter().collect();
        (Rule::AllFrames, frame_inputs(&all))
    } else {
        (Rule::Message, vec![placeholders(&error.message)])
    };

    inputs.insert(0, error.error_type.clone());
    Fingerprint {
        digest: digest(&inputs),
        rule,
        version: FINGERPRINT_VERSION,
        inputs,
    }
}

/// The top frames, by module and function only, with recursion collapsed.
///
/// A line number is deliberately absent. A reformatting change shifts every
/// line in a file, and a grouping that split on that would report a thousand
/// new defects after a formatter ran.
fn frame_inputs(frames: &[&StackFrame]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for frame in frames {
        let named = format!(
            "{}::{}",
            frame.module.as_deref().unwrap_or(""),
            frame.function.as_deref().unwrap_or("")
        );
        // Recursion produces the same frame many times over. Collapsing it
        // means one runaway recursion is one group rather than one group for
        // each depth it happened to reach.
        if out.last() == Some(&named) {
            continue;
        }
        out.push(named);
        if out.len() == FRAMES {
            break;
        }
    }
    out
}

/// Replace the parts of a message that differ between occurrences of one
/// defect.
///
/// `index 5 out of range for length 3` and `index 9 out of range for length 4`
/// are one defect. Without this they are two groups, and an error view that
/// makes one defect into a thousand groups is not an error view.
pub fn placeholders(message: &str) -> String {
    let mut out = String::with_capacity(message.len());
    let characters: Vec<char> = message.chars().collect();
    let mut index = 0;
    while index < characters.len() {
        let c = characters[index];
        if c.is_ascii_digit() {
            // A run of digits, and anything hexadecimal or dotted attached to
            // it, is one value. An address, a version, and a count all read the
            // same way here on purpose: none of them tells two occurrences of
            // one defect apart.
            while index < characters.len()
                && (characters[index].is_ascii_hexdigit()
                    || characters[index] == '.'
                    || characters[index] == 'x'
                    || characters[index] == '-')
            {
                index += 1;
            }
            out.push_str("<n>");
            continue;
        }
        if c == '"' || c == '\'' {
            let quote = c;
            index += 1;
            while index < characters.len() && characters[index] != quote {
                index += 1;
            }
            index = (index + 1).min(characters.len());
            out.push_str("<s>");
            continue;
        }
        out.push(c);
        index += 1;
    }
    out
}

fn digest(inputs: &[String]) -> String {
    // A separator that cannot appear in a module or function name, so
    // `["ab", "c"]` and `["a", "bc"]` are different fingerprints.
    let joined = inputs.join("\u{1f}");
    let hash = blake3::hash(joined.as_bytes());
    hash.to_hex()[..16].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tallyowl_collector_api::types::ErrorPayload_severity as Severity;

    fn frame(module: &str, function: &str, line: u64, in_app: bool) -> StackFrame {
        StackFrame {
            module: Some(module.into()),
            function: Some(function.into()),
            file: Some(format!("/app/{module}.rs")),
            line: Some(line),
            in_app,
        }
    }

    fn error(error_type: &str, message: &str, frames: Vec<StackFrame>) -> ErrorPayload {
        ErrorPayload {
            error_type: error_type.into(),
            message: message.into(),
            handled: false,
            severity: Severity::Error,
            mechanism: None,
            runtime: None,
            frames: (!frames.is_empty()).then_some(frames),
            breadcrumbs: None,
        }
    }

    #[test]
    fn a_line_number_change_keeps_one_group() {
        // The property D39 exists for. A formatter that shifted every line in a
        // file would otherwise report a thousand new defects.
        let before = error(
            "NullPointer",
            "no value",
            vec![frame("checkout", "total", 40, true)],
        );
        let after = error(
            "NullPointer",
            "no value",
            vec![frame("checkout", "total", 61, true)],
        );
        assert_eq!(fingerprint(&before).digest, fingerprint(&after).digest);
    }

    #[test]
    fn an_in_app_frame_decides_even_when_framework_frames_are_on_top() {
        // Two defects in one application throw through the same framework. A
        // fingerprint over the top frames alone would group them together.
        let first = error(
            "Timeout",
            "took too long",
            vec![
                frame("hyper", "poll", 1, false),
                frame("checkout", "charge", 10, true),
            ],
        );
        let second = error(
            "Timeout",
            "took too long",
            vec![
                frame("hyper", "poll", 1, false),
                frame("signup", "register", 10, true),
            ],
        );
        assert_ne!(fingerprint(&first).digest, fingerprint(&second).digest);
        assert_eq!(fingerprint(&first).rule, Rule::InAppFrames);
    }

    #[test]
    fn an_occurrence_with_no_in_app_frames_uses_every_frame() {
        let inside_the_runtime = error("Panic", "unwind", vec![frame("std", "abort", 1, false)]);
        assert_eq!(fingerprint(&inside_the_runtime).rule, Rule::AllFrames);
    }

    #[test]
    fn a_browser_error_with_no_stack_groups_by_its_message() {
        // Rule 3. A design that used frames alone would put every browser error
        // in one group, which is the same as having no groups.
        let first = error("TypeError", "x is not a function", Vec::new());
        let second = error("TypeError", "y is not a function", Vec::new());
        let third = error("TypeError", "x is not a function", Vec::new());

        assert_eq!(fingerprint(&first).rule, Rule::Message);
        assert_eq!(fingerprint(&first).digest, fingerprint(&third).digest);
        assert_ne!(fingerprint(&first).digest, fingerprint(&second).digest);
    }

    #[test]
    fn a_message_that_differs_only_in_its_numbers_is_one_group() {
        let first = error(
            "IndexError",
            "index 5 out of range for length 3",
            Vec::new(),
        );
        let second = error(
            "IndexError",
            "index 9 out of range for length 41",
            Vec::new(),
        );
        assert_eq!(fingerprint(&first).digest, fingerprint(&second).digest);
    }

    #[test]
    fn a_message_that_differs_only_in_a_quoted_value_is_one_group() {
        let first = error("KeyError", "no key \"order-88\" in cart", Vec::new());
        let second = error("KeyError", "no key \"order-91\" in cart", Vec::new());
        assert_eq!(fingerprint(&first).digest, fingerprint(&second).digest);
    }

    #[test]
    fn two_error_types_never_share_a_group() {
        let a = error("Timeout", "same", vec![frame("m", "f", 1, true)]);
        let b = error("Refused", "same", vec![frame("m", "f", 1, true)]);
        assert_ne!(fingerprint(&a).digest, fingerprint(&b).digest);
    }

    #[test]
    fn recursion_collapses_to_one_frame_rather_than_one_group_for_each_depth() {
        let shallow = error(
            "StackOverflow",
            "too deep",
            vec![
                frame("walk", "step", 1, true),
                frame("walk", "step", 1, true),
                frame("main", "run", 1, true),
            ],
        );
        let deep = error(
            "StackOverflow",
            "too deep",
            (0..4)
                .map(|_| frame("walk", "step", 1, true))
                .chain([frame("main", "run", 1, true)])
                .collect(),
        );
        assert_eq!(fingerprint(&shallow).digest, fingerprint(&deep).digest);
    }

    #[test]
    fn the_inputs_are_stored_so_a_later_version_can_rebuild() {
        let held = fingerprint(&error(
            "Timeout",
            "took too long",
            vec![frame("checkout", "charge", 10, true)],
        ));
        assert_eq!(held.version, FINGERPRINT_VERSION);
        assert_eq!(held.inputs[0], "Timeout");
        assert!(held.inputs[1].contains("checkout"));
    }

    #[test]
    fn a_digest_reads_the_same_way_every_time() {
        let held = fingerprint(&error("T", "m", Vec::new()));
        assert_eq!(held.digest.len(), 16);
        assert!(held.digest.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(
            held.digest,
            fingerprint(&error("T", "m", Vec::new())).digest
        );
    }

    #[test]
    fn two_input_lists_that_join_to_one_text_are_different_fingerprints() {
        // The separator has to be one that cannot appear in a module name, or
        // `["ab","c"]` and `["a","bc"]` would collide.
        assert_ne!(
            digest(&["ab".into(), "c".into()]),
            digest(&["a".into(), "bc".into()])
        );
    }
}
