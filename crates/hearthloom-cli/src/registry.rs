//! Ember registry client — the install-from-registry contract of
//! the hearth registry protocol's client contract (§5), implemented CLI-side:
//! resolve → fetch → sha256 → verify the developer's Ed25519 release
//! signature (and the registry counter-signature when present) → show
//! scopes → stage through the daemon's EXISTING upload+install
//! endpoints. The daemon's install path is unchanged — a registry
//! install is a verified sideload with a human scope grant.
//!
//! No default registry: `--registry <url>` (or HEARTHLOOM_REGISTRY_URL)
//! points anywhere that serves /.well-known/hearth-registry
//! (feature-detect: a missing well-known degrades to `<url>/registry/v1`).

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};

use crate::client::Client;

fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            other => format!("%{other:02X}"),
        })
        .collect()
}

pub struct Registry {
    /// the root URL as given (forwarded to a remote node's stage endpoint)
    root: String,
    /// e.g. https://registry.example.com/registry/v1
    base: String,
    /// org counter-sign key (base64 ed25519), when the registry has one
    countersign_key: Option<String>,
    http: reqwest::Client,
}

impl Registry {
    pub async fn connect(url: Option<String>) -> Result<Self> {
        // `--registry` or HEARTHLOOM_REGISTRY_URL (clap reads the env);
        // there is no built-in default — self-hosting talks to no outside
        // registry unless its owner chooses one.
        let root = url
            .filter(|u| !u.trim().is_empty())
            .ok_or_else(|| {
                anyhow!("no ember registry: pass --registry <url> or set HEARTHLOOM_REGISTRY_URL")
            })?
            .trim_end_matches('/')
            .to_string();
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(120))
            .build()?;
        // discovery document; absent = older/other registry, degrade
        let well_known = http
            .get(format!("{root}/.well-known/hearth-registry"))
            .send()
            .await;
        let (base, countersign_key) = match well_known {
            Ok(res) if res.status().is_success() => {
                let doc: Value = res.json().await.unwrap_or_default();
                let base = doc
                    .get("base")
                    .and_then(|b| b.as_str())
                    .map(String::from)
                    .unwrap_or_else(|| format!("{root}/registry/v1"));
                let key = doc
                    .pointer("/counterSign/publicKey")
                    .and_then(|k| k.as_str())
                    .filter(|k| !k.is_empty())
                    .map(String::from);
                (base, key)
            }
            _ => (format!("{root}/registry/v1"), None),
        };
        Ok(Registry {
            root,
            base,
            countersign_key,
            http,
        })
    }

    async fn get(&self, path: &str) -> Result<Value> {
        let res = self
            .http
            .get(format!("{}{path}", self.base))
            .send()
            .await
            .with_context(|| format!("couldn't reach the registry at {}", self.base))?;
        let status = res.status();
        if status.as_u16() == 404 {
            bail!("not found");
        }
        if !status.is_success() {
            bail!("the registry answered {status}");
        }
        res.json().await.map_err(Into::into)
    }

    pub async fn search(&self, query: &str, cloud: bool) -> Result<()> {
        let mut path = format!("/index?q={}", urlencode(query));
        if cloud {
            path.push_str("&context=cloud");
        }
        let res = self.get(&path).await?;
        let embers = res
            .get("embers")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        if embers.is_empty() {
            println!("nothing matched {query:?} at {}", self.base);
            return Ok(());
        }
        for e in embers {
            println!(
                "{:24} {:10} {}  — {}",
                e.get("name").and_then(|v| v.as_str()).unwrap_or("?"),
                e.get("latestVersion")
                    .and_then(|v| v.as_str())
                    .unwrap_or("-"),
                e.get("publisher").and_then(|v| v.as_str()).unwrap_or("?"),
                e.get("description").and_then(|v| v.as_str()).unwrap_or(""),
            );
        }
        Ok(())
    }

    pub async fn info(&self, name: &str) -> Result<()> {
        let res = self
            .get(&format!("/embers/{name}"))
            .await
            .map_err(|e| anyhow!("{name}: {e}"))?;
        let ember = res.get("ember").cloned().unwrap_or_default();
        println!(
            "{} — {}",
            ember
                .get("displayName")
                .or(ember.get("name"))
                .and_then(|v| v.as_str())
                .unwrap_or(name),
            ember
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or(""),
        );
        println!(
            "  publisher: {} · latest: {} · cloud-eligible: {}",
            ember
                .get("publisher")
                .and_then(|v| v.as_str())
                .unwrap_or("?"),
            ember
                .get("latestVersion")
                .and_then(|v| v.as_str())
                .unwrap_or("-"),
            ember
                .get("cloudEligible")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
        );
        for link in ["homepage", "sourceUrl"] {
            if let Some(url) = ember.get(link).and_then(|v| v.as_str()) {
                println!("  {link}: {url}");
            }
        }
        for v in res
            .get("versions")
            .and_then(|v| v.as_array())
            .unwrap_or(&vec![])
        {
            println!(
                "  {}  scopes [{}]{}  {}",
                v.get("version").and_then(|x| x.as_str()).unwrap_or("?"),
                v.get("scopes")
                    .and_then(|x| x.as_array())
                    .map(|a| a
                        .iter()
                        .filter_map(|s| s.as_str())
                        .collect::<Vec<_>>()
                        .join(", "))
                    .unwrap_or_default(),
                if v.get("countersigned")
                    .and_then(|x| x.as_bool())
                    .unwrap_or(false)
                {
                    "  ✓ registry-reviewed"
                } else {
                    ""
                },
                v.get("publishedAt")
                    .and_then(|x| x.as_str())
                    .map(|s| &s[..10.min(s.len())])
                    .unwrap_or(""),
            );
        }
        Ok(())
    }

    /// §5, steps 1–6. `client` is the target node (respects --url).
    #[allow(clippy::too_many_arguments)]
    pub async fn install(
        &self,
        client: &Client,
        name: &str,
        version: Option<String>,
        yes: bool,
        upgrade_host: bool,
    ) -> Result<()> {
        if client.is_remote() {
            // remote node: IT stages and verifies (its cloud policy is
            // enforced server-side); we show scopes and approve.
            let staged = self.remote_stage(client, name, version).await?;
            self.ensure_host(
                client,
                staged.get("requires_host").and_then(|v| v.as_str()),
                upgrade_host,
            )
            .await?;
            return self.remote_approve_install(client, staged, yes, &[]).await;
        }
        let ember = self
            .get(&format!("/embers/{name}"))
            .await
            .map_err(|e| anyhow!("{name}: {e} (try `hearthloom ember search`)"))?;
        let version = match version {
            Some(v) => v,
            None => ember
                .pointer("/ember/latestVersion")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("{name} has no released versions"))?
                .to_string(),
        };
        let v = self.fetch_and_verify(name, &version).await?;
        self.ensure_host(client, v.requires_host.as_deref(), upgrade_host)
            .await?;
        println!("{name} {version} by {}", v.publisher);
        println!(
            "  scopes it will hold: {}",
            if v.scopes.is_empty() {
                "(none)".into()
            } else {
                v.scopes.join(", ")
            }
        );
        println!("  {}", v.trust_line());
        if !yes && !confirm("install? [y/N] ")? {
            bail!("not installed");
        }
        self.stage_and_install(client, name, &version, v).await
    }

    /// Check installed embers against the registry; update outdated
    /// ones through the full verification chain. Two guards:
    /// - a version the registry has never seen = a LOCAL build — never
    ///   force-matched to a same-named registry ember (skipped, loudly)
    /// - an update that wants scopes beyond what's currently granted
    ///   always prompts with the delta, even under --yes
    pub async fn update(
        &self,
        client: &Client,
        only: Option<String>,
        check: bool,
        yes: bool,
    ) -> Result<()> {
        let installed = client.get("/v1/plugins", &[]).await?;
        let mut rows: Vec<Value> = installed
            .get("plugins")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        if let Some(only) = &only {
            rows.retain(|r| r.get("name").and_then(|v| v.as_str()) == Some(only.as_str()));
            if rows.is_empty() {
                bail!("{only} isn't installed on {}", client.base_url());
            }
        }
        if rows.is_empty() {
            println!("no embers installed.");
            return Ok(());
        }
        let mut outdated: Vec<(String, String, String, Vec<String>)> = Vec::new();
        for row in &rows {
            let name = row
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let have = row
                .get("version")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let granted: Vec<String> = row
                .get("granted_scopes")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|s| s.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            let page = match self.get(&format!("/embers/{name}")).await {
                Ok(p) => p,
                Err(_) => {
                    println!("· {name} {have} — not in this registry, skipped");
                    continue;
                }
            };
            // GUARD: the installed version must be one the registry
            // published, or this is a local build wearing a registry name.
            let known = page
                .get("versions")
                .and_then(|v| v.as_array())
                .map(|list| {
                    list.iter()
                        .any(|v| v.get("version").and_then(|x| x.as_str()) == Some(have.as_str()))
                })
                .unwrap_or(false);
            if !known {
                println!(
                    "· {name} {have} — local build (this version was never published here), skipped"
                );
                continue;
            }
            let latest = page
                .pointer("/ember/latestVersion")
                .and_then(|v| v.as_str())
                .unwrap_or(&have)
                .to_string();
            if semver_gt(&latest, &have) {
                println!("· {name} {have} → {latest}");
                outdated.push((name, have, latest, granted));
            } else {
                println!("· {name} {have} — up to date");
            }
        }
        if outdated.is_empty() {
            println!("nothing to update.");
            return Ok(());
        }
        if check {
            println!(
                "{} update{} available (run without --check to apply)",
                outdated.len(),
                if outdated.len() == 1 { "" } else { "s" }
            );
            return Ok(());
        }
        if client.is_remote() {
            for (name, _have, latest, granted) in outdated {
                let staged = self.remote_stage(client, &name, Some(latest)).await?;
                self.remote_approve_install(client, staged, yes, &granted)
                    .await?;
            }
            return Ok(());
        }
        for (name, _have, latest, granted) in outdated {
            let v = self.fetch_and_verify(&name, &latest).await?;
            let widened: Vec<&String> = v.scopes.iter().filter(|s| !granted.contains(s)).collect();
            println!("{name} → {latest} ({})", v.trust_line());
            let approved = if widened.is_empty() {
                yes || confirm(&format!("update {name}? [y/N] "))?
            } else {
                // scope widening NEVER rides --yes
                println!(
                    "  ⚠ this version asks for NEW scopes: {} (you granted: {})",
                    widened
                        .iter()
                        .map(|s| s.as_str())
                        .collect::<Vec<_>>()
                        .join(", "),
                    if granted.is_empty() {
                        "none".into()
                    } else {
                        granted.join(", ")
                    },
                );
                confirm(&format!("grant the wider scopes and update {name}? [y/N] "))?
            };
            if !approved {
                println!("  skipped {name}");
                continue;
            }
            self.stage_and_install(client, &name, &latest, v).await?;
        }
        Ok(())
    }

    /// §5 steps 2–4: download, hash-check, developer-signature-check,
    /// counter-signature-check. Everything install and update share.
    async fn fetch_and_verify(&self, name: &str, version: &str) -> Result<Verified> {
        let record = self.get(&format!("/embers/{name}/{version}")).await?;
        let download = record
            .get("download")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("the version record carries no download URL"))?;
        eprintln!("fetching {name} {version}…");
        let bytes = self
            .http
            .get(download)
            .send()
            .await?
            .error_for_status()?
            .bytes()
            .await?;
        let digest = {
            use sha2::Digest;
            format!("{:x}", sha2::Sha256::digest(&bytes))
        };
        let want = record
            .get("artifactSha256")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if digest != want {
            bail!("artifact hash mismatch (got {digest}, registry says {want}) — refusing");
        }
        let publisher = record
            .get("publisher")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let sig = record.get("signature").cloned().unwrap_or_default();
        let key_id = sig.get("keyId").and_then(|v| v.as_str()).unwrap_or("");
        let pub_doc = self.get(&format!("/publishers/{publisher}")).await?;
        let pubkey = pub_doc
            .get("keys")
            .and_then(|v| v.as_array())
            .and_then(|keys| {
                keys.iter()
                    .find(|k| k.get("keyId").and_then(|v| v.as_str()) == Some(key_id))
            })
            .and_then(|k| k.get("publicKey"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                anyhow!("publisher {publisher} has no active key {key_id} — refusing")
            })?;
        let mut scopes: Vec<String> = record
            .get("scopes")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|s| s.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        scopes.sort();
        let payload = format!(
            "hearth-ember-release.v1\nname={name}\nversion={version}\nprotocol={}\nscopes={}\nsha256={digest}\npublisher={publisher}\n",
            record.get("protocol").and_then(|v| v.as_i64()).unwrap_or(1),
            scopes.join(","),
        );
        verify_ed25519(
            pubkey,
            &payload,
            sig.get("sig").and_then(|v| v.as_str()).unwrap_or(""),
        )
        .context("developer signature verification FAILED — refusing")?;
        let reviewed = match (
            record.get("countersign").filter(|c| !c.is_null()),
            &self.countersign_key,
        ) {
            (Some(cs), Some(org_key)) => {
                // signed at second precision; may be served with millis
                let served = cs.get("approvedAt").and_then(|v| v.as_str()).unwrap_or("");
                let operator = cs.get("operator").and_then(|v| v.as_str()).unwrap_or("");
                let sig = cs.get("sig").and_then(|v| v.as_str()).unwrap_or("");
                let normalized = chrono::DateTime::parse_from_rfc3339(served)
                    .map(|t| {
                        t.with_timezone(&chrono::Utc)
                            .format("%Y-%m-%dT%H:%M:%SZ")
                            .to_string()
                    })
                    .unwrap_or_else(|_| served.to_string());
                let ok = [served.to_string(), normalized].iter().any(|ts| {
                    let payload = format!(
                        "hearth-ember-approval.v1\nsha256={digest}\napprovedAt={ts}\noperator={operator}\n"
                    );
                    verify_ed25519(org_key, &payload, sig).is_ok()
                });
                if !ok {
                    bail!("registry counter-signature verification FAILED — refusing");
                }
                true
            }
            _ => false,
        };
        let requires_host = record
            .get("manifest")
            .and_then(|m| m.get("requires"))
            .and_then(|r| r.get("host"))
            .and_then(|h| h.as_str())
            .map(String::from);
        Ok(Verified {
            bytes: bytes.to_vec(),
            scopes,
            publisher,
            reviewed,
            requires_host,
        })
    }

    async fn stage_and_install(
        &self,
        client: &Client,
        name: &str,
        version: &str,
        v: Verified,
    ) -> Result<()> {
        let staged = client.post_bytes("/v1/plugins/upload", v.bytes).await?;
        let path = staged
            .get("path")
            .and_then(|x| x.as_str())
            .ok_or_else(|| anyhow!("staging failed: {staged}"))?;
        let res = client
            .post(
                "/v1/plugins/install",
                &json!({"path": path, "scopes": v.scopes}),
            )
            .await?;
        println!(
            "✓ installed {} v{} — reload the dashboard for its pages",
            res.get("installed")
                .and_then(|x| x.as_str())
                .unwrap_or(name),
            res.get("version")
                .and_then(|x| x.as_str())
                .unwrap_or(version),
        );
        Ok(())
    }

    /// If the ember needs a newer hearthloom than the target runs,
    /// either upgrade the host first (--upgrade-host) or refuse with
    /// guidance. `req` is the ember's `requires.host` range.
    async fn ensure_host(
        &self,
        client: &Client,
        req: Option<&str>,
        upgrade_host: bool,
    ) -> Result<()> {
        let Some(req) = req.filter(|r| !r.trim().is_empty()) else {
            return Ok(());
        };
        let info = client.get("/v1/version", &[]).await.unwrap_or_default();
        let current = info
            .get("current")
            .and_then(|v| v.as_str())
            .unwrap_or("0.0.0")
            .to_string();
        if version_satisfies(&current, req) {
            return Ok(());
        }
        if !upgrade_host {
            bail!(
                "this ember needs hearthloom {req} but the node is {current} — \n                   re-run with --upgrade-host, or `hearthloom upgrade` first"
            );
        }
        println!("this ember needs hearthloom {req}; upgrading the host from {current}…");
        crate::ops::upgrade(client, false).await?;
        // wait for the daemon to come back on the new version
        for _ in 0..60 {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            if let Ok(h) = client.get("/health", &[]).await {
                let now = h.get("version").and_then(|v| v.as_str()).unwrap_or("");
                if version_satisfies(now, req) {
                    println!("host upgraded to {now}; continuing the install…");
                    return Ok(());
                }
            }
        }
        bail!("timed out waiting for the host to finish upgrading");
    }

    /// Ask a remote node to stage+verify from ITS side (cloud policy
    /// enforced there); returns the staged descriptor.
    async fn remote_stage(
        &self,
        client: &Client,
        name: &str,
        version: Option<String>,
    ) -> Result<Value> {
        client
            .post(
                "/v1/plugins/registry/stage",
                &json!({"name": name, "version": version, "registry": self.root}),
            )
            .await
    }

    /// Scope display + prompt + install for a remotely staged package.
    /// `granted` non-empty = an update: widening forces a prompt even
    /// under --yes (same rule as local updates).
    async fn remote_approve_install(
        &self,
        client: &Client,
        staged: Value,
        yes: bool,
        granted: &[String],
    ) -> Result<()> {
        let name = staged
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("?")
            .to_string();
        let version = staged
            .get("version")
            .and_then(|v| v.as_str())
            .unwrap_or("?")
            .to_string();
        let scopes: Vec<String> = staged
            .get("scopes")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|s| s.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let reviewed = staged
            .get("reviewed")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        println!(
            "{name} {version} by {} → {}",
            staged
                .get("publisher")
                .and_then(|v| v.as_str())
                .unwrap_or("?"),
            client.base_url(),
        );
        println!(
            "  scopes it will hold: {}",
            if scopes.is_empty() {
                "(none)".into()
            } else {
                scopes.join(", ")
            }
        );
        println!(
            "  {}",
            if reviewed {
                "✓ verified by the node · ✓ registry-reviewed"
            } else {
                "✓ verified by the node · NOT registry-reviewed"
            }
        );
        let widened: Vec<&String> = scopes.iter().filter(|s| !granted.contains(s)).collect();
        let approved = if granted.is_empty() || widened.is_empty() {
            yes || confirm("install? [y/N] ")?
        } else {
            println!(
                "  ⚠ this version asks for NEW scopes: {}",
                widened
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
            );
            confirm(&format!("grant the wider scopes and update {name}? [y/N] "))?
        };
        if !approved {
            bail!("not installed");
        }
        let path = staged
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("staging failed: {staged}"))?;
        let res = client
            .post(
                "/v1/plugins/install",
                &json!({"path": path, "scopes": scopes}),
            )
            .await?;
        println!(
            "✓ installed {} v{}",
            res.get("installed")
                .and_then(|x| x.as_str())
                .unwrap_or(&name),
            res.get("version")
                .and_then(|x| x.as_str())
                .unwrap_or(&version),
        );
        Ok(())
    }
}

struct Verified {
    bytes: Vec<u8>,
    scopes: Vec<String>,
    publisher: String,
    reviewed: bool,
    requires_host: Option<String>,
}

impl Verified {
    fn trust_line(&self) -> &'static str {
        if self.reviewed {
            "✓ signature verified · ✓ registry-reviewed (counter-signed)"
        } else {
            "✓ signature verified · NOT registry-reviewed — you trust this publisher"
        }
    }
}

fn confirm(prompt: &str) -> Result<bool> {
    use std::io::Write;
    eprint!("{prompt}");
    std::io::stderr().flush().ok();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(matches!(line.trim(), "y" | "Y" | "yes"))
}

/// Segment-wise numeric version compare: is `a` newer than `b`?
/// (String compare would call 0.9.0 newer than 0.10.0.)
fn semver_gt(a: &str, b: &str) -> bool {
    let parse = |v: &str| -> Vec<u64> {
        v.trim_start_matches('v')
            .split(['.', '-', '+'])
            .map(|seg| seg.parse::<u64>().unwrap_or(0))
            .collect()
    };
    let (a, b) = (parse(a), parse(b));
    for i in 0..a.len().max(b.len()) {
        let (x, y) = (
            a.get(i).copied().unwrap_or(0),
            b.get(i).copied().unwrap_or(0),
        );
        if x != y {
            return x > y;
        }
    }
    false
}

pub async fn uninstall(client: &Client, name: &str, yes: bool) -> Result<()> {
    if !yes {
        use std::io::Write;
        eprint!("put out {name}? Its key is revoked and its program stops; its data folder stays. [y/N] ");
        std::io::stderr().flush().ok();
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        if !matches!(line.trim(), "y" | "Y" | "yes") {
            bail!("not uninstalled");
        }
    }
    let res = client.delete(&format!("/v1/plugins/{name}"), &[]).await?;
    println!(
        "✓ uninstalled — data kept at {}",
        res.get("data_dir_kept")
            .and_then(|v| v.as_str())
            .unwrap_or("?"),
    );
    Ok(())
}

/// Client-side version-range check (mirror of the daemon's).
fn version_satisfies(current: &str, req: &str) -> bool {
    let parse = |s: &str| -> (u64, u64, u64) {
        let mut it = s
            .trim()
            .trim_start_matches('v')
            .split(['.', '-', '+'])
            .map(|x| x.parse::<u64>().unwrap_or(0));
        (
            it.next().unwrap_or(0),
            it.next().unwrap_or(0),
            it.next().unwrap_or(0),
        )
    };
    let req = req.trim();
    let (op, ver) = if let Some(r) = req.strip_prefix(">=") {
        (">=", r)
    } else if let Some(r) = req.strip_prefix("<=") {
        ("<=", r)
    } else if let Some(r) = req.strip_prefix('>') {
        (">", r)
    } else if let Some(r) = req.strip_prefix('<') {
        ("<", r)
    } else if let Some(r) = req.strip_prefix('=') {
        ("=", r)
    } else {
        (">=", req)
    };
    let (c, v) = (parse(current), parse(ver));
    match op {
        ">=" => c >= v,
        ">" => c > v,
        "<=" => c <= v,
        "<" => c < v,
        _ => c == v,
    }
}

fn verify_ed25519(pubkey_b64: &str, payload: &str, sig_b64: &str) -> Result<()> {
    use base64::Engine;
    use ed25519_dalek::{Signature, Verifier, VerifyingKey};
    let engine = base64::engine::general_purpose::STANDARD;
    let key_bytes: [u8; 32] = engine
        .decode(pubkey_b64)
        .context("bad public key encoding")?
        .try_into()
        .map_err(|_| anyhow!("public key isn't 32 bytes"))?;
    let sig_bytes: [u8; 64] = engine
        .decode(sig_b64)
        .context("bad signature encoding")?
        .try_into()
        .map_err(|_| anyhow!("signature isn't 64 bytes"))?;
    VerifyingKey::from_bytes(&key_bytes)?
        .verify(payload.as_bytes(), &Signature::from_bytes(&sig_bytes))
        .map_err(|_| anyhow!("signature does not match the payload"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_semver_gt() {
        assert!(semver_gt("0.10.0", "0.9.0"));
        assert!(semver_gt("1.0.0", "0.99.99"));
        assert!(semver_gt("v0.2.0", "0.1.9"));
        assert!(!semver_gt("0.1.0", "0.1.0"));
        assert!(!semver_gt("0.9.0", "0.10.0"));
        assert!(semver_gt("0.1.1", "0.1"));
    }

    #[test]
    fn test_verify_roundtrip() {
        use base64::Engine;
        use ed25519_dalek::{Signer, SigningKey};
        let engine = base64::engine::general_purpose::STANDARD;
        let sk = SigningKey::from_bytes(&[7u8; 32]);
        let payload = "hearth-ember-release.v1\nname=x\nversion=1\nprotocol=1\nscopes=ingest\nsha256=ab\npublisher=p\n";
        let sig = engine.encode(sk.sign(payload.as_bytes()).to_bytes());
        let pk = engine.encode(sk.verifying_key().to_bytes());
        verify_ed25519(&pk, payload, &sig).unwrap();
        assert!(verify_ed25519(&pk, "tampered", &sig).is_err());
    }
}
