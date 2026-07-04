//! `gitterm-agent attach` — bridge the current terminal to a remote
//! session's PTY over the AttachTerminal stream, and `gitterm-agent
//! sessions` — list sessions on an agent.
//!
//! This is how GitTerm desktop shows remote sessions: a normal local
//! terminal tab runs this command as its startup program, so the existing
//! terminal machinery works unchanged (like running ssh or mosh).

use std::error::Error;
use std::io::Write;

use gitterm::agentd::client::{
    RemoteAgentBackend, RemoteAgentClientConfig, TerminalRecv, TerminalSend,
};

struct CommonArgs {
    endpoint: String,
    token_ref: String,
}

type ParsedArgs = (CommonArgs, Vec<(String, String)>);

fn parse_common(
    args: &mut impl Iterator<Item = String>,
) -> Result<ParsedArgs, Box<dyn Error + Send + Sync>> {
    let mut endpoint = None;
    let mut token_ref = None;
    let mut extra = Vec::new();
    while let Some(arg) = args.next() {
        let mut take = |name: &str| -> Result<String, Box<dyn Error + Send + Sync>> {
            args.next()
                .ok_or_else(|| format!("missing value for {name}").into())
        };
        match arg.as_str() {
            "--endpoint" => endpoint = Some(take("--endpoint")?),
            "--token-ref" => token_ref = Some(take("--token-ref")?),
            other if other.starts_with("--") => {
                let value = take(other)?;
                extra.push((other.to_string(), value));
            }
            other => return Err(format!("unexpected argument: {other}").into()),
        }
    }
    Ok((
        CommonArgs {
            endpoint: endpoint.ok_or("--endpoint is required")?,
            token_ref: token_ref.ok_or("--token-ref is required")?,
        },
        extra,
    ))
}

fn backend(common: &CommonArgs) -> RemoteAgentBackend {
    RemoteAgentBackend::new(RemoteAgentClientConfig {
        remote_id: "attach-cli".to_string(),
        name: "attach-cli".to_string(),
        endpoint: common.endpoint.clone(),
        token_ref: common.token_ref.clone(),
    })
}

pub async fn list(
    mut args: impl Iterator<Item = String>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let (common, extra) = parse_common(&mut args)?;
    let workspace = extra
        .iter()
        .find(|(name, _)| name == "--workspace")
        .map(|(_, value)| value.clone())
        .unwrap_or_default();

    let sessions = backend(&common).list_sessions(workspace).await?;
    if sessions.is_empty() {
        println!("no sessions");
        return Ok(());
    }
    for session in sessions {
        println!(
            "{}\t{}\t{}\t{}\t{}",
            session.session_id,
            if session.running {
                "running".to_string()
            } else {
                format!("exited({})", session.exit_code)
            },
            session.kind,
            session.workspace_id,
            session.command,
        );
    }
    Ok(())
}

pub async fn start(
    mut args: impl Iterator<Item = String>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let (common, extra) = parse_common(&mut args)?;
    let get = |name: &str| {
        extra
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.clone())
    };
    let workspace = get("--workspace").ok_or("--workspace is required")?;
    let cwd = get("--cwd").ok_or("--cwd is required")?;
    let kind = get("--kind").unwrap_or_else(|| "shell".to_string());
    let command = get("--cmd").ok_or("--cmd is required")?;

    let session = backend(&common)
        .start_session(
            workspace,
            cwd,
            kind,
            vec!["/bin/sh".to_string(), "-lc".to_string(), command],
            Vec::new(),
            120,
            32,
        )
        .await?;
    println!("{}", session.session_id);
    Ok(())
}

pub async fn stop(
    mut args: impl Iterator<Item = String>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let (common, extra) = parse_common(&mut args)?;
    let session = extra
        .iter()
        .find(|(name, _)| name == "--session")
        .map(|(_, value)| value.clone())
        .ok_or("--session is required")?;
    let stopped = backend(&common).stop_session(session).await?;
    println!("{}", if stopped { "stopped" } else { "already exited" });
    Ok(())
}

pub async fn run(
    mut args: impl Iterator<Item = String>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let (common, extra) = parse_common(&mut args)?;
    let session_id = extra
        .iter()
        .find(|(name, _)| name == "--session")
        .map(|(_, value)| value.clone())
        .ok_or("--session is required")?;

    let (cols, rows) = terminal_size();
    let (input_tx, output) = backend(&common)
        .attach_terminal(session_id, cols, rows)
        .await?;

    // Read-only attach when stdin isn't a terminal (piping/inspection).
    let interactive = unsafe { libc::isatty(libc::STDIN_FILENO) } == 1;
    let _raw = if interactive {
        Some(RawModeGuard::enable()?)
    } else {
        None
    };

    // stdin → session, from a blocking thread (stdin has no async story
    // worth having here).
    let stdin_tx = input_tx.clone();
    if interactive {
        std::thread::spawn(move || {
            use std::io::Read;
            let mut stdin = std::io::stdin();
            let mut buf = [0u8; 4096];
            loop {
                match stdin.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if stdin_tx
                            .blocking_send(TerminalSend::Data(buf[..n].to_vec()))
                            .is_err()
                        {
                            break;
                        }
                    }
                }
            }
        });
    }

    // SIGWINCH → resize
    let resize_tx = input_tx.clone();
    tokio::spawn(async move {
        let Ok(mut winch) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())
        else {
            return;
        };
        while winch.recv().await.is_some() {
            let (cols, rows) = terminal_size();
            if resize_tx
                .send(TerminalSend::Resize { cols, rows })
                .await
                .is_err()
            {
                break;
            }
        }
    });

    // session output → stdout
    use tokio_stream::StreamExt;
    tokio::pin!(output);
    let mut exit_code = 0i32;
    let mut stdout = std::io::stdout();
    while let Some(item) = output.next().await {
        match item {
            Ok(TerminalRecv::Data(data)) => {
                stdout.write_all(&data)?;
                stdout.flush()?;
            }
            Ok(TerminalRecv::Exited(code)) => {
                exit_code = code;
                break;
            }
            Err(err) => {
                drop(_raw);
                return Err(format!("connection lost: {err}").into());
            }
        }
    }

    drop(_raw);
    eprintln!("\r\n[session ended with exit code {exit_code}]");
    std::process::exit(exit_code.clamp(0, 255));
}

fn terminal_size() -> (u16, u16) {
    let mut size = libc::winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let ok = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut size) } == 0;
    if ok && size.ws_col > 0 && size.ws_row > 0 {
        (size.ws_col, size.ws_row)
    } else {
        (80, 24)
    }
}

/// Puts the controlling terminal into raw mode; restores on drop.
struct RawModeGuard {
    original: libc::termios,
}

impl RawModeGuard {
    fn enable() -> Result<Self, Box<dyn Error + Send + Sync>> {
        unsafe {
            if libc::isatty(libc::STDIN_FILENO) == 0 {
                return Err("stdin is not a terminal".into());
            }
            let mut original: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(libc::STDIN_FILENO, &mut original) != 0 {
                return Err("could not read terminal attributes".into());
            }
            let mut raw = original;
            libc::cfmakeraw(&mut raw);
            if libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw) != 0 {
                return Err("could not enable raw mode".into());
            }
            Ok(Self { original })
        }
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.original);
        }
    }
}
