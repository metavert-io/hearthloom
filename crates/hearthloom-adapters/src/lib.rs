//! Source adapters: each turns external material into `EpisodeDraft`s.
//! Adapters never touch the store — the daemon applies their batches
//! (design law #4).

pub mod apple_health;
pub mod chatlog;
pub mod chrome;
pub mod claude_code;
pub mod facebook;
pub mod git;
pub mod gmail_imap;
pub mod ics;
pub mod imessage;
pub mod linkedin;
pub mod mail_compose;
pub mod mbox;
pub mod openai;
pub mod photos;
pub mod rewind;
pub mod substack;

use chrono::{DateTime, NaiveDate, Utc};

/// The first UTC day that hasn't settled at `now`. The Chrome refresh
/// bundles a day into one episode and emits only the days before it: a
/// day emitted while it is still being written would be stored again,
/// whole, by the next read. A day settles a whole day after it ends, which
/// leaves room for visits that sync in late from the owner's other devices.
pub fn first_unsettled_day(now: DateTime<Utc>) -> NaiveDate {
    (now - chrono::Duration::days(1)).date_naive()
}

/// `head` followed by `entries`, within `cap` bytes: all of them when they
/// fit; otherwise the first ones that fit in half the room and the last
/// ones that fit in the rest, with the line `omitted(count)` between them —
/// so the opening and what was added last both show, whatever the length.
pub(crate) fn fit_entries(
    mut head: String,
    entries: &[String],
    cap: usize,
    omitted: impl Fn(usize) -> String,
) -> String {
    let total: usize = entries.iter().map(String::len).sum();
    if head.len() + total <= cap {
        for entry in entries {
            head.push_str(entry);
        }
        return head;
    }
    let marker = omitted(entries.len()).len();
    let room = cap.saturating_sub(head.len() + marker);
    let (mut first, mut used) = (0, 0);
    while first < entries.len() && used + entries[first].len() <= room / 2 {
        used += entries[first].len();
        first += 1;
    }
    let mut last = entries.len();
    while last > first && used + entries[last - 1].len() <= room {
        used += entries[last - 1].len();
        last -= 1;
    }
    for entry in &entries[..first] {
        head.push_str(entry);
    }
    head.push_str(&omitted(last - first));
    for entry in &entries[last..] {
        head.push_str(entry);
    }
    head
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_a_day_settles_a_whole_day_after_it_ends() {
        let at = |t: &str| t.parse::<DateTime<Utc>>().unwrap();
        let day = |d: &str| d.parse::<NaiveDate>().unwrap();
        // 2026-03-15 ends at 03-16T00:00 and settles at 03-17T00:00
        assert_eq!(
            first_unsettled_day(at("2026-03-16T23:59:59Z")),
            day("2026-03-15")
        );
        assert_eq!(
            first_unsettled_day(at("2026-03-17T00:00:00Z")),
            day("2026-03-16")
        );
    }

    #[test]
    fn test_entries_past_the_cap_keep_the_opening_and_the_latest() {
        let entries: Vec<String> = (0..100).map(|i| format!("entry {i:03}\n")).collect();
        let omitted = |n: usize| format!("[{n} not shown]\n");
        // fits: everything
        let all = fit_entries("head\n".into(), &entries[..5], 1000, omitted);
        assert_eq!(all.lines().count(), 6);
        // too long: the first, the last, and how many between
        let fitted = fit_entries("head\n".into(), &entries, 200, omitted);
        assert!(fitted.len() <= 200, "{}", fitted.len());
        assert!(fitted.contains("entry 000\n"));
        assert!(fitted.ends_with("entry 099\n"));
        let shown = fitted.lines().filter(|l| l.starts_with("entry")).count();
        let left_out: usize = fitted
            .lines()
            .find_map(|l| {
                l.strip_prefix('[')?
                    .strip_suffix(" not shown]")?
                    .parse()
                    .ok()
            })
            .unwrap();
        assert_eq!(shown + left_out, 100);
    }
}
