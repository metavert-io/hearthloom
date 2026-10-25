//! Embedding-model download — the "semantic search" card's engine.
//!
//! The one deliberate third-party fetch in the product (cloud-doctrine
//! exception, user-consented from the card): pulls all-MiniLM-L6-v2
//! (~90MB ONNX + tokenizer) from Hugging Face into
//! `~/.hearthloom/models/`. Staged and atomic: files land in a
//! `.download/` staging dir, are VALIDATED by loading them through the
//! real tract Embedder plus one smoke embed, and only then renamed into
//! place — a failed or corrupt download leaves the node exactly as it
//! was (FTS-only, the mandated degraded mode). The embedder loads at
//! boot only, so success schedules the same self-restart the AI-key
//! save uses (launchd/Fly bring the daemon back in seconds).

use std::path::Path;
use std::sync::{Mutex, OnceLock};

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;

use crate::AppState;

const MODEL_FILE: &str = "all-MiniLM-L6-v2.onnx";
const TOKENIZER_FILE: &str = "tokenizer.json";
const MODEL_URL: &str =
    "https://huggingface.co/sentence-transformers/all-MiniLM-L6-v2/resolve/main/onnx/model.onnx";
const TOKENIZER_URL: &str =
    "https://huggingface.co/sentence-transformers/all-MiniLM-L6-v2/resolve/main/tokenizer.json";
/// Refuse to start without ~2× the payload free (disk pressure is a
/// real onboarding failure mode — fail at 0%, not 97%).
const NEED_FREE_BYTES: u64 = 250 * 1024 * 1024;

#[derive(Default, Clone)]
struct DlState {
    running: bool,
    bytes: u64,
    total: Option<u64>,
    error: Option<String>,
    done: bool,
}

fn cell() -> &'static Mutex<DlState> {
    static S: OnceLock<Mutex<DlState>> = OnceLock::new();
    S.get_or_init(Default::default)
}

/// Card status: file presence + live download progress.
pub fn status(models_dir: &Path, embedder_active: bool) -> Value {
    let s = cell().lock().unwrap().clone();
    let present =
        models_dir.join(MODEL_FILE).exists() && models_dir.join(TOKENIZER_FILE).exists();
    json!({
        "present": present,
        "active": embedder_active,
        "downloading": s.running,
        "bytes": s.bytes,
        "total": s.total,
        "error": s.error,
        "done": s.done,
    })
}

#[cfg(unix)]
fn free_bytes(dir: &Path) -> u64 {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(dir.as_os_str().as_bytes()).unwrap_or_default();
    let mut sv: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut sv) } == 0 {
        (sv.f_bavail as u64).saturating_mul(sv.f_frsize as u64)
    } else {
        u64::MAX // can't tell — don't block the download on a stat failure
    }
}

/// Kick off the download (idempotent: refuses while one runs or when the
/// model is already in place). Returns immediately; poll `status`.
pub fn start(state: AppState) -> Result<()> {
    let models = state.home.models_dir();
    if models.join(MODEL_FILE).exists() && models.join(TOKENIZER_FILE).exists() {
        bail!("the model is already installed");
    }
    std::fs::create_dir_all(&models)?;
    if free_bytes(&models) < NEED_FREE_BYTES {
        bail!("not enough disk space — need ~250MB free for the download and staging");
    }
    {
        let mut s = cell().lock().unwrap();
        if s.running {
            bail!("a download is already running");
        }
        *s = DlState {
            running: true,
            ..Default::default()
        };
    }
    tokio::spawn(async move {
        let result = run(&state).await;
        let mut s = cell().lock().unwrap();
        s.running = false;
        match result {
            Ok(()) => s.done = true,
            Err(err) => s.error = Some(format!("{err:#}")),
        }
    });
    Ok(())
}

async fn run(state: &AppState) -> Result<()> {
    let models = state.home.models_dir();
    let staging = models.join(".download");
    // a previous failed attempt's leftovers must not confuse validation
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging)?;

    fetch(MODEL_URL, &staging.join(MODEL_FILE), true).await?;
    fetch(TOKENIZER_URL, &staging.join(TOKENIZER_FILE), false).await?;

    // The real test: load through tract and embed one string. A wrong
    // opset, truncated file, or HTML error page fails HERE, before
    // anything reaches the live models dir.
    let st = staging.clone();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let embedder = hearthloom_core::embeddings::Embedder::load(&st)
            .context("downloaded files failed to load")?;
        embedder
            .embed("first light")
            .context("downloaded model failed a smoke embed")?;
        Ok(())
    })
    .await
    .map_err(anyhow::Error::from)?
    .inspect_err(|_| {
        let _ = std::fs::remove_dir_all(&staging);
    })?;

    std::fs::rename(staging.join(MODEL_FILE), models.join(MODEL_FILE))?;
    std::fs::rename(staging.join(TOKENIZER_FILE), models.join(TOKENIZER_FILE))?;
    let _ = std::fs::remove_dir_all(&staging);

    crate::monitor::log(
        state,
        "info",
        "embeddings_installed",
        "embedding model downloaded + validated — restarting to load it",
        None,
    );
    // boot-time loader: the restart IS the activation (the AI-key pattern)
    tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
        std::process::exit(0);
    });
    Ok(())
}

async fn fetch(url: &str, dest: &Path, track_progress: bool) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(600))
        .build()?;
    let mut resp = client.get(url).send().await?.error_for_status()?;
    if track_progress {
        cell().lock().unwrap().total = resp.content_length();
    }
    let mut file = tokio::fs::File::create(dest).await?;
    while let Some(chunk) = resp.chunk().await? {
        file.write_all(&chunk).await?;
        if track_progress {
            cell().lock().unwrap().bytes += chunk.len() as u64;
        }
    }
    file.flush().await?;
    Ok(())
}
