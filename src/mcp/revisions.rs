//! The protocol revisions this server serves.
//!
//! Split out of the SDK adapter so the framing layer can ask what this server
//! speaks without depending on the handler layer that sits above it.

use rmcp::model::ProtocolVersion;

/// One protocol revision this server serves, and how a client reaches it.
///
/// Whether a revision has a handshake is a fact about that revision, so it is
/// recorded next to it. Deriving it from position instead ("every revision but
/// the newest") holds only while exactly one revision lacks `initialize` and
/// it happens to sort first: the next such revision added to the head would
/// quietly make 2026-07-28 negotiable, which is the one thing the split exists
/// to prevent.
struct Revision {
    version: ProtocolVersion,
    /// Whether `initialize` can negotiate it. 2026-07-28 deleted the
    /// handshake, so it is reached through `server/discover` instead.
    handshake: bool,
}

/// The revisions this server serves, newest first.
const REVISIONS: &[Revision] = &[
    Revision {
        version: ProtocolVersion::V_2026_07_28,
        handshake: false,
    },
    Revision {
        version: ProtocolVersion::V_2025_11_25,
        handshake: true,
    },
    Revision {
        version: ProtocolVersion::V_2025_06_18,
        handshake: true,
    },
    Revision {
        version: ProtocolVersion::V_2025_03_26,
        handshake: true,
    },
    Revision {
        version: ProtocolVersion::V_2024_11_05,
        handshake: true,
    },
];

/// Everything served: what `server/discover` advertises and RMCP negotiates
/// within.
pub(crate) fn supported_protocol_versions() -> &'static [ProtocolVersion] {
    static ALL: std::sync::OnceLock<Vec<ProtocolVersion>> = std::sync::OnceLock::new();
    ALL.get_or_init(|| REVISIONS.iter().map(|r| r.version.clone()).collect())
}

/// What `initialize` can reach, which is the only useful thing to offer a
/// client whose `initialize` was refused.
pub(crate) fn negotiable_protocol_versions() -> &'static [ProtocolVersion] {
    static NEGOTIABLE: std::sync::OnceLock<Vec<ProtocolVersion>> = std::sync::OnceLock::new();
    NEGOTIABLE.get_or_init(|| {
        REVISIONS
            .iter()
            .filter(|r| r.handshake)
            .map(|r| r.version.clone())
            .collect()
    })
}

/// Whether a revision stands without a handshake.
///
/// Only the revisions that deleted `initialize` do. For every other one the
/// protocol version does not live in `_meta` at all, so a request carrying it
/// there is not a conforming client of that revision and has no standing to
/// skip the handshake the revision defines.
pub(crate) fn is_handshake_free(version: &str) -> bool {
    REVISIONS
        .iter()
        .any(|revision| !revision.handshake && revision.version.as_str() == version)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_revision_without_a_handshake_is_never_offered_by_one() {
        // The refusal for an unsupported `initialize` names what the client
        // could ask for instead, so a revision that has no `initialize` must
        // not appear there: it would send the client back to the method that
        // just failed. The table is what keeps the two lists in step.
        //
        // 2026-07-28 is named outright rather than left to the loop below,
        // which passes for free if nothing is marked as lacking a handshake.
        // That it deleted `initialize` is a fact about the revision, not a
        // preference, so the table is wrong if it ever says otherwise.
        let negotiable = negotiable_protocol_versions();
        assert!(
            supported_protocol_versions().contains(&ProtocolVersion::V_2026_07_28),
            "2026-07-28 is served, through server/discover"
        );
        assert!(
            !negotiable.contains(&ProtocolVersion::V_2026_07_28),
            "2026-07-28 has no initialize, so it cannot be negotiated by one"
        );
        for revision in REVISIONS.iter().filter(|r| !r.handshake) {
            assert!(
                !negotiable.contains(&revision.version),
                "{} has no handshake but is offered as one to negotiate",
                revision.version
            );
        }
        assert!(
            !negotiable.is_empty(),
            "some revision has to be reachable through initialize"
        );
    }

    #[test]
    fn every_served_revision_is_advertised() {
        // `server/discover` is the only place a client can learn about a
        // revision it cannot negotiate, so the advertised list is all of them.
        let supported = supported_protocol_versions();
        assert_eq!(supported.len(), REVISIONS.len());
        for revision in REVISIONS {
            assert!(supported.contains(&revision.version));
        }
    }
}
