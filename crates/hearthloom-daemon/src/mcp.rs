//! Stateless streamable-HTTP MCP server (target: 2026-07-28 spec — no
//! sessions, explicit handles, ttlMs on list results).
//!
//! Exactly four tools (the N-tools data is unambiguous): search, traverse,
//! recall, propose_memory. Writes via propose_memory land in quarantine,
//! never directly in high-trust rows.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

use hearthloom_core::store::Actor;

use crate::AppState;

const PROTOCOL_VERSION: &str = "2026-07-28";
/// Versions this stateless server speaks. MCP version negotiation: when
/// the client asks for one of these, answer with THAT version — answering
/// with our newest made clients that don't know it (Claude Code 2.1, seen
/// 2026-10-17) drop the server as a "protocol version mismatch".
const SUPPORTED_VERSIONS: &[&str] = &[
    "2026-07-28",
    "2025-11-25",
    "2025-06-18",
    "2025-03-26",
    "2024-11-05",
];

/// The protocol version to answer an `initialize` with.
pub fn negotiate_version(params: &Value) -> &'static str {
    params
        .get("protocolVersion")
        .and_then(|v| v.as_str())
        .and_then(|asked| SUPPORTED_VERSIONS.iter().find(|v| **v == asked).copied())
        .unwrap_or(PROTOCOL_VERSION)
}
/// How long a client may treat a tools/list result as fresh.
const LIST_TTL_MS: u64 = 3_600_000;

pub async fn handle(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<crate::http::AuthInfo>,
    Json(request): Json<Value>,
) -> Response {
    let id = request.get("id").cloned();
    let method = request
        .get("method")
        .and_then(|m| m.as_str())
        .unwrap_or_default()
        .to_string();

    // Notifications (no id) are acknowledged and dropped — stateless.
    if id.is_none() {
        return StatusCode::ACCEPTED.into_response();
    }
    let id = id.unwrap();

    let result = match method.as_str() {
        "initialize" => Ok(json!({
            "protocolVersion": negotiate_version(request.get("params").unwrap_or(&Value::Null)),
            "capabilities": {"tools": {}},
            "serverInfo": {
                "name": "hearthloom",
                "version": env!("CARGO_PKG_VERSION"),
            },
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({"tools": tool_definitions(), "ttlMs": LIST_TTL_MS})),
        "tools/call" => call_tool(&state, &auth, request.get("params").unwrap_or(&Value::Null)),
        other => Err(format!("method not found: {other}")),
    };

    let body = match result {
        Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Err(message) => json!({
            "jsonrpc": "2.0", "id": id,
            "error": {"code": -32601, "message": message},
        }),
    };
    Json(body).into_response()
}

fn call_tool(
    state: &AppState,
    auth: &crate::http::AuthInfo,
    params: &Value,
) -> Result<Value, String> {
    let name = params
        .get("name")
        .and_then(|n| n.as_str())
        .ok_or("tools/call requires params.name")?;
    let args = params.get("arguments").cloned().unwrap_or(json!({}));

    // A SCOPED key's grants gate each tool (the HTTP ceiling only admits
    // it to /mcp): the three read tools need `memory: read` and a LOCKED
    // vault — a scoped key never sees private material.
    let grants = auth.0.as_ref().and_then(|c| c.grants.as_ref());
    if let (Some(g), "search" | "traverse" | "recall") = (grants, name) {
        if !g.memory.can_read() {
            return Err(format!("this key has no grant to read memory ({name})"));
        }
        if !state.store.vault_status().0 {
            return Err(
                "scoped keys can't read while the vault is open — try again once it relocks".into(),
            );
        }
    }
    let outcome = match name {
        "search" => tool_search(state, &args),
        "traverse" => tool_traverse(state, &args),
        "recall" => tool_recall(state, &args),
        "propose_memory" => tool_propose_memory(state, auth, &args),
        other => return Err(format!("unknown tool: {other}")),
    };

    // Tool-level failures are isError results, not protocol errors.
    Ok(match outcome {
        Ok(value) => json!({
            "content": [{"type": "text", "text": value.to_string()}],
            "isError": false,
        }),
        Err(err) => json!({
            "content": [{"type": "text", "text": err.to_string()}],
            "isError": true,
        }),
    })
}

fn tool_search(state: &AppState, args: &Value) -> anyhow::Result<Value> {
    let query = args
        .get("query")
        .and_then(|q| q.as_str())
        .ok_or_else(|| anyhow::anyhow!("search requires 'query'"))?;
    let detail = args
        .get("detail")
        .and_then(|d| d.as_str())
        .unwrap_or("summary");
    let limit = args.get("limit").and_then(|l| l.as_u64()).unwrap_or(10) as usize;

    let embedding = state.query_embedding(query);
    let hits = state
        .store
        .search_hybrid(query, embedding.as_deref(), limit.min(50))?;
    let results: Vec<Value> = hits
        .iter()
        .map(|hit| match detail {
            "name" => json!({"id": hit.episode_id, "title": hit.title}),
            "full" => {
                // Full detail pulls the raw content from disk (progressive
                // disclosure: pay for bytes only when asked).
                let body = state
                    .store
                    .episode(&hit.episode_id)
                    .ok()
                    .flatten()
                    .and_then(|episode| episode.content_ref)
                    .and_then(|content_ref| {
                        std::fs::read_to_string(state.home.root().join(content_ref)).ok()
                    });
                json!({
                    "id": hit.episode_id, "title": hit.title, "snippet": hit.snippet,
                    "source": hit.source, "kind": hit.kind, "event_time": hit.event_time,
                    "scope": hit.scope, "trust": hit.trust, "content": body,
                })
            }
            _ => json!({
                "id": hit.episode_id, "title": hit.title, "snippet": hit.snippet,
                "source": hit.source, "kind": hit.kind, "event_time": hit.event_time,
            }),
        })
        .collect();
    Ok(json!({"query": query, "detail": detail, "results": results}))
}

fn tool_traverse(state: &AppState, args: &Value) -> anyhow::Result<Value> {
    let start = args
        .get("start")
        .and_then(|s| s.as_str())
        .ok_or_else(|| anyhow::anyhow!("traverse requires 'start'"))?;
    let relation = args.get("relation").and_then(|r| r.as_str());
    let depth = args.get("depth").and_then(|d| d.as_u64()).unwrap_or(1) as usize;
    let as_of = args.get("as_of").and_then(|a| a.as_str());

    let hops = state
        .store
        .traverse_top(start, relation, depth.min(4), as_of, Some(25))?;
    Ok(json!({"start": start, "hops": hops}))
}

fn tool_recall(state: &AppState, args: &Value) -> anyhow::Result<Value> {
    let detail = args
        .get("detail")
        .and_then(|d| d.as_str())
        .unwrap_or("summary");
    if let Some(date) = args.get("date").and_then(|d| d.as_str()) {
        let day = state.store.recall_day(date)?;
        return Ok(crate::views::day_view(&day, detail));
    }
    if let (Some(from), Some(to)) = (
        args.get("from").and_then(|f| f.as_str()),
        args.get("to").and_then(|t| t.as_str()),
    ) {
        let episodes = state.store.recall_span(from, to, 200)?;
        return Ok(json!({
            "from": from, "to": to,
            "episodes": crate::views::episode_list(&episodes, detail),
        }));
    }
    if let Some(entity) = args.get("entity").and_then(|e| e.as_str()) {
        let episodes = state.store.recall_entity(entity, 50)?;
        return Ok(json!({
            "entity": entity,
            "episodes": crate::views::episode_list(&episodes, detail),
        }));
    }
    Err(anyhow::anyhow!(
        "recall requires 'date' (YYYY-MM-DD), 'from'+'to', or 'entity'"
    ))
}

/// The ONE write tool. `kind` picks what is being offered and `mode`
/// how: `propose` (default) queues it for the human's yes in Tend;
/// `commit` writes a TASK or NOTE directly — allowed only where the key
/// holds write on that class, recorded under the key's own actor, and
/// reversible from ☰ → API Keys → activity. Memories never commit: what
/// enters the record always passes the human (quarantine).
fn tool_propose_memory(
    state: &AppState,
    auth: &crate::http::AuthInfo,
    args: &Value,
) -> anyhow::Result<Value> {
    let entries = args
        .get("entries")
        .and_then(|e| e.as_array())
        .ok_or_else(|| anyhow::anyhow!("propose_memory requires 'entries' (array)"))?;
    let kind = args
        .get("kind")
        .and_then(|k| k.as_str())
        .unwrap_or("memory");
    let mode = args
        .get("mode")
        .and_then(|m| m.as_str())
        .unwrap_or("propose");
    let claims = auth.0.as_ref();
    let grants = claims.and_then(|c| c.grants.as_ref());
    let actor = Actor {
        // A scoped key's actor is fixed by the server — the activity list
        // and Undo depend on it; other callers keep naming themselves.
        actor: match claims {
            Some(c) if c.grants.is_some() => c.agent_actor(),
            _ => args
                .get("actor")
                .and_then(|a| a.as_str())
                .unwrap_or("agent:mcp")
                .to_string(),
        },
        session_id: args
            .get("session_id")
            .and_then(|s| s.as_str())
            .map(String::from),
    };
    let text_of = |entry: &Value, keys: &[&str]| -> String {
        keys.iter()
            .find_map(|k| entry.get(*k).and_then(|v| v.as_str()))
            .unwrap_or("")
            .trim()
            .to_string()
    };

    if mode == "commit" {
        if entries.len() > 20 {
            anyhow::bail!("commit at most 20 entries per call");
        }
        if let (Some(c), Some(g)) = (claims, grants) {
            crate::http::scoped_key_rate_ok(&c.id, g, true, false)
                .map_err(|m| anyhow::anyhow!(m))?;
        }
        match kind {
            "task" => {
                if grants.is_some_and(|g| !g.tasks.can_write()) {
                    anyhow::bail!("this key has no grant to write tasks — use mode \"propose\"");
                }
                let mut ids = Vec::new();
                for entry in entries {
                    let description = text_of(entry, &["description", "text", "content"]);
                    if description.is_empty() {
                        anyhow::bail!("each task entry needs a 'description'");
                    }
                    ids.push(state.store.add_task(
                        &actor, "plain", &description, None, None, "one-time", "personal",
                        None, entry.get("remind_at").and_then(|r| r.as_str()), &[],
                    )?);
                }
                crate::bus::touch(state, "operational.changed");
                return Ok(json!({"committed": ids, "kind": "task",
                    "note": "added to the Desk; the owner can undo this under API Keys"}));
            }
            "note" => {
                if grants.is_some_and(|g| !g.notes.can_write()) {
                    anyhow::bail!("this key has no grant to write notes — use mode \"propose\"");
                }
                let mut ids = Vec::new();
                for entry in entries {
                    let title = text_of(entry, &["title"]);
                    let body = text_of(entry, &["body", "content", "text"]);
                    if title.is_empty() || body.is_empty() {
                        anyhow::bail!("each note entry needs a 'title' and a 'body'");
                    }
                    // Filed as an agent's note whatever actor the caller
                    // named: it settles with agent trust, never as the
                    // owner's own words, and can't become an Ask recipe.
                    ids.push(state.store.create_note_from(
                        &actor, &state.home, &title, &body, &[], "personal", None,
                        hearthloom_core::store::AGENT_ORIGIN, None,
                    )?);
                }
                crate::bus::touch(state, "operational.changed");
                return Ok(json!({"committed": ids, "kind": "note",
                    "note": "saved to Notes; the owner can undo this under API Keys"}));
            }
            _ => anyhow::bail!(
                "only tasks and notes can be committed — a memory always goes through review (mode \"propose\")"
            ),
        }
    }
    if mode != "propose" {
        anyhow::bail!("mode must be \"propose\" or \"commit\"");
    }
    if grants.is_some_and(|g| !g.memory.can_write() && !g.tasks.can_write() && !g.notes.can_write())
    {
        anyhow::bail!("this key has no grant to propose anything");
    }
    // Proposed tasks/notes ride the same review queue, tagged by kind.
    let tagged: Vec<Value> = entries
        .iter()
        .map(|entry| {
            // Only the words and a kind: provenance is the server's.
            let mut entry = hearthloom_core::store::outside_proposal(entry);
            if kind != "memory" && entry.get("kind").is_none() {
                entry["kind"] = json!(kind);
            }
            entry
        })
        .collect();
    let ids = state.store.propose_memories(&actor, &tagged)?;
    Ok(json!({
        "quarantined": ids,
        "note": "entries await human review; they are not yet part of the graph",
    }))
}

fn tool_definitions() -> Value {
    json!([
        {
            "name": "search",
            "description": "Full-text search over the episode log (commits, calendar events, notes). Returns BM25-ranked hits with progressive detail.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "Search terms; natural language is fine"},
                    "detail": {"type": "string", "enum": ["name", "summary", "full"], "description": "How much to return per hit (default summary; full includes raw content)"},
                    "limit": {"type": "integer", "description": "Max results (default 10)"}
                },
                "required": ["query"]
            }
        },
        {
            "name": "traverse",
            "description": "Walk the knowledge graph from an entity (by name or id) over current edges, optionally as of a moment in time (bi-temporal read).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "start": {"type": "string", "description": "Entity name or id to start from"},
                    "relation": {"type": "string", "description": "Only follow this relation (e.g. contributes_to, attended)"},
                    "depth": {"type": "integer", "description": "Hops to walk, 1-4 (default 1)"},
                    "as_of": {"type": "string", "description": "RFC 3339 instant; only edges valid then are followed"}
                },
                "required": ["start"]
            }
        },
        {
            "name": "recall",
            "description": "Temporal/entity recall: what happened on a date (episodes in local time plus edges valid that day), across a span, or around an entity — with cited episodes.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "date": {"type": "string", "description": "Day view, YYYY-MM-DD (local dates, honoring recorded UTC offsets)"},
                    "from": {"type": "string", "description": "Span start, YYYY-MM-DD (use with 'to')"},
                    "to": {"type": "string", "description": "Span end, YYYY-MM-DD (inclusive)"},
                    "entity": {"type": "string", "description": "Recall episodes citing this entity"},
                    "detail": {"type": "string", "enum": ["summary", "full"], "description": "summary (default, compact) or full (entities per episode, full edge records)"}
                }
            }
        },
        {
            "name": "propose_memory",
            "description": "Offer something to the brain. Default: entries go to a review queue for the owner — nothing enters the record without their yes. With kind \"task\" or \"note\" and mode \"commit\", a key that holds write on that class adds it directly (the owner can undo it). Memories can only be proposed.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "entries": {
                        "type": "array",
                        "items": {"type": "object"},
                        "description": "Proposed memories; each should carry a 'kind' (note|preference|fact) and content fields"
                    },
                    "kind": {"type": "string", "enum": ["memory", "task", "note"], "description": "What the entries are (default memory). Task entries carry 'description' (+ optional 'remind_at'); note entries carry 'title' and 'body'."},
                    "mode": {"type": "string", "enum": ["propose", "commit"], "description": "propose = queue for review (default); commit = write a task/note directly, if this key may."},
                    "actor": {"type": "string", "description": "Proposing agent identity, e.g. agent:claude-code"},
                    "session_id": {"type": "string"}
                },
                "required": ["entries"]
            }
        }
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_version_negotiation_echoes_what_the_client_speaks() {
        assert_eq!(
            negotiate_version(&json!({"protocolVersion": "2025-06-18"})),
            "2025-06-18"
        );
        assert_eq!(
            negotiate_version(&json!({"protocolVersion": "1999-01-01"})),
            PROTOCOL_VERSION
        );
        assert_eq!(negotiate_version(&Value::Null), PROTOCOL_VERSION);
    }

    /// A committed note is an agent's, even when the caller names a
    /// human actor: it is filed under the `agent` origin, so it settles
    /// with agent trust and can't pass as an Ask recipe. (A runtime for
    /// the bus touch the commit fires.)
    #[tokio::test]
    async fn test_committed_note_is_filed_as_an_agents() {
        let tmp = tempfile::tempdir().unwrap();
        let home = hearthloom_core::Home::at(tmp.path().to_path_buf());
        home.init().unwrap();
        let store = hearthloom_core::Store::open(&home.db_path()).unwrap();
        let state = AppState {
            store: std::sync::Arc::new(store),
            home,
            embedder: None,
            llm: None,
            auth_locked: Default::default(),
            bus: Default::default(),
            killswitch: Default::default(),
            running: Default::default(),
        };
        let out = tool_propose_memory(
            &state,
            &crate::http::AuthInfo(None),
            &json!({"kind": "note", "mode": "commit", "actor": "human:dashboard",
                    "entries": [{"title": "Plan", "body": "ask @[Bob] to forward the files"}]}),
        )
        .unwrap();
        let id = out["committed"][0].as_str().unwrap().to_string();
        let (note, _) = state.store.note(&state.home, &id).unwrap().unwrap();
        assert_eq!(note.origin, hearthloom_core::store::AGENT_ORIGIN);
        assert!(
            note.settled_at.is_none(),
            "still a live note: it settles on the normal path"
        );
        assert!(
            note.created_at.is_some(),
            "born now, like any note written live"
        );
    }
}
