//! `hearthloom calendar` (0.5.95): connect Google Calendar (free/busy
//! only), set your working hours, and see when you're free — what reply
//! drafts offer when a thread needs a time (daemon google_calendar.rs).
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use crate::client::Client;
use crate::ops::prompt;

pub enum Op {
    Status,
    Setup {
        file: Option<PathBuf>,
    },
    Connect,
    Disconnect,
    Reset,
    Calendars,
    Free {
        days: i64,
        minutes: i64,
    },
    Hours {
        days: Option<String>,
        hours: Option<String>,
        time_zone: Option<String>,
        notice: Option<i64>,
        calendars: Option<String>,
    },
}

fn print_status(s: &Value) {
    if s["connected"] == json!(true) {
        println!(
            "Google Calendar: connected ({}) — free/busy only",
            s["account"].as_str().unwrap_or("account unknown")
        );
    } else if s["client"] == json!(true) {
        println!("Google Calendar: not connected yet — hearthloom calendar connect");
    } else {
        println!("Google Calendar: not set up — hearthloom calendar setup (your Google Cloud Desktop-app client)");
    }
    if let Some(id) = s["client_id"].as_str() {
        println!("  client: {id}");
    }
    println!(
        "  working hours: {} {}{} · at least {} hours' notice",
        s["days"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|d| d.as_str())
            .collect::<Vec<_>>()
            .join(","),
        s["hours"].as_str().unwrap_or("?"),
        s["time_zone"]
            .as_str()
            .map(|t| format!(" {t}"))
            .unwrap_or_else(|| " (your calendar's time zone)".into()),
        s["notice_hours"]
    );
    if let Some(c) = s["calendars"].as_str() {
        println!("  calendars that count: {c}");
    }
}

pub async fn run(client: &Client, op: Op, as_json: bool) -> Result<()> {
    match op {
        Op::Status => {
            let s = client.get("/v1/calendar", &[]).await?;
            if as_json {
                println!("{s}");
            } else {
                print_status(&s);
            }
        }
        Op::Setup { file } => {
            let saved = client.get("/v1/calendar", &[]).await?;
            let (id, secret, from_file) = setup_input(file, saved["client_id"].as_str())?;
            eprintln!("Checking it with Google…");
            let s = client
                .post(
                    "/v1/calendar/client",
                    &json!({"client_id": id, "client_secret": secret}),
                )
                .await?;
            if as_json {
                println!("{s}");
            } else {
                println!(
                    "✓ Google accepted the client — saved (sealed): {}\n  Next: hearthloom calendar connect",
                    s["client_id"].as_str().unwrap_or("?")
                );
            }
            if let Some(path) = from_file {
                eprintln!(
                    "{} holds the secret in plain text — delete it once you don't need it.",
                    path.display()
                );
            }
        }
        Op::Reset => {
            let s = client.post("/v1/calendar/reset", &json!({})).await?;
            if as_json {
                println!("{s}");
            } else {
                println!("✓ forgot the Google client and the sign-in here. Set it up again: hearthloom calendar setup");
                println!("  (to remove its access on Google's side too: myaccount.google.com/permissions)");
            }
        }
        Op::Connect => {
            let res = client.post("/v1/calendar/connect", &json!({})).await?;
            let url = res["url"].as_str().unwrap_or("");
            eprintln!(
                "1. Open this address in a browser signed in to your Google account:\n\n{url}\n"
            );
            eprintln!("2. Allow it (\"Google hasn't verified this app\" → Continue is expected for your own client).");
            eprintln!("3. Your browser then lands on a page that doesn't load (127.0.0.1:43279). That's expected:");
            eprintln!("   copy the whole address from the address bar and paste it here.\n");
            let landing = prompt("The address it ended on")?;
            if landing.is_empty() {
                bail!("nothing pasted — run hearthloom calendar connect again");
            }
            let done = client
                .post("/v1/calendar/connect/finish", &json!({"landing": landing}))
                .await?;
            if as_json {
                println!("{done}");
            } else {
                println!(
                    "✓ Google Calendar connected ({}). Try: hearthloom calendar free",
                    done["account"].as_str().unwrap_or("")
                );
            }
        }
        Op::Disconnect => {
            let s = client.post("/v1/calendar/disconnect", &json!({})).await?;
            if as_json {
                println!("{s}");
            } else {
                println!("✓ disconnected (the Google client stays saved; revoke access at myaccount.google.com/permissions)");
            }
        }
        Op::Calendars => {
            let res = client.get("/v1/calendar/calendars", &[]).await?;
            if as_json {
                println!("{res}");
                return Ok(());
            }
            for c in res["calendars"].as_array().into_iter().flatten() {
                println!(
                    "{}{}  {}",
                    c["name"].as_str().unwrap_or("?"),
                    if c["primary"] == json!(true) {
                        " (primary)"
                    } else if c["selected"] == json!(true) {
                        " (shown)"
                    } else {
                        ""
                    },
                    c["id"].as_str().unwrap_or("?")
                );
            }
        }
        Op::Free { days, minutes } => {
            let res = client
                .get(
                    "/v1/calendar/free",
                    &[("days", days.to_string()), ("minutes", minutes.to_string())],
                )
                .await?;
            if as_json {
                println!("{res}");
                return Ok(());
            }
            let lines: Vec<&str> = res["lines"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|l| l.as_str())
                .collect();
            println!(
                "free in your working hours, next {days} days ({}):",
                res["time_zone"].as_str().unwrap_or("?")
            );
            if lines.is_empty() {
                println!("  nothing of {minutes} minutes or more");
            }
            for l in lines {
                println!("  {l}");
            }
        }
        Op::Hours {
            days,
            hours,
            time_zone,
            notice,
            calendars,
        } => {
            let s = client
                .post(
                    "/v1/calendar",
                    &json!({"days": days, "hours": hours, "time_zone": time_zone,
                            "notice_hours": notice, "calendars": calendars}),
                )
                .await?;
            if as_json {
                println!("{s}");
            } else {
                print_status(&s);
            }
        }
    }
    Ok(())
}

/// What `setup` was given, for the daemon to read and check with Google:
/// the JSON Google lets you download (pasted, dropped into the terminal,
/// or `--file`), or the secret with the client ID beside it — an empty ID
/// keeps the one saved on the node.
fn setup_input(
    file: Option<PathBuf>,
    saved_id: Option<&str>,
) -> Result<(String, String, Option<PathBuf>)> {
    use std::io::{IsTerminal, Read};
    let read_file = |path: PathBuf| -> Result<(String, String, Option<PathBuf>)> {
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("couldn't read {}", path.display()))?;
        Ok((String::new(), text, Some(path)))
    };
    if let Some(path) = file {
        return read_file(path);
    }
    if !std::io::stdin().is_terminal() {
        // the JSON; or two lines, the ID then the secret; or the secret alone
        let mut text = String::new();
        std::io::stdin().read_to_string(&mut text)?;
        if text.trim_start().starts_with('{') {
            return Ok((String::new(), text, None));
        }
        let lines: Vec<&str> = text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect();
        return Ok(match lines.as_slice() {
            [] => bail!("nothing on stdin — the JSON, or the client ID and then the secret"),
            [secret] => (String::new(), secret.to_string(), None),
            [id, secret, ..] => (id.to_string(), secret.to_string(), None),
        });
    }
    eprintln!(
        "From console.cloud.google.com → Google Auth Platform → Clients → your Desktop app client."
    );
    eprintln!(
        "Paste the JSON you downloaded for it (or drop the file here), or just its client secret."
    );
    eprintln!("What you paste stays hidden.");
    let first = read_hidden("Client JSON or secret")?;
    if first.is_empty() {
        bail!("nothing pasted — run hearthloom calendar setup again");
    }
    if let Some(path) = dropped_file(&first) {
        return read_file(path);
    }
    if first.starts_with('{') {
        return Ok((String::new(), first, None));
    }
    if first.ends_with(".apps.googleusercontent.com") {
        // the ID came first; now the secret
        let secret = read_hidden("Client secret")?;
        return Ok((first, secret, None));
    }
    let id = match saved_id {
        Some(saved) => prompt(&format!("Client ID (Enter keeps {saved})"))?,
        None => prompt("Client ID")?,
    };
    Ok((id, first, None))
}

/// A prompt that doesn't echo, and takes a JSON file pasted over several
/// lines: it reads until the JSON is whole (an empty line ends it early).
fn read_hidden(label: &str) -> Result<String> {
    use std::io::{BufRead, Write};
    eprint!("{label}: ");
    std::io::stderr().flush().ok();
    let _ = std::process::Command::new("stty").arg("-echo").status();
    let read = (|| -> Result<String> {
        let mut stdin = std::io::stdin().lock();
        let mut text = String::new();
        stdin.read_line(&mut text)?;
        let mut lines = 1;
        while text.trim_start().starts_with('{')
            && serde_json::from_str::<Value>(&text).is_err()
            && lines < 400
        {
            let mut line = String::new();
            if stdin.read_line(&mut line)? == 0 || line.trim().is_empty() {
                break;
            }
            text.push_str(&line);
            lines += 1;
        }
        Ok(text.trim().to_string())
    })();
    let _ = std::process::Command::new("stty").arg("echo").status();
    eprintln!();
    read
}

/// A file dragged into a terminal arrives as its path — quoted, or with
/// its spaces escaped.
fn dropped_file(text: &str) -> Option<PathBuf> {
    let t = text.trim();
    let t = t
        .strip_prefix('\'')
        .and_then(|s| s.strip_suffix('\''))
        .or_else(|| t.strip_prefix('"').and_then(|s| s.strip_suffix('"')))
        .map(str::to_string)
        .unwrap_or_else(|| t.replace("\\ ", " "));
    let path = match t.strip_prefix("~/") {
        Some(rest) => PathBuf::from(std::env::var_os("HOME")?).join(rest),
        None => PathBuf::from(&t),
    };
    (path.is_absolute() && path.is_file()).then_some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_a_dropped_file_is_read_and_a_secret_is_not_a_path() {
        let dir = std::env::temp_dir().join(format!("hl-cal-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("client secret.json");
        std::fs::write(&file, "{}").unwrap();
        let shown = file.display().to_string();
        assert_eq!(dropped_file(&shown.replace(' ', "\\ ")), Some(file.clone()));
        assert_eq!(dropped_file(&format!("'{shown}'")), Some(file.clone()));
        assert_eq!(dropped_file(&format!("\"{shown}\"  ")), Some(file.clone()));
        assert_eq!(dropped_file("GOCSPX-fake-secret-for-tests"), None);
        assert_eq!(
            dropped_file(&dir.join("missing.json").display().to_string()),
            None
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
