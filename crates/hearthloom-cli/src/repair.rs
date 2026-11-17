//! `hearthloom repair`: memories an earlier version stored wrongly, replaced
//! by corrected copies that keep their place (core store/repair.rs). A dry
//! run unless `--apply`; it shows counts, episode ids and short reasons,
//! never what a message, an event, a conversation or a day says. Owner
//! only, vault unlocked. `chatgpt` and `health` name an export on this
//! machine, which the node reads in place (so they refuse `--url`).
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde_json::{json, Value};

use crate::client::Client;

pub enum Op {
    Mail {
        apply: bool,
    },
    Calendar {
        files: Vec<PathBuf>,
        apply: bool,
    },
    Chatgpt {
        export: PathBuf,
        apply: bool,
    },
    Health {
        export: PathBuf,
        apply: bool,
    },
    Copies {
        apply: bool,
    },
    PhotoDays {
        apply: bool,
    },
    List,
    /// Undo a run; without `yes`, only say what that means.
    Undo {
        run: String,
        yes: bool,
    },
}

/// What undoing a run means, said before it is done: the same corrections
/// can't be made again by asking for them again.
pub const UNDO_WARNING: &str =
    "Undoing puts the memories this run replaced back in view, here and on \
     your other nodes. It can't be taken back: a later repair finds these corrections \"repaired \
     before and undone\" and leaves them, until the importer reads the originals differently.";

/// Findings printed before "… and N more" (--json lists every one).
const SHOWN: usize = 50;

fn status_label(status: &str) -> &str {
    match status {
        "replace" => "would replace",
        "add" => "would add",
        "remint" => "would re-mint",
        "ledger" => "count put right",
        "private" => "go into the vault",
        "fine" => "read the same",
        "unchanged" => "look encoded but read the same now",
        "not_in_archive" => "not in a local archive",
        "not_in_export" => "not in the export",
        "not_stored" => "in the export, not stored",
        "no_message_id" => "no Message-ID",
        "undone" => "repaired before and undone",
        "unreadable" => "unreadable",
        "other_message" => "keyed otherwise",
        "unmatched" => "no start time to match",
        "same_event" => "named twice in the files",
        "no_turns" => "no turns on the kept branch",
        "partial" => "the export's own day",
        "from_phone" => "recorded by the phone",
        "held" => "held by a fuller copy",
        "uncounted" => "no count to name a copy by",
        "no_points" => "no photos stored here",
        "not_covered" => "show photos not stored here",
        "cut" => "list cut short",
        "open" => "still open",
        other => other,
    }
}

/// An export's full path on this machine (the node reads it there).
fn export_path(path: &std::path::Path) -> Result<String> {
    let full =
        std::fs::canonicalize(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(full.display().to_string())
}

/// What the run's new episodes cost the idle passes: fact extraction is
/// counted; every new copy is also new to the goal matching, whose fresh
/// matches feed the cascade pass (consolidate.rs: one model call a cycle
/// for up to `CASCADES_PER_CYCLE` matches).
fn cost_lines(res: &Value) -> Vec<String> {
    let count = |key: &str| res[key].as_u64().unwrap_or(0);
    let again = count("extraction_again");
    let mut lines = vec![format!(
        "facts to extract again: {again}{}",
        if again == 0 {
            ""
        } else {
            " — the extraction pass reads each new copy, at its source's tier"
        }
    )];
    let new = count("replace") + count("add") + count("remint");
    if new > 0 {
        lines.push(format!(
            "{new} new episode{} — each also new to the goal matching: a copy that matches \
             one of your goals goes to the cascade pass (one model call a cycle, for up to \
             6 matches)",
            if new == 1 { "" } else { "s" }
        ));
    }
    lines
}

fn print_report(res: &Value, what: &str) {
    let applied = res["dry_run"] == json!(false);
    if res["kind"] == "copies" {
        // the count first: every re-mint is a new episode on every node
        let n = res["remint"].as_u64().unwrap_or(0);
        println!(
            "{n} stored day{} and session{} to re-mint here — each one a new episode on every \
             node; your other nodes re-mint the copies they hold when the run reaches them",
            if n == 1 { "" } else { "s" },
            if n == 1 { "" } else { "s" },
        );
    }
    let counts: Vec<String> = res["counts"]
        .as_object()
        .into_iter()
        .flatten()
        .map(|(k, v)| format!("{} {}", v, status_label(k)))
        .collect();
    println!(
        "repair {} — {}",
        res["kind"].as_str().unwrap_or(what),
        if applied {
            "applied"
        } else {
            "dry run: nothing changed"
        }
    );
    println!(
        "checked {} {what}{}",
        res["checked"],
        if counts.is_empty() {
            String::new()
        } else {
            format!(": {}", counts.join(" · "))
        }
    );
    for note in res["notes"].as_array().into_iter().flatten() {
        println!("  note: {}", note.as_str().unwrap_or_default());
    }
    let findings = res["findings"].as_array().cloned().unwrap_or_default();
    for f in findings.iter().take(SHOWN) {
        println!(
            "  {}  {:<14} {}",
            f["episode"].as_str().unwrap_or("?"),
            f["status"].as_str().unwrap_or("?"),
            f["reason"].as_str().unwrap_or("")
        );
    }
    let total = res["findings_total"]
        .as_u64()
        .unwrap_or(findings.len() as u64);
    if total as usize > SHOWN {
        println!(
            "  … and {} more (--json lists them)",
            total as usize - SHOWN
        );
    }
    for line in cost_lines(res) {
        println!("{line}");
    }
    let count = |key: &str| res[key].as_u64().unwrap_or(0);
    if applied {
        match res["run"].as_str() {
            Some(run) => {
                let mut done = vec![
                    format!("replaced {}", count("replaced")),
                    format!("added {}", count("added")),
                ];
                if count("reminted") > 0 {
                    done.push(format!("re-minted {}", count("reminted")));
                }
                if count("ledgers") > 0 {
                    done.push(format!("{} photo-day counts put right", count("ledgers")));
                }
                println!(
                    "✓ run {run}: {} — your other nodes follow at their next sync; \
                     to undo it (for good): hearthloom repair undo {run} --yes",
                    done.join(", ")
                );
            }
            None => println!("nothing to replace"),
        }
        if count("made_private") > 0 {
            println!(
                "{} flagged \"do not remember\" went into the vault (an undo doesn't take them out)",
                count("made_private")
            );
        }
    } else if ["replace", "add", "remint", "ledger", "private"]
        .iter()
        .any(|k| count(k) > 0)
    {
        println!("run it again with --apply to make these changes");
    }
}

/// `allow_older_nodes`: apply even while one of the owner's nodes doesn't
/// know owner repairs yet (the node refuses otherwise and says which).
pub async fn run(client: &Client, op: Op, as_json: bool, allow_older_nodes: bool) -> Result<()> {
    let body = |apply: bool| json!({"apply": apply, "allow_older_nodes": allow_older_nodes});
    match op {
        Op::Mail { apply } => {
            let res = client.post("/v1/repair/mail", &body(apply)).await?;
            if as_json {
                println!("{res}");
            } else {
                print_report(&res, "mail memories");
            }
        }
        Op::Calendar { files, apply } => {
            let mut sent = Vec::new();
            for path in &files {
                let text = std::fs::read_to_string(path)
                    .with_context(|| format!("reading {}", path.display()))?;
                sent.push(json!({"name": path.display().to_string(), "text": text}));
            }
            let mut req = body(apply);
            req["files"] = json!(sent);
            let res = client.post("/v1/repair/calendar", &req).await?;
            if as_json {
                println!("{res}");
            } else {
                print_report(&res, "calendar events");
            }
        }
        Op::Chatgpt { export, apply } => {
            let mut req = body(apply);
            req["path"] = json!(export_path(&export)?);
            let res = client.post("/v1/repair/chatgpt", &req).await?;
            if as_json {
                println!("{res}");
            } else {
                print_report(&res, "stored conversations");
            }
        }
        Op::Health { export, apply } => {
            let mut req = body(apply);
            req["path"] = json!(export_path(&export)?);
            let res = client.post("/v1/repair/health", &req).await?;
            if as_json {
                println!("{res}");
            } else {
                print_report(&res, "day summaries and workouts");
            }
        }
        Op::Copies { apply } => {
            let res = client.post("/v1/repair/copies", &body(apply)).await?;
            if as_json {
                println!("{res}");
            } else {
                print_report(&res, "stored days and sessions");
            }
        }
        Op::PhotoDays { apply } => {
            let res = client.post("/v1/repair/photo-days", &body(apply)).await?;
            if as_json {
                println!("{res}");
            } else {
                print_report(&res, "photo days");
            }
        }
        Op::List => {
            let res = client.get("/v1/repairs", &[]).await?;
            if as_json {
                println!("{res}");
                return Ok(());
            }
            let runs = res["repairs"].as_array().cloned().unwrap_or_default();
            if runs.is_empty() {
                println!("no repairs yet — try: hearthloom repair mail");
            }
            for r in &runs {
                println!(
                    "{}  {:<8} replaced {}, added {}  {}  ({})",
                    r["id"].as_str().unwrap_or("?"),
                    r["kind"].as_str().unwrap_or("?"),
                    r["replaced"],
                    r["added"],
                    r["created_at"].as_str().unwrap_or("?"),
                    r["state"].as_str().unwrap_or("?"),
                );
            }
        }
        Op::Undo { run, yes: false } => {
            if as_json {
                println!(
                    "{}",
                    json!({"undone": null, "run": run, "needs": "--yes", "warning": UNDO_WARNING})
                );
            } else {
                println!("{UNDO_WARNING}");
                println!("nothing changed — to undo {run}: hearthloom repair undo {run} --yes");
            }
        }
        Op::Undo { run, yes: true } => {
            let res = client
                .post(&format!("/v1/repairs/{run}/undo"), &json!({}))
                .await?;
            if as_json {
                println!("{res}");
            } else {
                println!(
                    "✓ undone: the memories {run} replaced are back in view here ({} changed); \
                     your other nodes follow at their next sync",
                    res["restored"]
                );
                let added = res["added_stay"].as_i64().unwrap_or(0);
                if added > 0 {
                    println!(
                        "the {added} it added stay — retire them like any memory if you don't want them"
                    );
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The cost shown never says "no model spend" while new episodes would
    /// reach the goal matching and its cascades.
    #[test]
    fn test_the_cost_lines_name_every_new_episode() {
        let lines =
            cost_lines(&json!({"extraction_again": 0, "replace": 3, "add": 1, "remint": 0}));
        assert_eq!(lines[0], "facts to extract again: 0");
        assert!(lines[1].starts_with("4 new episodes — each also new to the goal matching"));
        assert!(lines.iter().all(|l| !l.contains("no model spend")));
        let lines = cost_lines(&json!({"extraction_again": 2, "replace": 2}));
        assert!(lines[0].starts_with("facts to extract again: 2 — the extraction pass"));
        assert!(lines[1].starts_with("2 new episodes"));
        assert_eq!(
            cost_lines(&json!({"extraction_again": 0, "replace": 0})),
            ["facts to extract again: 0"]
        );
    }
}
