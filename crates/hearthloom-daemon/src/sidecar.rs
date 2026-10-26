//! The daemon-run ember supervisor — how an ember's sidecar runs where
//! there is no launchd: hosted (Linux) nodes, whose daemon is the only
//! process in the machine (docs/plugins.md deferred this in v1; the
//! Tesla ember is the first cloud-placed ember that needs it).
//!
//! One supervisor THREAD per running ember (std threads + std::process:
//! no runtime needed, so the blocking install/enable paths and the
//! monitor tick can call in directly). The thread spawns the binary with
//! the same three variables launchd hands it, appends its output to
//! `plugin.log`, restarts it with backoff when it exits (1s doubling to
//! 60s, reset after a minute of uptime — launchd's KeepAlive, with
//! manners), and stops on `stop`. On Linux the child gets
//! PR_SET_PDEATHSIG, so a daemon that dies hard takes its embers with
//! it instead of leaving orphans a restarted daemon would duplicate.
//!
//! macOS keeps launchd (plugins.rs dispatches); `HEARTHLOOM_SIDECAR_
//! BACKEND=supervisor` forces this path on a Mac for local testing.

use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::Result;

pub struct Spec {
    pub name: String,
    pub bin: PathBuf,
    pub env: Vec<(String, String)>,
    pub log: PathBuf,
    pub cwd: PathBuf,
}

struct Handle {
    stop: Arc<AtomicBool>,
    child: Arc<Mutex<Option<Child>>>,
}

fn table() -> &'static Mutex<HashMap<String, Handle>> {
    static TABLE: OnceLock<Mutex<HashMap<String, Handle>>> = OnceLock::new();
    TABLE.get_or_init(|| Mutex::new(HashMap::new()))
}

const BACKOFF_MAX: Duration = Duration::from_secs(60);
const HEALTHY_AFTER: Duration = Duration::from_secs(60);
const LOG_ROTATE_BYTES: u64 = 10 * 1024 * 1024;

/// Start supervising an ember (idempotent: an already-supervised ember
/// is left alone).
pub fn start(spec: Spec) -> Result<()> {
    let mut table = table().lock().unwrap();
    if table.contains_key(&spec.name) {
        return Ok(());
    }
    let stop = Arc::new(AtomicBool::new(false));
    let child = Arc::new(Mutex::new(None));
    table.insert(
        spec.name.clone(),
        Handle { stop: stop.clone(), child: child.clone() },
    );
    std::thread::Builder::new()
        .name(format!("ember-{}", spec.name))
        .spawn(move || supervise(spec, stop, child))?;
    Ok(())
}

/// Is a supervised child alive right now?
pub fn running(name: &str) -> bool {
    let table = table().lock().unwrap();
    let Some(handle) = table.get(name) else {
        return false;
    };
    let mut child = handle.child.lock().unwrap();
    matches!(child.as_mut().map(|c| c.try_wait()), Some(Ok(None)))
}

/// Stop an ember: no more restarts; SIGTERM, then SIGKILL after 5s.
pub fn stop(name: &str) {
    let handle = table().lock().unwrap().remove(name);
    if let Some(handle) = handle {
        handle.stop.store(true, Ordering::SeqCst);
        terminate(&handle.child);
    }
}

/// Stop every supervised ember (daemon shutdown).
pub fn stop_all() {
    let names: Vec<String> = table().lock().unwrap().keys().cloned().collect();
    for name in names {
        stop(&name);
    }
}

fn terminate(child: &Arc<Mutex<Option<Child>>>) {
    let mut guard = child.lock().unwrap();
    let Some(c) = guard.as_mut() else { return };
    #[cfg(unix)]
    unsafe {
        libc::kill(c.id() as libc::pid_t, libc::SIGTERM);
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if matches!(c.try_wait(), Ok(Some(_))) {
            *guard = None;
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = c.kill();
    let _ = c.wait();
    *guard = None;
}

fn log_line(log: &PathBuf, msg: &str) {
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(log) {
        let _ = writeln!(f, "[hearthloom {}] {msg}", hearthloom_core::time::now_utc());
    }
}

/// Sleep in short slices so a stop request is honored promptly.
fn nap(total: Duration, stop: &AtomicBool) {
    let until = Instant::now() + total;
    while Instant::now() < until && !stop.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn supervise(spec: Spec, stop: Arc<AtomicBool>, child: Arc<Mutex<Option<Child>>>) {
    let mut backoff = Duration::from_secs(1);
    while !stop.load(Ordering::SeqCst) {
        // keep the log bounded: one generation of history
        if std::fs::metadata(&spec.log).map(|m| m.len() > LOG_ROTATE_BYTES).unwrap_or(false) {
            let _ = std::fs::rename(&spec.log, spec.log.with_extension("log.1"));
        }
        let spawned = (|| -> std::io::Result<Child> {
            let out = std::fs::OpenOptions::new().create(true).append(true).open(&spec.log)?;
            let err = out.try_clone()?;
            let mut cmd = Command::new(&spec.bin);
            cmd.current_dir(&spec.cwd)
                .envs(spec.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
                .stdin(Stdio::null())
                .stdout(out)
                .stderr(err);
            #[cfg(target_os = "linux")]
            {
                use std::os::unix::process::CommandExt;
                // SAFETY: prctl is async-signal-safe; no allocation here.
                unsafe {
                    cmd.pre_exec(|| {
                        libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
                        Ok(())
                    });
                }
            }
            cmd.spawn()
        })();
        let started = Instant::now();
        match spawned {
            Ok(c) => {
                log_line(&spec.log, &format!("started {} (pid {})", spec.name, c.id()));
                *child.lock().unwrap() = Some(c);
                // poll (not wait): stop() needs the handle to signal it
                let status = loop {
                    if stop.load(Ordering::SeqCst) {
                        return; // stop() terminates the child
                    }
                    let mut guard = child.lock().unwrap();
                    match guard.as_mut().map(|c| c.try_wait()) {
                        Some(Ok(Some(status))) => {
                            *guard = None;
                            break Some(status);
                        }
                        Some(Ok(None)) => {}
                        Some(Err(_)) | None => {
                            *guard = None;
                            break None;
                        }
                    }
                    drop(guard);
                    std::thread::sleep(Duration::from_millis(500));
                };
                if started.elapsed() >= HEALTHY_AFTER {
                    backoff = Duration::from_secs(1);
                }
                let how = status.map(|s| s.to_string()).unwrap_or_else(|| "unknown status".into());
                log_line(
                    &spec.log,
                    &format!("{} exited ({how}); restarting in {}s", spec.name, backoff.as_secs()),
                );
            }
            Err(e) => {
                log_line(
                    &spec.log,
                    &format!("couldn't start {}: {e}; retrying in {}s", spec.name, backoff.as_secs()),
                );
            }
        }
        nap(backoff, &stop);
        backoff = (backoff * 2).min(BACKOFF_MAX);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wait_for(mut cond: impl FnMut() -> bool) -> bool {
        let until = Instant::now() + Duration::from_secs(10);
        while Instant::now() < until {
            if cond() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        false
    }

    #[test]
    fn test_supervisor_runs_restarts_and_stops() {
        let dir = std::env::temp_dir().join(format!("hl-sidecar-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        // a sidecar that records each start, then exits — the supervisor
        // must bring it back
        let bin = dir.join("ember.sh");
        std::fs::write(
            &bin,
            "#!/bin/sh\necho \"run $HEARTHLOOM_PLUGIN_NAME\" >> \"$PWD/runs\"\nsleep 0.2\nexit 3\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let name = format!("t-{}", uuid::Uuid::new_v4().simple());
        start(Spec {
            name: name.clone(),
            bin: bin.clone(),
            env: vec![("HEARTHLOOM_PLUGIN_NAME".into(), "demo".into())],
            log: dir.join("plugin.log"),
            cwd: dir.clone(),
        })
        .unwrap();
        let runs = || std::fs::read_to_string(dir.join("runs")).unwrap_or_default();
        assert!(wait_for(|| runs().lines().count() >= 2), "restarted after exit");
        assert!(runs().starts_with("run demo"), "env reached the child");
        stop(&name);
        let after = runs().lines().count();
        std::thread::sleep(Duration::from_millis(2500));
        assert_eq!(runs().lines().count(), after, "no restarts after stop");
        assert!(!running(&name));
        let log = std::fs::read_to_string(dir.join("plugin.log")).unwrap();
        assert!(log.contains("exited"), "exit recorded in plugin.log: {log}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_stop_terminates_a_long_runner() {
        let dir = std::env::temp_dir().join(format!("hl-sidecar-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("long.sh");
        std::fs::write(&bin, "#!/bin/sh\necho $$ > \"$PWD/pid\"\nexec sleep 30\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let name = format!("t-{}", uuid::Uuid::new_v4().simple());
        start(Spec {
            name: name.clone(),
            bin,
            env: vec![],
            log: dir.join("plugin.log"),
            cwd: dir.clone(),
        })
        .unwrap();
        assert!(wait_for(|| running(&name)), "long runner is up");
        let pid: i32 = {
            assert!(wait_for(|| dir.join("pid").exists()));
            std::fs::read_to_string(dir.join("pid")).unwrap().trim().parse().unwrap()
        };
        let t = Instant::now();
        stop(&name);
        assert!(t.elapsed() < Duration::from_secs(6), "SIGTERM honored promptly");
        assert!(!running(&name));
        #[cfg(unix)]
        {
            let alive = unsafe { libc::kill(pid, 0) } == 0;
            assert!(!alive, "the child process is gone after stop");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
