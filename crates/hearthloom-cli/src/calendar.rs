//! `hearthloom calendar` (0.5.95): connect Google Calendar (free/busy
//! only), set your working hours, and see when you're free — what reply
//! drafts offer when a thread needs a time (daemon google_calendar.rs).
use anyhow::{bail, Result};
use serde_json::{json, Value};

use crate::client::Client;
use crate::ops::{prompt, prompt_secret};

pub enum Op {
    Status,
    Setup,
    Connect,
    Disconnect,
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
        Op::Setup => {
            use std::io::{IsTerminal, Read};
            let (id, secret) = if std::io::stdin().is_terminal() {
                eprintln!("From console.cloud.google.com → Google Auth Platform → Clients → your Desktop app client.");
                (prompt("Client ID")?, prompt_secret("Client secret")?)
            } else {
                // two lines on stdin: the ID, then the secret
                let mut text = String::new();
                std::io::stdin().read_to_string(&mut text)?;
                let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty());
                (
                    lines.next().unwrap_or("").to_string(),
                    lines.next().unwrap_or("").to_string(),
                )
            };
            if id.is_empty() || secret.is_empty() {
                bail!("both the client ID and the client secret are needed");
            }
            client
                .post(
                    "/v1/calendar/client",
                    &json!({"client_id": id, "client_secret": secret}),
                )
                .await?;
            println!("✓ Google client saved (sealed). Next: hearthloom calendar connect");
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
