//! The Forge — SuperFrank's async process supervisor.
//!
//! Unlike OpenClaw's exec+yieldMs polling hack, the Forge gives Frank true
//! async ownership of every process it spawns. A 4-minute Rust compile runs
//! in the background; Frank checks on it, reads its output, writes to its
//! stdin, and gets notified on completion — without blocking or polling loops.
//!
//! Every ForgeProcess is a live Tokio task with ring-buffered output,
//! full stdin/stdout/stderr access, and signal control.

use anyhow::{anyhow, Result};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, Mutex, RwLock};
use tracing::{info, warn};
use uuid::Uuid;

const RING_BUFFER_LINES: usize = 500; // keep last 500 lines per process

// ── Process state ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ProcessStatus {
    Running,
    Exited { code: i32 },
    Killed,
    TimedOut,
}

#[derive(Debug)]
pub struct ForgeProcess {
    pub id: Uuid,
    pub command: String,
    pub cwd: String,
    pub status: RwLock<ProcessStatus>,
    pub stdout_ring: Mutex<VecDeque<String>>,
    pub stdin_tx: mpsc::Sender<String>,
    pub started_at: Instant,
    /// Send () to request graceful kill
    pub kill_tx: mpsc::Sender<()>,
}

impl ForgeProcess {
    pub async fn status_snapshot(&self) -> ProcessStatus {
        self.status.read().await.clone()
    }

    pub async fn tail_output(&self, lines: usize) -> Vec<String> {
        let ring = self.stdout_ring.lock().await;
        let n = lines.min(ring.len());
        ring.iter().rev().take(n).cloned().collect::<Vec<_>>().into_iter().rev().collect()
    }

    pub async fn write_stdin(&self, data: &str) -> Result<()> {
        self.stdin_tx.send(data.to_string()).await
            .map_err(|_| anyhow!("Process stdin closed"))
    }

    pub async fn kill(&self) {
        let _ = self.kill_tx.send(()).await;
    }

    pub fn elapsed_secs(&self) -> u64 {
        self.started_at.elapsed().as_secs()
    }
}

// ── Forge ─────────────────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct Forge {
    pub processes: Arc<DashMap<Uuid, Arc<ForgeProcess>>>,
}

impl Forge {
    pub fn new() -> Self {
        Self { processes: Arc::new(DashMap::new()) }
    }

    /// Spawn a process. Returns immediately with the process UUID.
    /// The process runs in the background; use status/log/write/wait to interact.
    pub async fn spawn(
        &self,
        command: &str,
        cwd: Option<&str>,
        env_vars: Option<Vec<(String, String)>>,
        timeout_secs: Option<u64>,
    ) -> Result<Uuid> {
        let id = Uuid::new_v4();
        let cwd = cwd.unwrap_or("/opt/frankos/workspace").to_string();

        let (stdin_tx, mut stdin_rx) = mpsc::channel::<String>(64);
        let (kill_tx, mut kill_rx) = mpsc::channel::<()>(1);

        let mut cmd = Command::new("bash");
        cmd.arg("-c")
           .arg(command)
           .current_dir(&cwd)
           .stdout(std::process::Stdio::piped())
           .stderr(std::process::Stdio::piped())
           .stdin(std::process::Stdio::piped())
           .kill_on_drop(true);

        if let Some(env) = &env_vars {
            for (k, v) in env { cmd.env(k, v); }
        }

        let mut child = cmd.spawn()
            .map_err(|e| anyhow!("Forge spawn failed: {}", e))?;

        let pid = child.id().unwrap_or(0);
        info!("[Forge] Spawned process {} (pid={}) cmd={}", id, pid, &command[..command.len().min(80)]);

        let process = Arc::new(ForgeProcess {
            id,
            command: command.to_string(),
            cwd: cwd.clone(),
            status: RwLock::new(ProcessStatus::Running),
            stdout_ring: Mutex::new(VecDeque::with_capacity(RING_BUFFER_LINES)),
            stdin_tx,
            started_at: Instant::now(),
            kill_tx,
        });

        self.processes.insert(id, process.clone());

        // Capture stdout+stderr → ring buffer
        let proc_for_io = process.clone();
        let stdout = child.stdout.take().expect("stdout piped");
        let stderr = child.stderr.take().expect("stderr piped");
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let mut ring = proc_for_io.stdout_ring.lock().await;
                if ring.len() >= RING_BUFFER_LINES { ring.pop_front(); }
                ring.push_back(line);
            }
        });
        let proc_for_err = process.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let mut ring = proc_for_err.stdout_ring.lock().await;
                if ring.len() >= RING_BUFFER_LINES { ring.pop_front(); }
                ring.push_back(format!("[stderr] {}", line));
            }
        });

        // Forward stdin writes
        let mut stdin = child.stdin.take().expect("stdin piped");
        tokio::spawn(async move {
            while let Some(data) = stdin_rx.recv().await {
                let _ = stdin.write_all(data.as_bytes()).await;
            }
        });

        // Monitor process lifecycle
        let proc_for_wait = process.clone();
        let processes_ref = self.processes.clone();
        tokio::spawn(async move {
            let result = if let Some(timeout) = timeout_secs {
                tokio::select! {
                    status = child.wait() => {
                        match status {
                            Ok(s) => {
                                if s.success() { ProcessStatus::Exited { code: 0 } }
                                else { ProcessStatus::Exited { code: s.code().unwrap_or(-1) } }
                            }
                            Err(_) => ProcessStatus::Exited { code: -1 },
                        }
                    }
                    _ = tokio::time::sleep(Duration::from_secs(timeout)) => {
                        let _ = child.kill().await;
                        ProcessStatus::TimedOut
                    }
                    _ = kill_rx.recv() => {
                        let _ = child.kill().await;
                        ProcessStatus::Killed
                    }
                }
            } else {
                tokio::select! {
                    status = child.wait() => {
                        match status {
                            Ok(s) => ProcessStatus::Exited { code: s.code().unwrap_or(-1) },
                            Err(_) => ProcessStatus::Exited { code: -1 },
                        }
                    }
                    _ = kill_rx.recv() => {
                        let _ = child.kill().await;
                        ProcessStatus::Killed
                    }
                }
            };

            info!("[Forge] Process {} finished: {:?}", id, result);
            *proc_for_wait.status.write().await = result;
        });

        Ok(id)
    }

    pub fn get(&self, id: Uuid) -> Option<Arc<ForgeProcess>> {
        self.processes.get(&id).map(|p| p.clone())
    }

    pub fn list(&self) -> Vec<Value> {
        self.processes.iter().map(|entry| {
            // We can't await inside a sync closure, so snapshot what we can sync
            let p = entry.value();
            json!({
                "id": p.id,
                "command": &p.command[..p.command.len().min(100)],
                "cwd": p.cwd,
                "elapsed_secs": p.elapsed_secs(),
            })
        }).collect()
    }

    pub async fn kill(&self, id: Uuid) -> Result<()> {
        let p = self.get(id).ok_or_else(|| anyhow!("Process {} not found", id))?;
        p.kill().await;
        Ok(())
    }

    /// Block until a process exits or timeout_secs passes.
    /// Returns the final status.
    pub async fn wait_for(&self, id: Uuid, timeout_secs: u64) -> Result<ProcessStatus> {
        let deadline = Instant::now() + Duration::from_secs(timeout_secs);
        loop {
            if let Some(p) = self.get(id) {
                let status = p.status_snapshot().await;
                if status != ProcessStatus::Running {
                    return Ok(status);
                }
            } else {
                return Err(anyhow!("Process {} not found", id));
            }
            if Instant::now() >= deadline {
                return Ok(ProcessStatus::TimedOut);
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    /// Reap exited/killed processes older than max_age_secs to prevent unbounded growth.
    pub async fn reap_old(&self, max_age_secs: u64) {
        let mut to_remove = vec![];
        for entry in self.processes.iter() {
            let p = entry.value();
            let done = matches!(
                p.status_snapshot().await,
                ProcessStatus::Exited { .. } | ProcessStatus::Killed | ProcessStatus::TimedOut
            );
            if done && p.elapsed_secs() > max_age_secs {
                to_remove.push(*entry.key());
            }
        }
        for id in to_remove {
            self.processes.remove(&id);
        }
    }
}

impl Default for Forge {
    fn default() -> Self { Self::new() }
}
