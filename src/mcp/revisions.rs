//! Which protocol revisions this server serves, and how a request names one.
//!
//! Both layers need this and neither owns it. The framing layer decides whether
//! a request may be served before any handshake, which is a question about the
//! revision it declares; the SDK adapter negotiates and advertises the same
//! table. Keeping it here is what stops the framing layer from depending on the
//! handler layer it sits underneath.

use rmcp::model::ProtocolVersion;
use serde_json::{Map, Value};

/// The `_meta` keys SEP-2575 reserves, spelled once.
///
/// They are read raw rather than through RMCP's typed accessors because one of
/// the things read out of them is this server's own `logging` extension, which
/// the typed `ClientCapabilities` drops on the way through.
mod key {
    pub(super) const PROTOCOL_VERSION: &str = "io.modelcontextprotocol/protocolVersion";
    pub(super) const CLIENT_CAPABILITIES: &str = "io.modelcontextprotocol/clientCapabilities";
}

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

/// The `_meta` object a request carries, if it carries one.
///
/// Takes the raw params because the framing layer reads them before RMCP has
/// parsed anything.
pub(crate) fn declaration(params: &Value) -> Option<&Map<String, Value>> {
    params.get("_meta")?.as_object()
}

/// Whether a request's own `_meta` declares it complete enough to be served
/// with no handshake behind it.
///
/// Both halves are required. The version says which revision is in force, and
/// only a handshake-free one earns the exemption; the capabilities are what
/// that revision puts in every request in place of the ones `initialize` used
/// to carry, and RMCP refuses the request without them anyway.
pub(crate) fn is_self_declaring(meta: &Map<String, Value>) -> bool {
    meta.get(key::PROTOCOL_VERSION)
        .and_then(Value::as_str)
        .is_some_and(is_handshake_free)
        && meta
            .get(key::CLIENT_CAPABILITIES)
            .is_some_and(Value::is_object)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two keys are spelled here and in RMCP. A rename on either side has
    /// to fail loudly: read raw, a drifted key is not a compile error, it is a
    /// server that quietly decides no client ever declares itself.
    #[test]
    fn the_keys_are_the_ones_rmcp_requires() {
        assert_eq!(
            [key::PROTOCOL_VERSION, key::CLIENT_CAPABILITIES],
            rmcp::model::RequestMetaObject::DRAFT_REQUIRED_KEYS,
            "SEP-2575 required-key spelling drifted from RMCP's"
        );
    }

    fn meta(version: &str, capabilities: Value) -> Map<String, Value> {
        let params = serde_json::json!({
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": version,
                "io.modelcontextprotocol/clientCapabilities": capabilities,
            }
        });
        declaration(&params).unwrap().clone()
    }

    #[test]
    fn only_a_handshake_free_revision_declares_itself() {
        assert!(is_self_declaring(&meta(
            "2026-07-28",
            serde_json::json!({})
        )));
        assert!(!is_self_declaring(&meta(
            "2025-06-18",
            serde_json::json!({})
        )));
        assert!(!is_self_declaring(&meta(
            "2099-01-01",
            serde_json::json!({})
        )));
    }

    #[test]
    fn capabilities_are_part_of_the_declaration() {
        // Present but not an object is not a declaration: the revision puts a
        // capabilities object in every request, and half of one says nothing.
        assert!(!is_self_declaring(&meta(
            "2026-07-28",
            serde_json::json!("nope")
        )));
        let no_capabilities = serde_json::json!({
            "_meta": { "io.modelcontextprotocol/protocolVersion": "2026-07-28" }
        });
        assert!(!is_self_declaring(
            declaration(&no_capabilities).expect("params carry a `_meta`")
        ));
    }

    #[test]
    fn params_without_a_meta_declare_nothing() {
        assert!(declaration(&serde_json::json!({})).is_none());

        // Array params reach the gate too, and indexing one by a string key is
        // not a lookup that can succeed.
        assert!(declaration(&serde_json::json!([1, 2])).is_none());
    }

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
