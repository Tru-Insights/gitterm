//! Agent-owned PTY sessions.
//!
//! The agent is the durable process host: sessions keep running while
//! desktops connect and disconnect. Each session owns a PTY child, a
//! bounded ring buffer of recent output (replayed on attach so a fresh
//! desktop sees scrollback), and a broadcast channel for live output.
//! Sessions do not survive an agent restart; tmux-behind-agent is the
//! documented upgrade path if that ever matters.

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize};
use tokio::sync::broadcast;

/// Bytes of recent output kept per session for reattach replay.
const RING_CAPACITY: usize = 256 * 1024;
/// Broadcast depth; slow receivers drop old chunks (they already got the
/// replay, and terminals tolerate gaps far better than blocking the PTY).
const BROADCAST_CAPACITY: usize = 512;

#[derive(Debug, Clone)]
pub enum SessionOutput {
    Data(Vec<u8>),
    Exited(i32),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionInfo {
    pub session_id: String,
    pub workspace_id: String,
    pub kind: String,
    pub command: String,
    pub cwd: String,
    pub running: bool,
    pub exit_code: i32,
    pub created_unix_secs: u64,
}

struct SessionHandle {
    info: SessionInfo,
    writer: Mutex<Box<dyn Write + Send>>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    ring: Arc<Mutex<VecDeque<u8>>>,
    output_tx: broadcast::Sender<SessionOutput>,
    exit: Arc<Mutex<Option<i32>>>,
}

#[derive(Default)]
pub struct SessionManager {
    sessions: Mutex<HashMap<String, Arc<SessionHandle>>>,
    next_id: AtomicU64,
}

pub struct AttachHandles {
    pub replay: Vec<u8>,
    pub output_rx: broadcast::Receiver<SessionOutput>,
    pub already_exited: Option<i32>,
}

impl SessionManager {
    #[allow(clippy::too_many_arguments)]
    pub fn start_session(
        &self,
        workspace_id: String,
        cwd: String,
        kind: String,
        argv: Vec<String>,
        env: Vec<(String, String)>,
        cols: u16,
        rows: u16,
    ) -> Result<SessionInfo, String> {
        if argv.is_empty() || argv[0].trim().is_empty() {
            return Err("command must not be empty".to_string());
        }
        let cwd_path = std::path::Path::new(&cwd);
        if !cwd_path.is_absolute() {
            return Err("cwd must be absolute".to_string());
        }
        let cwd_canonical = std::fs::canonicalize(cwd_path)
            .map_err(|err| format!("could not resolve cwd {cwd}: {err}"))?;
        if !cwd_canonical.is_dir() {
            return Err(format!(
                "cwd {} is not a directory",
                cwd_canonical.display()
            ));
        }

        let pty = portable_pty::native_pty_system()
            .openpty(PtySize {
                rows: rows.max(2),
                cols: cols.max(2),
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|err| format!("could not open pty: {err}"))?;

        let mut cmd = CommandBuilder::new(&argv[0]);
        cmd.args(&argv[1..]);
        cmd.cwd(&cwd_canonical);
        cmd.env("TERM", "xterm-256color");
        for (name, value) in env {
            cmd.env(name, value);
        }

        let child = pty
            .slave
            .spawn_command(cmd)
            .map_err(|err| format!("could not spawn {}: {err}", argv[0]))?;
        drop(pty.slave);

        let killer = child.clone_killer();
        let mut reader = pty
            .master
            .try_clone_reader()
            .map_err(|err| format!("could not open pty reader: {err}"))?;
        let writer = pty
            .master
            .take_writer()
            .map_err(|err| format!("could not open pty writer: {err}"))?;

        let id = format!("sess-{}", self.next_id.fetch_add(1, Ordering::SeqCst) + 1);
        let created_unix_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let info = SessionInfo {
            session_id: id.clone(),
            workspace_id,
            kind,
            command: argv.join(" "),
            cwd: cwd_canonical.to_string_lossy().to_string(),
            running: true,
            exit_code: 0,
            created_unix_secs,
        };

        let (output_tx, _) = broadcast::channel(BROADCAST_CAPACITY);
        let ring = Arc::new(Mutex::new(VecDeque::with_capacity(RING_CAPACITY / 4)));
        let exit = Arc::new(Mutex::new(None));

        let handle = Arc::new(SessionHandle {
            info: info.clone(),
            writer: Mutex::new(writer),
            master: Mutex::new(pty.master),
            killer: Mutex::new(killer),
            ring: ring.clone(),
            output_tx: output_tx.clone(),
            exit: exit.clone(),
        });
        self.sessions
            .lock()
            .expect("session registry poisoned")
            .insert(id.clone(), handle);

        // Reader thread: PTY output → ring buffer + live broadcast; on EOF
        // reap the child and broadcast the exit code.
        let mut child = child;
        std::thread::Builder::new()
            .name(format!("pty-read-{id}"))
            .spawn(move || {
                let mut buf = [0u8; 8192];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            let chunk = buf[..n].to_vec();
                            {
                                let mut ring = ring.lock().expect("ring poisoned");
                                ring.extend(chunk.iter().copied());
                                while ring.len() > RING_CAPACITY {
                                    ring.pop_front();
                                }
                            }
                            let _ = output_tx.send(SessionOutput::Data(chunk));
                        }
                    }
                }
                let code = child
                    .wait()
                    .map(|status| status.exit_code() as i32)
                    .unwrap_or(-1);
                *exit.lock().expect("exit slot poisoned") = Some(code);
                let _ = output_tx.send(SessionOutput::Exited(code));
            })
            .map_err(|err| format!("could not spawn reader thread: {err}"))?;

        Ok(info)
    }

    pub fn list_sessions(&self, workspace_id: &str) -> Vec<SessionInfo> {
        let sessions = self.sessions.lock().expect("session registry poisoned");
        let mut infos: Vec<SessionInfo> = sessions
            .values()
            .filter(|handle| workspace_id.is_empty() || handle.info.workspace_id == workspace_id)
            .map(|handle| {
                let mut info = handle.info.clone();
                if let Some(code) = *handle.exit.lock().expect("exit slot poisoned") {
                    info.running = false;
                    info.exit_code = code;
                }
                info
            })
            .collect();
        infos.sort_by_key(|info| info.created_unix_secs);
        infos
    }

    pub fn stop_session(&self, session_id: &str) -> Result<bool, String> {
        let handle = {
            let sessions = self.sessions.lock().expect("session registry poisoned");
            sessions.get(session_id).cloned()
        };
        let Some(handle) = handle else {
            return Err(format!("unknown session {session_id}"));
        };
        if handle.exit.lock().expect("exit slot poisoned").is_some() {
            return Ok(false);
        }
        handle
            .killer
            .lock()
            .expect("killer poisoned")
            .kill()
            .map_err(|err| format!("could not stop {session_id}: {err}"))?;
        Ok(true)
    }

    /// Remove exited sessions from the registry (list keeps them visible
    /// until then so exit codes can be observed).
    pub fn remove_session(&self, session_id: &str) -> bool {
        self.sessions
            .lock()
            .expect("session registry poisoned")
            .remove(session_id)
            .is_some()
    }

    pub fn attach(&self, session_id: &str, cols: u16, rows: u16) -> Result<AttachHandles, String> {
        let handle = {
            let sessions = self.sessions.lock().expect("session registry poisoned");
            sessions.get(session_id).cloned()
        };
        let Some(handle) = handle else {
            return Err(format!("unknown session {session_id}"));
        };
        // Subscribe before copying the ring so no chunk lands in the gap.
        let output_rx = handle.output_tx.subscribe();
        let replay: Vec<u8> = {
            let ring = handle.ring.lock().expect("ring poisoned");
            ring.iter().copied().collect()
        };
        if cols > 0 && rows > 0 {
            let _ = handle
                .master
                .lock()
                .expect("master poisoned")
                .resize(PtySize {
                    rows,
                    cols,
                    pixel_width: 0,
                    pixel_height: 0,
                });
        }
        let already_exited = *handle.exit.lock().expect("exit slot poisoned");
        Ok(AttachHandles {
            replay,
            output_rx,
            already_exited,
        })
    }

    pub fn write_input(&self, session_id: &str, data: &[u8]) -> Result<(), String> {
        let handle = {
            let sessions = self.sessions.lock().expect("session registry poisoned");
            sessions.get(session_id).cloned()
        };
        let Some(handle) = handle else {
            return Err(format!("unknown session {session_id}"));
        };
        let mut writer = handle.writer.lock().expect("writer poisoned");
        writer
            .write_all(data)
            .and_then(|_| writer.flush())
            .map_err(|err| format!("write to {session_id} failed: {err}"))
    }

    pub fn resize(&self, session_id: &str, cols: u16, rows: u16) -> Result<(), String> {
        let handle = {
            let sessions = self.sessions.lock().expect("session registry poisoned");
            sessions.get(session_id).cloned()
        };
        let Some(handle) = handle else {
            return Err(format!("unknown session {session_id}"));
        };
        let result = handle
            .master
            .lock()
            .expect("master poisoned")
            .resize(PtySize {
                rows: rows.max(2),
                cols: cols.max(2),
                pixel_width: 0,
                pixel_height: 0,
            });
        result.map_err(|err| format!("resize of {session_id} failed: {err}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn wait_for_exit(manager: &SessionManager, id: &str) -> i32 {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let infos = manager.list_sessions("");
            let info = infos
                .iter()
                .find(|i| i.session_id == id)
                .expect("session listed");
            if !info.running {
                return info.exit_code;
            }
            assert!(Instant::now() < deadline, "session did not exit in time");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    #[test]
    fn session_runs_and_output_is_replayable() {
        let manager = SessionManager::default();
        let info = manager
            .start_session(
                "ws".to_string(),
                std::env::temp_dir().to_string_lossy().to_string(),
                "shell".to_string(),
                vec![
                    "/bin/sh".to_string(),
                    "-c".to_string(),
                    "printf remote-hello".to_string(),
                ],
                Vec::new(),
                80,
                24,
            )
            .unwrap();
        assert!(info.running);

        let code = wait_for_exit(&manager, &info.session_id);
        assert_eq!(code, 0);

        let attach = manager.attach(&info.session_id, 0, 0).unwrap();
        let replay = String::from_utf8_lossy(&attach.replay).to_string();
        assert!(replay.contains("remote-hello"), "replay: {replay:?}");
        assert_eq!(attach.already_exited, Some(0));
    }

    #[test]
    fn stop_kills_long_running_session() {
        let manager = SessionManager::default();
        let info = manager
            .start_session(
                "ws".to_string(),
                std::env::temp_dir().to_string_lossy().to_string(),
                "shell".to_string(),
                vec![
                    "/bin/sh".to_string(),
                    "-c".to_string(),
                    "sleep 60".to_string(),
                ],
                Vec::new(),
                80,
                24,
            )
            .unwrap();

        assert!(manager.stop_session(&info.session_id).unwrap());
        let code = wait_for_exit(&manager, &info.session_id);
        assert_ne!(code, 0);
        assert!(manager.remove_session(&info.session_id));
        assert!(manager.list_sessions("").is_empty());
    }

    #[test]
    fn input_reaches_the_child() {
        let manager = SessionManager::default();
        let info = manager
            .start_session(
                "ws".to_string(),
                std::env::temp_dir().to_string_lossy().to_string(),
                "shell".to_string(),
                vec!["/bin/cat".to_string()],
                Vec::new(),
                80,
                24,
            )
            .unwrap();

        let mut rx = manager.attach(&info.session_id, 0, 0).unwrap().output_rx;
        manager.write_input(&info.session_id, b"ping\n").unwrap();

        let deadline = Instant::now() + Duration::from_secs(10);
        let mut seen = String::new();
        while Instant::now() < deadline && !seen.contains("ping") {
            match rx.try_recv() {
                Ok(SessionOutput::Data(chunk)) => {
                    seen.push_str(&String::from_utf8_lossy(&chunk));
                }
                Ok(SessionOutput::Exited(_)) => break,
                Err(broadcast::error::TryRecvError::Empty) => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(_) => break,
            }
        }
        assert!(seen.contains("ping"), "echoed output: {seen:?}");

        let _ = manager.stop_session(&info.session_id);
    }

    #[test]
    fn rejects_empty_command_and_bad_cwd() {
        let manager = SessionManager::default();
        assert!(manager
            .start_session(
                "ws".into(),
                "/tmp".into(),
                "shell".into(),
                vec![],
                Vec::new(),
                80,
                24
            )
            .is_err());
        assert!(manager
            .start_session(
                "ws".into(),
                "not-absolute".into(),
                "shell".into(),
                vec!["/bin/sh".into()],
                Vec::new(),
                80,
                24
            )
            .is_err());
    }
}
