//! Live HealthKit ingest (POST /v1/ingest/health) + the day-finalize
//! pass — the server half of the iOS companion app's health stream.
//!
//! The durable rows live in the store (health_sample, v44); this module
//! is the bridge onto the EXISTING export-path aggregation: stored rows
//! are mapped onto `apple_health::Sample` and fed through the same
//! `DayTable::into_episodes`, so a live-minted `health_day` episode is
//! identical to what an export.xml scan of the same samples would mint —
//! and the shared `day:YYYY-MM-DD` source_ref + UNIQUE(source,source_ref)
//! makes whichever path runs first win (convergence by construction).

use chrono::{DateTime, FixedOffset};
use hearthloom_adapters::apple_health;
use hearthloom_core::store::{Actor, StoredHealthSample};
use hearthloom_core::{EpisodeDraft, IngestBatch};

use crate::AppState;

/// RFC3339 UTC + offset → the export.xml timestamp shape the adapter
/// parses ("2026-07-24 08:00:00 -0400"). Keeping the string form
/// identical means `parse_ts`, event times, and workout bodies come out
/// exactly as the export path produces them.
pub(crate) fn to_raw_local(rfc3339_utc: &str, tz_off_min: i32) -> String {
    let off = FixedOffset::east_opt(tz_off_min * 60)
        .unwrap_or_else(|| FixedOffset::east_opt(0).unwrap());
    DateTime::parse_from_rfc3339(rfc3339_utc)
        .map(|dt| dt.with_timezone(&off).format("%Y-%m-%d %H:%M:%S %z").to_string())
        .unwrap_or_else(|_| rfc3339_utc.to_string())
}

/// One live workout as the iOS client sends it.
#[derive(Debug, serde::Deserialize)]
pub(crate) struct HealthWorkoutIn {
    pub uuid: String,
    /// Human-readable HKWorkoutActivityType name (stored as received —
    /// the client sends readable names, per the contract decision).
    pub activity: String,
    pub start: String,
    pub end: String,
    #[serde(default)]
    pub energy_kcal: Option<f64>,
    #[serde(default)]
    pub distance_m: Option<f64>,
    pub source: String,
    pub tz_offset_minutes: i32,
}

/// Live workout → episode, REUSING the export path's body/title builder
/// so both look identical — then re-keyed on the stable HK uuid
/// (`workout:<uuid>`, the decided live idempotency key; export keys on
/// its raw local start string) and stamped trust "self": this came from
/// the user's own device, not an imported archive.
pub(crate) fn workout_in_to_episode(w: &HealthWorkoutIn) -> EpisodeDraft {
    let start_local = to_raw_local(&w.start, w.tz_offset_minutes);
    let end_local = to_raw_local(&w.end, w.tz_offset_minutes);
    let duration_min = DateTime::parse_from_rfc3339(&w.start)
        .ok()
        .zip(DateTime::parse_from_rfc3339(&w.end).ok())
        .map(|(s, e)| (e - s).num_seconds().max(0) as f64 / 60.0);
    let rec = apple_health::WorkoutRec {
        activity: w.activity.clone(),
        duration_min,
        distance_km: w.distance_m.map(|m| m / 1000.0),
        kcal: w.energy_kcal,
        source: apple_health::normalize_source(&w.source),
        start: start_local,
        end: end_local,
        route: None,
    };
    let mut ep = apple_health::workout_to_episode(&rec, Some("health"));
    ep.source_ref = format!("workout:{}", w.uuid);
    ep.trust = "self".into();
    ep
}

/// Stored durable row → the adapter's Sample, so DayTable aggregates
/// live rows exactly as it aggregates export.xml rows.
fn stored_to_sample(s: &StoredHealthSample) -> apple_health::Sample {
    apple_health::Sample {
        metric: s.metric.clone(),
        value: s.value,
        category: s.category.clone(),
        unit: s.unit.clone(),
        source: apple_health::normalize_source(&s.source),
        start: to_raw_local(&s.start_utc, s.tz_offset_minutes),
        day: s.day.clone(),
    }
}

/// Mint `health_day` episodes for every closed, grace-elapsed day that
/// has live samples but no episode yet. Pure store+CPU — the caller
/// spawn_blockings this whole function (THE PASS CONTRACT). Returns
/// (days_finalized, episodes_new).
pub(crate) fn finalize_health_days(state: &AppState) -> anyhow::Result<(usize, usize)> {
    let actor = Actor::new("agent:day-finalize");
    // "Today" in the device's timezone — taken from the newest sample's
    // offset (falls back to UTC before any sample exists). Using the
    // DEVICE's day, not the server's, keeps a UTC-hosted tenant from
    // sealing the user's still-running local day.
    let (today_local, grace_cutoff) = {
        let tz_off = state.store.health_latest_tz_offset().unwrap_or(0);
        let off = FixedOffset::east_opt(tz_off * 60)
            .unwrap_or_else(|| FixedOffset::east_opt(0).unwrap());
        let now = chrono::Utc::now();
        (
            now.with_timezone(&off).format("%Y-%m-%d").to_string(),
            // newest sample must be ≥2h old before its day seals, so
            // hourly background-delivery stragglers land first
            (now - chrono::Duration::hours(2))
                .format("%Y-%m-%dT%H:%M:%SZ")
                .to_string(),
        )
    };
    let (sealed, minted) = finalize_days_inner(
        &state.store,
        &state.home,
        &actor,
        &today_local,
        &grace_cutoff,
    )?;
    for day in &sealed {
        crate::provisioner::notify_system(
            state,
            "info",
            "Health day ready",
            &format!("Your {day} health summary is in."),
            Some(&format!("health_day:{day}")),
        );
    }
    Ok((sealed.len(), minted))
}

/// The testable core of the pass: seal every eligible day, returning
/// (days sealed, episodes minted). AppState-free so tests can drive it
/// against a temp store.
fn finalize_days_inner(
    store: &hearthloom_core::store::Store,
    home: &hearthloom_core::Home,
    actor: &Actor,
    today_local: &str,
    grace_cutoff: &str,
) -> anyhow::Result<(Vec<String>, usize)> {
    let days = store.health_unfinalized_days(today_local, grace_cutoff)?;
    let mut minted = 0usize;
    let mut sealed = Vec::new();
    for day in days {
        let samples = store.health_samples_for_day(&day)?;
        let mut table = apple_health::DayTable::default();
        for s in &samples {
            table.add(&stored_to_sample(s));
        }
        let episodes = table.into_episodes(Some("health"));
        if episodes.is_empty() {
            continue; // nothing episode-worthy; the rows stay queryable
        }
        for chunk in episodes.chunks(200) {
            let r = store.apply_batch(
                home,
                &IngestBatch {
                    actor: actor.actor.clone(),
                    session_id: None,
                    episodes: chunk.to_vec(),
                },
            )?;
            minted += r.episodes_new;
        }
        sealed.push(day);
        // small gap between days so handlers parked on the store mutex
        // get through (the refresh_local_sources pattern)
        std::thread::sleep(std::time::Duration::from_millis(40));
    }
    Ok((sealed, minted))
}

#[cfg(test)]
mod tests {
    use super::*;
    use hearthloom_core::store::HealthSampleRow;

    #[test]
    fn test_to_raw_local_matches_export_shape() {
        let raw = to_raw_local("2026-07-24T12:00:00Z", -240);
        assert_eq!(raw, "2026-07-24 08:00:00 -0400");
    }

    #[test]
    fn test_live_day_converges_with_export_aggregation() {
        // The same physical samples, once through the LIVE path (store →
        // stored_to_sample → DayTable) and once as the EXPORT path would
        // see them (adapter Samples fed straight to DayTable), must mint
        // byte-identical health_day episodes.
        let store = hearthloom_core::store::Store::open_in_memory().unwrap();
        let live = vec![
            HealthSampleRow {
                uuid: "s1".into(),
                metric: "StepCount".into(),
                value: Some(4200.0),
                unit: Some("count".into()),
                category: None,
                source: "Ada’s Apple Watch".into(),
                start: "2026-07-24T12:00:00Z".into(),
                end: "2026-07-24T12:05:00Z".into(),
                tz_offset_minutes: -240,
            },
            HealthSampleRow {
                uuid: "s2".into(),
                metric: "SleepAnalysis".into(),
                value: None,
                unit: None,
                category: Some("AsleepDeep".into()),
                source: "Ada’s Apple Watch".into(),
                start: "2026-07-24T05:00:00Z".into(),
                end: "2026-07-24T06:30:00Z".into(),
                tz_offset_minutes: -240,
            },
        ];
        store.health_add_samples(&live).unwrap();
        let mut live_table = apple_health::DayTable::default();
        for day in ["2026-07-24"] {
            for s in &store.health_samples_for_day(day).unwrap() {
                live_table.add(&stored_to_sample(s));
            }
        }
        let live_eps = live_table.into_episodes(Some("health"));

        // the export path's view of the same reality
        let mut export_table = apple_health::DayTable::default();
        export_table.add(&apple_health::Sample {
            metric: "StepCount".into(),
            value: Some(4200.0),
            category: None,
            unit: Some("count".into()),
            source: "Apple Watch".into(),
            start: "2026-07-24 08:00:00 -0400".into(),
            day: "2026-07-24".into(),
        });
        export_table.add(&apple_health::Sample {
            metric: "SleepAnalysis".into(),
            value: Some(1.5),
            category: Some("AsleepDeep".into()),
            unit: None,
            source: "Apple Watch".into(),
            start: "2026-07-24 01:00:00 -0400".into(),
            day: "2026-07-24".into(),
        });
        let export_eps = export_table.into_episodes(Some("health"));

        assert_eq!(live_eps.len(), 1);
        assert_eq!(export_eps.len(), 1);
        assert_eq!(live_eps[0].source_ref, "day:2026-07-24");
        assert_eq!(live_eps[0].body, export_eps[0].body, "live body must equal export body");
        assert_eq!(live_eps[0].title, export_eps[0].title);
    }

    #[test]
    fn test_finalize_once_then_export_first_writer_wins() {
        // Seal a closed day → exactly one health_day episode; run the
        // pass again → nothing new; then an export.xml-style ingest of
        // the same day is a silent no-op (first-writer-wins on the
        // shared day:YYYY-MM-DD key).
        let tmp = tempfile::tempdir().unwrap();
        let home = hearthloom_core::Home::at(tmp.path().to_path_buf());
        home.init().unwrap();
        let store = hearthloom_core::store::Store::open(&home.db_path()).unwrap();
        let actor = Actor::new("agent:day-finalize");
        store
            .health_add_samples(&[HealthSampleRow {
                uuid: "s1".into(),
                metric: "StepCount".into(),
                value: Some(9000.0),
                unit: Some("count".into()),
                category: None,
                source: "Apple Watch".into(),
                start: "2026-07-24T12:00:00Z".into(),
                end: "2026-07-24T12:05:00Z".into(),
                tz_offset_minutes: -240,
            }])
            .unwrap();
        let far_future = "2999-01-01T00:00:00Z";
        let (sealed, minted) =
            finalize_days_inner(&store, &home, &actor, "2026-07-26", far_future).unwrap();
        assert_eq!(sealed, vec!["2026-07-24"]);
        assert_eq!(minted, 1);
        // finalize again: the day now has its episode → not eligible
        let (sealed, minted) =
            finalize_days_inner(&store, &home, &actor, "2026-07-26", far_future).unwrap();
        assert!(sealed.is_empty());
        assert_eq!(minted, 0);
        // an export-path ingest of the same day converges to a no-op
        let mut export_table = apple_health::DayTable::default();
        export_table.add(&apple_health::Sample {
            metric: "StepCount".into(),
            value: Some(9500.0),
            category: None,
            unit: Some("count".into()),
            source: "Apple Watch".into(),
            start: "2026-07-24 08:00:00 -0400".into(),
            day: "2026-07-24".into(),
        });
        let r = store
            .apply_batch(
                &home,
                &IngestBatch {
                    actor: "adapter:apple-health".into(),
                    session_id: None,
                    episodes: export_table.into_episodes(Some("health")),
                },
            )
            .unwrap();
        assert_eq!(r.episodes_new, 0, "live-minted day must win over the later export");
        assert_eq!(r.episodes_existing, 1);
    }

    #[test]
    fn test_workout_episode_keys_on_uuid_with_self_trust() {
        let w = HealthWorkoutIn {
            uuid: "ABC-123".into(),
            activity: "Running".into(),
            start: "2026-07-24T12:00:00Z".into(),
            end: "2026-07-24T12:45:00Z".into(),
            energy_kcal: Some(320.0),
            distance_m: Some(5000.0),
            source: "Apple Watch".into(),
            tz_offset_minutes: -240,
        };
        let ep = workout_in_to_episode(&w);
        assert_eq!(ep.source_ref, "workout:ABC-123");
        assert_eq!(ep.trust, "self");
        assert_eq!(ep.kind, "workout");
        assert!(ep.title.contains("Running"));
        assert!(ep.body.contains("45 min"), "duration derived: {}", ep.body);
        assert!(ep.body.contains("5.0 km"));
        assert!(ep.body.contains("320 kcal"));
        assert!(ep.body.contains("start: 2026-07-24 08:00:00 -0400"));
    }
}
