//! Visit consolidation (v51, Part A of the iOS device-feed wave) — the
//! pass that collapses cross-source location redundancy. Observations
//! from EVERY geolocated source (photo geotags, CLVisit dwells, calendar
//! venues, scans, later timeline imports) land in the unified
//! `location_observation` table; this pass clusters each closed day
//! spatially (~100 m, scaled by reported accuracy) and temporally
//! (gap ≤ 20 min) into canonical `visit` episodes — so "photo + GPS +
//! calendar all say I was at X" is ONE visit whose body cites all three
//! sources, never three episodes.
//!
//! Deduping happens HERE, at cluster time, never by trusting client
//! keys: each source's idempotency is its own (UNIQUE(source,
//! source_ref)); collapsing across sources is the server's job.
//!
//! Minting follows the photo_day precedent exactly: a `visit_day`
//! ledger records what each day was clustered from (observation count +
//! keys hash); the day re-mints under the next revision ref
//! (`visit:<day>:<idx>#r<n>`, THE SUPERSESSION PATTERN) only when its
//! observation union strictly grows, and both peers converge because
//! observations sync (/v1/sync/locations) and the deterministic episode
//! id is a hash of source+ref. When a re-cluster yields FEWER visits
//! (two clusters merged), the leftovers are absorbed into the day's
//! first new episode via supersede_stale_visits — again convergent,
//! because both nodes re-cluster the same synced set.
//!
//! Whether a calendar event ALONE makes a visit is the owner's setting
//! (`calendar_alone`, off by default, Integrations → Pair a device). It
//! syncs, because both nodes must cluster alike; switching it changes the
//! days clustered from then on, never days already done. Visits the old
//! rule minted from a calendar entry alone are withdrawn once, on every
//! node (`withdraw_calendar_only`).
//!
//! Pure store+CPU — the caller spawn_blockings the whole pass (THE PASS
//! CONTRACT), and each day's mint batches its lock holds.

use hearthloom_core::store::{Actor, Store, StoredObservation};
use hearthloom_core::time::{EventTime, TimeUncertainty};
use hearthloom_core::{one_line, EntityRef, EpisodeDraft, IngestBatch};
use serde_json::{json, Value};

use crate::AppState;

/// Spatial join radius (metres) — grown per-observation by reported
/// accuracy. ~100 m ≈ "the same venue" at city scale.
const BASE_RADIUS_M: f64 = 100.0;
/// Temporal join gap: an observation this soon after a cluster's last
/// sighting continues the same stay.
const MAX_GAP_MIN: i64 = 20;
/// Observation sources that DECLARE a place rather than sense one: a
/// calendar venue is where you meant to be. Such an observation may name
/// or lengthen a stay, but a cluster of them alone is not a visit (owner
/// decision, 2026-10-24) — it needs a sensed one too: a CLVisit dwell, a
/// location sample, a photo geotag, a scan's capture point. Unless the
/// owner switches `calendar_alone` on (2026-10-25).
const DECLARED_SOURCES: &[&str] = &["calendar"];

/// The calendar setting: "on" lets a calendar event alone make a visit.
/// Absent = off. Its stamp decides between the nodes: the setting rides
/// the operational mirror as `visit_rule` in BOTH directions, last writer
/// wins (the llm_budget pattern), so either node's dashboard can change
/// it and both cluster by the same rule.
const KEY_CALENDAR_ALONE: &str = "visits.calendar_alone";
const KEY_CALENDAR_ALONE_STAMP: &str = "visits.calendar_alone.updated_at";
/// When the calendar-only cleanup began. It begins the first time either
/// node runs this pass with the setting off, and the start rides the same
/// envelope: once begun, it runs on both nodes whatever the setting says
/// by then, so they withdraw the same visits.
const KEY_CLEANUP: &str = "visits.calendar_cleanup";
/// The cleanup's episodes-rowid watermark — node-local, like visit_day.
const KEY_CLEANUP_ROWID: &str = "visits.calendar_cleanup.rowid";
/// Visits the cleanup reads per lock hold.
const CLEANUP_BATCH: usize = 200;

/// Does a calendar event alone make a visit on this node?
pub(crate) fn calendar_alone(store: &Store) -> bool {
    matches!(store.consolidation_state(KEY_CALENDAR_ALONE), Ok(Some(v)) if v == "on")
}

/// Set the calendar setting here, stamped now; the next sync carries it.
pub(crate) fn set_calendar_alone(store: &Store, on: bool) -> anyhow::Result<()> {
    store.set_consolidation_state(KEY_CALENDAR_ALONE, if on { "on" } else { "off" })?;
    store.set_consolidation_state(KEY_CALENDAR_ALONE_STAMP, &hearthloom_core::time::now_utc())
}

/// The `visit_rule` envelope for the operational mirror: the setting and
/// its stamp, and when the cleanup began. None while neither exists.
pub(crate) fn rule_envelope(store: &Store) -> Option<Value> {
    let state = |key| store.consolidation_state(key).ok().flatten();
    let (stamp, cleanup) = (state(KEY_CALENDAR_ALONE_STAMP), state(KEY_CLEANUP));
    if stamp.is_none() && cleanup.is_none() {
        return None;
    }
    Some(json!({"calendar_alone": calendar_alone(store), "updated_at": stamp, "cleanup": cleanup}))
}

/// Take the other node's setting when it is the newer word — a tie goes
/// to off, so two changes in the same second still end alike — and its
/// cleanup start when ours has not begun. Returns whether anything changed.
pub(crate) fn adopt_rule(store: &Store, envelope: &Value) -> anyhow::Result<bool> {
    let mut changed = false;
    if let (Some(on), Some(stamp)) = (
        envelope.get("calendar_alone").and_then(Value::as_bool),
        envelope.get("updated_at").and_then(Value::as_str),
    ) {
        let ours = store.consolidation_state(KEY_CALENDAR_ALONE_STAMP)?.unwrap_or_default();
        if stamp > ours.as_str() || (stamp == ours && !on && calendar_alone(store)) {
            store.set_consolidation_state(KEY_CALENDAR_ALONE, if on { "on" } else { "off" })?;
            store.set_consolidation_state(KEY_CALENDAR_ALONE_STAMP, stamp)?;
            changed = true;
        }
    }
    if let Some(begun) = envelope.get("cleanup").and_then(Value::as_str).filter(|s| !s.is_empty()) {
        if store.consolidation_state(KEY_CLEANUP)?.is_none() {
            store.set_consolidation_state(KEY_CLEANUP, begun)?;
            changed = true;
        }
    }
    Ok(changed)
}

pub(crate) struct Cluster {
    pub members: Vec<StoredObservation>,
    pub lat: f64,
    pub lon: f64,
    /// Unix seconds of first arrival / last sighting (t_end when the
    /// source reported a departure, else t_start).
    pub start_unix: i64,
    pub end_unix: i64,
}

fn ts_unix(s: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(s).map(|d| d.timestamp()).ok()
}

/// Equirectangular approximation — exact enough at venue scale.
fn dist_m(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let mlat = (lat1 + lat2) / 2.0;
    let kx = mlat.to_radians().cos() * 111_320.0;
    let ky = 110_574.0;
    (((lat1 - lat2) * ky).powi(2) + ((lon1 - lon2) * kx).powi(2)).sqrt()
}

impl Cluster {
    fn new(o: StoredObservation, start: i64, end: i64) -> Self {
        Cluster { lat: o.lat, lon: o.lon, members: vec![o], start_unix: start, end_unix: end }
    }

    fn absorb(&mut self, o: StoredObservation, start: i64, end: i64) {
        // running centroid
        let n = self.members.len() as f64;
        self.lat = (self.lat * n + o.lat) / (n + 1.0);
        self.lon = (self.lon * n + o.lon) / (n + 1.0);
        self.start_unix = self.start_unix.min(start);
        self.end_unix = self.end_unix.max(end);
        self.members.push(o);
    }

    /// The most common non-null place label among members — a photo's
    /// reverse-geocoded place or a calendar venue name owns the Place.
    /// One line always (`one_line`): a calendar venue arrives exactly as
    /// the invite's sender typed it, and the visit body's lines are read
    /// by shape.
    pub fn place(&self) -> Option<String> {
        let mut counts: std::collections::HashMap<String, usize> = Default::default();
        for m in &self.members {
            if let Some(p) = m.place.as_deref().map(one_line).filter(|p| !p.is_empty()) {
                *counts.entry(p).or_default() += 1;
            }
        }
        // ties go to the first label in sort order, so every node names
        // the same cluster the same way
        counts
            .into_iter()
            .max_by(|(a, n), (b, m)| n.cmp(m).then_with(|| b.cmp(a)))
            .map(|(p, _)| p)
    }

    /// Is every member a declaration (DECLARED_SOURCES) — a cluster no
    /// phone or photo placed you in?
    fn declared_only(&self) -> bool {
        self.members.iter().all(|m| DECLARED_SOURCES.contains(&m.source.as_str()))
    }

    /// Is this cluster evidence of a STAY? It must hold at least one
    /// SENSED observation (a calendar entry alone is a plan, not a
    /// presence — DECLARED_SOURCES), unless the owner counts calendar
    /// entries alone (`declared_alone`). Then a dwell (an explicit
    /// departure, a calendar end included) always is; otherwise require
    /// several point-in-time signals spanning real time — one geotagged
    /// photo is a photo, not a visit.
    fn is_visit(&self, declared_alone: bool) -> bool {
        (declared_alone || !self.declared_only())
            && (self.members.iter().any(|m| m.t_end.is_some())
                || (self.members.len() >= 3 && self.end_unix - self.start_unix >= MAX_GAP_MIN * 60))
    }

    fn sources_line(&self) -> String {
        let mut counts: std::collections::BTreeMap<String, usize> = Default::default();
        for m in &self.members {
            // a source name comes from the posting client (ingest_location)
            *counts.entry(one_line(&m.source)).or_default() += 1;
        }
        counts
            .into_iter()
            .map(|(s, n)| format!("{s}\u{00d7}{n}"))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// Greedy time-ordered clustering: an observation joins the trailing
/// cluster it is spatially within (BASE_RADIUS grown by its accuracy)
/// and temporally adjacent to; else it opens a new cluster. Input must
/// be time-ordered (observations_for_day guarantees it). The setting
/// only decides which clusters count, never how they form.
pub(crate) fn cluster_day(observations: Vec<StoredObservation>, declared_alone: bool) -> Vec<Cluster> {
    let mut clusters: Vec<Cluster> = Vec::new();
    for o in observations {
        let Some(start) = ts_unix(&o.t_start) else { continue };
        let end = o.t_end.as_deref().and_then(ts_unix).unwrap_or(start);
        let radius = BASE_RADIUS_M.max(o.accuracy_m.unwrap_or(0.0));
        let joined = clusters.iter_mut().rev().find(|c| {
            start - c.end_unix <= MAX_GAP_MIN * 60
                && start >= c.start_unix - MAX_GAP_MIN * 60
                && dist_m(c.lat, c.lon, o.lat, o.lon) <= radius
        });
        match joined {
            Some(c) => c.absorb(o, start, end),
            None => clusters.push(Cluster::new(o, start, end)),
        }
    }
    clusters.retain(|c| c.is_visit(declared_alone));
    clusters
}

fn fmt_utc(unix: i64) -> String {
    chrono::DateTime::from_timestamp(unix, 0)
        .map(|d| d.format("%Y-%m-%dT%H:%M:%SZ").to_string())
        .unwrap_or_default()
}

fn fmt_hhmm_local(unix: i64, tz_offset_minutes: i32) -> String {
    chrono::DateTime::from_timestamp(unix + tz_offset_minutes as i64 * 60, 0)
        .map(|d| d.format("%H:%M").to_string())
        .unwrap_or_default()
}

/// Build one visit episode draft, `visit:<day>:<slot>` (`0`, `1`… for
/// sensed visits, `c0`, `c1`… for calendar-only ones). The body's single
/// `[HH:MM] place (lat,lon)` line is the canonical Atlas point for the
/// whole visit — per-observation points already reach the Atlas through
/// their own sources; the visit contributes exactly one.
pub(crate) fn visit_draft(day: &str, slot: &str, revision: u32, c: &Cluster, scope: &str) -> EpisodeDraft {
    let place = c.place();
    let label = place
        .clone()
        .unwrap_or_else(|| format!("({:.3}, {:.3})", c.lat, c.lon));
    let tz = c.members.first().map(|m| m.tz_offset_minutes).unwrap_or(0);
    let dwell_min = (c.end_unix - c.start_unix) / 60;
    let mut body = String::new();
    if let Some(p) = &place {
        body.push_str(&format!("place: {p}\n"));
    }
    body.push_str(&format!("arrival: {}\n", fmt_utc(c.start_unix)));
    if c.end_unix > c.start_unix {
        body.push_str(&format!("departure: {}\n", fmt_utc(c.end_unix)));
        body.push_str(&format!("dwell: {dwell_min} min\n"));
    }
    body.push_str(&format!("observations: {}\n", c.sources_line()));
    body.push_str(&format!(
        "[{}] {} ({:.5},{:.5})\n",
        fmt_hhmm_local(c.start_unix, tz),
        label,
        c.lat,
        c.lon
    ));
    let entities = place
        .as_ref()
        .map(|p| {
            vec![EntityRef {
                entity_type: "Place".into(),
                name: p.clone(),
                // a ~110 m grid key so the same venue resolves to the
                // same Place across days and sources
                attributes: vec![("geo".into(), format!("{:.3},{:.3}", c.lat, c.lon))],
                role: "location".into(),
            }]
        })
        .unwrap_or_default();
    let base = format!("visit:{day}:{slot}");
    EpisodeDraft {
        source: "visits".into(),
        source_ref: if revision <= 1 { base } else { format!("{base}#r{revision}") },
        kind: "visit".into(),
        title: format!("Visit \u{2014} {label}"),
        body,
        event_time: EventTime {
            utc: Some(fmt_utc(c.start_unix)),
            tz_offset_minutes: Some(tz),
            uncertainty: TimeUncertainty::Minute,
        },
        trust: "self".into(),
        scope: Some(scope.to_string()),
        entities,
        edges: Vec::new(),
    }
}

/// The device-data scope toggle (spec: "personal, with a config toggle
/// to force private"). `ingest.device.scope = private` in consolidation
/// state vaults everything this pass (and motion/focus ingest) mints.
pub(crate) fn device_scope(store: &hearthloom_core::store::Store) -> String {
    match store.consolidation_state("ingest.device.scope") {
        Ok(Some(v)) if v.trim().eq_ignore_ascii_case("private") => "private".into(),
        _ => "personal".into(),
    }
}

/// Cluster + mint every closed day whose observation union grew, then
/// withdraw calendar-only visits the old rule left. Returns
/// (days_processed, visits_minted, visits_superseded) — withdrawn visits
/// count as superseded.
pub(crate) fn finalize_visits(state: &AppState) -> anyhow::Result<(usize, usize, usize)> {
    let store = &state.store;
    let tz = store.observations_latest_tz_offset()?;
    let today_local = chrono::DateTime::from_timestamp(
        chrono::Utc::now().timestamp() + tz as i64 * 60,
        0,
    )
    .map(|d| d.format("%Y-%m-%d").to_string())
    .unwrap_or_else(|| chrono::Utc::now().format("%Y-%m-%d").to_string());
    finalize_visits_inner(store, &state.home, &today_local)
}

/// AppState-free core so tests drive it against a temp store.
pub(crate) fn finalize_visits_inner(
    store: &hearthloom_core::store::Store,
    home: &hearthloom_core::paths::Home,
    today_local: &str,
) -> anyhow::Result<(usize, usize, usize)> {
    let actor = Actor::new("agent:visit-finalize");
    let scope = device_scope(store);
    let declared_alone = calendar_alone(store);
    let days = store.observation_days_needing_visits(today_local)?;
    let (mut processed, mut minted, mut superseded) = (0usize, 0usize, 0usize);
    for day in days {
        let observations = store.observations_for_day(&day)?;
        let obs_count = observations.len() as i64;
        let mut keys: Vec<String> = observations.iter().map(|o| o.id.clone()).collect();
        let hash = hearthloom_core::store::photo_keys_hash(&mut keys);
        let prior = store.visit_day_state(&day)?;
        // superset guard (days_needing filters by count; the hash catches
        // an equal-count different-set race)
        if let Some((prior_count, prior_hash, _)) = &prior {
            if obs_count <= *prior_count || *prior_hash == hash {
                continue;
            }
        }
        let revision = prior.map(|(_, _, rev)| rev + 1).unwrap_or(1) as u32;
        let clusters = cluster_day(observations, declared_alone);
        // Sensed visits keep their `visit:<day>:<n>` series; calendar-only
        // ones (the setting on) count in their own `c<n>` series, after
        // them. So whether a node counted calendar entries alone never
        // moves another visit's ref: two nodes that clustered the day
        // under different settings still mint the same sensed visits, and
        // the cleanup below can tell these from what the old rule minted.
        let (declared, sensed): (Vec<&Cluster>, Vec<&Cluster>) =
            clusters.iter().partition(|c| c.declared_only());
        let drafts: Vec<EpisodeDraft> = sensed
            .iter()
            .enumerate()
            .map(|(n, c)| visit_draft(&day, &n.to_string(), revision, c, &scope))
            .chain(declared.iter().enumerate().map(|(n, c)| visit_draft(&day, &format!("c{n}"), revision, c, &scope)))
            .collect();
        let n_new = if drafts.is_empty() {
            0
        } else {
            let report = store.apply_batch(
                home,
                &IngestBatch {
                    actor: actor.actor.clone(),
                    session_id: None,
                    episodes: drafts.clone(),
                },
            )?;
            report.episodes_new
        };
        // absorb leftovers from a shrunken re-cluster into the day's
        // first visit (deterministic id — both peers compute the same).
        // A re-cluster can keep NO visit at all — a day minted before
        // calendar entries stopped counting alone — and its stale visits
        // go just the same, marked with the id of `visit:<day>#r<n>`, a
        // ref no episode carries: still a pure function of the day and
        // revision, so both peers converge.
        if revision > 1 {
            let absorbed_into = match drafts.first() {
                Some(first) => hearthloom_core::ids::episode_id("visits", &first.source_ref),
                None => hearthloom_core::ids::episode_id("visits", &format!("visit:{day}#r{revision}")),
            };
            superseded += store.supersede_stale_visits(&day, revision as i64, &absorbed_into)?;
        }
        store.set_visit_day_state(&day, obs_count, &hash, revision as i64)?;
        processed += 1;
        minted += n_new;
        std::thread::sleep(std::time::Duration::from_millis(40));
    }
    // The cleanup begins the first time this pass runs with the setting
    // off — here, or on the other node, whose start adopt_rule takes in.
    if !declared_alone && store.consolidation_state(KEY_CLEANUP)?.is_none() {
        store.set_consolidation_state(KEY_CLEANUP, &hearthloom_core::time::now_utc())?;
    }
    if store.consolidation_state(KEY_CLEANUP)?.is_some() {
        superseded += withdraw_calendar_only(store, &actor)?;
    }
    Ok((processed, minted, superseded))
}

/// `visit:<day>:<slot>[#r<n>]` → (day, slot).
fn ref_day_slot(source_ref: &str) -> Option<(&str, &str)> {
    let base = source_ref.strip_prefix("visit:")?.split('#').next()?;
    base.split_once(':').filter(|(day, slot)| !day.is_empty() && !slot.is_empty())
}

/// Did a visit come from declarations alone, by its own body? The
/// `observations:` line `visit_draft` writes lists every source that
/// contributed as `<source>×<count>`; a body qualifies only when it has
/// such a line and every entry of every such line is a declared source.
/// Before 0.5.71 a calendar venue was written with its line breaks, so an
/// old body can carry a forged `observations:` line: requiring ALL of
/// them means a forgery can keep a visit, never withdraw a sensed one.
pub(crate) fn declared_only_body(body: &str) -> bool {
    let mut lines = body.lines().filter_map(|line| line.strip_prefix("observations: ")).peekable();
    lines.peek().is_some()
        && lines.all(|sources| {
            sources.split(", ").all(|entry| {
                entry.rsplit_once('\u{00d7}').is_some_and(|(source, n)| {
                    DECLARED_SOURCES.contains(&source) && !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())
                })
            })
        })
}

/// The withdrawal of what the old rule minted from calendar entries alone
/// (before 0.5.71 a calendar event made a visit by itself — a stranger's
/// invite included): live visits in the sensed `<n>` series whose body
/// says calendar and nothing else. Calendar-only visits the setting
/// minted (`c<n>`) are left alone. Each is marked superseded by the id of
/// `visit:<day>#calendar-only` — a ref no episode carries, a pure function
/// of its day, so both nodes converge — and loses its map point (THE
/// SUPERSESSION PATTERN). Each visit is read once, past an episodes-rowid
/// watermark, in short batches 40 ms apart (THE PASS CONTRACT).
fn withdraw_calendar_only(store: &Store, actor: &Actor) -> anyhow::Result<usize> {
    let mut after: i64 = store
        .consolidation_state(KEY_CLEANUP_ROWID)?
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let mut withdrawn = 0usize;
    loop {
        let batch = store.visits_after(after, CLEANUP_BATCH)?;
        let Some(&(last, ..)) = batch.last() else { break };
        let calendar_only: Vec<(String, String)> = batch
            .iter()
            .filter(|(_, _, _, body)| declared_only_body(body))
            .filter_map(|(_, id, source_ref, _)| {
                let (day, slot) = ref_day_slot(source_ref)?;
                slot.bytes().all(|b| b.is_ascii_digit()).then(|| {
                    let marker = hearthloom_core::ids::episode_id("visits", &format!("visit:{day}#calendar-only"));
                    (id.clone(), marker)
                })
            })
            .collect();
        withdrawn += store.withdraw_visits(actor, "calendar_only", &calendar_only)?;
        store.set_consolidation_state(KEY_CLEANUP_ROWID, &last.to_string())?;
        if batch.len() < CLEANUP_BATCH {
            break;
        }
        after = last;
        std::thread::sleep(std::time::Duration::from_millis(40));
    }
    Ok(withdrawn)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hearthloom_core::paths::Home;
    use hearthloom_core::store::{ObservationIn, Store};

    fn node() -> (Store, Home, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().to_path_buf());
        home.init().unwrap();
        let store = Store::open(&home.db_path()).unwrap();
        (store, home, tmp)
    }

    fn obs(key: &str, t: &str, t_end: Option<&str>, lat: f64, lon: f64, place: Option<&str>) -> ObservationIn {
        ObservationIn {
            key: key.into(),
            lat,
            lon,
            accuracy_m: Some(30.0),
            t_start: t.into(),
            t_end: t_end.map(String::from),
            tz_offset_minutes: -240,
            place: place.map(String::from),
        }
    }

    /// THE cross-source dedup contract: a photo, a
    /// CLVisit, and a timeline import describing the same place/time
    /// become ONE visit episode citing all three.
    #[test]
    fn test_cross_source_observations_collapse_into_one_visit() {
        let (store, home, _tmp) = node();
        store
            .observations_add("photos", "a", &[obs(
                "p1", "2026-08-10T15:05:00Z", None, 42.3601, -71.0589, Some("Faneuil Hall"),
            )])
            .unwrap();
        store
            .observations_add("ios-visits", "a", &[obs(
                "v1", "2026-08-10T15:00:00Z", Some("2026-08-10T16:00:00Z"),
                42.3604, -71.0585, None,
            )])
            .unwrap();
        store
            .observations_add("google-timeline", "a", &[obs(
                "g1", "2026-08-10T15:10:00Z", Some("2026-08-10T15:55:00Z"),
                42.3599, -71.0592, None,
            )])
            .unwrap();
        let (days, minted, sup) = finalize_visits_inner(&store, &home, "2026-08-12").unwrap();
        assert_eq!((days, minted, sup), (1, 1, 0), "three sources, ONE visit");
        // re-run: nothing changes
        assert_eq!(finalize_visits_inner(&store, &home, "2026-08-12").unwrap(), (0, 0, 0));
        let (_, rows) = store
            .read_query(
                "SELECT e.id, e.source_ref FROM episodes e
                 WHERE e.source = 'visits' AND e.superseded_by IS NULL",
                10,
            )
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][1], serde_json::json!("visit:2026-08-10:0"));
        // provenance: the body cites all three sources + the place name won
        let (_, body_rows) = store
            .read_query("SELECT content_ref FROM episodes WHERE source = 'visits'", 10)
            .unwrap();
        let content_ref = body_rows[0][0].as_str().unwrap();
        let text = std::fs::read_to_string(home.root().join(content_ref)).unwrap();
        assert!(text.contains("google-timeline\u{00d7}1"), "{text}");
        assert!(text.contains("ios-visits\u{00d7}1"), "{text}");
        assert!(text.contains("photos\u{00d7}1"), "{text}");
        assert!(text.contains("place: Faneuil Hall"), "{text}");
        assert!(text.contains("dwell:"), "{text}");
    }

    #[test]
    fn test_lone_photo_is_not_a_visit_but_dwell_is() {
        let (store, home, _tmp) = node();
        // one geotagged photo → no visit
        store
            .observations_add("photos", "a", &[obs("p1", "2026-08-10T15:05:00Z", None, 42.0, -71.0, None)])
            .unwrap();
        let (days, minted, _) = finalize_visits_inner(&store, &home, "2026-08-12").unwrap();
        assert_eq!((days, minted), (1, 0), "a lone photo is a photo, not a visit");
        // a CLVisit dwell at another spot the next day → a visit even alone
        store
            .observations_add("ios-visits", "a", &[obs(
                "v1", "2026-08-11T10:00:00Z", Some("2026-08-11T11:00:00Z"), 43.0, -72.0, None,
            )])
            .unwrap();
        let (_, minted, _) = finalize_visits_inner(&store, &home, "2026-08-13").unwrap();
        assert_eq!(minted, 1);
    }

    /// The LIVE visits as (source_ref, body) — read_query sees no
    /// superseded rows.
    fn visit_bodies(store: &Store, home: &Home) -> Vec<(String, String)> {
        let (_, rows) = store
            .read_query("SELECT source_ref, content_ref FROM episodes WHERE source = 'visits'", 20)
            .unwrap();
        rows.iter()
            .map(|r| {
                let body = std::fs::read_to_string(home.root().join(r[1].as_str().unwrap())).unwrap();
                (r[0].as_str().unwrap().to_string(), body)
            })
            .collect()
    }

    /// Any visit by ref, whatever its lifecycle: its `superseded_by`.
    fn superseded_by(store: &Store, source_ref: &str) -> Option<String> {
        let id = hearthloom_core::ids::episode_id("visits", source_ref);
        store.episode_forensic(&id).unwrap().expect("the visit exists").1
    }

    /// A calendar entry alone is a plan, not a presence: even with an end
    /// time it mints no visit.
    #[test]
    fn test_calendar_alone_is_not_a_visit() {
        let (store, home, _tmp) = node();
        store
            .observations_add("calendar", "a", &[
                obs("c1", "2026-08-10T15:00:00Z", Some("2026-08-10T16:00:00Z"), 42.36, -71.06, Some("Board Room")),
                obs("c2", "2026-08-10T15:10:00Z", Some("2026-08-10T15:50:00Z"), 42.3601, -71.0601, Some("Board Room")),
                obs("c3", "2026-08-10T15:40:00Z", None, 42.3602, -71.0602, Some("Board Room")),
            ])
            .unwrap();
        let (days, minted, _) = finalize_visits_inner(&store, &home, "2026-08-12").unwrap();
        assert_eq!((days, minted), (1, 0), "calendar entries alone are not a visit");
        assert!(visit_bodies(&store, &home).is_empty());
        // the day is in the ledger all the same: not re-clustered every cycle
        assert_eq!(finalize_visits_inner(&store, &home, "2026-08-12").unwrap(), (0, 0, 0));
    }

    /// A sensed dwell at the calendar's venue IS a visit — named by the
    /// calendar, citing both sources.
    #[test]
    fn test_calendar_names_a_sensed_dwell() {
        let (store, home, _tmp) = node();
        store
            .observations_add("calendar", "a", &[obs(
                "c1", "2026-08-10T15:00:00Z", Some("2026-08-10T16:00:00Z"), 42.3601, -71.0589, Some("Board Room"),
            )])
            .unwrap();
        store
            .observations_add("ios-visits", "a", &[obs(
                "v1", "2026-08-10T15:05:00Z", Some("2026-08-10T15:55:00Z"), 42.3603, -71.0587, None,
            )])
            .unwrap();
        let (_, minted, _) = finalize_visits_inner(&store, &home, "2026-08-12").unwrap();
        assert_eq!(minted, 1);
        let visits = visit_bodies(&store, &home);
        assert_eq!(visits.len(), 1);
        let (source_ref, body) = &visits[0];
        assert_eq!(source_ref, "visit:2026-08-10:0");
        assert!(body.contains("place: Board Room\n"), "{body}");
        assert!(body.contains("calendar\u{00d7}1, ios-visits\u{00d7}1"), "{body}");
        assert!(body.lines().last().unwrap().starts_with("[11:00] Board Room ("), "{body}");
        let (_, titles) = store.read_query("SELECT title FROM episodes WHERE source = 'visits'", 5).unwrap();
        assert_eq!(titles[0][0], serde_json::json!("Visit \u{2014} Board Room"));
    }

    /// A day minted while calendar entries still counted alone: once its
    /// observations grow, the re-cluster keeps no visit, and the stale one
    /// goes (with its map point) instead of lingering.
    #[test]
    fn test_recluster_without_visits_withdraws_the_stale_ones() {
        let (store, home, _tmp) = node();
        let first = obs("c1", "2026-08-10T15:00:00Z", Some("2026-08-10T16:00:00Z"), 42.36, -71.06, Some("Dentist"));
        store.observations_add("calendar", "a", std::slice::from_ref(&first)).unwrap();
        // what the earlier rule minted for that day, and its ledger row
        let stored = store.observations_for_day("2026-08-10").unwrap();
        let (start, end) = (ts_unix(&stored[0].t_start).unwrap(), ts_unix(stored[0].t_end.as_deref().unwrap()).unwrap());
        let legacy = visit_draft("2026-08-10", "0", 1, &Cluster::new(stored[0].clone(), start, end), "personal");
        store
            .apply_batch(&home, &IngestBatch { actor: "agent:visit-finalize".into(), session_id: None, episodes: vec![legacy] })
            .unwrap();
        let mut keys = vec![stored[0].id.clone()];
        let hash = hearthloom_core::store::photo_keys_hash(&mut keys);
        store.set_visit_day_state("2026-08-10", 1, &hash, 1).unwrap();
        store.location_backfill(&Actor::new("human:test"), 10).unwrap();

        // a second calendar entry that day: the union grows, r2 keeps nothing
        store
            .observations_add("calendar", "a", &[obs(
                "c2", "2026-08-10T18:00:00Z", Some("2026-08-10T19:00:00Z"), 42.5, -71.5, Some("Gym"),
            )])
            .unwrap();
        let (days, minted, superseded) = finalize_visits_inner(&store, &home, "2026-08-12").unwrap();
        assert_eq!((days, minted, superseded), (1, 0, 1));
        assert!(visit_bodies(&store, &home).is_empty());
        let marker = hearthloom_core::ids::episode_id("visits", "visit:2026-08-10#r2");
        assert_eq!(superseded_by(&store, "visit:2026-08-10:0"), Some(marker.clone()));
        let (_, points) = store.read_query("SELECT COUNT(*) FROM location_points", 1).unwrap();
        assert_eq!(points[0][0], serde_json::json!(0));

        // a sensed dwell later: a visit again, named by the calendar
        store
            .observations_add("ios-visits", "a", &[obs(
                "v1", "2026-08-10T15:02:00Z", Some("2026-08-10T15:58:00Z"), 42.3601, -71.0601, None,
            )])
            .unwrap();
        let (_, minted, _) = finalize_visits_inner(&store, &home, "2026-08-12").unwrap();
        assert_eq!(minted, 1);
        let live = visit_bodies(&store, &home);
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].0, "visit:2026-08-10:0#r3");
        assert!(live[0].1.contains("place: Dentist\n"), "{}", live[0].1);
        // the withdrawn one stays withdrawn under the new revision
        assert_eq!(superseded_by(&store, "visit:2026-08-10:0"), Some(marker));
    }

    /// A venue with a line break in it (the calendar LOCATION arrives raw)
    /// cannot add a location line: the body keeps one, and the Atlas
    /// indexes exactly the cluster's own point.
    #[test]
    fn test_place_with_a_line_break_forges_no_point() {
        let (store, home, _tmp) = node();
        store
            .observations_add("calendar", "a", &[obs(
                "c1", "2026-08-10T15:00:00Z", Some("2026-08-10T16:00:00Z"), 42.3601, -71.0589,
                Some("Cafe\n[12:00] Elsewhere (40.71,-74.00)\u{2028}more"),
            )])
            .unwrap();
        store
            .observations_add("ios-visits\n[13:00] X (1,2)", "a", &[obs(
                "v1", "2026-08-10T15:05:00Z", Some("2026-08-10T15:55:00Z"), 42.3603, -71.0587, None,
            )])
            .unwrap();
        let (_, minted, _) = finalize_visits_inner(&store, &home, "2026-08-12").unwrap();
        assert_eq!(minted, 1);
        let (_, body) = &visit_bodies(&store, &home)[0];
        assert!(body.contains("place: Cafe [12:00] Elsewhere (40.71,-74.00) more\n"), "{body}");
        assert_eq!(body.lines().filter(|l| l.starts_with('[')).count(), 1, "{body}");
        let (_, added) = store.location_backfill(&Actor::new("human:test"), 10).unwrap();
        assert_eq!(added, 1);
        let (_, points) = store.read_query("SELECT lat, lon FROM location_points", 5).unwrap();
        let (lat, lon) = (points[0][0].as_f64().unwrap(), points[0][1].as_f64().unwrap());
        assert!((lat - 42.3602).abs() < 0.001 && (lon + 71.0588).abs() < 0.001, "{lat},{lon}");
    }

    /// Late-arriving observations re-mint the day under #r2 and the
    /// merged-away leftover is absorbed (superseded) — the shrink case.
    #[test]
    fn test_growing_union_supersedes_and_shrunken_recluster_absorbs() {
        let (store, home, _tmp) = node();
        // day 1: two separate dwells far apart → two visits
        store
            .observations_add("ios-visits", "a", &[
                obs("v1", "2026-08-10T10:00:00Z", Some("2026-08-10T11:00:00Z"), 42.0, -71.0, None),
                obs("v2", "2026-08-10T15:00:00Z", Some("2026-08-10T16:00:00Z"), 42.5, -71.5, None),
            ])
            .unwrap();
        let (_, minted, _) = finalize_visits_inner(&store, &home, "2026-08-12").unwrap();
        assert_eq!(minted, 2);
        // a photo lands later inside the first dwell → union grows, r2
        store
            .observations_add("photos", "a", &[obs("p1", "2026-08-10T10:30:00Z", None, 42.0001, -71.0001, Some("Home"))])
            .unwrap();
        let (_, minted2, _) = finalize_visits_inner(&store, &home, "2026-08-12").unwrap();
        assert!(minted2 >= 2, "r2 re-mints the day's visits");
        let (_, rows) = store
            .read_query(
                "SELECT COUNT(*), GROUP_CONCAT(source_ref) FROM episodes
                 WHERE source = 'visits' AND superseded_by IS NULL",
                10,
            )
            .unwrap();
        let unsup = rows[0][0].as_i64().unwrap();
        let refs = rows[0][1].as_str().unwrap_or("").to_string();
        assert_eq!(unsup, 2, "two live visits after r2: {refs}");
        assert!(refs.contains("#r2"), "{refs}");
    }

    /// Mint a day as the rule before 0.5.71 did — every cluster with a
    /// dwell counted, calendar-only ones too, numbered in one series — and
    /// ledger it, so the day is not clustered again.
    fn mint_as_old_rule(store: &Store, home: &Home, day: &str) {
        let observations = store.observations_for_day(day).unwrap();
        let mut keys: Vec<String> = observations.iter().map(|o| o.id.clone()).collect();
        let count = observations.len() as i64;
        let drafts: Vec<EpisodeDraft> = cluster_day(observations, true)
            .iter()
            .enumerate()
            .map(|(n, c)| visit_draft(day, &n.to_string(), 1, c, "personal"))
            .collect();
        store
            .apply_batch(home, &IngestBatch { actor: "agent:visit-finalize".into(), session_id: None, episodes: drafts })
            .unwrap();
        store.set_visit_day_state(day, count, &hearthloom_core::store::photo_keys_hash(&mut keys), 1).unwrap();
        store.location_backfill(&Actor::new("human:test"), 50).unwrap();
    }

    fn count(store: &Store, sql: &str) -> i64 {
        store.read_query(sql, 1).unwrap().1[0][0].as_i64().unwrap()
    }

    fn calendar_only_marker(day: &str) -> String {
        hearthloom_core::ids::episode_id("visits", &format!("visit:{day}#calendar-only"))
    }

    /// The `visit_withdrawn` mutations' payloads.
    fn withdrawals(store: &Store) -> Vec<Value> {
        let rows = store.mutations_since(0, 10_000).unwrap();
        rows.into_iter().filter(|m| m.kind == "visit_withdrawn").map(|m| m.payload).collect()
    }

    /// Setting off (the default): a calendar event alone mints nothing, and
    /// the visit the old rule minted from one is withdrawn — kept as
    /// content, gone from every live surface and from the map.
    #[test]
    fn test_setting_off_mints_no_calendar_visit_and_withdraws_the_old_one() {
        let (store, home, _tmp) = node();
        store
            .observations_add("calendar", "a", &[obs(
                "c1", "2026-08-09T13:00:00Z", Some("2026-08-09T14:00:00Z"), 42.36, -71.06, Some("Dentist"),
            )])
            .unwrap();
        mint_as_old_rule(&store, &home, "2026-08-09");
        assert_eq!(visit_bodies(&store, &home).len(), 1);
        assert_eq!(count(&store, "SELECT COUNT(*) FROM location_points"), 1);
        // a new day, a calendar entry and nothing else
        store
            .observations_add("calendar", "a", &[obs(
                "c2", "2026-08-10T15:00:00Z", Some("2026-08-10T16:00:00Z"), 42.5, -71.5, Some("Lunch"),
            )])
            .unwrap();

        let (days, minted, superseded) = finalize_visits_inner(&store, &home, "2026-08-12").unwrap();
        assert_eq!((days, minted, superseded), (1, 0, 1));
        assert!(visit_bodies(&store, &home).is_empty());
        assert_eq!(superseded_by(&store, "visit:2026-08-09:0"), Some(calendar_only_marker("2026-08-09")));
        assert_eq!(count(&store, "SELECT COUNT(*) FROM location_points"), 0);
        // nothing left the record: the episode and its words are all there
        let id = hearthloom_core::ids::episode_id("visits", "visit:2026-08-09:0");
        let (_, _, body) = store.episode_forensic(&id).unwrap().unwrap();
        assert!(body.unwrap().contains("observations: calendar\u{00d7}1"));
        assert_eq!(
            withdrawals(&store),
            [json!({"episode": id, "superseded_by": calendar_only_marker("2026-08-09"), "reason": "calendar_only"})]
        );
    }

    /// Setting on: a calendar-only cluster is a visit, in its own `c<n>`
    /// series, after the sensed ones.
    #[test]
    fn test_setting_on_counts_a_calendar_event_alone() {
        let (store, home, _tmp) = node();
        set_calendar_alone(&store, true).unwrap();
        store
            .observations_add("calendar", "a", &[obs(
                "c1", "2026-08-10T13:00:00Z", Some("2026-08-10T14:00:00Z"), 42.36, -71.06, Some("Dentist"),
            )])
            .unwrap();
        store
            .observations_add("ios-visits", "a", &[obs(
                "v1", "2026-08-10T18:00:00Z", Some("2026-08-10T19:00:00Z"), 42.5, -71.5, None,
            )])
            .unwrap();
        let (days, minted, superseded) = finalize_visits_inner(&store, &home, "2026-08-12").unwrap();
        assert_eq!((days, minted, superseded), (1, 2, 0));
        let mut visits = visit_bodies(&store, &home);
        visits.sort();
        assert_eq!(visits[0].0, "visit:2026-08-10:0");
        assert!(visits[0].1.contains("observations: ios-visits\u{00d7}1\n"), "{}", visits[0].1);
        assert_eq!(visits[1].0, "visit:2026-08-10:c0");
        assert!(visits[1].1.contains("place: Dentist\nar"), "{}", visits[1].1);
        assert!(visits[1].1.contains("observations: calendar\u{00d7}1\n"), "{}", visits[1].1);
        assert_eq!(finalize_visits_inner(&store, &home, "2026-08-12").unwrap(), (0, 0, 0));
    }

    /// A calendar entry with one geotagged photo inside it is a visit
    /// whatever the setting says, and no cleanup touches it.
    #[test]
    fn test_calendar_and_photo_is_a_visit_in_both_modes() {
        for on in [false, true] {
            let (store, home, _tmp) = node();
            set_calendar_alone(&store, on).unwrap();
            store
                .observations_add("calendar", "a", &[obs(
                    "c1", "2026-08-10T15:00:00Z", Some("2026-08-10T16:00:00Z"), 42.3601, -71.0589, Some("Board Room"),
                )])
                .unwrap();
            store
                .observations_add("photos", "a", &[obs("p1", "2026-08-10T15:20:00Z", None, 42.3602, -71.0588, None)])
                .unwrap();
            assert_eq!(finalize_visits_inner(&store, &home, "2026-08-12").unwrap(), (1, 1, 0), "on={on}");
            // the old rule's copy of the same day is kept too
            store
                .observations_add("calendar", "a", &[obs(
                    "c2", "2026-08-09T15:00:00Z", Some("2026-08-09T16:00:00Z"), 42.3601, -71.0589, Some("Board Room"),
                )])
                .unwrap();
            store
                .observations_add("photos", "a", &[obs("p2", "2026-08-09T15:20:00Z", None, 42.3602, -71.0588, None)])
                .unwrap();
            mint_as_old_rule(&store, &home, "2026-08-09");
            set_calendar_alone(&store, false).unwrap();
            assert_eq!(finalize_visits_inner(&store, &home, "2026-08-12").unwrap(), (0, 0, 0), "on={on}");
            let mut visits = visit_bodies(&store, &home);
            visits.sort();
            let refs: Vec<&str> = visits.iter().map(|(r, _)| r.as_str()).collect();
            assert_eq!(refs, ["visit:2026-08-09:0", "visit:2026-08-10:0"], "on={on}");
            for (_, body) in &visits {
                assert!(body.contains("observations: calendar\u{00d7}1, photos\u{00d7}1\n"), "{body}");
            }
        }
    }

    /// The cleanup withdraws only the calendar-alone visit of a day the
    /// old rule minted; a second pass, and even a full re-read, change
    /// nothing more.
    #[test]
    fn test_cleanup_keeps_sensed_visits_and_is_idempotent() {
        let (store, home, _tmp) = node();
        store
            .observations_add("calendar", "a", &[
                obs("c1", "2026-08-10T13:00:00Z", Some("2026-08-10T14:00:00Z"), 42.30, -71.00, Some("Dentist")),
                obs("c2", "2026-08-10T16:00:00Z", Some("2026-08-10T17:00:00Z"), 42.40, -71.10, Some("Board Room")),
            ])
            .unwrap();
        store
            .observations_add("photos", "a", &[obs("p1", "2026-08-10T16:30:00Z", None, 42.4001, -71.1001, None)])
            .unwrap();
        store
            .observations_add("ios-visits", "a", &[obs(
                "v1", "2026-08-10T20:00:00Z", Some("2026-08-10T21:00:00Z"), 42.50, -71.20, None,
            )])
            .unwrap();
        mint_as_old_rule(&store, &home, "2026-08-10");
        assert_eq!(visit_bodies(&store, &home).len(), 3);

        assert_eq!(finalize_visits_inner(&store, &home, "2026-08-12").unwrap(), (0, 0, 1));
        let mut live: Vec<String> = visit_bodies(&store, &home).into_iter().map(|(r, _)| r).collect();
        live.sort();
        assert_eq!(live, ["visit:2026-08-10:1", "visit:2026-08-10:2"], "the dentist alone goes");
        assert_eq!(superseded_by(&store, "visit:2026-08-10:0"), Some(calendar_only_marker("2026-08-10")));
        assert_eq!(count(&store, "SELECT COUNT(*) FROM location_points"), 2);

        // once per visit: the watermark moved past them …
        assert_eq!(finalize_visits_inner(&store, &home, "2026-08-12").unwrap(), (0, 0, 0));
        // … and reading them all again withdraws nothing more
        store.set_consolidation_state(KEY_CLEANUP_ROWID, "0").unwrap();
        assert_eq!(withdraw_calendar_only(&store, &Actor::new("human:test")).unwrap(), 0);
        assert_eq!(visit_bodies(&store, &home).len(), 2);
        assert_eq!(withdrawals(&store).len(), 1);
    }

    /// Switching changes only the days clustered after: on, the withdrawn
    /// visit stays withdrawn while a new calendar-only day mints; off
    /// again, that one stays too.
    #[test]
    fn test_switching_changes_only_later_days() {
        let (store, home, _tmp) = node();
        store
            .observations_add("calendar", "a", &[obs(
                "c1", "2026-08-09T13:00:00Z", Some("2026-08-09T14:00:00Z"), 42.36, -71.06, Some("Dentist"),
            )])
            .unwrap();
        mint_as_old_rule(&store, &home, "2026-08-09");
        assert_eq!(finalize_visits_inner(&store, &home, "2026-08-12").unwrap(), (0, 0, 1));

        set_calendar_alone(&store, true).unwrap();
        store
            .observations_add("calendar", "a", &[obs(
                "c2", "2026-08-10T15:00:00Z", Some("2026-08-10T16:00:00Z"), 42.5, -71.5, Some("Lunch"),
            )])
            .unwrap();
        assert_eq!(finalize_visits_inner(&store, &home, "2026-08-12").unwrap(), (1, 1, 0));
        let live = |store: &Store| -> Vec<String> {
            let mut refs: Vec<String> = visit_bodies(store, &home).into_iter().map(|(r, _)| r).collect();
            refs.sort();
            refs
        };
        assert_eq!(live(&store), ["visit:2026-08-10:c0"]);

        set_calendar_alone(&store, false).unwrap();
        assert_eq!(finalize_visits_inner(&store, &home, "2026-08-12").unwrap(), (0, 0, 0));
        assert_eq!(live(&store), ["visit:2026-08-10:c0"]);
    }

    /// The bodies the writers really produce, and an old body whose venue
    /// carried line breaks.
    #[test]
    fn test_declared_only_body_reads_real_bodies() {
        let calendar_only = "place: Dentist\narrival: 2026-08-10T13:00:00Z\n\
                             departure: 2026-08-10T14:00:00Z\ndwell: 60 min\n\
                             observations: calendar\u{00d7}1\n\
                             [09:00] Dentist (42.36000,-71.06000)\n";
        assert!(declared_only_body(calendar_only));
        assert!(declared_only_body(&calendar_only.replace("\u{00d7}1", "\u{00d7}2")));
        for sensed in ["calendar\u{00d7}1, photos\u{00d7}1", "calendar\u{00d7}1, ios-visits\u{00d7}1", "ios-visits\u{00d7}1"] {
            assert!(!declared_only_body(&calendar_only.replace("calendar\u{00d7}1", sensed)), "{sensed}");
        }
        // no line, or one not in visit_draft's shape: kept
        assert!(!declared_only_body("place: Dentist\narrival: 2026-08-10T13:00:00Z\n"));
        for odd in ["calendar", "calendar\u{00d7}", "calendar\u{00d7}one", "calendar\u{00d7}1,photos\u{00d7}1"] {
            assert!(!declared_only_body(&calendar_only.replace("calendar\u{00d7}1", odd)), "{odd}");
        }
        // a pre-0.5.71 venue with line breaks, written twice (place line and
        // label): a forged line can keep a calendar-only visit, never
        // withdraw a sensed one
        let venue = "Cafe\nobservations: calendar\u{00d7}1";
        let sensed = format!(
            "place: {venue}\narrival: 2026-08-10T13:00:00Z\n\
             observations: calendar\u{00d7}1, ios-visits\u{00d7}1\n[09:00] {venue} (42.36000,-71.06000)\n"
        );
        assert!(!declared_only_body(&sensed));
        let forged = calendar_only.replace("place: Dentist", "place: Dentist\nobservations: photos\u{00d7}1");
        assert!(!declared_only_body(&forged));
        // and what visit_draft writes today, both ways
        let o = |source: &str, key: &str| StoredObservation {
            id: format!("{source}|{key}"), source: source.into(), lat: 42.36, lon: -71.06, accuracy_m: None,
            t_start: "2026-08-10T13:00:00Z".into(), t_end: Some("2026-08-10T14:00:00Z".into()),
            tz_offset_minutes: -240, place: Some("Dentist".into()),
        };
        let mut c = Cluster::new(o("calendar", "c1"), 1, 2);
        assert!(declared_only_body(&visit_draft("2026-08-10", "c0", 1, &c, "personal").body));
        c.absorb(o("photos", "p1"), 1, 1);
        assert!(!declared_only_body(&visit_draft("2026-08-10", "0", 1, &c, "personal").body));
    }

    /// The setting syncs last-writer-wins (a tie goes to off), and a
    /// cleanup begun on one node runs on the other even with the setting
    /// on there — both withdraw the same visit under the same marker.
    #[test]
    fn test_rule_syncs_and_the_cleanup_runs_on_both_nodes() {
        let (laptop, laptop_home, _a) = node();
        let (tenant, tenant_home, _b) = node();
        assert!(rule_envelope(&laptop).is_none(), "nothing set, nothing sent");
        set_calendar_alone(&laptop, true).unwrap();
        assert!(adopt_rule(&tenant, &rule_envelope(&laptop).unwrap()).unwrap());
        assert!(calendar_alone(&tenant));
        assert!(!adopt_rule(&tenant, &rule_envelope(&laptop).unwrap()).unwrap(), "the same word twice");
        let stale = json!({"calendar_alone": false, "updated_at": "2000-01-01T00:00:00Z"});
        assert!(!adopt_rule(&tenant, &stale).unwrap());
        assert!(calendar_alone(&tenant));
        // the same second, opposite words: both end off
        for store in [&laptop, &tenant] {
            store.set_consolidation_state(KEY_CALENDAR_ALONE_STAMP, "2001-01-01T00:00:00Z").unwrap();
        }
        tenant.set_consolidation_state(KEY_CALENDAR_ALONE, "off").unwrap();
        assert!(!adopt_rule(&tenant, &rule_envelope(&laptop).unwrap()).unwrap());
        assert!(adopt_rule(&laptop, &rule_envelope(&tenant).unwrap()).unwrap());
        assert!(!calendar_alone(&laptop) && !calendar_alone(&tenant));

        // both hold the visit the old rule minted; the tenant runs its pass
        // with the setting off, then the owner switches it on at the laptop
        for (store, home) in [(&laptop, &laptop_home), (&tenant, &tenant_home)] {
            store
                .observations_add("calendar", "a", &[obs(
                    "c1", "2026-08-09T13:00:00Z", Some("2026-08-09T14:00:00Z"), 42.36, -71.06, Some("Dentist"),
                )])
                .unwrap();
            mint_as_old_rule(store, home, "2026-08-09");
        }
        assert_eq!(finalize_visits_inner(&tenant, &tenant_home, "2026-08-12").unwrap(), (0, 0, 1));
        set_calendar_alone(&laptop, true).unwrap();
        assert_eq!(finalize_visits_inner(&laptop, &laptop_home, "2026-08-12").unwrap(), (0, 0, 0));
        // the next sync: the laptop's newer setting goes, the tenant's start comes back
        assert!(adopt_rule(&tenant, &rule_envelope(&laptop).unwrap()).unwrap());
        assert!(adopt_rule(&laptop, &rule_envelope(&tenant).unwrap()).unwrap());
        assert!(calendar_alone(&laptop) && calendar_alone(&tenant));
        assert_eq!(finalize_visits_inner(&laptop, &laptop_home, "2026-08-12").unwrap(), (0, 0, 1));
        for store in [&laptop, &tenant] {
            assert_eq!(superseded_by(store, "visit:2026-08-09:0"), Some(calendar_only_marker("2026-08-09")));
        }
    }

    /// Two nodes that cluster a day under different settings mint the same
    /// sensed visit — same ref, same body — and differ only by the
    /// calendar-only one, which then reaches the other as any episode does.
    #[test]
    fn test_nodes_with_different_settings_mint_the_same_sensed_visits() {
        let (off, off_home, _a) = node();
        let (on, on_home, _b) = node();
        set_calendar_alone(&on, true).unwrap();
        for store in [&off, &on] {
            store
                .observations_add("calendar", "a", &[obs(
                    "c1", "2026-08-10T13:00:00Z", Some("2026-08-10T14:00:00Z"), 42.30, -71.00, Some("Dentist"),
                )])
                .unwrap();
            store
                .observations_add("ios-visits", "a", &[obs(
                    "v1", "2026-08-10T18:00:00Z", Some("2026-08-10T19:00:00Z"), 42.50, -71.20, None,
                )])
                .unwrap();
        }
        assert_eq!(finalize_visits_inner(&off, &off_home, "2026-08-12").unwrap(), (1, 1, 0));
        assert_eq!(finalize_visits_inner(&on, &on_home, "2026-08-12").unwrap(), (1, 2, 0));
        let mut on_visits = visit_bodies(&on, &on_home);
        on_visits.sort();
        assert_eq!(visit_bodies(&off, &off_home), on_visits[..1]);
        assert_eq!(on_visits[1].0, "visit:2026-08-10:c0");
    }
}
