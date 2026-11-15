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
    /// Normalized source ("Apple Watch", "WHOOP", "Withings", …): how the
    /// sample is shown and ranked.
    pub source: String,
    /// The device or app that recorded it, as named where it came from
    /// ("Ada’s Apple Watch Ultra"). Two devices can share a `source` (two
    /// watches are both "Apple Watch"); a night's sleep is one device's.
    pub device: String,
    /// Raw start timestamp, `2023-10-14 08:42:26 -0300`.
    pub start: String,
    /// Local day this sample belongs to (sleep buckets to the wake day).
    pub day: String,
}

impl Sample {
    /// Who recorded the sample, as the day table keeps it apart: the
    /// device for a sleep stage (two devices on the same night record the
    /// same hours, which must not add up), the source for the rest.
    fn tracker(&self) -> &str {
        let device = self.device.trim();
        if self.metric == "SleepAnalysis" && !device.is_empty() {
            device
        } else {
            &self.source
        }
    }
}

/// One workout, with totals folded in from `WorkoutStatistics` children
/// (newer exports moved energy/distance off the open tag). Totals are in
/// the units their names say, whatever unit the export wrote.
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
///
/// Only a name that says "Apple Watch" (in any language's device name:
/// "Apple Watch de Ada") is the watch: an app whose name merely contains
/// "Watch" (SleepWatch) is a tracker of its own, and two trackers must
/// never share a source — their sleep would add up.
pub fn normalize_source(raw: &str) -> String {
    let s = raw.trim();
    let spaced: String = s
        .chars()
        .map(|c| if c.is_whitespace() { ' ' } else { c })
        .collect();
    if spaced.to_lowercase().contains("apple watch") {
        return "Apple Watch".into();
    }
    if s.contains("iPhone") {
        return "iPhone".into();
    }
    s.to_string()
}

/// A HealthKit unit string as a dimension and the affine map onto that
/// dimension's base unit (base = value × scale + offset).
fn unit_def(unit: &str) -> Option<(&'static str, f64, f64)> {
    let linear = |dimension, scale| Some((dimension, scale, 0.0));
    match unit {
        "g" => linear("mass", 1.0),
        "kg" => linear("mass", 1000.0),
        "mg" => linear("mass", 1e-3),
        "mcg" => linear("mass", 1e-6),
        "lb" => linear("mass", 453.592_37),
        "oz" => linear("mass", 28.349_523_125),
        "st" => linear("mass", 6_350.293_18),
        "m" => linear("length", 1.0),
        "km" => linear("length", 1000.0),
        "cm" => linear("length", 0.01),
        "mm" => linear("length", 0.001),
        "mi" => linear("length", 1_609.344),
        "yd" => linear("length", 0.9144),
        "ft" => linear("length", 0.3048),
        "in" => linear("length", 0.0254),
        // HealthKit's "Cal" is the food calorie, a kilocalorie.
        "kcal" | "Cal" => linear("energy", 1.0),
        "cal" => linear("energy", 0.001),
        "kJ" => linear("energy", 1.0 / 4.184),
        "J" => linear("energy", 1.0 / 4184.0),
        "s" => linear("time", 1.0),
        "ms" => linear("time", 0.001),
        "min" => linear("time", 60.0),
        "hr" | "h" => linear("time", 3600.0),
        "d" => linear("time", 86_400.0),
        "L" => linear("volume", 1.0),
        "mL" => linear("volume", 0.001),
        "fl_oz_us" => linear("volume", 0.029_573_529_562_5),
        "fl_oz_imp" => linear("volume", 0.028_413_062_5),
        "cup_us" => linear("volume", 0.236_588_236_5),
        "m/s" => linear("speed", 1.0),
        "km/hr" => linear("speed", 1000.0 / 3600.0),
        "mi/hr" => linear("speed", 1_609.344 / 3600.0),
        "degC" => linear("temperature", 1.0),
        "degF" => Some(("temperature", 5.0 / 9.0, -160.0 / 9.0)),
        "K" => Some(("temperature", 1.0, -273.15)),
        "mg/dL" => linear("mass/volume", 1.0),
        // Blood glucose in molar units names its molar mass:
        // `mmol<180.15588000005408>/L`; mmol/L × g/mol ÷ 10 = mg/dL.
        other => {
            let molar_mass = other
                .strip_prefix("mmol<")?
                .strip_suffix(">/L")?
                .parse::<f64>()
                .ok()
                .filter(|m| m.is_finite() && *m > 0.0)?;
            linear("mass/volume", molar_mass / 10.0)
        }
    }
}

/// `value` written in `from`, read in `to`; None when the two units don't
/// measure the same thing (or either is one this table doesn't know and
/// they differ).
fn convert_unit(value: f64, from: &str, to: &str) -> Option<f64> {
    if from == to {
        return Some(value);
    }
    let (dimension_from, scale_from, offset_from) = unit_def(from)?;
    let (dimension_to, scale_to, offset_to) = unit_def(to)?;
    (dimension_from == dimension_to)
        .then(|| (value * scale_from + offset_from - offset_to) / scale_to)
}

/// The unit a metric always aggregates in, for the metrics whose readers
/// name one (the day's text says "kcal active", "min exercise", "hrv …ms";
/// Pulse labels HRV in ms). Metrics written in a regional unit (mass,
/// distance, glucose) aggregate in the first unit the table sees, which
/// their readers take from the table.
fn canonical_unit(metric: &str) -> Option<&'static str> {
    match metric {
        "ActiveEnergyBurned" => Some("kcal"),
        "AppleExerciseTime" => Some("min"),
        "HeartRateVariabilitySDNN" => Some("ms"),
        _ => None,
    }
}

/// A total read in `to`: as written when no unit is given, dropped when
/// its unit measures something else.
fn total_in(value: Option<String>, unit: Option<String>, to: &str) -> Option<f64> {
    let value = value?.parse::<f64>().ok()?;
    match unit {
        Some(unit) => convert_unit(value, &unit, to),
        None => Some(value),
    }
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
    let device = attr(line, "sourceName")?.trim().to_string();
    let source = normalize_source(&device);
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
        (
            hours_between(&start, &end),
            Some(stage),
            end[..10].to_string(),
        )
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
        device,
        start,
        day,
    })
}

/// Stream the export once; every Record/Workout goes to `sink`.
/// Returns (samples, workouts) counts.
pub fn scan(path: &Path, mut sink: impl FnMut(Item) -> Result<()>) -> Result<(usize, usize)> {
    let file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
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
                duration_min: total_in(attr(line, "duration"), attr(line, "durationUnit"), "min"),
                distance_km: total_in(
                    attr(line, "totalDistance"),
                    attr(line, "totalDistanceUnit"),
                    "km",
                ),
                kcal: total_in(
                    attr(line, "totalEnergyBurned"),
                    attr(line, "totalEnergyBurnedUnit"),
                    "kcal",
                ),
                source: normalize_source(&attr(line, "sourceName").unwrap_or_default()),
                start: attr(line, "startDate").unwrap_or_default(),
                end: attr(line, "endDate").unwrap_or_default(),
                route: None,
            });
        } else if let Some(w) = workout.as_mut() {
            if line.starts_with("<WorkoutStatistics ") {
                let stat = attr(line, "type").unwrap_or_default();
                let sum = |to: &str| total_in(attr(line, "sum"), attr(line, "unit"), to);
                if stat.ends_with("ActiveEnergyBurned") && w.kcal.is_none() {
                    w.kcal = sum("kcal");
                } else if (stat.ends_with("DistanceWalkingRunning")
                    || stat.ends_with("DistanceCycling")
                    || stat.ends_with("DistanceSwimming"))
                    && w.distance_km.is_none()
                {
                    w.distance_km = sum("km");
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

/// (day, metric, tracker) → stats. Sleep stages aggregate as
/// `Sleep/<stage>` metrics valued in hours. The tracker is the sample's
/// source, except for sleep stages, which are kept per device
/// (`Sample::tracker`): the day's text shows and ranks every tracker by
/// its source name.
///
/// Every value of a metric is aggregated in ONE unit: the one the table
/// was made with (`with_units`), else its canonical unit when its
/// readers name one (`canonical_unit`), else the first unit the table
/// sees for it. A sample in another unit of the same kind (154 lb after
/// 70 kg, kJ for kcal) is converted; one whose unit measures something
/// else, or that this table can't convert, is left out of the day.
#[derive(Default)]
pub struct DayTable {
    pub map: BTreeMap<(String, String, String), Agg>,
    /// metric → the unit its aggregates are in.
    units: BTreeMap<String, String>,
    /// First raw timestamp seen per day (anchors the episode's event time).
    first_ts: BTreeMap<String, String>,
}

impl DayTable {
    /// A table whose metrics aggregate in the given units (metric →
    /// unit) — a reader's, so new values join the ones it already holds.
    pub fn with_units(units: impl IntoIterator<Item = (String, String)>) -> DayTable {
        DayTable {
            units: units.into_iter().collect(),
            ..DayTable::default()
        }
    }

    pub fn add(&mut self, s: &Sample) {
        let Some(v) = s.value else { return };
        let metric = match (&s.metric[..], &s.category) {
            ("SleepAnalysis", Some(stage)) => format!("Sleep/{stage}"),
            _ => s.metric.clone(),
        };
        let v = match &s.unit {
            Some(unit) => {
                let table_unit = self.units.entry(metric.clone()).or_insert_with(|| {
                    canonical_unit(&metric)
                        .map(String::from)
                        .unwrap_or_else(|| unit.clone())
                });
                match convert_unit(v, unit, table_unit) {
                    Some(v) if v.is_finite() => v,
                    _ => return,
                }
            }
            None => v,
        };
        self.first_ts
            .entry(s.day.clone())
            .or_insert_with(|| s.start.clone());
        self.map
            .entry((s.day.clone(), metric, s.tracker().to_string()))
            .or_default()
            .add(v);
    }

    /// The unit a metric's aggregates are in (None: unitless, like the
    /// sleep stages' hours).
    pub fn unit(&self, metric: &str) -> Option<&str> {
        self.units.get(metric).map(String::as_str)
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
            // The preferred tracker of a metric, by its source name.
            let pick = |name: &str| -> Option<(String, Agg)> {
                let (tracker, agg) = metrics
                    .get(name)?
                    .iter()
                    .min_by_key(|(t, _)| source_priority(&normalize_source(t)))?;
                Some((normalize_source(tracker), agg.clone()))
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
                let unit = self
                    .units
                    .get("DistanceWalkingRunning")
                    .cloned()
                    .unwrap_or_default();
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

            // sleep: the night as ONE device recorded it (the sum of its
            // asleep stages). Two trackers on the same night record the
            // same hours, so stages from different devices never add up —
            // not even two that share a source name. InBed is separate.
            let mut nights: BTreeMap<&str, f64> = BTreeMap::new();
            for stage in ASLEEP_STAGES {
                for (device, a) in metrics.get(stage).into_iter().flatten() {
                    *nights.entry(device.as_str()).or_default() += a.sum;
                }
            }
            let night = nights
                .into_iter()
                .filter(|(_, hours)| *hours > 0.05)
                .min_by_key(|(device, _)| source_priority(&normalize_source(device)));
            if let Some((sleep_device, asleep)) = night {
                let stage = |name: &str| -> Option<f64> {
                    metrics
                        .get(name)?
                        .iter()
                        .find(|(device, _)| device == sleep_device)
                        .map(|(_, a)| a.sum)
                };
                let mut sleep = vec![format!(
                    "{} asleep [{}]",
                    fmt_hours(asleep),
                    lc(&normalize_source(sleep_device))
                )];
                if let Some(deep) = stage("Sleep/AsleepDeep") {
                    sleep.push(format!("{} deep", fmt_hours(deep)));
                }
                if let Some(rem) = stage("Sleep/AsleepREM") {
                    sleep.push(format!("{} rem", fmt_hours(rem)));
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
                    .map(|(tracker, _)| normalize_source(tracker))
                    .collect();
                set.sort();
                set.dedup();
                set
            };
            lines.push(format!(
                "sources: {}",
                sources.iter().map(|s| lc(s)).collect::<Vec<_>>().join(", ")
            ));

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

/// The sleep stages that count as asleep (`Asleep` is the value older
/// exports wrote before stages existed).
const ASLEEP_STAGES: [&str; 5] = [
    "Sleep/AsleepCore",
    "Sleep/AsleepDeep",
    "Sleep/AsleepREM",
    "Sleep/AsleepUnspecified",
    "Sleep/Asleep",
];

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
        "Apple Watch" => 0,
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
    let mut body = format!(
        "workout: {}\nstart: {}\nend: {}\n",
        w.activity, w.start, w.end
    );
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
                        EventTime::parse_rfc3339(&format!("{ts}T12:00:00Z"), TimeUncertainty::Day)
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
        let deep = samples
            .iter()
            .find(|s| s.category.as_deref() == Some("AsleepDeep"))
            .unwrap();
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
        assert!(
            apr2.body.contains("2212 steps [apple watch]"),
            "body: {}",
            apr2.body
        );
        assert!(apr2.body.contains("171.3 lb [withings]"));
        assert!(apr2.body.contains("rhr 52 [whoop]"));
        // Devices become entities for the Ribbons coverage story.
        let devices: Vec<&str> = apr2.entities.iter().map(|e| e.name.as_str()).collect();
        assert!(devices.contains(&"Apple Watch") && devices.contains(&"WHOOP"));

        let apr3 = &episodes[1];
        assert!(apr3.body.contains("6h00m asleep"), "body: {}", apr3.body);
        assert!(apr3.body.contains("2h00m deep"));
    }

    fn sample(metric: &str, value: f64, unit: Option<&str>, source: &str) -> Sample {
        Sample {
            metric: metric.into(),
            value: Some(value),
            category: None,
            unit: unit.map(String::from),
            source: normalize_source(source),
            device: source.into(),
            start: "2026-04-02 08:00:00 -0400".into(),
            day: "2026-04-02".into(),
        }
    }

    fn sleep(stage: &str, hours: f64, source: &str) -> Sample {
        Sample {
            metric: "SleepAnalysis".into(),
            category: Some(stage.into()),
            ..sample("SleepAnalysis", hours, None, source)
        }
    }

    /// Samples of one metric in different units aggregate in one unit; a
    /// sample whose unit measures something else stays out of the day.
    #[test]
    fn test_mixed_units_aggregate_in_one_unit() {
        let mut table = DayTable::default();
        table.add(&sample("BodyMass", 70.0, Some("kg"), "Withings"));
        table.add(&sample("BodyMass", 154.0, Some("lb"), "Withings"));
        table.add(&sample("BodyMass", 3.0, Some("count"), "Withings"));
        table.add(&sample(
            "DistanceWalkingRunning",
            1.0,
            Some("mi"),
            "Apple Watch",
        ));
        table.add(&sample(
            "DistanceWalkingRunning",
            500.0,
            Some("m"),
            "Apple Watch",
        ));
        let mass = &table.map[&(
            "2026-04-02".to_string(),
            "BodyMass".to_string(),
            "Withings".to_string(),
        )];
        assert_eq!(mass.n, 2);
        assert!((mass.avg() - 69.93).abs() < 0.01, "{}", mass.avg());
        assert_eq!(table.unit("BodyMass"), Some("kg"));
        let episodes = table.into_episodes(None);
        let body = &episodes[0].body;
        assert!(body.contains("69.9 kg [withings]"), "{body}");
        // 1 mi + 500 m = 1.31 mi.
        assert!(body.contains("1.3 mi [apple watch]"), "{body}");

        // Glucose in molar units joins mg/dL readings.
        let mut glucose = DayTable::default();
        glucose.add(&sample("BloodGlucose", 100.0, Some("mg/dL"), "Signos"));
        glucose.add(&sample(
            "BloodGlucose",
            5.55,
            Some("mmol<180.15588000005408>/L"),
            "Signos",
        ));
        let body = &glucose.into_episodes(None)[0].body;
        assert!(body.contains("glucose: avg 100 mg/dL (100–100)"), "{body}");

        assert_eq!(
            convert_unit(98.6, "degF", "degC").map(|c| (c * 10.0).round()),
            Some(370.0)
        );
        assert_eq!(convert_unit(1.0, "kg", "km"), None);
    }

    /// The day's text names a unit for some metrics; those aggregate in
    /// that unit whichever unit arrives first, and a regional metric is
    /// labelled with the unit it aggregated in, in either order.
    #[test]
    fn test_named_units_hold_in_either_input_order() {
        let day = |samples: &[(&str, f64, &str)]| -> String {
            let mut table = DayTable::default();
            for (metric, value, unit) in samples {
                table.add(&sample(metric, *value, Some(unit), "Apple Watch"));
            }
            table.into_episodes(None).remove(0).body
        };
        let named = [
            ("ActiveEnergyBurned", 418.4, "kJ"),
            ("ActiveEnergyBurned", 100.0, "kcal"),
            ("AppleExerciseTime", 0.5, "hr"),
            ("AppleExerciseTime", 30.0, "min"),
            ("HeartRateVariabilitySDNN", 0.05, "s"),
            ("HeartRateVariabilitySDNN", 40.0, "ms"),
        ];
        let mut reversed = named;
        reversed.reverse();
        for samples in [named, reversed] {
            let body = day(&samples);
            assert!(body.contains("200 kcal active [apple watch]"), "{body}");
            assert!(body.contains("60 min exercise [apple watch]"), "{body}");
            assert!(body.contains("hrv 45ms [apple watch]"), "{body}");
        }
        // HealthKit's "Cal" is a kilocalorie.
        assert!(day(&[("ActiveEnergyBurned", 312.0, "Cal")]).contains("312 kcal active"));

        let kg_first = day(&[("BodyMass", 70.0, "kg"), ("BodyMass", 154.0, "lb")]);
        assert!(
            kg_first.contains("body: 69.9 kg [apple watch]"),
            "{kg_first}"
        );
        let lb_first = day(&[("BodyMass", 154.0, "lb"), ("BodyMass", 70.0, "kg")]);
        assert!(
            lb_first.contains("body: 154.2 lb [apple watch]"),
            "{lb_first}"
        );
    }

    /// Workout totals are read in the units the export wrote them in.
    #[test]
    fn test_workout_totals_follow_their_units() {
        const XML: &str = r#"<HealthData>
 <Workout workoutActivityType="HKWorkoutActivityTypeRunning" duration="1800" durationUnit="s" totalDistance="3.1" totalDistanceUnit="mi" totalEnergyBurned="1255" totalEnergyBurnedUnit="kJ" sourceName="Apple Watch" startDate="2026-04-02 07:00:00 -0400" endDate="2026-04-02 07:30:00 -0400">
 </Workout>
 <Workout workoutActivityType="HKWorkoutActivityTypeSwimming" duration="40" durationUnit="min" sourceName="Apple Watch" startDate="2026-04-03 07:00:00 -0400" endDate="2026-04-03 07:40:00 -0400">
  <WorkoutStatistics type="HKQuantityTypeIdentifierDistanceSwimming" sum="1000" unit="m"/>
  <WorkoutStatistics type="HKQuantityTypeIdentifierActiveEnergyBurned" sum="300" unit="Cal"/>
 </Workout>
</HealthData>
"#;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("export.xml");
        std::fs::write(&path, XML).unwrap();
        let mut workouts = Vec::new();
        scan(&path, |item| {
            if let Item::Workout(w) = item {
                workouts.push(w);
            }
            Ok(())
        })
        .unwrap();
        let run = &workouts[0];
        assert_eq!(run.duration_min, Some(30.0));
        assert!((run.distance_km.unwrap() - 4.989).abs() < 0.001);
        assert!((run.kcal.unwrap() - 300.0).abs() < 0.1);
        let swim = &workouts[1];
        assert_eq!(swim.distance_km, Some(1.0));
        assert_eq!(swim.kcal, Some(300.0));
        let episode = workout_to_episode(swim, None);
        assert!(episode.body.contains("1.0 km"), "{}", episode.body);
    }

    /// Two trackers on the same night: the asleep total is one source's,
    /// never the sum of both.
    #[test]
    fn test_sleep_counts_one_source_per_night() {
        let mut table = DayTable::default();
        for (stage, hours) in [("AsleepCore", 4.0), ("AsleepDeep", 1.5), ("AsleepREM", 2.5)] {
            table.add(&sleep(stage, hours, "Apple Watch"));
        }
        table.add(&sleep("AsleepUnspecified", 7.5, "Sleep Tracker"));
        table.add(&sleep("Asleep", 7.0, "Another Tracker"));
        table.add(&sleep("InBed", 8.5, "Sleep Tracker"));
        let episodes = table.into_episodes(None);
        let body = &episodes[0].body;
        assert!(
            body.contains("sleep: 8h00m asleep [apple watch], 1h30m deep, 2h30m rem, 8h30m in bed [sleep tracker]"),
            "{body}"
        );
        assert!(
            episodes[0].title.contains("8h00m sleep"),
            "{}",
            episodes[0].title
        );

        // Without the Watch, the preferred remaining source alone counts.
        let mut table = DayTable::default();
        table.add(&sleep("AsleepUnspecified", 7.5, "Sleep Tracker"));
        table.add(&sleep("Asleep", 7.0, "Another Tracker"));
        let body = &table.into_episodes(None)[0].body;
        assert!(
            body.contains("sleep: 7h00m asleep [another tracker]"),
            "{body}"
        );
    }

    /// Read from an export: an app whose name contains "Watch" is a
    /// tracker of its own, so its record of the night doesn't add to the
    /// Apple Watch's.
    #[test]
    fn test_overlapping_trackers_stay_apart_through_scan() {
        const XML: &str = r#"<HealthData>
 <Record type="HKCategoryTypeIdentifierSleepAnalysis" sourceName="Ada’s Apple Watch" startDate="2026-04-02 23:00:00 -0400" endDate="2026-04-03 03:00:00 -0400" value="HKCategoryValueSleepAnalysisAsleepCore"/>
 <Record type="HKCategoryTypeIdentifierSleepAnalysis" sourceName="Ada’s Apple Watch" startDate="2026-04-03 03:00:00 -0400" endDate="2026-04-03 04:30:00 -0400" value="HKCategoryValueSleepAnalysisAsleepDeep"/>
 <Record type="HKCategoryTypeIdentifierSleepAnalysis" sourceName="Ada’s Apple Watch" startDate="2026-04-03 04:30:00 -0400" endDate="2026-04-03 07:00:00 -0400" value="HKCategoryValueSleepAnalysisAsleepREM"/>
 <Record type="HKCategoryTypeIdentifierSleepAnalysis" sourceName="SleepWatch" startDate="2026-04-02 23:00:00 -0400" endDate="2026-04-03 07:00:00 -0400" value="HKCategoryValueSleepAnalysisAsleepUnspecified"/>
</HealthData>
"#;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("export.xml");
        std::fs::write(&path, XML).unwrap();
        let mut table = DayTable::default();
        let mut sources = Vec::new();
        scan(&path, |item| {
            if let Item::Sample(s) = item {
                sources.push(s.source.clone());
                table.add(&s);
            }
            Ok(())
        })
        .unwrap();
        assert_eq!(
            sources,
            ["Apple Watch", "Apple Watch", "Apple Watch", "SleepWatch"]
        );
        let episodes = table.into_episodes(None);
        let body = &episodes[0].body;
        assert!(
            body.contains("sleep: 8h00m asleep [apple watch], 1h30m deep, 2h30m rem"),
            "{body}"
        );
        assert!(body.contains("sources: apple watch, sleepwatch"), "{body}");
        assert!(episodes[0].title.contains("8h00m sleep"));

        assert_eq!(normalize_source("Apple Watch de Ada"), "Apple Watch");
        assert_eq!(normalize_source("Ada’s Apple\u{a0}Watch"), "Apple Watch");
        assert_eq!(
            normalize_source("Watch Sleep Tracker"),
            "Watch Sleep Tracker"
        );
    }

    /// Two devices that share a source name (two Apple Watches), each
    /// recording the same night: the night is one device's eight hours,
    /// shown under the shared name — never the two added up.
    #[test]
    fn test_devices_sharing_a_name_keep_their_own_nights() {
        const XML: &str = r#"<HealthData>
 <Record type="HKCategoryTypeIdentifierSleepAnalysis" sourceName="Ada’s Apple Watch" startDate="2026-04-02 23:00:00 -0400" endDate="2026-04-03 07:00:00 -0400" value="HKCategoryValueSleepAnalysisAsleepCore"/>
 <Record type="HKCategoryTypeIdentifierSleepAnalysis" sourceName="Ada’s Apple Watch Ultra" startDate="2026-04-02 23:00:00 -0400" endDate="2026-04-03 07:00:00 -0400" value="HKCategoryValueSleepAnalysisAsleepCore"/>
 <Record type="HKQuantityTypeIdentifierStepCount" sourceName="Ada’s Apple Watch Ultra" unit="count" startDate="2026-04-03 09:00:00 -0400" endDate="2026-04-03 09:10:00 -0400" value="1200"/>
</HealthData>
"#;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("export.xml");
        std::fs::write(&path, XML).unwrap();
        let mut table = DayTable::default();
        scan(&path, |item| {
            if let Item::Sample(s) = item {
                table.add(&s);
            }
            Ok(())
        })
        .unwrap();
        let trackers: Vec<(&str, &str)> = table
            .map
            .keys()
            .map(|(_, metric, tracker)| (metric.as_str(), tracker.as_str()))
            .collect();
        assert_eq!(
            trackers,
            [
                ("Sleep/AsleepCore", "Ada’s Apple Watch"),
                ("Sleep/AsleepCore", "Ada’s Apple Watch Ultra"),
                ("StepCount", "Apple Watch"),
            ]
        );
        let episodes = table.into_episodes(None);
        let body = &episodes[0].body;
        assert!(body.contains("sleep: 8h00m asleep [apple watch]"), "{body}");
        assert!(body.ends_with("\nsources: apple watch"), "{body}");
        assert!(
            episodes[0].title.contains("8h00m sleep"),
            "{}",
            episodes[0].title
        );
        let devices: Vec<&str> = episodes[0]
            .entities
            .iter()
            .map(|e| e.name.as_str())
            .collect();
        assert_eq!(devices, ["Apple Watch"]);
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
