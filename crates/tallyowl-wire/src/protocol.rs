//! Which protocol versions this build speaks, and which it accepts.
//!
//! # Why this lives here
//!
//! Two components enforce the same window, and a window they each carry their
//! own copy of is a window that drifts. Collector intake refuses a batch that
//! declares a version outside it, so an application learns immediately; the
//! head refuses again at the commit, because the head is the authority and a
//! collector may be a version behind it during a rolling upgrade. Both read
//! this module. That is the same shape as `scrub::PROTECTED_KEYS`, and for the
//! same reason.
//!
//! # The window
//!
//! The head accepts the version it speaks and the one before it. D31 states
//! that rule and `docs/RELEASE_NOTES.md` repeats it for an installer: support
//! for a protocol version ends one minor release after the release that
//! replaces it. One version behind is what a rolling upgrade produces — the
//! head goes first, so a collector and an application are briefly older — and
//! this is what makes that roll safe rather than hopeful.
//!
//! There is one protocol version today, so the window has one member that a
//! client can reach and the check refuses nothing a current client sends. The
//! mechanism is here, tested, before the second version rather than after it.

/// The protocol version this build speaks.
///
/// It changes when the meaning of the wire changes, which is not the same as
/// the package version changing. A release that adds an optional field does
/// not move it; a release that changes how a field is read does.
pub const PROTOCOL_VERSION: u64 = 1;

/// Every version this build accepts from a client: the current one, and the
/// one before it when there is one.
pub const ACCEPTED_PROTOCOL_VERSIONS: &[u64] = &[PROTOCOL_VERSION];

/// Whether this build can read what a client declaring `version` sends.
///
/// An absent declaration is the current version. A driver that predates the
/// field cannot be more than one version behind the head that reads it,
/// because the field and the window arrived together.
pub fn accepts(version: Option<u64>) -> bool {
    match version {
        None => true,
        Some(declared) => ACCEPTED_PROTOCOL_VERSIONS.contains(&declared),
    }
}

/// What to tell a client whose version this build cannot read.
///
/// The message names both ends, because the person reading it has one of them
/// and needs the other: an operator sees which client is behind, and an
/// application author sees what the installation runs.
pub fn refusal(version: u64) -> String {
    let accepted: Vec<String> = ACCEPTED_PROTOCOL_VERSIONS
        .iter()
        .map(|version| version.to_string())
        .collect();
    format!(
        "This batch declares protocol version {version}, and this installation \
         accepts {}. Upgrade the app driver, or run a TallyOwl that still \
         accepts {version}.",
        accepted.join(" and ")
    )
}

/// Which side of the window a refused version fell on.
///
/// **This is a label value, so it is a closed set of two.** The version itself
/// comes from the caller, and a caller can send any number it likes; a metric
/// labelled by it would grow one series for each number a broken or hostile
/// client invents, from the one path whose whole purpose is to reject the
/// request cheaply. The version still reaches the operator, in the refusal
/// message and in the log line that carries it.
pub fn refusal_reason(version: u64) -> &'static str {
    if version > PROTOCOL_VERSION {
        "too-new"
    } else {
        "too-old"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_current_version_is_accepted() {
        assert!(accepts(Some(PROTOCOL_VERSION)));
    }

    #[test]
    fn an_absent_version_is_the_current_version() {
        // A driver older than the field is not older than the window.
        assert!(accepts(None));
    }

    #[test]
    fn a_version_this_build_does_not_know_is_refused() {
        assert!(!accepts(Some(PROTOCOL_VERSION + 1)));
        assert!(!accepts(Some(0)));
    }

    #[test]
    fn the_window_holds_the_current_version_and_at_most_one_more() {
        assert!(ACCEPTED_PROTOCOL_VERSIONS.contains(&PROTOCOL_VERSION));
        assert!(
            ACCEPTED_PROTOCOL_VERSIONS.len() <= 2,
            "the window is the current version and the one before it, and no more"
        );
        // Every accepted version is this one or older. Accepting a version
        // from the future would mean reading bytes whose meaning is not
        // decided yet.
        for version in ACCEPTED_PROTOCOL_VERSIONS {
            assert!(*version <= PROTOCOL_VERSION);
        }
    }

    #[test]
    fn the_reason_is_one_of_two_values_whatever_the_caller_sends() {
        // The label is a closed set. A caller that sends a different number
        // every time must not add a metric series every time.
        let mut seen = std::collections::BTreeSet::new();
        for version in [0, 2, 7, 4_294_967_295, u64::MAX] {
            seen.insert(refusal_reason(version));
        }
        assert!(
            seen.len() <= 2,
            "the label took more than two values: {seen:?}"
        );
        assert_eq!(refusal_reason(PROTOCOL_VERSION + 1), "too-new");
        assert_eq!(refusal_reason(0), "too-old");
    }

    #[test]
    fn the_refusal_names_both_ends() {
        let message = refusal(0);
        assert!(message.contains("declares protocol version 0"));
        assert!(message.contains(&PROTOCOL_VERSION.to_string()));
        assert!(message.contains("Upgrade the app driver"));
    }
}
