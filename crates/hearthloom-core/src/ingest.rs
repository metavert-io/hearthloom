//! Adapter output types: the universal "source yields episodes" interface
//! (HearthFS's Source trait generalized — a source yields episodes, not
//! files).
//!
//! Adapters produce `IngestBatch`es of `EpisodeDraft`s; only the daemon
//! turns drafts into rows (design law #4: the daemon is the sole writer).
//! Entity references are resolved by the store at apply time — adapters
//! name entities and identifying attributes, they never mint entity ids.

use serde::{Deserialize, Serialize};

use crate::time::EventTime;

/// One episode as emitted by an adapter, before it touches the store.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EpisodeDraft {
    /// Adapter id, e.g. `git`, `ics`.
    pub source: String,
    /// Source-native identity (commit sha, ICS UID). With `source`, this is
    /// the idempotency key: re-ingesting the same material is a no-op.
    pub source_ref: String,
    /// Episode kind, e.g. `commit`, `calendar_event`, `note`.
    pub kind: String,
    pub title: String,
    /// Raw content — kept verbatim on disk, never discarded.
    pub body: String,
    pub event_time: EventTime,
    /// Trust tier: `self` | `imported` | `agent` | `untrusted`.
    pub trust: String,
    /// Scope label: `work` | `personal` | `family` | `health` | `private`.
    /// None = infer at apply time: an already-known Project entity's scope
    /// wins (a repo you marked `personal` stays personal), then the
    /// author/organizer identity's scope (a `work` email labels its
    /// material work), then `personal`. One human, many contexts.
    pub scope: Option<String>,
    /// Entities this episode mentions/supports.
    pub entities: Vec<EntityRef>,
    /// Edges asserted by this episode, referencing `entities` by index.
    pub edges: Vec<EdgeAssertion>,
}

/// A reference to an entity by type + name + identifying attributes.
/// Identity resolution is attribute-first (an email owns a Person), then
/// exact (type, name) match, then a fresh entity — bias to under-merge.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntityRef {
    /// Person | Org | Place | Project | Event | Artifact | Preference |
    /// Task | Claim | Finding
    pub entity_type: String,
    pub name: String,
    /// Identifying attributes as (kind, value), e.g. ("email", "a@b.c"),
    /// ("repo-url", "https://…"). These drive cross-source identity.
    pub attributes: Vec<(String, String)>,
    /// This episode's relationship to the entity: author | attendee |
    /// organizer | repo | touched | subject …
    pub role: String,
}

/// A typed relation asserted between two entities in this episode's
/// `entities` list (by index). `t_valid` defaults to the episode's event
/// time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EdgeAssertion {
    pub src: usize,
    pub relation: String,
    pub dst: usize,
}

/// A batch of drafts plus the writing actor's identity (mandatory —
/// design law #4).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IngestBatch {
    pub actor: String,
    #[serde(default)]
    pub session_id: Option<String>,
    pub episodes: Vec<EpisodeDraft>,
}

/// What happened when a batch was applied.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IngestReport {
    pub episodes_new: usize,
    pub episodes_existing: usize,
    pub entities_created: usize,
    pub entities_resolved: usize,
    pub edges_created: usize,
    pub review_queued: usize,
}

/// A single-line field from someone else (a calendar title or venue, a
/// visit's place, the titles a chapter lists), made safe to write into a
/// line-structured body: every line break — `\r`, `\n`, VT, FF, NEL,
/// U+2028, U+2029 — becomes a space, the pieces are trimmed and joined
/// with one space, empty pieces dropped. Bodies carry lines that readers
/// trust by shape (`[HH:MM] place (lat,lon)` for the Atlas, `- title`
/// under a chapter's `written from:`), so a field with a line break in
/// it could otherwise forge one.
pub fn one_line(s: &str) -> String {
    s.split([
        '\r', '\n', '\u{0b}', '\u{0c}', '\u{85}', '\u{2028}', '\u{2029}',
    ])
    .map(str::trim)
    .filter(|piece| !piece.is_empty())
    .collect::<Vec<_>>()
    .join(" ")
}

#[cfg(test)]
mod tests {
    use super::one_line;

    #[test]
    fn test_one_line_folds_every_line_break() {
        assert_eq!(one_line("Lunch"), "Lunch");
        assert_eq!(one_line("  Lunch  "), "Lunch");
        assert_eq!(
            one_line("Lunch\n[12:00] Fake (40.71,-74.00)"),
            "Lunch [12:00] Fake (40.71,-74.00)"
        );
        assert_eq!(one_line("a\r\nb"), "a b");
        assert_eq!(one_line("a \n\n  b"), "a b");
        assert_eq!(
            one_line("a\u{85}b\u{2028}c\u{2029}d\u{0b}e\u{0c}f\rg"),
            "a b c d e f g"
        );
        assert_eq!(one_line("\n\n"), "");
        // what is inside a line is kept as it was
        assert_eq!(
            one_line("Café  (Rome) — upstairs"),
            "Café  (Rome) — upstairs"
        );
        assert!(!one_line("x\n- Forged").contains('\n'));
    }
}
