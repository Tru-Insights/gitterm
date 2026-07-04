use std::error::Error;
use std::net::SocketAddr;

use gitterm::agentd::config::AgentConfigFile;
use gitterm::agentd::server::{self, AgentServerConfig};

const DEFAULT_ADDR: &str = "127.0.0.1:7777";

#[tokio::main]
async fn main() {
    if let Err(err) = run().await {
        eprintln!("gitterm-agent: {err}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn Error + Send + Sync>> {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("serve") => {
            let config = parse_serve_config(args)?;
            eprintln!(
                "gitterm-agent serving {} as {}",
                config.bind_addr, config.agent_name
            );
            server::serve(config).await
        }
        Some("--help") | Some("-h") | None => {
            print_usage();
            Ok(())
        }
        Some(command) => Err(format!("unknown command: {command}").into()),
    }
}

fn parse_serve_config(
    args: impl IntoIterator<Item = String>,
) -> Result<AgentServerConfig, Box<dyn Error + Send + Sync>> {
    let file_config = AgentConfigFile::load().unwrap_or_default();
    let mut bind_addr = std::env::var("GITTERM_AGENT_ADDR")
        .ok()
        .or(file_config.bind_addr)
        .unwrap_or_else(|| DEFAULT_ADDR.into());
    let mut auth_token = std::env::var("GITTERM_AGENT_TOKEN").unwrap_or_default();
    let mut agent_name = std::env::var("GITTERM_AGENT_NAME")
        .ok()
        .or(file_config.agent_name)
        .unwrap_or_else(hostname_fallback);

    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--addr" => {
                bind_addr = next_arg(&mut args, "--addr")?;
            }
            "--token" => {
                auth_token = next_arg(&mut args, "--token")?;
            }
            "--name" => {
                agent_name = next_arg(&mut args, "--name")?;
            }
            "--help" | "-h" => {
                print_usage();
                std::process::exit(0);
            }
            other => return Err(format!("unknown serve option: {other}").into()),
        }
    }

    if auth_token.is_empty() {
        return Err("missing token; set GITTERM_AGENT_TOKEN or pass --token".into());
    }

    Ok(AgentServerConfig {
        bind_addr: bind_addr.parse::<SocketAddr>()?,
        auth_token,
        agent_name,
    })
}

fn next_arg(
    args: &mut impl Iterator<Item = String>,
    flag: &str,
) -> Result<String, Box<dyn Error + Send + Sync>> {
    args.next()
        .ok_or_else(|| format!("missing value for {flag}").into())
}

fn hostname_fallback() -> String {
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .unwrap_or_else(|_| "gitterm-agent".to_string())
}

fn print_usage() {
    eprintln!(
        "Usage:
  gitterm-agent serve [--addr HOST:PORT] [--token TOKEN] [--name NAME]

Environment:
  GITTERM_AGENT_ADDR    default: {DEFAULT_ADDR}
  GITTERM_AGENT_TOKEN   required unless --token is passed
  GITTERM_AGENT_NAME    default: HOSTNAME/COMPUTERNAME/gitterm-agent

Config:
  ~/.config/gitterm-agent/config.json may set bind_addr and agent_name.
  Tokens are intentionally supplied by env/CLI in this skeleton."
    );
}
