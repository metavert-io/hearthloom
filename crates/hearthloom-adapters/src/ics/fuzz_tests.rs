//! Mutation fuzzing of the calendar importer: no input may panic, and each
//! must finish within `PER_INPUT`.
//!
//! The seeds are the matrix's calendars (`matrix_tests`: its 39 cases and
//! nine ref inputs). Each input is a seed changed one to eight times: a
//! flipped bit, deleted bytes, an inserted token (component structure,
//! RRULE and RECURRENCE-ID lines, extreme numbers, non-ASCII text), a
//! duplicated line, a line spliced in from another seed, a digit run
//! replaced with an extreme number, a random RRULE, or a truncation. The
//! sequence comes from a fixed seed through a small PRNG, so a run makes
//! the same inputs every time; each goes through `drafts` (imported on
//! 1 January 2030, like the matrix) on a thread of its own, so a panic is
//! caught and an input that runs too long is noticed while it runs.
//!
//! The normal test suite makes a short run (`SHORT_RUN` inputs). The long
//! run is ignored unless asked for; it runs for `HEARTHLOOM_ICS_FUZZ_SECS`
//! seconds (default 60) from `HEARTHLOOM_ICS_FUZZ_SEED` (default 1):
//!
//! ```text
//! HEARTHLOOM_ICS_FUZZ_SECS=3600 cargo test --release -p hearthloom-adapters \
//!     --lib ics::fuzz_tests::test_fuzz_long_run -- --ignored --nocapture
//! ```
//!
//! It prints the folder it saves into (under the system's temp dir, kept
//! after the run). An input that panics (one per distinct message) or runs
//! past `PER_INPUT` is saved there and its path printed, and the test
//! fails. `current.ics` there holds the input being run, so one that
//! aborts the process (a stack overflow, running out of memory) is left
//! behind too. Both runs save failing inputs the same way.

use std::collections::HashSet;
use std::io::{Seek, SeekFrom, Write};
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use chrono::NaiveDateTime;

use super::matrix_tests;

/// How long one input may take to import.
const PER_INPUT: Duration = Duration::from_secs(5);

/// The inputs the short run makes, and the seed it makes them from.
const SHORT_RUN: usize = 10_000;
const SHORT_RUN_SEED: u64 = 20_261_115;

/// The most failing inputs one run saves before it stops.
const MAX_SAVED: usize = 20;

/// Text inserted at a random place.
const TOKENS: &[&str] = &[
    "\r\n",
    "\r\n ",
    ";",
    ":",
    ",",
    "=",
    "\"",
    "\\",
    "\\n",
    "/",
    "Z",
    "T",
    "-",
    "+",
    "BEGIN:VEVENT\r\n",
    "END:VEVENT\r\n",
    "BEGIN:VCALENDAR\r\n",
    "END:VCALENDAR\r\n",
    "BEGIN:VTIMEZONE\r\nTZID:X\r\nBEGIN:STANDARD\r\nDTSTART:16010101T020000\r\n\
     TZOFFSETFROM:-0400\r\nTZOFFSETTO:-0500\r\nRRULE:FREQ=YEARLY;BYDAY=1SU;BYMONTH=11\r\n\
     END:STANDARD\r\nEND:VTIMEZONE\r\n",
    "BEGIN:VALARM\r\n",
    "END:VALARM\r\n",
    "BEGIN:STANDARD\r\n",
    "END:STANDARD\r\n",
    "BEGIN:DAYLIGHT\r\n",
    "END:DAYLIGHT\r\n",
    "RECURRENCE-ID;RANGE=THISANDFUTURE:20260309T130000Z\r\n",
    "RECURRENCE-ID;VALUE=DATE:20260309\r\n",
    "RECURRENCE-ID;TZID=America/New_York:20260304T150000\r\n",
    "RRULE:FREQ=SECONDLY;COUNT=5\r\n",
    "RRULE:FREQ=MINUTELY;BYSETPOS=-1;BYSECOND=1,2\r\n",
    "RRULE:FREQ=YEARLY;BYWEEKNO=-53;BYYEARDAY=-366;BYSETPOS=-1\r\n",
    "RRULE:FREQ=MONTHLY;BYDAY=-5SU,53MO;BYMONTHDAY=-31\r\n",
    "EXDATE;VALUE=DATE:20260310\r\n",
    "RDATE:20260304T200000Z\r\n",
    "RDATE;VALUE=PERIOD:20260305T170000Z/PT1H\r\n",
    "RDATE;VALUE=PERIOD:20260305T170000Z/20260304T170000Z\r\n",
    "DURATION:P1W\r\n",
    "DURATION:-PT1H\r\n",
    "DURATION:P99999999W\r\n",
    "TZID=",
    "TZID=Eastern Standard Time",
    "TZID=(UTC-05:00) Eastern Time",
    "TZID=/a/b/c/America/New_York",
    "TZOFFSETFROM:+2400\r\n",
    "TZOFFSETTO:-9999\r\n",
    "X-MICROSOFT-CDO-TZID:999\r\n",
    "VALUE=DATE",
    "BYSETPOS=0",
    "INTERVAL=0",
    "INTERVAL=65535",
    "INTERVAL=4294967296",
    "COUNT=0",
    "COUNT=4294967295",
    "UNTIL=00010101",
    "UNTIL=99991231T235959Z",
    "BYMONTHDAY=-31",
    "BYDAY=-5SU",
    "BYDAY=53MO",
    "BYDAY=1M\u{c9}",
    "BYHOUR=23,0,24",
    "BYMINUTE=60",
    "BYSECOND=60",
    "WKST=SU",
    "STATUS:CANCELLED\r\n",
    "SEQUENCE:-9223372036854775808\r\n",
    "\u{FEFF}",
    "\u{e9}",
    "\u{df}",
    "\u{130}",
    "\u{301}",
    "\u{202E}",
    "\u{0}",
    "\u{1F600}",
    "DTSTART:99991231T235959Z\r\n",
    "DTSTART:00000101T000000\r\n",
    "DTSTART;VALUE=DATE:20260229\r\n",
    "DTSTART;TZID=America/New_York:20260308T023000\r\n",
];

/// Numbers at and past the edges of what a calendar field holds.
const NUMBERS: &[&str] = &[
    "0",
    "-1",
    "1",
    "7",
    "24",
    "31",
    "53",
    "59",
    "60",
    "61",
    "366",
    "-366",
    "367",
    "9999",
    "10000",
    "65535",
    "65536",
    "2147483647",
    "2147483648",
    "4294967295",
    "18446744073709551615",
    "99999999999999999999",
    "00000000",
    "99991231",
    "10000101",
    "20260229",
    "20270229",
];

/// xorshift64: small, and the same sequence on every machine.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Rng {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    /// A number below `n` (0 when `n` is 0).
    fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next() % n as u64) as usize
        }
    }

    fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
        items[self.below(items.len())]
    }
}

/// The matrix's calendars.
fn seeds() -> Vec<String> {
    let mut seeds: Vec<String> = matrix_tests::cases()
        .iter()
        .map(matrix_tests::Case::ics)
        .collect();
    seeds.extend(matrix_tests::ref_inputs().into_iter().map(|i| i.ics));
    seeds
}

/// An RRULE line with up to four random parts, each with up to four
/// values.
fn random_rule(rng: &mut Rng) -> String {
    const FREQS: &[&str] = &[
        "SECONDLY", "MINUTELY", "HOURLY", "DAILY", "WEEKLY", "MONTHLY", "YEARLY",
    ];
    const PARTS: &[&str] = &[
        "BYSECOND",
        "BYMINUTE",
        "BYHOUR",
        "BYDAY",
        "BYMONTHDAY",
        "BYYEARDAY",
        "BYWEEKNO",
        "BYMONTH",
        "BYSETPOS",
        "COUNT",
        "INTERVAL",
        "UNTIL",
        "WKST",
    ];
    let mut rule = format!("RRULE:FREQ={}", rng.pick(FREQS));
    for _ in 0..rng.below(5) {
        let part = rng.pick(PARTS);
        let value = match part {
            "BYDAY" => rng
                .pick(&[
                    "MO",
                    "1MO",
                    "-1SU",
                    "2TU,FR",
                    "53SA",
                    "-53SU",
                    "1\u{c9}",
                    "\u{1F600}MO",
                ])
                .to_string(),
            "UNTIL" => rng
                .pick(&[
                    "20260320T000000Z",
                    "20260320",
                    "20260320T090000",
                    "00010101",
                ])
                .to_string(),
            "WKST" => rng.pick(&["SU", "MO", "XX"]).to_string(),
            _ => {
                let n = 1 + rng.below(4);
                (0..n)
                    .map(|_| rng.pick(NUMBERS))
                    .collect::<Vec<_>>()
                    .join(",")
            }
        };
        rule.push_str(&format!(";{part}={value}"));
    }
    rule.push_str("\r\n");
    rule
}

/// One random change to `input`.
fn mutate(input: &mut Vec<u8>, seeds: &[String], rng: &mut Rng) {
    let len = input.len();
    match rng.below(9) {
        // A flipped bit.
        0 if len > 0 => {
            let i = rng.below(len);
            input[i] ^= 1 << rng.below(8);
        }
        // Up to 24 bytes deleted.
        1 if len > 0 => {
            let i = rng.below(len);
            let n = 1 + rng.below(24);
            input.drain(i..(i + n).min(len));
        }
        // A token inserted.
        2 => {
            let token = rng.pick(TOKENS);
            let i = rng.below(len + 1);
            input.splice(i..i, token.bytes());
        }
        // A line of the input repeated somewhere in it.
        3 if len > 0 => {
            let text = String::from_utf8_lossy(input).into_owned();
            let lines: Vec<&str> = text.split_inclusive('\n').collect();
            let line = lines[rng.below(lines.len())].to_string();
            let i = rng.below(len + 1);
            input.splice(i..i, line.bytes());
        }
        // A run of digits replaced with an extreme number.
        4 if len > 0 => {
            let from = rng.below(len);
            if let Some(p) = input[from..].iter().position(u8::is_ascii_digit) {
                let start = from + p;
                let end = input[start..]
                    .iter()
                    .position(|b| !b.is_ascii_digit())
                    .map_or(input.len(), |n| start + n);
                let number = rng.pick(NUMBERS);
                input.splice(start..end, number.bytes());
            }
        }
        // A line from another seed spliced in.
        5 => {
            let seed = &seeds[rng.below(seeds.len())];
            let lines: Vec<&str> = seed.split_inclusive('\n').collect();
            let line = lines[rng.below(lines.len())];
            let i = rng.below(len + 1);
            input.splice(i..i, line.bytes());
        }
        // Truncated.
        6 if len > 0 => input.truncate(rng.below(len)),
        // The first RRULE line replaced (or, without one, a rule added to
        // the first event).
        7 => {
            let rule = random_rule(rng);
            let mut text = String::from_utf8_lossy(input).into_owned();
            match text.find("RRULE:") {
                Some(at) => {
                    let end = text[at..].find('\n').map_or(text.len(), |n| at + n + 1);
                    text.replace_range(at..end, &rule);
                }
                None => {
                    let at = text.find("END:VEVENT").unwrap_or(0);
                    text.insert_str(at, &rule);
                }
            }
            *input = text.into_bytes();
        }
        // An extreme number inserted.
        _ => {
            let number = rng.pick(NUMBERS);
            let i = rng.below(len + 1);
            input.splice(i..i, number.bytes());
        }
    }
}

/// The next input: a seed changed one to eight times.
fn next_input(seeds: &[String], rng: &mut Rng) -> String {
    let mut input = seeds[rng.below(seeds.len())].clone().into_bytes();
    for _ in 0..1 + rng.below(8) {
        mutate(&mut input, seeds, rng);
    }
    String::from_utf8_lossy(&input).into_owned()
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| payload.downcast_ref::<&str>().map(|s| s.to_string()))
        .unwrap_or_else(|| "a panic without a message".into())
}

/// How one input went.
enum Outcome {
    Finished(Duration),
    Panicked(String),
    TooSlow,
}

/// The importer on a thread of its own: an input that panics is caught
/// there, and one that runs past `PER_INPUT` is noticed while it runs (it
/// is left running; the run stops).
struct Importer {
    inputs: mpsc::Sender<String>,
    outcomes: mpsc::Receiver<Result<(), String>>,
}

impl Importer {
    fn start() -> Importer {
        let (inputs, received) = mpsc::channel::<String>();
        let (sent, outcomes) = mpsc::channel();
        std::thread::Builder::new()
            .name("ics-fuzz".into())
            .spawn(move || {
                let imported_at = NaiveDateTime::parse_from_str("20300101T000000", "%Y%m%dT%H%M%S")
                    .expect("import time");
                for input in received {
                    let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| {
                        // An input it refuses is fine; only a panic or a
                        // hang isn't.
                        let _ = super::drafts(&input, None, imported_at, "fuzz.ics");
                    }))
                    .map_err(|payload| panic_message(&*payload));
                    if sent.send(outcome).is_err() {
                        return;
                    }
                }
            })
            .expect("fuzz thread");
        Importer { inputs, outcomes }
    }

    fn run(&self, input: &str) -> Outcome {
        let began = Instant::now();
        self.inputs
            .send(input.to_string())
            .expect("fuzz thread running");
        match self.outcomes.recv_timeout(PER_INPUT) {
            Ok(Ok(())) => Outcome::Finished(began.elapsed()),
            Ok(Err(message)) => Outcome::Panicked(message),
            Err(mpsc::RecvTimeoutError::Timeout) => Outcome::TooSlow,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Outcome::Panicked("the fuzz thread stopped".into())
            }
        }
    }
}

/// What a run found.
struct Report {
    inputs: usize,
    slowest: Duration,
    /// Each failing input saved: what went wrong, and where it is.
    failures: Vec<String>,
}

/// Where a run saves inputs: a folder under the system's temp dir, kept
/// after the run.
fn save_dir() -> PathBuf {
    tempfile::Builder::new()
        .prefix("hearthloom-ics-fuzz-")
        .tempdir()
        .expect("temp dir")
        .keep()
}

/// Inputs from `seed` until `more` says stop (it is told how many have
/// run). Failing inputs are saved under `dir` (made when first needed);
/// with `track`, the input being run is in `<dir>/current.ics`.
fn fuzz(
    seed: u64,
    mut more: impl FnMut(usize) -> bool,
    dir: &mut Option<PathBuf>,
    track: bool,
) -> Report {
    let seeds = seeds();
    let mut rng = Rng::new(seed);
    let importer = Importer::start();
    let mut current = track.then(|| {
        let dir = dir.get_or_insert_with(save_dir);
        std::fs::File::create(dir.join("current.ics")).expect("current.ics")
    });
    let mut report = Report {
        inputs: 0,
        slowest: Duration::ZERO,
        failures: Vec::new(),
    };
    let mut seen = HashSet::new();
    while more(report.inputs) && report.failures.len() < MAX_SAVED {
        let input = next_input(&seeds, &mut rng);
        if let Some(file) = current.as_mut() {
            file.set_len(0).expect("current.ics");
            file.seek(SeekFrom::Start(0)).expect("current.ics");
            file.write_all(input.as_bytes()).expect("current.ics");
        }
        let n = report.inputs;
        report.inputs += 1;
        let (problem, stop) = match importer.run(&input) {
            Outcome::Finished(took) => {
                report.slowest = report.slowest.max(took);
                continue;
            }
            Outcome::Panicked(message) => {
                let first_line = message.lines().next().unwrap_or("").to_string();
                if !seen.insert(first_line) {
                    continue;
                }
                (format!("panicked: {message}"), false)
            }
            Outcome::TooSlow => (format!("still running after {PER_INPUT:?}"), true),
        };
        let dir = dir.get_or_insert_with(save_dir);
        let path = save(dir, n, &input);
        let failure = format!(
            "input {n} (seed {seed}) {problem}\n  saved: {}",
            path.display()
        );
        eprintln!("{failure}");
        report.failures.push(failure);
        if stop {
            break;
        }
    }
    report
}

fn save(dir: &Path, n: usize, input: &str) -> PathBuf {
    let path = dir.join(format!("failing-{n}.ics"));
    std::fs::write(&path, input).expect("saving a failing input");
    path
}

/// The short run, in the normal test suite: a fixed set of inputs.
#[test]
fn test_fuzz_short_run() {
    let mut dir = None;
    let began = Instant::now();
    let report = fuzz(SHORT_RUN_SEED, |n| n < SHORT_RUN, &mut dir, false);
    println!(
        "{} inputs in {:?}, the slowest {:?}",
        report.inputs,
        began.elapsed(),
        report.slowest
    );
    assert!(report.failures.is_empty(), "{}", report.failures.join("\n"));
    assert_eq!(report.inputs, SHORT_RUN);
}

/// The long run: `HEARTHLOOM_ICS_FUZZ_SECS` seconds (see the module docs).
#[test]
#[ignore = "long; run with HEARTHLOOM_ICS_FUZZ_SECS (see the module docs)"]
fn test_fuzz_long_run() {
    let env = |name: &str, default: u64| {
        std::env::var(name).map_or(default, |v| {
            v.trim()
                .parse()
                .unwrap_or_else(|_| panic!("{name} must be a whole number, not {v:?}"))
        })
    };
    let secs = env("HEARTHLOOM_ICS_FUZZ_SECS", 60);
    let seed = env("HEARTHLOOM_ICS_FUZZ_SEED", 1);
    let mut dir = Some(save_dir());
    println!(
        "fuzzing for {secs}s from seed {seed}; failing inputs and current.ics go in {}",
        dir.as_ref().expect("save dir").display()
    );
    let until = Instant::now() + Duration::from_secs(secs);
    let report = fuzz(seed, |_| Instant::now() < until, &mut dir, true);
    println!(
        "{} inputs, the slowest {:?}, {} failing",
        report.inputs,
        report.slowest,
        report.failures.len()
    );
    assert!(report.failures.is_empty(), "{}", report.failures.join("\n"));
}
