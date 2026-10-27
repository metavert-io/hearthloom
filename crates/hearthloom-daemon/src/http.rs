//! HTTP API. Writes require actor identity (in the request body); reads
//! are open on loopback. Scoped-token auth is stubbed: a Bearer token, if
//! presented, is parsed into claims, but v0 loopback accepts everything.

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use hearthloom_core::store::Actor;
use hearthloom_core::{brain_index, IngestBatch};

use crate::{mcp, AppState};

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(crate::dashboard::index))
        .route("/assets/{*name}", get(crate::dashboard::asset))
        .route("/notes-media/{ref}/{file}", get(notes_media))
        .route("/v1/stats", get(crate::dashboard::stats))
        .route("/v1/ribbons", get(crate::dashboard::ribbons))
        .route("/v1/atlas/points", get(atlas_points))
        .route("/v1/timeline/histogram", get(timeline_histogram))
        .route("/v1/timeline/notable", get(timeline_notable))
        .route("/v1/onthisday", get(on_this_day))
        .route("/v1/constellation/evidence", post(constellation_evidence))
        .route("/health", get(health))
        .route("/v1/search", get(search))
        .route("/v1/recall", get(recall))
        .route("/v1/entity/{name}", get(entity))
        .route("/v1/entity/{name}/importance", post(set_importance))
        .route("/v1/episode/{id}", get(episode))
        .route("/v1/traverse", get(traverse))
        .route("/v1/events", get(events))
        .route("/v1/quarantine", get(quarantine))
        .route("/v1/quarantine/{id}", post(resolve_quarantine))
        .route("/v1/ingest", post(ingest))
        .route("/v1/ingest/health", post(ingest_health))
        .route("/v1/ingest/photos", post(ingest_photos))
        .route("/v1/ingest/contacts", post(ingest_contacts))
        .route("/v1/ingest/calendar", post(ingest_calendar))
        .route("/v1/ingest/reminders", post(ingest_reminders))
        .route("/v1/ingest/location", post(ingest_location))
        .route("/v1/ingest/motion", post(ingest_motion))
        .route("/v1/ingest/media", post(ingest_media))
        .route("/v1/ingest/scan", post(ingest_scan))
        .route("/v1/ingest/focus", post(ingest_focus))
        .route("/v1/ingest/movement", post(ingest_movement))
        .route("/v1/vehicles", get(vehicles_list))
        .route("/v1/vehicles/{vehicle}", post(vehicle_settings_set))
        .route("/v1/trips/questions", get(trip_questions))
        .route("/v1/trips/questions/{key}", post(trip_question_answer))
        .route("/v1/ingest/status", get(ingest_status))
        .route("/v1/ingest/recent", get(ingest_recent))
        .route("/v1/sql", post(raw_sql))
        .route("/v1/ingest/local", post(ingest_local))
        .route("/v1/propose", post(propose))
        .route("/v1/index", post(regenerate_index))
        .route("/v1/identity/claim", post(identity_claim))
        .route("/v1/identity/unclaim", post(identity_unclaim))
        .route("/v1/reviews/retriage", post(reviews_retriage))
        .route("/v1/identity/merge", post(identity_merge))
        .route("/v1/reviews", get(reviews))
        .route("/v1/reviews/{id}", post(resolve_review))
        .route("/v1/attributes/remove", post(remove_attribute))
        .route("/v1/merges", get(merges))
        .route("/v1/merges/{id}/revert", post(revert_merge))
        .route("/v1/scope", post(set_scope))
        .route("/v1/consolidate", post(consolidate_now))
        .route("/v1/query", post(deep_query))
        .route("/v1/query/stream", get(deep_query_stream))
        .route("/v1/ask/history", get(ask_history))
        .route("/v1/ask/{id}", get(ask_get).delete(ask_delete))
        .route("/v1/ask/{id}/trace", get(ask_trace))
        .route("/v1/ask/{id}/keep", post(ask_keep))
        .route("/v1/ask/{id}/thread", get(ask_thread))
        .route("/v1/ask/{id}/privacy", get(ask_privacy_get).post(ask_privacy_set))
        .route("/v1/browse/episodes", get(browse_episodes))
        .route("/v1/browse/entities", get(browse_entities))
        .route("/v1/browse/identities", get(browse_identities))
        .route("/v1/browse/edges", get(browse_edges))
        .route("/v1/browse/sources", get(browse_sources))
        .route("/v1/sources/virtual", post(register_virtual))
        .route("/v1/sources/virtual/{name}/search", get(search_virtual))
        .route("/v1/sources/virtual/{name}/query", get(query_virtual))
        .route("/v1/sources/virtual/{name}/promote", post(promote_virtual))
        .route(
            "/v1/integrations/hearthfs",
            get(integrations_hearthfs_status).post(integrations_hearthfs_set),
        )
        .route("/v1/inbox/upload", post(inbox_upload))
        .route(
            "/v1/integrations/anthropic",
            get(anthropic_status).post(anthropic_save),
        )
        .route(
            "/v1/integrations/extraction",
            get(extraction_status).post(extraction_set),
        )
        .route(
            "/v1/integrations/gmail",
            get(gmail_status).post(gmail_save),
        )
        .route(
            "/v1/integrations/embeddings",
            get(embeddings_status).post(embeddings_download),
        )
        .route("/v1/ingest/discover-git", get(discover_git))
        .route("/v1/health/today", get(health_today))
        .route(
            "/v1/integrations/openai/estimate",
            get(openai_estimate),
        )
        .route("/v1/integrations/imports", get(imports_status))
        .route("/v1/integrations/downloads", get(downloads_scan))
        .route("/v1/integrations/downloads/place", post(downloads_place))
        .route("/v1/keys", get(keys_list).post(keys_create))
        .route("/v1/keys/{id}/revoke", post(keys_revoke))
        .route("/v1/keys/{id}/activity", get(key_activity))
        .route("/v1/keys/{id}/undo", post(key_undo))
        .route("/v1/keys/request", post(key_request_create))
        .route("/v1/keys/request/{id}", get(key_request_poll))
        .route("/v1/keys/requests", get(key_requests_list))
        .route("/v1/keys/requests/{id}", post(key_request_decide))
        .route("/v1/ingest/capture", post(ingest_capture))
        .route("/v1/portrait", get(portrait))
        .route("/v1/keys/self-revoke", post(keys_self_revoke))
        .route("/v1/auth/me", get(auth_me))
        .route("/v1/auth/bootstrap", post(auth_bootstrap))
        .route("/v1/auth/login", post(auth_login))
        .route("/v1/auth/logout", post(auth_logout))
        .route("/v1/auth/session-from-key", post(auth_session_from_key))
        .route("/v1/sync/episodes", get(sync_episodes))
        .route(
            "/v1/peer",
            get(peer_status).post(peer_pair).delete(peer_unpair),
        )
        .route("/v1/peer/sync-now", post(peer_sync_now))
        .route("/v1/peer/heartbeat", post(peer_heartbeat))
        .route("/v1/peer/pubkey", get(peer_pubkey))
        .route("/v1/sync/telemetry", get(sync_telemetry_get).post(sync_telemetry_post))
        .route("/v1/sync/llm_usage", get(sync_llm_usage_get).post(sync_llm_usage_post))
        .route("/v1/sync/locations", get(sync_locations_get).post(sync_locations_post))
        .route("/v1/sync/movements", get(sync_movements_get).post(sync_movements_post))
        .route("/v1/sync/vehicles", get(sync_vehicles_get).post(sync_vehicles_post))
        .route("/v1/sync/contacts", get(sync_contacts_get).post(sync_contacts_post))
        .route(
            "/v1/sync/device_state",
            get(sync_device_state_get).post(sync_device_state_post),
        )
        .route("/v1/monitor/summary", get(monitor_summary))
        .route(
            "/v1/settings/automations",
            get(settings_automations).post(settings_automation_set),
        )
        .route(
            "/v1/settings/automations/{key}/run",
            post(settings_automation_run),
        )
        .route(
            "/v1/settings/telemetry",
            get(settings_telemetry).post(settings_telemetry_set),
        )
        .route("/v1/settings/telemetry/test", post(settings_telemetry_test))
        .route("/v1/monitor/health", get(monitor_health))
        .route("/v1/monitor/logs", get(monitor_logs))
        .route("/v1/llm/usage", get(llm_usage_view))
        .route("/v1/llm/spend", get(llm_spend_view))
        .route("/v1/llm/budget", post(llm_budget_set))
        .route("/v1/peer/pause", post(peer_pause))
        .route("/v1/sync/declared", post(sync_declared))
        .route("/v1/sync/scopes", get(sync_scopes_get).post(sync_scopes_post))
        .route("/v1/sync/operational", get(sync_operational_get).post(sync_operational))
        .route("/v1/sync/paused", post(sync_paused_informed))
        .route("/v1/bus/stream", get(bus_stream))
        .route("/v1/bus/publish", post(bus_publish))
        .route("/v1/onboarding", get(onboarding_get).post(onboarding_set))
        .route("/v1/export/start", post(export_start))
        .route("/v1/export/status", get(export_status))
        .route("/v1/export/download", get(export_download))
        // restore uploads stream to disk and can be many GB — no body cap
        .route(
            "/v1/import/restore",
            post(import_restore).layer(axum::extract::DefaultBodyLimit::disable()),
        )
        .route("/v1/notifications", get(notifications_list).post(notification_create))
        .route("/v1/notifications/read-all", post(notifications_read_all))
        .route("/v1/notifications/{id}/read", post(notification_read))
        .route("/v1/notifications/{id}/dismiss", post(notification_dismiss))
        .route("/v1/provisioner/killswitch", post(provisioner_killswitch))
        .route("/v1/provisioner/notify", post(provisioner_notify))
        .route("/v1/provisioner/link-domains", post(provisioner_link_domains))
        .route("/v1/provisioner/status", get(provisioner_status))
        .route("/v1/sync/notifications", get(sync_notifications_get).post(sync_notifications_post))
        .route("/v1/sync/extractions", get(sync_extractions_get).post(sync_extractions_post))
        .route("/v1/sync/journal", get(sync_journal_get).post(sync_journal_post))
        .route("/v1/sync/photos", get(sync_photos_get).post(sync_photos_post))
        .route("/v1/sync/sharing", get(sync_sharing_get).post(sync_sharing_post))
        .route("/v1/integrations/llm-resume", post(llm_resume))
        .route("/v1/integrations/extraction/rerun", post(extraction_rerun))
        .route("/v1/integrations/extraction/retry-failed", post(extraction_retry_failed))
        // plugins: lifecycle (session-authed; plugin keys denied by prefix)
        .route("/v1/plugins", get(plugins_list))
        .route("/v1/plugins/inspect", get(plugins_inspect))
        .route("/v1/plugins/install", post(plugins_install))
        .route("/v1/plugins/upload", post(plugins_upload))
        .route("/v1/plugins/ui", get(plugins_ui))
        .route("/v1/plugins/{name}/enable", post(plugins_toggle))
        .route("/v1/plugins/{name}/ui-token", post(plugin_ui_token))
        .route("/v1/plugins/{name}/config", get(plugin_config_get).post(plugin_config_set))
        .route("/v1/plugins/{name}/oauth", get(plugin_oauth_get))
        .route("/v1/plugins/{name}", axum::routing::delete(plugins_uninstall))
        // plugins: self storage (plugin keys only — always allowed by the table)
        .route("/v1/plugins/self/kv", get(plugin_kv_list))
        .route(
            "/v1/plugins/self/kv/{key}",
            get(plugin_kv_get).put(plugin_kv_put).delete(plugin_kv_delete),
        )
        .route("/v1/plugins/self/secrets", get(plugin_secret_list))
        .route(
            "/v1/plugins/self/secrets/{key}",
            get(plugin_secret_get).put(plugin_secret_put).delete(plugin_secret_delete),
        )
        .route("/v1/plugins/self/oauth/pending", post(plugin_oauth_pending))
        .route("/v1/plugins/self/oauth/take", post(plugin_oauth_take))
        .route("/v1/plugins/self/files", get(plugin_file_list))
        .route(
            "/v1/plugins/self/files/{*path}",
            get(plugin_file_get).put(plugin_file_put).delete(plugin_file_delete),
        )
        // plugins: peer awareness (what runs on the counterpart)
        .route("/v1/sync/plugins/installed", get(sync_plugins_installed))
        .route("/v1/plugins/peer", get(plugins_peer))
        // plugins: the registry gallery (session-authed; policy in registry_client)
        .route("/v1/plugins/registry/index", get(plugins_registry_index))
        .route("/v1/plugins/registry/stage", post(plugins_registry_stage))
        .route("/v1/version", get(version_get))
        .route("/v1/upgrade", post(upgrade_post))
        // plugins: the peering leg (peer scope)
        .route("/v1/sync/plugins", get(sync_plugins_get).post(sync_plugins_post))
        .route("/v1/sync/plugins/file", get(sync_plugin_file_get))
        // plugins: sandboxed UI assets
        .route("/plugins/{name}/ui/{*path}", get(plugin_ui_asset))
        // plugins: the OAuth redirect address an ember registers with a provider (pre-auth)
        .route("/plugins/{name}/oauth/callback", get(plugin_oauth_callback))
        // plugins: public files an ember declared in its manifest (pre-auth)
        .route("/.well-known/{*path}", get(well_known_asset))
        .route("/v1/network/hello", get(network_hello))
        .route("/v1/network/search", get(network_search))
        .route("/v1/network/fetch", get(network_fetch))
        .route("/v1/network/profile", get(network_profile))
        .route("/v1/network/tasks", get(network_tasks_get))
        .route("/v1/network/tasks/{id}/toggle", post(network_task_toggle))
        .route("/v1/network/relationships", get(network_relationships_get))
        .route("/v1/network/notes", get(network_notes_get))
        .route("/v1/network/note/{id}", get(network_note_get))
        .route("/v1/network/offer", get(network_offer))
        .route("/v1/network/shared", get(network_shared_get))
        .route("/v1/network/groups", get(network_groups_get))
        .route("/v1/groups", get(groups_list).post(groups_mutate))
        .route("/v1/network/mycard", get(mycard_get).post(mycard_set))
        .route("/v1/grants/default", get(grants_default_get).post(grants_default_set))
        .route("/v1/grants/sharing", post(grants_sharing_set))
        .route("/v1/sharing", get(sharing_list).post(sharing_mutate))
        .route("/v1/friends/shared", get(friends_shared))
        .route("/v1/friends/{name}/tasks/{id}/toggle", post(friends_task_toggle))
        .route("/v1/friends/{name}/fork", post(friends_fork))
        .route("/v1/friends/{name}/refresh-card", post(friends_refresh_card))
        .route("/v1/friends/{name}/offer", get(friends_offer))
        .route("/v1/friends", get(friends_list).post(friend_connect))
        .route("/v1/friends/{name}", axum::routing::delete(friend_remove))
        .route("/v1/grants", get(grants_list).post(grant_set))
        .route("/v1/grants/{friend}/revoke", post(grant_revoke))
        .route("/v1/grants/{friend}", axum::routing::delete(grant_delete))
        .route("/v1/network/accesses", get(network_accesses))
        .route("/api/discovery/manifest", get(discovery_manifest))
        .route("/brain/{*path}", get(brain_tree))
        .route("/v1/vault", get(vault_status).post(vault_set))
        .route("/v1/privacy/rules", get(privacy_rules_list).post(privacy_rules_add))
        .route("/v1/notes/vaulted", get(vaulted_notes))
        .route("/v1/privacy/rules/{id}", axum::routing::delete(privacy_rules_remove))
        .route("/v1/intents", get(list_intents).post(add_intent))
        .route("/v1/intents/{name}", post(update_goal).delete(remove_intent))
        .route("/v1/intents/{name}/matches", get(intent_matches))
        .route("/v1/intents/{name}/queries", post(goal_queries))
        .route("/v1/selfmodel", get(selfmodel_list).post(selfmodel_add))
        .route("/v1/selfmodel/{id}/resolve", post(selfmodel_resolve))
        .route("/v1/opportunities", get(opportunities_list))
        .route("/v1/opportunities/{id}/resolve", post(opportunity_resolve))
        .route("/v1/lint", get(lint_list))
        .route("/v1/lint/{id}/resolve", post(lint_resolve))
        .route("/v1/tasks", get(tasks_list).post(task_add))
        .route("/v1/tasks/{id}", post(task_update).delete(task_purge))
        .route("/v1/tasks/deleted", get(tasks_deleted))
        .route("/v1/tasks/{id}/complete", post(task_complete))
        .route("/v1/tasks/{id}/uncomplete", post(task_uncomplete))
        .route("/v1/tasks/{id}/restore", post(task_restore))
        .route("/v1/templates", get(templates_list).post(template_add))
        .route("/v1/notes", get(notes_list).post(note_add))
        .route("/v1/notes/origins", get(note_origins))
        .route("/v1/notes/tags", get(note_tags))
        .route("/v1/resolve", get(resolve_citations))
        .route("/v1/notes/scope", post(notes_scope_by_origin))
        .route("/v1/notes/{id}", get(note_get).post(note_update))
        .route("/v1/notes/{id}/status", post(note_status))
        .route("/v1/notes/{id}/versions", get(note_versions))
        .route("/v1/notes/{id}/versions/{entry}", get(note_version))
        .route("/v1/metrics", get(metrics_query))
        .route("/v1/exercises/recent", get(exercises_recent))
        .route("/v1/relationships", get(relationships_get).post(relationship_add))
        .route("/v1/entities/similar", get(entities_similar))
        .route("/v1/projects", get(projects_list).post(project_declare))
        .route("/v1/entities/rename", post(entity_rename))
        .route("/v1/projects/{id}/detail", get(project_detail))
        .route("/v1/projects/{id}/attach", post(project_attach))
        .route("/v1/projects/{id}/detach", post(project_detach))
        .route("/v1/goals/project", post(goal_link_project))
        .route("/v1/notes/{id}/primary", post(note_set_primary))
        .route("/v1/galaxy", get(galaxy_get))
        .route("/v1/connections", get(connections_get))
        .route("/v1/connections/materialize", post(connection_materialize))
        .route("/v1/relationships/{id}/resolve", post(relationship_resolve))
        .route("/mcp", post(mcp::handle))
        .route("/mcp/governance", post(agents_governance_mcp))
        .route("/v1/agents", get(agents_view).post(agents_mutate))
        .route("/v1/nodes", get(nodes_view).post(nodes_set))
        .route("/v1/agents/policy", post(agents_policy_set))
        .route("/v1/agents/approvals", get(agents_approvals))
        .route("/v1/agents/approvals/{id}", post(agents_approval_decide))
        .route("/v1/agents/tasks/{id}", get(agents_task_get))
        .route("/v1/agents/tasks/{id}/cancel", post(agents_task_cancel))
        .route("/v1/agents/tasks/{id}/report", post(agents_task_report))
        .route("/v1/agents/hook/{agent_id}", post(agents_hook))
        .route("/v1/sync/agents", get(sync_agents_get).post(sync_agents_post))
        .route("/v1/sync/retirements", get(sync_retirements_get).post(sync_retirements_post))
        .route("/v1/retire/preview", get(retire_preview))
        .route("/v1/retire", post(retire_post))
        .route("/v1/retirements", get(retirements_list))
        .route("/v1/retirements/{id}/undo", post(retirement_undo))
        .route("/v1/sync/agents/adopt", post(sync_agents_adopt))
        .route("/v1/sync/agents/release", post(sync_agents_release))
        .route("/v1/sync/agents/released", post(sync_agents_released))
        // Ingest batches carry raw episode bodies; the axum default (2MB)
        // rejects real archives.
        .layer(axum::extract::DefaultBodyLimit::max(256 * 1024 * 1024))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        // Outermost (added last): gates BEFORE auth_middleware's own
        // store reads, so ungated requests can't park workers there.
        .layer(axum::middleware::from_fn(store_gate))
        // Optional APM (apm.rs — inert without a DSN + user consent):
        // a transaction per request named by the axum route template,
        // continuing any sentry-trace/baggage headers the iOS app sent
        // (that continuation IS the phone→daemon end-to-end stitch).
        // The hub layer must wrap the transaction layer, so it is
        // added after (= outside).
        .layer(sentry::integrations::tower::SentryHttpLayer::new().enable_transaction())
        .layer(sentry::integrations::tower::NewSentryLayer::new_from_top())
        // CORS, credential-less and BEARER-ONLY: sandboxed plugin iframes
        // have an opaque origin, so their Bearer fetches are cross-origin.
        // `*` without Allow-Credentials means browsers NEVER attach the
        // session cookie cross-origin; keyless requests get no grant at
        // all (cors_bearer + THE BROWSER RULE in auth_middleware).
        .layer(axum::middleware::from_fn(cors_bearer))
        .with_state(state)
}

async fn cors_bearer(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::http::{header, HeaderValue, Method, StatusCode};
    // Cross-origin access is granted to BEARER callers only (sandboxed
    // plugin frames, other apps holding a key): a preflight for a request
    // that won't carry an Authorization header gets no grant, and a
    // keyless response carries no Allow-Origin — a web page can neither
    // call a node keyless nor read its answer (THE BROWSER RULE).
    if req.method() == Method::OPTIONS {
        let wants_bearer = req
            .headers()
            .get(header::ACCESS_CONTROL_REQUEST_HEADERS)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.split(',').any(|h| h.trim().eq_ignore_ascii_case("authorization")));
        if !wants_bearer {
            return StatusCode::NO_CONTENT.into_response();
        }
        return (
            StatusCode::NO_CONTENT,
            [
                (header::ACCESS_CONTROL_ALLOW_ORIGIN, "*"),
                (header::ACCESS_CONTROL_ALLOW_METHODS, "GET, POST, PUT, DELETE"),
                (header::ACCESS_CONTROL_ALLOW_HEADERS, "authorization, content-type"),
                (header::ACCESS_CONTROL_MAX_AGE, "600"),
            ],
        )
            .into_response();
    }
    let bearer = req.headers().contains_key(header::AUTHORIZATION);
    let mut res = next.run(req).await;
    if bearer {
        res.headers_mut()
            .insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*"));
    }
    res
}

/// Worker-exhaustion gate (THE PASS CONTRACT's HTTP-side half): store
/// handlers block runtime workers on the store lock INLINE, so with N
/// workers, N concurrent store requests park the whole runtime — the
/// IO driver goes unpolled and even store-free routes (/health) stop
/// answering. Cap concurrent store-touching requests below the worker
/// count; the excess queue HERE, asynchronously, off the workers.
/// Streaming routes are exempt (they hold a slot for their lifetime
/// but await channels, never the store): the bus SSE stream and the
/// RLM query stream.
async fn store_gate(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    static GATE: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);
    let path = req.uri().path();
    if path == "/health" || path == "/v1/bus/stream" || path == "/v1/query/stream" {
        return next.run(req).await;
    }
    // Ignore the (impossible) closed-semaphore error rather than 500.
    let _permit = GATE.acquire().await;
    next.run(req).await
}

/// Verified bearer claims for this request, if a key was presented.
/// Always inserted by `auth_middleware` so handlers can extract it.
#[derive(Clone)]
pub struct AuthInfo(pub Option<hearthloom_core::store::ApiKeyClaims>);

fn session_cookie(req: &axum::extract::Request) -> Option<String> {
    req.headers()
        .get(axum::http::header::COOKIE)?
        .to_str()
        .ok()?
        .split(';')
        .filter_map(|part| part.trim().strip_prefix("hl_session="))
        .next()
        .map(String::from)
}

/// Routes an agent harness calls back on with a token of its own.
fn is_agent_callback(path: &str) -> bool {
    path == "/mcp/governance"
        || (path.starts_with("/v1/agents/tasks/") && path.ends_with("/report"))
        || path.starts_with("/v1/agents/hook/")
}

/// A key whose every scope is `friend` — what connecting a friend mints.
/// (A key the owner deliberately gave other scopes as well is theirs.)
fn is_friend_only_key(scopes: &[String]) -> bool {
    !scopes.is_empty() && scopes.iter().all(|s| s == "friend")
}

/// THE FRIEND CEILING's route table — default-deny. Everything a
/// friend's node calls here (network_out.rs on their side), plus the
/// door that lets them retire their own key. A new friend-surface route
/// must be added HERE or friends cannot reach it; the owner-side
/// `/v1/network/mycard` and `/v1/network/accesses` are deliberately not.
fn friend_route_allowed(method: &str, path: &str) -> bool {
    const READS: &[&str] = &[
        "/v1/network/hello",
        "/v1/network/search",
        "/v1/network/fetch",
        "/v1/network/profile",
        "/v1/network/tasks",
        "/v1/network/relationships",
        "/v1/network/notes",
        "/v1/network/offer",
        "/v1/network/shared",
        "/v1/network/groups",
    ];
    let single_segment = |rest: &str| !rest.is_empty() && !rest.contains('/');
    match method {
        "GET" => {
            path == "/health"
                || READS.contains(&path)
                || path.strip_prefix("/v1/network/note/").is_some_and(single_segment)
        }
        "POST" => {
            path == "/v1/keys/self-revoke"
                || path
                    .strip_prefix("/v1/network/tasks/")
                    .and_then(|rest| rest.strip_suffix("/toggle"))
                    .is_some_and(single_segment)
        }
        _ => false,
    }
}

/// THE KEY CEILING (0.5.71): what an ORDINARY key reaches — one that is
/// not the owner's (`is_owner_key`: an admin key or the paired phone), not
/// a friend's, not bound to a plugin and minted without grants (those
/// have ceilings of their own). Default-deny: a route is owner-only for
/// such keys until it is listed here. Rows match the ROUTER'S TEMPLATE
/// (axum `MatchedPath`), so a static route like /v1/notes/vaulted is never
/// mistaken for /v1/notes/{id}. `*` = any key.
const KEY_ROUTES: &[(&str, &str, &[&str])] = &[
    // Any key: liveness, version, who am I, retiring itself.
    ("GET", "/health", &["*"]),
    ("GET", "/v1/version", &["*"]),
    ("GET", "/v1/auth/me", &["*"]),
    ("POST", "/v1/keys/self-revoke", &["*"]),
    // read — the record, as its viewer may see it (never private: a key's
    // vault view stays locked unless that key unlocked it, and only the
    // owner's keys can).
    ("GET", "/v1/stats", &["read"]),
    ("GET", "/v1/ribbons", &["read"]),
    ("GET", "/v1/atlas/points", &["read"]),
    ("GET", "/v1/timeline/histogram", &["read"]),
    ("GET", "/v1/timeline/notable", &["read"]),
    ("GET", "/v1/onthisday", &["read"]),
    ("POST", "/v1/constellation/evidence", &["read"]),
    ("GET", "/v1/search", &["read"]),
    ("GET", "/v1/recall", &["read"]),
    ("GET", "/v1/entity/{name}", &["read"]),
    ("GET", "/v1/traverse", &["read"]),
    ("GET", "/v1/ingest/status", &["read"]),
    ("GET", "/v1/ingest/recent", &["read"]),
    ("GET", "/v1/browse/episodes", &["read"]),
    ("GET", "/v1/browse/entities", &["read"]),
    ("GET", "/v1/browse/identities", &["read"]),
    ("GET", "/v1/browse/edges", &["read"]),
    ("GET", "/v1/browse/sources", &["read"]),
    ("GET", "/v1/sources/virtual/{name}/search", &["read"]),
    ("GET", "/v1/sources/virtual/{name}/query", &["read"]),
    ("GET", "/v1/health/today", &["read"]),
    ("GET", "/v1/portrait", &["read"]),
    ("GET", "/v1/intents", &["read"]),
    ("GET", "/v1/intents/{name}/matches", &["read"]),
    ("GET", "/v1/selfmodel", &["read"]),
    ("GET", "/v1/opportunities", &["read"]),
    ("GET", "/v1/tasks", &["read"]),
    ("GET", "/v1/tasks/deleted", &["read"]),
    ("GET", "/v1/templates", &["read"]),
    ("GET", "/v1/notes", &["read"]),
    ("GET", "/v1/notes/origins", &["read"]),
    ("GET", "/v1/notes/tags", &["read"]),
    ("GET", "/v1/resolve", &["read"]),
    ("GET", "/v1/notes/{id}", &["read"]),
    ("GET", "/v1/notes/{id}/versions", &["read"]),
    ("GET", "/v1/notes/{id}/versions/{entry}", &["read"]),
    ("GET", "/v1/metrics", &["read"]),
    ("GET", "/v1/exercises/recent", &["read"]),
    ("GET", "/v1/relationships", &["read"]),
    ("GET", "/v1/entities/similar", &["read"]),
    ("GET", "/v1/projects", &["read"]),
    ("GET", "/v1/projects/{id}/detail", &["read"]),
    ("GET", "/v1/galaxy", &["read"]),
    ("GET", "/v1/connections", &["read"]),
    ("GET", "/api/discovery/manifest", &["read"]),
    ("GET", "/brain/{*path}", &["read"]),
    ("POST", "/mcp", &["read", "mcp"]),
    // write — the Desk and the graph's curation, never labels or privacy.
    ("POST", "/v1/entity/{name}/importance", &["write"]),
    ("POST", "/v1/tasks", &["write"]),
    ("POST", "/v1/tasks/{id}", &["write"]),
    ("DELETE", "/v1/tasks/{id}", &["write"]),
    ("POST", "/v1/tasks/{id}/complete", &["write"]),
    ("POST", "/v1/tasks/{id}/uncomplete", &["write"]),
    ("POST", "/v1/tasks/{id}/restore", &["write"]),
    ("POST", "/v1/templates", &["write"]),
    ("POST", "/v1/notes", &["write"]),
    ("POST", "/v1/notes/{id}", &["write"]),
    ("POST", "/v1/notes/{id}/status", &["write"]),
    ("POST", "/v1/notes/{id}/primary", &["write"]),
    ("POST", "/v1/intents", &["write"]),
    ("POST", "/v1/intents/{name}", &["write"]),
    ("DELETE", "/v1/intents/{name}", &["write"]),
    ("POST", "/v1/intents/{name}/queries", &["write"]),
    ("POST", "/v1/relationships", &["write"]),
    ("POST", "/v1/relationships/{id}/resolve", &["write"]),
    ("POST", "/v1/projects", &["write"]),
    ("POST", "/v1/projects/{id}/attach", &["write"]),
    ("POST", "/v1/projects/{id}/detach", &["write"]),
    ("POST", "/v1/goals/project", &["write"]),
    ("POST", "/v1/entities/rename", &["write"]),
    ("POST", "/v1/connections/materialize", &["write"]),
    // ingest — device feeds and proposals (quarantined for a human).
    ("POST", "/v1/ingest", &["ingest", "peer"]),
    ("POST", "/v1/ingest/health", &["ingest"]),
    ("POST", "/v1/ingest/photos", &["ingest"]),
    ("POST", "/v1/ingest/contacts", &["ingest"]),
    ("POST", "/v1/ingest/calendar", &["ingest"]),
    ("POST", "/v1/ingest/reminders", &["ingest"]),
    ("POST", "/v1/ingest/location", &["ingest"]),
    ("POST", "/v1/ingest/motion", &["ingest"]),
    ("POST", "/v1/ingest/media", &["ingest"]),
    ("POST", "/v1/ingest/scan", &["ingest"]),
    ("POST", "/v1/ingest/focus", &["ingest"]),
    ("POST", "/v1/ingest/capture", &["ingest"]),
    // v60: spans of travel (a non-ember source names itself; embers meet
    // the plugin ceiling instead and may post only a vehicle's movement)
    ("POST", "/v1/ingest/movement", &["ingest"]),
    ("POST", "/v1/propose", &["ingest"]),
    // notify — ring the bell.
    ("POST", "/v1/notifications", &["notify"]),
    // peer — the other node of the same owner (sync.rs, bus.rs).
    ("POST", "/v1/peer/heartbeat", &["peer"]),
    ("GET", "/v1/peer/pubkey", &["peer"]),
    ("GET", "/v1/bus/stream", &["peer"]),
    ("POST", "/v1/bus/publish", &["peer"]),
    ("GET", "/v1/sync/episodes", &["peer"]),
    ("GET", "/v1/sync/telemetry", &["peer"]),
    ("POST", "/v1/sync/telemetry", &["peer"]),
    ("GET", "/v1/sync/llm_usage", &["peer"]),
    ("POST", "/v1/sync/llm_usage", &["peer"]),
    ("GET", "/v1/sync/locations", &["peer"]),
    ("POST", "/v1/sync/locations", &["peer"]),
    ("GET", "/v1/sync/contacts", &["peer"]),
    ("POST", "/v1/sync/contacts", &["peer"]),
    ("GET", "/v1/sync/movements", &["peer"]),
    ("POST", "/v1/sync/movements", &["peer"]),
    ("GET", "/v1/sync/vehicles", &["peer"]),
    ("POST", "/v1/sync/vehicles", &["peer"]),
    ("POST", "/v1/sync/declared", &["peer"]),
    ("GET", "/v1/sync/scopes", &["peer"]),
    ("POST", "/v1/sync/scopes", &["peer"]),
    ("GET", "/v1/sync/operational", &["peer"]),
    ("POST", "/v1/sync/operational", &["peer"]),
    ("POST", "/v1/sync/paused", &["peer"]),
    ("GET", "/v1/sync/notifications", &["peer"]),
    ("POST", "/v1/sync/notifications", &["peer"]),
    ("GET", "/v1/sync/extractions", &["peer"]),
    ("POST", "/v1/sync/extractions", &["peer"]),
    ("GET", "/v1/sync/journal", &["peer"]),
    ("POST", "/v1/sync/journal", &["peer"]),
    ("GET", "/v1/sync/photos", &["peer"]),
    ("POST", "/v1/sync/photos", &["peer"]),
    ("GET", "/v1/sync/sharing", &["peer"]),
    ("POST", "/v1/sync/sharing", &["peer"]),
    ("GET", "/v1/sync/plugins/installed", &["peer"]),
    ("GET", "/v1/sync/plugins", &["peer"]),
    ("POST", "/v1/sync/plugins", &["peer"]),
    ("GET", "/v1/sync/plugins/file", &["peer"]),
    ("GET", "/v1/sync/agents", &["peer"]),
    ("POST", "/v1/sync/agents", &["peer"]),
    ("GET", "/v1/sync/retirements", &["peer"]),
    ("POST", "/v1/sync/retirements", &["peer"]),
    ("POST", "/v1/sync/agents/adopt", &["peer"]),
    ("POST", "/v1/sync/agents/release", &["peer"]),
    ("POST", "/v1/sync/agents/released", &["peer"]),
    // friend — a key the owner gave friend scope AND others (a friend-only
    // key meets THE FRIEND CEILING first).
    ("GET", "/v1/network/hello", &["friend"]),
    ("GET", "/v1/network/search", &["friend"]),
    ("GET", "/v1/network/fetch", &["friend"]),
    ("GET", "/v1/network/profile", &["friend"]),
    ("GET", "/v1/network/tasks", &["friend"]),
    ("POST", "/v1/network/tasks/{id}/toggle", &["friend"]),
    ("GET", "/v1/network/relationships", &["friend"]),
    ("GET", "/v1/network/notes", &["friend"]),
    ("GET", "/v1/network/note/{id}", &["friend"]),
    ("GET", "/v1/network/offer", &["friend"]),
    ("GET", "/v1/network/shared", &["friend"]),
    ("GET", "/v1/network/groups", &["friend"]),
];

/// The scopes that reach a route (by method and router template); empty
/// when only the owner may call it.
fn key_route_scopes(method: &str, template: &str) -> &'static [&'static str] {
    let method = if method == "HEAD" { "GET" } else { method };
    KEY_ROUTES
        .iter()
        .find(|(m, t, _)| *m == method && *t == template)
        .map(|(_, _, scopes)| *scopes)
        .unwrap_or(&[])
}

/// May an ordinary key holding `scopes` call this route?
fn key_route_allowed(method: &str, template: &str, scopes: &[String]) -> bool {
    key_route_scopes(method, template)
        .iter()
        .any(|needed| *needed == "*" || scopes.iter().any(|s| s == needed))
}

#[cfg(test)]
mod write_actor_tests {
    use super::{write_actor, AuthInfo};
    use hearthloom_core::store::{ApiKeyClaims, KeyGrants};

    fn key(scopes: &[&str], grants: Option<KeyGrants>) -> AuthInfo {
        AuthInfo(Some(ApiKeyClaims {
            id: "ak_1".into(),
            name: "watch".into(),
            scopes: scopes.iter().map(|s| s.to_string()).collect(),
            grants,
        }))
    }

    #[test]
    fn test_who_a_write_is_recorded_under() {
        let claimed = || "human:dashboard".to_string();
        // The owner (session, admin key, the paired phone) keeps its claim.
        assert_eq!(write_actor(&AuthInfo(None), claimed()).actor, "human:dashboard");
        assert_eq!(write_actor(&key(&["admin"], None), claimed()).actor, "human:dashboard");
        assert_eq!(write_actor(&key(&["read", "ingest", "notify"], None), claimed()).actor, "human:dashboard");
        // Any other key is named by the server…
        assert_eq!(write_actor(&key(&["write"], None), claimed()).actor, "agent:key:watch");
        let capture = KeyGrants { capture: true, ..Default::default() };
        assert_eq!(write_actor(&key(&[], Some(capture.clone())), claimed()).actor, "agent:key:watch");
        // …unless the owner marked it as one of their own devices.
        let own = KeyGrants { own_device: true, ..capture };
        assert_eq!(write_actor(&key(&[], Some(own)), claimed()).actor, "human:device:watch");
    }

    #[test]
    fn test_the_phone_mints_only_scoped_keys() {
        use super::{device_mints_scoped_key, KeyCreateBody};
        let body = |v: serde_json::Value| serde_json::from_value::<KeyCreateBody>(v).unwrap();
        let capture_key = body(serde_json::json!({"actor": "x", "name": "capture", "grants": {"capture": true, "own_device": true}}));
        let admin_key = body(serde_json::json!({"actor": "x", "name": "evil", "scopes": ["admin"]}));
        let phone = key(&["read", "ingest", "notify"], None);
        assert!(device_mints_scoped_key(&phone, &capture_key));
        assert!(!device_mints_scoped_key(&phone, &admin_key), "never an ordinary key");
        // Keys that aren't the owner's never get the exception.
        assert!(!device_mints_scoped_key(&key(&["read"], None), &capture_key));
        let scoped = KeyGrants { capture: true, ..Default::default() };
        assert!(!device_mints_scoped_key(&key(&[], Some(scoped)), &capture_key));
    }
}

#[cfg(test)]
mod key_ceiling_tests {
    use super::{key_route_allowed, KEY_ROUTES};

    fn may(scopes: &[&str], method: &str, template: &str) -> bool {
        let scopes: Vec<String> = scopes.iter().map(|s| s.to_string()).collect();
        key_route_allowed(method, template, &scopes)
    }

    /// Every (method, template) the router registers, read from this file.
    fn router_routes() -> Vec<(String, String)> {
        let source = include_str!("http.rs");
        let body = source.split("pub fn router(").nth(1).unwrap().split("\n}\n").next().unwrap();
        let mut out = Vec::new();
        for line in body.lines().filter(|l| l.trim_start().starts_with(".route(\"")) {
            let rest = line.split_once(".route(\"").unwrap().1;
            let (template, handlers) = rest.split_once('"').unwrap();
            for method in ["get", "post", "put", "patch", "delete"] {
                let token = format!("{method}(");
                let found = handlers.match_indices(&token).any(|(i, _)| {
                    i == 0 || !handlers[..i].ends_with(|c: char| c.is_alphanumeric() || c == '_')
                });
                if found {
                    out.push((method.to_uppercase(), template.to_string()));
                }
            }
        }
        out
    }

    #[test]
    fn test_every_listed_route_exists() {
        let routes = router_routes();
        assert!(routes.len() > 200, "router parse found only {}", routes.len());
        for (method, template, _) in KEY_ROUTES {
            assert!(
                routes.iter().any(|(m, t)| m == method && t == template),
                "KEY_ROUTES lists {method} {template}, which the router doesn't register"
            );
        }
    }

    #[test]
    fn test_ember_pages_can_be_framed_only_by_the_dashboard() {
        // /plugins/ is static for the browser rule, so this header is what
        // keeps another site from framing an ember page and answering its
        // handshake as the parent window.
        assert_eq!(super::EMBER_PAGE_CSP, "frame-ancestors 'self'");
        assert!(axum::http::HeaderValue::from_str(super::EMBER_PAGE_CSP).is_ok());
        assert!(super::is_static_path("/plugins/x/ui/index.html"));
    }

    #[test]
    fn test_ordinary_keys_reach_only_their_scope() {
        // read: the record, nothing that manages the node.
        assert!(may(&["read"], "GET", "/v1/search"));
        assert!(may(&["read"], "GET", "/brain/{*path}"));
        assert!(may(&["read"], "POST", "/mcp") && may(&["mcp"], "POST", "/mcp"));
        assert!(!may(&["read"], "POST", "/v1/tasks"));
        // The escalations that made "a read key can do anything" true.
        for (method, template) in [
            ("POST", "/v1/keys"),
            ("GET", "/v1/keys"),
            ("POST", "/v1/vault"),
            ("POST", "/v1/scope"),
            ("POST", "/v1/notes/scope"),
            ("POST", "/v1/plugins/upload"),
            ("POST", "/v1/plugins/install"),
            ("POST", "/v1/upgrade"),
            ("POST", "/v1/export/start"),
            ("POST", "/v1/sources/virtual"),
            ("POST", "/v1/ingest/local"),
            ("POST", "/v1/friends"),
            ("POST", "/v1/grants"),
            ("POST", "/v1/sharing"),
            ("POST", "/v1/privacy/rules"),
            ("POST", "/v1/query"),
            ("GET", "/v1/ask/history"),
            ("GET", "/v1/events"),
            ("GET", "/v1/monitor/logs"),
            ("POST", "/v1/quarantine/{id}"),
            ("POST", "/v1/retire"),
            ("GET", "/v1/notes/vaulted"),
        ] {
            for scopes in [&["read"][..], &["write"], &["ingest"], &["notify"], &["peer"], &["mcp"], &["friend"],
                           &["read", "write", "ingest", "peer", "mcp", "friend"]] {
                assert!(!may(scopes, method, template), "{scopes:?} reached {method} {template}");
            }
        }
        // write, ingest, notify, peer: each its own surface.
        assert!(may(&["write"], "POST", "/v1/tasks") && !may(&["write"], "GET", "/v1/search"));
        assert!(may(&["ingest"], "POST", "/v1/ingest/health") && !may(&["ingest"], "POST", "/v1/tasks"));
        assert!(may(&["notify"], "POST", "/v1/notifications") && !may(&["notify"], "GET", "/v1/notifications"));
        assert!(may(&["peer"], "GET", "/v1/sync/journal") && may(&["peer"], "POST", "/v1/ingest"));
        assert!(!may(&["peer"], "GET", "/v1/search"));
        // Any key may retire itself and say hello; HEAD follows GET.
        assert!(may(&["notify"], "POST", "/v1/keys/self-revoke") && may(&["peer"], "HEAD", "/health"));
    }
}

#[cfg(test)]
mod browser_rule_tests {
    use super::{dashboard_host_ok, foreign_browser_request, is_static_path};
    use axum::http::HeaderMap;

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(*name, value.parse().unwrap());
        }
        map
    }

    #[test]
    fn test_only_pages_on_other_sites_are_foreign() {
        // The dashboard itself, typed URLs, and non-browser clients.
        assert!(!foreign_browser_request(&headers(&[("sec-fetch-site", "same-origin")])));
        assert!(!foreign_browser_request(&headers(&[("sec-fetch-site", "none")])));
        assert!(!foreign_browser_request(&headers(&[("host", "127.0.0.1:43278")])));
        assert!(!foreign_browser_request(&headers(&[
            ("origin", "http://127.0.0.1:43278"),
            ("host", "127.0.0.1:43278"),
        ])));
        // Another site, another port on the same machine, a sandboxed frame.
        assert!(foreign_browser_request(&headers(&[("sec-fetch-site", "cross-site")])));
        assert!(foreign_browser_request(&headers(&[("sec-fetch-site", "same-site")])));
        assert!(foreign_browser_request(&headers(&[
            ("origin", "https://evil.example"),
            ("host", "127.0.0.1:43278"),
        ])));
        assert!(foreign_browser_request(&headers(&[("origin", "null"), ("host", "127.0.0.1:43278")])));
    }

    #[test]
    fn test_dashboard_address_only() {
        for host in ["127.0.0.1:43278", "localhost:43278", "[::1]:43278", "localhost", "0.0.0.0:43278"] {
            assert!(dashboard_host_ok(&headers(&[("host", host)]), None), "{host}");
        }
        for host in ["evil.example:43278", "192.168.1.5:43278", "127.0.0.1.evil.example", ""] {
            assert!(!dashboard_host_ok(&headers(&[("host", host)]), None), "{host}");
        }
        // HEARTHLOOM_DASHBOARD_URL adds the owner's own name, any port, any case.
        assert!(dashboard_host_ok(&headers(&[("host", "Hearth.local:8080")]), Some("hearth.local")));
        assert!(!dashboard_host_ok(&headers(&[("host", "evil.example")]), Some("hearth.local")));
    }

    #[test]
    fn test_static_paths() {
        assert!(is_static_path("/") && is_static_path("/assets/app.js") && is_static_path("/health"));
        assert!(!is_static_path("/v1/keys") && !is_static_path("/mcp") && !is_static_path("/brain/x"));
    }
}

#[cfg(test)]
mod friend_ceiling_tests {
    use super::{friend_route_allowed, is_friend_only_key};

    #[test]
    fn test_friend_key_reaches_only_the_friend_surface() {
        // Everything a friend's node calls (network_out.rs) stays open…
        for (method, path) in [
            ("GET", "/v1/network/hello"),
            ("GET", "/v1/network/search"),
            ("GET", "/v1/network/fetch"),
            ("GET", "/v1/network/profile"),
            ("GET", "/v1/network/tasks"),
            ("GET", "/v1/network/relationships"),
            ("GET", "/v1/network/notes"),
            ("GET", "/v1/network/note/nt_123"),
            ("GET", "/v1/network/offer"),
            ("GET", "/v1/network/shared"),
            ("GET", "/v1/network/groups"),
            ("POST", "/v1/network/tasks/tk_123/toggle"),
            ("POST", "/v1/keys/self-revoke"),
            ("GET", "/health"),
        ] {
            assert!(friend_route_allowed(method, path), "{method} {path}");
        }
        // …and the owner's API does not, including the owner-side routes
        // that share the /v1/network prefix.
        for (method, path) in [
            ("GET", "/v1/tasks"),
            ("POST", "/v1/tasks"),
            ("POST", "/v1/vault"),
            ("GET", "/v1/keys"),
            ("POST", "/v1/keys"),
            ("GET", "/v1/search"),
            ("GET", "/v1/sharing"),
            ("POST", "/v1/sharing"),
            ("GET", "/v1/grants"),
            ("GET", "/v1/groups"),
            ("POST", "/v1/groups"),
            ("POST", "/v1/network/groups"),
            ("GET", "/v1/export/status"),
            ("POST", "/v1/query"),
            ("GET", "/v1/query/stream"),
            ("POST", "/mcp"),
            ("GET", "/brain"),
            ("GET", "/v1/bus/stream"),
            ("GET", "/v1/network/mycard"),
            ("POST", "/v1/network/mycard"),
            ("GET", "/v1/network/accesses"),
            ("POST", "/v1/network/search"),
            ("GET", "/v1/network/tasks/tk_123/toggle"),
            ("POST", "/v1/network/tasks//toggle"),
            ("POST", "/v1/network/tasks/a/b/toggle"),
            ("GET", "/v1/network/note/"),
            ("GET", "/v1/network/note/a/b"),
            ("GET", "/v1/network/hello/"),
            ("DELETE", "/v1/network/shared"),
            ("GET", "/"),
        ] {
            assert!(!friend_route_allowed(method, path), "{method} {path}");
        }
    }

    #[test]
    fn test_owner_gate_admits_only_the_owners_keys() {
        use super::is_owner_key;
        let scopes = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        // the owner: an admin key, a paired phone (read + ingest + notify)
        assert!(is_owner_key(&scopes(&["admin"]), false));
        assert!(is_owner_key(&scopes(&["read", "ingest", "notify"]), false));
        assert!(is_owner_key(&scopes(&["notify", "read", "write", "ingest"]), false));
        // everyone else
        for narrow in [
            &["read"][..], &["read", "write"], &["ingest"], &["notify"], &["read", "ingest"],
            &["peer"], &["mcp"], &["friend"], &["plugins"], &[],
        ] {
            assert!(!is_owner_key(&scopes(narrow), false), "{narrow:?}");
        }
        // a scoped (grants) key never is, whatever scopes ride along
        assert!(!is_owner_key(&scopes(&["read", "ingest", "notify"]), true));
        assert!(!is_owner_key(&scopes(&["admin"]), true));
    }

    #[test]
    fn test_only_pure_friend_keys_are_confined() {
        let scopes = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(is_friend_only_key(&scopes(&["friend"])));
        assert!(!is_friend_only_key(&scopes(&["friend", "read"])));
        assert!(!is_friend_only_key(&scopes(&["admin"])));
        assert!(!is_friend_only_key(&scopes(&["read", "ingest", "notify"])));
        assert!(!is_friend_only_key(&[]));
    }
}

/// Paths that must work BEFORE authentication: the app shell (which
/// renders the login screen), and the login door itself.
fn is_pre_auth_path(path: &str) -> bool {
    path == "/"
        || path.starts_with("/assets/")
        // plugin page shells: static chrome, same class as /assets/ —
        // their DATA still needs the scoped token the parent hands over
        || path.starts_with("/plugins/")
        // public files embers declare (manifest `publish`): a third-party
        // app's domain-verification key must answer without a session
        || path.starts_with("/.well-known/")
        || path == "/v1/auth/login"
        || path == "/v1/auth/me"
        || path == "/v1/auth/bootstrap"
        || path == "/health"
        // self-authenticating via X-Provisioner-Token (higher-than-admin;
        // the session gate must never run first — a provisioner requirement, 2026-07-17)
        || path.starts_with("/v1/provisioner/")
}

/// Static files only — the dashboard shell, its assets, plugin page
/// shells, the health probe. Everything else is API.
fn is_static_path(path: &str) -> bool {
    path == "/" || path == "/health" || path.starts_with("/assets/") || path.starts_with("/plugins/")
}

/// Did a web page on ANOTHER site make this request? Browsers say so in
/// Sec-Fetch-Site; older ones only send an Origin, which must then be this
/// host's own. Non-browser clients (CLI, curl, peers) send neither.
fn foreign_browser_request(headers: &axum::http::HeaderMap) -> bool {
    if let Some(site) = headers.get("sec-fetch-site").and_then(|v| v.to_str().ok()) {
        return matches!(site, "cross-site" | "same-site");
    }
    let Some(origin) = headers.get(axum::http::header::ORIGIN).and_then(|v| v.to_str().ok()) else {
        return false;
    };
    let host = headers.get(axum::http::header::HOST).and_then(|v| v.to_str().ok()).unwrap_or("");
    // "null" (a sandboxed frame) has no authority, so it never matches.
    origin.split_once("://").map(|(_, authority)| authority) != Some(host)
}

/// The host name of a `host[:port]` authority (IPv6 in brackets).
fn host_name(authority: &str) -> &str {
    match authority.strip_prefix('[') {
        Some(rest) => rest.split(']').next().unwrap_or(""),
        None => authority.rsplit_once(':').map_or(authority, |(name, _)| name),
    }
}

/// The host of HEARTHLOOM_DASHBOARD_URL — where the owner opens the
/// dashboard when it isn't on loopback (a LAN name, say). Unset: none.
fn configured_dashboard_host() -> Option<String> {
    let url = std::env::var("HEARTHLOOM_DASHBOARD_URL").ok()?;
    let rest = url.split_once("://").map_or(url.as_str(), |(_, rest)| rest);
    let name = host_name(rest.split('/').next().unwrap_or(""));
    (!name.is_empty()).then(|| name.to_ascii_lowercase())
}

/// Is the request addressed to the dashboard's own address? Loopback
/// names always (and the wildcard a local client may dial), plus the
/// configured dashboard host.
fn dashboard_host_ok(headers: &axum::http::HeaderMap, configured: Option<&str>) -> bool {
    let host = headers.get(axum::http::header::HOST).and_then(|v| v.to_str().ok()).unwrap_or("");
    let name = host_name(host);
    matches!(name, "127.0.0.1" | "localhost" | "::1" | "0.0.0.0" | "::")
        || configured.is_some_and(|c| name.eq_ignore_ascii_case(c))
}

/// Auth enforcement. Regimes, in order:
/// - Bearer presented â must verify (revoked/expired/unknown = 401, no
///   distinguishing which — don't oracle).
/// - Admin user exists (the bootstrap posture rule) â everything else
///   needs a live session cookie, except the pre-auth paths (app shell
///   and login). HEARTHLOOM_AUTH_OVERRIDE=open is the lost-password
///   escape hatch — requires shell access to ~/.hearthloom/.env, the
///   same trust as the disk itself.
/// - No admin (a grandfathered install, fresh dev trees) â open on
///   loopback as always, unless HEARTHLOOM_REQUIRE_KEYS=1.
/// - Integration surfaces (/api/discovery, /brain) require a PAT ALWAYS
///   (sessions don't reach them) — enforced in handlers via
///   `require_scope`.
async fn auth_middleware(
    State(state): State<AppState>,
    mut req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    // Agent callbacks carry their OWN tokens (a per-task token issued at
    // dispatch, or an agent's sealed hook token) — not API keys. Their
    // handlers verify them; nothing else is reachable this way.
    if is_agent_callback(req.uri().path()) {
        return next.run(req).await;
    }
    let token = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(String::from);
    let claims = match token {
        Some(token) => match state.store.verify_api_key(&token) {
            Ok(Some(claims)) => Some(claims),
            Ok(None) => {
                return ApiError(StatusCode::UNAUTHORIZED, "invalid API key".into())
                    .into_response()
            }
            Err(err) => return ApiError::from(err).into_response(),
        },
        None => None,
    };
    // Whose vault view this request reads with (the per-viewer vault): a
    // key reads as itself, a signed-in browser as its session, the keyless
    // caller of a node with no login as "local", anyone else locked.
    let mut viewer = match &claims {
        Some(c) => format!("key:{}", c.id),
        None => "anon".to_string(),
    };
    if claims.is_none() {
        let path = req.uri().path().to_string();
        let auth_override = std::env::var("HEARTHLOOM_AUTH_OVERRIDE")
            .map(|v| v.eq_ignore_ascii_case("open"))
            .unwrap_or(false);
        let locked =
            !auth_override && state.auth_locked.load(std::sync::atomic::Ordering::Relaxed);
        // THE BROWSER RULE (0.5.71): a KEYLESS request that a web page on
        // another site made is refused. On a node with no login it would
        // otherwise act as the owner (read everything, mint keys, unlock
        // the vault); with a session it would ride the cookie. Callers
        // that hold a key (plugin frames, other apps) are unaffected.
        if !is_static_path(&path) {
            if foreign_browser_request(req.headers()) {
                return ApiError(
                    StatusCode::FORBIDDEN,
                    "a request from another site needs an API key".into(),
                )
                .into_response();
            }
            // A node with no login answers keyless calls only at its
            // dashboard's address: loopback, or the host the owner set in
            // HEARTHLOOM_DASHBOARD_URL. A page whose domain resolves to
            // this machine (DNS rebinding) is same-origin with ITSELF, not
            // with that address.
            if !locked
                && !auth_override
                && !dashboard_host_ok(req.headers(), configured_dashboard_host().as_deref())
            {
                return ApiError(
                    StatusCode::FORBIDDEN,
                    "this node has no login, so it answers keyless calls only at its dashboard address (HEARTHLOOM_DASHBOARD_URL, by default 127.0.0.1 or localhost) — sign in or use an API key".into(),
                )
                .into_response();
            }
        }
        if locked && !is_pre_auth_path(&path) {
            let session = session_cookie(&req)
                .filter(|t| state.store.session_user(t).ok().flatten().is_some());
            match session {
                Some(token) => viewer = session_viewer(&token),
                None => {
                    return ApiError(StatusCode::UNAUTHORIZED, "sign in required".into())
                        .into_response()
                }
            }
        } else if !locked {
            viewer = "local".to_string();
            let require = std::env::var("HEARTHLOOM_REQUIRE_KEYS")
                .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                .unwrap_or(false);
            let is_ui = path == "/" || path.starts_with("/assets/") || path.starts_with("/.well-known/");
            if require && !is_ui {
                return ApiError(
                    StatusCode::UNAUTHORIZED,
                    "API keys are required (HEARTHLOOM_REQUIRE_KEYS is set)".into(),
                )
                .into_response();
            }
        }
    }
    // The killswitch gate (design locked 2026-07-16): while active, the
    // surface shrinks to the allowlist and everything else — UI and API
    // alike — answers 402 carrying the activator message + link. Sits
    // AFTER auth (a session is still established) and lets
    // /v1/provisioner/* through to its own higher-than-admin verifier.
    if state.killswitch.load(std::sync::atomic::Ordering::Relaxed)
        && !crate::provisioner::killswitch_allows(req.uri().path(), req.method())
    {
        return (
            StatusCode::PAYMENT_REQUIRED,
            Json(crate::provisioner::killswitch_body(&state.store)),
        )
            .into_response();
    }
    // THE PLUGIN CEILING: an ordinary key passes blanket auth on
    // unscoped routes (the trusted-integration posture, HearthFS-era),
    // but a key BOUND TO A PLUGIN reaches only its scope-mapped route
    // table — otherwise the install-time scope grant would be theater.
    let plugin_bound = claims
        .as_ref()
        .is_some_and(|c| crate::plugins::plugin_for_claims(&state, c).is_some());
    if let Some(c) = &claims {
        if plugin_bound
            && !crate::plugins::plugin_route_allowed(
                req.uri().path(),
                req.method().as_str(),
                &c.scopes,
            )
        {
            return ApiError(
                StatusCode::FORBIDDEN,
                "this plugin's granted scopes don't reach that surface".into(),
            )
            .into_response();
        }
    }
    // THE FRIEND CEILING (0.5.63): the key handed to a connected friend
    // reaches the friend surface and nothing else. Ordinary keys pass
    // blanket auth on routes that don't check a scope (the
    // trusted-integration posture above) — for a key that belongs to
    // ANOTHER PERSON that made the whole sharing matrix decorative: it
    // could read the owner API, write to it, and unlock the vault.
    if let Some(c) = &claims {
        if is_friend_only_key(&c.scopes)
            && c.grants.is_none()
            && !friend_route_allowed(req.method().as_str(), req.uri().path())
        {
            return ApiError(
                StatusCode::FORBIDDEN,
                "a connection's key reaches only what was shared with it".into(),
            )
            .into_response();
        }
    }
    // THE GRANT CEILING (v52): a SCOPED key — one minted with grants —
    // reaches only its default-deny route table (KeyGrants::permits),
    // never sees record content while the vault is open, and is held to
    // its own rate caps. Same reasoning as the plugin ceiling above:
    // without it, "this key may only add tasks" would be theater.
    if let Some(c) = &claims {
        if let Some(grants) = &c.grants {
            use hearthloom_core::store::GrantVerdict;
            let method = req.method().as_str().to_string();
            match grants.permits(&method, req.uri().path()) {
                GrantVerdict::Denied => {
                    return ApiError(
                        StatusCode::FORBIDDEN,
                        "this key's grants don't reach that surface".into(),
                    )
                    .into_response();
                }
                // No vault-open refusal any more: the vault is per viewer,
                // and a scoped key can never unlock (`require_owner`), so
                // its reads always run locked.
                GrantVerdict::Allowed { is_ask, .. } => {
                    // /mcp is exempt here — every MCP call is a POST, reads
                    // included; its commit path counts the write itself.
                    let is_write = method != "GET" && req.uri().path() != "/mcp";
                    if let Err(message) = scoped_key_rate_ok(&c.id, grants, is_write, is_ask) {
                        return ApiError(StatusCode::TOO_MANY_REQUESTS, message).into_response();
                    }
                }
            }
        }
    }
    // THE KEY CEILING (0.5.71): any other key that isn't the owner's
    // reaches only the routes its scopes are listed for (KEY_ROUTES) —
    // before this, a `notify` key could mint an admin key, install a
    // plugin or relabel private records.
    if let Some(c) = &claims {
        if c.grants.is_none()
            && !plugin_bound
            && !is_friend_only_key(&c.scopes)
            && !is_owner_key(&c.scopes, false)
        {
            let method = req.method().as_str().to_string();
            let template = req
                .extensions()
                .get::<axum::extract::MatchedPath>()
                .map(|m| m.as_str().to_string())
                .unwrap_or_default();
            if !key_route_allowed(&method, &template, &c.scopes) {
                let needs = key_route_scopes(&method, &template);
                let reach = if needs.is_empty() {
                    "only the owner can call it — sign in, or use an admin key or the paired phone".to_string()
                } else {
                    format!("it needs the {} scope", needs.join(" or "))
                };
                return ApiError(
                    StatusCode::FORBIDDEN,
                    format!("this key ({}) can't call {method} {}: {reach}", c.scopes.join(", "), req.uri().path()),
                )
                .into_response();
            }
        }
    }
    // Every request reads inside its scope — its viewer's vault view and,
    // for an area-limited key, its life areas — entered around every poll
    // (crate::scoped). Work it hands to another thread must use the
    // scoped spawns, or it reads locked.
    let areas = claims.as_ref().and_then(|c| c.grants.as_ref()).and_then(|g| g.areas.clone());
    let scope = hearthloom_core::store::ReadScope::new(&viewer, areas.as_deref());
    req.extensions_mut().insert(AuthInfo(claims));
    crate::scoped::Scoped::new(scope, next.run(req)).await
}

/// The vault viewer of a signed-in browser: its session, named by a
/// digest of the cookie (the token itself never sits in the unlock map).
fn session_viewer(token: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(token.as_bytes());
    let short: String = digest[..8].iter().map(|b| format!("{b:02x}")).collect();
    format!("session:{short}")
}

/// Per-key rate caps for scoped keys: writes per rolling hour, Ask runs
/// per UTC day. In memory — a restart forgets the counts (a cap, not a
/// ledger; the journal and the usage ledger are the durable record).
pub(crate) fn scoped_key_rate_ok(
    key_id: &str,
    grants: &hearthloom_core::store::KeyGrants,
    is_write: bool,
    is_ask: bool,
) -> Result<(), String> {
    use std::sync::{Mutex, OnceLock};
    use std::time::{Duration, Instant};
    #[derive(Default)]
    struct Counts {
        writes: std::collections::VecDeque<Instant>,
        ask_day: String,
        asks: u32,
    }
    static COUNTS: OnceLock<Mutex<std::collections::HashMap<String, Counts>>> = OnceLock::new();
    let mut all = COUNTS.get_or_init(Default::default).lock().unwrap();
    let counts = all.entry(key_id.to_string()).or_default();
    if is_ask {
        if let Some(cap) = grants.asks_per_day {
            let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
            if counts.ask_day != today {
                counts.ask_day = today;
                counts.asks = 0;
            }
            if counts.asks >= cap {
                return Err(format!("this key's limit of {cap} questions a day is used up"));
            }
            counts.asks += 1;
        }
    } else if is_write {
        if let Some(cap) = grants.writes_per_hour {
            let hour = Duration::from_secs(3600);
            while counts.writes.front().is_some_and(|t| t.elapsed() > hour) {
                counts.writes.pop_front();
            }
            if counts.writes.len() as u32 >= cap {
                return Err(format!("this key's limit of {cap} writes an hour is used up"));
            }
            counts.writes.push_back(Instant::now());
        }
    }
    Ok(())
}

/// Integration surfaces call this: a key with the scope, or 401/403.
fn require_scope(auth: &AuthInfo, scope: &str) -> Result<(), ApiError> {
    match &auth.0 {
        None => Err(ApiError(
            StatusCode::UNAUTHORIZED,
            "this surface requires an API key (mint one under â° â API Keys)".into(),
        )),
        Some(claims) if claims.scopes.iter().any(|s| s == scope || s == "admin") => Ok(()),
        Some(_) => Err(ApiError(
            StatusCode::FORBIDDEN,
            format!("this key lacks the {scope:?} scope"),
        )),
    }
}

/// The scopes pairing a phone mints (integrations.js). Holding all three
/// is what marks a key as the owner's own device.
const DEVICE_KEY_SCOPES: [&str; 3] = ["read", "ingest", "notify"];

/// Is this key the owner's own hand — an admin key, or a paired device?
/// Scoped keys (grants), plugin keys, and keys with a narrower scope set
/// (`read` alone, `peer`, `mcp`…) are not.
fn is_owner_key(scopes: &[String], has_grants: bool) -> bool {
    if has_grants {
        return false;
    }
    let has = |scope: &str| scopes.iter().any(|s| s == scope);
    has("admin") || DEVICE_KEY_SCOPES.iter().all(|s| has(s))
}

/// THE OWNER GATE (0.5.65): the acts only the person who owns this record
/// may perform — unlocking the vault, and turning a key into a signed-in
/// session (which can do everything). Allowed: a signed-in session (or
/// the open loopback of a node with no login), an admin key, a paired
/// device's key. Every other key is refused, whatever else it can reach.
/// A new route that reveals private material or grants the owner's reach
/// belongs behind this, not behind `require_scope`.
fn require_owner(auth: &AuthInfo, act: &str) -> Result<(), ApiError> {
    match &auth.0 {
        None => Ok(()),
        Some(claims) if is_owner_key(&claims.scopes, claims.grants.is_some()) => Ok(()),
        Some(_) => Err(ApiError(
            StatusCode::FORBIDDEN,
            format!(
                "this key can't {act} — only the owner can: sign in, or use an admin key or a paired device"
            ),
        )),
    }
}

/// v0 token stub: parse claims if a Bearer token is present. Loopback
/// needs none; when scoped expiring tokens arrive, enforcement lands here.
#[allow(dead_code)]
pub struct TokenClaims {
    pub token: Option<String>,
    pub scopes: Vec<String>,
}

pub fn claims(headers: &HeaderMap) -> TokenClaims {
    let token = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(String::from);
    TokenClaims {
        token,
        scopes: vec!["local".into()],
    }
}

/// Errors become JSON with a message, not bare 500 strings.
struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({"error": self.1}))).into_response()
    }
}

fn bad_request(msg: impl Into<String>) -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, msg.into())
}

impl From<anyhow::Error> for ApiError {
    fn from(err: anyhow::Error) -> Self {
        ApiError(StatusCode::INTERNAL_SERVER_ERROR, err.to_string())
    }
}

async fn health() -> Json<Value> {
    Json(json!({"status": "ok", "service": "hearthloom", "version": env!("CARGO_PKG_VERSION")}))
}

#[derive(Deserialize)]
struct AtlasParams {
    #[serde(default)]
    from: Option<String>,
    #[serde(default)]
    to: Option<String>,
}

/// Day-bucketed location clusters for the Atlas (vault-guarded in the
/// store via the episodes join).
async fn atlas_points(
    State(state): State<AppState>,
    Query(params): Query<AtlasParams>,
) -> Result<Json<Value>, ApiError> {
    let from = params.from.as_deref().unwrap_or("0000-01-01");
    let to = params.to.as_deref().unwrap_or("9999-12-31");
    let points = state.store.atlas_points(from, to, 20_000)?;
    Ok(Json(json!({"from": from, "to": to, "points": points})))
}

#[derive(Deserialize)]
struct HistogramParams {
    from: String,
    to: String,
    #[serde(default)]
    bucket: Option<String>,
    #[serde(default)]
    q: Option<String>,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    place: Option<String>,
}

/// Bucketed episode counts for the Dial (vault-guarded in the store).
async fn timeline_histogram(
    State(state): State<AppState>,
    Query(params): Query<HistogramParams>,
) -> Result<Json<Value>, ApiError> {
    let bucket = params.bucket.as_deref().unwrap_or("day");
    let rows = state.store.timeline_histogram(
        &params.from,
        &params.to,
        bucket,
        params.q.as_deref(),
        params.source.as_deref(),
        params.place.as_deref(),
    )?;
    let buckets: Vec<Value> = rows
        .into_iter()
        .map(|(b, n)| json!({"bucket": b, "n": n}))
        .collect();
    Ok(Json(json!({"bucket": bucket, "buckets": buckets})))
}

#[derive(Deserialize)]
struct NotableParams {
    from: String,
    to: String,
    #[serde(default)]
    bucket: Option<String>,
    #[serde(default)]
    k: Option<usize>,
    #[serde(default)]
    q: Option<String>,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    place: Option<String>,
}

/// Top-K notable episodes in a window — the Dial's label layer
/// (vault-guarded in the store; never more than K rows).
async fn timeline_notable(
    State(state): State<AppState>,
    Query(params): Query<NotableParams>,
) -> Result<Json<Value>, ApiError> {
    let notable = state.store.timeline_notable(
        &params.from,
        &params.to,
        params.bucket.as_deref().unwrap_or("day"),
        params.k.unwrap_or(16),
        params.q.as_deref(),
        params.source.as_deref(),
        params.place.as_deref(),
    )?;
    Ok(Json(json!({"notable": notable})))
}

#[derive(Deserialize)]
struct OnThisDayParams {
    /// The reference date (YYYY-MM-DD); results come from earlier years.
    date: String,
}

/// Memories resurfacing: what happened on this date in past years.
async fn on_this_day(
    State(state): State<AppState>,
    Query(params): Query<OnThisDayParams>,
) -> Result<Json<Value>, ApiError> {
    if params.date.len() < 10 {
        return Err(bad_request("date must be YYYY-MM-DD"));
    }
    let memories = state
        .store
        .on_this_day(&params.date[5..10], &params.date[..4], 16)?;
    Ok(Json(json!({"date": params.date, "memories": memories})))
}

#[derive(Deserialize)]
struct EvidenceBody {
    ids: Vec<String>,
    from: String,
    to: String,
}

/// Windowed evidence counts for constellation nodes (the "sky tonight"
/// signal): POST because the id list is long.
async fn constellation_evidence(
    State(state): State<AppState>,
    Json(body): Json<EvidenceBody>,
) -> Result<Json<Value>, ApiError> {
    let rows = state.store.entity_evidence_window(
        &body.ids[..body.ids.len().min(400)],
        &body.from,
        &body.to,
    )?;
    let map: serde_json::Map<String, Value> = rows
        .into_iter()
        .map(|(id, n, last)| (id, json!({"n": n, "last": last})))
        .collect();
    Ok(Json(json!({"evidence": map})))
}

#[derive(Deserialize)]
struct SearchParams {
    q: String,
    #[serde(default = "default_limit")]
    limit: usize,
}

fn default_limit() -> usize {
    10
}

async fn search(
    State(state): State<AppState>,
    Query(params): Query<SearchParams>,
) -> Result<Json<Value>, ApiError> {
    let embedding = state.query_embedding(&params.q);
    let hits =
        state
            .store
            .search_hybrid(&params.q, embedding.as_deref(), params.limit.min(100))?;
    Ok(Json(json!({
        "query": params.q,
        "hybrid": embedding.is_some(),
        "hits": hits,
    })))
}

#[derive(Deserialize)]
struct RecallParams {
    /// Day view: YYYY-MM-DD.
    on: Option<String>,
    /// Timespan: from/to (inclusive), YYYY-MM-DD.
    from: Option<String>,
    to: Option<String>,
    /// Entity recall.
    entity: Option<String>,
    /// summary (default) | full.
    #[serde(default = "default_detail")]
    detail: String,
    #[serde(default = "default_limit")]
    limit: usize,
}

fn default_detail() -> String {
    "summary".into()
}

async fn recall(
    State(state): State<AppState>,
    Query(params): Query<RecallParams>,
) -> Result<Json<Value>, ApiError> {
    if let Some(date) = &params.on {
        let day = state.store.recall_day(date)?;
        return Ok(Json(crate::views::day_view(&day, &params.detail)));
    }
    if let (Some(from), Some(to)) = (&params.from, &params.to) {
        let episodes = state.store.recall_span(from, to, params.limit.max(50))?;
        return Ok(Json(json!({
            "from": from, "to": to,
            "episodes": crate::views::episode_list(&episodes, &params.detail),
        })));
    }
    if let Some(name) = &params.entity {
        let episodes = state.store.recall_entity(name, params.limit.max(20))?;
        return Ok(Json(json!({
            "entity": name,
            "episodes": crate::views::episode_list(&episodes, &params.detail),
        })));
    }
    Err(bad_request("recall requires ?on=, ?from=&to=, or ?entity="))
}

async fn entity(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    match state.store.entity_dossier(&name)? {
        Some(dossier) => Ok(Json(serde_json::to_value(dossier).unwrap())),
        None => Err(ApiError(
            StatusCode::NOT_FOUND,
            format!("no entity named {name:?}"),
        )),
    }
}

async fn episode(
    State(state): State<AppState>,
    Path(id): Path<String>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
) -> Result<Json<Value>, ApiError> {
    // The owner's detail view is forensic: a retired or replaced memory
    // still opens, labelled. Every other caller reads live memories only.
    if require_owner(&auth, "").is_ok() {
        let Some((row, superseded, body)) = state.store.episode_forensic(&id)? else {
            return Err(ApiError(StatusCode::NOT_FOUND, format!("no episode {id:?}")));
        };
        let mut value = serde_json::to_value(row).unwrap();
        value["body"] = json!(body);
        if let Some(state_) = superseded {
            match state_.strip_prefix(hearthloom_core::store::RETIRED_PREFIX) {
                Some(rid) => value["retired_by"] = json!(rid),
                None => value["superseded_by"] = json!(state_),
            }
        }
        return Ok(Json(value));
    }
    let Some(row) = state.store.episode(&id)? else {
        return Err(ApiError(
            StatusCode::NOT_FOUND,
            format!("no episode {id:?}"),
        ));
    };
    let body = state.store.episode_body(&id)?;
    let mut value = serde_json::to_value(row).unwrap();
    value["body"] = json!(body);
    Ok(Json(value))
}

#[derive(Deserialize)]
struct TraverseParams {
    start: String,
    relation: Option<String>,
    #[serde(default = "default_depth")]
    depth: usize,
    as_of: Option<String>,
    /// Per-node significance cap (strongest edges first). Defaults to
    /// 25: an uncapped traverse over the full graph took 140s and held
    /// the store lock; pass top=0 explicitly for unlimited.
    #[serde(default)]
    top: Option<usize>,
}

fn default_depth() -> usize {
    1
}

async fn traverse(
    State(state): State<AppState>,
    Query(params): Query<TraverseParams>,
) -> Result<Json<Value>, ApiError> {
    let top = match params.top {
        Some(0) => None,
        Some(t) => Some(t),
        None => Some(25),
    };
    let hops = state.store.traverse_top(
        &params.start,
        params.relation.as_deref(),
        params.depth.min(4),
        params.as_of.as_deref(),
        top,
    )?;
    Ok(Json(json!({"start": params.start, "hops": hops})))
}

#[derive(Deserialize)]
struct EventsParams {
    #[serde(default)]
    since: i64,
    #[serde(default = "default_events_limit")]
    limit: usize,
}

fn default_events_limit() -> usize {
    100
}

async fn events(
    State(state): State<AppState>,
    Query(params): Query<EventsParams>,
) -> Result<Json<Value>, ApiError> {
    let events = state
        .store
        .mutations_since(params.since, params.limit.min(1000))?;
    let next_cursor = events.last().map(|e| e.seq).unwrap_or(params.since);
    Ok(Json(json!({"events": events, "next_cursor": next_cursor})))
}

async fn quarantine(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let pending = state.store.quarantine_pending(100)?;
    let resolved = state.store.resolved_memories(50)?;
    Ok(Json(json!({"pending": pending, "resolved": resolved})))
}

#[derive(Deserialize)]
struct QuarantineResolveBody {
    actor: String,
    /// accept | reject. Accepted proposals become agent-trust episodes
    /// (source `memory`, or a registered virtual source's promotion).
    resolution: String,
}

async fn resolve_quarantine(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<QuarantineResolveBody>,
) -> Result<Json<Value>, ApiError> {
    if body.actor.trim().is_empty() {
        return Err(bad_request("actor is required on every write"));
    }
    let accept = match body.resolution.as_str() {
        "accept" => true,
        "reject" => false,
        other => {
            return Err(bad_request(format!(
                "resolution must be accept|reject, got {other:?}"
            )))
        }
    };
    let actor = Actor::new(body.actor);
    // Fetch the entry before resolving so acceptance can promote it.
    let entry = state
        .store
        .quarantine_pending(1000)?
        .into_iter()
        .find(|entry| entry.id == id);
    state.store.resolve_quarantine(&actor, &id, accept)?;

    let mut episode_id = None;
    if accept {
        if let Some(entry) = entry {
            // A topic-page draft (the living-topic-pages pass) promotes
            // into the loom NOTE, not an episode: accept upserts by
            // (origin='loom', origin_ref=topic:<entity>) — a revision
            // updates the same hub note in place; it settles into the
            // record on the normal 48h-quiet path. Only the pass's own
            // drafts: outside proposals can't carry the kind or the ref
            // (`outside_proposal`), and this check covers older entries.
            if entry.kind == "topic_page" && entry.actor == "agent:consolidation" {
                let text = entry
                    .content
                    .get("text")
                    .and_then(|t| t.as_str())
                    .unwrap_or_default();
                let title = entry
                    .content
                    .get("title")
                    .and_then(|t| t.as_str())
                    .unwrap_or("Topic page");
                let origin_ref = entry
                    .content
                    .get("origin_ref")
                    .and_then(|t| t.as_str())
                    .ok_or_else(|| bad_request("topic_page proposal missing origin_ref"))?;
                // Stamp NOW so the accepted hub note surfaces as recent in
                // Notes — a dateless (NULL) loom note sorts OLDEST and hides
                // at the bottom (the "I can't find my approved memory" bug).
                let now = hearthloom_core::time::now_utc();
                state.store.import_note(
                    &actor,
                    &state.home,
                    title,
                    text,
                    &["topic".into()],
                    "personal",
                    "loom",
                    origin_ref,
                    Some((&now, &now)),
                )?;
                return Ok(Json(json!({
                    "id": id,
                    "resolution": body.resolution,
                    "note_upserted": true,
                    "landed": "note",
                    "title": title,
                })));
            }
            // Promotion: the accepted proposal becomes a real episode. A
            // plain proposed memory lands as source="memory" / agent trust.
            // A quarantined *virtual-source promotion* carries its own
            // provenance (source/source_ref) — honored only when the
            // promoting virtual source is registered here, so the origin
            // and idempotency key survive the human gate but no proposal
            // can land as a photo day, the digest or the owner's words.
            // Trust is always `agent`: a human accepting it doesn't make
            // it the owner's own.
            let text = entry
                .content
                .get("text")
                .and_then(|t| t.as_str())
                .map(String::from)
                .unwrap_or_else(|| entry.content.to_string());
            let field = |key: &str| entry.content.get(key).and_then(|v| v.as_str());
            let promoted = entry
                .content
                .pointer("/promoted_from/virtual_source")
                .and_then(|v| v.as_str())
                .filter(|name| field("source") == Some(*name))
                .is_some_and(|name| matches!(state.store.virtual_source(name), Ok(Some(_))));
            let (source, source_ref) = match (promoted, field("source_ref")) {
                (true, Some(source_ref)) => (field("source").unwrap_or("memory").to_string(), source_ref.to_string()),
                _ => ("memory".to_string(), entry.id.clone()),
            };
            let kind = if promoted || hearthloom_core::store::PROPOSAL_KINDS.contains(&entry.kind.as_str()) {
                entry.kind.clone()
            } else {
                "memory".to_string()
            };
            let trust = "agent".to_string();
            let title: String = field("title")
                .map(String::from)
                .unwrap_or_else(|| text.replace('\n', " ").chars().take(80).collect());
            let batch = hearthloom_core::IngestBatch {
                actor: actor.actor.clone(),
                session_id: None,
                episodes: vec![hearthloom_core::EpisodeDraft {
                    source: source.clone(),
                    source_ref: source_ref.clone(),
                    kind,
                    title,
                    body: text,
                    event_time: hearthloom_core::EventTime::unknown(),
                    trust,
                    scope: None,
                    entities: vec![],
                    edges: vec![],
                }],
            };
            state.store.apply_batch(&state.home, &batch)?;
            episode_id = Some(hearthloom_core::ids::episode_id(&source, &source_ref));
        }
    }
    Ok(Json(json!({
        "id": id,
        "resolution": body.resolution,
        "episode_id": episode_id,
    })))
}

async fn ingest(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::Extension(auth): axum::Extension<AuthInfo>,
    Json(mut batch): Json<IngestBatch>,
) -> Result<Json<Value>, ApiError> {
    let _claims = claims(&headers);
    if batch.actor.trim().is_empty() {
        return Err(bad_request("actor is required on every write"));
    }
    // A peer-scoped key identifies the SENDER by the name THIS node gave
    // the key at minting — provenance stamped by the receiver, so a
    // sender can't claim to be someone else.
    if let Some(claims) = &auth.0 {
        let peer = claims.scopes.iter().any(|s| s == "peer");
        if peer {
            batch.actor = format!("peer:{}", claims.name);
        } else if let Some(plugin) = crate::plugins::plugin_for_claims(&state, claims) {
            // same receiver-stamped provenance for plugins: a plugin
            // can't claim to be an adapter, a human, or another plugin
            batch.actor = format!("plugin:{plugin}");
        }
        // Any other key that isn't the owner's writes as an OUTSIDE source:
        // never under one of the record's own source names (a `#r` ref
        // there hides a real record; `consolidation` poses as the digest;
        // `desk`/`notes` read as the owner's declarations), and never as
        // the owner's own words.
        if !peer && !is_owner_key(&claims.scopes, claims.grants.is_some()) {
            if let Some(draft) = batch
                .episodes
                .iter()
                .find(|d| hearthloom_core::store::OWNER_ONLY_SOURCES.contains(&d.source.as_str()))
            {
                return Err(ApiError(
                    StatusCode::FORBIDDEN,
                    format!("source {:?} belongs to the record itself — only the owner's keys and peers write it", draft.source),
                ));
            }
            for draft in &mut batch.episodes {
                if draft.trust == "self" {
                    draft.trust = "imported".into();
                }
            }
        }
    }
    let report = state.store.apply_batch(&state.home, &batch)?;
    Ok(Json(serde_json::to_value(report).unwrap()))
}

#[derive(Deserialize)]
struct RegisterVirtualBody {
    actor: String,
    #[serde(default)]
    session_id: Option<String>,
    name: String,
    #[serde(default)]
    adapter: Option<String>,
    #[serde(default)]
    config: Value,
    #[serde(default)]
    capabilities: Vec<String>,
}

/// Register (or update) a virtual source: data kept at origin, queried at
/// need, surfaced as RLM read handles, materialized only on promotion.
async fn register_virtual(
    State(state): State<AppState>,
    Json(body): Json<RegisterVirtualBody>,
) -> Result<Json<Value>, ApiError> {
    if body.actor.trim().is_empty() {
        return Err(bad_request("actor is required on every write"));
    }
    if body.name.trim().is_empty() {
        return Err(bad_request("name is required"));
    }
    let actor = Actor {
        actor: body.actor,
        session_id: body.session_id,
    };
    let adapter = body.adapter.unwrap_or_else(|| body.name.clone());
    let capabilities = if body.capabilities.is_empty() {
        vec!["search".into(), "fetch".into(), "promote".into()]
    } else {
        body.capabilities
    };
    let config = if body.config.is_null() {
        json!({})
    } else {
        body.config
    };
    // Recursion guard: a HearthFS mount OF this brain (the /brain tree
    // serves a `.hearthloom-origin` marker) must never become a virtual
    // source — that's the brain indexing a window into itself.
    if let Some(mount) = config.get("mount").and_then(|m| m.as_str()) {
        let marker = crate::virtual_source::expand_tilde_path(mount).join(".hearthloom-origin");
        if marker.is_file() {
            return Err(bad_request(
                "that mount is a window into this brain (.hearthloom-origin found) — \
                 registering it would make the record index itself",
            ));
        }
    }
    state
        .store
        .register_virtual_source(&actor, &body.name, &adapter, &config, &capabilities)?;
    // Mail archives need their sidecar index; (re)build it off-request
    // when missing or stale (register again after replacing the archive).
    let mut indexing = false;
    if adapter == "mbox" {
        if let Ok(def) = crate::virtual_source::resolve(&state, &body.name) {
            if let Ok(mount) = crate::virtual_source::mount_path(&def) {
                if mount.is_file() && crate::mbox_index::is_stale(&mount) {
                    indexing = true;
                    crate::scoped::spawn_blocking(move || {
                        let started = std::time::Instant::now();
                        match crate::mbox_index::build(&mount) {
                            Ok(count) => tracing::info!(
                                "mbox index {}: DONE — {count} messages in {:.0?}",
                                mount.display(),
                                started.elapsed()
                            ),
                            Err(err) => tracing::error!(
                                "mbox index {}: FAILED: {err:#}",
                                mount.display()
                            ),
                        }
                    });
                }
            }
        }
    }
    Ok(Json(json!({
        "registered": body.name,
        "kind": "virtual",
        "indexing": indexing,
        "adapter": adapter,
        "capabilities": capabilities,
    })))
}

#[derive(Deserialize)]
struct VirtualSearchQuery {
    q: String,
    #[serde(default)]
    limit: Option<usize>,
}

/// Search a virtual source without materializing anything.
async fn search_virtual(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Query(params): Query<VirtualSearchQuery>,
) -> Result<Json<Value>, ApiError> {
    let def = crate::virtual_source::resolve(&state, &name)?;
    let hits = crate::virtual_source::search(&def, &params.q, params.limit.unwrap_or(20).min(100))?;
    Ok(Json(json!({"source": name, "query": params.q, "hits": hits})))
}

#[derive(Deserialize)]
struct VirtualQueryParams {
    sql: String,
    #[serde(default)]
    limit: Option<usize>,
}

/// Read-only SQL over a virtual source's numeric sidecar (vquery).
async fn query_virtual(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Query(params): Query<VirtualQueryParams>,
) -> Result<Json<Value>, ApiError> {
    // Daily-rollup series span a decade (~4k rows) — the Pulse and
    // year-clock views need them whole. The RLM's vquery handle keeps
    // its own tighter cap (120) at the call site.
    let (columns, rows) = crate::virtual_source::query(
        &state,
        &name,
        &params.sql,
        params.limit.unwrap_or(120).min(6000),
    )?;
    Ok(Json(json!({"source": name, "columns": columns, "rows": rows})))
}

#[derive(Deserialize)]
struct PromoteVirtualBody {
    actor: String,
    #[serde(default)]
    session_id: Option<String>,
    path: String,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    budget: Option<u32>,
}

/// Promote a virtual result into the substrate — materialize it as an
/// episode with provenance (or quarantine, for an untrusted source).
async fn promote_virtual(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<PromoteVirtualBody>,
) -> Result<Json<Value>, ApiError> {
    if body.actor.trim().is_empty() {
        return Err(bad_request("actor is required on every write"));
    }
    let actor = Actor {
        actor: body.actor,
        session_id: body.session_id,
    };
    let outcome = crate::virtual_source::promote(
        &state,
        &actor,
        &name,
        &body.path,
        body.kind.as_deref(),
        body.scope.as_deref(),
        body.budget,
    )?;
    Ok(Json(json!({
        "source": name,
        "path": body.path,
        "episode": outcome.episode_id,
        "quarantined": outcome.quarantined,
        "existing": outcome.existing,
    })))
}

async fn integrations_hearthfs_status(
    State(state): State<AppState>,
) -> Result<Json<Value>, ApiError> {
    let status = crate::integrations::hearthfs_status(&state)?;
    Ok(Json(serde_json::to_value(status).unwrap_or_default()))
}

#[derive(Deserialize)]
struct HearthfsSetBody {
    actor: String,
    /// "enable" | "disable"
    action: String,
    source_dir: Option<String>,
    port: Option<u16>,
    binary: Option<String>,
}

async fn integrations_hearthfs_set(
    State(state): State<AppState>,
    Json(body): Json<HearthfsSetBody>,
) -> Result<Json<Value>, ApiError> {
    if body.actor.trim().is_empty() {
        return Err(bad_request("actor is required on every write"));
    }
    let actor = Actor::new(body.actor);
    // launchctl + a settle sleep happen inside — off the async runtime.
    let status = crate::scoped::spawn_blocking(move || match body.action.as_str() {
        "enable" => {
            let source_dir = body
                .source_dir
                .ok_or_else(|| anyhow::anyhow!("source_dir is required to enable"))?;
            crate::integrations::hearthfs_enable(
                &state,
                &actor,
                &source_dir,
                body.port,
                body.binary.as_deref(),
            )
        }
        "disable" => crate::integrations::hearthfs_disable(&state, &actor),
        other => Err(anyhow::anyhow!("unknown action {other:?} (enable|disable)")),
    })
    .await
    .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, format!("integration task failed: {e}")))??;
    Ok(Json(serde_json::to_value(status).unwrap_or_default()))
}

#[derive(Deserialize)]
struct InboxUploadParams {
    filename: String,
}

/// Browser upload into the inbox drop folder — the web-UI equivalent of a
/// Finder drop; the watcher ingests it like any other arrival.
async fn inbox_upload(
    State(state): State<AppState>,
    Query(params): Query<InboxUploadParams>,
    body: axum::body::Bytes,
) -> Result<Json<Value>, ApiError> {
    let saved = crate::integrations::inbox_accept(&state.home, &params.filename, &body)
        .map_err(|e| bad_request(format!("{e:#}")))?;
    Ok(Json(json!({
        "saved": saved,
        "note": "landed in the inbox — the watcher will ingest it within a minute",
    })))
}

/// Full LLM config for the Integrations panel + wizard: the active
/// provider, every provider's key-configured status, and the active
/// tierâmodel map (DB config over provider defaults).
async fn anthropic_status(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let active = state
        .store
        .consolidation_state("llm.provider")?
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "anthropic".into());
    let defaults = hearthloom_core::provider::TierModels::defaults_for(&active);
    let model = |k: &str, d: &str| {
        state
            .store
            .consolidation_state(k)
            .ok()
            .flatten()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| d.to_string())
    };
    let base_url = crate::custom_base_url(&state.store);
    let providers: Vec<Value> = hearthloom_core::provider::PROVIDERS
        .iter()
        .map(|p| {
            // `custom` is configured by its address, not a key.
            let configured = if *p == "custom" {
                base_url.is_some()
            } else {
                crate::integrations::provider_configured(p)
            };
            json!({"name": p, "configured": configured})
        })
        .collect();
    let route = |tier: &str| {
        json!({
            "provider": model(&format!("llm.route.{tier}.provider"), ""),
            "model": model(&format!("llm.route.{tier}.model"), ""),
        })
    };
    Ok(Json(json!({
        "blocked": crate::consolidate::llm_blocked(&state.store).is_some(),
        "configured": crate::integrations::anthropic_configured(),
        "active": state.llm.is_some(),
        "active_provider": active,
        "providers": providers,
        "models": {
            "low": model("llm.model.low", &defaults.low),
            "medium": model("llm.model.medium", &defaults.medium),
            "high": model("llm.model.high", &defaults.high),
        },
        "defaults": {"low": defaults.low, "medium": defaults.medium, "high": defaults.high},
        // Node-local: the custom endpoint's address and per-tier routes.
        "base_url": base_url,
        "routes": {"low": route("low"), "medium": route("medium"), "high": route("high")},
        "efforts": {
            "low": model("llm.effort.low", ""),
            "medium": model("llm.effort.medium", ""),
            "high": model("llm.effort.high", ""),
        },
    })))
}

#[derive(Deserialize)]
struct RouteBody {
    #[serde(default)]
    provider: String,
    #[serde(default)]
    model: String,
}

#[derive(Deserialize)]
struct AnthropicSaveBody {
    actor: String,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    key: Option<String>,
    #[serde(default)]
    low: Option<String>,
    #[serde(default)]
    medium: Option<String>,
    #[serde(default)]
    high: Option<String>,
    /// The `custom` endpoint's OpenAI-compatible address.
    #[serde(default)]
    base_url: Option<String>,
    /// Per-tier provider overrides, keyed low|medium|high. A blank
    /// provider clears that tier's route.
    #[serde(default)]
    routes: Option<std::collections::BTreeMap<String, RouteBody>>,
    /// Per-tier effort overrides, keyed low|medium|high; blank clears.
    #[serde(default)]
    efforts: Option<std::collections::BTreeMap<String, String>>,
}

/// Validate + persist LLM config (provider, key, tier models), then restart
/// the daemon (launchd/Fly bring it back with the provider loaded). The
/// provider and its models are DB-stored (syncable); the key is sealed per
/// node; the custom endpoint address and per-tier routes are node-local.
async fn anthropic_save(
    State(state): State<AppState>,
    Json(body): Json<AnthropicSaveBody>,
) -> Result<Json<Value>, ApiError> {
    use hearthloom_core::provider::{normalize_base_url, Tier, PROVIDERS};
    if body.actor.trim().is_empty() {
        return Err(bad_request("actor is required on every write"));
    }
    let provider = body.provider.clone().unwrap_or_else(|| "anthropic".into());
    if !PROVIDERS.contains(&provider.as_str()) {
        return Err(bad_request(format!("unknown provider {provider}")));
    }
    let given = |v: &Option<String>| v.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(String::from);
    let key = given(&body.key);

    // Validate EVERYTHING before the first write — a half-saved config
    // followed by the restart below would strand the node.
    let routes: Vec<(Tier, String, String)> = {
        let mut out = Vec::new();
        for (tier, route) in body.routes.iter().flatten() {
            let tier = Tier::parse(tier)
                .ok_or_else(|| bad_request(format!("unknown tier {tier} (low|medium|high)")))?;
            let (name, model) = (route.provider.trim(), route.model.trim());
            if !name.is_empty() && !PROVIDERS.contains(&name) {
                return Err(bad_request(format!("unknown provider {name} for the {} tier", tier.as_str())));
            }
            if name == "custom" && model.is_empty() {
                return Err(bad_request(format!(
                    "the {} tier is routed to the custom endpoint — name the model it should use",
                    tier.as_str()
                )));
            }
            out.push((tier, name.to_string(), model.to_string()));
        }
        out
    };
    let efforts: Vec<(Tier, String)> = {
        let mut out = Vec::new();
        for (tier, level) in body.efforts.iter().flatten() {
            let tier = Tier::parse(tier)
                .ok_or_else(|| bad_request(format!("unknown tier {tier} (low|medium|high)")))?;
            let level = level.trim();
            if !level.is_empty() && !hearthloom_core::provider::Efforts::LEVELS.contains(&level) {
                return Err(bad_request(format!("unknown effort {level} (low|medium|high|xhigh|max)")));
            }
            out.push((tier, level.to_string()));
        }
        out
    };
    let uses_custom = provider == "custom" || routes.iter().any(|(_, name, _)| name == "custom");
    let base_url = match given(&body.base_url) {
        Some(raw) => Some(normalize_base_url(&raw).map_err(|e| bad_request(format!("{e:#}")))?),
        None => None,
    };
    if uses_custom {
        let Some(url) = base_url.clone().or_else(|| crate::custom_base_url(&state.store)) else {
            return Err(bad_request(
                "the custom endpoint needs a base URL (e.g. http://localhost:11434/v1)",
            ));
        };
        let custom_key = if provider == "custom" { key.clone() } else { None }
            .or_else(|| std::env::var(crate::integrations::provider_key_env("custom")).ok())
            .unwrap_or_default();
        crate::integrations::validate_endpoint(&url, &custom_key)
            .await
            .map_err(|e| bad_request(format!("{e:#}")))?;
    }
    if provider == "custom" {
        for (tier, value) in [("low", &body.low), ("medium", &body.medium), ("high", &body.high)] {
            let stored = state.store.consolidation_state(&format!("llm.model.{tier}"))?;
            let was_custom = state.store.consolidation_state("llm.provider")?.as_deref() == Some("custom");
            let kept = was_custom && stored.is_some_and(|s| !s.trim().is_empty());
            if given(value).is_none() && !kept {
                return Err(bad_request(format!(
                    "the custom endpoint has no default models — name the {tier} tier's model"
                )));
            }
        }
    }

    if let Some(key) = &key {
        // a fresh key is an implicit "billing's fixed" — unblock
        let _ = state.store.set_consolidation_state("llm.blocked", "");
        if provider == "custom" {
            // Already proven against the endpoint above.
            hearthloom_core::secrets::set_env_vars(
                state.home.root(),
                &[(crate::integrations::provider_key_env("custom"), key.as_str())],
            )
            .map_err(|e| bad_request(format!("{e:#}")))?;
        } else {
            crate::integrations::set_provider_key(&state.home, &provider, key)
                .await
                .map_err(|e| bad_request(format!("{e:#}")))?;
        }
    }
    // Switching provider with a tier left blank must fall back to the NEW
    // provider's default — not keep the old provider's model id, which
    // the new one would reject on every call.
    let switching = state
        .store
        .consolidation_state("llm.provider")?
        .is_some_and(|prev| !prev.trim().is_empty() && prev != provider);
    let set = |k: &str, v: &Option<String>| -> Result<(), ApiError> {
        match given(v) {
            Some(v) => state.store.set_consolidation_state(k, &v)?,
            None if switching => state.store.set_consolidation_state(k, "")?,
            None => {}
        }
        Ok(())
    };
    state.store.set_consolidation_state("llm.provider", &provider)?;
    set("llm.model.low", &body.low)?;
    set("llm.model.medium", &body.medium)?;
    set("llm.model.high", &body.high)?;
    if let Some(url) = &base_url {
        state.store.set_consolidation_state("llm.custom.base_url", url)?;
    }
    for (tier, name, model) in &routes {
        let t = tier.as_str();
        // Routing a tier to the active provider with no model named is
        // no route at all; with a model it is a node-local model override.
        let cleared = name.is_empty() || (*name == provider && model.is_empty());
        state.store.set_consolidation_state(
            &format!("llm.route.{t}.provider"),
            if cleared { "" } else { name },
        )?;
        state.store.set_consolidation_state(
            &format!("llm.route.{t}.model"),
            if cleared { "" } else { model },
        )?;
    }
    for (tier, level) in &efforts {
        state.store.set_consolidation_state(&format!("llm.effort.{}", tier.as_str()), level)?;
    }
    crate::monitor::log(&state, "info", "llm_config_set",
        &format!("LLM config saved ({provider}) — restarting to load it"), None);
    tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_millis(800)).await;
        std::process::exit(0);
    });
    Ok(Json(json!({
        "saved": true,
        "active_provider": provider,
        "note": "saved — restarting to load it (back in a few seconds)",
    })))
}

/* ————— live health: today so far ————— */

/// Today's per-(metric, source) rollup from the DURABLE live sample
/// store (v44) — the intra-day point the export sidecar can't have.
/// Same coarse vault gate as the apple-health vquery: any private
/// episode on the source locks the whole numeric surface.
async fn health_today(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let report = crate::scoped::spawn_blocking(move || -> anyhow::Result<Value> {
        let (vault_locked, _) = state.store.vault_status();
        if vault_locked && state.store.source_has_private("apple-health")? {
            return Ok(json!({"locked": true, "rows": []}));
        }
        let off = state.store.health_latest_tz_offset()?;
        let day = (chrono::Utc::now()
            + chrono::Duration::minutes(off as i64))
        .format("%Y-%m-%d")
        .to_string();
        Ok(json!({"day": day, "rows": state.store.health_day_rollup(&day)?}))
    })
    .await
    .map_err(anyhow::Error::from)??;
    Ok(Json(report))
}

/* ————— Google Mail live pull ————— */

/// Status for the Google Mail card. The app password never leaves the
/// sealed store — only presence and the account name are reported.
async fn gmail_status(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let get = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    let user = get("GMAIL_IMAP_USER");
    let configured = user.is_some() && get("GMAIL_IMAP_PASSWORD").is_some();
    Ok(Json(json!({
        "configured": configured,
        "user": user,
        "last_ran_at": state.store.consolidation_state("settings.gmail_pull.last_ran_at")?,
        "live_mbox": crate::gmail_pull::live_mbox_path(&state).exists(),
    })))
}

#[derive(Deserialize)]
struct GmailSaveBody {
    actor: String,
    #[serde(default)]
    user: Option<String>,
    #[serde(default)]
    password: Option<String>,
    #[serde(default)]
    disconnect: bool,
}

/// Validate + persist the Gmail IMAP credential. Test-then-store: a
/// real IMAP login must succeed before anything is written. Persistence
/// goes through `set_env_vars` (decrypt-merge-reseal + live process
/// env) — NEVER a raw .env write, which would clobber the sealed file
/// and lose every other secret on the next boot. No restart needed: the
/// gmail_pull automation feature-detects the vars each cycle.
async fn gmail_save(
    State(state): State<AppState>,
    Json(body): Json<GmailSaveBody>,
) -> Result<Json<Value>, ApiError> {
    if body.actor.trim().is_empty() {
        return Err(bad_request("actor is required on every write"));
    }
    if body.disconnect {
        hearthloom_core::secrets::set_env_vars(
            state.home.root(),
            &[("GMAIL_IMAP_USER", ""), ("GMAIL_IMAP_PASSWORD", "")],
        )
        .map_err(|e| bad_request(format!("{e:#}")))?;
        crate::monitor::log(&state, "info", "gmail_disconnected",
            "Google Mail live pull disconnected — the live archive stays", None);
        return Ok(Json(json!({"saved": true, "configured": false})));
    }
    let user = body
        .user
        .as_deref()
        .map(str::trim)
        .filter(|u| u.contains('@'))
        .ok_or_else(|| bad_request("a Gmail address is required"))?
        .to_string();
    // Google renders app passwords in spaced groups of four — accept
    // them pasted either way.
    let password: String = body
        .password
        .as_deref()
        .unwrap_or("")
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    if password.is_empty() {
        return Err(bad_request(
            "an app password is required — create one at myaccount.google.com/apppasswords",
        ));
    }
    let (u, p) = (user.clone(), password.clone());
    crate::scoped::spawn_blocking(move || -> anyhow::Result<()> {
        let mut feed = hearthloom_adapters::gmail_imap::ImapFeed::connect(&u, &p)?;
        use hearthloom_adapters::gmail_imap::MailFeed;
        feed.select_all_mail()?;
        feed.logout();
        Ok(())
    })
    .await
    .map_err(anyhow::Error::from)?
    .map_err(|e| bad_request(format!(
        "Gmail sign-in failed: {e:#}. Check the address, and that this is an app password \
         (myaccount.google.com/apppasswords), not your account password."
    )))?;
    hearthloom_core::secrets::set_env_vars(
        state.home.root(),
        &[("GMAIL_IMAP_USER", user.as_str()), ("GMAIL_IMAP_PASSWORD", password.as_str())],
    )
    .map_err(|e| bad_request(format!("{e:#}")))?;
    crate::monitor::log(&state, "info", "gmail_connected",
        &format!("Google Mail live pull connected ({user}) — pulling each cycle"), None);
    Ok(Json(json!({"saved": true, "configured": true, "user": user})))
}

/* ————— API keys (PATs) ————— */

async fn keys_list(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
) -> Result<Json<Value>, ApiError> {
    require_key_admin(&auth)?;
    Ok(Json(json!({
        "keys": state.store.list_api_keys()?,
        "scopes": hearthloom_core::store::API_KEY_SCOPES,
    })))
}

#[derive(Deserialize)]
struct KeyCreateBody {
    actor: String,
    name: String,
    #[serde(default)]
    scopes: Vec<String>,
    /// RFC3339; omit for a non-expiring key.
    expires_at: Option<String>,
    /// Or a lifetime from now, in seconds (the UI's presets).
    expires_in_secs: Option<i64>,
    /// Makes this a SCOPED key (then `scopes` must be empty).
    grants: Option<hearthloom_core::store::KeyGrants>,
}

fn expiry_from(expires_at: Option<String>, in_secs: Option<i64>) -> Option<String> {
    expires_at.filter(|s| !s.trim().is_empty()).or_else(|| {
        in_secs.filter(|s| *s > 0).map(|secs| {
            (chrono::Utc::now() + chrono::Duration::seconds(secs))
                .format("%Y-%m-%dT%H:%M:%SZ")
                .to_string()
        })
    })
}

/// Who a write is recorded under. A key that isn't the owner's — a
/// scoped key, or an ordinary one other than an admin key or the paired
/// phone — cannot name its own actor: its writes land as
/// `agent:key:<name>`, which is what the activity list, Undo and the
/// note-writer record read. The owner (session, admin key, phone) keeps
/// its claim.
fn write_actor(auth: &AuthInfo, claimed: String) -> Actor {
    match &auth.0 {
        // A scoped key the owner minted for one of their own devices
        // (`KeyGrants::own_device`): its words are the owner's.
        Some(c) if c.grants.as_ref().is_some_and(|g| g.own_device) => {
            Actor::new(format!("human:device:{}", c.name))
        }
        Some(c) if c.grants.is_some() || !is_owner_key(&c.scopes, false) => Actor::new(c.agent_actor()),
        _ => Actor::new(claimed),
    }
}

/// The owner's own device (the paired phone's key) may mint a SCOPED key
/// — grants, no scopes: always narrower than itself — such as the phone's
/// capture key. Every other key management needs `require_key_admin`.
fn device_mints_scoped_key(auth: &AuthInfo, body: &KeyCreateBody) -> bool {
    matches!(&auth.0, Some(c) if c.grants.is_none() && is_owner_key(&c.scopes, false))
        && body.grants.is_some()
        && body.scopes.is_empty()
}

/// Only the signed-in human (or an admin key) manages keys — a scoped or
/// ordinary integration key must never mint, approve or inspect others.
fn require_key_admin(auth: &AuthInfo) -> Result<(), ApiError> {
    match &auth.0 {
        None => Ok(()),
        Some(c) if c.scopes.iter().any(|s| s == "admin") => Ok(()),
        Some(_) => Err(ApiError(StatusCode::FORBIDDEN, "managing keys needs the admin scope".into())),
    }
}

async fn keys_create(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
    Json(body): Json<KeyCreateBody>,
) -> Result<Json<Value>, ApiError> {
    if !device_mints_scoped_key(&auth, &body) {
        require_key_admin(&auth)?;
    }
    if body.actor.trim().is_empty() {
        return Err(bad_request("actor is required on every write"));
    }
    let expires = expiry_from(body.expires_at, body.expires_in_secs);
    let (row, secret) = state
        .store
        .create_api_key_scoped(
            &Actor::new(body.actor),
            &body.name,
            &body.scopes,
            expires.as_deref(),
            body.grants.as_ref(),
        )
        .map_err(|e| bad_request(format!("{e:#}")))?;
    Ok(Json(json!({
        "key": row,
        // The plaintext exists in this response and nowhere else.
        "secret": secret,
    })))
}

/* —— access requests: an agent asks, the human approves (v52) —— */

#[derive(Deserialize)]
struct KeyRequestBody {
    name: String,
    grants: hearthloom_core::store::KeyGrants,
    /// How long the key should live once approved (seconds).
    expires_in_secs: Option<i64>,
}

/// An agent asks for a scoped key. Grants nothing by itself: the request
/// appears under ☰ → API Keys and lapses unless approved in the window.
/// Returns the id + a poll secret the requester uses to collect the key.
async fn key_request_create(
    State(state): State<AppState>,
    Json(body): Json<KeyRequestBody>,
) -> Result<Json<Value>, ApiError> {
    let (id, poll) = state
        .store
        .key_request_create(&body.name, &body.grants, body.expires_in_secs)
        .map_err(|e| bad_request(format!("{e:#}")))?;
    // Best-effort heads-up; the request list under API Keys is the truth.
    let _ = state.store.notify(
        &Actor::new("agent:key-request"),
        "key-request",
        "info",
        &format!("{} is asking for access", body.name.trim()),
        "Review what it wants under API Keys — the request lapses in 10 minutes.",
        Some(("#/apikeys", Some("review"))),
        Some(&id),
    );
    Ok(Json(json!({
        "request_id": id,
        "poll_secret": poll,
        "window_secs": hearthloom_core::store::REQUEST_WINDOW_SECS,
        "note": "waiting for the owner to approve under API Keys; poll GET /v1/keys/request/{id}?poll_secret=…",
    })))
}

#[derive(Deserialize)]
struct KeyRequestPollParams {
    poll_secret: String,
}

async fn key_request_poll(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<KeyRequestPollParams>,
) -> Result<Json<Value>, ApiError> {
    match state.store.key_request_poll(&id, &params.poll_secret)? {
        // The secret appears in exactly one response, then is erased.
        Some((status, secret)) => Ok(Json(json!({"status": status, "secret": secret}))),
        None => Err(ApiError(StatusCode::NOT_FOUND, "no such request".into())),
    }
}

async fn key_requests_list(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
) -> Result<Json<Value>, ApiError> {
    require_key_admin(&auth)?;
    Ok(Json(json!({"requests": state.store.key_requests_pending()?})))
}

#[derive(Deserialize)]
struct KeyRequestDecideBody {
    actor: String,
    approve: bool,
}

async fn key_request_decide(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
    Path(id): Path<String>,
    Json(body): Json<KeyRequestDecideBody>,
) -> Result<Json<Value>, ApiError> {
    require_key_admin(&auth)?;
    if state.store.key_request_decide(&Actor::new(body.actor), &id, body.approve)? {
        Ok(Json(json!({"decided": id, "approved": body.approve})))
    } else {
        Err(ApiError(StatusCode::NOT_FOUND, "no such pending request (it may have lapsed)".into()))
    }
}

/// What a key has written — the list behind Undo.
async fn key_activity(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    require_key_admin(&auth)?;
    let key = state
        .store
        .api_key(&id)?
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, format!("no key {id}")))?;
    let actor = format!("agent:key:{}", key.name);
    let mut items = state.store.key_activity(&actor, 100)?;
    // `live`: is the thing it created still on the Desk / in Notes? An
    // undone (archived) or since-deleted item must not offer Undo again.
    for item in items.iter_mut() {
        let id = item["entity_id"].as_str().unwrap_or("").to_string();
        let safe = !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        let table = match item["kind"].as_str().unwrap_or("") {
            k if k.starts_with("task") => "tasks",
            k if k.starts_with("note") => "notes_index",
            _ => "",
        };
        let live = safe
            && !table.is_empty()
            && state
                .store
                .read_query(&format!("SELECT status FROM {table} WHERE id = '{id}'"), 1)
                .ok()
                .and_then(|(_, rows)| rows.first().and_then(|r| r[0].as_str().map(String::from)))
                .is_some_and(|status| status != "archived" && status != "deleted");
        item["live"] = json!(live);
    }
    Ok(Json(json!({"actor": actor, "items": items})))
}

#[derive(Deserialize)]
struct KeyUndoBody {
    actor: String,
    /// "task" | "note" + the id the key created.
    kind: String,
    entity_id: String,
}

/// Undo one thing a key CREATED: the task or note is archived — off the
/// Desk, still restorable. Refuses
/// anything that key's actor didn't create.
async fn key_undo(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
    Path(id): Path<String>,
    Json(body): Json<KeyUndoBody>,
) -> Result<Json<Value>, ApiError> {
    require_key_admin(&auth)?;
    let key = state
        .store
        .api_key(&id)?
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, format!("no key {id}")))?;
    let agent = format!("agent:key:{}", key.name);
    let made_it = state.store.key_activity(&agent, 500)?.iter().any(|item| {
        item["op"] == "create" && item["entity_id"] == body.entity_id.as_str()
    });
    if !made_it {
        return Err(bad_request("that key didn't create this item"));
    }
    let actor = Actor::new(body.actor);
    match body.kind.as_str() {
        // Archived, not purged: it leaves the Desk and stays restorable.
        "task" | "tasks" => {
            state.store.update_task(&actor, &body.entity_id, None, None, Some("archived"), None, None)?
        }
        "note" | "notes" => {
            state.store.set_note_status(&actor, &state.home, &body.entity_id, "archived")?;
        }
        other => return Err(bad_request(format!("can't undo a {other}"))),
    }
    crate::bus::touch(&state, "operational.changed");
    Ok(Json(json!({"undone": body.entity_id})))
}

/* —— one-tap capture (the phone's lock-screen / Action Button door) —— */

#[derive(Deserialize)]
struct CaptureBody {
    text: String,
    /// "auto" (default) | "task" | "note".
    #[serde(default)]
    kind: String,
    /// RFC3339, when the thought was captured (the phone may be offline).
    captured_at: Option<String>,
    lat: Option<f64>,
    lon: Option<f64>,
    /// Client-generated id — a retried upload must not file twice.
    client_ref: Option<String>,
    #[serde(default)]
    device: String,
}

/// Does a captured line read like something to DO? Deliberately small:
/// an explicit `kind` or a #hashtag always wins over this guess.
pub(crate) fn capture_reads_as_task(text: &str) -> bool {
    let t = text.trim().to_lowercase();
    const LEADS: &[&str] = &[
        "remind me", "remember to", "todo", "to do", "to-do", "don't forget", "dont forget",
        "call ", "email ", "text ", "message ", "buy ", "pay ", "book ", "schedule ", "send ",
        "renew ", "cancel ", "order ", "pick up", "follow up", "ask ", "tell ", "fix ", "return ",
    ];
    LEADS.iter().any(|lead| t.starts_with(lead))
        || ["#today", "#tomorrow", "#urgent", "#important", "#task"].iter().any(|tag| t.contains(tag))
}

/// File one captured thought. WRITE-ONLY by design: it returns what it
/// created and nothing else, so the credential a locked phone holds
/// (`capture` grant, or the device `ingest` scope) can add to the record
/// but never read it. A task-shaped line becomes a task; anything else a
/// dated note, which settles into the record like any other note.
async fn ingest_capture(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
    Json(body): Json<CaptureBody>,
) -> Result<Json<Value>, ApiError> {
    let scoped = auth.0.as_ref().and_then(|c| c.grants.as_ref()).is_some_and(|g| g.capture);
    if !scoped {
        require_scope(&auth, "ingest")?;
    }
    let text = body.text.trim().to_string();
    if text.is_empty() {
        return Err(bad_request("nothing to capture"));
    }
    if text.chars().count() > 8000 {
        return Err(bad_request("a capture is a thought, not a document (8,000 characters max)"));
    }
    let dedupe = body
        .client_ref
        .as_deref()
        .map(str::trim)
        .filter(|r| !r.is_empty() && r.len() <= 100)
        .map(|r| format!("capture.ref.{r}"));
    if let Some(key) = &dedupe {
        if let Some(prior) = state.store.consolidation_state(key)?.filter(|v| !v.is_empty()) {
            let (kind, id) = prior.split_once(':').unwrap_or(("note", prior.as_str()));
            return Ok(Json(json!({"kind": kind, "id": id, "duplicate": true})));
        }
    }
    // The paired phone's key, and a capture key the owner marked as one of
    // their own devices, capture in the owner's own voice; any other
    // scoped key captures as itself (an agent's words).
    let device = |fallback: &str| {
        let device = body.device.trim();
        if device.is_empty() { fallback.to_string() } else { format!(":{device}") }
    };
    let actor = Actor::new(match &auth.0 {
        Some(c) if c.grants.as_ref().is_some_and(|g| g.own_device) => {
            format!("adapter:capture{}", device(&format!(":{}", c.name)))
        }
        Some(c) if c.grants.is_some() => c.agent_actor(),
        _ => format!("adapter:capture{}", device("")),
    });
    let as_task = match body.kind.trim() {
        "task" => true,
        "note" => false,
        _ => capture_reads_as_task(&text),
    };
    let when = body.captured_at.clone().unwrap_or_else(|| chrono::Utc::now().to_rfc3339());
    let place = match (body.lat, body.lon) {
        (Some(lat), Some(lon)) if lat.abs() <= 90.0 && lon.abs() <= 180.0 => Some(format!("{lat:.5},{lon:.5}")),
        _ => None,
    };
    let (kind, id) = if as_task {
        let id = state.store.add_task(
            &actor, "plain", &text, None, None, "one-time", "personal", None, None,
            &["capture".to_string()],
        )?;
        ("task", id)
    } else {
        let title: String = text.lines().next().unwrap_or("").chars().take(60).collect();
        let mut note = text.clone();
        note.push_str(&format!("\n\n_Captured {}{}_", when.chars().take(16).collect::<String>().replace('T', " "),
            place.as_ref().map(|p| format!(" at {p}")).unwrap_or_default()));
        let id = state.store.create_note(
            &actor, &state.home, &title, &note, &["capture".to_string()], "personal", None,
        )?;
        ("note", id)
    };
    if let Some(key) = &dedupe {
        let _ = state.store.set_consolidation_state(key, &format!("{kind}:{id}"));
    }
    crate::bus::touch(&state, "operational.changed");
    Ok(Json(json!({"kind": kind, "id": id, "duplicate": false})))
}

#[cfg(test)]
mod capture_tests {
    use super::capture_reads_as_task;

    #[test]
    fn test_capture_task_guess() {
        for line in ["Call the plumber tomorrow", "remind me to renew the passport", "buy oat milk",
                     "thoughts on the lease #today", "TODO: send Ada the deck"] {
            assert!(capture_reads_as_task(line), "{line}");
        }
        for line in ["The lease idea: split it over two years", "The ridge hike was harder than I expected",
                     "callback pattern seems wrong here", ""] {
            assert!(!capture_reads_as_task(line), "{line}");
        }
    }
}

/* —— "About me": everything Hearthloom holds as true about its owner —— */

/// One read for the About-me page: the self-model (believed, waiting,
/// retired), relationships, goals, the identities treated as "you", and
/// the exact briefing file agents load. Assembled from existing tables
/// through the vault-shadowed SQL reader — locked, nothing private (or
/// any count of it) is in here. No model call.
async fn portrait(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let report = crate::scoped::spawn_blocking(move || -> anyhow::Result<Value> {
        let rows = |sql: &str, cap: usize| -> anyhow::Result<Vec<Value>> {
            let (cols, rows) = state.store.read_query(sql, cap)?;
            Ok(rows
                .into_iter()
                .map(|row| Value::Object(cols.iter().cloned().zip(row).collect()))
                .collect())
        };
        let beliefs = rows(
            "SELECT id, sphere, kind, text, origin, status, evidence, created_at, updated_at
             FROM self_model ORDER BY kind, updated_at DESC",
            400,
        )?;
        let relationships = rows(
            "SELECT r.id, r.entity_id, e.name, e.type, r.kind, r.origin, r.status, r.note,
                    r.t_valid, r.t_invalid
             FROM relationships r JOIN entities e ON e.id = r.entity_id
             WHERE r.kind != 'me'
             ORDER BY CASE r.status WHEN 'confirmed' THEN 0 WHEN 'ended' THEN 2 ELSE 1 END, e.name",
            300,
        )?;
        let identities = rows(
            "SELECT a.kind, a.value, e.name FROM entity_attributes a
             JOIN entities e ON e.id = a.entity_id
             WHERE a.entity_id IN (SELECT entity_id FROM relationships WHERE kind = 'me')
             ORDER BY a.kind, a.value",
            80,
        )?;
        let goals = rows(
            "SELECT id, name, query, description, status, sphere, target_date FROM intents
             ORDER BY CASE status WHEN 'active' THEN 0 ELSE 1 END, name",
            100,
        )?;
        let (locked, _) = state.store.vault_status();
        let brain = std::fs::read_to_string(state.home.brain_index_path()).unwrap_or_default();
        Ok(json!({
            "beliefs": beliefs,
            "relationships": relationships,
            "identities": identities,
            "goals": goals,
            "brain_md": brain,
            "vault_locked": locked,
        }))
    })
    .await
    .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))??;
    Ok(Json(report))
}

#[derive(Deserialize)]
struct KeyRevokeBody {
    actor: String,
}

async fn keys_revoke(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
    Path(id): Path<String>,
    Json(body): Json<KeyRevokeBody>,
) -> Result<Json<Value>, ApiError> {
    require_key_admin(&auth)?;
    if body.actor.trim().is_empty() {
        return Err(bad_request("actor is required on every write"));
    }
    state.store.revoke_api_key(&Actor::new(body.actor), &id)?;
    Ok(Json(json!({"revoked": id})))
}

/* ————— auth: the login layer (deployment modes, 2026-07-11) —————
   The posture rule: auth is required iff an admin user exists.
   Bootstrap (first run only) creates the admin and the door locks in
   the same gesture. Sessions ride an HttpOnly cookie; PATs are the
   machine door and stay orthogonal. */

fn session_cookie_header(token: &str) -> (axum::http::HeaderName, String) {
    (
        axum::http::header::SET_COOKIE,
        // Lax (not Strict): the login POST itself must carry it home.
        // No `Secure` flag — loopback is http; remote postures terminate
        // TLS at the edge (Fly) where the scheme the daemon sees is http.
        format!(
            "hl_session={token}; HttpOnly; SameSite=Lax; Path=/; Max-Age={}",
            60 * 60 * 24 * hearthloom_core::store::SESSION_DAYS
        ),
    )
}

async fn auth_me(
    State(state): State<AppState>,
    req: axum::extract::Request,
) -> Result<Json<Value>, ApiError> {
    let locked = state.auth_locked.load(std::sync::atomic::Ordering::Relaxed);
    let user = session_cookie(&req).and_then(|t| state.store.session_user(&t).ok().flatten());
    // A provisioner-managed node (hearthlaunch tenant): the sign-in UI
    // hands the user back to the provisioner's own login/handoff instead
    // of the local password form — the provisioner re-enters via the
    // trusted-issuer token POST. Managed = the control-plane secret is
    // present (the same marker provisioner.rs gates on); the dashboard
    // URL is overridable for other provisioners.
    let provisioner: Option<String> = std::env::var("HEARTHLOOM_PROVISIONER_SECRET")
        .ok()
        .filter(|s| !s.is_empty())
        // the provisioner's address must be configured with its secret;
        // there is no built-in default (self-hosting is the default)
        .and_then(|_| std::env::var("HEARTHLOOM_PROVISIONER_URL").ok())
        .filter(|s| !s.is_empty());
    Ok(Json(json!({
        "auth": if locked { "required" } else { "open" },
        "user": user,
        "provisioner": provisioner,
        // the shell renders the killswitch screen from this flag (the
        // message rides the 402 bodies and GET /v1/notifications)
        "killswitch": state.killswitch.load(std::sync::atomic::Ordering::Relaxed),
    })))
}

#[derive(Deserialize)]
struct BootstrapBody {
    email: String,
    password: String,
}

/// First-run only: create the admin and lock the door. Refused once any
/// user exists (create_admin enforces it) — credential rotation will be
/// a separate, authenticated flow.
async fn auth_bootstrap(
    State(state): State<AppState>,
    Json(body): Json<BootstrapBody>,
) -> Result<Response, ApiError> {
    state.store.create_admin(
        &Actor::new("human:bootstrap"),
        &body.email,
        &body.password,
    )?;
    state
        .auth_locked
        .store(true, std::sync::atomic::Ordering::Relaxed);
    // Sign the creator straight in — bootstrap shouldn't end at a login
    // prompt for the password typed two seconds ago.
    let token = state
        .store
        .login(&body.email, &body.password)?
        .ok_or_else(|| bad_request("bootstrap succeeded but login failed"))?;
    let (name, value) = session_cookie_header(&token);
    Ok((
        [(name, value)],
        Json(json!({"bootstrapped": body.email, "auth": "required"})),
    )
        .into_response())
}

#[derive(Deserialize)]
struct LoginBody {
    #[serde(default)]
    email: String,
    #[serde(default)]
    password: String,
    /// the provisioner hook: a short-lived HMAC token from the trusted
    /// issuer (base64url(payload-json).base64url(hmac-sha256)) instead
    /// of credentials. payload: {"sub": "<email>", "exp": <epoch>}.
    #[serde(default)]
    issuer_token: Option<String>,
}

/// Sign in. Accepts JSON (the dashboard) OR a form-encoded body — the
/// the provisioner "open my brain" handoff is a top-level browser form POST
/// of issuer_token, which gets the cookie plus a 302 to `/` so the user
/// lands in their app instead of staring at JSON.
async fn auth_login(
    State(state): State<AppState>,
    headers: HeaderMap,
    raw: String,
) -> Result<Response, ApiError> {
    let content_type = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let is_form = content_type.starts_with("application/x-www-form-urlencoded");
    let body: LoginBody = if is_form {
        serde_urlencoded::from_str(&raw).map_err(|_| bad_request("malformed form body"))?
    } else {
        serde_json::from_str(&raw).map_err(|_| bad_request("malformed JSON body"))?
    };
    let token = if let Some(issued) = body.issuer_token.as_deref() {
        let subject = verify_issuer_token(issued)
            .map_err(|e| ApiError(StatusCode::UNAUTHORIZED, e.to_string()))?;
        state.store.login_as_issuer_subject(&subject)?
    } else {
        state.store.login(&body.email, &body.password)?
    };
    let Some(token) = token else {
        crate::monitor::log(&state, "warn", "login_failed",
            "a sign-in attempt was refused", None);
        return Err(ApiError(
            StatusCode::UNAUTHORIZED,
            "that didn't match — check the email and password".into(),
        ));
    };
    crate::monitor::log(&state, "info", "login", "a session was opened", None);
    let (name, value) = session_cookie_header(&token);
    // a browser form POST (or anything asking for html) gets redirected
    // into the app; API callers keep the JSON contract
    let wants_html = is_form
        || headers
            .get(axum::http::header::ACCEPT)
            .and_then(|v| v.to_str().ok())
            .map(|a| a.contains("text/html"))
            .unwrap_or(false);
    if wants_html {
        let mut resp = axum::response::Redirect::to("/").into_response();
        if let Ok(cookie) = value.parse() {
            resp.headers_mut().append(name, cookie);
        }
        return Ok(resp);
    }
    Ok(([(name, value)], Json(json!({"ok": true}))).into_response())
}

/// PAT → session exchange (the iOS shell's webview path): a device
/// holding only a scoped bearer PAT mints a browser session for the
/// node's admin, so the dashboard can render authenticated without
/// re-prompting for a password. The PAT itself already passed the auth
/// middleware; `read` is the floor every device PAT carries.
async fn auth_session_from_key(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
) -> Result<Response, ApiError> {
    require_scope(&auth, "read")?;
    // A session is the owner, vault and all — only an owner's key may
    // be traded for one (the paired phone's webview is the caller).
    require_owner(&auth, "sign in")?;
    let token = state.store.create_session_for_admin()?.ok_or(ApiError(
        StatusCode::CONFLICT,
        "no admin user on this node — nothing to sign in to".into(),
    ))?;
    crate::monitor::log(&state, "info", "login", "a session was opened from an API key", None);
    let (name, value) = session_cookie_header(&token);
    Ok(([(name, value)], Json(json!({"ok": true}))).into_response())
}

/// Live HealthKit ingest from the iOS companion app: durable samples
/// (idempotent by HK uuid) + immediate workout episodes. The daily
/// `health_day` episode is minted by the day-finalize consolidation
/// pass, never here — live and export.xml converge on the same
/// `day:YYYY-MM-DD` key (docs in live_health.rs).
async fn ingest_health(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
    Json(batch): Json<HealthIngestBatch>,
) -> Result<Json<Value>, ApiError> {
    require_scope(&auth, "ingest")?;
    if batch.actor.trim().is_empty() {
        return Err(bad_request("actor is required on every write"));
    }
    // Pure store+CPU → off the runtime workers (THE PASS CONTRACT);
    // health_add_samples batches its lock holds internally.
    let report = crate::scoped::spawn_blocking(move || -> anyhow::Result<Value> {
        let (accepted, duplicates) = state.store.health_add_samples(&batch.samples)?;
        let episodes: Vec<hearthloom_core::EpisodeDraft> = batch
            .workouts
            .iter()
            .map(crate::live_health::workout_in_to_episode)
            .collect();
        let mut workout_new = 0usize;
        for chunk in episodes.chunks(200) {
            let r = state.store.apply_batch(
                &state.home,
                &hearthloom_core::IngestBatch {
                    actor: batch.actor.clone(),
                    session_id: None,
                    episodes: chunk.to_vec(),
                },
            )?;
            workout_new += r.episodes_new;
        }
        // v48 audit trail: raw batches don't mint episodes until the
        // daily finalize, so this log is the only immediate evidence the
        // device stream landed (GET /v1/ingest/recent).
        let span = day_span_ref(batch.samples.iter().filter_map(|s| {
            chrono::DateTime::parse_from_rfc3339(&s.start)
                .ok()
                .map(|d| d.format("%Y-%m-%d").to_string())
        }));
        state.store.ingest_log_append(
            &batch.actor,
            "apple-health",
            "health_sample_batch",
            accepted,
            span.as_deref(),
        )?;
        if workout_new > 0 {
            state
                .store
                .ingest_log_append(&batch.actor, "apple-health", "workout", workout_new, None)?;
        }
        Ok(json!({
            "accepted": accepted,
            "duplicates": duplicates,
            "workouts_new": workout_new,
        }))
    })
    .await
    .map_err(anyhow::Error::from)??;
    Ok(Json(report))
}

/// iOS photo-location stream (v47): geotags only, never images. Points
/// land in the durable union store; the photos_day_finalize pass mints
/// (and supersedes) the per-day episodes.
async fn ingest_photos(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
    Json(batch): Json<PhotoIngestBatch>,
) -> Result<Json<Value>, ApiError> {
    require_scope(&auth, "ingest")?;
    if batch.actor.trim().is_empty() {
        return Err(bad_request("actor is required on every write"));
    }
    let report = crate::scoped::spawn_blocking(move || -> anyhow::Result<Value> {
        let (accepted, duplicates) = state
            .store
            .photos_add_points(&batch.device, &batch.photos)?;
        let day_span = day_span_ref(batch.photos.iter().filter_map(|p| {
            chrono::DateTime::parse_from_rfc3339(&p.taken_at)
                .ok()
                .map(|d| d.format("%Y-%m-%d").to_string())
        }));
        state.store.ingest_log_append(
            &batch.actor,
            "photos",
            "photo_batch",
            accepted,
            day_span.as_deref(),
        )?;
        Ok(json!({"accepted": accepted, "duplicates": duplicates}))
    })
    .await
    .map_err(anyhow::Error::from)??;
    Ok(Json(report))
}

/// A human-scannable ref for the ingest audit trail: the batch's day
/// span ("2026-08-13" or "2026-08-10..2026-08-13").
fn day_span_ref(days: impl Iterator<Item = String>) -> Option<String> {
    let (mut min, mut max): (Option<String>, Option<String>) = (None, None);
    for d in days {
        if min.as_deref().is_none_or(|m| d.as_str() < m) {
            min = Some(d.clone());
        }
        if max.as_deref().is_none_or(|m| d.as_str() > m) {
            max = Some(d);
        }
    }
    match (min, max) {
        (Some(a), Some(b)) if a == b => Some(a),
        (Some(a), Some(b)) => Some(format!("{a}..{b}")),
        _ => None,
    }
}

/// Shared preamble for the device-batch endpoints: ingest scope + a
/// non-empty actor (design law #4).
fn require_device_write(auth: &AuthInfo, actor: &str) -> Result<(), ApiError> {
    require_scope(auth, "ingest")?;
    if actor.trim().is_empty() {
        return Err(bad_request("actor is required on every write"));
    }
    Ok(())
}

/* ————— the movement layer (v60) ————— */

#[derive(Deserialize)]
struct VehicleIn {
    /// An opaque, stable id for the car (not the VIN — it lands in
    /// entity attributes).
    id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    descriptor: Option<Value>,
}

#[derive(Deserialize)]
struct MovementIngestBatch {
    actor: String,
    /// Non-plugin callers name their source; an ember's is fixed to
    /// `plugin:<name>` by its key.
    #[serde(default)]
    source: Option<String>,
    /// "me" (default) or omitted when `vehicle` is given.
    #[serde(default)]
    subject: Option<String>,
    #[serde(default)]
    vehicle: Option<VehicleIn>,
    items: Vec<hearthloom_core::store::MovementIn>,
}

fn clean_vehicle_id(id: &str) -> Option<String> {
    let ok = !id.is_empty()
        && id.len() <= 64
        && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    ok.then(|| id.to_string())
}

/// POST /v1/ingest/movement — spans of travel into the movement layer.
/// An EMBER may post only a vehicle's movements (a car's drive is the
/// car's until the trips pass decides you were in it); the subject is
/// receiver-stamped `vehicle:<ember>:<id>` and the source `plugin:<ember>`.
async fn ingest_movement(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
    Json(batch): Json<MovementIngestBatch>,
) -> Result<Json<Value>, ApiError> {
    require_device_write(&auth, &batch.actor)?;
    if batch.items.len() > 5000 {
        return Err(bad_request("at most 5000 movements per batch"));
    }
    let plugin = auth.0.as_ref().and_then(|c| crate::plugins::plugin_for_claims(&state, c));
    let (source, subject, actor) = match &plugin {
        Some(name) => {
            let vehicle = batch
                .vehicle
                .as_ref()
                .and_then(|v| clean_vehicle_id(&v.id))
                .ok_or_else(|| bad_request("an ember posts a vehicle's movements: give vehicle.id ([A-Za-z0-9_-], ≤64)"))?;
            if batch.subject.as_deref().is_some_and(|s| s != "vehicle") && batch.subject.is_some() {
                return Err(bad_request("an ember can't post the person's own movement — only a vehicle's"));
            }
            (format!("plugin:{name}"), format!("vehicle:{name}:{vehicle}"), format!("plugin:{name}"))
        }
        None => {
            let source = batch.source.clone().unwrap_or_default();
            let reserved = ["ios-motion", "trips", "photos", "visits"];
            if source.is_empty() || source.len() > 60 || reserved.contains(&source.as_str()) || source.starts_with("plugin:") {
                return Err(bad_request("name your source (not ios-motion/trips/plugin:…)"));
            }
            let subject = match &batch.vehicle {
                Some(v) => format!(
                    "vehicle:{source}:{}",
                    clean_vehicle_id(&v.id).ok_or_else(|| bad_request("vehicle.id must be [A-Za-z0-9_-], ≤64"))?
                ),
                None => "me".to_string(),
            };
            (source, subject, batch.actor.clone())
        }
    };
    let st = state.clone();
    let report = tokio::task::spawn_blocking(move || -> anyhow::Result<Value> {
        if let Some(v) = &batch.vehicle {
            st.store.vehicle_profile_ensure(
                &subject,
                v.name.as_deref().unwrap_or("").trim(),
                &v.descriptor.clone().unwrap_or_else(|| json!({})),
            )?;
        }
        let (accepted, duplicates) = st.store.movements_add(&source, &subject, &actor, &batch.items)?;
        let span = day_span_ref(batch.items.iter().filter_map(|m| {
            chrono::DateTime::parse_from_rfc3339(&m.start).ok().map(|d| d.format("%Y-%m-%d").to_string())
        }));
        st.store.ingest_log_append(&actor, &source, "movement_batch", accepted, span.as_deref())?;
        Ok(json!({"accepted": accepted, "duplicates": duplicates, "subject": subject}))
    })
    .await
    .map_err(anyhow::Error::from)?
    .map_err(|e| bad_request(format!("{e:#}")))?;
    Ok(Json(report))
}

async fn vehicles_list(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
) -> Result<Json<Value>, ApiError> {
    require_owner(&auth, "see your cars")?;
    Ok(Json(json!({
        "vehicles": state.store.vehicle_profiles()?,
        "questions_pending": state.store.trip_questions_pending_count()?,
    })))
}

/// Re-run trip reconciliation now (answers and settings should show at
/// once, not at tomorrow's pass). Best-effort, off the request.
fn reconcile_trips_soon(state: &AppState) {
    let st = state.clone();
    tokio::spawn(async move {
        let st2 = st.clone();
        match tokio::task::spawn_blocking(move || crate::trips::finalize_trips(&st2)).await {
            Ok(Ok(_)) => crate::bus::touch(&st, "operational.changed"),
            Ok(Err(e)) => tracing::warn!("trip reconcile (after an edit) failed: {e:#}"),
            Err(_) => {}
        }
    });
}

async fn vehicle_settings_set(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
    Path(vehicle): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    require_owner(&auth, "change a car's settings")?;
    let mut p = state
        .store
        .vehicle_profile(&vehicle)?
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, format!("no vehicle {vehicle:?}")))?;
    if let Some(v) = body.get("attribution").and_then(|v| v.as_str()) {
        p.attribution = v.to_string();
    }
    if let Some(v) = body.get("aboard_driver").and_then(|v| v.as_str()) {
        p.aboard_driver = v.to_string();
    }
    if let Some(v) = body.get("household").and_then(|v| v.as_array()) {
        p.household = v.iter().filter_map(|x| x.as_str().map(String::from)).collect();
    }
    if let Some(v) = body.get("others_scope").and_then(|v| v.as_str()) {
        p.others_scope = v.to_string();
    }
    if let Some(v) = body.get("ask").and_then(|v| v.as_bool()) {
        p.ask = v;
    }
    state.store.vehicle_profile_set(&p).map_err(|e| bad_request(format!("{e:#}")))?;
    reconcile_trips_soon(&state);
    Ok(Json(json!({"vehicle": state.store.vehicle_profile(&vehicle)?})))
}

async fn trip_questions(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
) -> Result<Json<Value>, ApiError> {
    require_owner(&auth, "see questions about your trips")?;
    Ok(Json(json!({"pending": state.store.trip_questions_pending()?})))
}

#[derive(Deserialize)]
struct TripAnswerBody {
    /// yes | no | dismiss
    answer: String,
}

async fn trip_question_answer(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
    Path(key): Path<String>,
    Json(body): Json<TripAnswerBody>,
) -> Result<Json<Value>, ApiError> {
    require_owner(&auth, "answer for your trips")?;
    state
        .store
        .trip_question_answer(&key, &body.answer)
        .map_err(|e| bad_request(format!("{e:#}")))?;
    reconcile_trips_soon(&state);
    Ok(Json(json!({"key": key, "answer": body.answer})))
}

#[derive(Deserialize)]
struct ContactsIngestBatch {
    actor: String,
    #[serde(default)]
    device: String,
    #[serde(default)]
    items: Vec<hearthloom_core::store::ContactCardIn>,
}

/// Device address book (v51): cards land in the durable contact_card
/// store, then apply into Person entities attribute-first — the
/// identity backbone. Changed cards re-mint under #r<n>.
async fn ingest_contacts(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
    Json(batch): Json<ContactsIngestBatch>,
) -> Result<Json<Value>, ApiError> {
    require_device_write(&auth, &batch.actor)?;
    let report = crate::scoped::spawn_blocking(move || -> anyhow::Result<Value> {
        let (new, updated, unchanged) =
            state.store.contacts_upsert(&batch.device, &batch.items)?;
        let applied = crate::live_ingest::apply_pending_contacts(&state, new + updated + 200)?;
        state.store.ingest_log_append(
            &batch.actor,
            "contacts",
            "contact_batch",
            new + updated,
            Some(&format!("{} cards", batch.items.len())),
        )?;
        Ok(json!({
            "accepted": new,
            "updated": updated,
            "duplicates": unchanged,
            "applied": applied,
        }))
    })
    .await
    .map_err(anyhow::Error::from)??;
    Ok(Json(report))
}

#[derive(Deserialize)]
struct CalendarIngestBatch {
    actor: String,
    #[serde(default)]
    items: Vec<crate::live_ingest::CalendarEventIn>,
}

/// Live device calendar (v51): events mint calendar_event episodes on
/// the SAME source="ics" + UID refs the .ics import uses, so the two
/// paths dedupe; a venue with coordinates also feeds the unified
/// location layer (Part A).
async fn ingest_calendar(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
    Json(batch): Json<CalendarIngestBatch>,
) -> Result<Json<Value>, ApiError> {
    require_device_write(&auth, &batch.actor)?;
    let report = crate::scoped::spawn_blocking(move || -> anyhow::Result<Value> {
        let mut drafts = Vec::new();
        let mut observations = Vec::new();
        for ev in &batch.items {
            let draft = crate::live_ingest::calendar_in_to_episode(ev)?;
            if let (Some(lat), Some(lon)) = (ev.lat, ev.lon) {
                observations.push(hearthloom_core::store::ObservationIn {
                    key: format!("{}|{}", draft.source_ref, ev.start),
                    lat,
                    lon,
                    accuracy_m: None,
                    t_start: chrono::DateTime::parse_from_rfc3339(&ev.start)
                        .map(|d| d.to_utc().format("%Y-%m-%dT%H:%M:%SZ").to_string())
                        .unwrap_or_else(|_| ev.start.clone()),
                    t_end: ev.end.as_deref().and_then(|e| {
                        chrono::DateTime::parse_from_rfc3339(e)
                            .map(|d| d.to_utc().format("%Y-%m-%dT%H:%M:%SZ").to_string())
                            .ok()
                    }),
                    tz_offset_minutes: draft.event_time.tz_offset_minutes.unwrap_or(0),
                    place: ev.location.clone(),
                });
            }
            drafts.push(draft);
        }
        let mut episodes_new = 0usize;
        for chunk in drafts.chunks(200) {
            let r = state.store.apply_batch(
                &state.home,
                &hearthloom_core::IngestBatch {
                    actor: batch.actor.clone(),
                    session_id: None,
                    episodes: chunk.to_vec(),
                },
            )?;
            episodes_new += r.episodes_new;
        }
        let (obs_new, _) =
            state.store.observations_add("calendar", &batch.actor, &observations)?;
        let span = day_span_ref(batch.items.iter().filter_map(|e| {
            chrono::DateTime::parse_from_rfc3339(&e.start)
                .ok()
                .map(|d| d.format("%Y-%m-%d").to_string())
        }));
        state.store.ingest_log_append(
            &batch.actor,
            "calendar",
            "calendar_batch",
            episodes_new,
            span.as_deref(),
        )?;
        Ok(json!({
            "accepted": episodes_new,
            "duplicates": batch.items.len().saturating_sub(episodes_new),
            "observations": obs_new,
        }))
    })
    .await
    .map_err(anyhow::Error::from)??;
    Ok(Json(report))
}

#[derive(Deserialize)]
struct ReminderIn {
    uid: String,
    title: String,
    #[serde(default)]
    due: Option<String>,
    #[serde(default)]
    completed: bool,
    #[serde(default)]
    list: Option<String>,
}

#[derive(Deserialize)]
struct RemindersIngestBatch {
    actor: String,
    #[serde(default)]
    items: Vec<ReminderIn>,
}

/// Device reminders (v51) → the tasks model, upserted under a
/// DETERMINISTIC task id from the reminder uid so re-syncs and
/// cross-node posts converge (the operational mirror LWWs the rest).
async fn ingest_reminders(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
    Json(batch): Json<RemindersIngestBatch>,
) -> Result<Json<Value>, ApiError> {
    require_device_write(&auth, &batch.actor)?;
    let st = state.clone();
    let report = crate::scoped::spawn_blocking(move || -> anyhow::Result<Value> {
        let actor = hearthloom_core::store::Actor::new(batch.actor.clone());
        let (mut new, mut updated, mut unchanged) = (0usize, 0usize, 0usize);
        for r in &batch.items {
            match st.store.reminder_upsert(
                &actor,
                &r.uid,
                &r.title,
                r.due.as_deref(),
                r.completed,
                r.list.as_deref(),
            ) {
                Ok("new") => new += 1,
                Ok("updated") => updated += 1,
                Ok(_) => unchanged += 1,
                Err(_) => {}
            }
        }
        st.store.ingest_log_append(
            &batch.actor,
            "reminders",
            "reminder_batch",
            new + updated,
            Some(&format!("{} reminders", batch.items.len())),
        )?;
        Ok(json!({"accepted": new, "updated": updated, "duplicates": unchanged}))
    })
    .await
    .map_err(anyhow::Error::from)??;
    if report["accepted"].as_u64().unwrap_or(0) + report["updated"].as_u64().unwrap_or(0) > 0 {
        crate::bus::touch(&state, "operational.changed");
    }
    Ok(Json(report))
}

#[derive(Deserialize)]
struct LocationIngestBatch {
    actor: String,
    #[serde(default)]
    source: String,
    #[serde(default)]
    items: Vec<hearthloom_core::store::ObservationIn>,
}

/// Presence/visits (v51, THE flagship): CLVisit dwells (and any other
/// geolocated stream — a Google-timeline import posts here too with its
/// own source name) land in the unified location_observation layer;
/// the visits_finalize pass clusters ACROSS sources into canonical
/// visit episodes.
async fn ingest_location(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
    Json(batch): Json<LocationIngestBatch>,
) -> Result<Json<Value>, ApiError> {
    require_device_write(&auth, &batch.actor)?;
    let report = crate::scoped::spawn_blocking(move || -> anyhow::Result<Value> {
        let source = if batch.source.trim().is_empty() {
            "ios-visits"
        } else {
            batch.source.trim()
        };
        let (accepted, duplicates) =
            state.store.observations_add(source, &batch.actor, &batch.items)?;
        let span = day_span_ref(batch.items.iter().filter_map(|o| {
            chrono::DateTime::parse_from_rfc3339(&o.t_start)
                .ok()
                .map(|d| d.format("%Y-%m-%d").to_string())
        }));
        state.store.ingest_log_append(
            &batch.actor,
            source,
            "location_batch",
            accepted,
            span.as_deref(),
        )?;
        Ok(json!({"accepted": accepted, "duplicates": duplicates}))
    })
    .await
    .map_err(anyhow::Error::from)??;
    Ok(Json(report))
}

#[derive(Deserialize)]
struct MotionIngestBatch {
    actor: String,
    #[serde(default)]
    items: Vec<crate::live_ingest::MotionIn>,
}

/// Trips/motion (v51): activity segments become `trip` episodes —
/// the movement between visits.
async fn ingest_motion(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
    Json(batch): Json<MotionIngestBatch>,
) -> Result<Json<Value>, ApiError> {
    require_device_write(&auth, &batch.actor)?;
    let report = crate::scoped::spawn_blocking(move || -> anyhow::Result<Value> {
        let scope = crate::visits::device_scope(&state.store);
        let drafts: Vec<hearthloom_core::EpisodeDraft> = batch
            .items
            .iter()
            .filter_map(|m| crate::live_ingest::motion_in_to_episode(m, &scope).ok())
            .collect();
        // v60: the same segments join the movement layer as the PERSON's
        // movement (subject me) — the trips pass reconciles them against
        // a car's drives. Same key as the episode's source_ref, so the
        // pass can find (and absorb) the phone's own trip episode.
        let movements: Vec<hearthloom_core::store::MovementIn> = batch
            .items
            .iter()
            .filter(|m| hearthloom_core::store::MOVEMENT_MODES.contains(&m.mode.as_str()))
            .map(|m| hearthloom_core::store::MovementIn {
                key: m.key.clone(),
                mode: m.mode.clone(),
                start: m.start.clone(),
                end: m.end.clone(),
                tz_offset_minutes: m.tz_offset_minutes,
                start_lat: None,
                start_lon: None,
                end_lat: None,
                end_lon: None,
                start_place: None,
                end_place: None,
                distance_m: m.distance_m,
                attrs: m.steps.map(|s| json!({"steps": s})),
            })
            .collect();
        if let Err(e) = state.store.movements_add("ios-motion", "me", &batch.actor, &movements) {
            tracing::warn!("motion → movement layer: {e:#}");
        }
        let mut episodes_new = 0usize;
        for chunk in drafts.chunks(200) {
            let r = state.store.apply_batch(
                &state.home,
                &hearthloom_core::IngestBatch {
                    actor: batch.actor.clone(),
                    session_id: None,
                    episodes: chunk.to_vec(),
                },
            )?;
            episodes_new += r.episodes_new;
        }
        let span = day_span_ref(batch.items.iter().filter_map(|m| {
            chrono::DateTime::parse_from_rfc3339(&m.start)
                .ok()
                .map(|d| d.format("%Y-%m-%d").to_string())
        }));
        state.store.ingest_log_append(
            &batch.actor,
            "ios-motion",
            "motion_batch",
            episodes_new,
            span.as_deref(),
        )?;
        Ok(json!({
            "accepted": episodes_new,
            "duplicates": batch.items.len().saturating_sub(episodes_new),
        }))
    })
    .await
    .map_err(anyhow::Error::from)??;
    Ok(Json(report))
}

#[derive(Deserialize)]
struct MediaIngestBatch {
    actor: String,
    #[serde(default)]
    items: Vec<crate::live_ingest::MediaIn>,
}

/// Media plays (v51): songs/podcasts become media_play episodes with
/// Track/Artist/Show entities — the taste/culture dimension.
async fn ingest_media(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
    Json(batch): Json<MediaIngestBatch>,
) -> Result<Json<Value>, ApiError> {
    require_device_write(&auth, &batch.actor)?;
    let report = crate::scoped::spawn_blocking(move || -> anyhow::Result<Value> {
        let drafts: Vec<hearthloom_core::EpisodeDraft> = batch
            .items
            .iter()
            .filter_map(|m| crate::live_ingest::media_in_to_episode(m).ok())
            .collect();
        let mut episodes_new = 0usize;
        for chunk in drafts.chunks(200) {
            let r = state.store.apply_batch(
                &state.home,
                &hearthloom_core::IngestBatch {
                    actor: batch.actor.clone(),
                    session_id: None,
                    episodes: chunk.to_vec(),
                },
            )?;
            episodes_new += r.episodes_new;
        }
        let span = day_span_ref(batch.items.iter().filter_map(|m| {
            chrono::DateTime::parse_from_rfc3339(&m.played_at)
                .ok()
                .map(|d| d.format("%Y-%m-%d").to_string())
        }));
        state.store.ingest_log_append(
            &batch.actor,
            "ios-media",
            "media_batch",
            episodes_new,
            span.as_deref(),
        )?;
        Ok(json!({
            "accepted": episodes_new,
            "duplicates": batch.items.len().saturating_sub(episodes_new),
        }))
    })
    .await
    .map_err(anyhow::Error::from)??;
    Ok(Json(report))
}

#[derive(Deserialize)]
struct ScanIngestBatch {
    actor: String,
    #[serde(default)]
    items: Vec<crate::live_ingest::ScanIn>,
}

/// Vision captures (v51): receipts/cards/documents arrive as OCR text
/// and become receipt/business_card/document episodes; the LLM
/// extraction pass (ios-scan is allowlisted) mines merchants → Org and
/// card owners → Person from the text. A geotagged capture also feeds
/// the location layer.
async fn ingest_scan(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
    Json(batch): Json<ScanIngestBatch>,
) -> Result<Json<Value>, ApiError> {
    require_device_write(&auth, &batch.actor)?;
    let report = crate::scoped::spawn_blocking(move || -> anyhow::Result<Value> {
        let mut drafts = Vec::new();
        let mut observations = Vec::new();
        for s in &batch.items {
            let draft = crate::live_ingest::scan_in_to_episode(s)?;
            if let (Some(lat), Some(lon)) = (s.lat, s.lon) {
                observations.push(hearthloom_core::store::ObservationIn {
                    key: s.key.clone(),
                    lat,
                    lon,
                    accuracy_m: None,
                    t_start: s.captured_at.clone(),
                    t_end: None,
                    tz_offset_minutes: s.tz_offset_minutes,
                    place: None,
                });
            }
            drafts.push(draft);
        }
        let mut episodes_new = 0usize;
        for chunk in drafts.chunks(100) {
            let r = state.store.apply_batch(
                &state.home,
                &hearthloom_core::IngestBatch {
                    actor: batch.actor.clone(),
                    session_id: None,
                    episodes: chunk.to_vec(),
                },
            )?;
            episodes_new += r.episodes_new;
        }
        let (obs_new, _) = state.store.observations_add("ios-scan", &batch.actor, &observations)?;
        state.store.ingest_log_append(
            &batch.actor,
            "ios-scan",
            "scan_batch",
            episodes_new,
            batch.items.first().map(|s| s.kind.as_str()),
        )?;
        Ok(json!({
            "accepted": episodes_new,
            "duplicates": batch.items.len().saturating_sub(episodes_new),
            "observations": obs_new,
        }))
    })
    .await
    .map_err(anyhow::Error::from)??;
    Ok(Json(report))
}

#[derive(Deserialize)]
struct FocusIngestBatch {
    actor: String,
    #[serde(default)]
    source: String,
    #[serde(default)]
    device: String,
    #[serde(default)]
    items: Vec<hearthloom_core::store::DeviceStateIn>,
}

/// Focus/attention states (v51): thin interval stream into the generic
/// device_state table — source-generic by design (harmonized with the
/// hpresence ember's model, Jon 2026-08-16) so Mac presence spans can
/// later share the stream. Never episodes.
async fn ingest_focus(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
    Json(batch): Json<FocusIngestBatch>,
) -> Result<Json<Value>, ApiError> {
    require_device_write(&auth, &batch.actor)?;
    let report = crate::scoped::spawn_blocking(move || -> anyhow::Result<Value> {
        let source = if batch.source.trim().is_empty() {
            "ios-focus"
        } else {
            batch.source.trim()
        };
        let (accepted, duplicates) =
            state.store.device_states_add(source, &batch.device, &batch.items)?;
        state.store.ingest_log_append(
            &batch.actor,
            source,
            "state_batch",
            accepted,
            batch.items.last().map(|s| s.state.as_str()),
        )?;
        Ok(json!({"accepted": accepted, "duplicates": duplicates}))
    })
    .await
    .map_err(anyhow::Error::from)??;
    Ok(Json(report))
}

/// ————— the embedding model (semantic search), managed from the UI —————
async fn embeddings_status(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    Ok(Json(crate::embeddings_dl::status(
        &state.home.models_dir(),
        state.embedder.is_some(),
    )))
}

async fn embeddings_download(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    crate::embeddings_dl::start(state)?;
    Ok(Json(json!({
        "started": true,
        "note": "downloading — the node restarts itself once the model validates",
    })))
}

/// Git repo auto-discovery for the wizard (read scope): bounded walk of
/// the home directory; per repo, total commits and whether this
/// machine's git identity authored any (the confirmation list — the
/// human picks, nothing ingests from here).
async fn discover_git(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
) -> Result<Json<Value>, ApiError> {
    if auth.0.is_some() {
        require_scope(&auth, "read")?;
    }
    let repos = crate::scoped::spawn_blocking(discover_git_repos)
        .await
        .map_err(anyhow::Error::from)?;
    let _ = &state;
    Ok(Json(json!({ "repos": repos })))
}

fn discover_git_repos() -> Vec<Value> {
    use std::process::Command;
    let home = std::env::var("HOME").unwrap_or_default();
    if home.is_empty() {
        return Vec::new();
    }
    let git = |args: &[&str]| -> Option<String> {
        Command::new("git")
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    };
    let my_email = git(&["config", "--global", "user.email"]).unwrap_or_default();
    // bounded walk: depth ≤ 4, skip the well-known heavy/system dirs
    const SKIP: [&str; 10] = [
        "Library", "Applications", "Pictures", "Music", "Movies", "node_modules",
        "target", "vendor", ".Trash", ".cache",
    ];
    let mut found: Vec<Value> = Vec::new();
    let mut stack: Vec<(std::path::PathBuf, usize)> = vec![(home.clone().into(), 0)];
    while let Some((dir, depth)) = stack.pop() {
        if found.len() >= 200 {
            break;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if SKIP.contains(&name.as_str()) || (name.starts_with('.') && name != ".git") {
                continue;
            }
            if path.join(".git").is_dir() {
                let p = path.to_string_lossy().to_string();
                let commits: i64 = git(&["-C", &p, "rev-list", "--count", "HEAD"])
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
                if commits == 0 {
                    continue;
                }
                let authored = if my_email.is_empty() {
                    true // no identity to test against — let the human decide
                } else {
                    git(&["-C", &p, "log", "-1", "--format=%H", "--author", &my_email])
                        .map(|s| !s.is_empty())
                        .unwrap_or(false)
                };
                found.push(json!({
                    "path": p,
                    "name": name,
                    "commits": commits,
                    "authored": authored,
                    // the friction-log default: authored + not a monster
                    "recommended": authored && commits <= 3000,
                }));
                continue; // don't descend into a repo
            }
            if depth < 4 {
                stack.push((path, depth + 1));
            }
        }
    }
    found.sort_by(|a, b| {
        b["recommended"]
            .as_bool()
            .cmp(&a["recommended"].as_bool())
            .then(b["commits"].as_i64().cmp(&a["commits"].as_i64()))
    });
    found
}

#[derive(Deserialize)]
struct SqlBody {
    query: String,
}

/// Guarded read-only SQL (admin scope) — the same TEMP-VIEW-shadowed
/// `read_query` surface the RLM's sql handle uses (single SELECT/WITH,
/// vault guard applies, 120-row cap). Exists for the eval harness and
/// operator-shell spelunking; it can never write.
async fn raw_sql(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
    Json(body): Json<SqlBody>,
) -> Result<Json<Value>, ApiError> {
    // Keyless callers passed the auth middleware (loopback-open or a
    // signed-in session) — the same trust as /v1/search; a PAT must
    // still carry admin (a low-scope device key must not read SQL).
    if auth.0.is_some() {
        require_scope(&auth, "admin")?;
    }
    let (columns, rows) =
        crate::scoped::spawn_blocking(move || state.store.read_query(&body.query, 120))
            .await
            .map_err(anyhow::Error::from)??;
    Ok(Json(json!({ "columns": columns, "rows": rows })))
}

/// Per-source raw-ingestion summary (read scope) — the iOS companion's
/// "is my stream actually landing?" check. Aggregates the durable
/// sample stores, not episodes: samples don't mint episodes until the
/// daily finalize passes.
async fn ingest_status(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
) -> Result<Json<Value>, ApiError> {
    if auth.0.is_some() {
        require_scope(&auth, "read")?;
    }
    let report =
        crate::scoped::spawn_blocking(move || state.store.ingest_status())
            .await
            .map_err(anyhow::Error::from)??;
    Ok(Json(report))
}

#[derive(Deserialize)]
struct IngestRecentParams {
    #[serde(default)]
    offset: usize,
    #[serde(default = "default_ingest_recent_limit")]
    limit: usize,
}
fn default_ingest_recent_limit() -> usize {
    50
}

/// Paged audit trail of accepted device batches (read scope), newest
/// first — stable paging by offset (ingest_log is append-only).
async fn ingest_recent(
    State(state): State<AppState>,
    Query(params): Query<IngestRecentParams>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
) -> Result<Json<Value>, ApiError> {
    if auth.0.is_some() {
        require_scope(&auth, "read")?;
    }
    let report = crate::scoped::spawn_blocking(move || {
        state.store.ingest_recent(params.offset, params.limit)
    })
    .await
    .map_err(anyhow::Error::from)??;
    Ok(Json(report))
}

#[derive(Deserialize)]
struct PhotoIngestBatch {
    actor: String,
    #[serde(default)]
    #[allow(dead_code)]
    source: String,
    #[serde(default)]
    device: String,
    #[serde(default)]
    photos: Vec<hearthloom_core::store::PhotoPointIn>,
}

/// Peer photo-point feed (v47) — the Mac scan lives on the laptop, the
/// phone posts to the always-on node; the union needs both.
async fn sync_photos_get(
    State(state): State<AppState>,
    Query(params): Query<JournalAfterParams>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
) -> Result<Json<Value>, ApiError> {
    require_scope(&auth, "peer")?;
    Ok(Json(state.store.photo_points_export(params.after, 500)?))
}

async fn sync_photos_post(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
    Json(batch): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    require_scope(&auth, "peer")?;
    Ok(Json(json!({"applied": state.store.photo_points_apply(&batch)?})))
}

/// Peer grants/shares feed (v47): ONE sharing posture across a user's
/// own nodes — edit anywhere, enforced wherever the friend connects.
async fn sync_sharing_get(
    State(state): State<AppState>,
    Query(params): Query<NotifSinceParams>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
) -> Result<Json<Value>, ApiError> {
    require_scope(&auth, "peer")?;
    Ok(Json(state.store.sharing_sync_export(&params.since)?))
}

async fn sync_sharing_post(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
    Json(batch): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    require_scope(&auth, "peer")?;
    Ok(Json(json!({"applied": state.store.sharing_sync_apply(&batch)?})))
}

#[derive(Deserialize)]
struct HealthIngestBatch {
    actor: String,
    /// Always "apple-health" from the client; accepted for wire
    /// compatibility (the samples' episode source is fixed server-side).
    #[serde(default)]
    #[allow(dead_code)]
    source: String,
    #[serde(default)]
    #[allow(dead_code)]
    device: String,
    #[serde(default)]
    samples: Vec<hearthloom_core::store::HealthSampleRow>,
    #[serde(default)]
    workouts: Vec<crate::live_health::HealthWorkoutIn>,
}

/// Trusted-issuer verification (the provisioner stub): HMAC-SHA256 over
/// the payload with the shared secret in HEARTHLOOM_TRUSTED_ISSUER_SECRET
/// (~/.hearthloom/.env). No secret configured = no issuer accepted.
fn verify_issuer_token(token: &str) -> anyhow::Result<String> {
    use hmac::{Hmac, Mac};
    let secret = std::env::var("HEARTHLOOM_TRUSTED_ISSUER_SECRET")
        .ok()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("no trusted issuer is configured"))?;
    let (payload_b64, sig_b64) = token
        .split_once('.')
        .ok_or_else(|| anyhow::anyhow!("malformed issuer token"))?;
    use base64::Engine;
    let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let payload = engine
        .decode(payload_b64)
        .map_err(|_| anyhow::anyhow!("malformed issuer token"))?;
    let sig = engine
        .decode(sig_b64)
        .map_err(|_| anyhow::anyhow!("malformed issuer token"))?;
    let mut mac = Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes())
        .map_err(|_| anyhow::anyhow!("bad issuer secret"))?;
    mac.update(&payload);
    mac.verify_slice(&sig)
        .map_err(|_| anyhow::anyhow!("issuer signature mismatch"))?;
    let claims: Value = serde_json::from_slice(&payload)?;
    let exp = claims.get("exp").and_then(|e| e.as_i64()).unwrap_or(0);
    if exp < chrono::Utc::now().timestamp() {
        anyhow::bail!("issuer token expired");
    }
    claims
        .get("sub")
        .and_then(|s| s.as_str())
        .map(String::from)
        .ok_or_else(|| anyhow::anyhow!("issuer token has no subject"))
}

async fn auth_logout(
    State(state): State<AppState>,
    req: axum::extract::Request,
) -> Result<Response, ApiError> {
    if let Some(token) = session_cookie(&req) {
        state.store.logout(&token)?;
        state.store.vault_forget(&session_viewer(&token));
    }
    Ok((
        [(
            axum::http::header::SET_COOKIE,
            "hl_session=; HttpOnly; SameSite=Lax; Path=/; Max-Age=0".to_string(),
        )],
        Json(json!({"ok": true})),
    )
        .into_response())
}

/* ————— hybrid peer sync (deployment modes phase 2) —————
   The export feed a paired node pulls from; pairing/status/trigger.
   All traffic is initiated by the node that holds the key (NAT
   reality) — this daemon never dials out except through sync.rs. */

#[derive(Deserialize)]
struct SyncEpisodesParams {
    #[serde(default)]
    after: i64,
    #[serde(default = "default_sync_limit")]
    limit: usize,
}
fn default_sync_limit() -> usize {
    100
}

/// The export feed. Requires a `peer`-scoped PAT ALWAYS (posture-
/// independent, like /brain): this surface exists precisely for
/// machines that are not this machine. Private never appears — the
/// exclusion is hardcoded in store/sync.rs.
async fn sync_episodes(
    State(state): State<AppState>,
    Query(params): Query<SyncEpisodesParams>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
) -> Result<Json<Value>, ApiError> {
    require_scope(&auth, "peer")?;
    let page = state
        .store
        .sync_export(&state.home, params.after, params.limit.min(500))?;
    Ok(Json(json!({
        "episodes": page.episodes,
        "next_cursor": page.next_cursor,
        "drained": page.drained,
        "head": state.store.sync_head()?,
    })))
}

#[derive(Deserialize)]
struct PeerPairBody {
    actor: String,
    /// A name for the counterpart node, e.g. "attic" or "cloud".
    name: String,
    url: String,
    /// The peer-scoped key THAT NODE minted; stored in .env, not the DB.
    key: String,
}

async fn peer_pair(
    State(state): State<AppState>,
    Json(body): Json<PeerPairBody>,
) -> Result<Json<Value>, ApiError> {
    if body.actor.trim().is_empty() {
        return Err(bad_request("actor is required on every write"));
    }
    if !body.url.starts_with("http://") && !body.url.starts_with("https://") {
        return Err(bad_request("peer url must be http(s)://…"));
    }
    if body.key.trim().is_empty() {
        return Err(bad_request("the peer's key is required (mint it on THAT node)"));
    }
    // Probe before committing: the peer must answer its export feed.
    let probe = reqwest::Client::new()
        .get(format!("{}/v1/sync/episodes?after=0&limit=1", body.url.trim_end_matches('/')))
        .bearer_auth(body.key.trim())
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await;
    match probe {
        Ok(res) if res.status().is_success() => {}
        Ok(res) => {
            return Err(bad_request(format!(
                "peer answered {} — check the key (needs the `peer` scope, minted on the peer)",
                res.status()
            )))
        }
        Err(err) => return Err(bad_request(format!("peer unreachable: {err}"))),
    }
    state
        .store
        .set_peer(&Actor::new(body.actor), &body.name, &body.url)?;
    // The always-on peer is where friends should reach this brain.
    if body.url.starts_with("https://") {
        let _ = state.store.set_network_address(&body.url);
    }
    crate::sync::store_peer_key(&state.home, body.key.trim())
        .map_err(|e| bad_request(format!("{e:#}")))?;
    // a NEW pairing always re-offers from zero — stale cursors from a
    // previous peer silently pushed nothing to a fresh node (2026-07-18);
    // idempotency makes this free against a duplicate node
    state.store.set_consolidation_state("peer_push_cursor", "0")?;
    state.store.set_consolidation_state("peer_pull_cursor", "0")?;
    for key in ["peer_scope_push_ts", "peer_scope_pull_ts", "peer_mirror_ts",
                "peer_telemetry_push_ts", "peer_telemetry_pull_ts",
                "peer_notif_push_ts", "peer_notif_pull_ts",
                "peer_verdict_push_ts", "peer_verdict_pull_ts",
                "peer_bus_cursor", "peer_first_sync_notified", "peer_fail_streak"] {
        state.store.set_consolidation_state(key, "")?;
    }
    crate::provisioner::notify_system(
        &state, "info",
        &format!("Paired with {}", body.name),
        "replication starts within minutes; watch progress on the Peering page",
        Some("peer-paired"),
    );
    Ok(Json(json!({"paired": body.name, "url": body.url})))
}

/// Revoke the key THIS REQUEST presented — how an un-peering node
/// retires its own credential on the counterpart without needing admin
/// there. Requires a bearer (a session has no key to revoke).
async fn keys_self_revoke(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
) -> Result<Json<Value>, ApiError> {
    let Some(claims) = &auth.0 else {
        return Err(bad_request("present the key to revoke as the bearer"));
    };
    state
        .store
        .revoke_api_key(&Actor::new(format!("peer:{}", claims.name)), &claims.id)?;
    Ok(Json(json!({"revoked": claims.id})))
}

async fn peer_unpair(
    State(state): State<AppState>,
    Json(body): Json<KeyRevokeBody>, // just {actor}
) -> Result<Json<Value>, ApiError> {
    if body.actor.trim().is_empty() {
        return Err(bad_request("actor is required on every write"));
    }
    // Retire our credential on the counterpart (best-effort — the peer
    // may be offline; the local unpair proceeds regardless, and a
    // leftover key can be revoked from the peer's API Keys page).
    let mut remote_revoked = false;
    if let (Ok(Some((_, url))), Ok(key)) =
        (state.store.peer(), std::env::var("HEARTHLOOM_PEER_KEY"))
    {
        if !key.trim().is_empty() {
            remote_revoked = reqwest::Client::new()
                .post(format!("{url}/v1/keys/self-revoke"))
                .bearer_auth(key.trim())
                // short: a dead peer must not make un-peering FEEL broken
                .timeout(std::time::Duration::from_secs(4))
                .send()
                .await
                .map(|r| r.status().is_success())
                .unwrap_or(false);
        }
    }
    state.store.remove_peer(&Actor::new(body.actor))?;
    crate::sync::remove_peer_key(&state.home).ok();
    Ok(Json(json!({"unpaired": true, "remote_key_revoked": remote_revoked})))
}

async fn peer_status(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    Ok(Json(crate::sync::status(&state)?))
}

/// This node's X25519 sealbox PUBLIC key (Phase 5) — a paired peer fetches it
/// to seal its LLM key to us. Peer-scoped: only a paired counterpart may read
/// it, and a public key is harmless anyway. Mints the keypair on first read.
async fn peer_pubkey(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
) -> Result<Json<Value>, ApiError> {
    require_scope(&auth, "peer")?;
    use base64::Engine;
    let public = crate::peerkey::ensure_keypair(&state.home, &state.store)?;
    Ok(Json(json!({
        "public": base64::engine::general_purpose::STANDARD.encode(public),
    })))
}

/// Run one push+pull cycle right now (the Integrations card's button,
/// and the verification hook). The scheduled loop stays untouched.
#[derive(Deserialize)]
struct PauseBody {
    actor: String,
    paused: bool,
}

/// Pause/resume replication (both scheduled and manual); the flag is
/// pushed to the peer so both ends display the same state.
async fn peer_pause(
    State(state): State<AppState>,
    Json(body): Json<PauseBody>,
) -> Result<Json<Value>, ApiError> {
    if body.actor.trim().is_empty() {
        return Err(bad_request("actor is required on every write"));
    }
    Ok(Json(crate::sync::set_paused(&state, body.paused).await?))
}

#[derive(Deserialize)]
struct PausedInformBody {
    paused: bool,
}

/// The counterpart telling us the shared pause state (peer scope).
async fn sync_paused_informed(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
    Json(body): Json<PausedInformBody>,
) -> Result<Json<Value>, ApiError> {
    require_scope(&auth, "peer")?;
    state.store.set_consolidation_state(
        "peer_sync_paused",
        if body.paused { "1" } else { "0" },
    )?;
    Ok(Json(json!({"paused": body.paused})))
}

#[derive(Deserialize)]
struct SyncNowBody {
    actor: String,
    /// Re-offer the whole record from the start (idempotent).
    #[serde(default)]
    full: bool,
}

async fn peer_sync_now(
    State(state): State<AppState>,
    Json(body): Json<SyncNowBody>,
) -> Result<Json<Value>, ApiError> {
    if body.actor.trim().is_empty() {
        return Err(bad_request("actor is required on every write"));
    }
    let outcome = crate::sync::sync_once_opts(&state, body.full)
        .await
        .map_err(|e| bad_request(format!("{e:#}")))?;
    Ok(Json(outcome))
}

/* ————— networked brains (phase 1): the inbound grant surface,
   outbound friend registry, and grant management ————— */

#[derive(Deserialize)]
struct ScopesSinceParams {
    #[serde(default)]
    since: String,
}

async fn sync_scopes_get(
    State(state): State<AppState>,
    Query(params): Query<ScopesSinceParams>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
) -> Result<Json<Value>, ApiError> {
    require_scope(&auth, "peer")?;
    Ok(Json(json!({
        "changes": state.store.scope_changes_since(&params.since)?,
        // v57 relay cursor (older pullers ignore it and use the stamps)
        "next": state.store.scope_changes_cursor(&params.since)?,
    })))
}

#[derive(Deserialize)]
struct ScopesPostBody {
    changes: Vec<(String, String, String)>,
}

async fn sync_scopes_post(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
    Json(body): Json<ScopesPostBody>,
) -> Result<Json<Value>, ApiError> {
    require_scope(&auth, "peer")?;
    let name = auth.0.as_ref().map(|c| c.name.clone()).unwrap_or_default();
    let applied = state
        .store
        .apply_scope_changes(&Actor::new(format!("peer:{name}")), &body.changes)?;
    Ok(Json(json!({"applied": applied})))
}

#[derive(Deserialize)]
struct TelemetrySinceParams {
    #[serde(default)]
    since: String,
}

async fn sync_telemetry_get(
    State(state): State<AppState>,
    Query(params): Query<TelemetrySinceParams>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
) -> Result<Json<Value>, ApiError> {
    require_scope(&auth, "peer")?;
    Ok(Json(state.store.telemetry_since(&params.since, 2000)?))
}

async fn sync_telemetry_post(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
    Json(batch): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    require_scope(&auth, "peer")?;
    Ok(Json(json!({"applied": state.store.apply_telemetry(&batch)?})))
}

/// v50 ledger peer legs (telemetry pattern: ts watermark, id-keyed
/// replay-safe apply) — either node's Monitoring page shows both
/// nodes' spend.
async fn sync_llm_usage_get(
    State(state): State<AppState>,
    Query(params): Query<TelemetrySinceParams>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
) -> Result<Json<Value>, ApiError> {
    require_scope(&auth, "peer")?;
    Ok(Json(state.store.llm_usage_since(&params.since, 2000)?))
}

async fn sync_llm_usage_post(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
    Json(batch): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    require_scope(&auth, "peer")?;
    Ok(Json(json!({"applied": state.store.apply_llm_usage(&batch)?})))
}

/// v51 unified-location peer legs (telemetry pattern) — the phone posts
/// observations to the tenant, the Mac photo scan lands on the laptop;
/// visit clustering on either node needs the union.
async fn sync_movements_get(
    State(state): State<AppState>,
    Query(params): Query<TelemetrySinceParams>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
) -> Result<Json<Value>, ApiError> {
    require_scope(&auth, "peer")?;
    Ok(Json(state.store.movements_since(&params.since, 2000)?))
}
async fn sync_movements_post(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
    Json(batch): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    require_scope(&auth, "peer")?;
    Ok(Json(json!({"applied": state.store.movements_apply(&batch)?})))
}
async fn sync_vehicles_get(
    State(state): State<AppState>,
    Query(params): Query<TelemetrySinceParams>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
) -> Result<Json<Value>, ApiError> {
    require_scope(&auth, "peer")?;
    Ok(Json(state.store.vehicles_since(&params.since, 500)?))
}
async fn sync_vehicles_post(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
    Json(batch): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    require_scope(&auth, "peer")?;
    Ok(Json(json!({"applied": state.store.vehicles_apply(&batch)?})))
}
async fn sync_locations_get(
    State(state): State<AppState>,
    Query(params): Query<TelemetrySinceParams>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
) -> Result<Json<Value>, ApiError> {
    require_scope(&auth, "peer")?;
    Ok(Json(state.store.observations_since(&params.since, 2000)?))
}

async fn sync_locations_post(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
    Json(batch): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    require_scope(&auth, "peer")?;
    Ok(Json(json!({"applied": state.store.observations_apply(&batch)?})))
}

/// v51 contact-card peer legs (LWW by updated_at — the grants
/// precedent): the address book edits wherever the phone points, both
/// nodes converge, and each node mints its own contact episodes from
/// the applied cards.
async fn sync_contacts_get(
    State(state): State<AppState>,
    Query(params): Query<TelemetrySinceParams>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
) -> Result<Json<Value>, ApiError> {
    require_scope(&auth, "peer")?;
    Ok(Json(state.store.contacts_since(&params.since, 1000)?))
}

async fn sync_contacts_post(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
    Json(batch): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    require_scope(&auth, "peer")?;
    Ok(Json(json!({"applied": state.store.contacts_apply(&batch)?})))
}

/// v51 device-state peer legs (telemetry pattern).
async fn sync_device_state_get(
    State(state): State<AppState>,
    Query(params): Query<TelemetrySinceParams>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
) -> Result<Json<Value>, ApiError> {
    require_scope(&auth, "peer")?;
    Ok(Json(state.store.device_states_since(&params.since, 2000)?))
}

async fn sync_device_state_post(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
    Json(batch): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    require_scope(&auth, "peer")?;
    Ok(Json(json!({"applied": state.store.device_states_apply(&batch)?})))
}

/// LLM traces & accounting are BLANKET-GATED on the vault (Jon
/// 2026-08-16): a trace can quote vaulted content and spend patterns
/// reveal vaulted activity, so a locked vault refuses outright —
/// stricter than scope_guard filtering, deliberately.
fn require_vault_open_for_traces(state: &AppState) -> Result<(), ApiError> {
    if state.store.vault_status().0 {
        return Err(ApiError(
            StatusCode::FORBIDDEN,
            "vault locked — unlock to view LLM traces and accounting".into(),
        ));
    }
    Ok(())
}

/// Pricing map: config `llm_pricing` overrides (substring match) first,
/// then the built-in table. Tokens are truth; cost computed here only.
///
/// The overrides are read from config at most once a minute. `Config::
/// load` also opens the sealed env (a keychain round trip), and this is
/// called per ROW — a 200-row spend report did ~600 of those loads and
/// took over ten seconds before the cache.
pub(crate) fn priced(home: &hearthloom_core::paths::Home, model: &str) -> Option<hearthloom_core::provider::ModelPricing> {
    use std::sync::{Mutex, OnceLock};
    use std::time::{Duration, Instant};
    type Overrides = Vec<(String, hearthloom_core::provider::ModelPricing)>;
    static CACHE: OnceLock<Mutex<Option<(Instant, Overrides)>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(None));
    let overrides: Overrides = {
        let mut slot = cache.lock().unwrap();
        match slot.as_ref() {
            Some((at, list)) if at.elapsed() < Duration::from_secs(60) => list.clone(),
            _ => {
                let list: Overrides = hearthloom_core::config::Config::load(&home.config_dir())
                    .map(|config| {
                        config
                            .llm_pricing
                            .iter()
                            .map(|(key, entry)| (key.to_lowercase(), entry.to_pricing()))
                            .collect()
                    })
                    .unwrap_or_default();
                *slot = Some((Instant::now(), list.clone()));
                list
            }
        }
    };
    let lower = model.to_lowercase();
    overrides
        .iter()
        .find(|(key, _)| lower.contains(key))
        .map(|(_, pricing)| *pricing)
        .or_else(|| hearthloom_core::provider::pricing_for(model))
}

pub(crate) fn attach_cost(home: &hearthloom_core::paths::Home, row: &mut Value) {
    let usage = hearthloom_core::provider::Usage {
        input_tokens: row["input_tokens"].as_i64().unwrap_or(0),
        output_tokens: row["output_tokens"].as_i64().unwrap_or(0),
        cache_creation_tokens: row["cache_creation_tokens"].as_i64().unwrap_or(0),
        cache_read_tokens: row["cache_read_tokens"].as_i64().unwrap_or(0),
    };
    let model = row["model"].as_str().unwrap_or("").to_string();
    row["cost_usd"] = match priced(home, &model) {
        Some(p) => json!(hearthloom_core::provider::usage_cost_usd(&p, &usage)),
        None => Value::Null, // no price on file — show tokens, never invent
    };
}

#[derive(Deserialize)]
struct LlmUsageParams {
    days: Option<i64>,
    node: Option<String>,
}

/// ☰ → Monitoring → "LLM usage": the reconciliation view over the v50
/// ledger — ALL LLM spend (Ask + every consolidation pass), per-day /
/// per-purpose / per-model, tokens + computed cost. Vault-gated.
async fn llm_usage_view(
    State(state): State<AppState>,
    Query(params): Query<LlmUsageParams>,
) -> Result<Json<Value>, ApiError> {
    require_vault_open_for_traces(&state)?;
    let days = params.days.unwrap_or(30).clamp(1, 365);
    let store = state.store.clone();
    let home = state.home.clone();
    let node = params.node.clone();
    let summary = crate::scoped::spawn_blocking(move || -> anyhow::Result<Value> {
        let mut v = store.llm_usage_summary(days, node.as_deref())?;
        for section in ["daily", "purposes", "models"] {
            if let Some(rows) = v[section].as_array_mut() {
                for row in rows {
                    attach_cost(&home, row);
                }
            }
        }
        Ok(v)
    })
    .await
    .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))??;
    Ok(Json(summary))
}

#[derive(Deserialize)]
struct LlmSpendParams {
    /// Inclusive UTC days, YYYY-MM-DD. Default: the last 30 days.
    from: Option<String>,
    to: Option<String>,
    node: Option<String>,
}

/// What prompt caching saved on a row versus paying full input price for
/// the same tokens: reads are cheaper than input, writes dearer. Net.
fn cache_saved_usd(p: &hearthloom_core::provider::ModelPricing, row: &Value) -> f64 {
    let n = |k: &str| row[k].as_i64().unwrap_or(0) as f64;
    (n("cache_read_tokens") * (p.input - p.cache_read)
        - n("cache_creation_tokens") * (p.cache_write - p.input))
        / 1_000_000.0
}

/// ☰ → Spend: every AI model call priced and broken down. Returns the
/// fine-grained cells (day × provider × model × tier × purpose) so the
/// page can regroup without another request, plus month-to-date figures,
/// per-question costs and the dearest questions. Vault-gated like the
/// usage view it grows out of (spend patterns can reveal vaulted
/// activity). Costs are computed at read time from the price table —
/// unknown models carry `cost_usd: null`, never an invented figure.
async fn llm_spend_view(
    State(state): State<AppState>,
    Query(params): Query<LlmSpendParams>,
) -> Result<Json<Value>, ApiError> {
    use chrono::{Datelike, Duration, NaiveDate, Utc};
    require_vault_open_for_traces(&state)?;
    let today = Utc::now().date_naive();
    let day = |raw: &Option<String>, fallback: NaiveDate| -> Result<NaiveDate, ApiError> {
        match raw.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            Some(s) => NaiveDate::parse_from_str(s, "%Y-%m-%d")
                .map_err(|_| bad_request(format!("bad date {s:?} — use YYYY-MM-DD"))),
            None => Ok(fallback),
        }
    };
    let to = day(&params.to, today)?;
    let from = day(&params.from, to - Duration::days(29))?;
    if from > to {
        return Err(bad_request("from is after to"));
    }
    let node = params.node.clone().filter(|n| !n.trim().is_empty());
    let (store, home) = (state.store.clone(), state.home.clone());
    let budget = crate::budget::current(&state);
    let report = crate::scoped::spawn_blocking(move || -> anyhow::Result<Value> {
        let iso = |d: NaiveDate| d.format("%Y-%m-%d").to_string();
        let end = iso(to + Duration::days(1)); // ts < end ⇒ `to` inclusive
        let price = |row: &mut Value| -> Option<f64> {
            attach_cost(&home, row);
            let model = row["model"].as_str().unwrap_or("").to_string();
            let saved = priced(&home, &model).map(|p| cache_saved_usd(&p, row));
            row["cache_saved_usd"] = json!(saved);
            row["cost_usd"].as_f64()
        };

        let mut cells = store.llm_spend_cells(&iso(from), &end, node.as_deref())?;
        let mut unpriced = std::collections::BTreeSet::new();
        for cell in cells.iter_mut() {
            if price(cell).is_none() {
                unpriced.insert(cell["model"].as_str().unwrap_or("").to_string());
            }
        }

        // Month to date — independent of the window being looked at.
        let month_start = today.with_day(1).unwrap_or(today);
        let mut month_cells =
            store.llm_spend_cells(&iso(month_start), &iso(today + Duration::days(1)), node.as_deref())?;
        let (mut month_cost, mut today_cost) = (0.0, 0.0);
        for cell in month_cells.iter_mut() {
            let cost = price(cell).unwrap_or(0.0);
            month_cost += cost;
            if cell["day"].as_str() == Some(iso(today).as_str()) {
                today_cost += cost;
            }
        }
        // What the budget is measured against: the month across every
        // node (the tiles above follow the node filter).
        let budget_spent = match (&node, budget) {
            (_, None) => None,
            (None, Some(_)) => Some(month_cost),
            (Some(_), Some(_)) => {
                let mut all =
                    store.llm_spend_cells(&iso(month_start), &iso(today + Duration::days(1)), None)?;
                Some(all.iter_mut().map(|cell| price(cell).unwrap_or(0.0)).sum::<f64>())
            }
        };
        let next_month = if month_start.month() == 12 {
            NaiveDate::from_ymd_opt(month_start.year() + 1, 1, 1)
        } else {
            NaiveDate::from_ymd_opt(month_start.year(), month_start.month() + 1, 1)
        }
        .unwrap_or(today);
        let days_in_month = (next_month - month_start).num_days().max(1);
        // Project from COMPLETED days only — a half-finished today would
        // drag the rate down every morning.
        let done_days = (today - month_start).num_days();
        let projected = (done_days > 0).then(|| {
            (month_cost - today_cost) / done_days as f64 * days_in_month as f64
        });

        // Per-question cost: fold the (ask, model) rows into one per ask.
        struct AskSpend {
            ts: String,
            calls: i64,
            cost: f64,
            priced: bool,
        }
        let mut asks: std::collections::BTreeMap<String, AskSpend> = Default::default();
        for mut row in store.llm_spend_by_ask(&iso(from), &end, node.as_deref())? {
            let cost = price(&mut row);
            let id = row["ask_id"].as_str().unwrap_or("").to_string();
            let ts = row["ts"].as_str().unwrap_or("").to_string();
            let entry = asks.entry(id).or_insert_with(|| AskSpend {
                ts: ts.clone(),
                calls: 0,
                cost: 0.0,
                priced: true,
            });
            if ts < entry.ts {
                entry.ts = ts;
            }
            entry.calls += row["calls"].as_i64().unwrap_or(0);
            entry.cost += cost.unwrap_or(0.0);
            entry.priced &= cost.is_some();
        }
        let mut costs: Vec<f64> = asks.values().filter(|a| a.priced).map(|a| a.cost).collect();
        costs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let mut ranked: Vec<(&String, &AskSpend)> = asks.iter().collect();
        ranked.sort_by(|a, b| b.1.cost.partial_cmp(&a.1.cost).unwrap_or(std::cmp::Ordering::Equal));
        let top: Vec<Value> = ranked
            .into_iter()
            .take(10)
            .map(|(id, spend)| {
                // The question text, when the ask is still in history
                // (a deleted one keeps its ledger rows but has no text).
                let question = store.ask_get(id).ok().flatten().map(|r| {
                    r.question.chars().take(140).collect::<String>()
                });
                json!({
                    "ask_id": id, "ts": spend.ts, "calls": spend.calls,
                    "cost_usd": spend.cost, "question": question,
                })
            })
            .collect();
        let median = (!costs.is_empty()).then(|| costs[costs.len() / 2]);
        let mean = (!costs.is_empty()).then(|| costs.iter().sum::<f64>() / costs.len() as f64);

        Ok(json!({
            "from": iso(from),
            "to": iso(to),
            "today": iso(today),
            "cells": cells,
            "unpriced_models": unpriced,
            "month": {
                "start": iso(month_start),
                "cost_usd": month_cost,
                "today_usd": today_cost,
                "projected_usd": projected,
                "days_in_month": days_in_month,
            },
            "asks": {
                "count": asks.len(),
                "mean_usd": mean,
                "median_usd": median,
                "costs": costs,
                "top": top,
            },
            "ledger_starts": store.llm_usage_first_ts()?,
            "budget": {
                "monthly_usd": budget,
                // Always both nodes, whatever node the page is filtered to.
                "month_spent_usd": budget_spent,
            },
        }))
    })
    .await
    .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))??;
    Ok(Json(report))
}

#[derive(Deserialize)]
struct LlmBudgetBody {
    /// Dollars per calendar month; null or 0 clears the budget.
    monthly_usd: Option<f64>,
}

/// Set or clear the monthly AI-spend budget (the Spend page's budget
/// tile). An alert threshold, not a cap — see budget.rs.
async fn llm_budget_set(
    State(state): State<AppState>,
    Json(body): Json<LlmBudgetBody>,
) -> Result<Json<Value>, ApiError> {
    let budget = match body.monthly_usd {
        None | Some(0.0) => None,
        Some(b) if b.is_finite() && b > 0.0 && b <= 1_000_000.0 => Some(b),
        Some(_) => return Err(bad_request("the budget must be a positive dollar amount")),
    };
    crate::budget::set(&state, budget)?;
    // A budget already passed should say so now, not at the next cycle.
    let store_state = state.clone();
    let alerted = crate::scoped::spawn_blocking(move || crate::budget::check(&store_state))
        .await
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))??;
    Ok(Json(json!({ "monthly_usd": crate::budget::current(&state), "alerted": alerted })))
}

/// Per-chat debug drill-down: the ask row (full fat transcript — per
/// round tool/args/why/FULL result + LLM latency/tokens) joined with
/// its ledger rows and priced totals. Vault-gated like the aggregate.
async fn ask_trace(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    require_vault_open_for_traces(&state)?;
    let store = state.store.clone();
    let home = state.home.clone();
    let trace = crate::scoped::spawn_blocking(move || -> anyhow::Result<Option<Value>> {
        let Some(record) = store.ask_get(&id)? else {
            return Ok(None);
        };
        let usage = store.llm_usage_for_ref(&id)?;
        let mut totals = json!({
            "calls": usage.len(),
            "input_tokens": usage.iter().map(|u| u["input_tokens"].as_i64().unwrap_or(0)).sum::<i64>(),
            "output_tokens": usage.iter().map(|u| u["output_tokens"].as_i64().unwrap_or(0)).sum::<i64>(),
            "cache_creation_tokens": usage.iter().map(|u| u["cache_creation_tokens"].as_i64().unwrap_or(0)).sum::<i64>(),
            "cache_read_tokens": usage.iter().map(|u| u["cache_read_tokens"].as_i64().unwrap_or(0)).sum::<i64>(),
            "llm_ms": usage.iter().map(|u| u["latency_ms"].as_i64().unwrap_or(0)).sum::<i64>(),
            "errors": usage.iter().filter(|u| u["status"] != "ok").count(),
            "model": usage.first().map(|u| u["model"].clone()).unwrap_or(Value::Null),
        });
        attach_cost(&home, &mut totals);
        let mut usage = usage;
        for row in &mut usage {
            attach_cost(&home, row);
        }
        Ok(Some(json!({
            "ask": serde_json::to_value(record)?,
            "usage": usage,
            "totals": totals,
        })))
    })
    .await
    .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))??;
    match trace {
        Some(v) => Ok(Json(v)),
        None => Err(ApiError(StatusCode::NOT_FOUND, "no such ask".into())),
    }
}

/// Which node am I talking to, what version, is a counterpart known —
/// the Monitoring page banner.
async fn monitor_summary(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let nodes: Vec<String> = {
        // which node tags exist in telemetry (drives the tab set)
        let timeline_local = state.store.health_timeline("local", 24)?;
        let timeline_cloud = state.store.health_timeline("cloud", 24)?;
        let mut v = Vec::new();
        if timeline_local.as_object().map(|o| !o.is_empty()).unwrap_or(false) { v.push("local".into()); }
        if timeline_cloud.as_object().map(|o| !o.is_empty()).unwrap_or(false) { v.push("cloud".into()); }
        v
    };
    Ok(Json(json!({
        "this_node": crate::monitor::node_kind(),
        "llm_configured": state.llm.is_some(),
        "version": env!("CARGO_PKG_VERSION"),
        "nodes_with_data": nodes,
        "paired": state.store.peer()?.map(|(name, url)| json!({"name": name, "url": url})),
        // The sealed-secrets posture: plaintext_fallback=true means
        // secrets sit unencrypted on the data disk (dev mode) — the
        // Monitoring page warns.
        "secrets": hearthloom_core::secrets::status(),
    })))
}

/// â° â Settings: the idle-time automations, each with its stored
/// enable/frequency (defaults applied) and last-run stamp.
/// Error-reporting status for the Settings page: whether this BUILD has
/// a Sentry DSN configured (operator concern) and whether THIS USER has
/// consented (the toggle) — two independent gates, both required.
async fn settings_telemetry(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
) -> Result<Json<Value>, ApiError> {
    if auth.0.is_some() {
        require_scope(&auth, "read")?;
    }
    Ok(Json(crate::apm::status(&state.store)))
}

#[derive(Deserialize)]
struct TelemetrySetBody {
    enabled: bool,
}

/// The consent toggle. Persists as consolidation_state
/// `telemetry.error_reporting` and applies instantly (the SDK's
/// before_send/traces_sampler consult the live flag) — no restart.
async fn settings_telemetry_set(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
    Json(body): Json<TelemetrySetBody>,
) -> Result<Json<Value>, ApiError> {
    if auth.0.is_some() {
        require_scope(&auth, "admin")?;
    }
    state.store.set_consolidation_state(
        "telemetry.error_reporting",
        if body.enabled { "on" } else { "off" },
    )?;
    crate::apm::set_consent(body.enabled);
    Ok(Json(crate::apm::status(&state.store)))
}

/// Send one test event so the operator can confirm the pipeline in the
/// Sentry dashboard. Honest about every gate: no client, or consent
/// off, or the flush not draining each report as their own failure.
async fn settings_telemetry_test(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
) -> Result<Json<Value>, ApiError> {
    if auth.0.is_some() {
        require_scope(&auth, "admin")?;
    }
    let status = crate::apm::status(&state.store);
    if !status["configured"].as_bool().unwrap_or(false) {
        return Ok(Json(
            json!({"sent": false, "reason": "no SENTRY_DSN configured in this build/environment"}),
        ));
    }
    if !status["enabled"].as_bool().unwrap_or(false) {
        return Ok(Json(
            json!({"sent": false, "reason": "error reporting is switched off (Settings)"}),
        ));
    }
    let id = sentry::capture_message(
        "hearthloom telemetry test event (Settings → send test)",
        sentry::Level::Warning,
    );
    let drained = sentry::Hub::current()
        .client()
        .map(|c| c.flush(Some(std::time::Duration::from_secs(5))))
        .unwrap_or(false);
    Ok(Json(json!({
        "sent": drained,
        "event_id": id.to_string(),
        "note": if drained { "delivered to the transport — check the Sentry dashboard" }
                else { "queued but the transport did not drain within 5s" },
    })))
}

async fn settings_automations(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let mut rows = Vec::new();
    for a in crate::consolidate::AUTOMATIONS {
        let (enabled, frequency) =
            crate::consolidate::automation_setting(&state.store, a.key);
        let last_run = state
            .store
            .consolidation_state(&format!("settings.{}.last_run", a.key))?;
        let last_ran_at = state
            .store
            .consolidation_state(&format!("settings.{}.last_ran_at", a.key))?;
        let tier = crate::consolidate::automation_tier(&state.store, a.key);
        let mut row = json!({
            "key": a.key,
            "label": a.label,
            "detail": a.detail,
            "llm": a.llm,
            "default_frequency": a.default_frequency,
            "default_tier": a.default_tier.as_str(),
            "tier": tier.as_str(),
            "enabled": enabled,
            "frequency": frequency,
            "last_run": last_run,
            "last_ran_at": last_ran_at,
            "running": state.is_automation_running(a.key),
        });
        // The visit pass's own setting (the Pair-a-device card shows it).
        if a.key == "visits_finalize" {
            row["calendar_alone"] = json!(crate::visits::calendar_alone(&state.store));
        }
        rows.push(row);
    }
    Ok(Json(json!({
        "automations": rows,
        "frequencies": crate::consolidate::FREQUENCIES
            .iter()
            .map(|(name, _)| *name)
            .collect::<Vec<_>>(),
        "llm_configured": state.llm.is_some(),
    })))
}

#[derive(Deserialize)]
struct AutomationSet {
    key: String,
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    frequency: Option<String>,
    #[serde(default)]
    tier: Option<String>,
    /// visits_finalize only: may a calendar event alone make a visit?
    #[serde(default)]
    calendar_alone: Option<bool>,
}

async fn settings_automation_set(
    State(state): State<AppState>,
    Json(body): Json<AutomationSet>,
) -> Result<Json<Value>, ApiError> {
    if !crate::consolidate::AUTOMATIONS.iter().any(|a| a.key == body.key) {
        return Err(ApiError(StatusCode::BAD_REQUEST, "unknown automation".into()));
    }
    if body.calendar_alone.is_some() && body.key != "visits_finalize" {
        return Err(bad_request("calendar_alone belongs to visits_finalize"));
    }
    if let Some(on) = body.calendar_alone {
        crate::visits::set_calendar_alone(&state.store, on)?;
        // the other node clusters the same days, so tell it now
        crate::bus::touch(&state, "operational.changed");
    }
    if let Some(enabled) = body.enabled {
        state.store.set_consolidation_state(
            &format!("settings.{}.enabled", body.key),
            if enabled { "true" } else { "false" },
        )?;
    }
    if let Some(frequency) = &body.frequency {
        if !crate::consolidate::FREQUENCIES.iter().any(|(n, _)| n == frequency) {
            return Err(ApiError(StatusCode::BAD_REQUEST, "unknown frequency".into()));
        }
        state
            .store
            .set_consolidation_state(&format!("settings.{}.frequency", body.key), frequency)?;
    }
    if let Some(tier) = &body.tier {
        if hearthloom_core::provider::Tier::parse(tier).is_none() {
            return Err(ApiError(StatusCode::BAD_REQUEST, "unknown tier".into()));
        }
        state
            .store
            .set_consolidation_state(&format!("settings.{}.tier", body.key), tier)?;
    }
    let (enabled, frequency) = crate::consolidate::automation_setting(&state.store, &body.key);
    let tier = crate::consolidate::automation_tier(&state.store, &body.key);
    let mut out =
        json!({"key": body.key, "enabled": enabled, "frequency": frequency, "tier": tier.as_str()});
    if body.key == "visits_finalize" {
        out["calendar_alone"] = json!(crate::visits::calendar_alone(&state.store));
    }
    Ok(Json(out))
}

/// Per-source LLM entity-extraction tier (Phase 3b). GET lists every
/// extraction-eligible source with its recommended default and the
/// currently-resolved tier ("disabled" when off).
async fn extraction_status(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let sources: Vec<Value> = crate::consolidate::EXTRACT_DEFAULTS
        .iter()
        .map(|(src, def)| {
            let resolved = crate::consolidate::extraction_tier(&state.store, src);
            json!({
                "source": src,
                "default_tier": def.map(|t| t.as_str()).unwrap_or("disabled"),
                "tier": resolved.map(|t| t.as_str()).unwrap_or("disabled"),
            })
        })
        .collect();
    Ok(Json(json!({
        "sources": sources,
        "llm_configured": state.llm.is_some(),
    })))
}

#[derive(Deserialize)]
struct ExtractionSet {
    source: String,
    tier: String, // low | medium | high | disabled
}

async fn extraction_set(
    State(state): State<AppState>,
    Json(body): Json<ExtractionSet>,
) -> Result<Json<Value>, ApiError> {
    if !crate::consolidate::EXTRACT_DEFAULTS
        .iter()
        .any(|(s, _)| *s == body.source)
    {
        return Err(ApiError(StatusCode::BAD_REQUEST, "unknown source".into()));
    }
    let t = body.tier.trim().to_lowercase();
    let valid = matches!(t.as_str(), "disabled" | "off" | "none")
        || hearthloom_core::provider::Tier::parse(&t).is_some();
    if !valid {
        return Err(ApiError(StatusCode::BAD_REQUEST, "unknown tier".into()));
    }
    state
        .store
        .set_consolidation_state(&format!("extract.{}.tier", body.source), &t)?;
    let resolved = crate::consolidate::extraction_tier(&state.store, &body.source);
    Ok(Json(json!({
        "source": body.source,
        "tier": resolved.map(|t| t.as_str()).unwrap_or("disabled"),
    })))
}

/// Pre-flight cost estimate for a ChatGPT export import (Phase 3c). The
/// ingest itself spends no LLM — this estimates the idle-time ENTITY
/// EXTRACTION that follows, at openai's configured extraction tier, so a
/// user sees the order of magnitude before committing a huge archive.
/// Sizes come from file metadata (instant); token/$ math is deliberately
/// rough (see `approx_input_usd_per_mtok`).
async fn openai_estimate(
    State(state): State<AppState>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Result<Json<Value>, ApiError> {
    // Calibrated against a real 5,288-conversation / 240MB export.
    const AVG_CONV_BYTES: f64 = 47_500.0;
    // Extraction sees a ~1,200-char excerpt/episode (episodes.rs) â ~300 input
    // tokens, plus batched system prompt + a small structured reply.
    const EXTRACT_TOKENS_PER_CONV: f64 = 480.0;

    let dir = params
        .get("path")
        .cloned()
        .filter(|p| !p.trim().is_empty())
        .unwrap_or_else(|| {
            format!(
                "{}/.hearthloom/archives/openai",
                std::env::var("HOME").unwrap_or_default()
            )
        });
    let root = std::path::PathBuf::from(&dir);
    if !root.is_dir() {
        return Ok(Json(json!({ "path": dir, "found": false })));
    }
    let mut bytes: u64 = 0;
    let mut chunks = 0u64;
    if let Ok(rd) = std::fs::read_dir(&root) {
        for e in rd.flatten() {
            let name = e.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("conversations") && name.ends_with(".json") {
                if let Ok(md) = e.metadata() {
                    bytes += md.len();
                    chunks += 1;
                }
            }
        }
    }
    if bytes == 0 {
        return Ok(Json(json!({ "path": dir, "found": false })));
    }
    let est_conversations = (bytes as f64 / AVG_CONV_BYTES).round() as u64;
    let corpus_tokens = (bytes as f64 / 4.0).round() as u64;

    // openai's extraction tier + the model it resolves to under the active provider.
    let tier = crate::consolidate::extraction_tier(&state.store, "openai");
    let (tier_name, extract_usd, extract_tokens, model) = match tier {
        None => ("disabled".to_string(), 0.0, 0u64, String::new()),
        Some(t) => {
            let active = state
                .store
                .consolidation_state("llm.provider")?
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| "anthropic".into());
            let defaults = hearthloom_core::provider::TierModels::defaults_for(&active);
            let model = state
                .store
                .consolidation_state(&format!("llm.model.{}", t.as_str()))
                .ok()
                .flatten()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| defaults.get(t).to_string());
            let tokens = (est_conversations as f64 * EXTRACT_TOKENS_PER_CONV).round() as u64;
            let usd = tokens as f64 / 1_000_000.0
                * hearthloom_core::provider::approx_input_usd_per_mtok(&model);
            (t.as_str().to_string(), usd, tokens, model)
        }
    };

    Ok(Json(json!({
        "path": dir,
        "found": true,
        "chunks": chunks,
        "bytes": bytes,
        "est_conversations": est_conversations,
        "corpus_tokens": corpus_tokens,
        "extract_tier": tier_name,
        "extract_model": model,
        "est_extract_tokens": extract_tokens,
        "est_extract_usd": (extract_usd * 100.0).round() / 100.0,
        "rate_note": "rough estimate; ingest itself is free — this is the idle-time entity extraction",
    })))
}

/// Item 16: scan ~/Downloads for recognizable exports. Cloud nodes (no
/// meaningful Downloads) return an empty list; the UI adapts on
/// `this_node`.
async fn downloads_scan(State(_state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let found = crate::scoped::spawn_blocking(crate::downloads::scan_downloads)
        .await
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(json!({
        "this_node": crate::monitor::node_kind(),
        "found": found,
    })))
}

#[derive(Deserialize)]
struct DownloadsPlaceBody {
    path: String,
    key: String,
    #[serde(default)]
    keep_copy: bool,
}

/// Item 16: move (or copy) a detected export into place — inbox for
/// files, archives/<key> for extracted dirs. Re-detects server-side.
async fn downloads_place(
    State(state): State<AppState>,
    Json(body): Json<DownloadsPlaceBody>,
) -> Result<Json<Value>, ApiError> {
    let home = state.home.clone();
    let (dest, note) = crate::scoped::spawn_blocking(move || {
        crate::downloads::place(&home, &body.path, &body.key, body.keep_copy)
    })
    .await
    .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    .map_err(|e| ApiError(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    Ok(Json(json!({
        "placed": dest.to_string_lossy(),
        "note": note,
    })))
}

/// Manually trigger ONE automation now — an explicit exception to its
/// schedule (it does NOT reset `last_run`). Single-flight: 409 if a
/// scheduled OR manual run already holds it. Runs detached; the UI polls
/// `settings_automations` to watch `running` flip back.
async fn settings_automation_run(
    State(state): State<AppState>,
    Path(key): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let Some(auto) = crate::consolidate::AUTOMATIONS.iter().find(|a| a.key == key) else {
        return Err(ApiError(StatusCode::BAD_REQUEST, "unknown automation".into()));
    };
    if auto.llm && state.llm.is_none() {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "this automation needs an LLM provider, which isn't configured".into(),
        ));
    }
    // Claim the key up front so we can answer 409 synchronously; the guard
    // rides into the detached task and releases when the pass finishes.
    let Some(guard) = state.try_start_automation(&key) else {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "that automation is already running".into(),
        ));
    };
    let bg = state.clone();
    let key2 = key.clone();
    tokio::spawn(async move {
        let _guard = guard; // held for the task's lifetime
        let actor = Actor::new("human:dashboard");
        match crate::consolidate::run_automation(&bg, &key2, &actor).await {
            Ok(v) => crate::monitor::log(
                &bg,
                "info",
                "automation_run",
                &format!("manual run of {key2} finished"),
                Some(v),
            ),
            Err(e) => crate::monitor::log(
                &bg,
                "warn",
                "automation_run",
                &format!("manual run of {key2} failed: {e:#}"),
                None,
            ),
        }
    });
    Ok(Json(json!({ "started": key })))
}

#[derive(Deserialize)]
struct MonitorHealthParams {
    #[serde(default = "default_node")]
    node: String,
    #[serde(default = "default_hours")]
    hours: i64,
}
fn default_node() -> String { "local".into() }
fn default_hours() -> i64 { 24 }

async fn monitor_health(
    State(state): State<AppState>,
    Query(params): Query<MonitorHealthParams>,
) -> Result<Json<Value>, ApiError> {
    let series = |name: &str| state.store.telemetry_series(&params.node, name, params.hours);
    Ok(Json(json!({
        "node": params.node,
        "timeline": state.store.health_timeline(&params.node, params.hours)?,
        "cpu": series("cpu_pct")?,
        "mem": series("mem_mb")?,
        "disk": series("disk_free_gb")?,
        "db": series("db_mb")?,
    })))
}

#[derive(Deserialize)]
struct MonitorLogsParams {
    #[serde(default)]
    node: Option<String>,
    #[serde(default)]
    level: Option<String>,
    #[serde(default = "default_sort")]
    sort: String,
    #[serde(default = "default_desc")]
    desc: bool,
    #[serde(default)]
    offset: usize,
    #[serde(default = "default_log_limit")]
    limit: usize,
}
fn default_sort() -> String { "ts".into() }
fn default_desc() -> bool { true }
fn default_log_limit() -> usize { 50 }

async fn monitor_logs(
    State(state): State<AppState>,
    Query(p): Query<MonitorLogsParams>,
) -> Result<Json<Value>, ApiError> {
    let (rows, total) = state.store.logs(
        p.node.as_deref(),
        p.level.as_deref(),
        &p.sort,
        p.desc,
        p.offset,
        p.limit.min(200),
    )?;
    Ok(Json(json!({"logs": rows, "total": total})))
}

/// A counterpart announcing itself + its progress (peer scope). Renders
/// as the "inbound" card on this node's Peering page.
async fn peer_heartbeat(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    require_scope(&auth, "peer")?;
    let from = auth.0.as_ref().map(|c| c.name.clone()).unwrap_or_default();
    crate::sync::record_inbound(&state, &from, &body)?;
    Ok(Json(json!({
        "ok": true,
        "llm_configured": state.llm.is_some(),
        "version": env!("CARGO_PKG_VERSION"),
    })))
}

/// A spoke pulling the hub's operational state (multi-node M5): what the
/// other nodes mirrored here reaches it. Merged last-writer-wins on the
/// receiving side, exactly like a push.
async fn sync_operational_get(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
) -> Result<Json<Value>, ApiError> {
    require_scope(&auth, "peer")?;
    let (store, home) = (state.store.clone(), state.home.clone());
    let mut out = crate::scoped::spawn_blocking(move || store.export_operational(&home))
        .await
        .map_err(anyhow::Error::from)??;
    // The visit rule travels both ways (visits.rs): set on this node, it
    // reaches the one that dials us through this pull.
    if let (Some(obj), Some(rule)) = (out.as_object_mut(), crate::visits::rule_envelope(&state.store)) {
        obj.insert("visit_rule".into(), rule);
    }
    Ok(Json(out))
}

/// The authoritative node mirroring operational state here (peer scope).
async fn sync_operational(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
    Json(operational): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    require_scope(&auth, "peer")?;
    let applied = state.store.apply_operational(&state.home, &operational)?;
    let llm_key_adopted = apply_llm_envelope(&state, operational.get("llm"));
    // v46: the profile card rides the mirror (LWW by its own stamp) so
    // "who I am" edited on either node follows the record to both.
    if let Some(envelope) = operational.get("profile") {
        if let (Some(value), Some(stamp)) = (
            envelope.get("value").and_then(|v| v.as_str()),
            envelope.get("updated_at").and_then(|v| v.as_str()),
        ) {
            let local_stamp = state
                .store
                .consolidation_state("network.profile.updated_at")?
                .unwrap_or_default();
            if stamp > local_stamp.as_str() {
                state.store.set_consolidation_state("network.profile", value)?;
                state
                    .store
                    .set_consolidation_state("network.profile.updated_at", stamp)?;
            }
        }
    }
    // The monthly spend budget follows the same way (budget.rs).
    if let Some(envelope) = operational.get("llm_budget") {
        if let (Some(value), Some(stamp)) = (
            envelope.get("value").and_then(|v| v.as_str()),
            envelope.get("updated_at").and_then(|v| v.as_str()),
        ) {
            crate::budget::adopt(&state, value, stamp)?;
        }
    }
    // And the visit rule (visits.rs), which both nodes must cluster by.
    if let Some(envelope) = operational.get("visit_rule") {
        crate::visits::adopt_rule(&state.store, envelope)?;
    }
    if llm_key_adopted {
        crate::provisioner::notify_system(
            &state, "info", "AI key adopted from your other node",
            "thinking features come on after the imminent restart",
            Some("llm-key-adopted"),
        );
    }
    Ok(Json(json!({"applied": applied, "llm_key_adopted": llm_key_adopted})))
}

/// The push bus (15b), subscribe side: a counterpart holds a long-lived
/// SSE stream of this node's event HINTS (envelopes of ids/kinds, never
/// content — WILL-ENFORCE stays at the read paths it pulls through).
/// Peer scope for now; friend-scope willingness lands with federation
/// topics. Last-Event-ID resumes from the replay ring; a cursor beyond
/// the ring gets a `resync` event and runs a full reconcile cycle.
async fn bus_stream(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
    headers: HeaderMap,
) -> Result<
    axum::response::Sse<
        impl futures_core::Stream<Item = Result<axum::response::sse::Event, std::convert::Infallible>>,
    >,
    ApiError,
> {
    use axum::response::sse::{Event, KeepAlive};
    // peer scope, or any plugin-bound key: the stream carries event
    // HINTS never data, and every follow-up fetch re-checks scope.
    let plugin_bound = auth
        .0
        .as_ref()
        .is_some_and(|c| crate::plugins::plugin_for_claims(&state, c).is_some());
    if !plugin_bound {
        require_scope(&auth, "peer")?;
    }
    let claims = auth.0.clone().expect("scope checks guarantee claims");
    let cursor = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    let replay = state.bus.replay_since(cursor.as_deref());
    let bus = state.bus.clone();
    let mut rx = bus.subscribe();
    let to_sse = |event: &crate::bus::BusEvent| {
        Event::default()
            .event("bus")
            .id(&event.id)
            .data(serde_json::to_string(event).unwrap_or_default())
    };
    let stream = async_stream::stream! {
        match replay {
            crate::bus::Replay::Resync => {
                // the id jumps the cursor past the gap the subscriber is
                // about to reconcile with a full cycle
                yield Ok(Event::default()
                    .event("resync")
                    .id(bus.head_id().unwrap_or_default())
                    .data("{}"));
            }
            crate::bus::Replay::Events(events) => {
                for event in events {
                    if crate::bus::offered(&claims, &event) {
                        yield Ok(to_sse(&event));
                    }
                }
            }
        }
        loop {
            match rx.recv().await {
                // MAY: willingness consulted live at fan-out, per event.
                Ok(event) if crate::bus::offered(&claims, &event) => yield Ok(to_sse(&event)),
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    yield Ok(Event::default()
                        .event("resync")
                        .id(bus.head_id().unwrap_or_default())
                        .data("{}"));
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    };
    Ok(axum::response::Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(std::time::Duration::from_secs(30))
            .text("hb"),
    ))
}

/// The push bus, publish side: a counterpart POSTs event hints at this
/// node (the NAT-ed laptop can reach us; we can't dial it). Re-enveloped
/// into OUR ring — Last-Event-ID is a per-hop stream position — then the
/// reaction is a pull through the existing sync cycle.
async fn bus_publish(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    require_scope(&auth, "peer")?;
    Ok(Json(crate::bus::receive(&state, &body)))
}

/// Per-adapter import history + live state for the Integrations cards:
/// last `<kind>_ingest_done` mutation each, plus which detached imports
/// hold their single-flight key right now.
async fn imports_status(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let mut last = serde_json::Map::new();
    for (kind, ts, payload) in state.store.latest_ingest_events()? {
        let adapter = kind.trim_end_matches("_ingest_done").to_string();
        last.insert(adapter, json!({"ts": ts, "summary": payload}));
    }
    let running: Vec<&str> = [
        "mbox", "openai", "facebook", "markdown", "notion", "linkedin", "health",
    ]
    .into_iter()
    .filter(|a| state.is_automation_running(&format!("import-{a}")))
    .collect();
    // The interpretation phase: per-source extraction backlog + a rough
    // forecast (~0.6s/episode at batch 8) so a card can say "still
    // interpreting, ~40m left" instead of looking finished.
    let extract_sources: Vec<&str> = crate::consolidate::EXTRACT_DEFAULTS
        .iter()
        .filter(|(s, _)| crate::consolidate::extraction_tier(&state.store, s).is_some())
        .map(|(s, _)| *s)
        .collect();
    let mut interpreting = serde_json::Map::new();
    for (source, pending) in state.store.pending_extraction_counts(&extract_sources)? {
        if pending > 0 {
            interpreting.insert(
                source,
                json!({
                    "pending": pending,
                    "forecast_minutes": ((pending as f64) * 0.6 / 60.0).ceil() as i64,
                }),
            );
        }
    }
    // Poison-batch failures must SURFACE, not vanish: a failed episode
    // leaves the pending pool without ever being interpreted, so a card
    // that only shows "N to go" reads as done when it isn't.
    for (source, failed) in state.store.failed_extraction_counts(&extract_sources)? {
        if failed == 0 {
            continue;
        }
        let error = state
            .store
            .consolidation_state(&format!("extract.{source}.last_error"))
            .ok()
            .flatten()
            .and_then(|v| serde_json::from_str::<Value>(&v).ok());
        let entry = interpreting
            .entry(source)
            .or_insert_with(|| json!({"pending": 0, "forecast_minutes": 0}));
        entry["failed"] = json!(failed);
        if let Some(error) = error {
            entry["error"] = error;
        }
    }
    Ok(Json(json!({
        "last": last,
        "running": running,
        "interpreting": interpreting,
        // extraction runs on whichever node holds the consolidator role
        "llm_here": state.llm.is_some(),
        // A keyed LOCAL node still delegates the idle burn to a healthy
        // peer — the cards must not forecast local throughput then
        // (the frozen "~7m left" Jon watched for hours, 2026-07-19).
        "llm_delegated": crate::sync::llm_delegated_to_peer(&state),
        "llm_blocked": crate::consolidate::llm_blocked(&state.store).is_some(),
    })))
}

/* ————— the full export (design: CLAUDE.md "Full export") ————— */

/// Start the export job. HARD RULE: the vault must be UNLOCKED — the
/// export contains everything, private included, and starting it must
/// be a conscious unvaulted act.
async fn export_start(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let (locked, _) = state.store.vault_status();
    if locked {
        return Err(ApiError(
            StatusCode::FORBIDDEN,
            "unlock the vault first — a full export contains everything, private included".into(),
        ));
    }
    let Some(guard) = state.try_start_automation("export-full") else {
        return Ok(Json(json!({"started": false, "note": "an export is already running"})));
    };
    let bg = state.clone();
    tokio::spawn(async move {
        let _guard = guard;
        crate::export::run_export(bg).await;
    });
    Ok(Json(json!({"started": true})))
}

async fn export_status(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let mut p = crate::export::progress(&state);
    if let Some(obj) = p.as_object_mut() {
        obj.insert("running".into(), json!(state.is_automation_running("export-full")));
        obj.insert("vault_locked".into(), json!(state.store.vault_status().0));
    }
    Ok(Json(p))
}

async fn export_download(State(state): State<AppState>) -> Result<Response, ApiError> {
    let dir = crate::export::export_dir(&state);
    let newest = std::fs::read_dir(&dir)
        .ok()
        .and_then(|entries| {
            entries
                .flatten()
                .filter(|e| e.file_name().to_string_lossy().ends_with(".zip"))
                .max_by_key(|e| e.metadata().and_then(|m| m.modified()).ok())
        })
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "no export is ready — start one first".into()))?;
    let name = newest.file_name().to_string_lossy().into_owned();
    let file = tokio::fs::File::open(newest.path())
        .await
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let stream = tokio_util::io::ReaderStream::new(file);
    let mut resp = axum::body::Body::from_stream(stream).into_response();
    resp.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        "application/zip".parse().unwrap(),
    );
    if let Ok(cd) = format!("attachment; filename=\"{name}\"").parse() {
        resp.headers_mut().insert(axum::http::header::CONTENT_DISPOSITION, cd);
    }
    Ok(resp)
}

/// Fresh-node restore (the wizard's upload): body = the export zip.
/// Stages files + hearthloom.db.incoming, answers, then exits so the
/// supervisor restarts through export::apply_incoming_restore.
async fn import_restore(
    State(state): State<AppState>,
    request: axum::extract::Request,
) -> Result<Json<Value>, ApiError> {
    // STREAM the upload to disk — exports are routinely multi-GB and
    // must never buffer in memory (large imports are the norm).
    use futures_util::StreamExt;
    use tokio::io::AsyncWriteExt;
    let staging = state.home.root().join("exports");
    std::fs::create_dir_all(&staging).map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let upload = staging.join("restore-upload.zip");
    let mut file = tokio::fs::File::create(&upload)
        .await
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let mut stream = request.into_body().into_data_stream();
    let mut received: u64 = 0;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| bad_request(format!("upload interrupted: {e}")))?;
        received += chunk.len() as u64;
        file.write_all(&chunk)
            .await
            .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    }
    file.flush().await.ok();
    drop(file);
    if received == 0 {
        let _ = std::fs::remove_file(&upload);
        return Err(bad_request("upload the export zip as the request body"));
    }
    let out = crate::export::stage_restore(&state, &upload).map_err(|e| bad_request(format!("{e:#}")))?;
    let _ = std::fs::remove_file(&upload);
    tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_millis(800)).await;
        tracing::info!("restore staged — exiting for the supervisor to restart us");
        std::process::exit(0);
    });
    Ok(Json(out))
}

/* ————— onboarding state: "" = active (nag), done, dismissed —
   server-side so it follows the record across browsers/peers ————— */

async fn onboarding_get(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    Ok(Json(json!({
        "state": state.store.consolidation_state("onboarding.state")?.unwrap_or_default(),
    })))
}

#[derive(Deserialize)]
struct OnboardingBody {
    /// "" (fresh record) | active (new-user mode) | done | dismissed
    state: String,
}

async fn onboarding_set(
    State(state): State<AppState>,
    Json(body): Json<OnboardingBody>,
) -> Result<Json<Value>, ApiError> {
    if !["", "active", "done", "dismissed"].contains(&body.state.as_str()) {
        return Err(bad_request("state must be \"\", active, done, or dismissed"));
    }
    state.store.set_consolidation_state("onboarding.state", &body.state)?;
    Ok(Json(json!({"state": body.state})))
}

/* ————— notifications (v38) + the provisioner control plane ————— */

async fn notifications_list(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    Ok(Json(json!({
        "notifications": state.store.notifications(false, 100)?,
        "unread": state.store.notifications_unread()?,
        "killswitch": state.killswitch.load(std::sync::atomic::Ordering::Relaxed),
    })))
}

#[derive(Deserialize)]
struct NotificationCreateBody {
    severity: Option<String>,
    title: String,
    #[serde(default)]
    body: String,
    link: Option<String>,
    link_label: Option<String>,
    dedupe_key: Option<String>,
}

/// The plugin surface: any key with scope `notify` may create — origin
/// derives from the KEY's name, never claimed by the sender; links must
/// pass the structural allowlist (reject, never strip).
async fn notification_create(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
    Json(body): Json<NotificationCreateBody>,
) -> Result<Json<Value>, ApiError> {
    require_scope(&auth, "notify")?;
    let key_name = auth.0.as_ref().map(|c| c.name.clone()).unwrap_or_default();
    let origin = format!("plugin:{key_name}");
    if let Some(link) = body.link.as_deref() {
        hearthloom_core::store::validate_link(link, &crate::provisioner::allowed_origins(&state))
            .map_err(|e| bad_request(format!("{e:#}")))?;
    }
    let id = state.store.notify(
        &Actor::new(format!("agent:{key_name}")),
        &origin,
        body.severity.as_deref().unwrap_or("info"),
        &body.title,
        &body.body,
        body.link.as_deref().map(|l| (l, body.link_label.as_deref())),
        body.dedupe_key.as_deref(),
    )?;
    crate::bus::touch(&state, "notification.created");
    Ok(Json(json!({"id": id})))
}

async fn notification_read(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(json!({"changed": state.store.notification_mark(Some(&id), "read")?})))
}

async fn notification_dismiss(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(json!({"changed": state.store.notification_mark(Some(&id), "dismiss")?})))
}

async fn notifications_read_all(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    Ok(Json(json!({"changed": state.store.notification_mark(None, "read_all")?})))
}

/// Extract + verify the control token (X-Provisioner-Token). 404 when no
/// provisioner is configured — the surface simply doesn't exist.
fn require_provisioner(headers: &HeaderMap) -> Result<String, ApiError> {
    if crate::provisioner::secret().is_none() {
        return Err(ApiError(StatusCode::NOT_FOUND, "not found".into()));
    }
    let token = headers
        .get("x-provisioner-token")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| ApiError(StatusCode::UNAUTHORIZED, "control token required".into()))?;
    crate::provisioner::verify_control_token(token)
        .map_err(|e| ApiError(StatusCode::UNAUTHORIZED, format!("{e:#}")))
}

#[derive(Deserialize)]
struct KillswitchBody {
    active: bool,
    #[serde(default)]
    message: String,
    link: Option<String>,
    allow_export: Option<bool>,
}

async fn provisioner_killswitch(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<KillswitchBody>,
) -> Result<Json<Value>, ApiError> {
    let sub = require_provisioner(&headers)?;
    if let Some(link) = body.link.as_deref().filter(|l| !l.is_empty()) {
        hearthloom_core::store::validate_link(link, &crate::provisioner::allowed_origins(&state))
            .map_err(|e| bad_request(format!("{e:#}")))?;
    }
    crate::provisioner::set_killswitch(
        &state, &sub, body.active, &body.message, body.link.as_deref(), body.allow_export,
    )?;
    Ok(Json(json!({"active": body.active})))
}

#[derive(Deserialize)]
struct ProvisionerNotifyBody {
    severity: Option<String>,
    title: String,
    #[serde(default)]
    body: String,
    link: Option<String>,
    link_label: Option<String>,
    dedupe_key: Option<String>,
}

async fn provisioner_notify(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<ProvisionerNotifyBody>,
) -> Result<Json<Value>, ApiError> {
    let sub = require_provisioner(&headers)?;
    if let Some(link) = body.link.as_deref() {
        hearthloom_core::store::validate_link(link, &crate::provisioner::allowed_origins(&state))
            .map_err(|e| bad_request(format!("{e:#}")))?;
    }
    let id = state.store.notify(
        &Actor::new(format!("provisioner:{sub}")),
        "provisioner",
        body.severity.as_deref().unwrap_or("info"),
        &body.title,
        &body.body,
        body.link.as_deref().map(|l| (l, body.link_label.as_deref())),
        body.dedupe_key.as_deref(),
    )?;
    crate::bus::touch(&state, "notification.created");
    Ok(Json(json!({"id": id})))
}

#[derive(Deserialize)]
struct LinkDomainsBody {
    domains: Vec<String>,
}

async fn provisioner_link_domains(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<LinkDomainsBody>,
) -> Result<Json<Value>, ApiError> {
    require_provisioner(&headers)?;
    crate::provisioner::set_link_domains(&state, &body.domains)?;
    Ok(Json(json!({"domains": body.domains})))
}

async fn provisioner_status(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    require_provisioner(&headers)?;
    // Fleet-health fields are ADDITIVE-ONLY (wire discipline): a
    // provisioner must ignore unknown fields and never require new ones.
    let last_sync: Option<Value> = state
        .store
        .consolidation_state("peer_last_outcome")
        .ok()
        .flatten()
        .and_then(|v| serde_json::from_str(&v).ok());
    Ok(Json(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "killswitch": state.killswitch.load(std::sync::atomic::Ordering::Relaxed),
        "episodes": state.store.sync_head().unwrap_or(0),
        "node": crate::monitor::node_kind(),
        "db_bytes": std::fs::metadata(state.home.db_path()).map(|m| m.len()).unwrap_or(0),
        "unread_notifications": state.store.notifications_unread().unwrap_or(0),
        "auth_locked": state.auth_locked.load(std::sync::atomic::Ordering::Relaxed),
        "last_peer_sync": last_sync,
        "llm_configured": state.llm.is_some(),
        // needs-real-attention faults (NOT the killswitch fence):
        // billing-class llm blocks, sustained sync failure — additive.
        "degraded": degraded_components(&state),
    })))
}

fn degraded_components(state: &AppState) -> Vec<Value> {
    let mut out = Vec::new();
    if let Some(raw) = crate::consolidate::llm_blocked(&state.store) {
        let parsed: Value = serde_json::from_str(&raw).unwrap_or(json!({"detail": raw}));
        out.push(json!({
            "component": "llm",
            "class": parsed.get("class").and_then(|c| c.as_str()).unwrap_or("billing"),
            "since": parsed.get("since").cloned().unwrap_or(Value::Null),
            "detail": parsed.get("detail").cloned().unwrap_or(Value::Null),
        }));
    }
    let streak: i64 = state
        .store
        .consolidation_state("peer_fail_streak")
        .ok()
        .flatten()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    if streak >= 5 {
        out.push(json!({
            "component": "peer-sync",
            "class": "transient",
            "detail": format!("{streak} consecutive failed cycles"),
        }));
    }
    out
}

#[derive(Deserialize)]
struct NotifSinceParams {
    #[serde(default)]
    since: String,
}

async fn sync_extractions_get(
    State(state): State<AppState>,
    Query(params): Query<NotifSinceParams>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
) -> Result<Json<Value>, ApiError> {
    require_scope(&auth, "peer")?;
    Ok(Json(state.store.verdicts_since(&params.since, 500)?))
}

async fn sync_extractions_post(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
    Json(batch): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    require_scope(&auth, "peer")?;
    let name = auth.0.as_ref().map(|c| c.name.clone()).unwrap_or_default();
    let applied = state
        .store
        .apply_verdicts(&Actor::new(format!("peer:{name}")), &batch)?;
    Ok(Json(json!({"applied": applied})))
}

#[derive(Deserialize)]
struct JournalAfterParams {
    #[serde(default)]
    after: i64,
}

/// The operational-journal feed (v43 Phase 3): entries after a seq
/// cursor, for idempotent peer replay. Delete tombstones are the one
/// leg that removes rows across nodes — LWW-gated on apply.
async fn sync_journal_get(
    State(state): State<AppState>,
    Query(params): Query<JournalAfterParams>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
) -> Result<Json<Value>, ApiError> {
    require_scope(&auth, "peer")?;
    // relay what other nodes sent; skip only the caller's own entries
    let caller = auth.0.as_ref().map(|c| c.name.clone()).unwrap_or_default();
    Ok(Json(state.store.journal_export_for(params.after, 500, Some(&caller))?))
}

async fn sync_journal_post(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
    Json(batch): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    require_scope(&auth, "peer")?;
    let name = auth.0.as_ref().map(|c| c.name.clone()).unwrap_or_default();
    let applied = state
        .store
        .apply_journal(&Actor::new(format!("peer:{name}")), &batch)?;
    Ok(Json(json!({"applied": applied})))
}

/// Billing fixed — the explicit resume (transient failures never need
/// this; their pending pool resumes on its own).
async fn llm_resume(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    state.store.set_consolidation_state("llm.blocked", "")?;
    crate::provisioner::notify_system(
        &state, "info", "AI provider resumed",
        "thinking passes pick the backlog back up from the next cycle",
        Some("llm-blocked"),
    );
    Ok(Json(json!({"resumed": true})))
}

#[derive(Deserialize)]
struct RerunBody {
    actor: String,
    source: String,
    /// re-interpret everything whose verdict came from a WEAKER tier
    tier: String,
}

async fn extraction_rerun(
    State(state): State<AppState>,
    Json(body): Json<RerunBody>,
) -> Result<Json<Value>, ApiError> {
    if body.actor.trim().is_empty() {
        return Err(bad_request("actor is required on every write"));
    }
    let requeued = state.store.requeue_below_tier(&body.source, &body.tier)?;
    Ok(Json(json!({
        "requeued": requeued,
        "note": "the next consolidation cycles re-interpret them; stronger verdicts supersede",
    })))
}

#[derive(Deserialize)]
struct RetryFailedBody {
    source: String,
}

/// The Integrations card's "try again" for poison-batch failures:
/// put the source's failed episodes back in the pending pool and
/// clear the captured error.
async fn extraction_retry_failed(
    State(state): State<AppState>,
    Json(body): Json<RetryFailedBody>,
) -> Result<Json<Value>, ApiError> {
    let requeued = state.store.requeue_failed_extraction(&body.source)?;
    state
        .store
        .set_consolidation_state(&format!("extract.{}.last_error", body.source), "")?;
    Ok(Json(json!({"requeued": requeued})))
}

async fn sync_notifications_get(
    State(state): State<AppState>,
    Query(params): Query<NotifSinceParams>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
) -> Result<Json<Value>, ApiError> {
    require_scope(&auth, "peer")?;
    Ok(Json(state.store.notifications_since(&params.since, 500)?))
}

async fn sync_notifications_post(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
    Json(batch): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    require_scope(&auth, "peer")?;
    Ok(Json(json!({"applied": state.store.apply_notifications(&batch)?})))
}

/// Phase-5 receiver: adopt the sender's LLM config from the operational
/// mirror's `llm` envelope. Clear (non-secret) provider + tier models go
/// straight to consolidation_state; the SEALED api key is unsealed with this
/// node's secret and, ONLY IF it differs from what we already hold, persisted
/// and a restart scheduled to load it. Comparing the DECRYPTED key (not the
/// ever-fresh ciphertext) keeps this idempotent — no restart loop. Returns
/// true when a NEW key was adopted (a restart is imminent).
fn apply_llm_envelope(state: &AppState, llm: Option<&Value>) -> bool {
    let Some(llm) = llm.and_then(|v| v.as_object()) else {
        return false;
    };
    // Clear config first — harmless, non-secret, and useful even keyless.
    let provider = llm
        .get("provider")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("anthropic")
        .to_string();
    let _ = state.store.set_consolidation_state("llm.provider", &provider);
    for tier in ["low", "medium", "high"] {
        if let Some(m) = llm
            .get(&format!("model_{tier}"))
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
        {
            let _ = state
                .store
                .set_consolidation_state(&format!("llm.model.{tier}"), m);
        }
    }
    // The sealed key.
    let Some(sealed) = llm.get("sealed_key").and_then(|v| v.as_str()) else {
        return false;
    };
    let key = match crate::peerkey::open_local(sealed) {
        Ok(bytes) => match String::from_utf8(bytes) {
            Ok(k) => k,
            Err(_) => return false,
        },
        // Not our box, or no secret yet — silently ignore (best-effort mirror).
        Err(_) => return false,
    };
    if key.trim().is_empty() {
        return false;
    }
    // Adopt ONLY into an EMPTY slot for this provider — fill a keyless peer,
    // or a newly-switched provider, but NEVER overwrite a key already set by
    // hand. That keeps sync purely additive: no surprise restart on a
    // configured node, and no restart fight if the two nodes somehow hold
    // different keys for the same provider (rotation stays a deliberate
    // per-node act). See the design log 2026-07-13.
    let env = crate::integrations::provider_key_env(&provider);
    if std::env::var(env).ok().is_some_and(|k| !k.trim().is_empty()) {
        return false; // already keyed for this provider — leave it
    }
    if hearthloom_core::secrets::set_env_vars(state.home.root(), &[(env, &key)]).is_err() {
        return false;
    }
    crate::monitor::log(
        state,
        "info",
        "llm_key_synced",
        &format!("adopted {provider} key from peer — restarting to load it"),
        None,
    );
    let _ = state.store.record_event(
        &Actor::new("peer:sync"),
        "llm_key_synced",
        &json!({ "provider": provider }),
    );
    tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_millis(800)).await;
        std::process::exit(0);
    });
    true
}

/// A peered node replicating same-owner declared state (peer scope).
async fn sync_declared(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
    Json(declared): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    require_scope(&auth, "peer")?;
    let applied = state.store.apply_declared(&declared)?;
    Ok(Json(json!({"applied": applied})))
}

/// Lightweight discovery: where this brain prefers to be reached (its
/// always-on node when one exists). Friend-scoped so only connections
/// see it.
async fn network_hello(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
) -> Result<Json<Value>, ApiError> {
    require_scope(&auth, "friend")?;
    Ok(Json(json!({
        "preferred_url": state.store.network_address()?,
        // CAN: capability strings, version-tied (wire discipline —
        // callers feature-detect; never gate on the release string)
        "capabilities": ["search-v1", "fetch-v1", "profile-v1", "tasks-v1",
                         "notes-v1", "relationships-v1", "shared-v1",
                         "toggle-v1", "offer-v1", "groups-v1"],
    })))
}

/// The scope every friend-surface read runs under, resolved by the key's
/// NAME (callers cannot choose whose grant they use).
fn friend_name(auth: &AuthInfo) -> String {
    auth.0.as_ref().map(|c| c.name.clone()).unwrap_or_default()
}

/// My calling card, as served to a connected friend (class `profile`).
async fn network_profile(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
) -> Result<Json<Value>, ApiError> {
    require_scope(&auth, "friend")?;
    let friend = friend_name(&auth);
    let store = state.store.clone();
    let card = crate::scoped::spawn_blocking(move || -> anyhow::Result<Value> {
        // direct grant or any group they belong to (store/groups.rs)
        if !store.access(&friend)?.any(|g| g.has_class("profile")) {
            anyhow::bail!("the grant for {friend:?} doesn't cover the profile card");
        }
        let mut card: Value = store
            .consolidation_state("network.profile")?
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_else(|| json!({}));
        if card.get("name").and_then(|n| n.as_str()).map(str::trim).unwrap_or("").is_empty() {
            // an uncurated card still has a name: the admin's email localpart
            let fallback = store
                .admin_email()?
                .and_then(|e| e.split('@').next().map(String::from))
                .unwrap_or_else(|| "a hearthloom".into());
            card["name"] = json!(fallback);
        }
        Ok(card)
    })
    .await
    .map_err(anyhow::Error::from)?
    .map_err(|e| ApiError(StatusCode::FORBIDDEN, format!("{e:#}")))?;
    let mut out = card;
    out["preferred_url"] = json!(state.store.network_address()?);
    Ok(Json(out))
}

#[derive(Deserialize)]
struct NetworkTasksParams {
    #[serde(default = "default_task_status")]
    status: String,
    #[serde(default = "default_sync_limit")]
    limit: usize,
}
fn default_task_status() -> String {
    "open".into()
}

async fn network_tasks_get(
    State(state): State<AppState>,
    Query(params): Query<NetworkTasksParams>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
) -> Result<Json<Value>, ApiError> {
    require_scope(&auth, "friend")?;
    let friend = friend_name(&auth);
    let store = state.store.clone();
    let tasks = crate::scoped::spawn_blocking(move || {
        store.network_tasks(&friend, &params.status, params.limit.min(200))
    })
    .await
    .map_err(anyhow::Error::from)?
    .map_err(|e| ApiError(StatusCode::FORBIDDEN, format!("{e:#}")))?;
    Ok(Json(json!({"tasks": tasks, "preferred_url": state.store.network_address()?})))
}

async fn network_relationships_get(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
) -> Result<Json<Value>, ApiError> {
    require_scope(&auth, "friend")?;
    let friend = friend_name(&auth);
    let store = state.store.clone();
    let relationships = crate::scoped::spawn_blocking(move || store.network_relationships(&friend, 200))
        .await
        .map_err(anyhow::Error::from)?
        .map_err(|e| ApiError(StatusCode::FORBIDDEN, format!("{e:#}")))?;
    Ok(Json(json!({"relationships": relationships,
                   "preferred_url": state.store.network_address()?})))
}

async fn network_notes_get(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
) -> Result<Json<Value>, ApiError> {
    require_scope(&auth, "friend")?;
    let friend = friend_name(&auth);
    let store = state.store.clone();
    let notes = crate::scoped::spawn_blocking(move || store.network_notes(&friend, 200))
        .await
        .map_err(anyhow::Error::from)?
        .map_err(|e| ApiError(StatusCode::FORBIDDEN, format!("{e:#}")))?;
    Ok(Json(json!({"notes": notes, "preferred_url": state.store.network_address()?})))
}

async fn network_note_get(
    State(state): State<AppState>,
    Path(id): Path<String>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
) -> Result<Json<Value>, ApiError> {
    require_scope(&auth, "friend")?;
    let friend = friend_name(&auth);
    let store = state.store.clone();
    let home = state.home.clone();
    let out = crate::scoped::spawn_blocking(move || -> anyhow::Result<Value> {
        if !store.network_note_allowed(&friend, &id)? {
            anyhow::bail!("that note isn't shared with {friend:?}");
        }
        let (row, body) = store
            .note(&home, &id)?
            .ok_or_else(|| anyhow::anyhow!("no such note"))?;
        store.log_network_access_pub(&friend, "note", &json!({"id": id}))?;
        Ok(json!({"id": row.id, "title": row.title, "tags": row.tags, "body": body}))
    })
    .await
    .map_err(anyhow::Error::from)?
    .map_err(|e| ApiError(StatusCode::FORBIDDEN, format!("{e:#}")))?;
    Ok(Json(out))
}

/// The MAY descriptor: what this caller is currently offered — a
/// ceiling and a courtesy, never the enforcement.
async fn network_offer(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
) -> Result<Json<Value>, ApiError> {
    require_scope(&auth, "friend")?;
    let friend = friend_name(&auth);
    let store = state.store.clone();
    let out = crate::scoped::spawn_blocking(move || -> anyhow::Result<Value> {
        // The advertised CEILING across the direct grant and every group
        // — wider than what any single read serves; never enforcement.
        let access = store.access(&friend)?;
        let grant = access.ceiling();
        // The exact picture, grant by grant (additive field): the top-level
        // keys pair kinds and life areas that no single grant pairs.
        let mut grants = vec![json!({"via": "direct", "grant": access.direct})];
        for (group, group_grant) in &access.groups {
            grants.push(json!({"via": "group", "group": group.name, "grant": group_grant}));
        }
        let bundle_cursor = store
            .network_shared_cursor(&friend)?;
        Ok(json!({
            "grants": grants,
            "classes": grant.classes, "spheres": grant.spheres,
            "detail": grant.detail, "completed_tasks": grant.completed_tasks,
            "evidence_relationships": grant.evidence_relationships,
            "shared_cursor": bundle_cursor,
        }))
    })
    .await
    .map_err(anyhow::Error::from)?
    .map_err(|e| ApiError(StatusCode::FORBIDDEN, format!("{e:#}")))?;
    Ok(Json(out))
}

/// The groups this caller has been put in on THIS record: name and
/// roster, so their node can show "you're in Ada's Family group".
async fn network_groups_get(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
) -> Result<Json<Value>, ApiError> {
    require_scope(&auth, "friend")?;
    let friend = friend_name(&auth);
    let store = state.store.clone();
    let groups = crate::scoped::spawn_blocking(move || store.network_groups(&friend))
        .await
        .map_err(anyhow::Error::from)?
        .map_err(|e| ApiError(StatusCode::FORBIDDEN, format!("{e:#}")))?;
    Ok(Json(json!({"groups": groups})))
}

/// Owner side: every group with its roster and settings, who can be
/// added (active connections), and the groups other people's records
/// have put this one in.
async fn groups_list(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let store = state.store.clone();
    let out = crate::scoped::spawn_blocking(move || -> anyhow::Result<Value> {
        let shares = store.all_shares()?;
        let groups: Vec<Value> = store
            .groups()?
            .into_iter()
            .map(|g| {
                let key = g.key();
                let shared = shares.iter().filter(|s| s["friend"] == json!(key)).count();
                json!({
                    "id": g.id, "key": key, "name": g.name, "members": g.members,
                    "sharing": g.sharing, "shared_items": shared, "updated_at": g.updated_at,
                })
            })
            .collect();
        let connections: Vec<String> = store
            .list_grants()?
            .into_iter()
            .filter(|g| g.revoked_at.is_none())
            .map(|g| g.friend)
            .collect();
        Ok(json!({
            "groups": groups,
            "connections": connections,
            "member_of": store.peer_groups()?,
            "spheres": hearthloom_core::store::GRANTABLE_SPHERES,
            "classes": hearthloom_core::store::SHARE_CLASSES,
        }))
    })
    .await
    .map_err(anyhow::Error::from)??;
    Ok(Json(out))
}

#[derive(Deserialize)]
struct GroupMutateBody {
    actor: String,
    action: String, // create | rename | members | sharing | delete
    #[serde(default)]
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    members: Vec<String>,
    #[serde(default)]
    sharing: Value,
}

/// Owner side: create a group, rename it, set its roster, set what it
/// grants, or delete it. Only the owner's node writes groups.
async fn groups_mutate(
    State(state): State<AppState>,
    Json(body): Json<GroupMutateBody>,
) -> Result<Json<Value>, ApiError> {
    if body.actor.trim().is_empty() {
        return Err(bad_request("actor is required on every write"));
    }
    let actor = Actor::new(body.actor);
    let store = &state.store;
    let refuse = |e: anyhow::Error| bad_request(format!("{e:#}"));
    let out = match body.action.as_str() {
        "create" => json!({"group": store.create_group(&actor, &body.name).map_err(refuse)?}),
        "rename" => json!({"group": store.rename_group(&actor, &body.id, &body.name).map_err(refuse)?}),
        "members" => {
            json!({"group": store.set_group_members(&actor, &body.id, &body.members).map_err(refuse)?})
        }
        "sharing" => {
            json!({"group": store.set_group_sharing(&actor, &body.id, &body.sharing).map_err(refuse)?})
        }
        "delete" => json!({"deleted": store.delete_group(&actor, &body.id)?}),
        _ => return Err(bad_request("action must be create|rename|members|sharing|delete")),
    };
    crate::bus::touch(&state, "operational.changed");
    Ok(Json(out))
}

async fn network_shared_get(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
) -> Result<Json<Value>, ApiError> {
    require_scope(&auth, "friend")?;
    let friend = friend_name(&auth);
    let store = state.store.clone();
    let out = crate::scoped::spawn_blocking(move || store.network_shared_bundle(&friend))
        .await
        .map_err(anyhow::Error::from)?
        .map_err(|e| ApiError(StatusCode::FORBIDDEN, format!("{e:#}")))?;
    Ok(Json(out))
}

#[derive(Deserialize)]
struct ToggleBody {
    index: usize,
    text: String,
    done: bool,
}

/// The ONE friend write: checklist toggles on collaborate-shared tasks.
async fn network_task_toggle(
    State(state): State<AppState>,
    Path(id): Path<String>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
    Json(body): Json<ToggleBody>,
) -> Result<Json<Value>, ApiError> {
    require_scope(&auth, "friend")?;
    let friend = friend_name(&auth);
    let (fname, text, done) = (friend.clone(), body.text.clone(), body.done);
    let store = state.store.clone();
    let out = crate::scoped::spawn_blocking(move || {
        store.friend_toggle_checklist(&friend, &id, body.index, &body.text, body.done)
    })
    .await
    .map_err(anyhow::Error::from)?
    .map_err(|e| ApiError(StatusCode::FORBIDDEN, format!("{e:#}")))?;
    crate::bus::touch(&state, "operational.changed");
    // activity notification: the owner learns about the friend's tick
    let what = out.get("done").and_then(|d| d.as_i64()).unwrap_or(0);
    let total = out.get("total").and_then(|t| t.as_i64()).unwrap_or(0);
    crate::provisioner::notify_system(
        &state, "info",
        &format!("{fname} {} an item", if done { "checked off" } else { "unchecked" }),
        &format!("\"{text}\" — now {what} of {total} done"),
        None,
    );
    Ok(Json(out))
}

/* ————— owner-side sharing surfaces ————— */

/// My calling card, edited from Settings (session/PAT auth, not friend).
async fn mycard_get(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let card: Value = state
        .store
        .consolidation_state("network.profile")?
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| json!({}));
    Ok(Json(card))
}

#[derive(Deserialize)]
struct MyCardBody {
    actor: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    emails: Vec<String>,
    #[serde(default)]
    emoji: String,
    #[serde(default)]
    color: String,
    #[serde(default)]
    bio: String,
    /// Either an https URL to an avatar image, or a data: URI from the
    /// upload path (≤160KB — it travels inside the card, so friends
    /// render it with no cross-origin fetch and it peer-syncs for free).
    #[serde(default)]
    avatar: String,
}

async fn mycard_set(
    State(state): State<AppState>,
    Json(body): Json<MyCardBody>,
) -> Result<Json<Value>, ApiError> {
    if body.actor.trim().is_empty() {
        return Err(bad_request("actor is required on every write"));
    }
    let avatar = body.avatar.trim();
    if avatar.len() > 1_400_000 {
        return Err(bad_request("that avatar is too big — keep it under 1MB"));
    }
    if !avatar.is_empty() && !avatar.starts_with("https://") && !avatar.starts_with("data:image/") {
        return Err(bad_request("avatar must be an https:// URL or an uploaded image"));
    }
    let card = json!({
        "name": body.name.trim(),
        "emails": body.emails.iter().map(|e| e.trim().to_lowercase())
            .filter(|e| e.contains('@')).collect::<Vec<_>>(),
        "emoji": body.emoji.trim(), "color": body.color.trim(),
        "bio": body.bio.trim(),
        "avatar": avatar,
    });
    state
        .store
        .set_consolidation_state("network.profile", &card.to_string())?;
    // the LWW stamp the peer mirror uses — the card follows the record
    state
        .store
        .set_consolidation_state("network.profile.updated_at", &hearthloom_core::time::now_utc())?;
    Ok(Json(card))
}

async fn grants_default_get(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    Ok(Json(state.store.default_sharing()?))
}

#[derive(Deserialize)]
struct DefaultSharingBody {
    actor: String,
    sharing: Value,
}

async fn grants_default_set(
    State(state): State<AppState>,
    Json(body): Json<DefaultSharingBody>,
) -> Result<Json<Value>, ApiError> {
    if body.actor.trim().is_empty() {
        return Err(bad_request("actor is required on every write"));
    }
    let clean = state
        .store
        .set_default_sharing(&Actor::new(body.actor), &body.sharing)
        .map_err(|e| bad_request(format!("{e:#}")))?;
    Ok(Json(clean))
}

#[derive(Deserialize)]
struct SharingListParams {
    #[serde(default)]
    kind: String,
    #[serde(default)]
    item_id: String,
    #[serde(default)]
    friend: String,
}

async fn sharing_list(
    State(state): State<AppState>,
    Query(params): Query<SharingListParams>,
) -> Result<Json<Value>, ApiError> {
    if !params.friend.is_empty() {
        return Ok(Json(json!({"shares": state.store.shares_to_friend(&params.friend)?})));
    }
    if !params.kind.is_empty() && !params.item_id.is_empty() {
        return Ok(Json(json!({"shares": state.store.item_shares(&params.kind, &params.item_id)?})));
    }
    // no filter: everything — one fetch paints every badge
    Ok(Json(json!({"shares": state.store.all_shares()?})))
}

#[derive(Deserialize)]
struct FriendSharingBody {
    actor: String,
    friend: String,
    sharing: Value,
}

/// Per-friend sharing overrides (tri-state — absent keys inherit the
/// default template).
async fn grants_sharing_set(
    State(state): State<AppState>,
    Json(body): Json<FriendSharingBody>,
) -> Result<Json<Value>, ApiError> {
    if body.actor.trim().is_empty() {
        return Err(bad_request("actor is required on every write"));
    }
    let clean = state
        .store
        .set_friend_sharing(&Actor::new(body.actor), &body.friend, &body.sharing)
        .map_err(|e| bad_request(format!("{e:#}")))?;
    Ok(Json(clean))
}

#[derive(Deserialize)]
struct SharingMutateBody {
    actor: String,
    action: String, // share | unshare
    kind: String,
    item_id: String,
    friend: String,
    #[serde(default)]
    collaborate: bool,
    #[serde(default)]
    expires_at: Option<String>,
}

#[derive(Deserialize)]
struct UnclaimBody {
    actor: String,
    kind: String,
    value: String,
}

#[derive(Deserialize)]
struct RetriageBody { actor: String }

async fn reviews_retriage(
    State(state): State<AppState>,
    Json(body): Json<RetriageBody>,
) -> Result<Json<Value>, ApiError> {
    if body.actor.trim().is_empty() {
        return Err(bad_request("actor is required on every write"));
    }
    let store = state.store.clone();
    let cleared = crate::scoped::spawn_blocking(move || {
        store.retriage_reviews(&Actor::new(body.actor))
    })
    .await
    .map_err(anyhow::Error::from)??;
    Ok(Json(json!({"cleared": cleared})))
}

async fn identity_unclaim(
    State(state): State<AppState>,
    Json(body): Json<UnclaimBody>,
) -> Result<Json<Value>, ApiError> {
    if body.actor.trim().is_empty() {
        return Err(bad_request("actor is required on every write"));
    }
    let removed = state
        .store
        .unclaim_identity(&Actor::new(body.actor), &body.kind, &body.value)?;
    Ok(Json(json!({"removed": removed})))
}

async fn sharing_mutate(
    State(state): State<AppState>,
    Json(body): Json<SharingMutateBody>,
) -> Result<Json<Value>, ApiError> {
    if body.actor.trim().is_empty() {
        return Err(bad_request("actor is required on every write"));
    }
    let actor = Actor::new(body.actor);
    let out = match body.action.as_str() {
        "share" => state
            .store
            .share_item(&actor, &body.kind, &body.item_id, &body.friend,
                body.collaborate, body.expires_at.as_deref())
            .map_err(|e| bad_request(format!("{e:#}")))?,
        "unshare" => {
            let removed = state
                .store
                .unshare_item(&actor, &body.kind, &body.item_id, &body.friend)?;
            json!({"unshared": removed})
        }
        _ => return Err(bad_request("action must be share|unshare")),
    };
    crate::bus::touch(&state, "operational.changed");
    Ok(Json(out))
}

/// Aggregate everything friends share TO this record (the Desk rail).
/// Best-effort per friend — an offline or older node is skipped, never
/// a failed view (virtual-source doctrine: friends are online-best-effort).
async fn friends_shared(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let friends = state.store.friends()?;
    let mut out = Vec::new();
    for (name, _url, entity) in friends {
        let bundle = crate::network_out::shared(&state, &name);
        let profile = crate::network_out::profile(&state, &name).ok();
        if let Ok(bundle) = bundle {
            // their node is reachable: also learn which of its groups we're in
            crate::network_out::refresh_peer_groups(&state, &name);
            crate::network_out::scan_friend_activity(&state, &name, &bundle);
            out.push(json!({
                "friend": name, "entity": entity, "profile": profile,
                "tasks": bundle.get("tasks").cloned().unwrap_or_else(|| json!([])),
                "notes": bundle.get("notes").cloned().unwrap_or_else(|| json!([])),
                "relationships": bundle.get("relationships").cloned().unwrap_or_else(|| json!([])),
                "cursor": bundle.get("cursor").cloned(),
            }));
        }
    }
    Ok(Json(json!({"shared": out})))
}

async fn friends_task_toggle(
    State(state): State<AppState>,
    Path((name, id)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    let out = crate::network_out::toggle(&state, &name, &id, &body)
        .map_err(|e| bad_request(format!("{e:#}")))?;
    Ok(Json(out))
}

#[derive(Deserialize)]
struct ForkBody {
    actor: String,
    kind: String, // task | note
    id: String,
}

/// The one materialize gesture: copy a friend-shared item into THIS
/// record as my own (provenance recorded; it diverges freely after).
async fn friends_fork(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<ForkBody>,
) -> Result<Json<Value>, ApiError> {
    if body.actor.trim().is_empty() {
        return Err(bad_request("actor is required on every write"));
    }
    let actor = Actor::new(body.actor.clone());
    match body.kind.as_str() {
        "task" => {
            let bundle = crate::network_out::shared(&state, &name)
                .map_err(|e| bad_request(format!("{e:#}")))?;
            let task = bundle
                .get("tasks")
                .and_then(|t| t.as_array())
                .and_then(|list| list.iter().find(|t| t["id"] == json!(body.id)))
                .ok_or_else(|| bad_request("they don't share that task (anymore)"))?
                .clone();
            let payload = task.get("payload").filter(|p| !p.is_null()).cloned();
            let id = state.store.add_task(
                &actor,
                task.get("kind").and_then(|k| k.as_str()).unwrap_or("plain"),
                &format!(
                    "{} (from {name}'s hearth)",
                    task.get("description").and_then(|d| d.as_str()).unwrap_or("")
                ),
                payload.as_ref(),
                None,
                "one-time",
                "personal",
                None,
                None,
                &[],
            )?;
            crate::bus::touch(&state, "operational.changed");
            Ok(Json(json!({"forked": id})))
        }
        "note" => {
            let note = crate::network_out::note(&state, &name, &body.id)
                .map_err(|e| bad_request(format!("{e:#}")))?;
            // Filed under origin `network`: a friend's words settle as
            // imported, never as the owner's own (`NoteWriter`).
            let now = hearthloom_core::time::now_utc();
            let id = state.store.create_note_from(
                &actor,
                &state.home,
                &format!(
                    "{} (from {name})",
                    note.get("title").and_then(|t| t.as_str()).unwrap_or("Shared note")
                ),
                note.get("body").and_then(|b| b.as_str()).unwrap_or(""),
                &[],
                "personal",
                None,
                "network",
                Some((&now, &now)),
            )?;
            crate::bus::touch(&state, "operational.changed");
            Ok(Json(json!({"forked": id})))
        }
        _ => Err(bad_request("kind must be task|note")),
    }
}

async fn friends_refresh_card(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let out = crate::network_out::ensure_friend_card(&state, &name)
        .map_err(|e| bad_request(format!("{e:#}")))?;
    Ok(Json(out))
}

async fn friends_offer(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let out = crate::network_out::offer(&state, &name)
        .map_err(|e| bad_request(format!("{e:#}")))?;
    Ok(Json(out))
}

#[derive(Deserialize)]
struct NetworkQueryParams {
    #[serde(default)]
    q: String,
    #[serde(default)]
    id: String,
    #[serde(default = "default_sync_limit")]
    limit: usize,
}

/// A friend's brain searching THIS one. Requires a `friend`-scoped PAT
/// ALWAYS; the grant is looked up by the name THIS node gave that key —
/// callers cannot choose whose grant they use.
async fn network_search(
    State(state): State<AppState>,
    Query(params): Query<NetworkQueryParams>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
) -> Result<Json<Value>, ApiError> {
    require_scope(&auth, "friend")?;
    let friend = auth.0.as_ref().map(|c| c.name.clone()).unwrap_or_default();
    if params.q.trim().is_empty() {
        return Err(bad_request("search needs ?q="));
    }
    // THE HANDLER CONTRACT: this is the friend surface — OTHER people's
    // nodes call it, and a synchronous FTS query on a runtime worker is
    // uncancellable after the caller times out. Stacked retries of a
    // grinding query ate every worker on 2026-07-28 (the hosted tenant
    // friend-connect wedge, together with the missing v45 index).
    let hits = {
        let store = state.store.clone();
        let (f, q, limit) = (friend.clone(), params.q.clone(), params.limit.min(25));
        crate::scoped::spawn_blocking(move || store.network_search(&f, &q, limit))
            .await
            .map_err(anyhow::Error::from)?
            .map_err(|e| ApiError(StatusCode::FORBIDDEN, format!("{e:#}")))?
    };
    Ok(Json(json!({
        "hits": hits,
        "friend": friend,
        // auto-upgrade signal: callers switch to this address when set
        "preferred_url": state.store.network_address()?,
    })))
}

async fn network_fetch(
    State(state): State<AppState>,
    Query(params): Query<NetworkQueryParams>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
) -> Result<Json<Value>, ApiError> {
    require_scope(&auth, "friend")?;
    let friend = auth.0.as_ref().map(|c| c.name.clone()).unwrap_or_default();
    // Friend surface → spawn_blocking (see network_search above).
    let body = {
        let store = state.store.clone();
        let home = state.home.clone();
        let (f, id) = (friend.clone(), params.id.clone());
        crate::scoped::spawn_blocking(move || store.network_fetch(&home, &f, &id))
            .await
            .map_err(anyhow::Error::from)?
            .map_err(|e| ApiError(StatusCode::FORBIDDEN, format!("{e:#}")))?
    };
    Ok(Json(json!({"id": params.id, "body": body})))
}

#[derive(Deserialize)]
struct FriendConnectBody {
    actor: String,
    name: String,
    url: String,
    /// The friend-scoped key THEIR node minted for us.
    key: String,
    /// Optional: the Person entity this brain belongs to (id or name) —
    /// dossiers and Relationships render a "connected brain" chip.
    #[serde(default)]
    entity: Option<String>,
}

async fn friend_connect(
    State(state): State<AppState>,
    Json(body): Json<FriendConnectBody>,
) -> Result<Json<Value>, ApiError> {
    if body.actor.trim().is_empty() {
        return Err(bad_request("actor is required on every write"));
    }
    if !body.url.starts_with("http") {
        return Err(bad_request("friend url must be http(s)://…"));
    }
    // Probe: their brain must answer a granted search for us.
    let probe = reqwest::Client::new()
        .get(format!(
            "{}/v1/network/search?q=hello&limit=1",
            body.url.trim_end_matches('/')
        ))
        .bearer_auth(body.key.trim())
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await;
    match probe {
        Ok(res) if res.status().is_success() => {}
        Ok(res) => {
            return Err(bad_request(format!(
                "their brain answered {} — the key must be friend-scoped AND a grant \
                 must exist for it over there",
                res.status()
            )))
        }
        Err(err) => return Err(bad_request(format!("their brain is unreachable: {err}"))),
    }
    let entity = match &body.entity {
        Some(e) if !e.trim().is_empty() => Some(resolve_entity_row(&state, e.trim())?.id),
        _ => None,
    };
    state.store.set_friend(
        &Actor::new(body.actor),
        &body.name,
        &body.url,
        entity.as_deref(),
    )?;
    crate::network_keys::store_friend_key(&state.home, &body.name, body.key.trim())
        .map_err(|e| bad_request(format!("{e:#}")))?;
    // v46: the connection seeds a relationship record NOW — a Person +
    // 'friend' relationship built from their profile card, existing
    // before any import identifies them (later imports enrich the same
    // entity). Best-effort: an older peer without profile-v1 still gets
    // the minimal card from the connection name.
    let card = crate::network_out::ensure_friend_card(&state, &body.name).ok();
    crate::provisioner::notify_system(
        &state, "info",
        &format!("Connected to {}'s brain", body.name),
        "Ask can now search what they've granted you — manage grants on the Network page",
        None,
    );
    Ok(Json(json!({
        "connected": body.name,
        "entity": card.as_ref().and_then(|c| c.get("entity").cloned()).unwrap_or(json!(entity)),
    })))
}

async fn friends_list(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    // Retroactive card seeding: a connection made before v46 has no
    // bound entity — seed it lazily (idempotent, fire-and-forget) so
    // existing connections gain their relationship record on the first
    // Network-page visit after the upgrade.
    for (name, _, entity) in state.store.friends()? {
        if entity.is_none() {
            let st = state.clone();
            // network_out uses block_in_place internally — run on a
            // runtime worker (plain spawn), never inside spawn_blocking.
            tokio::spawn(async move {
                let _ = crate::network_out::ensure_friend_card(&st, &name);
            });
        }
    }
    let friends: Vec<Value> = state
        .store
        .friends()?
        .into_iter()
        .map(|(name, url, entity)| {
            let key_present = crate::network_keys::friend_key(&name).is_some();
            json!({"name": name, "url": url, "entity": entity, "key_present": key_present})
        })
        .collect();
    Ok(Json(json!({"friends": friends})))
}

async fn friend_remove(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<KeyRevokeBody>,
) -> Result<Json<Value>, ApiError> {
    if body.actor.trim().is_empty() {
        return Err(bad_request("actor is required on every write"));
    }
    // Retire our credential over there, best-effort (the self-revoke door).
    let mut remote_revoked = false;
    if let Some(key) = crate::network_keys::friend_key(&name) {
        if let Ok(friends) = state.store.friends() {
            if let Some((_, url, _)) = friends.iter().find(|(n, _, _)| n == &name) {
                remote_revoked = reqwest::Client::new()
                    .post(format!("{url}/v1/keys/self-revoke"))
                    .bearer_auth(&key)
                    .timeout(std::time::Duration::from_secs(10))
                    .send()
                    .await
                    .map(|r| r.status().is_success())
                    .unwrap_or(false);
            }
        }
    }
    state.store.remove_friend(&Actor::new(body.actor), &name)?;
    // what their node told us about its groups goes with the connection
    state.store.clear_peer_groups(&name).ok();
    crate::network_keys::remove_friend_key(&state.home, &name).ok();
    Ok(Json(json!({"removed": name, "remote_key_revoked": remote_revoked})))
}

#[derive(Deserialize)]
struct GrantSetBody {
    actor: String,
    friend: String,
    spheres: Vec<String>,
    #[serde(default = "default_grant_detail")]
    detail: String,
    #[serde(default)]
    expires_at: Option<String>,
    /// Set on invite: mint the friend-scoped key alongside the grant.
    #[serde(default)]
    mint_key: bool,
    /// v46: tri-state sharing overrides (classes/spheres/detail/flags);
    /// absent keys inherit the default template.
    #[serde(default)]
    sharing: Option<Value>,
}
fn default_grant_detail() -> String {
    "excerpts".into()
}

/// The invite gesture: one call creates/updates the grant and (when
/// mint_key) mints the friend-scoped key to hand them — secret shown once.
async fn grant_set(
    State(state): State<AppState>,
    Json(body): Json<GrantSetBody>,
) -> Result<Json<Value>, ApiError> {
    if body.actor.trim().is_empty() {
        return Err(bad_request("actor is required on every write"));
    }
    let actor = Actor::new(body.actor);
    let grant = state.store.set_grant(
        &actor,
        &body.friend,
        &body.spheres,
        &body.detail,
        body.expires_at.as_deref(),
    )?;
    if let Some(sharing) = &body.sharing {
        state
            .store
            .set_friend_sharing(&actor, &body.friend, sharing)
            .map_err(|e| bad_request(format!("{e:#}")))?;
    }
    let secret = if body.mint_key {
        let (_, secret) =
            state
                .store
                .create_api_key(&actor, &body.friend, &["friend".into()], None)?;
        Some(secret)
    } else {
        None
    };
    Ok(Json(json!({"grant": grant, "secret": secret})))
}

async fn grant_delete(
    State(state): State<AppState>,
    Path(friend): Path<String>,
    Json(body): Json<ActorBody>,
) -> Result<Json<Value>, ApiError> {
    if body.actor.trim().is_empty() {
        return Err(bad_request("actor is required on every write"));
    }
    let removed = state
        .store
        .delete_grant(&Actor::new(body.actor), &friend)
        .map_err(|e| bad_request(format!("{e:#}")))?;
    Ok(Json(json!({"deleted": removed})))
}

async fn grants_list(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    Ok(Json(json!({
        "grants": state.store.list_grants()?,
        "spheres": hearthloom_core::store::GRANTABLE_SPHERES,
    })))
}

async fn grant_revoke(
    State(state): State<AppState>,
    Path(friend): Path<String>,
    Json(body): Json<KeyRevokeBody>,
) -> Result<Json<Value>, ApiError> {
    if body.actor.trim().is_empty() {
        return Err(bad_request("actor is required on every write"));
    }
    state.store.revoke_grant(&Actor::new(body.actor), &friend)?;
    // Also revoke their key so the door is fully shut.
    if let Ok(keys) = state.store.list_api_keys() {
        for key in keys {
            if key.name == friend && key.revoked_at.is_none()
                && key.scopes.iter().any(|s| s == "friend")
            {
                let _ = state
                    .store
                    .revoke_api_key(&Actor::new("human:dashboard"), &key.id);
            }
        }
    }
    Ok(Json(json!({"revoked": friend})))
}

/// Recent inbound accesses — the transparency ledger.
async fn network_accesses(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let recent: Vec<Value> = state
        .store
        .recent_network_accesses(50)?
        .into_iter()
        .map(|(ts, payload)| json!({"ts": ts, "payload": payload}))
        .collect();
    Ok(Json(json!({"accesses": recent})))
}

/* ————— the discovery manifest + /brain tree (Tier 2a) —————
   The brain rendered as a mountable filesystem for HearthFS's manifest-
   driven ApiSource. Requires a PAT with the `read` scope ALWAYS (new
   integration surfaces launch authenticated). Every leaf resolves
   through `brain_fs.rs`, which hardcodes the vault-LOCKED view — mount
   clients cache responses, so an unlock window must never cross this
   boundary (BRAIN.md itself is private-excluded by construction). */

async fn discovery_manifest(
    axum::Extension(auth): axum::Extension<AuthInfo>,
) -> Result<Json<Value>, ApiError> {
    require_scope(&auth, "read")?;
    Ok(Json(json!({
        "name": "hearthloom",
        "version": env!("CARGO_PKG_VERSION"),
        "search": { "supported": true, "endpoint": "/brain/search", "method": "GET" },
        "markdown": { "supported": true },
        "tree": [
            { "path": "/BRAIN.md", "type": "file",
              "source": { "api": "/brain/BRAIN.md" },
              "cache": { "ttl_seconds": 300 } },
            { "path": "/.hearthloom-origin", "type": "file",
              "source": { "api": "/brain/.hearthloom-origin" },
              "cache": { "ttl_seconds": 3600 } },
            { "path": "/entities", "type": "collection",
              "source": { "list": "/brain/entities",
                          "item": "/brain/entities/{id}.md",
                          "slug_from": "name", "id_field": "id" },
              "cache": { "ttl_seconds": 300 } },
            { "path": "/days", "type": "collection",
              "source": { "list": "/brain/days",
                          "item": "/brain/days/{id}.md",
                          "slug_from": "name", "id_field": "id" },
              "cache": { "ttl_seconds": 300 } },
            { "path": "/goals", "type": "collection",
              "source": { "list": "/brain/intents",
                          "item": "/brain/intents/{id}.md",
                          "slug_from": "name", "id_field": "id" },
              "cache": { "ttl_seconds": 300 } },
        ],
    })))
}

fn markdown_response(body: String) -> Response {
    (
        [(axum::http::header::CONTENT_TYPE, "text/markdown; charset=utf-8")],
        body,
    )
        .into_response()
}

/// Token-fit passthrough: `?budget=<tokens>` trims a markdown view to
/// roughly that many tokens (~4 chars each) instead of making the client
/// truncate blind. HearthFS can wire `file@fit/<n>` through to this.
fn fit_budget(body: String, params: &std::collections::HashMap<String, String>) -> String {
    match params.get("budget").and_then(|b| b.parse::<usize>().ok()) {
        Some(tokens) if body.len() > tokens * 4 => {
            let mut cut: String = body.chars().take(tokens * 4).collect();
            cut.push_str("\n\n…(budget-trimmed)\n");
            cut
        }
        _ => body,
    }
}

async fn brain_tree(
    State(state): State<AppState>,
    Path(path): Path<String>,
    Query(params): Query<std::collections::HashMap<String, String>>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
) -> Result<Response, ApiError> {
    require_scope(&auth, "read")?;
    let path = path.trim_matches('/');
    match path {
        "BRAIN.md" => {
            // Private-excluded by construction (its generator never sees
            // private rows, vault open or not).
            let body = std::fs::read_to_string(state.home.brain_index_path())
                .unwrap_or_else(|_| "# BRAIN.md\n\nNot generated yet.\n".into());
            Ok(markdown_response(fit_budget(body, &params)))
        }
        ".hearthloom-origin" => {
            // The recursion marker: registering a mount that serves this
            // file as a virtual source would be the brain indexing a
            // window into itself — register_virtual refuses it.
            Ok(markdown_response(format!(
                "hearthloom-origin\nversion: {}\n",
                env!("CARGO_PKG_VERSION")
            )))
        }
        "entities" => Ok(Json(json!(state.store.brain_fs_entities(200)?)).into_response()),
        "days" => {
            // Bounded listing: the last 90 calendar days. Older material
            // is reachable through search and BRAIN.md.
            let today = chrono::Utc::now().date_naive();
            let days: Vec<Value> = (0..90)
                .map(|n| {
                    let d = (today - chrono::Duration::days(n)).to_string();
                    json!({"id": d, "name": d})
                })
                .collect();
            Ok(Json(json!(days)).into_response())
        }
        "intents" => Ok(Json(json!(state.store.brain_fs_intents()?)).into_response()),
        "search" => {
            let q = params.get("q").cloned().unwrap_or_default();
            if q.trim().is_empty() {
                return Err(bad_request("search needs ?q="));
            }
            let limit = params
                .get("limit")
                .and_then(|l| l.parse::<usize>().ok())
                .unwrap_or(20)
                .min(50);
            Ok(Json(json!({"results": state.store.brain_fs_search(&q, limit)?})).into_response())
        }
        other => {
            let (dir, leaf) = other.split_once('/').ok_or_else(|| {
                ApiError(StatusCode::NOT_FOUND, format!("no such path {other:?}"))
            })?;
            let id = leaf.trim_end_matches(".md");
            let body = match dir {
                "entities" => state.store.brain_fs_entity(id)?,
                "days" => Some(state.store.brain_fs_day(id)?),
                "intents" => state.store.brain_fs_intent(id)?,
                _ => None,
            };
            match body {
                Some(body) => Ok(markdown_response(fit_budget(body, &params))),
                None => Err(ApiError(
                    StatusCode::NOT_FOUND,
                    format!("nothing at {other:?} on this surface"),
                )),
            }
        }
    }
}

#[derive(Deserialize)]
struct IngestLocalBody {
    actor: String,
    /// imessage | photos | chrome | claude-code | mbox
    adapter: String,
    /// Source path; adapter default when omitted.
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    scope: Option<String>,
    /// mbox only: keep messages carrying any of these X-Gmail-Labels
    /// (empty = everything except the always-dropped Spam/Trash).
    #[serde(default)]
    include_labels: Vec<String>,
    /// mbox only: drop messages carrying any of these labels.
    #[serde(default)]
    exclude_labels: Vec<String>,
}

/// Run a local-source adapter inside the daemon process. This exists for
/// TCC-gated sources (iMessage, Photos): grant Full Disk Access to the
/// hearthloom binary once and the daemon can read them even when the
/// calling shell can't.
async fn ingest_local(
    State(state): State<AppState>,
    Json(body): Json<IngestLocalBody>,
) -> Result<Json<Value>, ApiError> {
    if body.actor.trim().is_empty() {
        return Err(bad_request("actor is required on every write"));
    }
    // Mbox archives can be tens of GB: run those detached and report
    // progress through daemon.log + a mutations event, instead of
    // holding the HTTP request open for the whole scan.
    if body.adapter == "mbox" {
        return ingest_mbox_background(state, body);
    }
    // Same for Apple Health exports (a multi-GB XML): one detached
    // streaming pass builds BOTH the numeric sidecar and the episode
    // layer (day aggregates + workouts + clinical + ECG).
    if body.adapter == "apple-health" {
        return ingest_health_background(state, body);
    }
    // And for Facebook archives (thousands of thread files): detached,
    // batched, friends-list â the social-connections registry.
    if body.adapter == "facebook" {
        return ingest_facebook_background(state, body);
    }
    // LinkedIn exports: small, but same shape — detached, connections
    // (with company/position/email detail) â the registry.
    if body.adapter == "linkedin" {
        return ingest_linkedin_background(state, body);
    }
    // Notion workspace exports â NOTES (origin='notion'), idempotent
    // by page-id. Detached like every archive import.
    if body.adapter == "notion" {
        return ingest_notion_background(state, body);
    }
    // Markdown vaults (Obsidian/Logseq/Reflect Open) â NOTES
    // (origin='markdown'), idempotent by vault-relative path; the vault
    // is read IN PLACE, never moved or written.
    if body.adapter == "markdown" {
        return ingest_mdvault_background(state, body);
    }
    // ChatGPT multi-chunk exports (thousands of conversations + image
    // assets): detached, batched with lock-breathing, media preserved.
    if body.adapter == "openai" {
        return ingest_openai_background(state, body);
    }
    let adapter = body.adapter.clone();
    let st = state.clone();
    let result = crate::scoped::spawn_blocking(move || -> anyhow::Result<Vec<hearthloom_core::EpisodeDraft>> {
        let scope = body.scope.as_deref();
        match body.adapter.as_str() {
            "imessage" => {
                let path = match &body.path {
                    Some(p) => std::path::PathBuf::from(p),
                    None => std::path::PathBuf::from(std::env::var("HOME")?)
                        .join("Library/Messages/chat.db"),
                };
                hearthloom_adapters::imessage::ingest(&path, scope)
            }
            "photos" => {
                let path = match &body.path {
                    Some(p) => std::path::PathBuf::from(p),
                    None => hearthloom_adapters::photos::default_library()?,
                };
                // v47: the manual scan feeds the photo-point union too
                if let Ok(points) = hearthloom_adapters::photos::scan_points(&path) {
                    let _ = crate::live_photos::store_scan_points(&st, "mac", &points);
                }
                hearthloom_adapters::photos::ingest(&path, scope)
            }
            "chrome" => {
                let path = match &body.path {
                    Some(p) => std::path::PathBuf::from(p),
                    None => hearthloom_adapters::chrome::default_data_dir()?,
                };
                hearthloom_adapters::chrome::ingest(&path, scope)
            }
            "rewind" => {
                let path = match &body.path {
                    Some(p) => std::path::PathBuf::from(p),
                    None => hearthloom_adapters::rewind::default_dir()?,
                };
                hearthloom_adapters::rewind::ingest(&path, scope)
            }
            "claude-code" => {
                let path = match &body.path {
                    Some(p) => std::path::PathBuf::from(p),
                    None => std::path::PathBuf::from(std::env::var("HOME")?)
                        .join(".claude/projects"),
                };
                hearthloom_adapters::claude_code::ingest(&path, scope)
            }
            // The wizard's instant-kindling path: one repo by path.
            "git" => {
                let path = body
                    .path
                    .as_deref()
                    .ok_or_else(|| anyhow::anyhow!("git needs a repo path"))?;
                hearthloom_adapters::git::ingest(std::path::Path::new(path), scope)
            }
            other => Err(anyhow::anyhow!("unknown local adapter: {other}")),
        }
    })
    .await
    .map_err(anyhow::Error::from)?;

    let drafts = result?;
    let count = drafts.len();
    let mut new = 0usize;
    let mut existing = 0usize;
    for chunk in drafts.chunks(500) {
        let report = state.store.apply_batch(
            &state.home,
            &hearthloom_core::IngestBatch {
                actor: format!("adapter:{adapter}"),
                session_id: None,
                episodes: chunk.to_vec(),
            },
        )?;
        new += report.episodes_new;
        existing += report.episodes_existing;
    }
    Ok(Json(json!({
        "adapter": adapter,
        "parsed": count,
        "episodes_new": new,
        "episodes_existing": existing,
    })))
}

/// Detached mbox ingest: returns immediately; the scan streams batches
/// into the store, logs progress, and lands an `mbox_ingest_done`
/// mutations event on completion (idempotency makes crashed or repeated
/// runs safe to re-issue).
fn ingest_mbox_background(
    state: AppState,
    body: IngestLocalBody,
) -> Result<Json<Value>, ApiError> {
    let path = std::path::PathBuf::from(
        body.path
            .clone()
            .ok_or_else(|| bad_request("mbox ingest needs a path"))?,
    );
    if !path.is_file() {
        return Err(bad_request(format!("no such file: {}", path.display())));
    }
    let filter = hearthloom_adapters::mbox::LabelFilter {
        include_any: body.include_labels.clone(),
        exclude_any: body.exclude_labels.clone(),
    };
    let display = path.display().to_string();
    crate::scoped::spawn_blocking(move || {
        // single-flight + the card's "running" status
        let _import_guard = state.try_start_automation("import-mbox");
        let started = std::time::Instant::now();
        let mut new = 0usize;
        let mut existing = 0usize;
        let mut batches = 0usize;
        let result = hearthloom_adapters::mbox::ingest_batched_filtered(
            &path,
            body.scope.as_deref(),
            &filter,
            500,
            |batch| {
                let report = state.store.apply_batch(
                    &state.home,
                    &IngestBatch {
                        actor: "adapter:mbox".into(),
                        session_id: None,
                        episodes: batch,
                    },
                )?;
                new += report.episodes_new;
                existing += report.episodes_existing;
                batches += 1;
                // Keep the dashboard responsive during a big archive
                // scan (shared store mutex — see consolidate.rs note).
                std::thread::sleep(std::time::Duration::from_millis(40));
                if batches.is_multiple_of(10) {
                    tracing::info!(
                        "mbox ingest {}: {new} new episodes ({existing} existing) so far",
                        path.display()
                    );
                }
                Ok(())
            },
        );
        match result {
            Ok(total) => {
                tracing::info!(
                    "mbox ingest {}: DONE — {total} messages kept, {new} new episodes \
                     ({existing} already present) in {:.0?}",
                    path.display(),
                    started.elapsed()
                );
                crate::provisioner::notify_system(
                    &state, "info", "✓ mbox import finished",
                    "the archive is in the record — details in daemon.log", Some("mbox-import"));
                    let _ = state.store.record_event(
                    &Actor::new(body.actor),
                    "mbox_ingest_done",
                    &json!({
                        "path": path.display().to_string(),
                        "kept": total,
                        "episodes_new": new,
                        "episodes_existing": existing,
                        "include_labels": body.include_labels,
                        "exclude_labels": body.exclude_labels,
                        "duration_secs": started.elapsed().as_secs(),
                    }),
                );
            }
            Err(err) => {
                tracing::error!("mbox ingest {}: FAILED: {err:#}", path.display());
            }
        }
    });
    Ok(Json(json!({
        "adapter": "mbox",
        "path": display,
        "started": true,
        "note": "running in the background; progress in daemon.log, completion \
                 recorded as an mbox_ingest_done mutations event",
    })))
}

/// Detached Facebook-archive ingest: parses the extracted dump into
/// fb_* episodes (batched applies, â¥40ms lock sleeps) and loads the
/// friends list into the social-connections registry — friends do NOT
/// become entities (see connections.rs). Idempotent per (source,
/// source_ref); re-running is safe.
/// Detached ChatGPT multi-chunk export ingest. One episode per conversation
/// (source `openai`, idempotent by conversation_id); image assets copied
/// under `raw/openai/media/<conversation_id>/`. Batched with a 40ms
/// lock-breather between chunks so the dashboard stays responsive under the
/// shared store mutex.
fn ingest_openai_background(
    state: AppState,
    body: IngestLocalBody,
) -> Result<Json<Value>, ApiError> {
    let root = std::path::PathBuf::from(body.path.clone().unwrap_or_else(|| {
        format!(
            "{}/.hearthloom/archives/openai",
            std::env::var("HOME").unwrap_or_default()
        )
    }));
    if !root.is_dir() {
        return Err(bad_request(format!(
            "{} is not an extracted ChatGPT export directory",
            root.display()
        )));
    }
    let display = root.display().to_string();
    crate::scoped::spawn_blocking(move || {
        // single-flight + the card's "running" status
        let _import_guard = state.try_start_automation("import-openai");
        let started = std::time::Instant::now();
        let scope = body.scope.as_deref();
        let media_out = state.home.raw_dir().join("openai/media");
        let result = (|| -> anyhow::Result<(usize, usize, usize)> {
            let drafts =
                hearthloom_adapters::openai::ingest(&root, Some(&media_out), scope)?;
            let parsed = drafts.len();
            tracing::info!(
                "openai ingest {}: {parsed} conversations parsed, applying…",
                root.display()
            );
            let mut new = 0usize;
            let mut existing = 0usize;
            for chunk in drafts.chunks(500) {
                let report = state.store.apply_batch(
                    &state.home,
                    &IngestBatch {
                        actor: "adapter:openai".into(),
                        session_id: None,
                        episodes: chunk.to_vec(),
                    },
                )?;
                new += report.episodes_new;
                existing += report.episodes_existing;
                // shared store mutex — keep the dashboard alive
                std::thread::sleep(std::time::Duration::from_millis(40));
            }
            Ok((parsed, new, existing))
        })();
        match result {
            Ok((parsed, new, existing)) => {
                tracing::info!(
                    "openai ingest {}: DONE — {parsed} conversations, {new} new episodes \
                     ({existing} already present), in {:.0?}",
                    root.display(),
                    started.elapsed()
                );
                crate::provisioner::notify_system(
                    &state, "info", "✓ openai import finished",
                    "the archive is in the record — details in daemon.log", Some("openai-import"));
                    let _ = state.store.record_event(
                    &Actor::new(body.actor),
                    "openai_ingest_done",
                    &json!({
                        "path": root.display().to_string(),
                        "parsed": parsed,
                        "episodes_new": new,
                        "episodes_existing": existing,
                        "duration_secs": started.elapsed().as_secs(),
                    }),
                );
            }
            Err(err) => tracing::error!("openai ingest {}: FAILED: {err:#}", root.display()),
        }
    });
    Ok(Json(json!({
        "adapter": "openai",
        "path": display,
        "started": true,
        "note": "running in the background; progress in daemon.log, completion \
                 recorded as an openai_ingest_done mutations event",
    })))
}

fn ingest_facebook_background(
    state: AppState,
    body: IngestLocalBody,
) -> Result<Json<Value>, ApiError> {
    let root = std::path::PathBuf::from(body.path.clone().unwrap_or_else(|| {
        format!(
            "{}/.hearthloom/archives/facebook",
            std::env::var("HOME").unwrap_or_default()
        )
    }));
    if !root.join("your_facebook_activity").is_dir() {
        return Err(bad_request(format!(
            "{} is not an extracted Facebook archive",
            root.display()
        )));
    }
    let display = root.display().to_string();
    crate::scoped::spawn_blocking(move || {
        // single-flight + the card's "running" status
        let _import_guard = state.try_start_automation("import-facebook");
        let started = std::time::Instant::now();
        let scope = body.scope.as_deref();
        let result = (|| -> anyhow::Result<(usize, usize, usize, usize)> {
            let drafts = hearthloom_adapters::facebook::ingest(&root, scope)?;
            let parsed = drafts.len();
            tracing::info!("facebook ingest {}: {parsed} drafts parsed, applying…", root.display());
            let mut new = 0usize;
            let mut existing = 0usize;
            for chunk in drafts.chunks(500) {
                let report = state.store.apply_batch(
                    &state.home,
                    &IngestBatch {
                        actor: "adapter:facebook".into(),
                        session_id: None,
                        episodes: chunk.to_vec(),
                    },
                )?;
                new += report.episodes_new;
                existing += report.episodes_existing;
                // shared store mutex — keep the dashboard alive
                std::thread::sleep(std::time::Duration::from_millis(40));
            }
            let connections = hearthloom_adapters::facebook::friends(&root)?;
            let rows: Vec<hearthloom_core::store::ConnectionUpsert> = connections
                .into_iter()
                .map(|c| hearthloom_core::store::ConnectionUpsert {
                    name: c.name,
                    connected_at: c.connected_at,
                    ended_at: c.ended_at,
                    status: c.status,
                    ..Default::default()
                })
                .collect();
            let upserted = state.store.upsert_social_connections(
                &Actor::new("adapter:facebook".to_string()),
                "facebook",
                &rows,
            )?;
            Ok((parsed, new, existing, upserted))
        })();
        match result {
            Ok((parsed, new, existing, friends)) => {
                tracing::info!(
                    "facebook ingest {}: DONE — {parsed} drafts, {new} new episodes \
                     ({existing} already present), {friends} connections in the registry, \
                     in {:.0?}",
                    root.display(),
                    started.elapsed()
                );
                crate::provisioner::notify_system(
                    &state, "info", "✓ facebook import finished",
                    "the archive is in the record — details in daemon.log", Some("facebook-import"));
                    let _ = state.store.record_event(
                    &Actor::new(body.actor),
                    "facebook_ingest_done",
                    &json!({
                        "path": root.display().to_string(),
                        "parsed": parsed,
                        "episodes_new": new,
                        "episodes_existing": existing,
                        "connections": friends,
                        "duration_secs": started.elapsed().as_secs(),
                    }),
                );
            }
            Err(err) => tracing::error!("facebook ingest {}: FAILED: {err:#}", root.display()),
        }
    });
    Ok(Json(json!({
        "adapter": "facebook",
        "path": display,
        "started": true,
        "note": "running in the background; progress in daemon.log, completion \
                 recorded as a facebook_ingest_done mutations event",
    })))
}

/// Detached markdown-vault import (Obsidian/Logseq/Reflect Open):
/// .md files â notes (origin='markdown'), upserted by vault-relative
/// path. Reads the vault in place — a live Obsidian vault stays where
/// (and whose) it is; re-imports refresh changed files.
fn ingest_mdvault_background(
    state: AppState,
    body: IngestLocalBody,
) -> Result<Json<Value>, ApiError> {
    let root = std::path::PathBuf::from(body.path.clone().unwrap_or_else(|| {
        format!(
            "{}/.hearthloom/archives/markdown",
            std::env::var("HOME").unwrap_or_default()
        )
    }));
    if !root.is_dir() {
        return Err(bad_request(format!(
            "no such directory: {} — pass the vault's full path",
            root.display()
        )));
    }
    let display = root.display().to_string();
    crate::scoped::spawn_blocking(move || {
        // single-flight + the card's "running" status
        let _import_guard = state.try_start_automation("import-markdown");
        let started = std::time::Instant::now();
        let scope = body.scope.clone().unwrap_or_else(|| "personal".into());
        match crate::mdvault::import(&state.store, &state.home, &root, &scope) {
            Ok(report) => {
                tracing::info!(
                    "markdown vault import {}: DONE — {} notes ({} created, {} updated, \
                     {} unchanged), {} images preserved ({} KB), {} media refs stripped, \
                     {} empty + {} drawings skipped in {:.0?}",
                    root.display(),
                    report.files,
                    report.created,
                    report.updated,
                    report.unchanged,
                    report.images_copied,
                    report.image_bytes / 1024,
                    report.media_stripped,
                    report.empty_skipped,
                    report.excalidraw_skipped,
                    started.elapsed()
                );
                let _ = state.store.record_event(
                    &Actor::new(body.actor),
                    "markdown_ingest_done",
                    &serde_json::to_value(&report).unwrap_or_default(),
                );
                crate::provisioner::notify_system(
                    &state, "info",
                    "✓ markdown import finished",
                    "the archive is in the record — details in daemon.log",
                    Some("markdown-import"),
                );
            }
            Err(err) => tracing::error!("markdown vault import {}: FAILED: {err:#}", root.display()),
        }
    });
    Ok(Json(json!({
        "adapter": "markdown",
        "path": display,
        "started": true,
        "note": "running in the background; breakdown in daemon.log, completion \
                 recorded as a markdown_ingest_done mutations event",
    })))
}

/// Detached Notion import: extracted Markdown & CSV export â notes
/// (origin='notion'), upserted by page-id — re-imports refresh changed
/// pages and duplicate nothing. Small databases become table notes;
/// big ones are named in the log, never silently truncated.
fn ingest_notion_background(
    state: AppState,
    body: IngestLocalBody,
) -> Result<Json<Value>, ApiError> {
    let root = std::path::PathBuf::from(body.path.clone().unwrap_or_else(|| {
        format!(
            "{}/.hearthloom/archives/notion",
            std::env::var("HOME").unwrap_or_default()
        )
    }));
    if !root.is_dir() {
        return Err(bad_request(format!("no such directory: {}", root.display())));
    }
    let display = root.display().to_string();
    crate::scoped::spawn_blocking(move || {
        // single-flight + the card's "running" status
        let _import_guard = state.try_start_automation("import-notion");
        let started = std::time::Instant::now();
        let scope = body.scope.clone().unwrap_or_else(|| "personal".into());
        match crate::notion::import(&state.store, &state.home, &root, &scope) {
            Ok(report) => {
                tracing::info!(
                    "notion import {}: DONE — {} pages + {} tables ({} created, {} updated, \
                     {} unchanged), {} images preserved ({} KB), {} media refs stripped, \
                     {} empty skipped{} in {:.0?}",
                    root.display(),
                    report.pages,
                    report.tables,
                    report.created,
                    report.updated,
                    report.unchanged,
                    report.images_copied,
                    report.image_bytes / 1024,
                    report.media_stripped,
                    report.empty_skipped,
                    if report.big_csvs.is_empty() {
                        String::new()
                    } else {
                        format!(", databases too big for table notes: {}", report.big_csvs.join("; "))
                    },
                    started.elapsed()
                );
                let _ = state.store.record_event(
                    &Actor::new(body.actor),
                    "notion_ingest_done",
                    &serde_json::to_value(&report).unwrap_or_default(),
                );
                crate::provisioner::notify_system(
                    &state, "info",
                    "✓ notion import finished",
                    "the archive is in the record — details in daemon.log",
                    Some("notion-import"),
                );
            }
            Err(err) => tracing::error!("notion import {}: FAILED: {err:#}", root.display()),
        }
    });
    Ok(Json(json!({
        "adapter": "notion",
        "path": display,
        "started": true,
        "note": "running in the background; breakdown in daemon.log, completion \
                 recorded as a notion_ingest_done mutations event",
    })))
}

/// Detached LinkedIn ingest: career/articles/events episodes + the
/// ~30k-row connections list (company/position/email detail) into the
/// social-connections registry. Idempotent; built to absorb the later
/// FULL export through the same source_refs.
fn ingest_linkedin_background(
    state: AppState,
    body: IngestLocalBody,
) -> Result<Json<Value>, ApiError> {
    let root = std::path::PathBuf::from(body.path.clone().unwrap_or_else(|| {
        format!(
            "{}/.hearthloom/archives/linkedin",
            std::env::var("HOME").unwrap_or_default()
        )
    }));
    if !root.join("Connections.csv").is_file() && !root.join("Profile.csv").is_file() {
        return Err(bad_request(format!(
            "{} is not an extracted LinkedIn export",
            root.display()
        )));
    }
    let display = root.display().to_string();
    crate::scoped::spawn_blocking(move || {
        // single-flight + the card's "running" status
        let _import_guard = state.try_start_automation("import-linkedin");
        let started = std::time::Instant::now();
        let scope = body.scope.as_deref();
        let result = (|| -> anyhow::Result<(usize, usize, usize, usize)> {
            let drafts = hearthloom_adapters::linkedin::ingest(&root, scope)?;
            let parsed = drafts.len();
            let mut new = 0usize;
            let mut existing = 0usize;
            for chunk in drafts.chunks(500) {
                let report = state.store.apply_batch(
                    &state.home,
                    &IngestBatch {
                        actor: "adapter:linkedin".into(),
                        session_id: None,
                        episodes: chunk.to_vec(),
                    },
                )?;
                new += report.episodes_new;
                existing += report.episodes_existing;
                std::thread::sleep(std::time::Duration::from_millis(40));
            }
            let rows: Vec<hearthloom_core::store::ConnectionUpsert> =
                hearthloom_adapters::linkedin::connections(&root)?
                    .into_iter()
                    .map(|c| hearthloom_core::store::ConnectionUpsert {
                        name: c.name,
                        connected_at: c.connected_at,
                        ended_at: None,
                        status: "active".into(),
                        company: c.company,
                        position: c.position,
                        handle: c.handle,
                    })
                    .collect();
            // 30k rows through the shared mutex: chunked with sleeps
            let mut upserted = 0usize;
            for chunk in rows.chunks(1000) {
                upserted += state.store.upsert_social_connections(
                    &Actor::new("adapter:linkedin".to_string()),
                    "linkedin",
                    chunk,
                )?;
                std::thread::sleep(std::time::Duration::from_millis(40));
            }
            Ok((parsed, new, existing, upserted))
        })();
        match result {
            Ok((parsed, new, existing, connections)) => {
                tracing::info!(
                    "linkedin ingest {}: DONE — {parsed} drafts, {new} new episodes \
                     ({existing} existing), {connections} connections in the registry, in {:.0?}",
                    root.display(),
                    started.elapsed()
                );
                crate::provisioner::notify_system(
                    &state, "info", "✓ linkedin import finished",
                    "the archive is in the record — details in daemon.log", Some("linkedin-import"));
                    let _ = state.store.record_event(
                    &Actor::new(body.actor),
                    "linkedin_ingest_done",
                    &json!({
                        "path": root.display().to_string(),
                        "parsed": parsed,
                        "episodes_new": new,
                        "episodes_existing": existing,
                        "connections": connections,
                        "duration_secs": started.elapsed().as_secs(),
                    }),
                );
            }
            Err(err) => tracing::error!("linkedin ingest {}: FAILED: {err:#}", root.display()),
        }
    });
    Ok(Json(json!({
        "adapter": "linkedin",
        "path": display,
        "started": true,
        "note": "running in the background; progress in daemon.log, completion \
                 recorded as a linkedin_ingest_done mutations event",
    })))
}

/// Detached Apple Health ingest: one streaming pass over export.xml
/// writes the numeric sidecar (samples + daily rollups) and applies the
/// episode layer (health_day aggregates, workouts, clinical records,
/// ECGs). Idempotent per (source, source_ref); re-running is safe.
fn ingest_health_background(
    state: AppState,
    body: IngestLocalBody,
) -> Result<Json<Value>, ApiError> {
    // No explicit path â the staged archive (where the inbox zip route
    // and Downloads move-into-place both land the export).
    let given = body
        .path
        .clone()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| state.home.root().join("archives/apple-health"));
    // Accept the export dir, the archive root, or export.xml itself.
    let xml = if given.is_file() {
        given
    } else {
        let nested = given.join("apple_health_export/export.xml");
        let flat = given.join("export.xml");
        if nested.is_file() {
            nested
        } else if flat.is_file() {
            flat
        } else {
            return Err(bad_request(format!(
                "no export.xml under {}",
                given.display()
            )));
        }
    };
    let xml_display = xml.display().to_string();
    crate::scoped::spawn_blocking(move || {
        // single-flight + the card's "running" status
        let _import_guard = state.try_start_automation("import-health");
        let started = std::time::Instant::now();
        let result = (|| -> anyhow::Result<(usize, usize, usize, usize)> {
            use hearthloom_adapters::apple_health as ah;
            let mut writer = crate::health_index::SidecarWriter::create(&xml)?;
            let mut table = ah::DayTable::default();
            let mut workouts: Vec<ah::WorkoutRec> = Vec::new();
            let (samples, _) = ah::scan(&xml, |item| {
                match item {
                    ah::Item::Sample(s) => {
                        writer.add_sample(&s)?;
                        table.add(&s);
                    }
                    ah::Item::Workout(w) => workouts.push(w),
                }
                Ok(())
            })?;
            tracing::info!("apple-health {}: scanned {samples} samples, building sidecar rollups", xml.display());
            writer.finish(&table.map)?;

            let scope = body.scope.as_deref();
            let export_dir = xml.parent().map(std::path::Path::to_path_buf).unwrap_or_default();
            let mut episodes = table.into_episodes(scope);
            let day_count = episodes.len();
            episodes.extend(workouts.iter().map(|w| ah::workout_to_episode(w, scope)));
            episodes.extend(ah::ingest_clinical(&export_dir.join("clinical-records"), scope)?);
            episodes.extend(ah::ingest_ecg(&export_dir.join("electrocardiograms"), scope)?);
            let total = episodes.len();

            let mut new = 0usize;
            for chunk in episodes.chunks(500) {
                let report = state.store.apply_batch(
                    &state.home,
                    &IngestBatch {
                        actor: "adapter:apple-health".into(),
                        session_id: None,
                        episodes: chunk.to_vec(),
                    },
                )?;
                new += report.episodes_new;
                std::thread::sleep(std::time::Duration::from_millis(40));
            }
            Ok((samples, day_count, total, new))
        })();
        match result {
            Ok((samples, days, episodes, new)) => {
                tracing::info!(
                    "apple-health {xml_display}: DONE — {samples} samples â sidecar, \
                     {days} day aggregates, {episodes} episodes ({new} new) in {:.0?}",
                    started.elapsed()
                );
                crate::provisioner::notify_system(
                    &state, "info", "✓ health import finished",
                    "the archive is in the record — details in daemon.log", Some("health-import"));
                    let _ = state.store.record_event(
                    &Actor::new(body.actor),
                    "health_ingest_done",
                    &json!({
                        "path": xml_display,
                        "samples": samples,
                        "day_episodes": days,
                        "episodes": episodes,
                        "episodes_new": new,
                        "duration_secs": started.elapsed().as_secs(),
                    }),
                );
            }
            Err(err) => tracing::error!("apple-health {xml_display}: FAILED: {err:#}"),
        }
    });
    Ok(Json(json!({
        "adapter": "apple-health",
        "started": true,
        "note": "running in the background; progress in daemon.log, completion \
                 recorded as a health_ingest_done mutations event",
    })))
}

#[derive(Deserialize)]
struct ProposeBody {
    actor: String,
    #[serde(default)]
    session_id: Option<String>,
    entries: Vec<Value>,
}

async fn propose(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
    Json(body): Json<ProposeBody>,
) -> Result<Json<Value>, ApiError> {
    if body.actor.trim().is_empty() {
        return Err(bad_request("actor is required on every write"));
    }
    // Tend shows who proposed: a key is named by the server, never by
    // the body (a key could otherwise sign as "agent:consolidation").
    let actor = Actor {
        actor: write_actor(&auth, body.actor).actor,
        session_id: body.session_id,
    };
    let entries: Vec<Value> = body.entries.iter().map(hearthloom_core::store::outside_proposal).collect();
    let ids = state.store.propose_memories(&actor, &entries)?;
    Ok(Json(json!({"quarantined": ids})))
}

#[derive(Deserialize)]
struct ClaimBody {
    actor: String,
    #[serde(default)]
    session_id: Option<String>,
    /// The entity this identity belongs to (usually you).
    entity: String,
    /// Attribute kind: email | phone | handle | repo-url …
    kind: String,
    value: String,
    /// Optional context this identity carries (work | personal | …);
    /// material ingested through it defaults to this scope.
    #[serde(default)]
    scope: Option<String>,
}

async fn identity_claim(
    State(state): State<AppState>,
    Json(body): Json<ClaimBody>,
) -> Result<Json<Value>, ApiError> {
    if body.actor.trim().is_empty() {
        return Err(bad_request("actor is required on every write"));
    }
    let actor = Actor {
        actor: body.actor,
        session_id: body.session_id,
    };
    let outcome = state.store.claim_identity(
        &actor,
        &body.entity,
        &body.kind,
        &body.value,
        body.scope.as_deref(),
    )?;
    Ok(Json(serde_json::to_value(outcome).unwrap()))
}

#[derive(Deserialize)]
struct MergeBody {
    actor: String,
    /// Surviving entity — id or exact name.
    winner: String,
    /// Entity merged away (reversibly) — id or exact name.
    loser: String,
    #[serde(default)]
    reason: Option<String>,
}

/// Accept an entity id OR an exact name — the dashboard works in names;
/// sending a name to an id-only endpoint used to fail with zero
/// feedback (the merge-panel bug Jon hit).
fn resolve_entity_arg(state: &AppState, arg: &str) -> Result<String, ApiError> {
    resolve_entity_row(state, arg).map(|e| e.id)
}

/// Same id-or-name contract, returning the whole row — for handlers that
/// need the entity, not just its id (mentions/relationships lookups; the
/// project modal opens by id and used to 400 on all of them).
fn resolve_entity_row(
    state: &AppState,
    arg: &str,
) -> Result<hearthloom_core::store::EntityRow, ApiError> {
    if let Some(e) = state.store.entity_by_id(arg)? {
        return Ok(e);
    }
    state
        .store
        .entity_by_name(arg)?
        .ok_or_else(|| bad_request(format!("unknown entity {arg:?}")))
}

async fn identity_merge(
    State(state): State<AppState>,
    Json(body): Json<MergeBody>,
) -> Result<Json<Value>, ApiError> {
    if body.actor.trim().is_empty() {
        return Err(bad_request("actor is required on every write"));
    }
    let winner = resolve_entity_arg(&state, &body.winner)?;
    let loser = resolve_entity_arg(&state, &body.loser)?;
    let merge_id = state.store.merge_entities(
        &Actor::new(body.actor),
        &winner,
        &loser,
        1.0,
        body.reason.as_deref().unwrap_or("manual merge"),
    )?;
    Ok(Json(json!({"merge_id": merge_id})))
}

#[derive(Deserialize)]
struct ReviewsParams {
    /// unknown = the don't-know pile; absent = the pending queue.
    state: Option<String>,
}

async fn reviews(
    State(state): State<AppState>,
    Query(params): Query<ReviewsParams>,
) -> Result<Json<Value>, ApiError> {
    let rows = state
        .store
        .reviews_in_state(params.state.as_deref(), 100)?;
    Ok(Json(json!({"pending": rows})))
}

#[derive(Deserialize)]
struct ReviewBody {
    actor: String,
    /// same | distinct | unknown (park for later) | reopen
    resolution: String,
}

async fn resolve_review(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<ReviewBody>,
) -> Result<Json<Value>, ApiError> {
    if body.actor.trim().is_empty() {
        return Err(bad_request("actor is required on every write"));
    }
    let merge_id = state
        .store
        .resolve_review(&Actor::new(body.actor), &id, &body.resolution)?;
    Ok(Json(json!({"review": id, "resolution": body.resolution, "merge_id": merge_id})))
}

#[derive(Deserialize)]
struct AttributeRemoveBody {
    actor: String,
    entity_id: String,
    kind: String,
    value: String,
}

/// "This email is not them" — detach an identifying attribute.
async fn remove_attribute(
    State(state): State<AppState>,
    Json(body): Json<AttributeRemoveBody>,
) -> Result<Json<Value>, ApiError> {
    if body.actor.trim().is_empty() {
        return Err(bad_request("actor is required on every write"));
    }
    let removed = state.store.remove_entity_attribute(
        &Actor::new(body.actor),
        &body.entity_id,
        &body.kind,
        &body.value,
    )?;
    Ok(Json(json!({"removed": removed})))
}

async fn merges(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let merges = state.store.recent_merges(100)?;
    Ok(Json(json!({"merges": merges})))
}

#[derive(Deserialize)]
struct ActorBody {
    actor: String,
}

async fn revert_merge(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<ActorBody>,
) -> Result<Json<Value>, ApiError> {
    if body.actor.trim().is_empty() {
        return Err(bad_request("actor is required on every write"));
    }
    state.store.revert_merge(&Actor::new(body.actor), &id)?;
    Ok(Json(json!({"reverted": id})))
}

#[derive(Deserialize)]
struct ScopeBody {
    actor: String,
    /// An entity name…
    entity: Option<String>,
    /// …or an episode source (e.g. chatlog-claude) to relabel wholesale…
    source: Option<String>,
    /// …or specific episode ids (single or bulk-selected in Browse).
    episodes: Option<Vec<String>>,
    scope: String,
    /// For Project entities: also relabel the episodes citing it as repo.
    #[serde(default)]
    cascade: bool,
}

async fn set_scope(
    State(state): State<AppState>,
    Json(body): Json<ScopeBody>,
) -> Result<Json<Value>, ApiError> {
    if body.actor.trim().is_empty() {
        return Err(bad_request("actor is required on every write"));
    }
    const SCOPES: &[&str] = &["work", "personal", "family", "health", "private"];
    if !SCOPES.contains(&body.scope.as_str()) {
        return Err(bad_request(format!(
            "scope must be one of {SCOPES:?}, got {:?}",
            body.scope
        )));
    }
    let actor = Actor::new(body.actor);
    let (target, relabeled) = match (&body.entity, &body.source, &body.episodes) {
        (Some(entity), None, None) => (
            entity.clone(),
            state
                .store
                .set_entity_scope(&actor, entity, &body.scope, body.cascade)?,
        ),
        (None, Some(source), None) => (
            format!("source:{source}"),
            state.store.set_scope_by_source(&actor, source, &body.scope)?,
        ),
        (None, None, Some(ids)) if !ids.is_empty() => (
            format!("{} episode(s)", ids.len()),
            state.store.set_episode_scopes(&actor, ids, &body.scope)?,
        ),
        _ => {
            return Err(bad_request(
                "pass exactly one of 'entity', 'source', or 'episodes'",
            ))
        }
    };
    crate::bus::touch(&state, "privacy.changed");
    Ok(Json(json!({
        "target": target,
        "scope": body.scope,
        "episodes_relabeled": relabeled,
    })))
}

#[derive(Deserialize)]
struct QueryBody {
    #[serde(default = "default_query_actor")]
    actor: String,
    question: String,
    /// Ask ids of the conversation so far (panel order). The server
    /// re-reads them vault-guarded — a sensitive turn silently drops
    /// from context while the vault is locked.
    #[serde(default)]
    thread: Vec<String>,
}

fn default_query_actor() -> String {
    "human:cli".into()
}

/// Resolve prior ask ids into RLM context turns + the thread root to
/// record on the new ask. Unknown/locked ids are skipped, not errors.
fn thread_context(
    state: &AppState,
    ids: &[String],
) -> (Vec<crate::rlm::ThreadTurn>, Option<String>) {
    let mut turns = Vec::new();
    let mut root: Option<String> = None;
    for id in ids {
        let Ok(Some(record)) = state.store.ask_get(id) else {
            continue;
        };
        if root.is_none() {
            root = Some(record.thread.clone().unwrap_or_else(|| record.id.clone()));
        }
        let tainted = state.store.ask_turns_tainted(std::slice::from_ref(&record.id)).ok().flatten();
        turns.push(crate::rlm::ThreadTurn {
            question: record.question,
            answer: record.answer,
            error: record.error,
            tainted,
        });
    }
    (turns, root)
}

async fn deep_query(
    State(state): State<AppState>,
    Json(body): Json<QueryBody>,
) -> Result<Json<Value>, ApiError> {
    if body.question.trim().is_empty() {
        return Err(bad_request("question is required"));
    }
    let actor = Actor::new(body.actor);
    let (context, root) = thread_context(&state, &body.thread);
    let ask_id = state.store.ask_start(&actor, &body.question, root.as_deref())?;
    let started = std::time::Instant::now();
    let outcome = crate::rlm::run_query(&state, &ask_id, &body.question, &actor, &context, None).await;
    let elapsed = started.elapsed().as_millis() as i64;
    match outcome {
        Ok(outcome) => {
            crate::rlm::record_query(&state, &actor, &body.question, outcome.rounds);
            if let Some(reason) = &outcome.tainted {
                let _ = state.store.ask_set_tainted(&ask_id, reason);
            }
            let _ = state.store.ask_finish(
                &ask_id,
                Some(&outcome.answer),
                None,
                &json!(outcome.citations),
                &json!(outcome.transcript),
                outcome.rounds,
                elapsed,
            );
            Ok(Json(json!({
                "ask_id": ask_id,
                "question": body.question,
                "answer": outcome.answer,
                "citations": outcome.citations,
                "dropped_citations": outcome.dropped_citations,
                "rounds": outcome.rounds,
                "transcript": outcome.transcript,
                "task": outcome.delegate.as_ref().and_then(|d|
                    propose_from_ask(&state, &ask_id, &body.question, d, outcome.tainted.as_deref()).ok()),
            })))
        }
        Err(err) => {
            let _ = state.store.ask_finish(
                &ask_id,
                None,
                Some(&format!("{err:#}")),
                &json!([]),
                &json!([]),
                0,
                elapsed,
            );
            Err(err.into())
        }
    }
}

#[derive(Deserialize)]
struct QueryStreamParams {
    question: String,
    /// Comma-separated prior ask ids (the panel's conversation so far).
    #[serde(default)]
    thread: Option<String>,
}

/* —— Agents (v55): Ask's hand-offs, governance, bridges —— */

/// Ask asked for something to be DONE: create the task under governance.
fn propose_from_ask(
    state: &AppState,
    ask_id: &str,
    question: &str,
    delegate: &Value,
    tainted: Option<&str>,
) -> anyhow::Result<Value> {
    let needs: Vec<String> = delegate["needs"]
        .as_array()
        .map(|list| list.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();
    let needs = if needs.is_empty() { vec!["other".to_string()] } else { needs };
    let sensitive = state.store.ask_sensitive(ask_id).unwrap_or(true);
    crate::agents::propose(
        state, Some(ask_id), sensitive, question,
        delegate["task"].as_str().unwrap_or(question), &needs,
        delegate["agent"].as_str(), tainted,
    )
}

/// Every machine this person runs Hearthloom on, with roles and when
/// each was last seen (the Peering page). Owner only.
async fn nodes_view(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
) -> Result<Json<Value>, ApiError> {
    require_owner(&auth, "see your nodes")?;
    crate::agents::publish_self(&state);
    Ok(Json(json!({
        "this": crate::agents::node_id(&state),
        "nodes": state.store.nodes()?,
        "thinking_here": crate::agents::thinking_here(&state),
        "thinking_holder": crate::agents::thinking_holder(&state),
    })))
}

#[derive(Deserialize)]
struct NodesSetBody {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    thinking: Option<String>,
}

/// This node's name and thinking role.
async fn nodes_set(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
    Json(body): Json<NodesSetBody>,
) -> Result<Json<Value>, ApiError> {
    require_owner(&auth, "change this node")?;
    if let Some(name) = body.name.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
        if name.chars().count() > 40 {
            return Err(bad_request("a name of up to 40 characters"));
        }
        state.store.set_consolidation_state("node.name", name)?;
        crate::agents::publish_all(&state);
    }
    if let Some(mode) = body.thinking.as_deref() {
        if !["on", "off", "auto"].contains(&mode) {
            return Err(bad_request("thinking must be on, off or auto"));
        }
        state.store.set_consolidation_state("node.thinking", mode)?;
    }
    crate::agents::publish_self(&state);
    Ok(Json(json!({"node_id": crate::agents::node_id(&state), "thinking_here": crate::agents::thinking_here(&state)})))
}

fn agents_vault_open(state: &AppState) -> Result<(), ApiError> {
    if state.store.vault_status().0 {
        return Err(ApiError(
            StatusCode::FORBIDDEN,
            "unlock the vault to see or change how your agents are set up".into(),
        ));
    }
    Ok(())
}

/// ☰ → Agents. Locked: only what waits for the owner (non-sensitive
/// approvals) and the catalog. Unlocked: everything.
async fn agents_view(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
) -> Result<Json<Value>, ApiError> {
    require_owner(&auth, "see your agents")?;
    let locked = state.store.vault_status().0;
    let catalog: Vec<Value> = crate::agents::CATALOG
        .iter()
        .map(|h| json!({"id": h.id, "label": h.label, "depth": h.depth, "runs": h.runs,
                        "good_for": h.good_for, "blurb": h.blurb}))
        .collect();
    let approvals = state.store.pending_approvals(!locked)?;
    if locked {
        return Ok(Json(json!({"locked": true, "approvals": approvals, "catalog": catalog})));
    }
    let agents: Vec<Value> = state
        .store
        .agents()?
        .into_iter()
        .map(|a| {
            let overrides = state.store.agent_policy_overrides(&a.id).unwrap_or_else(|_| json!({}));
            json!({
                "agent": a,
                "depth": crate::agents::harness(&a.harness).map(|h| h.depth),
                "has_key": crate::agents::secret(&a.id, "KEY").is_some(),
                "has_token": crate::agents::secret(&a.id, "TOKEN").is_some(),
                "has_hook": crate::agents::secret(&a.id, "HOOK").is_some(),
                "overrides": overrides,
            })
        })
        .collect();
    let presets: Vec<Value> = hearthloom_core::store::PRESETS
        .iter()
        .map(|(name, _)| json!({"name": name, "policy": hearthloom_core::store::Policy::preset(name)}))
        .collect();
    let here = crate::agents::node_id(&state);
    let elsewhere: Vec<Value> = state
        .store
        .agent_directory()?
        .into_iter()
        .filter(|(a, node_id, _)| node_id != &here && state.store.agent(&a.id).ok().flatten().is_none())
        .map(|(a, _, node)| json!({"id": a.id, "name": a.name, "harness": a.harness, "enabled": a.enabled,
            "node": node, "depth": crate::agents::harness(&a.harness).map(|h| h.depth)}))
        .collect();
    Ok(Json(json!({
        "locked": false,
        "node": {"id": here, "name": crate::agents::node_name(&state)},
        "peer": state.store.peer()?.map(|(name, _)| json!({"name": name})),
        "elsewhere": elsewhere,
        "agents": agents,
        "policy": state.store.global_policy()?,
        "presets": presets,
        "classes": crate::agents::classes_json(),
        "catalog": catalog,
        "approvals": approvals,
        "tasks": state.store.agent_tasks(60, true)?,
        "areas": hearthloom_core::store::LIFE_AREAS,
    })))
}

#[derive(Deserialize)]
struct AgentsMutateBody {
    action: String, // save | delete | mint_key | set_token | hook | test | move_to_peer | move_from_peer | rename_node
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    agent: Option<hearthloom_core::store::AgentIn>,
    #[serde(default)]
    areas: Option<Vec<String>>,
    #[serde(default)]
    token: Option<String>,
    #[serde(default)]
    name: Option<String>,
}

/// This node's paired peer: (name, url, key) — or a clear refusal.
fn agents_peer(state: &AppState) -> Result<(String, String, String), ApiError> {
    let (name, url) = state
        .store
        .peer()?
        .ok_or_else(|| bad_request("this machine isn't paired with another node (☰ → Peering)"))?;
    let key = crate::sync::peer_key().ok_or_else(|| bad_request("the pairing key is missing — re-pair (☰ → Peering)"))?;
    Ok((name, url, key))
}

async fn agents_mutate(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
    Json(body): Json<AgentsMutateBody>,
) -> Result<Json<Value>, ApiError> {
    require_owner(&auth, "change your agents")?;
    agents_vault_open(&state)?;
    let actor = Actor::new("human:dashboard");
    let refuse = |e: anyhow::Error| bad_request(format!("{e:#}"));
    let need_agent = |id: &Option<String>| -> Result<hearthloom_core::store::AgentRow, ApiError> {
        let id = id.as_deref().ok_or_else(|| bad_request("which agent?"))?;
        state.store.agent(id)?.ok_or_else(|| bad_request("no such agent"))
    };
    let home = state.home.root().to_path_buf();
    let out = match body.action.as_str() {
        "save" => {
            let input = body.agent.as_ref().ok_or_else(|| bad_request("agent details are required"))?;
            let saved = state.store.save_agent(&actor, body.id.as_deref(), input).map_err(refuse)?;
            let _ = state.store.publish_agent(&saved, &crate::agents::node_id(&state), &crate::agents::node_name(&state), false);
            json!({"agent": saved})
        }
        "delete" => {
            let agent = need_agent(&body.id)?;
            if let Some(key) = &agent.key_id {
                let _ = state.store.revoke_api_key(&actor, key);
            }
            let vars: Vec<String> = ["KEY", "TOKEN", "HOOK"].iter().map(|w| crate::agents::secret_var(&agent.id, w)).collect();
            let blank: Vec<(&str, &str)> = vars.iter().map(|v| (v.as_str(), "")).collect();
            let _ = hearthloom_core::secrets::set_env_vars(&home, &blank);
            let _ = state.store.publish_agent(&agent, &crate::agents::node_id(&state), &crate::agents::node_name(&state), true);
            json!({"deleted": state.store.delete_agent(&actor, &agent.id)?.is_some()})
        }
        "mint_key" => {
            let agent = need_agent(&body.id)?;
            let key_id = crate::agents::mint_agent_key(&state, &agent, body.areas.clone(), &actor).map_err(refuse)?;
            json!({"key_id": key_id})
        }
        "set_token" => {
            let agent = need_agent(&body.id)?;
            let token = body.token.as_deref().map(str::trim).unwrap_or("");
            hearthloom_core::secrets::set_env_vars(&home, &[(&crate::agents::secret_var(&agent.id, "TOKEN"), token)])
                .map_err(refuse)?;
            json!({"saved": !token.is_empty()})
        }
        "hook" => {
            // a fresh hook token for Hermes's approval script — shown once
            let agent = need_agent(&body.id)?;
            let token = format!("hlh_{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple());
            hearthloom_core::secrets::set_env_vars(&home, &[(&crate::agents::secret_var(&agent.id, "HOOK"), &token)])
                .map_err(refuse)?;
            let me = std::env::var("HEARTHLOOM_PUBLIC_URL").unwrap_or_else(|_| {
                format!("http://127.0.0.1:{}", std::env::var("HEARTHLOOM_PORT").unwrap_or_else(|_| "43278".into()))
            });
            json!({"script": crate::agents::hermes::hook_script(&agent.id, &me, &token)})
        }
        "test" => {
            let agent = need_agent(&body.id)?;
            json!({"test": agents_test(&agent).await})
        }
        "rename_node" => {
            let name = body.name.as_deref().map(str::trim).filter(|n| !n.is_empty() && n.chars().count() <= 40)
                .ok_or_else(|| bad_request("a name of up to 40 characters"))?;
            state.store.set_consolidation_state("node.name", name)?;
            crate::agents::publish_all(&state);
            json!({"node": {"id": crate::agents::node_id(&state), "name": name}})
        }
        "move_to_peer" => {
            // this machine → the paired node
            let agent = need_agent(&body.id)?;
            crate::agents::movable(&state, &agent.id).map_err(refuse)?;
            let (peer_name, url, key) = agents_peer(&state)?;
            let client = reqwest::Client::new();
            let public = client.get(format!("{url}/v1/peer/pubkey")).bearer_auth(&key)
                .timeout(std::time::Duration::from_secs(15)).send().await
                .map_err(|e| bad_request(format!("couldn't reach {peer_name}: {e}")))?
                .json::<Value>().await.map_err(|e| bad_request(e.to_string()))?;
            let public = public["public"].as_str().ok_or_else(|| bad_request("the other node has no public key"))?.to_string();
            let bundle = crate::agents::pack(&agent, &public).map_err(refuse)?;
            let res = client.post(format!("{url}/v1/sync/agents/adopt")).bearer_auth(&key).json(&bundle)
                .timeout(std::time::Duration::from_secs(30)).send().await
                .map_err(|e| bad_request(format!("couldn't reach {peer_name}: {e}")))?;
            if !res.status().is_success() {
                return Err(bad_request(format!("{peer_name} refused it: {}", res.text().await.unwrap_or_default())));
            }
            let there: Value = res.json().await.unwrap_or_default();
            crate::agents::forget(&state, &agent).map_err(refuse)?;
            // the new host republishes; say so here at once
            let _ = state.store.publish_agent(&agent,
                there["node_id"].as_str().unwrap_or("peer"), there["node_name"].as_str().unwrap_or(&peer_name), false);
            json!({"moved_to": there["node_name"].as_str().unwrap_or(&peer_name)})
        }
        "move_from_peer" => {
            // the paired node → this machine
            let id = body.id.as_deref().ok_or_else(|| bad_request("which agent?"))?;
            let (peer_name, url, key) = agents_peer(&state)?;
            use base64::Engine;
            let mine = base64::engine::general_purpose::STANDARD
                .encode(crate::peerkey::ensure_keypair(&state.home, &state.store)?);
            let client = reqwest::Client::new();
            let res = client.post(format!("{url}/v1/sync/agents/release")).bearer_auth(&key)
                .json(&json!({"id": id, "recipient_public": mine}))
                .timeout(std::time::Duration::from_secs(30)).send().await
                .map_err(|e| bad_request(format!("couldn't reach {peer_name}: {e}")))?;
            if !res.status().is_success() {
                return Err(bad_request(format!("{peer_name} refused: {}", res.text().await.unwrap_or_default())));
            }
            let bundle: Value = res.json().await.map_err(|e| bad_request(e.to_string()))?;
            let installed = crate::agents::unpack(&state, &bundle).map_err(refuse)?;
            // only now may the old host forget it
            let _ = client.post(format!("{url}/v1/sync/agents/released")).bearer_auth(&key)
                .json(&json!({"id": id})).timeout(std::time::Duration::from_secs(30)).send().await;
            json!({"agent": installed})
        }
        _ => return Err(bad_request("action must be save|delete|mint_key|set_token|hook|test|rename_node|move_to_peer|move_from_peer")),
    };
    Ok(Json(out))
}

/// A connectivity check, nothing more: can Hearthloom reach the harness?
async fn agents_test(agent: &hearthloom_core::store::AgentRow) -> Value {
    match agent.harness.as_str() {
        "claude_code" => {
            let workdir_ok = agent.config["workdir"].as_str().is_some_and(|w| std::path::Path::new(w).is_dir());
            // the same `claude` the bridge will run
            let version = match crate::agents::claude_code::find_claude(agent) {
                Ok(path) => tokio::process::Command::new(path)
                    .arg("--version")
                    .output()
                    .await
                    .ok()
                    .filter(|o| o.status.success())
                    .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()),
                Err(_) => None,
            };
            json!({"ok": workdir_ok && version.is_some(), "claude": version, "workdir_ok": workdir_ok})
        }
        _ => {
            let Some(url) = agent.config["url"].as_str().filter(|u| !u.trim().is_empty()) else {
                return json!({"ok": false, "error": "no address set"});
            };
            let path = if agent.harness == "hermes" { "/v1/models" } else { "/" };
            let mut req = reqwest::Client::new()
                .get(format!("{}{path}", url.trim_end_matches('/')))
                .timeout(std::time::Duration::from_secs(8));
            if let Some(token) = crate::agents::secret(&agent.id, "TOKEN") {
                req = req.bearer_auth(token);
            }
            match req.send().await {
                Ok(r) => json!({"ok": r.status().is_success() || r.status().as_u16() == 404 && agent.harness == "openclaw",
                                "status": r.status().as_u16()}),
                Err(e) => json!({"ok": false, "error": e.to_string()}),
            }
        }
    }
}

#[derive(Deserialize)]
struct PolicyBody {
    id: String, // "global" or an agent id
    #[serde(default)]
    policy: Option<Value>,
    #[serde(default)]
    preset: Option<String>,
}

async fn agents_policy_set(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
    Json(body): Json<PolicyBody>,
) -> Result<Json<Value>, ApiError> {
    require_owner(&auth, "change agent settings")?;
    agents_vault_open(&state)?;
    let raw = match (&body.preset, &body.policy) {
        (Some(name), _) => serde_json::to_value(
            hearthloom_core::store::Policy::preset(name).ok_or_else(|| bad_request("unknown preset"))?,
        )
        .map_err(|e| bad_request(e.to_string()))?,
        (None, Some(policy)) => policy.clone(),
        (None, None) => return Err(bad_request("send a policy or a preset")),
    };
    let stored = state
        .store
        .set_policy(&Actor::new("human:dashboard"), &body.id, &raw)
        .map_err(|e| bad_request(format!("{e:#}")))?;
    crate::bus::touch(&state, "operational.changed");
    Ok(Json(json!({"stored": stored})))
}

/// What waits for the owner. Needs the owner, not the vault (Jon,
/// 2026-10-17, decision A) — except tasks born of a vault-open ask.
async fn agents_approvals(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
) -> Result<Json<Value>, ApiError> {
    require_owner(&auth, "see what waits for your OK")?;
    let open = !state.store.vault_status().0;
    Ok(Json(json!({"approvals": state.store.pending_approvals(open)?, "vault_open": open})))
}

#[derive(Deserialize)]
struct ApprovalBody {
    approve: bool,
}

async fn agents_approval_decide(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
    Path(id): Path<String>,
    Json(body): Json<ApprovalBody>,
) -> Result<Json<Value>, ApiError> {
    require_owner(&auth, "approve agent actions")?;
    let approval = state.store.approval(&id)?.ok_or_else(|| bad_request("no such approval"))?;
    if approval["sensitive"] == json!(true) {
        agents_vault_open(&state)?;
    }
    let by = auth.0.as_ref().map(|c| format!("key:{}", c.name)).unwrap_or_else(|| "human:dashboard".into());
    let decided = crate::agents::decide_approval(&state, &id, body.approve, &by)
        .map_err(|e| bad_request(format!("{e:#}")))?;
    Ok(Json(json!({"approval": decided})))
}

async fn agents_task_get(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    require_owner(&auth, "see agent tasks")?;
    let task = state.store.agent_task(&id)?.ok_or_else(|| bad_request("no such task"))?;
    if task.sensitive {
        agents_vault_open(&state)?;
    }
    Ok(Json(crate::agents::task_card(&state, &task)?))
}

async fn agents_task_cancel(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    require_owner(&auth, "stop agent tasks")?;
    let task = state.store.agent_task(&id)?.ok_or_else(|| bad_request("no such task"))?;
    if task.sensitive {
        agents_vault_open(&state)?;
    }
    let task = crate::agents::cancel(&state, &id, "human:dashboard").map_err(|e| bad_request(format!("{e:#}")))?;
    crate::bus::touch(&state, "operational.changed");
    Ok(Json(crate::agents::task_card(&state, &task)?))
}

fn bearer(headers: &HeaderMap) -> Option<String> {
    headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|v| v.trim().to_string())
}

/// An agent reporting its result (OpenClaw), with its one-task token.
async fn agents_task_report(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    let token = bearer(&headers).ok_or(ApiError(StatusCode::UNAUTHORIZED, "token required".into()))?;
    if crate::agents::task_for_token(&token).as_deref() != Some(id.as_str()) {
        return Err(ApiError(StatusCode::UNAUTHORIZED, "invalid token".into()));
    }
    let ok = body["ok"].as_bool().unwrap_or(true);
    let text = body["result"].as_str().or(body["error"].as_str()).unwrap_or("");
    let task = crate::agents::report(&state, &id, ok, text).map_err(|e| bad_request(format!("{e:#}")))?;
    Ok(Json(json!({"status": task.status})))
}

/// Hermes's dangerous-command hook: decided by the same policy, against
/// the agent's running task. Answers {"decision": "approve"|"deny"}.
async fn agents_hook(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(agent_id): Path<String>,
    Json(body): Json<Value>,
) -> Json<Value> {
    let deny = |why: &str| Json(json!({"decision": "deny", "reason": why}));
    let (Some(token), Some(expected)) = (bearer(&headers), crate::agents::secret(&agent_id, "HOOK")) else {
        return deny("unauthorized");
    };
    if token.len() != expected.len()
        || token.bytes().zip(expected.bytes()).fold(0u8, |acc, (a, b)| acc | (a ^ b)) != 0
    {
        return deny("unauthorized");
    }
    let running = state
        .store
        .agent_tasks(50, true)
        .unwrap_or_default()
        .into_iter()
        .find(|t| t.status == "running" && t.agent_id.as_deref() == Some(agent_id.as_str()));
    let Some(task) = running else {
        return deny("no Hearthloom task is running for this agent");
    };
    let tool = body["tool"].as_str().unwrap_or("command");
    let input = json!({"command": body["command"], "reason": body["reason"]});
    // a command Hermes flags as dangerous is a files/exec action
    let tool = if tool.is_empty() || tool == "terminal" || tool == "command" { "Bash" } else { tool };
    let (allow, reason) = crate::agents::tool_permission(&state, &task.id, tool, &input).await;
    Json(json!({"decision": if allow { "approve" } else { "deny" }, "reason": reason}))
}

/// The governance MCP server Claude Code consults for every permission
/// (`--permission-prompt-tool mcp__hearthloom_gov__approve`). One tool.
async fn agents_governance_mcp(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<Value>,
) -> Response {
    let Some(task_id) = bearer(&headers).and_then(|t| crate::agents::task_for_token(&t)) else {
        return ApiError(StatusCode::UNAUTHORIZED, "invalid token".into()).into_response();
    };
    let Some(id) = request.get("id").cloned() else {
        return StatusCode::ACCEPTED.into_response();
    };
    let result = match request["method"].as_str().unwrap_or("") {
        "initialize" => json!({
            "protocolVersion": crate::mcp::negotiate_version(&request["params"]),
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "hearthloom-governance", "version": env!("CARGO_PKG_VERSION")},
        }),
        "ping" => json!({}),
        "tools/list" => json!({"tools": [{
            "name": "approve",
            "description": "Hearthloom decides whether the agent may use a tool, by the owner's policy.",
            "inputSchema": {"type": "object", "properties": {
                "tool_name": {"type": "string"}, "input": {"type": "object"},
                "tool_use_id": {"type": "string"}}, "required": ["tool_name"]},
        }]}),
        "tools/call" => {
            let args = &request["params"]["arguments"];
            let tool = args["tool_name"].as_str().unwrap_or("");
            let input = args.get("input").cloned().unwrap_or_else(|| json!({}));
            let (allow, reason) = crate::agents::tool_permission(&state, &task_id, tool, &input).await;
            let payload = if allow {
                json!({"behavior": "allow", "updatedInput": input})
            } else {
                json!({"behavior": "deny", "message": reason})
            };
            json!({"content": [{"type": "text", "text": payload.to_string()}]})
        }
        other => {
            return Json(json!({"jsonrpc": "2.0", "id": id,
                "error": {"code": -32601, "message": format!("method not found: {other}")}})).into_response();
        }
    };
    Json(json!({"jsonrpc": "2.0", "id": id, "result": result})).into_response()
}

/* ————— reversible retirement (D.12) ————— */

#[derive(Deserialize)]
struct RetireTarget {
    kind: String,
    target: String,
}

/// Owner only; a memory must be visible under the vault as it stands; a
/// whole source needs the vault open (it may hold private memories, and
/// counting them while locked would say they exist).
fn retire_allowed(state: &AppState, auth: &AuthInfo, kind: &str, target: &str) -> Result<(), ApiError> {
    require_owner(auth, "retire memories")?;
    match kind {
        "memory" => {
            if state.store.episode_forensic(target)?.is_none() {
                return Err(ApiError(StatusCode::NOT_FOUND, "no such memory".into()));
            }
        }
        "source" => {
            if state.store.vault_status().0 {
                return Err(ApiError(
                    StatusCode::FORBIDDEN,
                    "unlock the vault to retire a whole source".into(),
                ));
            }
        }
        _ => return Err(bad_request("retire a memory or a source")),
    }
    Ok(())
}

/// What retiring would change — grouped counts, the summaries it would
/// rewrite, the beliefs that go back to Tend, the entities it would hide,
/// and what can't be traced. Writes nothing.
async fn retire_preview(
    State(state): State<AppState>,
    Query(params): Query<RetireTarget>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
) -> Result<Json<Value>, ApiError> {
    retire_allowed(&state, &auth, &params.kind, &params.target)?;
    let store = state.store.clone();
    let preview = crate::scoped::spawn_blocking(move || store.retirement_preview(&params.kind, &params.target))
        .await
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .map_err(|e| bad_request(format!("{e:#}")))?;
    Ok(Json(preview))
}

#[derive(Deserialize)]
struct RetireBody {
    kind: String,
    target: String,
    /// Summaries left ticked in the preview.
    #[serde(default)]
    regenerate: Vec<String>,
    #[serde(default)]
    reason: String,
}

/// The one confirm. A memory is applied before answering; a whole source
/// carries on in the background (`state: applying` on the list).
async fn retire_post(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
    Json(body): Json<RetireBody>,
) -> Result<Json<Value>, ApiError> {
    retire_allowed(&state, &auth, &body.kind, &body.target)?;
    let row = state
        .store
        .retire(&Actor::new("human:dashboard"), &body.kind, &body.target, &body.regenerate, &body.reason)
        .map_err(|e| bad_request(format!("{e:#}")))?;
    let id = row.id.clone();
    if body.kind == "memory" {
        let counts = crate::retire::apply_now(&state, &id)
            .await
            .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
        return Ok(Json(json!({"retirement": row, "state": "done", "counts": counts})));
    }
    let background = state.clone();
    tokio::spawn(async move {
        if let Err(err) = crate::retire::apply_now(&background, &id).await {
            tracing::warn!("retiring {id} failed part-way (the next cycle resumes it): {err:#}");
        }
    });
    Ok(Json(json!({"retirement": row, "state": "applying"})))
}

async fn retirements_list(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
) -> Result<Json<Value>, ApiError> {
    require_owner(&auth, "see retirements")?;
    Ok(Json(json!({"retirements": state.store.retirements(200)?})))
}

/// The one undo: everything the retirement changed comes back, here and —
/// through the synced event — on every other node.
async fn retirement_undo(
    State(state): State<AppState>,
    Path(id): Path<String>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
) -> Result<Json<Value>, ApiError> {
    require_owner(&auth, "undo a retirement")?;
    let row = state
        .store
        .retirement(&id)?
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "no such retirement".into()))?;
    if row.private && state.store.vault_status().0 {
        return Err(ApiError(StatusCode::NOT_FOUND, "no such retirement".into()));
    }
    let restored = crate::retire::undo_now(&state, "human:dashboard", &id)
        .await
        .map_err(|e| bad_request(format!("{e:#}")))?;
    Ok(Json(json!({"undone": id, "restored": restored})))
}

/// Peer: retirement events + summary inputs since a cursor.
async fn sync_retirements_get(
    State(state): State<AppState>,
    Query(params): Query<NotifSinceParams>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
) -> Result<Json<Value>, ApiError> {
    require_scope(&auth, "peer")?;
    Ok(Json(state.store.retirements_sync_export(&params.since)?))
}

/// Peer: take the other node's page; apply or undo what changed here.
async fn sync_retirements_post(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
    Json(batch): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    require_scope(&auth, "peer")?;
    let changed = state.store.retirements_sync_apply(&batch)?;
    if !changed.is_empty() {
        let background = state.clone();
        tokio::spawn(async move {
            crate::retire::catch_up(&background).await;
        });
    }
    Ok(Json(json!({"applied": changed.len()})))
}

async fn sync_agents_get(
    State(state): State<AppState>,
    Query(params): Query<NotifSinceParams>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
) -> Result<Json<Value>, ApiError> {
    require_scope(&auth, "peer")?;
    Ok(Json(state.store.agents_sync_export(&params.since)?))
}

/// Peer: take over an agent the other node is moving here.
async fn sync_agents_adopt(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
    Json(bundle): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    require_scope(&auth, "peer")?;
    let installed = crate::agents::unpack(&state, &bundle).map_err(|e| bad_request(format!("{e:#}")))?;
    Ok(Json(json!({"agent_id": installed.id, "node_id": crate::agents::node_id(&state),
                   "node_name": crate::agents::node_name(&state)})))
}

#[derive(Deserialize)]
struct AgentReleaseBody {
    id: String,
    #[serde(default)]
    recipient_public: String,
}

/// Peer: hand one of this node's agents to the other node (packed and
/// sealed to it). Nothing is forgotten until `released` confirms.
async fn sync_agents_release(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
    Json(body): Json<AgentReleaseBody>,
) -> Result<Json<Value>, ApiError> {
    require_scope(&auth, "peer")?;
    let agent = state.store.agent(&body.id)?.ok_or_else(|| bad_request("no such agent on this node"))?;
    crate::agents::movable(&state, &agent.id).map_err(|e| bad_request(format!("{e:#}")))?;
    Ok(Json(crate::agents::pack(&agent, &body.recipient_public).map_err(|e| bad_request(format!("{e:#}")))?))
}

async fn sync_agents_released(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
    Json(body): Json<AgentReleaseBody>,
) -> Result<Json<Value>, ApiError> {
    require_scope(&auth, "peer")?;
    if let Some(agent) = state.store.agent(&body.id)? {
        crate::agents::forget(&state, &agent).map_err(|e| bad_request(format!("{e:#}")))?;
    }
    Ok(Json(json!({"forgotten": true})))
}

async fn sync_agents_post(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
    Json(batch): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    require_scope(&auth, "peer")?;
    let applied = state.store.agents_sync_apply(&batch)?;
    if applied > 0 {
        crate::agents::kick();
    }
    Ok(Json(json!({"applied": applied})))
}

/// SSE variant for the dashboard's Ask panel: emits a `round` event per
/// tool call as the loop runs, `delta` events while the final answer is
/// written (`answer_reset` withdraws them), then `answer` — the checked,
/// authoritative text — or `fail`, then `done`. A "do it" request also
/// gets a `task` event: the hand-off's card (agents::propose).
async fn deep_query_stream(
    State(state): State<AppState>,
    Query(params): Query<QueryStreamParams>,
) -> axum::response::Sse<impl futures_core::Stream<Item = Result<axum::response::sse::Event, std::convert::Infallible>>>
{
    use axum::response::sse::Event;
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel::<Value>();
    let question = params.question;
    let thread_ids: Vec<String> = params
        .thread
        .as_deref()
        .unwrap_or("")
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect();
    let state_clone = state.clone();
    let sender_final = sender.clone();
    crate::scoped::spawn(async move {
        let actor = Actor::new("human:dashboard");
        let (context, root) = thread_context(&state_clone, &thread_ids);
        // Every ask is traced start-to-finish (QA/improvement loop).
        let ask_id = match state_clone.store.ask_start(&actor, &question, root.as_deref()) {
            Ok(id) => id,
            Err(err) => {
                let _ = sender_final.send(json!({"type": "fail", "message": err.to_string()}));
                return;
            }
        };
        let _ = sender_final.send(json!({"type": "meta", "ask_id": ask_id}));
        let started = std::time::Instant::now();
        match crate::rlm::run_query(&state_clone, &ask_id, &question, &actor, &context, Some(sender)).await {
            Ok(outcome) => {
                crate::rlm::record_query(&state_clone, &actor, &question, outcome.rounds);
                if let Some(reason) = &outcome.tainted {
                    let _ = state_clone.store.ask_set_tainted(&ask_id, reason);
                }
                let _ = state_clone.store.ask_finish(
                    &ask_id,
                    Some(&outcome.answer),
                    None,
                    &json!(outcome.citations),
                    &json!(outcome.transcript),
                    outcome.rounds,
                    started.elapsed().as_millis() as i64,
                );
                // "Do it": hand the task to governance; its card goes out
                // BEFORE the answer — the panel closes the stream on
                // `answer`, and shows the card right under it.
                if let Some(delegate) = &outcome.delegate {
                    match propose_from_ask(&state_clone, &ask_id, &question, delegate, outcome.tainted.as_deref()) {
                        Ok(card) => { let _ = sender_final.send(json!({"type": "task", "ask_id": ask_id, "card": card})); }
                        Err(err) => { let _ = sender_final.send(json!({"type": "fail", "ask_id": ask_id,
                            "message": format!("couldn't set up the task: {err:#}")})); }
                    }
                }
                let _ = sender_final.send(json!({
                    "type": "answer",
                    "ask_id": ask_id,
                    "answer": outcome.answer,
                    "citations": outcome.citations,
                    "rounds": outcome.rounds,
                }));
            }
            Err(err) => {
                let message = format!("{err:#}");
                let _ = state_clone.store.ask_finish(
                    &ask_id,
                    None,
                    Some(&message),
                    &json!([]),
                    &json!([]),
                    0,
                    started.elapsed().as_millis() as i64,
                );
                // NB: named "fail", not "error" — "error" collides with
                // EventSource's built-in connection-error event.
                let _ = sender_final.send(json!({"type": "fail", "ask_id": ask_id, "message": message}));
            }
        }
        // channel drop closes the stream
    });

    let stream = async_stream::stream! {
        while let Some(value) = receiver.recv().await {
            let kind = value.get("type").and_then(|t| t.as_str()).unwrap_or("event").to_string();
            yield Ok(Event::default().event(kind).data(value.to_string()));
        }
        yield Ok(Event::default().event("done").data("{}"));
    };
    axum::response::Sse::new(stream)
}

#[derive(Deserialize)]
struct AskHistoryParams {
    #[serde(default)]
    offset: usize,
    #[serde(default = "default_ask_limit")]
    limit: usize,
    q: Option<String>,
}

fn default_ask_limit() -> usize {
    30
}

async fn ask_history(
    State(state): State<AppState>,
    Query(params): Query<AskHistoryParams>,
) -> Result<Json<Value>, ApiError> {
    let items = state.store.ask_history(
        params.offset,
        params.limit.min(100),
        params.q.as_deref().filter(|q| !q.trim().is_empty()),
    )?;
    // Sidebar payload stays light: no transcripts in the list. One item
    // per CONVERSATION — `id` is its first turn, `turns` how many it has.
    let list: Vec<Value> = items
        .iter()
        .map(|c| {
            json!({
                "id": c.first.id, "ts": c.last_ts, "question": c.first.question,
                "status": c.last_status, "rounds": c.first.rounds,
                "turns": c.turns, "last_id": c.last_id,
                "private": c.private, "can_unseal": c.can_unseal,
            })
        })
        .collect();
    Ok(Json(json!({"items": list, "next_offset": params.offset + items.len()})))
}

async fn ask_get(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    match state.store.ask_get(&id)? {
        Some(record) => Ok(Json(serde_json::to_value(record).unwrap())),
        None => Err(ApiError(StatusCode::NOT_FOUND, format!("no ask {id}"))),
    }
}

#[derive(Deserialize)]
struct AskKeepBody {
    actor: String,
    title: Option<String>,
}

/// "Keep this answer" (G of the 2026-07-12 batch): file a synthesis
/// worth keeping back into the record as a kind=finding episode
/// (source 'ask' â derivation=inferred), citations preserved. The
/// button IS the human gate — two-phase always; the engine never wrote.
async fn ask_keep(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<AskKeepBody>,
) -> Result<Json<Value>, ApiError> {
    if body.actor.trim().is_empty() {
        return Err(bad_request("actor is required on every write"));
    }
    // ask_get is vault-guarded: a locked-away sensitive ask can't be kept.
    let record = state
        .store
        .ask_get(&id)?
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, format!("no ask {id}")))?;
    let answer = record
        .answer
        .filter(|a| !a.trim().is_empty())
        .ok_or_else(|| bad_request("this ask has no answer to keep"))?;
    // Strip the UI-only ```action fences; the finding keeps the prose.
    let clean = {
        let mut out = String::with_capacity(answer.len());
        let mut rest = answer.as_str();
        while let Some(at) = rest.find("```action") {
            out.push_str(&rest[..at]);
            match rest[at..].find("\n```").map(|end| at + end + 4) {
                Some(end) => rest = &rest[end..],
                None => {
                    rest = "";
                    break;
                }
            }
        }
        out.push_str(rest);
        out.trim().to_string()
    };
    let citations: Vec<String> = record
        .citations
        .as_array()
        .map(|list| {
            list.iter()
                .filter_map(|v| v.as_str())
                .map(String::from)
                .collect()
        })
        .unwrap_or_default();
    let title = body
        .title
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| {
            format!(
                "Finding: {}",
                record.question.replace('\n', " ").chars().take(70).collect::<String>()
            )
        });
    // An answer produced with the vault open may quote private episodes
    // — the kept finding lands private then, personal otherwise.
    let scope = if state.store.ask_sensitive(&id)? {
        "private"
    } else {
        "personal"
    };
    let source_ref = format!("keep:{id}");
    let text = if citations.is_empty() {
        format!("asked: {}\n\n{clean}", record.question)
    } else {
        format!(
            "asked: {}\n\n{clean}\n\ndrawn from:\n{}",
            record.question,
            citations.join("\n")
        )
    };
    let actor = Actor::new(body.actor);
    state.store.apply_batch(
        &state.home,
        &hearthloom_core::IngestBatch {
            actor: actor.actor.clone(),
            session_id: None,
            episodes: vec![hearthloom_core::EpisodeDraft {
                source: "ask".into(),
                source_ref: source_ref.clone(),
                kind: "finding".into(),
                title,
                body: text,
                event_time: hearthloom_core::EventTime::parse_rfc3339(
                    &hearthloom_core::time::now_utc(),
                    hearthloom_core::TimeUncertainty::Minute,
                )
                .map_err(ApiError::from)?,
                trust: "agent".into(),
                scope: Some(scope.into()),
                entities: vec![],
                edges: vec![],
            }],
        },
    )?;
    Ok(Json(json!({
        "episode_id": hearthloom_core::ids::episode_id("ask", &source_ref),
        "scope": scope,
    })))
}

/// Full conversation replay: every turn of the thread this ask belongs
/// to, oldest first (vault-guarded — locked-sensitive turns vanish).
async fn ask_thread(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let turns = state.store.ask_thread(&id)?;
    Ok(Json(json!({ "turns": turns })))
}

/// Is this conversation sealed, and could it be unsealed? 404 when it
/// isn't readable (gone — or sealed, and the vault is locked).
async fn ask_privacy_get(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    match state.store.ask_privacy(&id)? {
        Some((private, can_unseal)) => Ok(Json(json!({"private": private, "can_unseal": can_unseal}))),
        None => Err(ApiError(StatusCode::NOT_FOUND, format!("no ask {id}"))),
    }
}

#[derive(Deserialize)]
struct AskPrivacyBody {
    private: bool,
}

/// Seal (or unseal) a whole conversation by hand — the lock button in
/// the Ask panel. `visible` tells the panel whether the conversation is
/// still readable in the CURRENT lock state (sealing while locked makes
/// it vanish). Unsealing a conversation with a vault-open turn is a 400.
async fn ask_privacy_set(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<AskPrivacyBody>,
) -> Result<Json<Value>, ApiError> {
    let actor = Actor::new("human:dashboard");
    match state.store.ask_set_private(&actor, &id, body.private) {
        Ok(Some((private, can_unseal))) => {
            let (locked, _) = state.store.vault_status();
            Ok(Json(json!({
                "private": private, "can_unseal": can_unseal, "visible": !(private && locked),
            })))
        }
        Ok(None) => Err(ApiError(StatusCode::NOT_FOUND, format!("no ask {id}"))),
        Err(err) => Err(bad_request(format!("{err:#}"))),
    }
}

/// Permanent, user-confirmed removal of one history item — which is a
/// whole CONVERSATION (every turn of the one `id` belongs to). Vault-
/// guarded like ask_get: while locked, a sensitive one 404s here too.
async fn ask_delete(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let actor = Actor::new("human:dashboard");
    match state.store.ask_delete_conversation(&actor, &id)? {
        0 => Err(ApiError(StatusCode::NOT_FOUND, format!("no ask {id}"))),
        turns => Ok(Json(json!({ "deleted": id, "turns": turns }))),
    }
}

/* ————— Browse: the data explorer's faceted grids ————— */

/// Comma-separated multi-value filter param â vec (empty = no filter).
fn csv(param: &Option<String>) -> Vec<String> {
    param
        .as_deref()
        .unwrap_or("")
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

#[derive(Deserialize)]
struct BrowseEpisodesParams {
    source: Option<String>,
    kind: Option<String>,
    scope: Option<String>,
    trust: Option<String>,
    from: Option<String>,
    to: Option<String>,
    q: Option<String>,
    place: Option<String>,
    /// `all` also lists retired and replaced memories (labelled).
    lifecycle: Option<String>,
    #[serde(default)]
    sort: String,
    #[serde(default)]
    dir: String,
    #[serde(default)]
    offset: usize,
    #[serde(default = "default_browse_limit")]
    limit: usize,
}

fn default_browse_limit() -> usize {
    50
}

async fn browse_episodes(
    State(state): State<AppState>,
    Query(params): Query<BrowseEpisodesParams>,
) -> Result<Json<Value>, ApiError> {
    let filter = hearthloom_core::store::EpisodeFilter {
        sources: csv(&params.source),
        kinds: csv(&params.kind),
        scopes: csv(&params.scope),
        trusts: csv(&params.trust),
        from: params.from,
        to: params.to,
        q: params.q,
        place: params.place,
        include_retired: params.lifecycle.as_deref() == Some("all"),
    };
    let page = state.store.browse_episodes(
        &filter,
        &params.sort,
        &params.dir,
        params.offset,
        params.limit,
    )?;
    Ok(Json(serde_json::to_value(page).unwrap()))
}

#[derive(Deserialize)]
struct BrowseEntitiesParams {
    #[serde(rename = "type")]
    entity_type: Option<String>,
    scope: Option<String>,
    q: Option<String>,
    #[serde(default)]
    sort: String,
    #[serde(default)]
    dir: String,
    #[serde(default)]
    offset: usize,
    #[serde(default = "default_browse_limit")]
    limit: usize,
}

async fn browse_entities(
    State(state): State<AppState>,
    Query(params): Query<BrowseEntitiesParams>,
) -> Result<Json<Value>, ApiError> {
    let page = state.store.browse_entities(
        &csv(&params.entity_type),
        &csv(&params.scope),
        params.q.as_deref(),
        &params.sort,
        &params.dir,
        params.offset,
        params.limit,
    )?;
    Ok(Json(serde_json::to_value(page).unwrap()))
}

#[derive(Deserialize)]
struct BrowseIdentitiesParams {
    kind: Option<String>,
    q: Option<String>,
    #[serde(default)]
    offset: usize,
    #[serde(default = "default_browse_limit")]
    limit: usize,
}

async fn browse_identities(
    State(state): State<AppState>,
    Query(params): Query<BrowseIdentitiesParams>,
) -> Result<Json<Value>, ApiError> {
    let page = state.store.browse_identities(
        &csv(&params.kind),
        params.q.as_deref(),
        params.offset,
        params.limit,
    )?;
    Ok(Json(serde_json::to_value(page).unwrap()))
}

#[derive(Deserialize)]
struct BrowseEdgesParams {
    relation: Option<String>,
    status: Option<String>,
    #[serde(default)]
    offset: usize,
    #[serde(default = "default_browse_limit")]
    limit: usize,
}

async fn browse_edges(
    State(state): State<AppState>,
    Query(params): Query<BrowseEdgesParams>,
) -> Result<Json<Value>, ApiError> {
    let page = state.store.browse_edges(
        &csv(&params.relation),
        &csv(&params.status),
        params.offset,
        params.limit,
    )?;
    Ok(Json(serde_json::to_value(page).unwrap()))
}

async fn browse_sources(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    // Lazy sync keeps the registry honest without a write on every ingest.
    state.store.sync_sources()?;
    Ok(Json(json!({"sources": state.store.list_sources()?})))
}

/* ————— the vault + privacy rules ————— */

fn vault_json(state: &AppState) -> Value {
    let (locked, seconds_remaining) = state.store.vault_status();
    json!({"locked": locked, "seconds_remaining": seconds_remaining})
}

async fn vault_status(State(state): State<AppState>) -> Json<Value> {
    Json(vault_json(&state))
}

#[derive(Deserialize)]
struct VaultBody {
    actor: String,
    action: String, // unlock|lock
    minutes: Option<u64>,
}

async fn vault_set(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
    Json(body): Json<VaultBody>,
) -> Result<Json<Value>, ApiError> {
    let actor = Actor::new(body.actor);
    match body.action.as_str() {
        "unlock" => {
            // Unlocking is the owner's act alone (locking is anyone's).
            require_owner(&auth, "unlock the vault")?;
            let minutes = body.minutes.unwrap_or(15).clamp(1, 60);
            state.store.vault_unlock(minutes);
            // Who did it, as the server knows it — not as the request says.
            let via = auth.0.as_ref().map(|c| format!("key:{}", c.name)).unwrap_or_else(|| "session".into());
            state
                .store
                .record_event(&actor, "vault_unlocked", &json!({"minutes": minutes, "via": via}))?;
        }
        "lock" => {
            state.store.vault_lock();
            state.store.record_event(&actor, "vault_locked", &json!({}))?;
        }
        other => {
            return Err(bad_request(format!(
                "action must be unlock|lock, got {other:?}"
            )))
        }
    }
    Ok(Json(vault_json(&state)))
}

#[derive(Deserialize)]
struct PrivacyRuleBody {
    actor: String,
    kind: String,
    pattern: String,
}

async fn privacy_rules_list(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    // The page loads always; the RULE PATTERNS (a vaulted domain IS
    // sensitive) reveal only when the vault is unlocked — count stays
    // honest, details gate (Jon 2026-07-10).
    let locked = state.store.vault_status().0;
    let rules = state.store.list_privacy_rules()?;
    if locked {
        let masked: Vec<Value> = rules
            .iter()
            .map(|r| json!({"id": r.id, "kind": r.kind, "pattern": null,
                            "created_at": r.created_at}))
            .collect();
        return Ok(Json(json!({"rules": masked, "locked": true, "count": rules.len()})));
    }
    Ok(Json(json!({"rules": rules, "locked": false})))
}

async fn vaulted_notes(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    Ok(Json(json!({
        "count": state.store.vaulted_note_count()?,
        "notes": state.store.vaulted_notes(200)?,
        "locked": state.store.vault_status().0,
    })))
}

async fn privacy_rules_add(
    State(state): State<AppState>,
    Json(body): Json<PrivacyRuleBody>,
) -> Result<Json<Value>, ApiError> {
    let actor = Actor::new(body.actor);
    let (rule, relabeled) = state
        .store
        .add_privacy_rule(&actor, &body.kind, &body.pattern)?;
    crate::bus::touch(&state, "privacy.changed");
    Ok(Json(json!({"rule": rule, "episodes_relabeled": relabeled})))
}

#[derive(Deserialize)]
struct ActorParam {
    actor: String,
}

async fn privacy_rules_remove(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<ActorParam>,
) -> Result<Json<Value>, ApiError> {
    let removed = state
        .store
        .remove_privacy_rule(&Actor::new(params.actor), &id)?;
    crate::bus::touch(&state, "privacy.changed");
    Ok(Json(json!({"removed": removed})))
}

async fn consolidate_now(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let summary = crate::consolidate::consolidate(&state).await?;
    Ok(Json(summary))
}

async fn list_intents(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    Ok(Json(json!({"intents": state.store.list_intents()?})))
}

#[derive(Deserialize)]
struct IntentBody {
    actor: String,
    name: String,
    query: String,
    description: Option<String>,
    sphere: Option<String>,
    target_date: Option<String>,
}

async fn add_intent(
    State(state): State<AppState>,
    Json(body): Json<IntentBody>,
) -> Result<Json<Value>, ApiError> {
    if body.actor.trim().is_empty() {
        return Err(bad_request("actor is required on every write"));
    }
    let actor = Actor::new(body.actor);
    let id = state.store.add_intent(&actor, &body.name, &body.query)?;
    if body.description.is_some() || body.sphere.is_some() || body.target_date.is_some() {
        state.store.update_goal(
            &actor,
            &body.name,
            body.description.as_deref(),
            None,
            body.sphere.as_deref(),
            body.target_date.as_deref(),
            None,
            None,
        )?;
    }
    // Fire the setup pass in the background: decompose into standing
    // queries, scan the backlog, run the watch — a new goal should know
    // what's already true within seconds, not at the next idle cycle.
    tokio::spawn(crate::consolidate::setup_goal(state.clone(), id.clone()));
    crate::bus::touch(&state, "operational.changed");
    Ok(Json(json!({"id": id, "name": body.name})))
}

#[derive(Deserialize)]
struct GoalQueriesBody {
    actor: String,
    #[serde(default)]
    add: Vec<String>,
    #[serde(default)]
    remove: Vec<String>,
}

/// Edit a goal's watch terms directly — the human's direction outranks
/// the LLM's decomposition. Added terms trigger an immediate backlog
/// scan so new matches surface right away.
async fn goal_queries(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<GoalQueriesBody>,
) -> Result<Json<Value>, ApiError> {
    if body.actor.trim().is_empty() {
        return Err(bad_request("actor is required on every write"));
    }
    let actor = Actor::new(body.actor);
    let intent_id = state
        .store
        .intent_id_by_name(&name)?
        .ok_or_else(|| bad_request(format!("no goal named {name:?}")))?;
    for query in &body.remove {
        state.store.remove_standing_query(&actor, &intent_id, query)?;
    }
    if !body.add.is_empty() {
        let queries: Vec<(String, Option<Vec<f32>>)> = body
            .add
            .iter()
            .map(|q| {
                let embedding = state.embedder.as_ref().and_then(|e| e.embed(q).ok());
                (q.clone(), embedding)
            })
            .collect();
        state.store.add_standing_queries(&actor, &intent_id, &queries)?;
        let store = state.store.clone();
        let scan_actor = actor.clone();
        let id = intent_id.clone();
        tokio::spawn(async move {
            let _ = crate::scoped::spawn_blocking(move || {
                store.run_intent_backlog(&scan_actor, &id, 200)
            })
            .await;
        });
    }
    crate::bus::touch(&state, "operational.changed");
    Ok(Json(json!({"name": name})))
}

#[derive(Deserialize)]
struct GoalUpdateBody {
    actor: String,
    description: Option<String>,
    /// active | achieved | abandoned
    status: Option<String>,
    sphere: Option<String>,
    target_date: Option<String>,
    /// A new statement redraws the watch (standing queries rebuilt).
    query: Option<String>,
    new_name: Option<String>,
}

async fn update_goal(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<GoalUpdateBody>,
) -> Result<Json<Value>, ApiError> {
    if body.actor.trim().is_empty() {
        return Err(bad_request("actor is required on every write"));
    }
    let restated = state.store.update_goal(
        &Actor::new(body.actor),
        &name,
        body.description.as_deref(),
        body.status.as_deref(),
        body.sphere.as_deref(),
        body.target_date.as_deref(),
        body.query.as_deref(),
        body.new_name.as_deref(),
    )?;
    let final_name = body.new_name.clone().unwrap_or_else(|| name.clone());
    if restated {
        // the watch was redrawn — set it up again right away
        if let Some(id) = state.store.intent_id_by_name(&final_name)? {
            tokio::spawn(crate::consolidate::setup_goal(state.clone(), id));
        }
    }
    crate::bus::touch(&state, "operational.changed");
    Ok(Json(json!({"name": final_name, "restated": restated})))
}

#[derive(Deserialize)]
struct RemoveIntentParams {
    actor: String,
}

async fn remove_intent(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Query(params): Query<RemoveIntentParams>,
) -> Result<Json<Value>, ApiError> {
    state
        .store
        .remove_intent(&Actor::new(params.actor), &name)?;
    crate::bus::touch(&state, "operational.changed");
    Ok(Json(json!({"removed": name})))
}

#[derive(Deserialize)]
struct MatchesParams {
    /// Viewing is the lazy-cascade trigger; pass false to peek silently.
    #[serde(default = "default_true")]
    mark_seen: bool,
}

fn default_true() -> bool {
    true
}

async fn intent_matches(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Query(params): Query<MatchesParams>,
) -> Result<Json<Value>, ApiError> {
    let matches = state.store.intent_matches(&name, params.mark_seen)?;
    Ok(Json(json!({"intent": name, "matches": matches})))
}

#[derive(Deserialize)]
struct ImportanceBody {
    actor: String,
    /// 0 = unset, 1..3 — a tier, not a score.
    level: i64,
}

async fn set_importance(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<ImportanceBody>,
) -> Result<Json<Value>, ApiError> {
    if body.actor.trim().is_empty() {
        return Err(bad_request("actor is required on every write"));
    }
    state
        .store
        .set_declared_importance(&Actor::new(body.actor), &name, body.level)?;
    Ok(Json(json!({"entity": name, "level": body.level})))
}

#[derive(Deserialize)]
struct SelfModelListParams {
    status: Option<String>,
}

async fn selfmodel_list(
    State(state): State<AppState>,
    Query(params): Query<SelfModelListParams>,
) -> Result<Json<Value>, ApiError> {
    let mut propositions = serde_json::to_value(state.store.list_propositions(params.status.as_deref())?)
        .unwrap_or_default();
    // Beliefs a retirement sent back say why they are asking again.
    let back = state.store.beliefs_sent_back()?;
    if !back.is_empty() {
        for p in propositions.as_array_mut().into_iter().flatten() {
            if p["status"] == "proposed" {
                if let Some(label) = p["id"].as_str().and_then(|id| back.get(id)) {
                    p["back_after"] = json!(label);
                }
            }
        }
    }
    Ok(Json(json!({"propositions": propositions})))
}

#[derive(Deserialize)]
struct SelfModelAddBody {
    actor: String,
    sphere: String,
    kind: String,
    text: String,
}

/// Human-declared propositions only — the grower writes inferred ones
/// through the store directly, with evidence.
async fn selfmodel_add(
    State(state): State<AppState>,
    Json(body): Json<SelfModelAddBody>,
) -> Result<Json<Value>, ApiError> {
    if body.actor.trim().is_empty() {
        return Err(bad_request("actor is required on every write"));
    }
    let embedding = state
        .embedder
        .as_ref()
        .and_then(|e| e.embed(&body.text).ok());
    let id = state.store.add_proposition(
        &Actor::new(body.actor),
        &body.sphere,
        &body.kind,
        &body.text,
        "declared",
        &[],
        embedding.as_deref(),
    )?;
    crate::bus::touch(&state, "operational.changed");
    Ok(Json(json!({"id": id})))
}

#[derive(Deserialize)]
struct SelfModelResolveBody {
    actor: String,
    /// confirm | reject | retire; confirm may carry edited text.
    action: String,
    text: Option<String>,
}

async fn selfmodel_resolve(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<SelfModelResolveBody>,
) -> Result<Json<Value>, ApiError> {
    if body.actor.trim().is_empty() {
        return Err(bad_request("actor is required on every write"));
    }
    state.store.resolve_proposition(
        &Actor::new(body.actor),
        &id,
        &body.action,
        body.text.as_deref(),
    )?;
    // A confirmed seeking proposition should match immediately next
    // cycle; make sure it has an embedding even if added pre-embedder.
    if body.action == "confirm" {
        if let Some(embedder) = &state.embedder {
            if let Some(p) = state
                .store
                .list_propositions(Some("confirmed"))?
                .into_iter()
                .find(|p| p.id == id)
            {
                if let Ok(vector) = embedder.embed(&p.text) {
                    state.store.set_proposition_embedding(&id, &vector)?;
                }
            }
        }
    }
    crate::bus::touch(&state, "operational.changed");
    Ok(Json(json!({"id": id, "action": body.action})))
}

async fn opportunities_list(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    Ok(Json(json!({"opportunities": state.store.list_opportunities(50)?})))
}

#[derive(Deserialize)]
struct OpportunityResolveBody {
    actor: String,
    /// keep | dismiss. Keeping mints a kind=opportunity episode.
    action: String,
}

async fn opportunity_resolve(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<OpportunityResolveBody>,
) -> Result<Json<Value>, ApiError> {
    if body.actor.trim().is_empty() {
        return Err(bad_request("actor is required on every write"));
    }
    let keep = match body.action.as_str() {
        "keep" => true,
        "dismiss" => false,
        other => return Err(bad_request(format!("action must be keep|dismiss, got {other:?}"))),
    };
    let episode_id =
        state
            .store
            .resolve_opportunity(&Actor::new(body.actor), &state.home, &id, keep)?;
    Ok(Json(json!({"id": id, "action": body.action, "episode_id": episode_id})))
}

/* ————— the record lint: report-only findings, human-resolved ————— */

async fn lint_list(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    Ok(Json(json!({"findings": state.store.lint_pending(50)?})))
}

#[derive(Deserialize)]
struct LintResolveBody {
    actor: String,
    resolution: String, // fixed | not_an_issue
}

async fn lint_resolve(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<LintResolveBody>,
) -> Result<Json<Value>, ApiError> {
    if body.actor.trim().is_empty() {
        return Err(bad_request("actor is required on every write"));
    }
    state
        .store
        .resolve_lint_finding(&Actor::new(body.actor), &id, &body.resolution)?;
    Ok(Json(json!({"id": id, "resolution": body.resolution})))
}

/* ————— the Desk: tasks, templates, notes, metrics ————— */

#[derive(Deserialize)]
struct TasksListParams {
    status: Option<String>,
    /// Entity name: only tasks @-mentioning them (the dossier section).
    mentions: Option<String>,
    #[serde(default = "default_task_limit")]
    limit: usize,
}
fn default_task_limit() -> usize {
    200
}

async fn tasks_list(
    State(state): State<AppState>,
    Query(params): Query<TasksListParams>,
) -> Result<Json<Value>, ApiError> {
    if let Some(name) = params.mentions.as_deref().filter(|n| !n.trim().is_empty()) {
        let entity = resolve_entity_row(&state, name)?;
        let tasks = state.store.tasks_mentioning(&entity.id, params.limit.min(100))?;
        return Ok(Json(json!({"tasks": tasks, "mentions": entity.name})));
    }
    let tasks = state
        .store
        .list_tasks(params.status.as_deref(), params.limit.min(500))?;
    let completions = state.store.task_completions_by_day(14)?;
    Ok(Json(json!({"tasks": tasks, "completions": completions})))
}

#[derive(Deserialize)]
struct TaskAddBody {
    actor: String,
    #[serde(default = "default_task_kind")]
    kind: String,
    description: String,
    payload: Option<Value>,
    template_id: Option<String>,
    #[serde(default = "default_frequency")]
    frequency: String,
    #[serde(default = "default_task_scope")]
    scope: String,
    goal_id: Option<String>,
    remind_at: Option<String>,
}
fn default_task_kind() -> String {
    "plain".into()
}
fn default_frequency() -> String {
    "one-time".into()
}
fn default_task_scope() -> String {
    "personal".into()
}

async fn task_add(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
    Json(body): Json<TaskAddBody>,
) -> Result<Json<Value>, ApiError> {
    if body.actor.trim().is_empty() {
        return Err(bad_request("actor is required on every write"));
    }
    let id = state.store.add_task(
        &write_actor(&auth, body.actor),
        &body.kind,
        &body.description,
        body.payload.as_ref(),
        body.template_id.as_deref(),
        &body.frequency,
        &body.scope,
        body.goal_id.as_deref(),
        body.remind_at.as_deref(),
        &[],
    )?;
    crate::bus::touch(&state, "operational.changed");
    Ok(Json(json!({"id": id})))
}

#[derive(Deserialize)]
struct TaskUpdateBody {
    actor: String,
    description: Option<String>,
    payload: Option<Value>,
    /// open | backlog | deleted (completion has its own route)
    status: Option<String>,
    /// present-but-null clears the reminder
    #[serde(default, with = "double_option")]
    remind_at: Option<Option<String>>,
    #[serde(default, with = "double_option")]
    goal_id: Option<Option<String>>,
}

/// Distinguish "absent" from "present but null" for clearable fields.
mod double_option {
    use serde::{Deserialize, Deserializer};
    pub fn deserialize<'de, D>(d: D) -> Result<Option<Option<String>>, D::Error>
    where
        D: Deserializer<'de>,
    {
        Option::<String>::deserialize(d).map(Some)
    }
}

async fn task_update(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<TaskUpdateBody>,
) -> Result<Json<Value>, ApiError> {
    if body.actor.trim().is_empty() {
        return Err(bad_request("actor is required on every write"));
    }
    state.store.update_task(
        &Actor::new(body.actor),
        &id,
        body.description.as_deref(),
        body.payload.as_ref(),
        body.status.as_deref(),
        body.remind_at.as_ref().map(|o| o.as_deref()),
        body.goal_id.as_ref().map(|o| o.as_deref()),
    )?;
    crate::bus::touch(&state, "operational.changed");
    Ok(Json(json!({"id": id})))
}

#[derive(Deserialize)]
struct TaskPurgeParams {
    actor: String,
}

async fn task_purge(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<TaskPurgeParams>,
) -> Result<Json<Value>, ApiError> {
    state.store.purge_task(&Actor::new(params.actor), &id)?;
    crate::bus::touch(&state, "operational.changed");
    Ok(Json(json!({"purged": id})))
}

#[derive(Deserialize)]
struct TaskCompleteBody {
    actor: String,
    payload: Option<Value>,
}

async fn task_complete(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<TaskCompleteBody>,
) -> Result<Json<Value>, ApiError> {
    if body.actor.trim().is_empty() {
        return Err(bad_request("actor is required on every write"));
    }
    let task_before = state.store.task(&id)?;
    let outcome = state.store.complete_task(
        &Actor::new(body.actor),
        &state.home,
        &id,
        body.payload.as_ref(),
    )?;
    // The task â research â note loop: hand the UI a prefilled note —
    // rendered from the bound template's skeleton when there is one.
    let prefill = if outcome.offer_note {
        state.store.task(&id)?.or(task_before).map(|task| {
            let (title, body) = match task
                .template_id
                .as_deref()
                .and_then(|tid| state.store.template(&state.home, tid).ok().flatten())
            {
                Some(template) => hearthloom_core::store::render_template(&template, &task),
                None => hearthloom_core::store::note_prefill_for_task(&task),
            };
            json!({"title": title, "body": body, "linked_task": id})
        })
    } else {
        None
    };
    crate::bus::touch(&state, "operational.changed");
    Ok(Json(json!({
        "episode_id": outcome.episode_id,
        "next_task_id": outcome.next_task_id,
        "note_prefill": prefill,
    })))
}

/// Bring a completed task back to the desk as an open item.
async fn task_uncomplete(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<TaskCompleteBody>,
) -> Result<Json<Value>, ApiError> {
    if body.actor.trim().is_empty() {
        return Err(bad_request("actor is required on every write"));
    }
    state.store.uncomplete_task(&Actor::new(body.actor), &id)?;
    crate::bus::touch(&state, "operational.changed");
    Ok(Json(json!({"id": id, "status": "open"})))
}

/// Restore a task from the operational journal (archived, edited-away, or purged).
async fn task_restore(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<TaskCompleteBody>,
) -> Result<Json<Value>, ApiError> {
    if body.actor.trim().is_empty() {
        return Err(bad_request("actor is required on every write"));
    }
    state.store.restore_task(&Actor::new(body.actor), &id)?;
    crate::bus::touch(&state, "operational.changed");
    Ok(Json(json!({"id": id, "restored": true})))
}

/// Purged tasks recoverable from the journal — the "recently deleted" list.
async fn tasks_deleted(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let rows = state.store.deleted_tasks(100)?;
    let deleted: Vec<Value> = rows
        .into_iter()
        .map(|(id, description, ts)| json!({"id": id, "description": description, "deleted_at": ts}))
        .collect();
    Ok(Json(json!({ "deleted": deleted })))
}

async fn templates_list(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    Ok(Json(json!({"templates": state.store.list_templates(&state.home)?})))
}

#[derive(Deserialize)]
struct TemplateAddBody {
    actor: String,
    name: String,
    /// [{key, label, type, options?}] — becomes the frontmatter block.
    #[serde(default)]
    fields: Value,
    #[serde(default)]
    note_on_complete: bool,
    /// The note skeleton ({{field-key}}, {{task}}, {{date}}).
    #[serde(default)]
    skeleton: String,
}

/// Templates ARE notes: this convenience assembles the frontmatter and
/// creates a kind=template note (fully editable in the Notes view).
async fn template_add(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
    Json(body): Json<TemplateAddBody>,
) -> Result<Json<Value>, ApiError> {
    if body.actor.trim().is_empty() {
        return Err(bad_request("actor is required on every write"));
    }
    let mut front = String::from("---template\n");
    for field in body.fields.as_array().into_iter().flatten() {
        let key = field.get("key").and_then(|v| v.as_str()).unwrap_or("");
        if key.is_empty() {
            continue;
        }
        let label = field.get("label").and_then(|v| v.as_str()).unwrap_or(key);
        let ftype = field.get("type").and_then(|v| v.as_str()).unwrap_or("text");
        front.push_str(&format!("field: {key} | {label} | {ftype}"));
        if let Some(options) = field.get("options").and_then(|v| v.as_array()) {
            let opts: Vec<&str> = options.iter().filter_map(|o| o.as_str()).collect();
            front.push_str(&format!(" | {}", opts.join(",")));
        }
        front.push('\n');
    }
    front.push_str(&format!("note-on-complete: {}\n---\n", body.note_on_complete));
    let note_body = format!("{front}{}", if body.skeleton.is_empty() {
        "## What I learned\n\n"
    } else {
        &body.skeleton
    });
    let id = state.store.create_note_kind(
        &write_actor(&auth, body.actor),
        &state.home,
        &body.name,
        &note_body,
        &[],
        "personal",
        None,
        "hearthloom",
        None,
        "template",
    )?;
    crate::bus::touch(&state, "operational.changed");
    Ok(Json(json!({"id": id})))
}

#[derive(Deserialize)]
struct NotesListParams {
    q: Option<String>,
    /// Entity NAME — only notes @-mentioning them (query, never a scan).
    mentions: Option<String>,
    /// Exact tag filter (leading # tolerated).
    tag: Option<String>,
    /// Infinite-scroll paging.
    #[serde(default)]
    offset: usize,
    #[serde(default = "default_task_limit")]
    limit: usize,
}

async fn notes_list(
    State(state): State<AppState>,
    Query(params): Query<NotesListParams>,
) -> Result<Json<Value>, ApiError> {
    if let Some(name) = params.mentions.as_deref().filter(|m| !m.trim().is_empty()) {
        let entity = resolve_entity_row(&state, name)?;
        let notes = state.store.notes_mentioning(&entity.id, params.limit.min(500))?;
        return Ok(Json(json!({"notes": notes, "mentions": entity.name})));
    }
    if params.tag.is_some() || params.offset > 0 {
        let notes = state.store.list_notes_filtered(
            params.q.as_deref(),
            params.tag.as_deref(),
            params.limit.min(500),
            params.offset,
        )?;
        return Ok(Json(json!({"notes": notes})));
    }
    let notes = state
        .store
        .list_notes(params.q.as_deref(), params.limit.min(500))?;
    Ok(Json(json!({"notes": notes})))
}

#[derive(Deserialize)]
struct GalaxyParams {
    from: Option<String>,
    to: Option<String>,
    #[serde(default = "default_galaxy_per_source")]
    per_source: usize,
}
fn default_galaxy_per_source() -> usize {
    9
}

/// The whole-record map: sources as suns, top entities as planets with
/// gravity spokes, co-citation links, and the idle-computed semantic
/// comet pairs. Windowed for the dial's time-lapse; vault-guarded.
///
/// Cached per (window, vault-state) for 10 minutes: the full-life
/// aggregate walks every episodeâentity link (~2.4s) and the map
/// changes at ingest speed, not click speed. Vault state keys the
/// cache so locking never serves an unlocked render (and vice versa).
async fn galaxy_get(
    State(state): State<AppState>,
    Query(params): Query<GalaxyParams>,
) -> Result<Json<Value>, ApiError> {
    use std::sync::{Mutex, OnceLock};
    use std::time::{Duration, Instant};
    static CACHE: OnceLock<Mutex<std::collections::HashMap<String, (Instant, Value)>>> =
        OnceLock::new();
    let from = params.from.as_deref().unwrap_or("0000-01-01").to_string();
    let to = params.to.as_deref().unwrap_or("9999-12-31").to_string();
    let per_source = params.per_source.min(15);
    let (locked, _) = state.store.vault_status();
    let key = format!("{from}|{to}|{per_source}|{locked}");
    let cache = CACHE.get_or_init(|| Mutex::new(std::collections::HashMap::new()));
    if let Some((at, value)) = cache.lock().unwrap().get(&key) {
        if at.elapsed() < Duration::from_secs(600) {
            return Ok(Json(value.clone()));
        }
    }
    let data = {
        let store = state.store.clone();
        let (from, to) = (from.clone(), to.clone());
        crate::scoped::spawn_blocking(move || store.galaxy(&from, &to, per_source))
            .await
            .map_err(anyhow::Error::from)??
    };
    let value = serde_json::to_value(data).map_err(anyhow::Error::from)?;
    {
        let mut cache = cache.lock().unwrap();
        if cache.len() > 24 {
            cache.clear();
        }
        cache.insert(key, (Instant::now(), value.clone()));
    }
    Ok(Json(value))
}

#[derive(Deserialize)]
struct ConnectionsParams {
    q: Option<String>,
    name: Option<String>,
    #[serde(default = "default_connections_limit")]
    limit: usize,
}
fn default_connections_limit() -> usize {
    12
}

/// The social-connections registry: search (?q=) or exact lookup
/// (?name=). Names a network says you know — NOT entities until a
/// human materializes them or real evidence does.
async fn connections_get(
    State(state): State<AppState>,
    Query(params): Query<ConnectionsParams>,
) -> Result<Json<Value>, ApiError> {
    let rows = state.store.social_connections(
        params.q.as_deref(),
        params.name.as_deref(),
        params.limit.min(50),
    )?;
    Ok(Json(json!({"connections": rows})))
}

#[derive(Deserialize)]
struct MaterializeBody {
    actor: String,
    network: String,
    name: String,
}

/// The explicit gesture: registry row â Person entity.
async fn connection_materialize(
    State(state): State<AppState>,
    Json(body): Json<MaterializeBody>,
) -> Result<Json<Value>, ApiError> {
    if body.actor.trim().is_empty() {
        return Err(bad_request("actor is required on every write"));
    }
    let id = state
        .store
        .materialize_connection(&Actor::new(body.actor), &body.network, &body.name)?;
    Ok(Json(json!({"entity_id": id, "name": body.name})))
}

#[derive(Deserialize)]
struct SimilarParams {
    name: String,
}

/// Live close-match suggestions for the merge menu: same-type entities
/// sharing a whole name token (â¥2 shared or a subset relation — the
/// tightened merge-scan rule, on demand for ONE name).
async fn entities_similar(
    State(state): State<AppState>,
    Query(params): Query<SimilarParams>,
) -> Result<Json<Value>, ApiError> {
    let target = resolve_entity_row(&state, &params.name)?;
    // tokenize the resolved NAME — params.name may be an entity id
    let tokens: std::collections::HashSet<String> = target
        .name
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.len() >= 3)
        .map(String::from)
        .collect();
    let mut seen = std::collections::HashSet::new();
    let mut matches = Vec::new();
    for token in tokens.iter().take(4) {
        for (id, name) in state.store.persons_sharing_token(token, &target.id, 15)? {
            if !seen.insert(id.clone()) {
                continue;
            }
            let other: std::collections::HashSet<String> = name
                .to_lowercase()
                .split(|c: char| !c.is_alphanumeric())
                .filter(|t| t.len() >= 3)
                .map(String::from)
                .collect();
            let shared = tokens.intersection(&other).count();
            let subset = tokens.is_subset(&other) || other.is_subset(&tokens);
            if shared >= 2 || subset {
                matches.push((id, name));
            }
        }
    }
    matches.truncate(8);
    // Identify each candidate before the human decides: email/phone +
    // episode count ("which Maria is this?").
    let matches: Vec<Value> = matches
        .into_iter()
        .map(|(id, name)| -> anyhow::Result<Value> {
            let (handle, episodes) = state.store.entity_contact_card(&id)?;
            Ok(json!({"id": id, "name": name, "handle": handle, "episodes": episodes}))
        })
        .collect::<anyhow::Result<_>>()?;
    Ok(Json(json!({"target": target, "matches": matches})))
}

/// Serve an image preserved from a note import — VAULT-GUARDED via the
/// owning note's scope: a diagram inside a private note is as invisible
/// as the note (plain 404, zero leakage), exactly like every read path.
async fn notes_media(
    State(state): State<AppState>,
    Path((media_ref, file)): Path<(String, String)>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let not_found = || (axum::http::StatusCode::NOT_FOUND, "no such media").into_response();
    // path hygiene: single segments only, no traversal
    if media_ref.contains("..") || file.contains("..") || media_ref.contains('/') || file.contains('/') {
        return not_found();
    }
    let Ok(Some(scope)) = state.store.media_owner_scope(&media_ref) else {
        return not_found();
    };
    if scope == "private" && state.store.vault_status().0 {
        return not_found();
    }
    let path = state.home.notes_media_dir().join(&media_ref).join(&file);
    let Ok(bytes) = std::fs::read(&path) else {
        return not_found();
    };
    let mime = match file.rsplit('.').next().unwrap_or("").to_lowercase().as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "heic" => "image/heic",
        "bmp" => "image/bmp",
        _ => "application/octet-stream",
    };
    // Imported media is other people's content in the dashboard's
    // origin: an SVG opened directly would run its scripts. Sandboxed,
    // script-free, and never sniffed into something else.
    (
        [
            (axum::http::header::CONTENT_TYPE, mime),
            (
                axum::http::header::CONTENT_SECURITY_POLICY,
                "default-src 'none'; img-src 'self' data:; style-src 'unsafe-inline'; sandbox",
            ),
            (axum::http::header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        bytes,
    )
        .into_response()
}

async fn projects_list(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    Ok(Json(json!({"projects": state.store.list_projects()?})))
}

#[derive(Deserialize)]
struct ProjectDeclareBody {
    actor: String,
    name: String,
}
async fn project_declare(
    State(state): State<AppState>,
    Json(body): Json<ProjectDeclareBody>,
) -> Result<Json<Value>, ApiError> {
    if body.actor.trim().is_empty() {
        return Err(bad_request("actor is required on every write"));
    }
    let id = state.store.declare_project(&Actor::new(body.actor), &body.name)?;
    crate::bus::touch(&state, "operational.changed");
    Ok(Json(json!({"id": id, "name": body.name})))
}

#[derive(Deserialize)]
struct EntityRenameBody {
    actor: String,
    id: String,
    name: String,
}
/// Rename an entity by ID (item 12) — dependencies follow the id, the
/// v37 audit trail carries the rename to the peer (item 14).
async fn entity_rename(
    State(state): State<AppState>,
    Json(body): Json<EntityRenameBody>,
) -> Result<Json<Value>, ApiError> {
    if body.actor.trim().is_empty() {
        return Err(bad_request("actor is required on every write"));
    }
    let (id, from, to) = state
        .store
        .rename_entity(&Actor::new(body.actor), &body.id, &body.name)
        .map_err(|e| bad_request(format!("{e:#}")))?;
    crate::bus::touch(&state, "entity.renamed");
    Ok(Json(json!({"id": id, "from": from, "to": to})))
}

async fn project_detail(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let id = resolve_entity_arg(&state, &id)?; // {id} may be a name
    let repos: Vec<Value> = state
        .store
        .project_detail(&id)?
        .into_iter()
        .map(|(id, name)| json!({"id": id, "name": name}))
        .collect();
    let goals = state.store.goals_for_project(&id)?;
    let primary = state
        .store
        .primary_note_meta(&state.home, &id)?
        .map(|(nid, title, excerpt)| json!({"id": nid, "title": title, "excerpt": excerpt}));
    Ok(Json(json!({"repos": repos, "goals": goals, "primary_note": primary})))
}

#[derive(Deserialize)]
struct ProjectChildBody {
    actor: String,
    /// The entity to compose under (or remove from) the project — id or
    /// exact name (a repo Project, usually).
    child: String,
}
async fn project_attach(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<ProjectChildBody>,
) -> Result<Json<Value>, ApiError> {
    if body.actor.trim().is_empty() {
        return Err(bad_request("actor is required on every write"));
    }
    let project = resolve_entity_arg(&state, &id)?; // {id} may be a name
    let child = resolve_entity_arg(&state, &body.child)?;
    state.store.attach_to_project(&Actor::new(body.actor), &project, &child)?;
    crate::bus::touch(&state, "operational.changed");
    Ok(Json(json!({"project": project, "child": child})))
}
async fn project_detach(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<ProjectChildBody>,
) -> Result<Json<Value>, ApiError> {
    if body.actor.trim().is_empty() {
        return Err(bad_request("actor is required on every write"));
    }
    let project = resolve_entity_arg(&state, &id)?;
    let child = resolve_entity_arg(&state, &body.child)?;
    state.store.detach_from_project(&Actor::new(body.actor), &project, &child)?;
    crate::bus::touch(&state, "operational.changed");
    Ok(Json(json!({"project": project, "child": child})))
}

#[derive(Deserialize)]
struct GoalProjectBody {
    actor: String,
    goal: String,
    /// Project entity id or exact name.
    project: String,
    /// false = unlink (default true = link). Many-to-many both ways.
    #[serde(default = "default_true")]
    link: bool,
}
async fn goal_link_project(
    State(state): State<AppState>,
    Json(body): Json<GoalProjectBody>,
) -> Result<Json<Value>, ApiError> {
    if body.actor.trim().is_empty() {
        return Err(bad_request("actor is required on every write"));
    }
    let project_id = resolve_entity_arg(&state, &body.project)?;
    state
        .store
        .link_goal_project(&Actor::new(body.actor), &body.goal, &project_id, body.link)?;
    crate::bus::touch(&state, "operational.changed");
    Ok(Json(json!({"goal": body.goal, "project": project_id, "linked": body.link})))
}

#[derive(Deserialize)]
struct NotePrimaryBody {
    actor: String,
    /// The subject this note is the 1:1 description of (project entity
    /// id/name, or a goal name); null clears the note's primary role.
    subject: Option<String>,
}
async fn note_set_primary(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<NotePrimaryBody>,
) -> Result<Json<Value>, ApiError> {
    if body.actor.trim().is_empty() {
        return Err(bad_request("actor is required on every write"));
    }
    // resolve a project NAME to its entity id; a goal name stays as-is
    let subject = match body.subject.as_deref().filter(|s| !s.trim().is_empty()) {
        Some(s) if s.starts_with("en_") || s.starts_with("in_") => Some(s.to_string()),
        Some(s) => Some(
            state
                .store
                .entity_by_name(s)?
                .map(|e| e.id)
                .unwrap_or_else(|| s.to_string()),
        ),
        None => None,
    };
    state
        .store
        .set_primary_note(&Actor::new(body.actor), &id, subject.as_deref())?;
    crate::bus::touch(&state, "operational.changed");
    Ok(Json(json!({"note": id, "subject": subject})))
}

#[derive(Deserialize)]
struct ResolveParams {
    /// Comma-separated cited ids (nt_/en_/ep_/tk_/in_).
    ids: String,
}

/// Turn the RLM's symbolic citation ids into human-readable references
/// with the right click target. Vault-guarded (unresolvable dropped).
async fn resolve_citations(
    State(state): State<AppState>,
    Query(params): Query<ResolveParams>,
) -> Result<Json<Value>, ApiError> {
    let ids: Vec<String> = params
        .ids
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let refs = state.store.resolve_citations(&ids)?;
    Ok(Json(json!({"refs": refs})))
}

/// Tag census: quick tabs + the # typeahead.
async fn note_tags(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let tags: Vec<Value> = state
        .store
        .note_tags(40)?
        .into_iter()
        .map(|(tag, n)| json!({"tag": tag, "n": n}))
        .collect();
    Ok(Json(json!({"tags": tags})))
}

/// Origin census for the Privacy page.
async fn note_origins(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let origins: Vec<Value> = state
        .store
        .note_origins()?
        .into_iter()
        .map(|(origin, total, private)| {
            json!({"origin": origin, "total": total, "private": private})
        })
        .collect();
    Ok(Json(json!({"origins": origins})))
}

#[derive(Deserialize)]
struct NotesScopeBody {
    actor: String,
    origin: String,
    /// private = vault the whole origin; anything else unvaults (rules
    /// still outrank — rule-matched notes stay private).
    scope: String,
}

async fn notes_scope_by_origin(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
    Json(body): Json<NotesScopeBody>,
) -> Result<Json<Value>, ApiError> {
    if body.actor.trim().is_empty() {
        return Err(bad_request("actor is required on every write"));
    }
    let changed = state.store.set_notes_scope_by_origin(
        &write_actor(&auth, body.actor),
        &state.home,
        &body.origin,
        &body.scope,
    )?;
    crate::bus::touch(&state, "privacy.changed");
    Ok(Json(json!({"origin": body.origin, "scope": body.scope, "notes": changed})))
}

#[derive(Deserialize)]
struct NoteAddBody {
    actor: String,
    title: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default = "default_task_scope")]
    scope: String,
    linked_task: Option<String>,
    /// Importer fields: where the note came from + original timestamps.
    origin: Option<String>,
    created_at: Option<String>,
    updated_at: Option<String>,
    /// note | template (a template is a note the task system reads).
    kind: Option<String>,
    /// v29: make this the 1:1 primary/description note for a subject
    /// (project entity id/name or goal name).
    primary_for: Option<String>,
}

async fn note_add(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
    Json(body): Json<NoteAddBody>,
) -> Result<Json<Value>, ApiError> {
    if body.actor.trim().is_empty() {
        return Err(bad_request("actor is required on every write"));
    }
    let timestamps = match (&body.created_at, &body.updated_at) {
        (Some(c), Some(u)) => Some((c.as_str(), u.as_str())),
        _ => None,
    };
    let id = state.store.create_note_kind(
        &write_actor(&auth, body.actor.clone()),
        &state.home,
        &body.title,
        &body.body,
        &body.tags,
        &body.scope,
        body.linked_task.as_deref(),
        body.origin.as_deref().unwrap_or("hearthloom"),
        timestamps,
        body.kind.as_deref().unwrap_or("note"),
    )?;
    if let Some(task_id) = &body.linked_task {
        state.store.set_task_note(task_id, &id)?;
    }
    if let Some(subject) = body.primary_for.as_deref().filter(|s| !s.trim().is_empty()) {
        let subject = if subject.starts_with("en_") || subject.starts_with("in_") {
            subject.to_string()
        } else {
            state.store.entity_by_name(subject)?.map(|e| e.id).unwrap_or_else(|| subject.to_string())
        };
        state
            .store
            .set_primary_note(&Actor::new(body.actor.clone()), &id, Some(&subject))?;
    }
    crate::bus::touch(&state, "operational.changed");
    Ok(Json(json!({"id": id})))
}

/// Journal-backed version history (v43 op_journal) — list is light
/// (title + body length per entry); the single-entry fetch carries the
/// full markdown body. Both 404 when the note is absent OR vaulted.
async fn note_versions(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    match state.store.note_versions(&id)? {
        Some(versions) => Ok(Json(json!({ "versions": versions }))),
        None => Err(ApiError(StatusCode::NOT_FOUND, "no such note".into())),
    }
}

async fn note_version(
    State(state): State<AppState>,
    Path((id, entry)): Path<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    match state.store.note_version(&id, &entry)? {
        Some(version) => Ok(Json(version)),
        None => Err(ApiError(StatusCode::NOT_FOUND, "no such version".into())),
    }
}

async fn note_get(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    match state.store.note(&state.home, &id)? {
        Some((note, body)) => {
            let mentions: Vec<Value> = state
                .store
                .note_mention_entities(&id)?
                .into_iter()
                .map(|(eid, name, ty)| json!({"id": eid, "name": name, "type": ty}))
                .collect();
            let link_json = |rows: Vec<(String, String)>| -> Vec<Value> {
                rows.into_iter()
                    .map(|(nid, title)| json!({"id": nid, "title": title}))
                    .collect()
            };
            let links = link_json(state.store.note_links(&id)?);
            let backlinks = link_json(state.store.note_backlinks(&id)?);
            let resonant: Vec<Value> = state
                .store
                .note_resonant(&id, 5)?
                .into_iter()
                .map(|(nid, title, score)| {
                    json!({"id": nid, "title": title, "score": score})
                })
                .collect();
            Ok(Json(json!({
                "note": note,
                "body": body,
                "mentions": mentions,
                "links": links,
                "backlinks": backlinks,
                "resonant": resonant,
            })))
        }
        None => Err(ApiError(StatusCode::NOT_FOUND, format!("no note {id}"))),
    }
}

#[derive(Deserialize)]
struct NoteUpdateBody {
    actor: String,
    title: Option<String>,
    body: Option<String>,
    tags: Option<Vec<String>>,
    /// Per-note vaulting: scope=private hides it while the vault is locked.
    scope: Option<String>,
    /// Optimistic lock: the version this edit was based on.
    version: i64,
}

async fn note_update(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
    Path(id): Path<String>,
    Json(body): Json<NoteUpdateBody>,
) -> Result<Json<Value>, ApiError> {
    if body.actor.trim().is_empty() {
        return Err(bad_request("actor is required on every write"));
    }
    let version = state.store.update_note(
        &write_actor(&auth, body.actor),
        &state.home,
        &id,
        body.title.as_deref(),
        body.body.as_deref(),
        body.tags.as_deref(),
        body.scope.as_deref(),
        body.version,
    )?;
    crate::bus::touch(&state, "operational.changed");
    Ok(Json(json!({"id": id, "version": version})))
}

#[derive(Deserialize)]
struct NoteStatusBody {
    actor: String,
    /// active | archived | deleted (archived mints the snapshot episode)
    status: String,
}

async fn note_status(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
    Path(id): Path<String>,
    Json(body): Json<NoteStatusBody>,
) -> Result<Json<Value>, ApiError> {
    if body.actor.trim().is_empty() {
        return Err(bad_request("actor is required on every write"));
    }
    let episode_id =
        state
            .store
            .set_note_status(&write_actor(&auth, body.actor), &state.home, &id, &body.status)?;
    crate::bus::touch(&state, "operational.changed");
    Ok(Json(json!({"id": id, "status": body.status, "episode_id": episode_id})))
}

#[derive(Deserialize)]
struct MetricsParams {
    /// SQL LIKE pattern over metric keys, e.g. `strength/%/volume`.
    metric: Option<String>,
    #[serde(default = "default_metric_from")]
    from: String,
    #[serde(default = "default_metric_to")]
    to: String,
}
fn default_metric_from() -> String {
    "0000-01-01".into()
}
fn default_metric_to() -> String {
    "9999-12-31".into()
}

async fn metrics_query(
    State(state): State<AppState>,
    Query(params): Query<MetricsParams>,
) -> Result<Json<Value>, ApiError> {
    match params.metric {
        Some(metric) => {
            let series = state
                .store
                .metric_series(&metric, &params.from, &params.to, 5000)?;
            let rows: Vec<Value> = series
                .into_iter()
                .map(|(day, metric, value, unit)| json!([day, metric, value, unit]))
                .collect();
            Ok(Json(json!({"rows": rows})))
        }
        None => Ok(Json(json!({"metrics": state.store.metric_names()?}))),
    }
}

#[derive(Deserialize)]
struct RelationshipsParams {
    /// With an entity name: that entity's relationships + latest brief.
    /// Without: the tiered directory.
    entity: Option<String>,
    /// Directory narrowing: Person | Org (orgs get their own tiers —
    /// people's citation volumes crowd them out of shared caps).
    #[serde(rename = "type")]
    entity_type: Option<String>,
}

async fn relationships_get(
    State(state): State<AppState>,
    Query(params): Query<RelationshipsParams>,
) -> Result<Json<Value>, ApiError> {
    match params.entity.as_deref().filter(|e| !e.trim().is_empty()) {
        Some(name) => {
            let entity = resolve_entity_row(&state, name)?;
            let relationships = state.store.relationships_for(&entity.id)?;
            let brief = state.store.latest_brief(&entity.id)?;
            Ok(Json(json!({
                "entity": entity,
                "relationships": relationships,
                "brief": brief.map(|(id, body)| json!({"episode_id": id, "body": body})),
                "kinds": hearthloom_core::store::RELATIONSHIP_KINDS,
            })))
        }
        None => Ok(Json(json!({
            "directory": state.store.relationship_directory(40, params.entity_type.as_deref())?,
            "pending": state.store.pending_relationships()?,
            "kinds": hearthloom_core::store::RELATIONSHIP_KINDS,
        }))),
    }
}

#[derive(Deserialize)]
struct RelationshipAddBody {
    actor: String,
    entity: String,
    kind: String,
    note: Option<String>,
    /// Declaring a FORMER relationship ("was partner"): confirmed on
    /// arrival, then immediately ended — bi-temporal history recorded
    /// in one gesture.
    #[serde(default)]
    ended: bool,
}

/// Declared by the human — confirmed on arrival.
async fn relationship_add(
    State(state): State<AppState>,
    Json(body): Json<RelationshipAddBody>,
) -> Result<Json<Value>, ApiError> {
    if body.actor.trim().is_empty() {
        return Err(bad_request("actor is required on every write"));
    }
    let entity = resolve_entity_row(&state, &body.entity)?;
    let actor = Actor::new(body.actor);
    let id = state.store.add_relationship(
        &actor,
        &entity.id,
        &body.kind,
        "declared",
        &[],
        body.note.as_deref(),
        None,
    )?;
    if body.ended {
        state.store.resolve_relationship(&actor, &id, "end")?;
    }
    crate::bus::touch(&state, "operational.changed");
    Ok(Json(json!({"id": id})))
}

#[derive(Deserialize)]
struct RelationshipResolveBody {
    actor: String,
    /// confirm | reject | end
    action: String,
}

async fn relationship_resolve(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<RelationshipResolveBody>,
) -> Result<Json<Value>, ApiError> {
    if body.actor.trim().is_empty() {
        return Err(bad_request("actor is required on every write"));
    }
    state
        .store
        .resolve_relationship(&Actor::new(body.actor), &id, &body.action)?;
    crate::bus::touch(&state, "operational.changed");
    Ok(Json(json!({"id": id, "action": body.action})))
}

async fn exercises_recent(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    Ok(Json(json!({"exercises": state.store.recent_exercises()?})))
}

async fn regenerate_index(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let content = brain_index::generate(&state.store)?;
    let path = state.home.brain_index_path();
    std::fs::write(&path, &content).map_err(anyhow::Error::from)?;
    Ok(Json(json!({
        "path": path.display().to_string(),
        "bytes": content.len(),
        "lines": content.lines().count(),
    })))
}

/* ————— plugins (docs/plugins.md — the sidecar contract, 2026-07-20) —————
   Lifecycle + storage + UI + the peering leg. Admin surfaces (list/
   install/…) ride the normal session auth; plugin-bound keys are
   DEFAULT-DENIED down to plugins::plugin_route_allowed in
   auth_middleware, so a plugin key can never reach these. */

/// The bound plugin for this request's key, or 403.
fn bound_plugin(state: &AppState, auth: &AuthInfo) -> Result<String, ApiError> {
    auth.0
        .as_ref()
        .and_then(|c| crate::plugins::plugin_for_claims(state, c))
        .ok_or_else(|| {
            ApiError(
                StatusCode::FORBIDDEN,
                "this surface is for plugin keys (the daemon hands one to every installed plugin)".into(),
            )
        })
}

async fn plugins_list(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let rows = state.store.list_plugins()?;
    let list: Vec<Value> = rows
        .into_iter()
        .map(|row| {
            let running = row.enabled && crate::plugins::sidecar_running(&row.name);
            let data_dir = crate::plugins::plugin_dir(&state, &row.name).join("data");
            let runner = state
                .store
                .plugin_kv_get(&row.name, crate::plugins::RUNNER_KEY)
                .ok()
                .flatten()
                .and_then(|raw| serde_json::from_str::<Value>(&raw).ok());
            json!({
                "runner": runner,
                "name": row.name, "version": row.version,
                "manifest": row.manifest, "granted_scopes": row.granted_scopes,
                "sync": row.sync, "enabled": row.enabled,
                "installed_at": row.installed_at, "running": running,
                "data_dir": data_dir.display().to_string(),
            })
        })
        .collect();
    Ok(Json(json!({"plugins": list})))
}

#[derive(Deserialize)]
struct PluginPathParams {
    path: String,
}

/// Parse a package's manifest for the approve-scopes step — no side
/// effects.
async fn plugins_inspect(
    State(_state): State<AppState>,
    Query(params): Query<PluginPathParams>,
) -> Result<Json<Value>, ApiError> {
    let dir = std::path::PathBuf::from(params.path);
    let manifest = crate::scoped::spawn_blocking(move || crate::plugins::load_manifest(&dir))
        .await
        .map_err(anyhow::Error::from)?
        .map_err(|e| bad_request(format!("{e:#}")))?;
    Ok(Json(json!({
        "name": manifest.name, "version": manifest.version,
        "publisher": manifest.publisher, "description": manifest.description,
        "requested_scopes": manifest.scopes,
        "sync": manifest.sync.unwrap_or_else(|| "local-only".into()),
        "local_only": manifest.local_only,
        "has_sidecar": !manifest.launch.is_empty(),
        "has_ui": manifest.ui.is_some(),
        "publishes": manifest.publish.iter().map(|p| p.path.clone()).collect::<Vec<_>>(),
    })))
}

#[derive(Deserialize)]
struct PluginInstallBody {
    path: String,
    #[serde(default)]
    scopes: Option<Vec<String>>,
}

async fn plugins_install(
    State(state): State<AppState>,
    Json(body): Json<PluginInstallBody>,
) -> Result<Json<Value>, ApiError> {
    let dir = std::path::PathBuf::from(body.path);
    let st = state.clone();
    let (row, secret) =
        crate::scoped::spawn_blocking(move || crate::plugins::install_from_dir(&st, &dir, body.scopes))
            .await
            .map_err(anyhow::Error::from)?
            .map_err(|e| bad_request(format!("{e:#}")))?;
    crate::bus::touch(&state, "operational.changed");
    Ok(Json(json!({
        "installed": row.name, "version": row.version,
        "granted_scopes": row.granted_scopes,
        // shown once, like every key; the sidecar reads its key file
        "key": secret,
    })))
}

/// Stage an uploaded package zip; returns the staged dir + its parsed
/// manifest so the UI can run the same approve→install flow as a dir.
async fn plugins_upload(
    State(state): State<AppState>,
    body: axum::body::Bytes,
) -> Result<Json<Value>, ApiError> {
    if body.len() > 64 * 1024 * 1024 {
        return Err(bad_request("plugin packages cap at 64MB"));
    }
    let st = state.clone();
    let staged = crate::scoped::spawn_blocking(move || {
        let dir = crate::plugins::stage_zip(&st, &body)?;
        let manifest = crate::plugins::load_manifest(&dir)?;
        Ok::<_, anyhow::Error>((dir, manifest))
    })
    .await
    .map_err(anyhow::Error::from)?
    .map_err(|e| bad_request(format!("{e:#}")))?;
    let (dir, manifest) = staged;
    Ok(Json(json!({
        "path": dir.display().to_string(),
        "name": manifest.name, "version": manifest.version,
        "requested_scopes": manifest.scopes,
        "description": manifest.description,
        "publishes": manifest.publish.iter().map(|p| p.path.clone()).collect::<Vec<_>>(),
    })))
}

#[derive(Deserialize)]
struct PluginToggleBody {
    enabled: bool,
}

async fn plugins_toggle(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<PluginToggleBody>,
) -> Result<Json<Value>, ApiError> {
    let st = state.clone();
    let n = name.clone();
    crate::scoped::spawn_blocking(move || crate::plugins::set_enabled(&st, &n, body.enabled))
        .await
        .map_err(anyhow::Error::from)?
        .map_err(|e| bad_request(format!("{e:#}")))?;
    crate::bus::touch(&state, "operational.changed");
    Ok(Json(json!({"name": name, "enabled": body.enabled})))
}

async fn plugins_uninstall(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let st = state.clone();
    let n = name.clone();
    let data_dir = crate::scoped::spawn_blocking(move || crate::plugins::uninstall(&st, &n))
        .await
        .map_err(anyhow::Error::from)?
        .map_err(|e| bad_request(format!("{e:#}")))?;
    crate::bus::touch(&state, "operational.changed");
    Ok(Json(json!({
        "uninstalled": name,
        // the user's data is never silently deleted — tell them where it lives
        "data_dir_kept": data_dir.display().to_string(),
    })))
}

/// The chrome contributions of enabled plugins (nav / Visualize /
/// hamburger / Desk) — app.js fetches this once per shell load.
async fn plugins_ui(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    Ok(Json(crate::plugins::ui_contributions(&state)?))
}

/// Mint the short-lived token a plugin PAGE uses from inside its
/// sandboxed iframe (the parent hands it over via postMessage). Scopes
/// = exactly what the user granted the plugin; expires in 12h; revoked
/// on disable/uninstall. Plugin keys can't reach this route (denied
/// prefix) — only the signed-in dashboard mints.
async fn plugin_ui_token(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let row = state
        .store
        .plugin(&name)?
        .filter(|r| r.enabled)
        .ok_or_else(|| bad_request(format!("no enabled plugin {name:?}")))?;
    let expires = (chrono::Utc::now() + chrono::Duration::hours(12))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    let (_, secret) = state.store.create_api_key(
        &Actor::new("human:dashboard"),
        &format!("plugin-ui:{name}"),
        &row.granted_scopes,
        Some(&expires),
    )?;
    Ok(Json(json!({"token": secret, "expires_at": expires, "scopes": row.granted_scopes})))
}

/// Serve a plugin's static UI assets (pkg/ui/…) into its sandbox.
async fn plugin_ui_asset(
    State(state): State<AppState>,
    Path((name, rest)): Path<(String, String)>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let not_found = || (StatusCode::NOT_FOUND, "no such plugin asset").into_response();
    let Ok(rel) = crate::plugins::safe_rel_path(&rest) else {
        return not_found();
    };
    let enabled = matches!(state.store.plugin(&name), Ok(Some(row)) if row.enabled);
    if !enabled {
        return not_found();
    }
    let path = crate::plugins::plugin_dir(&state, &name).join("pkg/ui").join(rel);
    let Ok(bytes) = std::fs::read(&path) else {
        return not_found();
    };
    let mime = match path.extension().and_then(|e| e.to_str()).unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" => "text/javascript",
        "css" => "text/css",
        "json" => "application/json",
        "png" => "image/png",
        "svg" => "image/svg+xml",
        "woff2" => "font/woff2",
        _ => "application/octet-stream",
    };
    // Only the dashboard may frame an ember page. /plugins/ is exempt from
    // the browser rule (static), so without this any site could frame the
    // page, become its parent and answer the page's handshake itself.
    (
        [
            (axum::http::header::CONTENT_TYPE, mime),
            (axum::http::header::CACHE_CONTROL, "no-cache, must-revalidate"),
            (axum::http::header::CONTENT_SECURITY_POLICY, EMBER_PAGE_CSP),
        ],
        bytes,
    )
        .into_response()
}

/// The CSP on every file under /plugins/{name}/ui/: framing only, so an
/// ember's own scripts and styles keep working as they are.
const EMBER_PAGE_CSP: &str = "frame-ancestors 'self'";

/// Serve a public file an enabled ember declared under `publish` (the
/// address → a file in its own data store). Pre-auth by design and
/// minimal by design: GET only, ≤64KB, no listing, 404 for anything
/// not declared — the manifest is the whole allowlist.
async fn well_known_asset(
    State(state): State<AppState>,
    Path(rest): Path<String>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let not_found = || (StatusCode::NOT_FOUND, "not found").into_response();
    let path = format!("/.well-known/{rest}");
    if !crate::plugins::valid_publish_path(&path) {
        return not_found();
    }
    let st = state.clone();
    let found = tokio::task::spawn_blocking(move || {
        let (name, entry) = crate::plugins::publisher_of(&st, &path)?;
        let rel = crate::plugins::safe_rel_path(&entry.file).ok()?.to_string();
        let file = plugin_files_dir(&st, &name).join(rel);
        let meta = std::fs::metadata(&file).ok()?;
        if !meta.is_file() || meta.len() > crate::plugins::PUBLISH_MAX_BYTES {
            return None;
        }
        Some((std::fs::read(&file).ok()?, entry.content_type))
    })
    .await
    .ok()
    .flatten();
    let Some((bytes, content_type)) = found else {
        return not_found();
    };
    let mime = content_type
        .filter(|t| !t.is_empty() && t.len() < 100 && !t.contains(['\r', '\n']))
        .unwrap_or_else(|| "text/plain; charset=utf-8".into());
    (
        [
            (axum::http::header::CONTENT_TYPE, mime),
            (axum::http::header::CACHE_CONTROL, "public, max-age=300".to_string()),
            (axum::http::header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_string()),
        ],
        bytes,
    )
        .into_response()
}

/* —— plugin self-storage: the ONLY way a plugin persists anything ——
   (law #4 — the daemon writes; the plugin never opens files or the db) */

async fn plugin_kv_list(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
) -> Result<Json<Value>, ApiError> {
    let plugin = bound_plugin(&state, &auth)?;
    let rows = state.store.plugin_kv_list(&plugin)?;
    Ok(Json(json!({
        "kv": rows.into_iter()
            .map(|(k, v, ts)| json!({"key": k, "value": v, "updated_at": ts}))
            .collect::<Vec<_>>(),
    })))
}

async fn plugin_kv_get(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
    Path(key): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let plugin = bound_plugin(&state, &auth)?;
    match state.store.plugin_kv_get(&plugin, &key)? {
        Some(value) => Ok(Json(json!({"key": key, "value": value}))),
        None => Err(ApiError(StatusCode::NOT_FOUND, format!("no value at {key:?}"))),
    }
}

async fn plugin_kv_put(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
    Path(key): Path<String>,
    body: String,
) -> Result<Json<Value>, ApiError> {
    let plugin = bound_plugin(&state, &auth)?;
    if key.starts_with("__") {
        return Err(bad_request("keys starting with __ are reserved for the daemon (the runner lease lives there)"));
    }
    if body.len() > 256 * 1024 {
        return Err(bad_request("kv values cap at 256KB — larger state goes in /files"));
    }
    state.store.plugin_kv_set(&plugin, &key, &body)?;
    Ok(Json(json!({"key": key, "bytes": body.len()})))
}

async fn plugin_kv_delete(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
    Path(key): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let plugin = bound_plugin(&state, &auth)?;
    if key.starts_with("__") {
        return Err(bad_request("keys starting with __ are reserved for the daemon"));
    }
    let deleted = state.store.plugin_kv_delete(&plugin, &key)?;
    Ok(Json(json!({"key": key, "deleted": deleted})))
}

/* —— ember secrets (sealed; see plugins.rs) —— */

/// Like bound_plugin, but only the SIDECAR's own key — never the 12h
/// token a UI page holds. Reading a secret back is the binary's right.
fn bound_sidecar(state: &AppState, auth: &AuthInfo) -> Result<String, ApiError> {
    let plugin = bound_plugin(state, auth)?;
    let is_ui_token = auth.0.as_ref().is_some_and(|c| c.name.starts_with("plugin-ui:"));
    if is_ui_token {
        return Err(ApiError(
            StatusCode::FORBIDDEN,
            "an ember's page can set its secrets but not read them back".into(),
        ));
    }
    Ok(plugin)
}

async fn plugin_secret_list(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
) -> Result<Json<Value>, ApiError> {
    let plugin = bound_plugin(&state, &auth)?;
    let st = state.clone();
    let list = tokio::task::spawn_blocking(move || crate::plugins::secret_list(&st, &plugin))
        .await
        .map_err(anyhow::Error::from)?;
    Ok(Json(json!({
        "secrets": list,
        // master-key | node-key — the latter is sealed with a key on the
        // same disk (no master key provisioned); pages should say so
        "sealing": hearthloom_core::secrets::node_seal_source(),
    })))
}

async fn plugin_secret_get(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
    Path(key): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let plugin = bound_sidecar(&state, &auth)?;
    let st = state.clone();
    let k = key.clone();
    let value = tokio::task::spawn_blocking(move || crate::plugins::secret_get(&st, &plugin, &k))
        .await
        .map_err(anyhow::Error::from)??;
    match value {
        Some(bytes) => Ok(Json(json!({
            "key": key,
            "value": String::from_utf8(bytes).map_err(|_| anyhow::anyhow!("secret isn't text"))?,
        }))),
        None => Err(ApiError(StatusCode::NOT_FOUND, format!("no secret {key:?}"))),
    }
}

async fn plugin_secret_put(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
    Path(key): Path<String>,
    body: String,
) -> Result<Json<Value>, ApiError> {
    let plugin = bound_plugin(&state, &auth)?;
    if key.starts_with("__") {
        return Err(bad_request("secret names starting with __ are written by the daemon"));
    }
    let st = state.clone();
    let k = key.clone();
    tokio::task::spawn_blocking(move || crate::plugins::secret_put(&st, &plugin, &k, body.as_bytes()))
        .await
        .map_err(anyhow::Error::from)?
        .map_err(|e| bad_request(format!("{e:#}")))?;
    Ok(Json(json!({"key": key, "set": true})))
}

async fn plugin_secret_delete(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
    Path(key): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let plugin = bound_plugin(&state, &auth)?;
    let deleted = crate::plugins::secret_delete(&state, &plugin, &key);
    Ok(Json(json!({"key": key, "deleted": deleted})))
}

/* —— the OAuth callback relay (see plugins.rs) —— */

#[derive(Deserialize)]
struct OAuthPendingBody {
    state: String,
    authorize_url: String,
    #[serde(default)]
    ttl_secs: Option<u64>,
}

async fn plugin_oauth_pending(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
    Json(body): Json<OAuthPendingBody>,
) -> Result<Json<Value>, ApiError> {
    let plugin = bound_plugin(&state, &auth)?;
    crate::plugins::oauth_register(&plugin, &body.state, &body.authorize_url, body.ttl_secs.unwrap_or(900))
        .map_err(|e| bad_request(format!("{e:#}")))?;
    Ok(Json(json!({
        "pending": true,
        // where the provider must send the user back
        "callback_path": format!("/plugins/{plugin}/oauth/callback"),
    })))
}

/// The sidecar takes the code ONCE; it's deleted as it's read.
async fn plugin_oauth_take(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
) -> Result<Json<Value>, ApiError> {
    let plugin = bound_sidecar(&state, &auth)?;
    let st = state.clone();
    let taken = tokio::task::spawn_blocking(move || -> anyhow::Result<Option<(String, String)>> {
        let Some(code) = crate::plugins::secret_get(&st, &plugin, "__oauth_code")? else {
            return Ok(None);
        };
        let st_val = crate::plugins::secret_get(&st, &plugin, "__oauth_state")?.unwrap_or_default();
        crate::plugins::secret_delete(&st, &plugin, "__oauth_code");
        crate::plugins::secret_delete(&st, &plugin, "__oauth_state");
        Ok(Some((String::from_utf8(code)?, String::from_utf8(st_val)?)))
    })
    .await
    .map_err(anyhow::Error::from)??;
    match taken {
        Some((code, st)) => Ok(Json(json!({"code": code, "state": st}))),
        None => Err(ApiError(StatusCode::NOT_FOUND, "no sign-in code waiting".into())),
    }
}

/// Session-side: the authorize URL the dashboard may open for an ember
/// (plugin keys can't reach /v1/plugins/<name>/… — default-deny).
async fn plugin_oauth_get(Path(name): Path<String>) -> Result<Json<Value>, ApiError> {
    match crate::plugins::oauth_authorize_url(&name) {
        Some(url) => Ok(Json(json!({"authorize_url": url}))),
        None => Err(ApiError(StatusCode::NOT_FOUND, "no sign-in pending".into())),
    }
}

#[derive(Deserialize)]
struct OAuthCallbackParams {
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    error_description: Option<String>,
}

fn oauth_page(status: StatusCode, title: &str, body: &str) -> axum::response::Response {
    use axum::response::IntoResponse;
    let html = format!(
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1"><title>{title}</title>
<style>:root{{color-scheme:light dark;--bg:#faf7f2;--ink:#2a2420;--faint:#6b625b}}
@media (prefers-color-scheme:dark){{:root{{--bg:#16130f;--ink:#efe7dc;--faint:#a39889}}}}
body{{margin:0;background:var(--bg);color:var(--ink);font:16px/1.5 ui-serif,Georgia,serif;
display:grid;place-items:center;min-height:100vh;padding:16px;box-sizing:border-box}}
main{{max-width:30rem}}h1{{font-size:1.4rem;margin:0 0 .5rem}}p{{color:var(--faint);margin:0}}</style>
</head><body><main><h1>{title}</h1><p>{body}</p></main></body></html>"#,
        title = html_escape(title),
        body = html_escape(body),
    );
    (
        status,
        [
            (axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (axum::http::header::CACHE_CONTROL, "no-store"),
            (axum::http::header::REFERRER_POLICY, "no-referrer"),
        ],
        html,
    )
        .into_response()
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// The provider sends the user back here. Pre-auth (it's a browser
/// redirect from the provider), so the state check IS the gate: a code
/// lands only when it answers the sign-in this ember registered.
async fn plugin_oauth_callback(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Query(params): Query<OAuthCallbackParams>,
) -> axum::response::Response {
    let enabled = matches!(state.store.plugin(&name), Ok(Some(row)) if row.enabled);
    if !enabled {
        return oauth_page(StatusCode::NOT_FOUND, "Nothing to sign in to", "That ember isn't installed here.");
    }
    if let Some(err) = params.error {
        let why = params.error_description.unwrap_or(err);
        return oauth_page(
            StatusCode::BAD_REQUEST,
            "Sign-in didn't finish",
            &format!("The provider said: {why}. Go back to Hearthloom and try again."),
        );
    }
    let (Some(code), Some(st)) = (params.code, params.state) else {
        return oauth_page(StatusCode::BAD_REQUEST, "Sign-in didn't finish", "The provider sent no code back.");
    };
    if code.len() > 4096 || !crate::plugins::oauth_consume(&name, &st) {
        return oauth_page(
            StatusCode::BAD_REQUEST,
            "This sign-in link has expired",
            "Start signing in again from the ember's page in Hearthloom (on this node's dashboard).",
        );
    }
    let st2 = state.clone();
    let n = name.clone();
    let saved = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        crate::plugins::secret_put(&st2, &n, "__oauth_code", code.as_bytes())?;
        crate::plugins::secret_put(&st2, &n, "__oauth_state", st.as_bytes())?;
        Ok(())
    })
    .await;
    if !matches!(saved, Ok(Ok(()))) {
        return oauth_page(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Sign-in didn't finish",
            "Hearthloom couldn't store the sign-in. Try again.",
        );
    }
    crate::bus::touch(&state, "plugin.oauth");
    oauth_page(
        StatusCode::OK,
        "Signed in",
        &format!("{name} can finish connecting now. You can close this tab and go back to Hearthloom."),
    )
}

fn plugin_files_dir(state: &AppState, plugin: &str) -> std::path::PathBuf {
    crate::plugins::plugin_dir(state, plugin).join("data")
}

async fn plugin_file_list(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
) -> Result<Json<Value>, ApiError> {
    let plugin = bound_plugin(&state, &auth)?;
    let dir = plugin_files_dir(&state, &plugin);
    let files = crate::scoped::spawn_blocking(move || list_files_rec(&dir, &dir))
        .await
        .map_err(anyhow::Error::from)??;
    Ok(Json(json!({"files": files})))
}

/// Relative listing with size+mtime — shared by the self surface and
/// the peering leg's fingerprint exchange.
fn list_files_rec(root: &std::path::Path, dir: &std::path::Path) -> anyhow::Result<Vec<Value>> {
    let mut out = Vec::new();
    if !dir.exists() {
        return Ok(out);
    }
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            out.extend(list_files_rec(root, &path)?);
        } else if let Ok(rel) = path.strip_prefix(root) {
            let meta = entry.metadata()?;
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            out.push(json!({
                "path": rel.to_string_lossy(),
                "size": meta.len(),
                "mtime": mtime,
            }));
        }
    }
    Ok(out)
}

async fn plugin_file_get(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
    Path(rest): Path<String>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let plugin = match bound_plugin(&state, &auth) {
        Ok(p) => p,
        Err(e) => return e.into_response(),
    };
    let Ok(rel) = crate::plugins::safe_rel_path(&rest) else {
        return (StatusCode::BAD_REQUEST, "invalid path").into_response();
    };
    let path = plugin_files_dir(&state, &plugin).join(rel);
    match crate::scoped::spawn_blocking(move || std::fs::read(&path)).await {
        Ok(Ok(bytes)) => (
            [(axum::http::header::CONTENT_TYPE, "application/octet-stream")],
            bytes,
        )
            .into_response(),
        _ => (StatusCode::NOT_FOUND, "no such file").into_response(),
    }
}

async fn plugin_file_put(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
    Path(rest): Path<String>,
    body: axum::body::Bytes,
) -> Result<Json<Value>, ApiError> {
    let plugin = bound_plugin(&state, &auth)?;
    let rel = crate::plugins::safe_rel_path(&rest)
        .map_err(|e| bad_request(format!("{e:#}")))?
        .to_string();
    if body.len() > 32 * 1024 * 1024 {
        return Err(bad_request("plugin files cap at 32MB"));
    }
    let path = plugin_files_dir(&state, &plugin).join(&rel);
    let len = body.len();
    crate::scoped::spawn_blocking(move || -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, &body)?;
        Ok(())
    })
    .await
    .map_err(anyhow::Error::from)??;
    Ok(Json(json!({"path": rel, "bytes": len})))
}

async fn plugin_file_delete(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
    Path(rest): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let plugin = bound_plugin(&state, &auth)?;
    let rel = crate::plugins::safe_rel_path(&rest).map_err(|e| bad_request(format!("{e:#}")))?;
    let path = plugin_files_dir(&state, &plugin).join(rel);
    let deleted = std::fs::remove_file(&path).is_ok();
    Ok(Json(json!({"path": rel, "deleted": deleted})))
}

/* —— the peering leg (item: plugins participate in replication) ——
   KV: push+pull with LWW by updated_at (apply_plugin_kv refuses
   plugins this node doesn't mirror). Files: fingerprint listing +
   per-file fetch, newer-mtime-wins. Wire discipline: additive JSON,
   404 → the peer predates plugins → skip. */

#[derive(Deserialize)]
struct SyncPluginsParams {
    #[serde(default)]
    since: String,
}

async fn sync_plugins_get(
    State(state): State<AppState>,
    Query(params): Query<SyncPluginsParams>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
) -> Result<Json<Value>, ApiError> {
    require_scope(&auth, "peer")?;
    let kv = state.store.plugin_kv_since(&params.since, 500)?;
    let next = state.store.plugin_kv_cursor(&params.since, 500)?;
    // fingerprints for every mirror plugin's data dir (small trees)
    let mirror: Vec<String> = state
        .store
        .list_plugins()?
        .into_iter()
        .filter(|r| r.sync == "mirror")
        .map(|r| r.name)
        .collect();
    let st = state.clone();
    let files = crate::scoped::spawn_blocking(move || -> anyhow::Result<Vec<Value>> {
        let mut out = Vec::new();
        for name in mirror {
            let dir = plugin_files_dir(&st, &name);
            for f in list_files_rec(&dir, &dir)? {
                let mut f = f;
                f["plugin"] = json!(name);
                out.push(f);
            }
        }
        Ok(out)
    })
    .await
    .map_err(anyhow::Error::from)??;
    Ok(Json(json!({
        "kv": kv.into_iter()
            .map(|(p, k, v, ts)| json!([p, k, v, ts]))
            .collect::<Vec<_>>(),
        "next": next,
        "files": files,
    })))
}

async fn sync_plugins_post(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
    Json(batch): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    require_scope(&auth, "peer")?;
    let rows: Vec<(String, String, String, String)> = batch
        .get("kv")
        .and_then(|v| v.as_array())
        .map(|list| {
            list.iter()
                .filter_map(|row| {
                    let a = row.as_array()?;
                    Some((
                        a.first()?.as_str()?.to_string(),
                        a.get(1)?.as_str()?.to_string(),
                        a.get(2)?.as_str()?.to_string(),
                        a.get(3)?.as_str()?.to_string(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default();
    let applied = state.store.apply_plugin_kv(&rows)?;
    Ok(Json(json!({"applied": applied})))
}

#[derive(Deserialize)]
struct SyncPluginFileParams {
    plugin: String,
    path: String,
}

async fn sync_plugin_file_get(
    State(state): State<AppState>,
    Query(params): Query<SyncPluginFileParams>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if let Err(e) = require_scope(&auth, "peer") {
        return e.into_response();
    }
    // only mirror plugins' data leaves the node
    let mirror = matches!(state.store.plugin(&params.plugin),
        Ok(Some(row)) if row.sync == "mirror");
    let Ok(rel) = crate::plugins::safe_rel_path(&params.path) else {
        return (StatusCode::BAD_REQUEST, "invalid path").into_response();
    };
    if !mirror {
        return (StatusCode::NOT_FOUND, "no such plugin file").into_response();
    }
    let path = plugin_files_dir(&state, &params.plugin).join(rel);
    match crate::scoped::spawn_blocking(move || std::fs::read(&path)).await {
        Ok(Ok(bytes)) => (
            [(axum::http::header::CONTENT_TYPE, "application/octet-stream")],
            bytes,
        )
            .into_response(),
        _ => (StatusCode::NOT_FOUND, "no such plugin file").into_response(),
    }
}

#[derive(Deserialize)]
struct RegistryIndexParams {
    #[serde(default)]
    q: String,
    #[serde(default)]
    registry: Option<String>,
}

/// The gallery's data: the registry index, proxied (context=cloud is
/// appended automatically on cloud nodes — a hosted instance can only
/// see what it may install).
async fn plugins_registry_index(
    Query(params): Query<RegistryIndexParams>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(crate::registry_client::index(params.registry, &params.q).await?))
}

#[derive(Deserialize)]
struct RegistryStageBody {
    name: String,
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    registry: Option<String>,
}

/// Download + verify (sha256 + Ed25519 + counter-sign; cloud policy
/// enforced) and stage — the caller shows the scopes and finishes with
/// the normal /v1/plugins/install. The grant stays a human act.
async fn plugins_registry_stage(
    State(state): State<AppState>,
    Json(body): Json<RegistryStageBody>,
) -> Result<Json<Value>, ApiError> {
    let staged =
        crate::registry_client::stage(&state, body.registry, &body.name, body.version).await
            .map_err(|e| bad_request(format!("{e:#}")))?;
    Ok(Json(json!({
        "path": staged.path.display().to_string(),
        "name": staged.name,
        "version": staged.version,
        "scopes": staged.scopes,
        "publisher": staged.publisher,
        "reviewed": staged.reviewed,
        "placement": staged.placement,
        "requires_host": staged.requires_host,
        "publishes": staged.publishes,
    })))
}

/// What's installed HERE, for the counterpart's Integrations page
/// (peer scope; names + placement, no secrets).
async fn sync_plugins_installed(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthInfo>,
) -> Result<Json<Value>, ApiError> {
    require_scope(&auth, "peer")?;
    let list: Vec<Value> = state
        .store
        .list_plugins()?
        .into_iter()
        .map(|row| {
            let placement = serde_json::from_value::<crate::plugins::Manifest>(row.manifest.clone())
                .map(|m| m.effective_placement().to_string())
                .unwrap_or_else(|_| "any".into());
            json!({
                "name": row.name,
                "version": row.version,
                "enabled": row.enabled,
                "running": row.enabled && crate::plugins::sidecar_running(&row.name),
                "sync": row.sync,
                "placement": placement,
                "description": row.manifest.get("description").cloned().unwrap_or(Value::Null),
            })
        })
        .collect();
    Ok(Json(json!({"plugins": list, "version": env!("CARGO_PKG_VERSION")})))
}

/// The counterpart's ember list, proxied with this node's peer key
/// (60s cache) — Integrations renders "on your other node" from this.
async fn plugins_peer(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    use std::sync::{Mutex, OnceLock};
    use std::time::{Duration, Instant};
    static CACHE: OnceLock<Mutex<Option<(Instant, Value)>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(None));
    if let Some((at, value)) = cache.lock().unwrap().as_ref() {
        if at.elapsed() < Duration::from_secs(60) {
            return Ok(Json(value.clone()));
        }
    }
    let Some((_, url)) = state.store.peer()? else {
        return Ok(Json(json!({"paired": false, "plugins": []})));
    };
    let Some(key) = crate::sync::peer_key() else {
        return Ok(Json(json!({"paired": false, "plugins": []})));
    };
    let value = match reqwest::Client::new()
        .get(format!("{url}/v1/sync/plugins/installed"))
        .bearer_auth(&key)
        .timeout(std::time::Duration::from_secs(8))
        .send()
        .await
    {
        Ok(res) if res.status().is_success() => {
            let mut v: Value = res.json().await.unwrap_or_else(|_| json!({}));
            v["paired"] = json!(true);
            v["peer_url"] = json!(url);
            v
        }
        // older peer or offline: paired but unknowable — say so honestly
        _ => json!({"paired": true, "peer_url": url, "plugins": Value::Null}),
    };
    *cache.lock().unwrap() = Some((Instant::now(), value.clone()));
    Ok(Json(value))
}

/// Read an ember's current config (the same `config` KV the ember
/// reads) — session surface for the card's settings form.
async fn plugin_config_get(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let raw = state.store.plugin_kv_get(&name, "config")?;
    let values: Value = raw
        .and_then(|r| serde_json::from_str(&r).ok())
        .unwrap_or_else(|| json!({}));
    Ok(Json(json!({"values": values})))
}

#[derive(Deserialize)]
struct PluginConfigBody {
    values: Value,
}

/// Write config values into the ember's KV, keyed by its manifest's
/// declared config schema — unknown keys are refused so the form can't
/// be repurposed as arbitrary KV access.
async fn plugin_config_set(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<PluginConfigBody>,
) -> Result<Json<Value>, ApiError> {
    let row = state
        .store
        .plugin(&name)?
        .ok_or_else(|| bad_request(format!("no installed ember {name:?}")))?;
    let manifest: crate::plugins::Manifest =
        serde_json::from_value(row.manifest).map_err(anyhow::Error::from)?;
    let schema = manifest
        .config
        .as_ref()
        .and_then(|c| c.as_array())
        .ok_or_else(|| bad_request(format!("{name} declares no configuration")))?;
    let allowed: std::collections::HashSet<&str> = schema
        .iter()
        .filter_map(|i| i.get("key").and_then(|k| k.as_str()))
        .collect();
    let incoming = body
        .values
        .as_object()
        .ok_or_else(|| bad_request("values must be an object"))?;
    // merge over what's there (an ember may keep extra state in config)
    let mut current: serde_json::Map<String, Value> = state
        .store
        .plugin_kv_get(&name, "config")?
        .and_then(|r| serde_json::from_str(&r).ok())
        .unwrap_or_default();
    for (k, v) in incoming {
        if !allowed.contains(k.as_str()) {
            return Err(bad_request(format!("{k:?} isn't in {name}'s declared config")));
        }
        current.insert(k.clone(), v.clone());
    }
    state
        .store
        .plugin_kv_set(&name, "config", &Value::Object(current.clone()).to_string())?;
    Ok(Json(json!({"saved": true, "values": current})))
}

/// Host version status: current, latest (from the release channel),
/// and whether this node can self-update.
async fn version_get() -> Json<Value> {
    Json(crate::registry_client::version_info().await)
}

/// Self-update the host binary: download the platform build, verify its
/// sha256 (and its Ed25519 release signature when a signing key is pinned
/// with HEARTHLOOM_UPDATE_SIGNING_KEY), replace the running executable,
/// and restart via launchd. Refused on hosted nodes (their image is
/// managed by the hosting provider). A release without a hash is refused.
async fn upgrade_post(State(_state): State<AppState>) -> Result<Json<Value>, ApiError> {
    if crate::monitor::node_kind() == "cloud" {
        return Err(bad_request(
            "hosted instances upgrade through their hosting provider, not from here",
        ));
    }
    let info = crate::registry_client::version_info().await;
    if !info["update_available"].as_bool().unwrap_or(false) {
        return Err(bad_request("already on the latest version"));
    }
    if !info["can_self_update"].as_bool().unwrap_or(false) {
        return Err(bad_request(
            "no official build is available for this platform to self-update",
        ));
    }
    let dl = &info["download"];
    let url = dl
        .get("url")
        .and_then(|v| v.as_str())
        .ok_or_else(|| bad_request("release has no download url"))?
        .to_string();
    let want_sha = dl
        .get("sha256")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let to = info["latest"].as_str().unwrap_or("?").to_string();
    if want_sha.trim().is_empty() {
        return Err(bad_request(
            "the release publishes no sha256 for this build — refusing to install a binary that can't be checked",
        ));
    }
    if let Some(key) = crate::registry_client::update_signing_key() {
        let sig = dl.get("signature").and_then(|v| v.as_str()).unwrap_or("");
        let target = info["target"].as_str().unwrap_or("");
        crate::registry_client::verify_ed25519(
            &key,
            &crate::registry_client::host_release_payload(&to, target, &want_sha),
            sig,
        )
        .map_err(|e| bad_request(format!("release signature check failed — refusing: {e:#}")))?;
    }

    // download + verify
    let bytes = reqwest::Client::new()
        .get(&url)
        .timeout(std::time::Duration::from_secs(300))
        .send()
        .await
        .map_err(anyhow::Error::from)?
        .error_for_status()
        .map_err(anyhow::Error::from)?
        .bytes()
        .await
        .map_err(anyhow::Error::from)?;
    let got_sha = {
        use sha2::Digest;
        format!("{:x}", sha2::Sha256::digest(&bytes))
    };
    if got_sha != want_sha {
        return Err(bad_request(format!(
            "downloaded binary hash mismatch (got {got_sha}, expected {want_sha}) — refusing"
        )));
    }

    // atomically replace the running executable (Unix keeps our inode)
    let exe = std::env::current_exe().map_err(anyhow::Error::from)?;
    let tmp = exe.with_extension("new");
    crate::scoped::spawn_blocking({
        let (tmp, exe) = (tmp.clone(), exe.clone());
        move || -> anyhow::Result<()> {
            std::fs::write(&tmp, &bytes)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
            }
            std::fs::rename(&tmp, &exe)?;
            Ok(())
        }
    })
    .await
    .map_err(anyhow::Error::from)?
    .map_err(|e| bad_request(format!("{e:#}")))?;

    // restart from the new binary shortly, after this response flushes
    tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_millis(700)).await;
        if let Ok(out) = std::process::Command::new("id").arg("-u").output() {
            let uid = String::from_utf8_lossy(&out.stdout).trim().to_string();
            let _ = std::process::Command::new("launchctl")
                .args(["kickstart", "-k", &format!("gui/{uid}/com.hearthloom.daemon")])
                .output();
        }
    });
    Ok(Json(json!({"upgrading": true, "to": to, "restarting": true})))
}
