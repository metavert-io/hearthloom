//! Progressive-disclosure renderings of recall results. The day view of a
//! busy day is tens of thousands of tokens at full detail; `summary` is
//! the default everywhere and `full` is an explicit ask.

use hearthloom_core::store::{DayView, EpisodeRow};
use serde_json::{json, Value};

const SUMMARY_TITLE_CAP: usize = 120;
const SUMMARY_ENTITY_CAP: usize = 4;
/// A summary day view lists at most this many episodes (chronological
/// head); the rest are rolled up into per-source counts.
const SUMMARY_EPISODE_CAP: usize = 120;

fn cap_title(title: &str) -> String {
    let mut t: String = title.chars().take(SUMMARY_TITLE_CAP).collect();
    if t.len() < title.len() {
        t.push('…');
    }
    t
}

/// Compact one-line-ish rendering of an episode.
pub fn episode_summary(episode: &EpisodeRow) -> Value {
    json!({
        "id": episode.id,
        "event_time": episode.event_time,
        "source": episode.source,
        "kind": episode.kind,
        "title": cap_title(&episode.title),
        "scope": episode.scope,
    })
}

/// Render a day view at the requested detail level.
pub fn day_view(day: &DayView, detail: &str) -> Value {
    if detail == "full" {
        return serde_json::to_value(day).unwrap_or(Value::Null);
    }
    let mut omitted: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for de in day.episodes.iter().skip(SUMMARY_EPISODE_CAP) {
        *omitted
            .entry(format!("{}/{}", de.episode.source, de.episode.kind))
            .or_default() += 1;
    }
    let episodes: Vec<Value> = day
        .episodes
        .iter()
        .take(SUMMARY_EPISODE_CAP)
        .map(|de| {
            let mut value = episode_summary(&de.episode);
            let named: Vec<String> = de
                .entities
                .iter()
                .filter(|(_, _, role)| role != "touched") // file lists dominate otherwise
                .take(SUMMARY_ENTITY_CAP)
                .map(|(name, _, role)| format!("{role}:{name}"))
                .collect();
            if !named.is_empty() {
                value["entities"] = json!(named);
            }
            value
        })
        .collect();
    let edges: Vec<String> = day
        .valid_edges
        .iter()
        .map(|edge| format!("{} —{}→ {}", edge.src.name, edge.relation, edge.dst.name))
        .collect();
    let mut result = json!({
        "date": day.date,
        "detail": "summary",
        "episode_count": day.episodes.len(),
        "episodes": episodes,
        "valid_edges": edges,
        "note": "pass detail=full for entities-per-episode and full edge records",
    });
    if !omitted.is_empty() {
        result["omitted"] = json!(omitted
            .into_iter()
            .map(|(source, count)| json!({"source": source, "count": count}))
            .collect::<Vec<_>>());
        result["note"] = json!(
            "episode list capped; 'omitted' has per-source counts — narrow with \
             recall from/to or search, or pass detail=full"
        );
    }
    result
}

/// Render an episode list (span/entity recall) at the requested detail.
pub fn episode_list(episodes: &[EpisodeRow], detail: &str) -> Value {
    if detail == "full" {
        return serde_json::to_value(episodes).unwrap_or(Value::Null);
    }
    json!(episodes.iter().map(episode_summary).collect::<Vec<_>>())
}

#[cfg(test)]
mod tests {
    use super::*;
    use hearthloom_core::store::Store;

    #[test]
    fn test_summary_is_much_smaller_than_full() {
        let store = Store::open_in_memory().unwrap();
        // Reuse the ingest path to build a real day view.
        let tmp = tempfile::tempdir().unwrap();
        let home = hearthloom_core::Home::at(tmp.path());
        home.init().unwrap();
        let episodes = (0..30)
            .map(|i| hearthloom_core::EpisodeDraft {
                source: "test".into(),
                source_ref: format!("r{i}"),
                kind: "commit".into(),
                title: format!("commit number {i} with a reasonably long subject line"),
                body: "line\n".repeat(200),
                event_time: hearthloom_core::EventTime::parse_rfc3339(
                    "2026-03-15T10:00:00Z",
                    hearthloom_core::TimeUncertainty::Exact,
                )
                .unwrap(),
                trust: "self".into(),
                scope: Some("work".into()),
                entities: (0..10)
                    .map(|f| hearthloom_core::EntityRef {
                        entity_type: "Artifact".into(),
                        name: format!("repo/file{f}.rs"),
                        attributes: vec![],
                        role: "touched".into(),
                    })
                    .collect(),
                edges: vec![],
            })
            .collect();
        store
            .apply_batch(
                &home,
                &hearthloom_core::IngestBatch {
                    actor: "adapter:test".into(),
                    session_id: None,
                    episodes,
                },
            )
            .unwrap();

        let day = store.recall_day("2026-03-15").unwrap();
        let full = day_view(&day, "full").to_string();
        let summary = day_view(&day, "summary").to_string();
        assert!(
            summary.len() * 4 < full.len(),
            "summary ({}) should be <25% of full ({})",
            summary.len(),
            full.len()
        );
        // Summary drops the touched-file noise but keeps identity.
        assert!(summary.contains("episode_count"));
        assert!(!summary.contains("file7.rs"));
    }
}
