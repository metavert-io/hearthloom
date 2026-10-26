//! Trip reconciliation (v60, the movement layer's pass — visits.rs's
//! sibling). For each closed day with a vehicle's drives, decide WHOSE
//! drive each one was and mint one canonical episode per drive:
//!
//! | phone says                       | car says | result                              |
//! |----------------------------------|----------|-------------------------------------|
//! | in a vehicle, overlapping        | a drive  | `trip` — you were in the car; the car's numbers, the phone's presence; the phone's own trip episode is absorbed |
//! | walking/still, or a dwell far away | a drive | `vehicle_drive` — the car was driven, not by you; kept off your timeline and visits |
//! | nothing (and you have a phone feed) | a drive | `vehicle_drive`, "who was in it: not known yet" + a Tend yes/no |
//! | no phone feed at all             | a drive  | the car's default (you, unless the settings say never) |
//! | in a vehicle                     | nothing  | untouched — the phone's own trip (another car) |
//!
//! An explicit answer outranks everything; Guest Mode means it wasn't
//! you; a car's settings can say always-me / never-me. The car exposes no
//! driver identity (no profile name, no phone-key id — confirmed against
//! Tesla's signal list), so "who drove" when you were aboard comes from
//! the car's `aboard_driver` setting and is labelled as a default.
//!
//! Convergence (see core movements.rs): the revision suffix is a
//! fingerprint of the day's OUTPUT, so two nodes with the same inputs mint
//! identical refs, and `assert_trip_day` re-establishes the minted set if
//! a peer's stale revision landed later. Your drives' endpoints become
//! location observations (source `trips`) so visits know where you
//! parked; they're excluded from the "were you elsewhere" test.
//!
//! Pure store+CPU — spawn_blocking'd whole (THE PASS CONTRACT).

use hearthloom_core::store::{Actor, ObservationIn, StoredMovement, StoredObservation, VehicleProfile};
use hearthloom_core::time::{EventTime, TimeUncertainty};
use hearthloom_core::{EntityRef, EpisodeDraft, IngestBatch};
use serde_json::json;

use crate::AppState;

/// An overlap counts when it covers this share of the shorter span.
const OVERLAP_SHARE: f64 = 0.5;
/// A dwell this far from both ends of a drive means you weren't in it.
const ELSEWHERE_M: f64 = 1000.0;
/// "Do you have a phone motion feed at all?" looks this far back.
const PHONE_FEED_DAYS: i64 = 30;

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Aboard {
    Yes,
    No,
    Unknown,
}

#[derive(Debug, Clone)]
pub(crate) struct Decision {
    pub aboard: Aboard,
    /// Why, in words the user reads in the episode body.
    pub basis: &'static str,
    /// driver | passenger | occupant (aboard, driver unknown) | none
    pub role: &'static str,
    pub driver: Option<String>,
    /// Phone segments (movement ids) this drive absorbs.
    pub absorbed: Vec<String>,
}

fn unix(t: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(t).map(|d| d.timestamp()).unwrap_or(0)
}

fn overlap_share(a0: i64, a1: i64, b0: i64, b1: i64) -> f64 {
    let ov = (a1.min(b1) - a0.max(b0)).max(0) as f64;
    let shorter = ((a1 - a0).min(b1 - b0)).max(1) as f64;
    ov / shorter
}

fn dist_m(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let mlat = (lat1 + lat2) / 2.0;
    let kx = mlat.to_radians().cos() * 111_320.0;
    (((lat1 - lat2) * 110_574.0).powi(2) + ((lon1 - lon2) * kx).powi(2)).sqrt()
}

/// Decide one drive. Pure: everything it needs is passed in.
pub(crate) fn decide(
    drive: &StoredMovement,
    phone: &[StoredMovement],
    dwells: &[StoredObservation],
    profile: &VehicleProfile,
    answer: Option<&str>,
    phone_feed: bool,
    used: &std::collections::HashSet<String>,
) -> Decision {
    let (d0, d1) = (unix(&drive.t_start), unix(&drive.t_end));
    let aboard_role = |basis: &'static str, absorbed: Vec<String>| -> Decision {
        let (role, driver) = match profile.aboard_driver.as_str() {
            "me" => ("driver", None),
            "unknown" => ("occupant", None),
            name => ("passenger", Some(name.to_string())),
        };
        Decision { aboard: Aboard::Yes, basis, role, driver, absorbed }
    };
    let not_me = |basis: &'static str| -> Decision {
        let driver = (profile.household.len() == 1).then(|| profile.household[0].clone());
        Decision { aboard: Aboard::No, basis, role: "none", driver, absorbed: Vec::new() }
    };
    // the phone's in-vehicle segments overlapping this drive
    let in_vehicle: Vec<String> = phone
        .iter()
        .filter(|p| p.mode == "automotive" && !used.contains(&p.id))
        .filter(|p| overlap_share(d0, d1, unix(&p.t_start), unix(&p.t_end)) >= OVERLAP_SHARE)
        .map(|p| p.id.clone())
        .collect();
    match answer {
        Some("aboard") => return aboard_role("you said you were in the car", in_vehicle),
        Some("not_me") => return not_me("you said you weren't in the car"),
        _ => {}
    }
    if drive.attrs.get("guest_mode").and_then(|v| v.as_bool()) == Some(true) {
        return not_me("the car was in Guest Mode");
    }
    match profile.attribution.as_str() {
        "always_me" => return aboard_role("this car's settings say it's always you", in_vehicle),
        "never_me" => return not_me("this car's settings say it's never you"),
        _ => {}
    }
    if !in_vehicle.is_empty() {
        return aboard_role("your phone was in a vehicle at the same time", in_vehicle);
    }
    let on_foot = phone.iter().any(|p| {
        matches!(p.mode.as_str(), "walking" | "running" | "cycling" | "stationary")
            && overlap_share(d0, d1, unix(&p.t_start), unix(&p.t_end)) >= OVERLAP_SHARE
            && (unix(&p.t_end) - unix(&p.t_start)) >= (d1 - d0) / 2
    });
    let far_dwell = dwells.iter().any(|o| {
        let Some(t_end) = o.t_end.as_deref() else { return false };
        if overlap_share(d0, d1, unix(&o.t_start), unix(t_end)) < OVERLAP_SHARE {
            return false;
        }
        let ends = [(drive.start_lat, drive.start_lon), (drive.end_lat, drive.end_lon)];
        let known: Vec<(f64, f64)> = ends.iter().filter_map(|(a, b)| Some(((*a)?, (*b)?))).collect();
        !known.is_empty() && known.iter().all(|(la, lo)| dist_m(o.lat, o.lon, *la, *lo) > ELSEWHERE_M)
    });
    if on_foot || far_dwell {
        return not_me("your phone was somewhere else at the time");
    }
    if !phone_feed {
        return aboard_role("no phone data to check — this car's default", Vec::new());
    }
    Decision { aboard: Aboard::Unknown, basis: "not known yet", role: "none", driver: None, absorbed: Vec::new() }
}

fn fmt_hhmm(unix_s: i64, tz: i32) -> String {
    chrono::DateTime::from_timestamp(unix_s + tz as i64 * 60, 0)
        .map(|d| d.format("%H:%M").to_string())
        .unwrap_or_default()
}

fn place_label(place: &Option<String>, lat: Option<f64>, lon: Option<f64>) -> Option<String> {
    match (place, lat, lon) {
        (Some(p), _, _) if !p.trim().is_empty() => Some(p.trim().to_string()),
        (_, Some(la), Some(lo)) => Some(format!("({la:.3}, {lo:.3})")),
        _ => None,
    }
}

fn vehicle_name(profile: &VehicleProfile) -> String {
    if profile.name.trim().is_empty() { "the car".into() } else { profile.name.clone() }
}

/// One drive's episode draft (ref filled in by the caller).
pub(crate) fn drive_draft(
    drive: &StoredMovement,
    d: &Decision,
    profile: &VehicleProfile,
    absorbed_sources: &[String],
    mine_scope: &str,
) -> EpisodeDraft {
    let tz = drive.tz_offset_minutes;
    let (s, e) = (unix(&drive.t_start), unix(&drive.t_end));
    let from = place_label(&drive.start_place, drive.start_lat, drive.start_lon);
    let to = place_label(&drive.end_place, drive.end_lat, drive.end_lon);
    let route = match (&from, &to) {
        (Some(a), Some(b)) => format!("{a} \u{2192} {b}"),
        (Some(a), None) => format!("from {a}"),
        (None, Some(b)) => format!("to {b}"),
        _ => String::new(),
    };
    let km = drive.distance_m.map(|m| m / 1000.0);
    let car = vehicle_name(profile);
    let size = km.map(|k| format!("{k:.1} km")).unwrap_or_else(|| format!("{} min", (e - s) / 60));
    let title = match (&d.aboard, d.role) {
        (Aboard::Yes, "driver") => format!("Drive \u{2014} {}{size}", if route.is_empty() { String::new() } else { format!("{route}, ") }),
        (Aboard::Yes, _) => format!("Ride in the {car} \u{2014} {}{size}", if route.is_empty() { String::new() } else { format!("{route}, ") }),
        (Aboard::No, _) => format!(
            "{car} \u{2014} driven by {}{}",
            d.driver.as_deref().unwrap_or("someone else"),
            if route.is_empty() { format!(", {size}") } else { format!(", {route} ({size})") }
        ),
        (Aboard::Unknown, _) => format!(
            "{car} drove {}",
            if route.is_empty() { size } else { format!("{route} ({size})") }
        ),
    };
    let mut body = String::new();
    body.push_str(&format!("vehicle: {car}\n"));
    let who = match (&d.aboard, d.role) {
        (Aboard::Yes, "driver") => "you drove".to_string(),
        (Aboard::Yes, "passenger") => format!("you rode along; {} drove", d.driver.as_deref().unwrap_or("someone")),
        (Aboard::Yes, _) => "you were in the car".to_string(),
        (Aboard::No, _) => format!("not you — {}", d.driver.as_deref().unwrap_or("someone else")),
        (Aboard::Unknown, _) => "who was in the car isn't known yet".to_string(),
    };
    body.push_str(&format!("who: {who} ({})\n", d.basis));
    body.push_str(&format!("start: {}\nend: {}\n", drive.t_start, drive.t_end));
    body.push_str(&format!("duration: {} min\n", (e - s).max(0) / 60));
    if let Some(k) = km {
        body.push_str(&format!("distance: {k:.2} km\n"));
    }
    let a = &drive.attrs;
    if let Some(kwh) = a.get("energy_kwh").and_then(|v| v.as_f64()) {
        body.push_str(&format!("energy: {kwh:.2} kWh\n"));
        if let Some(k) = km.filter(|k| *k > 0.5) {
            body.push_str(&format!("efficiency: {:.0} Wh/km\n", kwh * 1000.0 / k));
        }
    }
    if let Some(t) = a.get("outside_temp_c").and_then(|v| v.as_f64()) {
        body.push_str(&format!("outside: {t:.0} \u{00b0}C\n"));
    }
    if let Some(media) = a.get("media").and_then(|v| v.as_array()).filter(|m| !m.is_empty()) {
        let items: Vec<String> = media
            .iter()
            .filter_map(|m| {
                let title = m.get("title").and_then(|v| v.as_str())?;
                Some(match m.get("artist").and_then(|v| v.as_str()) {
                    Some(artist) => format!("{title} \u{2014} {artist}"),
                    None => title.to_string(),
                })
            })
            .take(12)
            .collect();
        if !items.is_empty() {
            body.push_str(&format!("listening: {}\n", items.join("; ")));
        }
    }
    let mut srcs = vec![drive.source.clone()];
    srcs.extend(absorbed_sources.iter().cloned());
    let mut counts: std::collections::BTreeMap<String, usize> = Default::default();
    for s in srcs {
        *counts.entry(s).or_default() += 1;
    }
    body.push_str(&format!(
        "sources: {}\n",
        counts.iter().map(|(s, n)| format!("{s}\u{00d7}{n}")).collect::<Vec<_>>().join(", ")
    ));
    let mut entities = vec![EntityRef {
        entity_type: "Vehicle".into(),
        name: car.clone(),
        attributes: vec![("vehicle".into(), drive.subject.clone())],
        role: "vehicle".into(),
    }];
    // Only YOUR drives place you: map points + Place entities. Someone
    // else's drive never puts their whereabouts on your Atlas.
    if d.aboard == Aboard::Yes {
        for (label, lat, lon, at) in [
            (&from, drive.start_lat, drive.start_lon, s),
            (&to, drive.end_lat, drive.end_lon, e),
        ] {
            if let (Some(label), Some(lat), Some(lon)) = (label, lat, lon) {
                body.push_str(&format!("[{}] {label} ({lat:.5},{lon:.5})\n", fmt_hhmm(at, tz)));
            }
        }
        for place in [&drive.start_place, &drive.end_place].into_iter().flatten() {
            if !place.trim().is_empty() {
                entities.push(EntityRef {
                    entity_type: "Place".into(),
                    name: place.trim().to_string(),
                    attributes: Vec::new(),
                    role: "location".into(),
                });
            }
        }
    }
    if let Some(driver) = d.driver.as_ref().filter(|_| d.role == "passenger" || d.aboard == Aboard::No) {
        entities.push(EntityRef {
            entity_type: "Person".into(),
            name: driver.clone(),
            attributes: Vec::new(),
            role: "driver".into(),
        });
    }
    EpisodeDraft {
        source: "trips".into(),
        source_ref: String::new(),
        kind: if d.aboard == Aboard::Yes { "trip".into() } else { "vehicle_drive".into() },
        title,
        body,
        event_time: EventTime {
            utc: Some(drive.t_start.clone()),
            tz_offset_minutes: Some(tz),
            uncertainty: TimeUncertainty::Minute,
        },
        trust: "self".into(),
        scope: Some(if d.aboard == Aboard::Yes { mine_scope.to_string() } else { profile.others_scope.clone() }),
        entities,
        edges: Vec::new(),
    }
}

/// A numeric fingerprint of a day's minted output — same inputs, same
/// digits, same refs on every node.
fn fingerprint(drafts: &[EpisodeDraft]) -> String {
    let mut basis = String::new();
    for d in drafts {
        basis.push_str(&format!("{}\u{1f}{}\u{1f}{}\u{1f}{:?}\u{1e}", d.kind, d.title, d.body, d.scope));
    }
    let hex = hearthloom_core::ids::content_hash(basis.as_bytes());
    let n = u64::from_str_radix(&hex.chars().filter(|c| c.is_ascii_hexdigit()).take(12).collect::<String>(), 16)
        .unwrap_or(0);
    format!("{}", 2 + n % 999_999_999)
}

fn question_summary(drive: &StoredMovement, profile: &VehicleProfile) -> String {
    let tz = drive.tz_offset_minutes;
    let (s, e) = (unix(&drive.t_start), unix(&drive.t_end));
    let day = chrono::DateTime::from_timestamp(s + tz as i64 * 60, 0)
        .map(|d| d.format("%a %b %-d").to_string())
        .unwrap_or_default();
    let from = place_label(&drive.start_place, drive.start_lat, drive.start_lon);
    let to = place_label(&drive.end_place, drive.end_lat, drive.end_lon);
    let route = match (from, to) {
        (Some(a), Some(b)) => format!(" \u{00b7} {a} \u{2192} {b}"),
        _ => String::new(),
    };
    let km = drive.distance_m.map(|m| format!(" \u{00b7} {:.1} km", m / 1000.0)).unwrap_or_default();
    format!(
        "Were you in the {} on {day}, {}\u{2013}{}{route}{km}?",
        vehicle_name(profile),
        fmt_hhmm(s, tz),
        fmt_hhmm(e, tz)
    )
}

pub(crate) fn finalize_trips(state: &AppState) -> anyhow::Result<(usize, usize, usize)> {
    let store = &state.store;
    let tz = store.movements_latest_tz_offset()?;
    let today_local = chrono::DateTime::from_timestamp(chrono::Utc::now().timestamp() + tz as i64 * 60, 0)
        .map(|d| d.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| chrono::Utc::now().format("%Y-%m-%d").to_string());
    finalize_trips_inner(store, &state.home, &today_local)
}

/// AppState-free core so tests drive it against a temp store.
/// Returns (days, episodes minted, phone trips absorbed).
pub(crate) fn finalize_trips_inner(
    store: &hearthloom_core::store::Store,
    home: &hearthloom_core::paths::Home,
    today_local: &str,
) -> anyhow::Result<(usize, usize, usize)> {
    let actor = Actor::new("agent:trip-reconcile");
    let mine_scope = crate::visits::device_scope(store);
    let (mut days_n, mut minted, mut absorbed_n) = (0usize, 0usize, 0usize);
    for day in store.movement_days_needing_trips(today_local)? {
        let movements = store.movements_for_day(&day)?;
        let drives: Vec<&StoredMovement> = movements
            .iter()
            .filter(|m| m.subject.starts_with("vehicle:") && m.mode == "drive")
            .collect();
        let phone: Vec<StoredMovement> = movements.iter().filter(|m| m.subject == "me").cloned().collect();
        let dwells: Vec<StoredObservation> = store
            .observations_for_day(&day)?
            .into_iter()
            .filter(|o| o.source != "trips" && o.t_end.is_some())
            .collect();
        let since = chrono::NaiveDate::parse_from_str(&day, "%Y-%m-%d")
            .map(|d| (d - chrono::Duration::days(PHONE_FEED_DAYS)).format("%Y-%m-%dT00:00:00Z").to_string())
            .unwrap_or_default();
        let phone_feed = store.phone_motion_since(&since)?;

        let mut used: std::collections::HashSet<String> = Default::default();
        let mut decided: Vec<(&StoredMovement, Decision, VehicleProfile)> = Vec::new();
        for drive in &drives {
            let profile = match store.vehicle_profile(&drive.subject)? {
                Some(p) => p,
                None => {
                    store.vehicle_profile_ensure(&drive.subject, "", &json!({}))?;
                    store.vehicle_profile(&drive.subject)?.expect("just ensured")
                }
            };
            let answer = store.trip_answer(&drive.id)?;
            let d = decide(drive, &phone, &dwells, &profile, answer.as_deref(), phone_feed, &used);
            used.extend(d.absorbed.iter().cloned());
            decided.push((drive, d, profile));
        }

        let phone_by_id: std::collections::HashMap<&str, &StoredMovement> =
            phone.iter().map(|p| (p.id.as_str(), p)).collect();
        let mut drafts: Vec<EpisodeDraft> = decided
            .iter()
            .map(|(drive, d, profile)| {
                let srcs: Vec<String> = d
                    .absorbed
                    .iter()
                    .filter_map(|id| phone_by_id.get(id.as_str()).map(|p| p.source.clone()))
                    .collect();
                drive_draft(drive, d, profile, &srcs, &mine_scope)
            })
            .collect();
        let fp = fingerprint(&drafts);
        for (idx, draft) in drafts.iter_mut().enumerate() {
            draft.source_ref = format!("trip:{day}:{idx}#r{fp}");
        }
        let intended: Vec<String> = drafts
            .iter()
            .map(|d| hearthloom_core::ids::episode_id("trips", &d.source_ref))
            .collect();
        if !drafts.is_empty() {
            let report = store.apply_batch(
                home,
                &IngestBatch { actor: actor.actor.clone(), session_id: None, episodes: drafts.clone() },
            )?;
            minted += report.episodes_new;
        }
        store.assert_trip_day(&day, &intended)?;

        // the phone's own trip episodes these drives absorbed
        let mut pairs = Vec::new();
        for ((_, d, _), trip_id) in decided.iter().zip(&intended) {
            for seg in &d.absorbed {
                if let Some(p) = phone_by_id.get(seg.as_str()) {
                    pairs.push((hearthloom_core::ids::episode_id(&p.source, &p.source_ref), trip_id.clone()));
                }
            }
        }
        absorbed_n += store.repoint_phone_trips(&day, &pairs)?;

        // your drives' endpoints → location observations (visits learn
        // where you parked); drop the ones no longer yours
        let mut keep = Vec::new();
        let mut obs = Vec::new();
        for (drive, d, _) in &decided {
            if d.aboard != Aboard::Yes {
                continue;
            }
            for (end, lat, lon, place, at) in [
                ("start", drive.start_lat, drive.start_lon, &drive.start_place, &drive.t_start),
                ("end", drive.end_lat, drive.end_lon, &drive.end_place, &drive.t_end),
            ] {
                if let (Some(lat), Some(lon)) = (lat, lon) {
                    let key = format!("{}:{end}", drive.id);
                    keep.push(key.clone());
                    obs.push(ObservationIn {
                        key,
                        lat,
                        lon,
                        accuracy_m: Some(30.0),
                        t_start: at.clone(),
                        t_end: None,
                        tz_offset_minutes: drive.tz_offset_minutes,
                        place: place.clone(),
                    });
                }
            }
        }
        if !obs.is_empty() {
            store.observations_add("trips", &actor.actor, &obs)?;
        }
        let stale: Vec<String> = store
            .trip_observation_keys(&day)?
            .into_iter()
            .filter(|k| !keep.contains(k))
            .collect();
        store.delete_trip_observations(&stale)?;

        // ask about the drives the evidence can't settle
        for (drive, d, profile) in &decided {
            if d.aboard == Aboard::Unknown && profile.ask {
                store.trip_question_open(&drive.id, &drive.subject, &day, &question_summary(drive, profile))?;
            }
        }

        store.set_trip_day_state(&day, movements.len() as i64, &fp, &intended)?;
        days_n += 1;
        std::thread::sleep(std::time::Duration::from_millis(40));
    }
    Ok((days_n, minted, absorbed_n))
}

#[cfg(test)]
mod tests {
    use super::*;
    use hearthloom_core::paths::Home;
    use hearthloom_core::store::{MovementIn, Store};

    fn node() -> (Store, Home, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().to_path_buf());
        home.init().unwrap();
        let store = Store::open(&home.db_path()).unwrap();
        (store, home, tmp)
    }

    const CAR: &str = "vehicle:htesla:5YJ";

    fn drive(key: &str, start: &str, end: &str) -> MovementIn {
        MovementIn {
            key: key.into(),
            mode: "drive".into(),
            start: start.into(),
            end: end.into(),
            tz_offset_minutes: -240,
            start_lat: Some(42.3601),
            start_lon: Some(-71.0589),
            end_lat: Some(42.3736),
            end_lon: Some(-71.1097),
            start_place: Some("Home".into()),
            end_place: Some("Harvard Square".into()),
            distance_m: Some(6100.0),
            attrs: Some(json!({"energy_kwh": 0.9, "media": [{"title": "So What", "artist": "Miles Davis"}]})),
        }
    }

    fn phone(key: &str, mode: &str, start: &str, end: &str) -> MovementIn {
        MovementIn {
            key: key.into(),
            mode: mode.into(),
            start: start.into(),
            end: end.into(),
            tz_offset_minutes: -240,
            start_lat: None,
            start_lon: None,
            end_lat: None,
            end_lon: None,
            start_place: None,
            end_place: None,
            distance_m: Some(6000.0),
            attrs: None,
        }
    }

    fn setup_car(store: &Store) {
        store.vehicle_profile_ensure(CAR, "Model Y", &json!({"make": "Tesla"})).unwrap();
    }

    fn live(store: &Store, kind: &str) -> Vec<(String, String)> {
        let (_, rows) = store
            .read_query(
                &format!(
                    "SELECT title, scope FROM episodes WHERE source = 'trips' AND kind = '{kind}'
                     AND superseded_by IS NULL ORDER BY title"
                ),
                50,
            )
            .unwrap();
        rows.iter()
            .map(|r| (r[0].as_str().unwrap().to_string(), r[1].as_str().unwrap_or("").to_string()))
            .collect()
    }

    /// read_query shadows superseded episodes, so "live" = still visible.
    fn phone_trip_live(store: &Store, key: &str) -> bool {
        let (_, rows) = store
            .read_query(
                &format!("SELECT COUNT(*) FROM episodes WHERE source = 'ios-motion' AND source_ref = '{key}'"),
                5,
            )
            .unwrap();
        rows[0][0].as_i64() == Some(1)
    }

    /// Mint the phone's own trip episode the way /v1/ingest/motion does.
    fn phone_trip_episode(store: &Store, home: &Home, key: &str, start: &str, end: &str) {
        let m = crate::live_ingest::MotionIn {
            key: key.into(),
            mode: "automotive".into(),
            start: start.into(),
            end: end.into(),
            distance_m: Some(6000.0),
            steps: None,
            tz_offset_minutes: -240,
        };
        let draft = crate::live_ingest::motion_in_to_episode(&m, "personal").unwrap();
        store
            .apply_batch(home, &IngestBatch { actor: "device:phone".into(), session_id: None, episodes: vec![draft] })
            .unwrap();
    }

    #[test]
    fn test_phone_in_vehicle_makes_it_your_trip_and_absorbs_the_phone_trip() {
        let (store, home, _t) = node();
        setup_car(&store);
        store.movements_add("plugin:htesla", CAR, "plugin:htesla", &[drive("d1", "2026-10-20T12:10:00Z", "2026-10-20T12:32:00Z")]).unwrap();
        store.movements_add("ios-motion", "me", "device:phone", &[phone("m1", "automotive", "2026-10-20T12:11:00Z", "2026-10-20T12:31:00Z")]).unwrap();
        phone_trip_episode(&store, &home, "m1", "2026-10-20T12:11:00Z", "2026-10-20T12:31:00Z");
        let (days, minted, absorbed) = finalize_trips_inner(&store, &home, "2026-10-22").unwrap();
        assert_eq!((days, minted, absorbed), (1, 1, 1));
        let trips = live(&store, "trip");
        assert_eq!(trips.len(), 1, "{trips:?}");
        assert!(trips[0].0.starts_with("Drive \u{2014} Home \u{2192} Harvard Square, 6.1 km"), "{trips:?}");
        assert_eq!(trips[0].1, "personal");
        assert!(!phone_trip_live(&store, "m1"), "the phone's own trip is absorbed");
        // endpoints became location observations for visits
        assert_eq!(store.trip_observation_keys("2026-10-20").unwrap().len(), 2);
        // idempotent
        assert_eq!(finalize_trips_inner(&store, &home, "2026-10-22").unwrap(), (0, 0, 0));
    }

    #[test]
    fn test_phone_elsewhere_means_not_you_and_stays_off_your_map() {
        let (store, home, _t) = node();
        setup_car(&store);
        let mut p = store.vehicle_profile(CAR).unwrap().unwrap();
        p.household = vec!["Alex".into()];
        store.vehicle_profile_set(&p).unwrap();
        store.movements_add("plugin:htesla", CAR, "plugin:htesla", &[drive("d1", "2026-10-20T12:10:00Z", "2026-10-20T12:32:00Z")]).unwrap();
        store.movements_add("ios-motion", "me", "device:phone", &[phone("w1", "stationary", "2026-10-20T11:00:00Z", "2026-10-20T13:00:00Z")]).unwrap();
        finalize_trips_inner(&store, &home, "2026-10-22").unwrap();
        assert!(live(&store, "trip").is_empty());
        let other = live(&store, "vehicle_drive");
        assert_eq!(other.len(), 1);
        assert!(other[0].0.contains("driven by Alex"), "{other:?}");
        assert_eq!(other[0].1, "family", "others' drives take the car's scope");
        assert!(store.trip_observation_keys("2026-10-20").unwrap().is_empty(), "not your whereabouts");
        let (_, pts) = store.read_query("SELECT COUNT(*) FROM location_points", 5).unwrap();
        assert_eq!(pts[0][0].as_i64(), Some(0), "someone else's drive puts nothing on your Atlas");
    }

    #[test]
    fn test_unknown_asks_and_the_answer_reconciles() {
        let (store, home, _t) = node();
        setup_car(&store);
        store.movements_add("plugin:htesla", CAR, "plugin:htesla", &[drive("d1", "2026-10-20T12:10:00Z", "2026-10-20T12:32:00Z")]).unwrap();
        // a phone feed exists (another day), but says nothing about this drive
        store.movements_add("ios-motion", "me", "device:phone", &[phone("x", "walking", "2026-10-19T09:00:00Z", "2026-10-19T09:20:00Z")]).unwrap();
        finalize_trips_inner(&store, &home, "2026-10-22").unwrap();
        assert!(live(&store, "trip").is_empty());
        assert!(live(&store, "vehicle_drive")[0].0.starts_with("Model Y drove Home"));
        let qs = store.trip_questions_pending().unwrap();
        assert_eq!(qs.len(), 1);
        assert!(qs[0]["summary"].as_str().unwrap().starts_with("Were you in the Model Y on Tue Oct 20, 08:10\u{2013}08:32"), "{qs:?}");
        // yes → re-mints as your trip under a new fingerprint
        store.trip_question_answer(qs[0]["key"].as_str().unwrap(), "yes").unwrap();
        let (days, minted, _) = finalize_trips_inner(&store, &home, "2026-10-22").unwrap();
        assert_eq!((days, minted), (1, 1));
        assert_eq!(live(&store, "trip").len(), 1);
        assert!(live(&store, "vehicle_drive").is_empty(), "the unknown version is superseded");
        assert_eq!(store.trip_questions_pending_count().unwrap(), 0);
    }

    #[test]
    fn test_no_phone_feed_uses_the_car_default_and_guest_mode_is_never_you() {
        let (store, home, _t) = node();
        setup_car(&store);
        let mut guest = drive("d2", "2026-10-20T18:00:00Z", "2026-10-20T18:20:00Z");
        guest.attrs = Some(json!({"guest_mode": true}));
        store.movements_add("plugin:htesla", CAR, "plugin:htesla", &[drive("d1", "2026-10-20T12:10:00Z", "2026-10-20T12:32:00Z"), guest]).unwrap();
        finalize_trips_inner(&store, &home, "2026-10-22").unwrap();
        assert_eq!(live(&store, "trip").len(), 1, "no phone data: the default says you");
        assert_eq!(live(&store, "vehicle_drive").len(), 1, "guest mode: not you");
        assert_eq!(store.trip_questions_pending_count().unwrap(), 0, "nothing to ask");
    }

    #[test]
    fn test_settings_change_flips_and_releases_the_phone_trip() {
        let (store, home, _t) = node();
        setup_car(&store);
        store.movements_add("plugin:htesla", CAR, "plugin:htesla", &[drive("d1", "2026-10-20T12:10:00Z", "2026-10-20T12:32:00Z")]).unwrap();
        store.movements_add("ios-motion", "me", "device:phone", &[phone("m1", "automotive", "2026-10-20T12:11:00Z", "2026-10-20T12:31:00Z")]).unwrap();
        phone_trip_episode(&store, &home, "m1", "2026-10-20T12:11:00Z", "2026-10-20T12:31:00Z");
        finalize_trips_inner(&store, &home, "2026-10-22").unwrap();
        assert!(!phone_trip_live(&store, "m1"));
        // "this car is never me" (e.g. it's my teenager's): the phone was in
        // ANOTHER vehicle, so its own trip comes back
        let mut p = store.vehicle_profile(CAR).unwrap().unwrap();
        p.attribution = "never_me".into();
        store.vehicle_profile_set(&p).unwrap();
        finalize_trips_inner(&store, &home, "2026-10-22").unwrap();
        assert!(live(&store, "trip").is_empty());
        assert_eq!(live(&store, "vehicle_drive").len(), 1);
        assert!(phone_trip_live(&store, "m1"), "released back to the phone");
        assert!(store.trip_observation_keys("2026-10-20").unwrap().is_empty());
    }

    #[test]
    fn test_two_nodes_converge_and_a_stale_peer_revision_cannot_win() {
        let (a, ha, _ta) = node();
        let (b, hb, _tb) = node();
        for s in [&a, &b] {
            setup_car(s);
            s.movements_add("plugin:htesla", CAR, "plugin:htesla", &[drive("d1", "2026-10-20T12:10:00Z", "2026-10-20T12:32:00Z")]).unwrap();
            s.movements_add("ios-motion", "me", "device:phone", &[phone("x", "walking", "2026-10-19T09:00:00Z", "2026-10-19T09:20:00Z")]).unwrap();
        }
        // b reconciles BEFORE learning the answer (stale), a after
        finalize_trips_inner(&b, &hb, "2026-10-22").unwrap();
        a.trip_question_open("plugin:htesla|d1", CAR, "2026-10-20", "q").unwrap();
        a.trip_question_answer("plugin:htesla|d1", "yes").unwrap();
        finalize_trips_inner(&a, &ha, "2026-10-22").unwrap();
        let ref_of = |s: &Store| {
            let (_, r) = s
                .read_query("SELECT source_ref FROM episodes WHERE source = 'trips' AND superseded_by IS NULL", 5)
                .unwrap();
            r.iter().map(|x| x[0].as_str().unwrap().to_string()).collect::<Vec<_>>()
        };
        let a_ref = ref_of(&a);
        // b's stale episode lands on a (episode sync)
        let (_, rows) = b
            .read_query("SELECT title, kind, content_ref, source_ref FROM episodes WHERE source = 'trips'", 5)
            .unwrap();
        let stale = EpisodeDraft {
            source: "trips".into(),
            source_ref: rows[0][3].as_str().unwrap().into(),
            kind: rows[0][1].as_str().unwrap().into(),
            title: rows[0][0].as_str().unwrap().into(),
            body: std::fs::read_to_string(hb.root().join(rows[0][2].as_str().unwrap())).unwrap(),
            event_time: EventTime { utc: Some("2026-10-20T12:10:00Z".into()), tz_offset_minutes: Some(-240), uncertainty: TimeUncertainty::Minute },
            trust: "self".into(),
            scope: Some("family".into()),
            entities: Vec::new(),
            edges: Vec::new(),
        };
        a.apply_batch(&ha, &IngestBatch { actor: "peer".into(), session_id: None, episodes: vec![stale] }).unwrap();
        assert_ne!(ref_of(&a), a_ref, "the stale landing superseded a's episode…");
        finalize_trips_inner(&a, &ha, "2026-10-22").unwrap();
        assert_eq!(ref_of(&a), a_ref, "…and the pass re-asserted the right one");
        // b learns the answer (synced) and converges on the same ref
        b.vehicles_apply(&a.vehicles_since("", 100).unwrap()).unwrap();
        finalize_trips_inner(&b, &hb, "2026-10-22").unwrap();
        assert_eq!(ref_of(&b), a_ref, "same inputs, same refs on both nodes");
    }
}
