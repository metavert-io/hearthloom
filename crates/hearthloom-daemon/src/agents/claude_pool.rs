//! The Claude account pool (0.5.85): which of the owner's Claude
//! subscriptions a Claude Code run uses, and the caps that keep each one
//! under the share of its limits the owner allows.
//!
//! Most people have one subscription, so the pool starts as just "your
//! usual sign-in" (Claude Code's own login on this machine) with no caps,
//! and behaves exactly as before. An extra account is an extra Claude
//! Code home (`~/.hearthloom/claude-accounts/<id>/`, passed as
//! `CLAUDE_CONFIG_DIR`), signed in through Claude Code's own flow
//! (`claude auth login`, which `hearthloom agents accounts login` runs in
//! the owner's terminal). Hearthloom never reads, stores or forwards a
//! login token: Claude Code keeps it, as it does for the usual sign-in.
//!
//! What the pool knows of usage comes from the runs: Claude Code's stream
//! reports `rate_limit_event`s with each window's utilization and reset
//! time. That event is undocumented, so readings are best-effort: a
//! missing reading means "unknown", never "free". A run that hits a limit
//! marks its account used up until the reset.
//!
//! `choose` (pure): accounts in the owner's order. Caps apply to every
//! account; moving on to the NEXT account at a cap or a limit happens only
//! with `switching` on (off by default). When nothing can run, the task
//! waits in the queue and starts once an account frees — or, with
//! `api_fallback` on and an API key on this node, runs billed to the key.
//!
//! Node-local: logins belong to this machine. Settings and readings live
//! in this node's consolidation_state and never sync; run usage lands in
//! the llm_usage ledger (subscription runs priced at zero), which does.
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::AppState;

pub const KEY: &str = "claude.pool";
const READING_PREFIX: &str = "claude.pool.reading.";
/// The usual sign-in: Claude Code's own login on this machine.
pub const MAIN: &str = "main";
/// Below a cap of 100%, stop this many points short: a run spends while it runs.
pub const MARGIN: f64 = 5.0;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Account {
    pub id: String,
    pub label: String,
    /// Use at most this % of the 5-hour limit (100 = up to Claude's own limit).
    #[serde(default = "full")]
    pub cap_5h: u8,
    /// … and of the weekly limit.
    #[serde(default = "full")]
    pub cap_7d: u8,
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Only for agents on these model families (opus, sonnet, haiku,
    /// fable); empty = any. Claude's per-family weekly limits are separate.
    #[serde(default)]
    pub models: Vec<String>,
}

fn full() -> u8 {
    100
}
fn yes() -> bool {
    true
}

impl Account {
    fn main() -> Self {
        Account {
            id: MAIN.into(),
            label: "usual".into(),
            cap_5h: 100,
            cap_7d: 100,
            enabled: true,
            models: vec![],
        }
    }

    fn cap_for(&self, window: &str) -> u8 {
        if window == "five_hour" {
            self.cap_5h
        } else if window.starts_with("seven_day") {
            self.cap_7d
        } else {
            100
        }
    }

    fn fits(&self, family: Option<&str>) -> bool {
        self.models.is_empty()
            || family.is_some_and(|f| self.models.iter().any(|m| m.eq_ignore_ascii_case(f)))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, Default, PartialEq)]
pub struct Pool {
    /// Move on to the next account when one is at its cap or limit.
    #[serde(default)]
    pub switching: bool,
    /// When nothing can run, use this node's Anthropic API key (billed).
    #[serde(default)]
    pub api_fallback: bool,
    /// Stop a run that crosses its account's cap (otherwise caps are
    /// checked before a run starts).
    #[serde(default)]
    pub stop_at_cap: bool,
    /// In the owner's order of preference.
    #[serde(default)]
    pub accounts: Vec<Account>,
}

impl Pool {
    /// The usual sign-in is always there; caps stay in 1–100; ids unique.
    fn normalized(mut self) -> Self {
        let mut seen = std::collections::HashSet::new();
        self.accounts.retain(|a| seen.insert(a.id.clone()));
        if !self.accounts.iter().any(|a| a.id == MAIN) {
            self.accounts.insert(0, Account::main());
        }
        for a in &mut self.accounts {
            a.cap_5h = a.cap_5h.clamp(1, 100);
            a.cap_7d = a.cap_7d.clamp(1, 100);
        }
        self
    }

    pub fn account(&self, id_or_label: &str) -> Option<&Account> {
        self.accounts
            .iter()
            .find(|a| a.id == id_or_label || a.label.eq_ignore_ascii_case(id_or_label))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, Default, PartialEq)]
pub struct Window {
    /// 0–1
    pub utilization: f64,
    /// Unix seconds
    pub resets_at: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default, PartialEq)]
pub struct Reading {
    #[serde(default)]
    pub windows: BTreeMap<String, Window>,
    #[serde(default)]
    pub status: Option<String>,
    /// Claude's "extra usage" state for the account (e.g. rejected).
    #[serde(default)]
    pub overage: Option<String>,
    #[serde(default)]
    pub exhausted_until: Option<i64>,
    #[serde(default)]
    pub checked_at: Option<i64>,
}

impl Reading {
    /// Fold in one `rate_limit_event`'s `rate_limit_info`.
    pub fn absorb(&mut self, info: &Value, now: i64) {
        let fraction = |v: f64| if v > 1.0 { v / 100.0 } else { v };
        if let Some(windows) = info.get("unifiedWindows").and_then(|w| w.as_object()) {
            for (name, w) in windows {
                if let (Some(u), Some(r)) = (
                    w.get("utilization").and_then(|x| x.as_f64()),
                    w.get("resetsAt").and_then(|x| x.as_i64()),
                ) {
                    self.windows.insert(
                        name.clone(),
                        Window {
                            utilization: fraction(u).max(0.0),
                            resets_at: r,
                        },
                    );
                }
            }
        }
        if let Some(status) = info.get("status").and_then(|s| s.as_str()) {
            self.status = Some(status.to_string());
            if status == "rejected" {
                self.exhausted_until = Some(
                    info.get("resetsAt")
                        .and_then(|x| x.as_i64())
                        .filter(|t| *t > now)
                        .unwrap_or(now + 3600),
                );
            }
        }
        if let Some(o) = info.get("overageStatus").and_then(|s| s.as_str()) {
            self.overage = Some(o.to_string());
        }
        self.checked_at = Some(now);
        self.forget_past(now);
    }

    fn forget_past(&mut self, now: i64) {
        self.windows.retain(|_, w| w.resets_at > now);
        if self.exhausted_until.is_some_and(|t| t <= now) {
            self.exhausted_until = None;
        }
    }

    /// Until when this account can't take a run under its caps (None: it can).
    fn blocked_until(&self, account: &Account, now: i64) -> Option<i64> {
        if let Some(t) = self.exhausted_until.filter(|t| *t > now) {
            return Some(t);
        }
        self.windows
            .iter()
            .filter(|(_, w)| w.resets_at > now)
            .filter(|(name, w)| {
                let cap = account.cap_for(name);
                cap < 100 && w.utilization * 100.0 + MARGIN >= f64::from(cap)
            })
            .map(|(_, w)| w.resets_at)
            .max()
    }

    /// Over the cap right now (for stopping a running run).
    pub fn over_cap(&self, account: &Account, now: i64) -> bool {
        self.windows.iter().any(|(name, w)| {
            let cap = account.cap_for(name);
            w.resets_at > now && cap < 100 && w.utilization * 100.0 >= f64::from(cap)
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Choice {
    Use(Account),
    /// Every account is held back; this node's API key runs it (billed).
    Api,
    /// Nothing can run; the first account frees at `until` (if known).
    Wait {
        until: Option<i64>,
    },
}

/// Which family a model id belongs to, for accounts limited to some.
pub fn family(model: Option<&str>) -> Option<&'static str> {
    let m = model?.to_ascii_lowercase();
    ["opus", "sonnet", "haiku", "fable"]
        .into_iter()
        .find(|f| m.contains(f))
}

pub fn choose(
    pool: &Pool,
    readings: &HashMap<String, Reading>,
    family: Option<&str>,
    api_key: bool,
    now: i64,
) -> Choice {
    let candidates: Vec<&Account> = pool
        .accounts
        .iter()
        .filter(|a| a.enabled && a.fits(family))
        .take(if pool.switching { usize::MAX } else { 1 })
        .collect();
    let mut until: Option<i64> = None;
    for account in candidates {
        match readings
            .get(&account.id)
            .and_then(|r| r.blocked_until(account, now))
        {
            None => return Choice::Use(account.clone()),
            Some(t) => until = Some(until.map_or(t, |u| u.min(t))),
        }
    }
    if pool.api_fallback && api_key {
        return Choice::Api;
    }
    Choice::Wait { until }
}

/* ————— this node's settings and readings ————— */

pub fn load(state: &AppState) -> Pool {
    state
        .store
        .consolidation_state(KEY)
        .ok()
        .flatten()
        .and_then(|raw| serde_json::from_str::<Pool>(&raw).ok())
        .unwrap_or_default()
        .normalized()
}

pub fn save(state: &AppState, pool: &Pool) -> Result<()> {
    let pool = pool.clone().normalized();
    state
        .store
        .set_consolidation_state(KEY, &serde_json::to_string(&pool)?)
}

pub fn reading(state: &AppState, id: &str) -> Reading {
    let mut r: Reading = state
        .store
        .consolidation_state(&format!("{READING_PREFIX}{id}"))
        .ok()
        .flatten()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default();
    r.forget_past(chrono::Utc::now().timestamp());
    r
}

pub fn update_reading(state: &AppState, id: &str, f: impl FnOnce(&mut Reading)) -> Reading {
    let mut r = reading(state, id);
    f(&mut r);
    let _ = state.store.set_consolidation_state(
        &format!("{READING_PREFIX}{id}"),
        &serde_json::to_string(&r).unwrap_or_default(),
    );
    r
}

fn readings(state: &AppState, pool: &Pool) -> HashMap<String, Reading> {
    pool.accounts
        .iter()
        .map(|a| (a.id.clone(), reading(state, &a.id)))
        .collect()
}

/// The Claude Code home of an extra account (None: the usual sign-in).
pub fn config_dir(state: &AppState, account: &Account) -> Option<PathBuf> {
    // Only an id this module made (`ca_` + hex, `new_id`) names a folder:
    // whatever else reaches here — a hand-edited pool, a future bug — gets
    // none, so nothing outside claude-accounts/ is ever created or removed.
    (account.id != MAIN && valid_id(&account.id))
        .then(|| state.home.root().join("claude-accounts").join(&account.id))
}

fn valid_id(id: &str) -> bool {
    id.strip_prefix("ca_").is_some_and(|rest| {
        (8..=32).contains(&rest.len()) && rest.bytes().all(|b| b.is_ascii_hexdigit())
    })
}

/// This node's Anthropic API key, for the fallback only.
fn api_key() -> Option<String> {
    std::env::var("ANTHROPIC_API_KEY")
        .ok()
        .filter(|k| !k.trim().is_empty())
}

/// What a run uses.
#[derive(Clone, Debug)]
pub struct Seat {
    /// None: the API-key fallback.
    pub account: Option<Account>,
    pub config_dir: Option<PathBuf>,
    pub api_key: Option<String>,
}

impl Seat {
    pub fn label(&self) -> String {
        match &self.account {
            Some(a) if a.id == MAIN => "your usual Claude sign-in".into(),
            Some(a) => format!("the {} Claude account", a.label),
            None => "the API key".into(),
        }
    }

    /// Environment the run adds to `child_env::inherited()`.
    pub fn env(&self) -> Vec<(String, String)> {
        let mut env = vec![];
        if let Some(dir) = &self.config_dir {
            env.push(("CLAUDE_CONFIG_DIR".into(), dir.display().to_string()));
        }
        if let Some(key) = &self.api_key {
            env.push(("ANTHROPIC_API_KEY".into(), key.clone()));
        }
        env
    }
}

pub fn when(t: i64) -> String {
    use chrono::TimeZone;
    chrono::Local
        .timestamp_opt(t, 0)
        .single()
        .map(|d| {
            if d.date_naive() == chrono::Local::now().date_naive() {
                d.format("%H:%M").to_string()
            } else {
                d.format("%a %H:%M").to_string()
            }
        })
        .unwrap_or_else(|| "later".into())
}

/// The account a run of `agent` should use now, or why it must wait.
pub fn pick(state: &AppState, model: Option<&str>) -> std::result::Result<Seat, String> {
    let pool = load(state);
    let now = chrono::Utc::now().timestamp();
    let key = api_key();
    match choose(
        &pool,
        &readings(state, &pool),
        family(model),
        key.is_some(),
        now,
    ) {
        Choice::Use(account) => Ok(Seat {
            config_dir: config_dir(state, &account),
            account: Some(account),
            api_key: None,
        }),
        Choice::Api => Ok(Seat {
            account: None,
            config_dir: None,
            api_key: key,
        }),
        Choice::Wait { until } => {
            let usable = pool
                .accounts
                .iter()
                .filter(|a| a.enabled && a.fits(family(model)))
                .count();
            Err(if usable == 0 {
                "waiting: no Claude account is turned on for this agent's model (☰ → Agents → Claude account)".to_string()
            } else {
                let who = if usable > 1 && pool.switching {
                    "every Claude account is at its limit".to_string()
                } else {
                    "your Claude account is at its limit".to_string()
                };
                match until {
                    Some(t) => format!("waiting: {who} until {} — the task starts then", when(t)),
                    None => format!("waiting: {who} — the task starts when it frees"),
                }
            })
        }
    }
}

/// Mark the account used up after a run hit Claude's limit.
pub fn exhausted(state: &AppState, account: &Account, until: Option<i64>) -> i64 {
    let now = chrono::Utc::now().timestamp();
    let r = update_reading(state, &account.id, |r| {
        let t = until
            .filter(|t| *t > now)
            .or(r.exhausted_until)
            .unwrap_or_else(|| {
                // the latest window near its end is the one that ran out
                r.windows
                    .values()
                    .filter(|w| w.utilization >= 0.95)
                    .map(|w| w.resets_at)
                    .max()
                    .unwrap_or(now + 3600)
            });
        r.exhausted_until = Some(t);
    });
    r.exhausted_until.unwrap_or(now + 3600)
}

/* ————— sign-in state (`claude auth status`, cached a minute) ————— */

static SIGN_IN: Mutex<Option<HashMap<String, (Instant, Value)>>> = Mutex::new(None);

async fn sign_in(claude: &std::path::Path, dir: Option<&PathBuf>, id: &str, fresh: bool) -> Value {
    if !fresh {
        let cache = SIGN_IN.lock().unwrap_or_else(|p| p.into_inner());
        if let Some((at, v)) = cache.as_ref().and_then(|c| c.get(id)) {
            if at.elapsed() < Duration::from_secs(60) {
                return v.clone();
            }
        }
    }
    let mut cmd = tokio::process::Command::new(claude);
    cmd.env_clear()
        .envs(crate::child_env::inherited())
        .args(["auth", "status", "--json"])
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    if let Some(dir) = dir {
        cmd.env("CLAUDE_CONFIG_DIR", dir);
    }
    let out = tokio::time::timeout(Duration::from_secs(15), cmd.output()).await;
    let v = match out {
        Ok(Ok(o)) => serde_json::from_slice::<Value>(&o.stdout)
            .map(|s| {
                json!({
                    "signed_in": s["loggedIn"].as_bool().unwrap_or(false),
                    "email": s["email"], "plan": s["subscriptionType"], "method": s["authMethod"],
                })
            })
            .unwrap_or_else(|_| json!({"signed_in": false, "error": "Claude Code didn't say"})),
        _ => json!({"signed_in": false, "error": "Claude Code didn't answer"}),
    };
    SIGN_IN
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get_or_insert_with(HashMap::new)
        .insert(id.to_string(), (Instant::now(), v.clone()));
    v
}

/// Where `claude` is on this machine: a Claude Code agent's own setting,
/// else the usual places.
pub fn claude_bin(state: &AppState) -> Option<PathBuf> {
    let agents = state.store.agents().ok()?;
    agents
        .iter()
        .filter(|a| a.harness == "claude_code")
        .find_map(|a| super::claude_code::find_claude(a).ok())
}

pub fn login_command(account: &Account) -> String {
    let label = &account.label;
    if label
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
    {
        format!("hearthloom agents accounts login {label}")
    } else {
        format!(
            "hearthloom agents accounts login '{}'",
            label.replace('\'', r"'\''")
        )
    }
}

/// The pool as the dashboard and the CLI show it.
pub async fn view(state: &AppState, fresh: bool) -> Value {
    let pool = load(state);
    let claude = claude_bin(state);
    let now = chrono::Utc::now().timestamp();
    let mut accounts = vec![];
    for a in &pool.accounts {
        let dir = config_dir(state, a);
        let r = reading(state, &a.id);
        let sign_in = match &claude {
            Some(bin) => sign_in(bin, dir.as_ref(), &a.id, fresh).await,
            None => {
                json!({"signed_in": false, "error": "Claude Code isn't installed on this machine"})
            }
        };
        accounts.push(json!({
            "id": a.id, "label": a.label, "usual": a.id == MAIN,
            "cap_5h": a.cap_5h, "cap_7d": a.cap_7d, "enabled": a.enabled, "models": a.models,
            "dir": dir.map(|d| d.display().to_string()),
            "login_command": (a.id != MAIN).then(|| login_command(a)),
            "sign_in": sign_in,
            "windows": r.windows, "exhausted_until": r.exhausted_until.filter(|t| *t > now),
            "extra_usage": r.overage, "checked_at": r.checked_at,
            "held_back_until": r.blocked_until(a, now),
        }));
    }
    json!({
        "switching": pool.switching, "api_fallback": pool.api_fallback,
        "stop_at_cap": pool.stop_at_cap, "has_api_key": api_key().is_some(),
        "margin": MARGIN, "accounts": accounts,
    })
}

fn new_id() -> String {
    format!("ca_{}", &uuid::Uuid::new_v4().simple().to_string()[..8])
}

fn clean_label(label: &str) -> Result<String> {
    let label = label.trim();
    if label.is_empty() || label.chars().count() > 40 || label.starts_with('-') {
        bail!("give the account a short name (up to 40 characters)");
    }
    Ok(label.to_string())
}

/// One change to the pool (`{"op": …}`), from the dashboard or the CLI.
pub async fn mutate(state: &AppState, body: &Value) -> Result<Value> {
    let mut pool = load(state);
    let op = body["op"].as_str().unwrap_or("");
    let find = |pool: &Pool| -> Result<Account> {
        let id = body["id"].as_str().unwrap_or("");
        pool.account(id)
            .cloned()
            .ok_or_else(|| anyhow!("no Claude account {id:?} in the pool"))
    };
    match op {
        "settings" => {
            for (k, slot) in [
                ("switching", &mut pool.switching),
                ("api_fallback", &mut pool.api_fallback),
                ("stop_at_cap", &mut pool.stop_at_cap),
            ] {
                if let Some(v) = body[k].as_bool() {
                    *slot = v;
                }
            }
            save(state, &pool)?;
        }
        "add" => {
            let label = clean_label(body["label"].as_str().unwrap_or(""))?;
            if pool.account(&label).is_some() {
                bail!("there's already an account called {label:?}");
            }
            let account = Account {
                id: new_id(),
                label,
                cap_5h: 100,
                cap_7d: 100,
                enabled: true,
                models: vec![],
            };
            if let Some(dir) = config_dir(state, &account) {
                std::fs::create_dir_all(&dir)?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
                }
            }
            pool.accounts.push(account.clone());
            save(state, &pool)?;
            return Ok(
                json!({"added": account.id, "login_command": login_command(&account),
                             "pool": view(state, true).await}),
            );
        }
        "set" => {
            let account = find(&pool)?;
            let label = match body["label"].as_str() {
                Some(l) => {
                    let l = clean_label(l)?;
                    if pool.account(&l).is_some_and(|a| a.id != account.id) {
                        bail!("there's already an account called {l:?}");
                    }
                    Some(l)
                }
                None => None,
            };
            let slot = pool
                .accounts
                .iter_mut()
                .find(|a| a.id == account.id)
                .expect("found above");
            if let Some(l) = label {
                slot.label = l;
            }
            for (k, cap) in [("cap_5h", &mut slot.cap_5h), ("cap_7d", &mut slot.cap_7d)] {
                if let Some(v) = body[k].as_u64() {
                    if !(1..=100).contains(&v) {
                        bail!("a cap is a percentage from 1 to 100");
                    }
                    *cap = v as u8;
                }
            }
            if let Some(v) = body["enabled"].as_bool() {
                slot.enabled = v;
            }
            if let Some(models) = body["models"].as_array() {
                let models: Vec<String> = models
                    .iter()
                    .filter_map(|m| m.as_str())
                    .map(|m| m.trim().to_ascii_lowercase())
                    .filter(|m| !m.is_empty())
                    .collect();
                if let Some(bad) = models
                    .iter()
                    .find(|m| !["opus", "sonnet", "haiku", "fable"].contains(&m.as_str()))
                {
                    bail!("{bad:?} isn't a model family (opus, sonnet, haiku, fable)");
                }
                slot.models = models;
            }
            save(state, &pool)?;
        }
        "move" => {
            let account = find(&pool)?;
            let from = pool
                .accounts
                .iter()
                .position(|a| a.id == account.id)
                .expect("found above");
            let to = body["to"]
                .as_u64()
                .map(|t| (t as usize).saturating_sub(1))
                .ok_or_else(|| anyhow!("move it to which place (1 = first)?"))?
                .min(pool.accounts.len() - 1);
            let a = pool.accounts.remove(from);
            pool.accounts.insert(to, a);
            save(state, &pool)?;
        }
        "remove" => {
            let account = find(&pool)?;
            if account.id == MAIN {
                bail!("your usual sign-in stays; turn it off instead");
            }
            if let (Some(dir), Some(bin)) = (config_dir(state, &account), claude_bin(state)) {
                // Claude Code signs the account out (its stored login goes
                // with it), then the folder goes
                let _ = tokio::time::timeout(
                    Duration::from_secs(20),
                    tokio::process::Command::new(bin)
                        .env_clear()
                        .envs(crate::child_env::inherited())
                        .env("CLAUDE_CONFIG_DIR", &dir)
                        .args(["auth", "logout"])
                        .stdin(std::process::Stdio::null())
                        .kill_on_drop(true)
                        .output(),
                )
                .await;
                let _ = std::fs::remove_dir_all(&dir);
            }
            pool.accounts.retain(|a| a.id != account.id);
            save(state, &pool)?;
            let _ = state
                .store
                .set_consolidation_state(&format!("{READING_PREFIX}{}", account.id), "{}");
        }
        "check" => {
            let account = find(&pool)?;
            if body["usage"].as_bool() == Some(true) {
                check_usage(state, &account).await?;
            }
            return Ok(json!({"pool": view(state, true).await}));
        }
        other => bail!("unknown pool change {other:?}"),
    }
    Ok(json!({"pool": view(state, false).await}))
}

/// A one-word run on the account, to read its usage now (it spends a few
/// tokens of that account). Claude Code reports the readings as it runs.
async fn check_usage(state: &AppState, account: &Account) -> Result<()> {
    let bin = claude_bin(state).ok_or_else(|| {
        anyhow!(
            "Claude Code isn't installed on this machine (or no Claude Code agent points at it)"
        )
    })?;
    let scratch = std::env::temp_dir().join(format!(
        "hearthloom-usage-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&scratch)?;
    let mut cmd = tokio::process::Command::new(bin);
    cmd.env_clear()
        .envs(crate::child_env::inherited())
        .current_dir(&scratch)
        .args([
            "-p",
            "Reply with the single word: ok",
            "--output-format",
            "stream-json",
            "--verbose",
            "--no-session-persistence",
            "--setting-sources",
            "project",
            "--strict-mcp-config",
            "--model",
            "haiku",
        ])
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    if let Some(dir) = config_dir(state, account) {
        cmd.env("CLAUDE_CONFIG_DIR", dir);
    }
    let out = tokio::time::timeout(Duration::from_secs(90), cmd.output()).await;
    let _ = std::fs::remove_dir_all(&scratch);
    let out = out
        .map_err(|_| anyhow!("Claude Code took too long"))?
        .map_err(|e| anyhow!("couldn't start Claude Code: {e}"))?;
    let now = chrono::Utc::now().timestamp();
    let mut saw = false;
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let Ok(event) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if event["type"] == "rate_limit_event" {
            saw = true;
            update_reading(state, &account.id, |r| {
                r.absorb(&event["rate_limit_info"], now)
            });
        }
        if event["type"] == "result" && event["is_error"] == json!(true) {
            let text = event["result"].as_str().unwrap_or("");
            if super::claude_code::is_limit_message(text) {
                exhausted(state, account, super::claude_code::limit_reset(text));
                return Ok(());
            }
            bail!(
                "Claude Code: {}",
                text.chars().take(300).collect::<String>()
            );
        }
    }
    if !saw {
        bail!("Claude Code didn't report usage for this account (is it signed in?)");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_only_an_id_this_module_made_names_a_folder() {
        assert!(valid_id(&new_id()));
        for bad in [
            "",
            "ca_",
            "main",
            "../x",
            "ca_../../etc",
            "ca_abc",
            "ca_ABCDEFGH/..",
            "ca_12345678/x",
        ] {
            assert!(!valid_id(bad), "{bad:?}");
        }
    }

    fn acct(id: &str, cap_5h: u8, cap_7d: u8) -> Account {
        Account {
            id: id.into(),
            label: id.into(),
            cap_5h,
            cap_7d,
            enabled: true,
            models: vec![],
        }
    }
    fn at(u: f64, resets_at: i64) -> Reading {
        let mut r = Reading::default();
        r.windows.insert(
            "seven_day".into(),
            Window {
                utilization: u,
                resets_at,
            },
        );
        r
    }
    const NOW: i64 = 1_000_000;

    #[test]
    fn test_one_account_with_no_caps_always_runs() {
        let pool = Pool::default().normalized();
        assert_eq!(pool.accounts[0].id, MAIN);
        let readings = HashMap::from([(MAIN.to_string(), at(0.99, NOW + 60))]);
        assert!(matches!(
            choose(&pool, &readings, None, false, NOW),
            Choice::Use(a) if a.id == MAIN
        ));
    }

    #[test]
    fn test_a_cap_holds_back_and_switching_moves_on() {
        let mut pool = Pool {
            accounts: vec![acct(MAIN, 100, 70), acct("b", 100, 100)],
            ..Default::default()
        };
        // 66% + the 5-point margin reaches a 70% cap
        let readings = HashMap::from([(MAIN.to_string(), at(0.66, NOW + 600))]);
        assert_eq!(
            choose(&pool, &readings, None, false, NOW),
            Choice::Wait {
                until: Some(NOW + 600)
            },
            "switching is off: wait for the first account"
        );
        pool.switching = true;
        assert!(
            matches!(choose(&pool, &readings, None, false, NOW), Choice::Use(a) if a.id == "b")
        );
        // under the cap: priority order wins
        let readings = HashMap::from([(MAIN.to_string(), at(0.40, NOW + 600))]);
        assert!(
            matches!(choose(&pool, &readings, None, false, NOW), Choice::Use(a) if a.id == MAIN)
        );
        // a reading past its reset no longer counts
        let readings = HashMap::from([(MAIN.to_string(), at(0.99, NOW - 1))]);
        assert!(
            matches!(choose(&pool, &readings, None, false, NOW), Choice::Use(a) if a.id == MAIN)
        );
    }

    #[test]
    fn test_a_used_up_account_waits_for_its_reset_then_the_key_if_allowed() {
        let mut pool = Pool {
            switching: true,
            accounts: vec![acct(MAIN, 100, 100), acct("b", 100, 100)],
            ..Default::default()
        };
        let gone = |t| Reading {
            exhausted_until: Some(t),
            ..Default::default()
        };
        let readings = HashMap::from([
            (MAIN.to_string(), gone(NOW + 900)),
            ("b".to_string(), gone(NOW + 300)),
        ]);
        assert_eq!(
            choose(&pool, &readings, None, true, NOW),
            Choice::Wait {
                until: Some(NOW + 300)
            },
            "the key only when the owner turned the fallback on"
        );
        pool.api_fallback = true;
        assert_eq!(
            choose(&pool, &readings, None, false, NOW),
            Choice::Wait {
                until: Some(NOW + 300)
            },
            "no key, no fallback"
        );
        assert_eq!(choose(&pool, &readings, None, true, NOW), Choice::Api);
    }

    #[test]
    fn test_model_limited_accounts_and_turned_off_ones_are_skipped() {
        let mut sonnet_only = acct("b", 100, 100);
        sonnet_only.models = vec!["sonnet".into()];
        let mut off = acct(MAIN, 100, 100);
        off.enabled = false;
        let pool = Pool {
            switching: true,
            accounts: vec![off, sonnet_only],
            ..Default::default()
        };
        let none = HashMap::new();
        assert!(matches!(
            choose(&pool, &none, family(Some("claude-sonnet-5-5")), false, NOW),
            Choice::Use(a) if a.id == "b"
        ));
        assert_eq!(
            choose(&pool, &none, family(Some("claude-opus-5-5")), false, NOW),
            Choice::Wait { until: None }
        );
        assert_eq!(
            choose(&pool, &none, None, false, NOW),
            Choice::Wait { until: None },
            "an unknown model never lands on a limited account"
        );
    }

    #[test]
    fn test_readings_come_from_claude_codes_rate_limit_event() {
        let mut r = Reading::default();
        r.absorb(
            &json!({"status": "allowed", "resetsAt": NOW + 50, "rateLimitType": "seven_day",
                    "overageStatus": "rejected",
                    "unifiedWindows": {"seven_day": {"utilization": 0.06, "resetsAt": NOW + 50},
                                       "five_hour": {"utilization": 41.0, "resetsAt": NOW + 20}}}),
            NOW,
        );
        assert_eq!(r.windows["seven_day"].utilization, 0.06);
        assert_eq!(
            r.windows["five_hour"].utilization, 0.41,
            "a percentage reads as a fraction"
        );
        assert_eq!(r.overage.as_deref(), Some("rejected"));
        assert_eq!(r.exhausted_until, None);
        r.absorb(&json!({"status": "rejected", "resetsAt": NOW + 20}), NOW);
        assert_eq!(r.exhausted_until, Some(NOW + 20));
    }

    #[test]
    fn test_the_usual_sign_in_is_always_in_the_pool() {
        let pool = Pool {
            accounts: vec![acct("b", 100, 100), acct("b", 50, 50)],
            ..Default::default()
        }
        .normalized();
        assert_eq!(
            pool.accounts
                .iter()
                .map(|a| a.id.as_str())
                .collect::<Vec<_>>(),
            [MAIN, "b"]
        );
        assert_eq!(
            pool.accounts[1].cap_5h, 100,
            "the first of a duplicate stays"
        );
    }
}
