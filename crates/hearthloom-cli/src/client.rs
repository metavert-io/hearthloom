//! HTTP client for the daemon — the CLI never opens the database itself
//! (design law #4). If no daemon is running, one is spawned in the
//! background by re-execing this binary with `daemon run`.

use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use hearthloom_core::config::Config;
use hearthloom_core::Home;
use serde_json::Value;

pub struct Client {
    base_url: String,
    /// true = user pointed --url at another node; never auto-spawn
    remote: bool,
    http: reqwest::Client,
    home: Home,
}

impl Client {
    pub fn new(home: Home) -> Result<Self> {
        Self::with_url(home, None)
    }

    /// `--url` targets any node (the tenant included); a remote target
    /// is never auto-spawned. Key resolution: HEARTHLOOM_API_KEY env >
    /// the keychain/file key `hearthloom auth login` stored for this
    /// exact base URL.
    pub fn with_url(home: Home, url: Option<String>) -> Result<Self> {
        // Config::load also pulls ~/.hearthloom/.env into the process
        // env — including HEARTHLOOM_API_KEY on bootstrapped installs.
        let config = Config::load(&home.config_dir())?;
        let remote = url.is_some();
        // A daemon bound to every interface is still dialled on loopback.
        let host = match config.daemon.host.as_str() {
            "0.0.0.0" | "::" | "[::]" => "127.0.0.1",
            host => host,
        };
        let base_url = url
            .map(|u| u.trim_end_matches('/').to_string())
            .unwrap_or_else(|| format!("http://{host}:{}", config.daemon.port));
        let mut headers = reqwest::header::HeaderMap::new();
        let key = std::env::var("HEARTHLOOM_API_KEY")
            .ok()
            .filter(|k| !k.trim().is_empty())
            .or_else(|| crate::ops::stored_key(&base_url));
        if let Some(key) = key {
            if let Ok(value) = format!("Bearer {}", key.trim()).parse() {
                headers.insert(reqwest::header::AUTHORIZATION, value);
            }
        }
        Ok(Client {
            base_url,
            remote,
            http: reqwest::Client::builder()
                // Deep queries and big ingests are legitimately slow.
                .timeout(Duration::from_secs(600))
                .default_headers(headers)
                .build()?,
            home,
        })
    }

    /// Stream a response body to disk (the export zip).
    pub async fn download(&self, path: &str, out: &std::path::Path) -> Result<()> {
        let resp = self
            .http
            .get(format!("{}{}", self.base_url, path))
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(anyhow!("{} answered {}", path, resp.status()));
        }
        let bytes = resp.bytes().await?;
        std::fs::write(out, &bytes)?;
        Ok(())
    }

    pub fn is_remote(&self) -> bool {
        self.remote
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub async fn daemon_running(&self) -> bool {
        self.http
            .get(format!("{}/health", self.base_url))
            .timeout(Duration::from_millis(1500))
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false)
    }

    /// Make sure a daemon is up, spawning one detached if necessary.
    pub async fn ensure_daemon(&self) -> Result<()> {
        if self.remote {
            return Ok(()); // someone else's machine — never spawn here
        }
        if self.daemon_running().await {
            return Ok(());
        }
        let exe = std::env::current_exe().context("locating hearthloom binary")?;
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.home.daemon_log_path())
            .context("opening daemon.log")?;
        Command::new(exe)
            .args(["daemon", "run"])
            .stdin(Stdio::null())
            .stdout(Stdio::from(log.try_clone()?))
            .stderr(Stdio::from(log))
            .spawn()
            .context("spawning daemon")?;
        eprintln!("(started hearthloom daemon at {})", self.base_url);

        for _ in 0..40 {
            tokio::time::sleep(Duration::from_millis(200)).await;
            if self.daemon_running().await {
                return Ok(());
            }
        }
        Err(anyhow!(
            "daemon failed to start; see {}",
            self.home.daemon_log_path().display()
        ))
    }

    pub async fn get(&self, path: &str, query: &[(&str, String)]) -> Result<Value> {
        self.ensure_daemon().await?;
        let resp = self
            .http
            .get(format!("{}{}", self.base_url, path))
            .query(query)
            .send()
            .await?;
        Self::decode(resp).await
    }

    pub async fn post(&self, path: &str, body: &Value) -> Result<Value> {
        self.ensure_daemon().await?;
        let resp = self
            .http
            .post(format!("{}{}", self.base_url, path))
            .json(body)
            .send()
            .await?;
        Self::decode(resp).await
    }

    /// POST raw bytes (the ember package upload).
    pub async fn post_bytes(&self, path: &str, body: Vec<u8>) -> Result<Value> {
        self.ensure_daemon().await?;
        let resp = self
            .http
            .post(format!("{}{}", self.base_url, path))
            .body(body)
            .send()
            .await?;
        Self::decode(resp).await
    }

    pub async fn delete(&self, path: &str, query: &[(&str, String)]) -> Result<Value> {
        self.ensure_daemon().await?;
        let resp = self
            .http
            .delete(format!("{}{}", self.base_url, path))
            .query(query)
            .send()
            .await?;
        Self::decode(resp).await
    }

    async fn decode(resp: reqwest::Response) -> Result<Value> {
        let status = resp.status();
        let value: Value = resp
            .json()
            .await
            .with_context(|| format!("bad response ({status})"))?;
        if !status.is_success() {
            let message = value
                .get("error")
                .and_then(|e| e.as_str())
                .unwrap_or("unknown error");
            return Err(anyhow!("{status}: {message}"));
        }
        Ok(value)
    }
}
