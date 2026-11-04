//! Embers you built yourself, from the command line (0.5.84): install or
//! upgrade one from a folder or a .zip, and the rest of the Integrations
//! card — enable, disable, settings.
//!
//! A package from a path is UNREVIEWED: nobody but its author has looked
//! at it. The human gate stays: the CLI shows what the dashboard's card
//! shows and asks. A script answers for the human with `--yes`, and names
//! what it trusts the ember with in `--grant`. An upgrade keeps the access
//! the installed version already holds, so `--yes` alone covers it unless
//! the new version asks for more; then it installs without the extra and
//! says how to re-run (exit 3), never granting what nobody named.
//!
//! Exit codes a script can act on (with `--json`, the same outcome is the
//! `status` field of one JSON object on stdout):
//! 0 `installed` / `upgraded`; 3 `installed_scopes_withheld` /
//! `upgraded_scopes_withheld` (it runs without `withheld`; `rerun` grants
//! them); 4 `needs_grant` / `needs_yes` (nothing installed: approval
//! missing); 5 `refused_hosted` (a hosted node takes reviewed embers only);
//! 1 anything else (`declined`, `error`).
use std::path::PathBuf;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};

use crate::client::Client;

pub const EXIT_WITHHELD: i32 = 3;
pub const EXIT_NEEDS_APPROVAL: i32 = 4;
pub const EXIT_REFUSED_HOSTED: i32 = 5;

/// Is this `install` argument a package on disk rather than a registry
/// name? A path says so (a separator, `.`/`~`, or a .zip); a bare word is
/// a registry name even if a folder of that name is here (`./name`).
pub fn is_path(arg: &str) -> bool {
    arg.contains('/')
        || arg.contains(std::path::MAIN_SEPARATOR)
        || arg.starts_with('.')
        || arg.starts_with('~')
        || arg.to_ascii_lowercase().ends_with(".zip")
}

/// What an install grants: what the ember asks for, limited to what was
/// named (`--grant`) plus, on an upgrade, what the installed version holds.
#[derive(Debug, PartialEq)]
pub struct Grants {
    pub granted: Vec<String>,
    /// asked for, not granted: the ember runs without these
    pub withheld: Vec<String>,
    /// named in --grant but not asked for: nothing to grant
    pub ignored: Vec<String>,
    /// held by the installed version, not asked for by this one: the key loses them
    pub dropped: Vec<String>,
}

pub fn decide_grants(requested: &[String], held: Option<&[String]>, grant: &[String]) -> Grants {
    let held = held.unwrap_or(&[]);
    let allowed = |s: &String| held.contains(s) || grant.contains(s);
    Grants {
        granted: requested.iter().filter(|s| allowed(s)).cloned().collect(),
        withheld: requested.iter().filter(|s| !allowed(s)).cloned().collect(),
        ignored: grant
            .iter()
            .filter(|s| !requested.contains(s))
            .cloned()
            .collect(),
        dropped: held
            .iter()
            .filter(|s| !requested.contains(s))
            .cloned()
            .collect(),
    }
}

/// Quote a path for the re-run line only when the shell needs it.
fn shell_word(word: &str) -> String {
    if word
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "/._-~+:=@%,".contains(c))
    {
        word.to_string()
    } else {
        format!("'{}'", word.replace('\'', r"'\''"))
    }
}

fn strings(v: Option<&Value>) -> Vec<String> {
    v.and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

pub struct InstallArgs<'a> {
    pub path: &'a str,
    pub grant: Vec<String>,
    pub yes: bool,
    pub upgrade_host: bool,
    pub as_json: bool,
    pub url: Option<&'a str>,
}

/// Install or upgrade an ember from a folder or a .zip. Returns the exit
/// code; the outcome is already printed.
pub async fn install_path(client: &Client, args: InstallArgs<'_>) -> Result<i32> {
    let path = PathBuf::from(args.path);
    let abs = std::fs::canonicalize(&path)
        .with_context(|| format!("no folder or file at {}", path.display()))?;
    let bytes = if abs.is_dir() {
        // the daemon's own parser, so a bad manifest fails here, not half-way
        hearthloom_daemon::plugins::load_manifest(&abs)?;
        hearthloom_daemon::plugins::pack_dir(&abs)?
    } else {
        std::fs::read(&abs).with_context(|| format!("reading {}", abs.display()))?
    };
    let staged = client.post_bytes("/v1/plugins/upload", bytes).await?;
    let staged_path = staged
        .get("path")
        .and_then(|x| x.as_str())
        .ok_or_else(|| anyhow!("the node didn't stage the package: {staged}"))?
        .to_string();
    let field = |k: &str| staged.get(k).and_then(|v| v.as_str()).unwrap_or("");
    let name = field("name").to_string();
    let version = field("version").to_string();
    let requested = strings(staged.get("requested_scopes"));
    let installed = staged.get("installed").filter(|v| !v.is_null());
    let from_version = installed
        .and_then(|i| i.get("version"))
        .and_then(|v| v.as_str())
        .map(String::from);
    let held = installed.map(|i| strings(i.get("granted_scopes")));

    let mut out = json!({
        "name": name, "version": version, "from_version": from_version,
        "requested": requested, "unreviewed": true,
    });
    let emit = |status: &str, mut out: Value, text: &str| {
        out["status"] = json!(status);
        if args.as_json {
            println!("{out}");
        } else if !text.is_empty() {
            eprintln!("{text}");
        }
    };

    if staged
        .get("hosted_refuses_unreviewed")
        .and_then(|v| v.as_bool())
        == Some(true)
    {
        let text = format!(
            "{name} wasn't installed: this is a hosted node, and it installs only embers \
             reviewed through a registry. Publish it to a registry and install it by name, \
             or ask the node's operator to allow unreviewed embers."
        );
        out["error"] = json!(text);
        emit("refused_hosted", out, &text);
        return Ok(EXIT_REFUSED_HOSTED);
    }

    let grants = decide_grants(&requested, held.as_deref(), &args.grant);
    // a person at the terminal approves everything it asks for, unless
    // they named less with --grant
    let interactive = !args.yes;
    let grants = if interactive && args.grant.is_empty() {
        Grants {
            granted: requested.clone(),
            withheld: vec![],
            ..grants
        }
    } else {
        grants
    };
    out["granted"] = json!(grants.granted);
    out["withheld"] = json!(grants.withheld);
    out["ignored"] = json!(grants.ignored);
    out["dropped"] = json!(grants.dropped);

    let url_flag = args
        .url
        .map(|u| format!(" --url {}", shell_word(u)))
        .unwrap_or_default();
    let rerun = format!(
        "hearthloom{url_flag} ember install {} --grant {} --yes",
        shell_word(&abs.display().to_string()),
        requested.join(",")
    );

    if !args.as_json || interactive {
        eprintln!("{}", gate(&staged, &grants, from_version.as_deref()));
    }

    if interactive {
        use std::io::IsTerminal;
        if !std::io::stdin().is_terminal() {
            let text = format!(
                "{name} wasn't installed: no one is at a terminal to approve it. \
                 Pass --yes, and --grant <scopes> for what it may do (it asks for: {}).",
                requested.join(", ")
            );
            out["error"] = json!(text);
            out["rerun"] = json!(rerun);
            emit("needs_yes", out, &text);
            return Ok(EXIT_NEEDS_APPROVAL);
        }
        let verb = if from_version.is_some() {
            "upgrade"
        } else {
            "install"
        };
        let access = if grants.granted.is_empty() {
            "no access".to_string()
        } else {
            format!("access to {}", grants.granted.join(", "))
        };
        if !confirm(&format!("{verb} {name} v{version} with {access}? [y/N] "))? {
            out["error"] = json!("not installed");
            emit("declined", out, "not installed");
            return Ok(1);
        }
    }

    if grants.granted.is_empty() && !requested.is_empty() {
        let text = format!(
            "{name} wasn't installed: nothing it asks for ({}) was granted. To allow it, run:\n  {rerun}",
            requested.join(", ")
        );
        out["error"] = json!(text);
        out["rerun"] = json!(rerun);
        emit("needs_grant", out, &text);
        return Ok(EXIT_NEEDS_APPROVAL);
    }

    crate::registry::Registry::ensure_host(
        client,
        staged.get("requires_host").and_then(|v| v.as_str()),
        args.upgrade_host,
    )
    .await?;
    let res = client
        .post(
            "/v1/plugins/install",
            &json!({"path": staged_path, "scopes": grants.granted}),
        )
        .await?;
    out["granted"] = res
        .get("granted_scopes")
        .cloned()
        .unwrap_or_else(|| json!(grants.granted));

    let upgraded = from_version.is_some();
    let done = if from_version.as_deref() == Some(version.as_str()) {
        format!("✓ reinstalled {name} v{version} — it kept its key and settings")
    } else if upgraded {
        format!(
            "✓ upgraded {name} v{} → v{version} — it kept its key and settings",
            from_version.as_deref().unwrap_or("?")
        )
    } else {
        format!("✓ installed {name} v{version} (unreviewed) — reload the dashboard for its pages")
    };
    if grants.withheld.is_empty() {
        if !args.as_json {
            println!("{done}");
        }
        emit(if upgraded { "upgraded" } else { "installed" }, out, "");
        return Ok(0);
    }
    out["rerun"] = json!(rerun);
    let text = format!(
        "⚠ {name} v{version} also asked for {} — not granted, so it may not work properly. \
         To allow it, re-run:\n  {rerun}",
        grants.withheld.join(", ")
    );
    if !args.as_json {
        println!("{done}");
    }
    emit(
        if upgraded {
            "upgraded_scopes_withheld"
        } else {
            "installed_scopes_withheld"
        },
        out,
        &text,
    );
    Ok(EXIT_WITHHELD)
}

/// The consent screen: what the dashboard's card shows before installing.
fn gate(staged: &Value, grants: &Grants, from_version: Option<&str>) -> String {
    let field = |k: &str| staged.get(k).and_then(|v| v.as_str()).unwrap_or("");
    let mut lines = vec![format!("{} v{}", field("name"), field("version"))];
    if let Some(from) = from_version {
        lines[0] = if from == field("version") {
            format!(
                "{} v{from}, already installed (a reinstall keeps its key and settings)",
                field("name")
            )
        } else {
            format!(
                "{} v{from} → v{} (an upgrade keeps its key and settings)",
                field("name"),
                field("version")
            )
        };
    }
    let publisher = field("publisher");
    lines.push(format!(
        "  by {}",
        if publisher.is_empty() {
            "an unnamed publisher"
        } else {
            publisher
        }
    ));
    if !field("description").is_empty() {
        lines.push(format!("  {}", field("description")));
    }
    lines.push(
        "  Unreviewed — installing means you trust its author with your machine.".to_string(),
    );
    let requested = strings(staged.get("requested_scopes"));
    lines.push(format!(
        "  asks for: {}",
        if requested.is_empty() {
            "nothing".to_string()
        } else {
            requested.join(", ")
        }
    ));
    lines.push(format!(
        "  will get: {}",
        if grants.granted.is_empty() {
            "nothing".to_string()
        } else {
            grants.granted.join(", ")
        }
    ));
    if !grants.withheld.is_empty() {
        lines.push(format!("  withheld: {}", grants.withheld.join(", ")));
    }
    if !grants.dropped.is_empty() {
        lines.push(format!(
            "  no longer asks for: {} (its key loses that access)",
            grants.dropped.join(", ")
        ));
    }
    if !grants.ignored.is_empty() {
        lines.push(format!(
            "  --grant names what it doesn't ask for: {} (ignored)",
            grants.ignored.join(", ")
        ));
    }
    match field("placement") {
        "local" => lines.push("  runs on: your own machines, not hosted nodes".to_string()),
        "cloud" => lines.push("  runs on: hosted nodes only".to_string()),
        _ => {}
    }
    if !field("runs").is_empty() {
        lines.push(format!("  starts a program: {}", field("runs")));
    }
    let publishes = strings(staged.get("publishes"));
    if !publishes.is_empty() {
        lines.push(format!(
            "  serves without sign-in: {}",
            publishes.join(", ")
        ));
    }
    if !field("requires_host").is_empty() {
        lines.push(format!("  needs hearthloom {}", field("requires_host")));
    }
    lines.join("\n")
}

fn confirm(prompt: &str) -> Result<bool> {
    use std::io::Write;
    eprint!("{prompt}");
    std::io::stderr().flush().ok();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(matches!(line.trim(), "y" | "Y" | "yes"))
}

pub async fn set_enabled(client: &Client, name: &str, enabled: bool, as_json: bool) -> Result<()> {
    let res = client
        .post(
            &format!("/v1/plugins/{}/enable", crate::urlencode(name)),
            &json!({ "enabled": enabled }),
        )
        .await?;
    if as_json {
        println!(
            "{}",
            json!({"status": if enabled { "enabled" } else { "disabled" }, "name": name, "enabled": res.get("enabled")})
        );
    } else if enabled {
        println!("✓ {name} enabled");
    } else {
        println!("✓ {name} disabled — it stops; its settings and data are kept");
    }
    Ok(())
}

/// The installed ember's declared settings (`config[]` in its manifest).
async fn config_schema(client: &Client, name: &str) -> Result<Vec<Value>> {
    let list = client.get("/v1/plugins", &[]).await?;
    let plugin = list
        .get("plugins")
        .and_then(|v| v.as_array())
        .and_then(|a| {
            a.iter()
                .find(|p| p.get("name").and_then(|n| n.as_str()) == Some(name))
        })
        .ok_or_else(|| anyhow!("no installed ember {name:?} (see `hearthloom ember list`)"))?;
    Ok(plugin
        .pointer("/manifest/config")
        .and_then(|c| c.as_array())
        .cloned()
        .unwrap_or_default())
}

/// A typed value from the command line, by the setting's declared type.
pub fn parse_setting(kind: &str, raw: &str) -> Result<Value> {
    Ok(match kind {
        // a whole number stays whole: an ember reading `15` may not take `15.0`
        "number" => match raw.trim().parse::<i64>() {
            Ok(n) => json!(n),
            Err(_) => raw
                .trim()
                .parse::<f64>()
                .ok()
                .and_then(serde_json::Number::from_f64)
                .map(Value::Number)
                .ok_or_else(|| anyhow!("{raw:?} isn't a number"))?,
        },
        "boolean" => match raw.trim().to_ascii_lowercase().as_str() {
            "true" | "on" | "yes" | "1" => json!(true),
            "false" | "off" | "no" | "0" => json!(false),
            _ => bail!("{raw:?} isn't on or off (use true/false)"),
        },
        "list" => json!(raw
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()),
        _ => json!(raw),
    })
}

/// `ember config <name>` lists its settings; `<key>` shows one;
/// `<key> <value>` saves it (checked against the declared type here AND
/// on the node); `--unset` clears it back to the default.
pub async fn config(
    client: &Client,
    name: &str,
    key: Option<String>,
    value: Option<String>,
    unset: bool,
    as_json: bool,
) -> Result<()> {
    let schema = config_schema(client, name).await?;
    let path = format!("/v1/plugins/{}/config", crate::urlencode(name));
    let values = client
        .get(&path, &[])
        .await?
        .get("values")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let item = |k: &str| {
        schema
            .iter()
            .find(|i| i.get("key").and_then(|x| x.as_str()) == Some(k))
            .cloned()
    };
    let Some(key) = key else {
        if as_json {
            println!(
                "{}",
                json!({"name": name, "settings": schema, "values": values})
            );
            return Ok(());
        }
        if schema.is_empty() {
            println!("{name} has no settings");
        }
        for i in &schema {
            let k = i.get("key").and_then(|x| x.as_str()).unwrap_or("?");
            let kind = i.get("type").and_then(|x| x.as_str()).unwrap_or("string");
            let shown = match values.get(k).filter(|v| !v.is_null()) {
                Some(v) => v.to_string(),
                None => match i.get("default") {
                    Some(d) => format!("{d} (default)"),
                    None => "not set".to_string(),
                },
            };
            println!("{k} ({kind}) = {shown}");
            if let Some(label) = i.get("label").and_then(|x| x.as_str()) {
                println!("    {label}");
            }
        }
        return Ok(());
    };
    let Some(def) = item(&key) else {
        let known: Vec<&str> = schema
            .iter()
            .filter_map(|i| i.get("key").and_then(|x| x.as_str()))
            .collect();
        bail!(
            "{key:?} isn't one of {name}'s settings ({})",
            if known.is_empty() {
                "it has none".to_string()
            } else {
                known.join(", ")
            }
        );
    };
    let kind = def.get("type").and_then(|x| x.as_str()).unwrap_or("string");
    let new = match (value, unset) {
        (Some(_), true) => bail!("give a value or --unset, not both"),
        (Some(raw), false) => Some(parse_setting(kind, &raw)?),
        (None, true) => Some(Value::Null),
        (None, false) => None,
    };
    let Some(new) = new else {
        let v = values.get(&key).cloned().unwrap_or(Value::Null);
        if as_json {
            println!(
                "{}",
                json!({"name": name, "key": key, "type": kind, "value": v, "default": def.get("default")})
            );
        } else if v.is_null() {
            match def.get("default") {
                Some(d) => println!("{d} (default)"),
                None => println!("not set"),
            }
        } else {
            println!("{v}");
        }
        return Ok(());
    };
    let res = client
        .post(&path, &json!({"values": {key.clone(): new.clone()}}))
        .await?;
    if as_json {
        println!(
            "{}",
            json!({"status": "saved", "name": name, "key": key, "value": new, "values": res.get("values")})
        );
    } else if new.is_null() {
        println!("✓ cleared {key} — back to its default; {name} picks it up within a few minutes");
    } else {
        println!("✓ saved {key} = {new} — {name} picks it up within a few minutes");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn test_a_first_install_gets_only_what_was_named() {
        let g = decide_grants(&v(&["read", "notify"]), None, &v(&["read"]));
        assert_eq!(g.granted, v(&["read"]));
        assert_eq!(g.withheld, v(&["notify"]));
        let g = decide_grants(&v(&["read", "notify"]), None, &[]);
        assert!(g.granted.is_empty());
        assert_eq!(g.withheld, v(&["read", "notify"]));
    }

    #[test]
    fn test_an_upgrade_keeps_what_it_held_and_no_more() {
        // same asks: --yes alone covers it
        let g = decide_grants(&v(&["read", "notify"]), Some(&v(&["read", "notify"])), &[]);
        assert_eq!(g.granted, v(&["read", "notify"]));
        assert!(g.withheld.is_empty());
        // asks for more: the extra waits for --grant
        let g = decide_grants(&v(&["read", "ingest"]), Some(&v(&["read"])), &[]);
        assert_eq!(g.granted, v(&["read"]));
        assert_eq!(g.withheld, v(&["ingest"]));
        let g = decide_grants(
            &v(&["read", "ingest"]),
            Some(&v(&["read"])),
            &v(&["ingest"]),
        );
        assert_eq!(g.granted, v(&["read", "ingest"]));
        assert!(g.withheld.is_empty());
        // asks for less: the key loses what it no longer asks for
        let g = decide_grants(&v(&["read"]), Some(&v(&["read", "notify"])), &[]);
        assert_eq!(g.granted, v(&["read"]));
        assert_eq!(g.dropped, v(&["notify"]));
    }

    #[test]
    fn test_grant_never_adds_what_the_ember_did_not_ask_for() {
        let g = decide_grants(&v(&["read"]), None, &v(&["read", "ingest"]));
        assert_eq!(g.granted, v(&["read"]));
        assert_eq!(g.ignored, v(&["ingest"]));
    }

    #[test]
    fn test_paths_and_registry_names_are_told_apart() {
        assert!(is_path("./minidash"));
        assert!(is_path("~/embers/minidash"));
        assert!(is_path("/Users/x/minidash"));
        assert!(is_path("minidash-1.2.0.zip"));
        assert!(is_path("."));
        assert!(!is_path("minidash"));
        assert!(!is_path("hpresence"));
    }

    #[test]
    fn test_settings_parse_by_their_declared_type() {
        assert_eq!(parse_setting("number", "15").unwrap(), json!(15));
        assert_eq!(parse_setting("number", "2.5").unwrap(), json!(2.5));
        assert!(parse_setting("number", "soon").is_err());
        assert_eq!(parse_setting("boolean", "off").unwrap(), json!(false));
        assert!(parse_setting("boolean", "maybe").is_err());
        assert_eq!(
            parse_setting("list", "a, b,,c").unwrap(),
            json!(["a", "b", "c"])
        );
        assert_eq!(parse_setting("string", "15").unwrap(), json!("15"));
    }

    #[test]
    fn test_the_rerun_line_survives_the_shell() {
        assert_eq!(shell_word("/Users/x/minidash"), "/Users/x/minidash");
        assert_eq!(shell_word("/Users/x/my ember"), "'/Users/x/my ember'");
        assert_eq!(shell_word("it's"), r"'it'\''s'");
    }
}
