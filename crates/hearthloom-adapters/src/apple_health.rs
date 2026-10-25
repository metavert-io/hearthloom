//! Apple Health export adapter (`export.zip` → `apple_health_export/`):
//! the archive is millions of sensor samples, so — unlike every other
//! adapter — raw samples do NOT become episodes. One `health_day`
//! episode per day carries the human-readable aggregate (the timeline/
//! correlation layer); each workout, ECG, and clinical record is
//! episode-worthy on its own. The full-resolution numbers land in a
//! numeric sidecar built by the daemon from the same streaming scan
//! (see daemon `health_index.rs`) — trends and comparisons are SQL over
//! that sidecar, never prose-scraping over episodes.
//!
//! Deliberately shallow parse, like the mbox adapter: Apple's exporter
//! writes one element per line with all attributes on the open tag, so
//! a line scanner beats a 2.5GB XML tree. Every record keeps its
//! `sourceName` — the per-device provenance is what lets Ribbons tell
//! the "stopped wearing the Whoop" story (device entities per day).

use std::collections::BTreeMap;
use std::io::BufRead;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::DateTime;
use hearthloom_core::{EntityRef, EpisodeDraft, EventTime, TimeUncertainty};

/// A single HealthKit record, streaming out of the scanner.
#[derive(Debug, Clone)]
pub struct Sample {
    /// Short metric name (`StepCount`, `SleepAnalysis`, …) — HK prefixes
    /// stripped.
    pub metric: String,
    /// Numeric value when the record has one; for category records
    /// (sleep stages, mindful sessions) this is the duration in hours.
    pub value: Option<f64>,
    /// Category value for category records (`AsleepDeep`, …), stripped
    /// of the `HKCategoryValueSleepAnalysis` prefix.
    pub category: Option<String>,
    pub unit: Option<String>,
    /// Normalized source ("Apple Watch", "WHOOP", "Withings", …).
    pub source: String,
    /// Raw start timestamp, `2023-10-14 08:42:26 -0300`.
    pub start: String,
    /// Local day this sample belongs to (sleep buckets to the wake day).
    pub day: String,
}

/// One workout, with totals folded in from `WorkoutStatistics` children
/// (newer exports moved energy/distance off the open tag).
#[derive(Debug, Clone, Default)]
pub struct WorkoutRec {
    pub activity: String,
    pub duration_min: Option<f64>,
    pub distance_km: Option<f64>,
    pub kcal: Option<f64>,
    pub source: String,
    pub start: String,
    pub end: String,
    pub route: Option<String>,
}

pub enum Item {
    Sample(Sample),
    Workout(WorkoutRec),
}

/// Pull one attribute off a single-line tag, un-escaping XML entities.
fn attr(line: &str, name: &str) -> Option<String> {
    let needle = format!("{name}=\"");
    let at = line.find(&needle)? + needle.len();
    let rest = &line[at..];
    let end = rest.find('"')?;
    let raw = &rest[..end];
    Some(
        raw.replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&quot;", "\"")
            .replace("&#39;", "'")
            .replace("&amp;", "&"),
    )
}

fn parse_ts(raw: &str) -> Option<DateTime<chrono::FixedOffset>> {
    DateTime::parse_from_str(raw, "%Y-%m-%d %H:%M:%S %z").ok()
}

fn hours_between(start: &str, end: &str) -> Option<f64> {
    let (s, e) = (parse_ts(start)?, parse_ts(end)?);
    Some((e - s).num_seconds().max(0) as f64 / 3600.0)
}

/// "Ada’s Apple Watch" → "Apple Watch"; "iPhone (7)" → "iPhone".
/// Pub: the daemon's live-HealthKit finalize pass normalizes device
/// names the same way so live and export rows aggregate together.
pub fn normalize_source(raw: &str) -> String {
    let s = raw.trim();
    if s.contains("Watch") {
        return "Apple Watch".into();
    }
    if s.contains("iPhone") {
        return "iPhone".into();
    }
    s.to_string()
}

fn short_metric(hk_type: &str) -> String {
    hk_type
        .trim_start_matches("HKQuantityTypeIdentifier")
        .trim_start_matches("HKCategoryTypeIdentifier")
        .trim_start_matches("HKDataType")
        .to_string()
}

fn record_to_sample(line: &str) -> Option<Sample> {
    let hk_type = attr(line, "type")?;
    let metric = short_metric(&hk_type);
    let source = normalize_source(&attr(line, "sourceName")?);
    let start = attr(line, "startDate")?;
    let end = attr(line, "endDate").unwrap_or_else(|| start.clone());
    let unit = attr(line, "unit");
    let raw_value = attr(line, "value");

    let (value, category, day) = if metric == "SleepAnalysis" {
        // Stage segments: value is the stage, duration is the number;
        // a night belongs to the day you woke up.
        let stage = raw_value
            .as_deref()
            .unwrap_or("Asleep")
            .trim_start_matches("HKCategoryValueSleepAnalysis")
            .to_string();
        (hours_between(&start, &end), Some(stage), end[..10].to_string())
    } else if metric == "MindfulSession" {
        (hours_between(&start, &end), None, start[..10].to_string())
    } else {
        (
            raw_value.as_deref().and_then(|v| v.parse::<f64>().ok()),
            None,
            start[..10].to_string(),
        )
    };
    Some(Sample {
        metric,
        value,
        category,
        unit,
        source,
        start,
        day,
    })
}

/// Stream the export once; every Record/Workout goes to `sink`.
/// Returns (samples, workouts) counts.
pub fn scan(path: &Path, mut sink: impl FnMut(Item) -> Result<()>) -> Result<(usize, usize)> {
    let file = std::fs::File::open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    let reader = std::io::BufReader::with_capacity(1 << 20, file);

    let mut samples = 0usize;
    let mut workouts = 0usize;
    let mut workout: Option<WorkoutRec> = None;
    for line in reader.split(b'\n') {
        let line = line?;
        let line = String::from_utf8_lossy(&line);
        let line = line.trim_start();
        if line.starts_with("<Record ") {
            if let Some(sample) = record_to_sample(line) {
                samples += 1;
                sink(Item::Sample(sample))?;
            }
        } else if line.starts_with("<Workout ") {
            workout = Some(WorkoutRec {
                activity: attr(line, "workoutActivityType")
                    .unwrap_or_default()
                    .trim_start_matches("HKWorkoutActivityType")
                    .to_string(),
                duration_min: attr(line, "duration").and_then(|v| v.parse().ok()),
                distance_km: attr(line, "totalDistance").and_then(|v| v.parse().ok()),
                kcal: attr(line, "totalEnergyBurned").and_then(|v| v.parse().ok()),
                source: normalize_source(&attr(line, "sourceName").unwrap_or_default()),
                start: attr(line, "startDate").unwrap_or_default(),
                end: attr(line, "endDate").unwrap_or_default(),
                route: None,
            });
        } else if let Some(w) = workout.as_mut() {
            if line.starts_with("<WorkoutStatistics ") {
                let stat = attr(line, "type").unwrap_or_default();
                let sum = attr(line, "sum").and_then(|v| v.parse::<f64>().ok());
                if stat.ends_with("ActiveEnergyBurned") && w.kcal.is_none() {
                    w.kcal = sum;
                } else if (stat.ends_with("DistanceWalkingRunning")
                    || stat.ends_with("DistanceCycling")
                    || stat.ends_with("DistanceSwimming"))
                    && w.distance_km.is_none()
                {
                    w.distance_km = sum;
                }
            } else if line.starts_with("<FileReference ") {
                w.route = attr(line, "path");
            } else if line.starts_with("</Workout>") {
                if let Some(w) = workout.take() {
                    if !w.start.is_empty() {
                        workouts += 1;
                        sink(Item::Workout(w))?;
                    }
                }
            }
        }
    }
    Ok((samples, workouts))
}

/* ————— day aggregation ————— */

#[derive(Debug, Clone, Default)]
pub struct Agg {
    pub n: u64,
    pub sum: f64,
    pub min: f64,
    pub max: f64,
}

impl Agg {
    fn add(&mut self, v: f64) {
        if self.n == 0 {
            self.min = v;
            self.max = v;
        } else {
            self.min = self.min.min(v);
            self.max = self.max.max(v);
        }
        self.n += 1;
        self.sum += v;
    }
    pub fn avg(&self) -> f64 {
        if self.n == 0 {
            0.0
        } else {
            self.sum / self.n as f64
        }
    }
}

/// (day, metric, source) → stats. Sleep stages aggregate as
/// `Sleep/<stage>` metrics valued in hours.
#[derive(Default)]
pub struct DayTable {
    pub map: BTreeMap<(String, String, String), Agg>,
    units: BTreeMap<String, String>,
    /// First raw timestamp seen per day (anchors the episode's event time).
    first_ts: BTreeMap<String, String>,
}

impl DayTable {
    pub fn add(&mut self, s: &Sample) {
        let Some(v) = s.value else { return };
        let metric = match (&s.metric[..], &s.category) {
            ("SleepAnalysis", Some(stage)) => format!("Sleep/{stage}"),
            _ => s.metric.clone(),
        };
        if let Some(u) = &s.unit {
            self.units.entry(metric.clone()).or_insert_with(|| u.clone());
        }
        self.first_ts
            .entry(s.day.clone())
            .or_insert_with(|| s.start.clone());
        self.map
            .entry((s.day.clone(), metric, s.source.clone()))
            .or_default()
            .add(v);
    }

    /// One `health_day` episode per day, with a compact per-family body
    /// and one Device entity per contributing source.
    pub fn into_episodes(self, scope: Option<&str>) -> Vec<EpisodeDraft> {
        // Regroup day → metric → (source, agg).
        let mut days: BTreeMap<String, BTreeMap<String, Vec<(String, Agg)>>> = BTreeMap::new();
        for ((day, metric, source), agg) in self.map {
            days.entry(day)
                .or_default()
                .entry(metric)
                .or_default()
                .push((source, agg));
        }
        let mut episodes = Vec::new();
        for (day, metrics) in days {
            let pick = |name: &str| -> Option<(String, Agg)> {
                let sources = metrics.get(name)?;
                sources
                    .iter()
                    .min_by_key(|(s, _)| source_priority(s))
                    .cloned()
            };
            let mut lines: Vec<String> = Vec::new();
            let mut highlights: Vec<String> = Vec::new();

            // activity
            let mut activity = Vec::new();
            if let Some((src, a)) = pick("StepCount") {
                activity.push(format!("{} steps [{}]", (a.sum as i64), lc(&src)));
                highlights.push(format!("{} steps", a.sum as i64));
            }
            if let Some((src, a)) = pick("DistanceWalkingRunning") {
                let unit = self.units.get("DistanceWalkingRunning").cloned().unwrap_or_default();
                activity.push(format!("{:.1} {unit} [{}]", a.sum, lc(&src)));
            }
            if let Some((src, a)) = pick("ActiveEnergyBurned") {
                activity.push(format!("{:.0} kcal active [{}]", a.sum, lc(&src)));
            }
            if let Some((src, a)) = pick("AppleExerciseTime") {
                activity.push(format!("{:.0} min exercise [{}]", a.sum, lc(&src)));
            }
            if !activity.is_empty() {
                lines.push(format!("activity: {}", activity.join(" · ")));
            }

            // heart
            let mut heart = Vec::new();
            if let Some((src, a)) = pick("RestingHeartRate") {
                heart.push(format!("rhr {:.0} [{}]", a.avg(), lc(&src)));
                highlights.push(format!("rhr {:.0}", a.avg()));
            }
            if let Some((src, a)) = pick("HeartRate") {
                heart.push(format!(
                    "hr {:.0}–{:.0} avg {:.0} [{}]",
                    a.min,
                    a.max,
                    a.avg(),
                    lc(&src)
                ));
            }
            if let Some((src, a)) = pick("HeartRateVariabilitySDNN") {
                heart.push(format!("hrv {:.0}ms [{}]", a.avg(), lc(&src)));
            }
            if let Some((src, a)) = pick("RespiratoryRate") {
                heart.push(format!("resp {:.1}/min [{}]", a.avg(), lc(&src)));
            }
            if let Some((src, a)) = pick("OxygenSaturation") {
                heart.push(format!("spo2 {:.0}% [{}]", a.avg() * 100.0, lc(&src)));
            }
            if let Some((src, a)) = pick("VO2Max") {
                heart.push(format!("vo2max {:.1} [{}]", a.avg(), lc(&src)));
            }
            if !heart.is_empty() {
                lines.push(format!("heart: {}", heart.join(" · ")));
            }

            // sleep (sum of asleep stages; InBed separate)
            let asleep: f64 = ["Sleep/AsleepCore", "Sleep/AsleepDeep", "Sleep/AsleepREM", "Sleep/AsleepUnspecified", "Sleep/Asleep"]
                .iter()
                .filter_map(|k| pick(k).map(|(_, a)| a.sum))
                .sum();
            if asleep > 0.05 {
                let mut sleep = vec![format!("{} asleep", fmt_hours(asleep))];
                if let Some((_, deep)) = pick("Sleep/AsleepDeep") {
                    sleep.push(format!("{} deep", fmt_hours(deep.sum)));
                }
                if let Some((_, rem)) = pick("Sleep/AsleepREM") {
                    sleep.push(format!("{} rem", fmt_hours(rem.sum)));
                }
                if let Some((src, bed)) = pick("Sleep/InBed") {
                    sleep.push(format!("{} in bed [{}]", fmt_hours(bed.sum), lc(&src)));
                }
                lines.push(format!("sleep: {}", sleep.join(", ")));
                highlights.push(format!("{} sleep", fmt_hours(asleep)));
            }

            // body
            let mut body = Vec::new();
            if let Some((src, a)) = pick("BodyMass") {
                let unit = self.units.get("BodyMass").cloned().unwrap_or_default();
                body.push(format!("{:.1} {unit} [{}]", a.avg(), lc(&src)));
            }
            if let Some((src, a)) = pick("BodyFatPercentage") {
                body.push(format!("{:.1}% body fat [{}]", a.avg() * 100.0, lc(&src)));
            }
            if !body.is_empty() {
                lines.push(format!("body: {}", body.join(" · ")));
            }

            // glucose (CGM)
            if let Some((src, a)) = pick("BloodGlucose") {
                let unit = self.units.get("BloodGlucose").cloned().unwrap_or_default();
                lines.push(format!(
                    "glucose: avg {:.0} {unit} ({:.0}–{:.0}) [{}]",
                    a.avg(),
                    a.min,
                    a.max,
                    lc(&src)
                ));
            }

            // mindfulness
            if let Some((src, a)) = pick("MindfulSession") {
                lines.push(format!(
                    "mindfulness: {} session{} · {:.0} min [{}]",
                    a.n,
                    if a.n == 1 { "" } else { "s" },
                    a.sum * 60.0,
                    lc(&src)
                ));
            }

            if lines.is_empty() {
                continue; // nothing episode-worthy that day (sidecar still has it)
            }

            let sources: Vec<String> = {
                let mut set: Vec<String> = metrics
                    .values()
                    .flatten()
                    .map(|(s, _)| s.clone())
                    .collect();
                set.sort();
                set.dedup();
                set
            };
            lines.push(format!("sources: {}", sources.iter().map(|s| lc(s)).collect::<Vec<_>>().join(", ")));

            let event_time = self
                .first_ts
                .get(&day)
                .and_then(|ts| parse_ts(ts))
                .map(|dt| EventTime::from_fixed(dt, TimeUncertainty::Day))
                .unwrap_or_else(EventTime::unknown);
            let title = if highlights.is_empty() {
                format!("Health: {day}")
            } else {
                format!("Health: {day} — {}", highlights.join(" · "))
            };
            episodes.push(EpisodeDraft {
                source: "apple-health".into(),
                source_ref: format!("day:{day}"),
                kind: "health_day".into(),
                title,
                body: lines.join("\n"),
                event_time,
                trust: "imported".into(),
                scope: scope.map(String::from).or(Some("health".into())),
                entities: sources
                    .iter()
                    .map(|s| EntityRef {
                        entity_type: "Device".into(),
                        name: s.clone(),
                        attributes: vec![],
                        role: "reported".into(),
                    })
                    .collect(),
                edges: vec![],
            });
        }
        episodes
    }
}

fn lc(s: &str) -> String {
    s.to_lowercase()
}

fn fmt_hours(h: f64) -> String {
    let mins = (h * 60.0).round() as i64;
    format!("{}h{:02}m", mins / 60, mins % 60)
}

/// Lower = preferred when several sources report the same metric.
fn source_priority(source: &str) -> u8 {
    match source {
        s if s.contains("Watch") => 0,
        "WHOOP" => 1,
        "Oura" => 2,
        "Withings" => 3,
        "Signos" => 4,
        "Peloton" => 5,
        "iPhone" => 6,
        _ => 9,
    }
}

pub fn workout_to_episode(w: &WorkoutRec, scope: Option<&str>) -> EpisodeDraft {
    let mut bits = Vec::new();
    if let Some(d) = w.duration_min {
        bits.push(format!("{:.0} min", d));
    }
    if let Some(km) = w.distance_km {
        bits.push(format!("{km:.1} km"));
    }
    if let Some(k) = w.kcal {
        bits.push(format!("{k:.0} kcal"));
    }
    let title = format!(
        "{}{} ({})",
        w.activity,
        if bits.is_empty() {
            String::new()
        } else {
            format!(" — {}", bits.join(" · "))
        },
        lc(&w.source)
    );
    let mut body = format!("workout: {}\nstart: {}\nend: {}\n", w.activity, w.start, w.end);
    if !bits.is_empty() {
        body.push_str(&format!("totals: {}\n", bits.join(" · ")));
    }
    if let Some(route) = &w.route {
        body.push_str(&format!("route: {route}\n"));
    }
    EpisodeDraft {
        source: "apple-health".into(),
        source_ref: format!("workout:{}", w.start),
        kind: "workout".into(),
        title,
        body,
        event_time: parse_ts(&w.start)
            .map(|dt| EventTime::from_fixed(dt, TimeUncertainty::Exact))
            .unwrap_or_else(EventTime::unknown),
        trust: "imported".into(),
        scope: scope.map(String::from).or(Some("health".into())),
        entities: vec![EntityRef {
            entity_type: "Device".into(),
            name: w.source.clone(),
            attributes: vec![],
            role: "recorded".into(),
        }],
        edges: vec![],
    }
}

/// FHIR clinical records (immunizations, labs) — discrete medical
/// events, one episode each.
pub fn ingest_clinical(dir: &Path, scope: Option<&str>) -> Result<Vec<EpisodeDraft>> {
    let mut episodes = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return Ok(episodes), // no clinical records in this export
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let raw = std::fs::read_to_string(&path)?;
        let doc: serde_json::Value = match serde_json::from_str(&raw) {
            Ok(doc) => doc,
            Err(_) => continue,
        };
        let resource = doc["resourceType"].as_str().unwrap_or("ClinicalRecord");
        let what = doc["vaccineCode"]["text"]
            .as_str()
            .or_else(|| doc["code"]["text"].as_str())
            .unwrap_or(resource);
        let when = doc["occurrenceDateTime"]
            .as_str()
            .or_else(|| doc["effectiveDateTime"].as_str())
            .or_else(|| doc["date"].as_str());
        let event_time = when
            .and_then(|ts| {
                EventTime::parse_rfc3339(ts, TimeUncertainty::Day)
                    .ok()
                    .or_else(|| {
                        // Bare dates ("2021-04-03") are common in FHIR.
                        EventTime::parse_rfc3339(
                            &format!("{ts}T12:00:00Z"),
                            TimeUncertainty::Day,
                        )
                        .ok()
                    })
            })
            .unwrap_or_else(EventTime::unknown);
        episodes.push(EpisodeDraft {
            source: "apple-health".into(),
            source_ref: format!(
                "clinical:{}",
                path.file_stem().unwrap_or_default().to_string_lossy()
            ),
            kind: "clinical_record".into(),
            title: format!("{resource}: {what}"),
            body: format!(
                "{resource}: {what}\ndate: {}\n(from Apple Health clinical records)",
                when.unwrap_or("unknown")
            ),
            event_time,
            trust: "imported".into(),
            scope: scope.map(String::from).or(Some("health".into())),
            entities: vec![],
            edges: vec![],
        });
    }
    Ok(episodes)
}

/// Apple Watch ECG CSVs: a handful of discrete recordings.
pub fn ingest_ecg(dir: &Path, scope: Option<&str>) -> Result<Vec<EpisodeDraft>> {
    let mut episodes = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return Ok(episodes),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("csv") {
            continue;
        }
        let head = std::fs::read_to_string(&path)?
            .lines()
            .take(12)
            .map(String::from)
            .collect::<Vec<_>>();
        let field = |name: &str| -> Option<String> {
            head.iter()
                .find(|l| l.starts_with(name))
                .and_then(|l| l.split(',').nth(1))
                .map(|v| v.trim().to_string())
        };
        let recorded = field("Recorded Date");
        let classification = field("Classification").unwrap_or_else(|| "ECG".into());
        let event_time = recorded
            .as_deref()
            .and_then(parse_ts)
            .map(|dt| EventTime::from_fixed(dt, TimeUncertainty::Exact))
            .unwrap_or_else(EventTime::unknown);
        episodes.push(EpisodeDraft {
            source: "apple-health".into(),
            source_ref: format!(
                "ecg:{}",
                path.file_stem().unwrap_or_default().to_string_lossy()
            ),
            kind: "ecg".into(),
            title: format!("ECG — {classification}"),
            body: format!(
                "ECG recording\nclassification: {classification}\nrecorded: {}\nfile: {}",
                recorded.as_deref().unwrap_or("unknown"),
                path.file_name().unwrap_or_default().to_string_lossy()
            ),
            event_time,
            trust: "imported".into(),
            scope: scope.map(String::from).or(Some("health".into())),
            entities: vec![EntityRef {
                entity_type: "Device".into(),
                name: "Apple Watch".into(),
                attributes: vec![],
                role: "recorded".into(),
            }],
            edges: vec![],
        });
    }
    Ok(episodes)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_XML: &str = r#"<?xml version="1.0"?>
<HealthData>
 <Record type="HKQuantityTypeIdentifierStepCount" sourceName="Ada’s Apple Watch" unit="count" creationDate="2026-04-02 09:00:00 -0400" startDate="2026-04-02 08:00:00 -0400" endDate="2026-04-02 08:10:00 -0400" value="812"/>
 <Record type="HKQuantityTypeIdentifierStepCount" sourceName="iPhone (7)" unit="count" creationDate="2026-04-02 09:00:00 -0400" startDate="2026-04-02 08:00:00 -0400" endDate="2026-04-02 08:10:00 -0400" value="7300"/>
 <Record type="HKQuantityTypeIdentifierStepCount" sourceName="Ada’s Apple Watch" unit="count" creationDate="2026-04-02 10:00:00 -0400" startDate="2026-04-02 09:30:00 -0400" endDate="2026-04-02 09:40:00 -0400" value="1400"/>
 <Record type="HKQuantityTypeIdentifierRestingHeartRate" sourceName="WHOOP" unit="count/min" creationDate="2026-04-03 07:00:00 -0400" startDate="2026-04-02 23:00:00 -0400" endDate="2026-04-02 23:00:00 -0400" value="52"/>
 <Record type="HKCategoryTypeIdentifierSleepAnalysis" sourceName="WHOOP" creationDate="2026-04-03 07:00:00 -0400" startDate="2026-04-02 23:30:00 -0400" endDate="2026-04-03 01:30:00 -0400" value="HKCategoryValueSleepAnalysisAsleepDeep"/>
 <Record type="HKCategoryTypeIdentifierSleepAnalysis" sourceName="WHOOP" creationDate="2026-04-03 07:00:00 -0400" startDate="2026-04-03 01:30:00 -0400" endDate="2026-04-03 05:30:00 -0400" value="HKCategoryValueSleepAnalysisAsleepCore"/>
 <Record type="HKQuantityTypeIdentifierBodyMass" sourceName="Withings" unit="lb" creationDate="2026-04-02 08:00:00 -0400" startDate="2026-04-02 07:55:00 -0400" endDate="2026-04-02 07:55:00 -0400" value="171.3">
  <MetadataEntry key="HKWasUserEntered" value="0"/>
 </Record>
 <Workout workoutActivityType="HKWorkoutActivityTypeCycling" duration="32.5" durationUnit="min" sourceName="Peloton" creationDate="2026-04-02 18:00:00 -0400" startDate="2026-04-02 17:20:00 -0400" endDate="2026-04-02 17:52:30 -0400">
  <WorkoutStatistics type="HKQuantityTypeIdentifierActiveEnergyBurned" startDate="2026-04-02 17:20:00 -0400" endDate="2026-04-02 17:52:30 -0400" sum="312" unit="Cal"/>
  <WorkoutStatistics type="HKQuantityTypeIdentifierDistanceCycling" startDate="2026-04-02 17:20:00 -0400" endDate="2026-04-02 17:52:30 -0400" sum="14.2" unit="km"/>
 </Workout>
</HealthData>
"#;

    fn scan_fixture() -> (Vec<Sample>, Vec<WorkoutRec>) {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("export.xml");
        std::fs::write(&path, SAMPLE_XML).unwrap();
        let mut samples = Vec::new();
        let mut workouts = Vec::new();
        scan(&path, |item| {
            match item {
                Item::Sample(s) => samples.push(s),
                Item::Workout(w) => workouts.push(w),
            }
            Ok(())
        })
        .unwrap();
        (samples, workouts)
    }

    #[test]
    fn test_scan_records_and_workouts() {
        let (samples, workouts) = scan_fixture();
        assert_eq!(samples.len(), 7);
        assert_eq!(workouts.len(), 1);

        // Source normalization + entity unescaping.
        assert_eq!(samples[0].source, "Apple Watch");
        assert_eq!(samples[1].source, "iPhone");

        // Sleep buckets to the WAKE day and carries stage + duration-hours.
        let deep = samples.iter().find(|s| s.category.as_deref() == Some("AsleepDeep")).unwrap();
        assert_eq!(deep.day, "2026-04-03");
        assert!((deep.value.unwrap() - 2.0).abs() < 0.01);

        // Workout totals fold in from WorkoutStatistics children.
        let w = &workouts[0];
        assert_eq!(w.activity, "Cycling");
        assert_eq!(w.kcal, Some(312.0));
        assert_eq!(w.distance_km, Some(14.2));
        assert_eq!(w.source, "Peloton");
    }

    #[test]
    fn test_day_episode_prefers_priority_source() {
        let (samples, _) = scan_fixture();
        let mut table = DayTable::default();
        for s in &samples {
            table.add(s);
        }
        let episodes = table.into_episodes(None);
        assert_eq!(episodes.len(), 2); // Apr 2 (activity/body) + Apr 3 (sleep)

        let apr2 = &episodes[0];
        assert_eq!(apr2.source_ref, "day:2026-04-02");
        assert_eq!(apr2.kind, "health_day");
        assert_eq!(apr2.scope.as_deref(), Some("health"));
        // Watch (2212 steps) wins over the iPhone's 7300 — no double count.
        assert!(apr2.body.contains("2212 steps [apple watch]"), "body: {}", apr2.body);
        assert!(apr2.body.contains("171.3 lb [withings]"));
        assert!(apr2.body.contains("rhr 52 [whoop]"));
        // Devices become entities for the Ribbons coverage story.
        let devices: Vec<&str> = apr2.entities.iter().map(|e| e.name.as_str()).collect();
        assert!(devices.contains(&"Apple Watch") && devices.contains(&"WHOOP"));

        let apr3 = &episodes[1];
        assert!(apr3.body.contains("6h00m asleep"), "body: {}", apr3.body);
        assert!(apr3.body.contains("2h00m deep"));
    }

    #[test]
    fn test_workout_episode() {
        let (_, workouts) = scan_fixture();
        let episode = workout_to_episode(&workouts[0], None);
        assert_eq!(episode.kind, "workout");
        assert!(episode.title.contains("Cycling"));
        assert!(episode.title.contains("312 kcal"));
        assert_eq!(episode.source_ref, "workout:2026-04-02 17:20:00 -0400");
        assert_eq!(episode.scope.as_deref(), Some("health"));
    }
}
