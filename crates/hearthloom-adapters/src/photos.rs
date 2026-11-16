//! Apple Photos adapter: `Photos.sqlite` inside the Photos library —
//! Jon's Timeline substitute. Every asset carries a timestamp, usually a
//! GPS fix, a per-photo timezone offset, and Apple's own reverse-geocoded
//! "moment" title (e.g. "Cambridge"), so photo history doubles as a
//! location trace without decoding a single image.
//!
//! One episode per local day; moment titles become Place entities
//! (role `visited`) — day-view recall answers "where was I?".

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{anyhow, Context, Result};
use chrono::DateTime;
use hearthloom_core::{one_line, EntityRef, EpisodeDraft, EventTime, TimeUncertainty};
use rusqlite::{Connection, OpenFlags};

const BODY_CAP: usize = 64 * 1024;
/// Apple epoch (2001-01-01T00:00:00Z) as Unix seconds.
const APPLE_EPOCH: i64 = 978_307_200;

/// One geotagged photo record. `key()` — `<unix-capture-secs>|<filename>` —
/// is the CROSS-DEVICE identity: PHAsset.localIdentifier is not stable
/// between the Mac Photos library and the phone, but capture time +
/// original filename are visible to both, so the Mac scan and the iOS
/// photo-location stream UNION on it instead of duplicating (v47).
/// `filename` is therefore the ORIGINAL name (`IMG_1234.HEIC`); the Mac
/// library keeps the file under a name of its own (`<uuid>.heic`).
#[derive(Debug, Clone)]
pub struct PhotoPoint {
    pub at_unix: i64,
    pub tz_offset_minutes: Option<i32>,
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
    pub filename: String,
    pub place: Option<String>,
    /// The library's own name for the file, when it differs from
    /// `filename`: what earlier versions' Mac scans keyed the photo by.
    pub stored_name: Option<String>,
}

impl PhotoPoint {
    pub fn key(&self) -> String {
        format!("{}|{}", self.at_unix, self.filename)
    }
    /// The key earlier versions' Mac scans gave this photo, when it
    /// differs from [`key`](Self::key).
    pub fn earlier_key(&self) -> Option<String> {
        self.stored_name
            .as_ref()
            .map(|name| format!("{}|{name}", self.at_unix))
    }
    /// Local calendar day this photo buckets to.
    pub fn day(&self) -> Option<String> {
        let local = self.at_unix + self.tz_offset_minutes.unwrap_or(0) as i64 * 60;
        DateTime::from_timestamp(local, 0).map(|dt| dt.format("%Y-%m-%d").to_string())
    }
}

type Photo = PhotoPoint;

/// Default library path: `~/Pictures/Photos Library.photoslibrary`.
pub fn default_library() -> Result<std::path::PathBuf> {
    let home = std::env::var("HOME").map_err(|_| anyhow!("HOME is not set"))?;
    Ok(Path::new(&home).join("Pictures/Photos Library.photoslibrary"))
}

pub fn ingest(library: &Path, scope: Option<&str>) -> Result<Vec<EpisodeDraft>> {
    day_drafts(scan_points(library)?, scope)
        .ok_or_else(|| anyhow!("no dated photos found in {}", db_path(library).display()))
}

/// The day episodes of points one [`scan_points`] read (what [`ingest`]
/// returns), so a caller that also stores the points scans once. `None`
/// when there are none.
pub fn day_drafts(photos: Vec<PhotoPoint>, scope: Option<&str>) -> Option<Vec<EpisodeDraft>> {
    if photos.is_empty() {
        return None;
    }
    Some(
        bucket_days(photos)
            .into_iter()
            .map(|(date, photos)| day_draft(&date, &photos, scope, 1))
            .collect(),
    )
}

fn db_path(library: &Path) -> std::path::PathBuf {
    if library.extension().map(|e| e == "sqlite").unwrap_or(false) {
        library.to_path_buf()
    } else {
        library.join("database/Photos.sqlite")
    }
}

/// All geotag points from a Photos.sqlite — the per-photo records the
/// daemon stores durably (v47) so Mac and phone streams union.
pub fn scan_points(library: &Path) -> Result<Vec<PhotoPoint>> {
    let db_path = db_path(library);
    let conn = Connection::open_with_flags(
        &db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| {
        format!(
            "opening {} read-only (needs Full Disk Access for this terminal)",
            db_path.display()
        )
    })?;
    scan_points_from(&conn)
}

fn scan_points_from(conn: &Connection) -> Result<Vec<PhotoPoint>> {
    Ok(query_photos(conn)?
        .into_iter()
        // Zeroed/corrupt ZDATECREATED yields pre-digital dates; drop them.
        .filter(|p| p.at_unix >= 631_152_000)
        .collect())
}

/// Bundle per *local* day using each photo's own timezone offset.
pub fn bucket_days(photos: Vec<PhotoPoint>) -> BTreeMap<String, Vec<PhotoPoint>> {
    let mut buckets: BTreeMap<String, Vec<PhotoPoint>> = BTreeMap::new();
    for photo in photos {
        let Some(day) = photo.day() else { continue };
        buckets.entry(day).or_default().push(photo);
    }
    buckets
}

/// One day's episode. `revision` 1 = the base ref `photos#DAY`; higher
/// revisions get `photos#DAY#r<n>` — the SUPERSESSION convention: a
/// fuller union re-mints under the next revision and receivers mark
/// prior revisions superseded (deterministic, both peers converge).
pub fn day_draft(
    date: &str,
    photos: &[PhotoPoint],
    scope: Option<&str>,
    revision: u32,
) -> EpisodeDraft {
    {
        // File names and places go into `[HH:MM] file (lat,lon) — place`
        // lines the Atlas reads by shape, so each is `one_line`d: a file's
        // original name can come from someone else (a saved or received
        // image), and a line break in it must not start a line of its own.
        let place_of = |photo: &PhotoPoint| {
            photo
                .place
                .as_deref()
                .map(one_line)
                .filter(|p| !p.is_empty())
        };
        let mut places: Vec<String> = Vec::new();
        for photo in photos {
            if let Some(place) = place_of(photo) {
                if !places.contains(&place) {
                    places.push(place);
                }
            }
        }
        let title = if places.is_empty() {
            format!("{} photos", photos.len())
        } else {
            format!("Photos: {}", places.join(", "))
        };

        let mut body = format!("date: {date}\nphotos: {}\n", photos.len());
        if !places.is_empty() {
            body.push_str(&format!("places: {}\n", places.join(", ")));
        }
        body.push('\n');
        for photo in photos {
            let local = photo.at_unix + photo.tz_offset_minutes.unwrap_or(0) as i64 * 60;
            let Some(dt) = DateTime::from_timestamp(local, 0) else {
                continue;
            };
            let mut line = format!("[{}] {}", dt.format("%H:%M"), one_line(&photo.filename));
            if let (Some(lat), Some(lon)) = (photo.latitude, photo.longitude) {
                line.push_str(&format!(" ({lat:.4},{lon:.4})"));
            }
            if let Some(place) = place_of(photo) {
                line.push_str(&format!(" — {place}"));
            }
            line.push('\n');
            if body.len() + line.len() > BODY_CAP {
                body.push_str("[truncated]\n");
                break;
            }
            body.push_str(&line);
        }

        let first = &photos[0];
        let event_time = EventTime {
            utc: DateTime::from_timestamp(first.at_unix, 0)
                .map(|dt| dt.format("%Y-%m-%dT%H:%M:%SZ").to_string()),
            tz_offset_minutes: first.tz_offset_minutes,
            uncertainty: if first.tz_offset_minutes.is_some() {
                TimeUncertainty::Exact
            } else {
                TimeUncertainty::Hour
            },
        };
        let entities = places
            .iter()
            .map(|place| EntityRef {
                entity_type: "Place".into(),
                name: place.clone(),
                attributes: vec![],
                role: "visited".into(),
            })
            .collect();

        EpisodeDraft {
            source: "photos".into(),
            source_ref: if revision <= 1 {
                format!("photos#{date}")
            } else {
                format!("photos#{date}#r{revision}")
            },
            kind: "photos".into(),
            title,
            body,
            event_time,
            trust: "self".into(),
            scope: scope.map(String::from),
            entities,
            edges: vec![],
        }
    }
}

/// Query assets with graceful degradation: the moment join and the
/// additional-attributes join exist on current macOS but not guaranteed
/// everywhere; fall back to plainer queries rather than fail. The file
/// name is the original one (`ZADDITIONALASSETATTRIBUTES.ZORIGINALFILENAME`,
/// what the phone reports too) where the library records it, else the
/// library's own (`ZASSET.ZFILENAME`).
fn query_photos(conn: &Connection) -> Result<Vec<Photo>> {
    let has_original: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('ZADDITIONALASSETATTRIBUTES')
                           WHERE name = 'ZORIGINALFILENAME')",
            [],
            |r| r.get(0),
        )
        .unwrap_or(false);
    let original = if has_original {
        "aa.ZORIGINALFILENAME"
    } else {
        "NULL"
    };
    let queries = [
        // Full: timezone offsets + reverse-geocoded moment titles.
        format!(
            "SELECT a.ZDATECREATED, a.ZLATITUDE, a.ZLONGITUDE, a.ZFILENAME,
                    aa.ZTIMEZONEOFFSET, m.ZTITLE, {original}
             FROM ZASSET a
             LEFT JOIN ZADDITIONALASSETATTRIBUTES aa ON aa.Z_PK = a.ZADDITIONALATTRIBUTES
             LEFT JOIN ZMOMENT m ON m.Z_PK = a.ZMOMENT
             WHERE a.ZTRASHEDSTATE = 0 AND a.ZDATECREATED IS NOT NULL
             ORDER BY a.ZDATECREATED"
        ),
        // No ZMOMENT table.
        format!(
            "SELECT a.ZDATECREATED, a.ZLATITUDE, a.ZLONGITUDE, a.ZFILENAME,
                    aa.ZTIMEZONEOFFSET, NULL, {original}
             FROM ZASSET a
             LEFT JOIN ZADDITIONALASSETATTRIBUTES aa ON aa.Z_PK = a.ZADDITIONALATTRIBUTES
             WHERE a.ZTRASHEDSTATE = 0 AND a.ZDATECREATED IS NOT NULL
             ORDER BY a.ZDATECREATED"
        ),
        // Bare minimum.
        "SELECT a.ZDATECREATED, a.ZLATITUDE, a.ZLONGITUDE, a.ZFILENAME, NULL, NULL, NULL
         FROM ZASSET a
         WHERE a.ZTRASHEDSTATE = 0 AND a.ZDATECREATED IS NOT NULL
         ORDER BY a.ZDATECREATED"
            .to_string(),
    ];

    let mut last_err = None;
    for sql in &queries {
        match try_query(conn, sql) {
            Ok(photos) => return Ok(photos),
            Err(err) => last_err = Some(err),
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow!("no queries attempted")))
}

fn try_query(conn: &Connection, sql: &str) -> Result<Vec<Photo>> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map([], |row| {
        let created: f64 = row.get(0)?;
        let latitude: Option<f64> = row.get(1)?;
        let longitude: Option<f64> = row.get(2)?;
        let stored: Option<String> = row.get::<_, Option<String>>(3)?.filter(|n| !n.is_empty());
        let tz_seconds: Option<i64> = row.get(4)?;
        let place: Option<String> = row.get(5)?;
        let original: Option<String> = row.get::<_, Option<String>>(6)?.filter(|n| !n.is_empty());
        let (filename, stored_name) = match (original, stored) {
            (Some(original), Some(stored)) if original != stored => (Some(original), Some(stored)),
            (Some(original), _) => (Some(original), None),
            (None, stored) => (stored, None),
        };
        Ok(Photo {
            at_unix: created as i64 + APPLE_EPOCH,
            tz_offset_minutes: tz_seconds.map(|s| (s / 60) as i32),
            // Photos stores -180.0/-180.0 when there is no GPS fix.
            latitude: latitude.filter(|l| (-90.0..=90.0).contains(l)),
            longitude: longitude
                .filter(|l| (-180.0..=180.0).contains(l))
                .filter(|_| {
                    latitude
                        .map(|l| (-90.0..=90.0).contains(&l))
                        .unwrap_or(false)
                }),
            filename: filename.unwrap_or_else(|| "(unnamed)".into()),
            place: place.filter(|p| !p.is_empty()),
            stored_name,
        })
    })?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(path: &Path) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE ZASSET (Z_PK INTEGER PRIMARY KEY, ZDATECREATED REAL,
                 ZLATITUDE REAL, ZLONGITUDE REAL, ZFILENAME TEXT,
                 ZTRASHEDSTATE INTEGER, ZMOMENT INTEGER, ZADDITIONALATTRIBUTES INTEGER);
             CREATE TABLE ZADDITIONALASSETATTRIBUTES (Z_PK INTEGER PRIMARY KEY,
                 ZTIMEZONEOFFSET INTEGER);
             CREATE TABLE ZMOMENT (Z_PK INTEGER PRIMARY KEY, ZTITLE TEXT);
             INSERT INTO ZADDITIONALASSETATTRIBUTES VALUES (1, -14400);
             INSERT INTO ZMOMENT VALUES (1, 'Cambridge');
             -- 795362400 apple-seconds ≈ 2026-03-15 14:00 UTC, offset -4h.
             INSERT INTO ZASSET VALUES (1, 795362400, 42.3736, -71.1097, 'IMG_1.HEIC', 0, 1, 1);
             INSERT INTO ZASSET VALUES (2, 795366000, -180.0, -180.0, 'IMG_2.HEIC', 0, 1, 1);
             -- trashed: excluded.
             INSERT INTO ZASSET VALUES (3, 795366000, 1.0, 1.0, 'GONE.HEIC', 1, 1, 1);
             -- next local day.
             INSERT INTO ZASSET VALUES (4, 795448800, 40.7128, -74.0060, 'IMG_3.HEIC', 0, NULL, 1);",
        )
        .unwrap();
    }

    #[test]
    fn test_day_bundles_and_places() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("Photos.sqlite");
        fixture(&db);

        let drafts = ingest(&db, None).unwrap();
        assert_eq!(drafts.len(), 2);

        let first = &drafts[0];
        assert_eq!(first.kind, "photos");
        assert!(first.title.contains("Cambridge"));
        assert!(first
            .body
            .contains("IMG_1.HEIC (42.3736,-71.1097) — Cambridge"));
        // -180 sentinel → no coordinates printed for IMG_2.
        assert!(first.body.contains("IMG_2.HEIC —") || first.body.contains("IMG_2.HEIC\n"));
        assert!(!first.body.contains("GONE.HEIC"));
        // Place entity for day-view "where was I".
        assert_eq!(first.entities[0].entity_type, "Place");
        assert_eq!(first.entities[0].name, "Cambridge");
        assert_eq!(first.entities[0].role, "visited");
        // Per-photo timezone honored.
        assert_eq!(first.event_time.tz_offset_minutes, Some(-240));
        assert_eq!(first.event_time.uncertainty, TimeUncertainty::Exact);
    }

    /// The library keeps a file under a name of its own; the phone knows
    /// the photo by its original name. A point is keyed — and shown — by
    /// the original, and remembers the library's name for the key earlier
    /// scans used.
    #[test]
    fn test_points_are_keyed_by_the_original_file_name() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("Photos.sqlite");
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE ZASSET (Z_PK INTEGER PRIMARY KEY, ZDATECREATED REAL,
                 ZLATITUDE REAL, ZLONGITUDE REAL, ZFILENAME TEXT,
                 ZTRASHEDSTATE INTEGER, ZMOMENT INTEGER, ZADDITIONALATTRIBUTES INTEGER);
             CREATE TABLE ZADDITIONALASSETATTRIBUTES (Z_PK INTEGER PRIMARY KEY,
                 ZTIMEZONEOFFSET INTEGER, ZORIGINALFILENAME TEXT);
             CREATE TABLE ZMOMENT (Z_PK INTEGER PRIMARY KEY, ZTITLE TEXT);
             INSERT INTO ZADDITIONALASSETATTRIBUTES VALUES (1, -14400, 'IMG_1234.HEIC');
             INSERT INTO ZADDITIONALASSETATTRIBUTES VALUES (2, -14400, NULL);
             INSERT INTO ZADDITIONALASSETATTRIBUTES VALUES (3, -14400, 'IMG_9.JPG');
             INSERT INTO ZASSET VALUES (1, 795362400.6, 42.3736, -71.1097,
                 '4F2B1C3D-0000-4000-8000-00000000AAAA.heic', 0, NULL, 1);
             INSERT INTO ZASSET VALUES (2, 795366000, 42.3736, -71.1097, 'IMG_2.HEIC', 0, NULL, 2);
             INSERT INTO ZASSET VALUES (3, 795369600, 42.3736, -71.1097, 'IMG_9.JPG', 0, NULL, 3);",
        )
        .unwrap();
        drop(conn);
        let points = scan_points(&db).unwrap();
        let keys: Vec<(String, Option<String>)> =
            points.iter().map(|p| (p.key(), p.earlier_key())).collect();
        let at = |apple: i64| apple + APPLE_EPOCH;
        assert_eq!(
            keys,
            [
                (
                    format!("{}|IMG_1234.HEIC", at(795362400)),
                    Some(format!(
                        "{}|4F2B1C3D-0000-4000-8000-00000000AAAA.heic",
                        at(795362400)
                    ))
                ),
                // no original name recorded: the library's name, as before
                (format!("{}|IMG_2.HEIC", at(795366000)), None),
                // the same name both ways: one key
                (format!("{}|IMG_9.JPG", at(795369600)), None),
            ]
        );
        let drafts = day_drafts(points, None).unwrap();
        assert!(drafts[0].body.contains("IMG_1234.HEIC"));
        assert!(!drafts[0].body.contains("4F2B1C3D"));
    }
}
