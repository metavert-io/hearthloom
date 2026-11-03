//! The provenance view (0.5.78): how a memory got here, in words the
//! dashboard, the CLI and Ask can show. The facts are the store's
//! (core store/provenance.rs: origin + route, recorded on arrival); this
//! names the nodes from the synced node list and the agent task that
//! produced it — what that task read from outside and what it wrote —
//! and, for a summary, what it summarised.
use serde_json::{json, Value};

use crate::AppState;

pub fn describe(state: &AppState, episode_id: &str) -> Option<Value> {
    let origin = state.store.episode_origin(episode_id).ok().flatten()?;
    let nodes = state.store.nodes().unwrap_or_default();
    let me = crate::agents::node_id(state);
    let node = |id: Option<&str>, known_as: Option<&str>| -> Value {
        let name = id
            .and_then(|id| nodes.iter().find(|n| n["node_id"].as_str() == Some(id)))
            .and_then(|n| n["name"].as_str())
            .or(known_as);
        json!({"id": id, "name": name, "this": id == Some(me.as_str())})
    };
    let route: Vec<Value> = origin
        .route
        .iter()
        .map(|h| json!({"node": node(h.node.as_deref(), h.name.as_deref()), "at": h.at}))
        .collect();
    // the task: named on the origin, or the episode IS a task's outcome
    let task_id = origin.task.clone().or_else(|| {
        (origin.source == "agent")
            .then(|| origin.source_ref.strip_prefix("task:").map(String::from))
            .flatten()
    });
    let task = task_id
        .and_then(|id| state.store.agent_task(&id).ok().flatten())
        .map(|t| {
            let agent = t.agent_id.as_deref().and_then(|aid| {
                crate::agents::all_agents(state)
                    .into_iter()
                    .find(|(a, _)| a.id == aid)
                    .map(|(a, _)| a.name)
            });
            json!({
                "id": t.id, "request": t.request, "agent": agent, "status": t.status,
                "tainted": t.tainted, "read": t.inputs, "wrote": t.outputs,
            })
        });
    let summary_of = state
        .store
        .recorded_inputs(episode_id)
        .ok()
        .flatten()
        .map(|rows| {
            rows.iter()
                .take(20)
                .map(|e| json!({"id": e.id, "title": e.title, "source": e.source}))
                .collect::<Vec<_>>()
        });
    Some(json!({
        "writer": origin.writer,
        "writer_label": writer_label(origin.writer.as_deref()),
        "node": node(origin.node.as_deref(), None),
        "at": origin.at,
        "accepted_by": origin.accepted_by,
        "accepted_by_label": origin.accepted_by.as_deref().map(|a| writer_label(Some(a))),
        "route": route,
        "recorded": origin.recorded,
        "task": task,
        "summary_of": summary_of,
    }))
}

/// Who wrote something, in plain words.
pub fn writer_label(actor: Option<&str>) -> String {
    let Some(actor) = actor else {
        return "not known (it arrived before Hearthloom kept track)".into();
    };
    let (kind, rest) = actor.split_once(':').unwrap_or((actor, ""));
    match (kind, rest) {
        ("human", r) if r.starts_with("device:") => format!("you, from {}", &r[7..]),
        ("human", _) => "you".into(),
        ("adapter", r) if r.starts_with("capture:") => format!("you, captured on {}", &r[8..]),
        ("adapter", r) => format!("the {r} import"),
        ("agent", "consolidation") => "Hearthloom's own summaries".into(),
        ("agent", r) if r.starts_with("key:") => format!("an agent using the key \"{}\"", &r[4..]),
        ("agent", r) => format!("the agent \"{r}\""),
        ("plugin", r) => format!("the {r} ember"),
        ("peer", r) => format!("your node \"{r}\""),
        ("friend", r) => format!("your friend {r}"),
        ("system", r) => format!("Hearthloom ({r})"),
        _ => actor.to_string(),
    }
}

/// One line for the CLI and Ask: "written by the imessage import on
/// MacBook at … · via Cloud → Mac mini".
pub fn one_line(view: &Value) -> String {
    let name = |n: &Value| {
        n["name"]
            .as_str()
            .or(n["id"].as_str())
            .unwrap_or("an unknown node")
            .to_string()
    };
    let mut line = format!(
        "written by {}",
        view["writer_label"].as_str().unwrap_or("?")
    );
    if !view["node"]["id"].is_null() {
        line.push_str(&format!(" on {}", name(&view["node"])));
    }
    if let Some(at) = view["at"].as_str() {
        line.push_str(&format!(" at {}", at[..at.len().min(16)].replace('T', " ")));
    }
    if let Some(by) = view["accepted_by_label"].as_str() {
        line.push_str(&format!(", let in by {by}"));
    }
    let hops: Vec<String> = view["route"]
        .as_array()
        .into_iter()
        .flatten()
        .skip(1)
        .map(|h| name(&h["node"]))
        .collect();
    if !hops.is_empty() {
        line.push_str(&format!(" · came via {}", hops.join(" → ")));
    }
    if let Some(agent) = view["task"]["agent"].as_str() {
        let read = view["task"]["read"].as_array().map_or(0, Vec::len);
        line.push_str(&format!(
            " · from a task by {agent} that read {read} outside thing(s)"
        ));
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_writers_in_plain_words() {
        assert_eq!(
            writer_label(Some("adapter:imessage")),
            "the imessage import"
        );
        assert_eq!(writer_label(Some("human:dashboard")), "you");
        assert_eq!(writer_label(Some("human:device:phone")), "you, from phone");
        assert_eq!(
            writer_label(Some("agent:key:desk-hand")),
            "an agent using the key \"desk-hand\""
        );
        assert_eq!(
            writer_label(Some("agent:consolidation")),
            "Hearthloom's own summaries"
        );
        assert!(writer_label(None).starts_with("not known"));
        let view = json!({
            "writer_label": "the imessage import",
            "node": {"id": "nd_1", "name": "MacBook"}, "at": "2026-11-02T10:00:00Z",
            "route": [{"node": {"id": "nd_1", "name": "MacBook"}}, {"node": {"id": "nd_2", "name": "Cloud"}},
                      {"node": {"id": "nd_3", "name": "Mac mini"}}],
            "task": null,
        });
        assert_eq!(
            one_line(&view),
            "written by the imessage import on MacBook at 2026-11-02 10:00 · came via Cloud → Mac mini"
        );
    }
}
