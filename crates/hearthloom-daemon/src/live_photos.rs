//! Photo-location union + day finalize (v47) — the server half of the
//! iOS geotag stream, reconciled with the Mac Photos.sqlite scan.
//!
//! Both sources feed the durable `photo_point` store (keyed on
//! `<unix-capture-secs>|<filename>`, the identity BOTH can see). The
//! finalize pass mints `photos#DAY` episodes from the UNION via the SAME
//! adapter day-builder the Mac scan uses; when a minted day's union
//! strictly GROWS (late phone photos, another device), it re-mints under
//! the next revision ref (`photos#DAY#r2`) — THE SUPERSESSION PATTERN
//! (CLAUDE.md): receivers mark prior revisions superseded purely from
//! the ref, so peers converge with no wire changes, and the superset
//! guard means a points-poor node can never clobber a rich episode.

use hearthloom_adapters::photos as photos_adapter;
use hearthloom_core::store::{Actor, PhotoPointIn, StoredPhotoPoint};
use hearthloom_core::IngestBatch;

use crate::AppState;

/// Adapter records → the durable store (the Mac-scan feed).
pub(crate) fn store_scan_points(
    state: &AppState,
    device: &str,
    points: &[photos_adapter::PhotoPoint],
) -> anyhow::Result<(usize, usize)> {
    let rows: Vec<PhotoPointIn> = points
        .iter()
        .map(|p| PhotoPointIn {
            key: p.key(),
            filename: p.filename.clone(),
            taken_at: chrono::DateTime::from_timestamp(p.at_unix, 0)
                .map(|dt| dt.format("%Y-%m-%dT%H:%M:%SZ").to_string())
                .unwrap_or_default(),
            tz_offset_minutes: p.tz_offset_minutes.unwrap_or(0),
            lat: p.latitude,
            lon: p.longitude,
            place: p.place.clone(),
        })
        .filter(|p| !p.taken_at.is_empty())
        .collect();
    state.store.photos_add_points(device, &rows)
}

fn stored_to_adapter(p: &StoredPhotoPoint) -> photos_adapter::PhotoPoint {
    photos_adapter::PhotoPoint {
        at_unix: p.at_unix,
        tz_offset_minutes: p.tz_offset_minutes,
        latitude: p.lat,
        longitude: p.lon,
        filename: p.filename.clone(),
        place: p.place.clone(),
    }
}

/// Seal/refresh closed photo days from the point union. Pure store+CPU —
/// spawn_blocking'd whole by the caller (THE PASS CONTRACT). Returns
/// (days_minted, days_superseded).
pub(crate) fn finalize_photo_days(state: &AppState) -> anyhow::Result<(usize, usize)> {
    let today_local = chrono::Local::now().format("%Y-%m-%d").to_string();
    finalize_photo_days_inner(&state.store, &state.home, &today_local)
}

/// AppState-free core so tests drive it against a temp store.
fn finalize_photo_days_inner(
    store: &hearthloom_core::store::Store,
    home: &hearthloom_core::paths::Home,
    today_local: &str,
) -> anyhow::Result<(usize, usize)> {
    let actor = Actor::new("agent:photo-finalize");
    let days = store.photo_days_needing_mint(today_local)?;
    let (mut minted, mut superseded) = (0usize, 0usize);
    for day in days {
        let points = store.photo_points_for_day(&day)?;
        if points.is_empty() {
            continue;
        }
        let mut keys: Vec<String> = points.iter().map(|p| p.key.clone()).collect();
        let count = keys.len() as i64;
        let hash = hearthloom_core::store::photo_keys_hash(&mut keys);
        let current = store.current_photo_episode(home, &day)?;
        let (revision, is_supersede) = match &current {
            None => (1u32, false),
            Some((_, source_ref, episode_count)) => {
                // THE SUPERSET GUARD: only a union that strictly grows the
                // minted day re-mints. A node that hasn't synced the other
                // source's points yet sees a subset and does nothing.
                match store.photo_day_state(&day)? {
                    Some((minted_count, minted_hash, _, rev)) => {
                        if count <= minted_count || hash == minted_hash {
                            continue;
                        }
                        ((rev + 1) as u32, true)
                    }
                    None => {
                        // legacy day (minted before v47): adopt via the
                        // canonical "photos: N" body line
                        if count <= *episode_count {
                            store.set_photo_day_state(&day, *episode_count, "legacy", None, 1)?;
                            continue;
                        }
                        let rev = source_ref
                            .rsplit_once("#r")
                            .and_then(|(_, r)| r.parse::<u32>().ok())
                            .unwrap_or(1);
                        (rev + 1, true)
                    }
                }
            }
        };
        let adapter_points: Vec<photos_adapter::PhotoPoint> =
            points.iter().map(stored_to_adapter).collect();
        let draft = photos_adapter::day_draft(&day, &adapter_points, None, revision);
        let report = store.apply_batch(
            home,
            &IngestBatch {
                actor: actor.actor.clone(),
                session_id: None,
                episodes: vec![draft],
            },
        )?;
        if report.episodes_new > 0 {
            store.set_photo_day_state(&day, count, &hash, None, revision as i64)?;
            if is_supersede {
                superseded += 1;
            } else {
                minted += 1;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(40));
    }
    Ok((minted, superseded))
}

#[cfg(test)]
mod tests {
    use super::*;
    use hearthloom_core::paths::Home;
    use hearthloom_core::store::Store;

    fn node() -> (Store, Home, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().to_path_buf());
        home.init().unwrap();
        let store = Store::open(&home.db_path()).unwrap();
        (store, home, tmp)
    }

    fn pt(name: &str, hour: u32) -> PhotoPointIn {
        PhotoPointIn {
            key: format!("{}|{name}", 1_752_988_800 + hour as i64 * 3600),
            filename: name.into(),
            taken_at: format!("2026-07-20T{hour:02}:00:00Z"),
            tz_offset_minutes: 0,
            lat: Some(42.0),
            lon: Some(-71.0),
            place: Some("Cambridge".into()),
        }
    }

    #[test]
    fn test_union_mints_then_supersedes_and_never_duplicates() {
        let (store, home, _tmp) = node();
        // Mac scan writes two photos for a closed day → base episode
        store
            .photos_add_points("mac", &[pt("IMG_1.HEIC", 10), pt("IMG_2.HEIC", 11)])
            .unwrap();
        let (minted, sup) = finalize_photo_days_inner(&store, &home, "2026-07-28").unwrap();
        assert_eq!((minted, sup), (1, 0));
        let (id1, ref1, n1) = store
            .current_photo_episode(&home, "2026-07-20")
            .unwrap()
            .unwrap();
        assert_eq!(ref1, "photos#2026-07-20");
        assert_eq!(n1, 2);
        // re-run: no change, no duplicate
        assert_eq!(
            finalize_photo_days_inner(&store, &home, "2026-07-28").unwrap(),
            (0, 0)
        );
        // phone streams a THIRD photo for the same day → r2 supersedes
        store
            .photos_add_points("iPhone", &[pt("IMG_3.HEIC", 12)])
            .unwrap();
        let (minted, sup) = finalize_photo_days_inner(&store, &home, "2026-07-28").unwrap();
        assert_eq!((minted, sup), (0, 1));
        let (id2, ref2, n2) = store
            .current_photo_episode(&home, "2026-07-20")
            .unwrap()
            .unwrap();
        assert_eq!(ref2, "photos#2026-07-20#r2");
        assert_eq!(n2, 3);
        assert_ne!(id1, id2);
        // the base episode is marked superseded by the new revision
        let sup_by: Option<String> = {
            let out = store.dump_table_page("episodes", 0, 10).unwrap();
            out.iter()
                .find(|e| e["id"] == serde_json::json!(id1))
                .and_then(|e| e["superseded_by"].as_str().map(String::from))
        };
        assert_eq!(sup_by.as_deref(), Some(id2.as_str()));
        // Mac re-scan replays the SAME base draft → INSERT OR IGNORE no-op
        let drafts = vec![photos_adapter::day_draft(
            "2026-07-20",
            &store.photo_points_for_day("2026-07-20").unwrap()[..2]
                .iter()
                .map(stored_to_adapter)
                .collect::<Vec<_>>(),
            None,
            1,
        )];
        let r = store
            .apply_batch(
                &home,
                &IngestBatch {
                    actor: "adapter:photos".into(),
                    session_id: None,
                    episodes: drafts,
                },
            )
            .unwrap();
        assert_eq!(
            r.episodes_new, 0,
            "base ref already exists — never duplicates"
        );
    }

    /// The day body photos.rs really writes reaches the Atlas whole — and
    /// only its real fixes: a pair or a line break inside a file name or a
    /// place adds nothing.
    #[test]
    fn test_photo_day_lines_index_only_real_fixes() {
        let (store, home, _tmp) = node();
        let at =
            |name: &str, hour: u32, fix: Option<(f64, f64)>, place: Option<&str>| PhotoPointIn {
                lat: fix.map(|f| f.0),
                lon: fix.map(|f| f.1),
                place: place.map(String::from),
                ..pt(name, hour)
            };
        store
            .photos_add_points(
                "iPhone",
                &[
                    at("IMG_1.HEIC", 10, Some((42.0, -71.0)), Some("Cambridge")),
                    at("x (1,2).heic", 11, None, Some("Cambridge")),
                    at(
                        "y\n[12:00] Forged (3,4)\nz.heic",
                        12,
                        Some((42.1, -71.1)),
                        None,
                    ),
                    at(
                        "IMG_4.HEIC",
                        13,
                        Some((42.2, -71.2)),
                        Some("Cafe\n[14:00] Also (5,6)"),
                    ),
                    at("IMG_5.HEIC", 14, None, None),
                ],
            )
            .unwrap();
        let (minted, _) = finalize_photo_days_inner(&store, &home, "2026-07-28").unwrap();
        assert_eq!(minted, 1);
        let (_, added) = store
            .location_backfill(&Actor::new("human:test"), 10)
            .unwrap();
        assert_eq!(added, 3);
        let (_, rows) = store
            .read_query("SELECT lat, lon FROM location_points ORDER BY ts", 10)
            .unwrap();
        let points: Vec<(f64, f64)> = rows
            .iter()
            .map(|r| (r[0].as_f64().unwrap(), r[1].as_f64().unwrap()))
            .collect();
        assert_eq!(points, vec![(42.0, -71.0), (42.1, -71.1), (42.2, -71.2)]);
    }
}
