use std::error::Error;
use std::net::SocketAddr;

use gitterm::agentd::config::AgentConfigFile;
use gitterm::agentd::phone_http::PhoneServerConfig;
use gitterm::agentd::server::{self, AgentServerConfig};

#[cfg(unix)]
mod attach;

const DEFAULT_ADDR: &str = "127.0.0.1:8797";
const AGENT_ADDR_ENV: &str = "GITTERM_V6_AGENT_ADDR";
const AGENT_TOKEN_ENV: &str = "GITTERM_V6_AGENT_TOKEN";
const AGENT_NAME_ENV: &str = "GITTERM_V6_AGENT_NAME";
/// Overrides where the agent keeps its paired phones (tests and smoke runs
/// that must not touch the real config dir).
const AGENT_CONFIG_DIR_ENV: &str = "GITTERM_V6_AGENT_CONFIG_DIR";

#[tokio::main]
async fn main() {
    if let Err(err) = run().await {
        eprintln!("gitterm-v6-agent: {err}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn Error + Send + Sync>> {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("serve") => {
            let config = parse_serve_config(args)?;
            eprintln!(
                "gitterm-v6-agent serving {} as {}",
                config.bind_addr, config.agent_name
            );
            match &config.phone {
                Some(phone) => eprintln!(
                    "gitterm-v6-agent phone app on {} (paired phones in {})",
                    phone.bind_addr,
                    phone.store_dir.display()
                ),
                None => eprintln!("gitterm-v6-agent phone app off"),
            }
            server::serve(config).await
        }
        #[cfg(unix)]
        Some("attach") => attach::run(args).await,
        #[cfg(unix)]
        Some("sessions") => attach::list(args).await,
        #[cfg(unix)]
        Some("start") => attach::start(args).await,
        #[cfg(unix)]
        Some("stop") => attach::stop(args).await,
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
    let mut bind_addr = std::env::var(AGENT_ADDR_ENV)
        .ok()
        .or(file_config.bind_addr)
        .unwrap_or_else(|| DEFAULT_ADDR.into());
    let mut auth_token = std::env::var(AGENT_TOKEN_ENV).unwrap_or_default();
    let mut agent_name = std::env::var(AGENT_NAME_ENV)
        .ok()
        .or(file_config.agent_name)
        .unwrap_or_else(hostname_fallback);
    let mut phone_addr: Option<String> = None;

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
            "--phone-addr" => {
                phone_addr = Some(next_arg(&mut args, "--phone-addr")?);
            }
            "--help" | "-h" => {
                print_usage();
                std::process::exit(0);
            }
            other => return Err(format!("unknown serve option: {other}").into()),
        }
    }

    if auth_token.is_empty() {
        return Err(format!("missing token; set {AGENT_TOKEN_ENV} or pass --token").into());
    }

    let bind_addr = bind_addr.parse::<SocketAddr>()?;
    let phone = match phone_bind_addr(bind_addr, phone_addr.as_deref())? {
        Some((phone_addr, required)) => Some(PhoneServerConfig {
            bind_addr: phone_addr,
            store_dir: agent_config_dir()?,
            required,
        }),
        None => None,
    };

    Ok(AgentServerConfig {
        claude_chat_limits: Default::default(),
        bind_addr,
        auth_token,
        agent_name,
        phone,
    })
}

/// Where the phone app listens, and whether it was asked for explicitly:
/// `--phone-addr HOST:PORT`, `--phone-addr off` (none), or by default the
/// agent's host at the next port up.
fn phone_bind_addr(
    agent_addr: SocketAddr,
    flag: Option<&str>,
) -> Result<Option<(SocketAddr, bool)>, Box<dyn Error + Send + Sync>> {
    match flag.map(str::trim) {
        Some("off") => Ok(None),
        Some(addr) => Ok(Some((
            addr.parse::<SocketAddr>()
                .map_err(|err| format!("--phone-addr {addr:?}: {err}"))?,
            true,
        ))),
        None => {
            let port = agent_addr.port().checked_add(1).ok_or_else(|| {
                format!(
                    "the agent port {} has no next port for the phone app; pass --phone-addr HOST:PORT or --phone-addr off",
                    agent_addr.port()
                )
            })?;
            Ok(Some((SocketAddr::new(agent_addr.ip(), port), false)))
        }
    }
}

fn agent_config_dir() -> Result<std::path::PathBuf, Box<dyn Error + Send + Sync>> {
    if let Ok(dir) = std::env::var(AGENT_CONFIG_DIR_ENV) {
        if !dir.trim().is_empty() {
            return Ok(dir.into());
        }
    }
    AgentConfigFile::dir().ok_or_else(|| {
        format!("no home directory for the agent config dir; set {AGENT_CONFIG_DIR_ENV}").into()
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
        .unwrap_or_else(|_| "gitterm-v6-agent".to_string())
}

fn print_usage() {
    eprintln!(
        "Usage:
  gitterm-v6-agent serve [--addr HOST:PORT] [--token TOKEN] [--name NAME]
                         [--phone-addr HOST:PORT|off]
  gitterm-v6-agent attach --endpoint URL --token-ref REF --session ID
  gitterm-v6-agent sessions --endpoint URL --token-ref REF [--workspace ID]
  gitterm-v6-agent start --endpoint URL --token-ref REF --workspace ID --cwd DIR [--kind K] --cmd CMD
  gitterm-v6-agent stop --endpoint URL --token-ref REF --session ID

Environment:
  {AGENT_ADDR_ENV}    default: {DEFAULT_ADDR}
  {AGENT_TOKEN_ENV}   required unless --token is passed
  {AGENT_NAME_ENV}    default: HOSTNAME/COMPUTERNAME/gitterm-v6-agent
  {AGENT_CONFIG_DIR_ENV}  default: ~/.config/gitterm-v6-agent (paired phones)

Phone app:
  Served on the agent's host at the next port up (--addr 100.x.y.z:7777
  serves phones on 100.x.y.z:7778). --phone-addr picks another address;
  --phone-addr off turns it off.

Config:
  ~/.config/gitterm-v6-agent/config.json may set bind_addr and agent_name.
  Tokens are intentionally supplied by env/CLI in this skeleton."
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_phone_app_defaults_to_the_next_port() {
        let agent: SocketAddr = "100.100.151.63:7777".parse().unwrap();
        assert_eq!(
            phone_bind_addr(agent, None).unwrap(),
            Some(("100.100.151.63:7778".parse().unwrap(), false))
        );
        assert_eq!(phone_bind_addr(agent, Some("off")).unwrap(), None);
        assert_eq!(
            phone_bind_addr(agent, Some("127.0.0.1:9000")).unwrap(),
            Some(("127.0.0.1:9000".parse().unwrap(), true))
        );
        assert!(phone_bind_addr(agent, Some("nonsense")).is_err());
        assert!(phone_bind_addr("127.0.0.1:65535".parse().unwrap(), None).is_err());
    }

    #[test]
    fn default_endpoint_and_environment_are_v6_only() {
        assert_eq!(DEFAULT_ADDR, "127.0.0.1:8797");
        assert_ne!(DEFAULT_ADDR, "127.0.0.1:8787");
        assert!(AGENT_ADDR_ENV.starts_with("GITTERM_V6_"));
        assert!(AGENT_TOKEN_ENV.starts_with("GITTERM_V6_"));
        assert!(AGENT_NAME_ENV.starts_with("GITTERM_V6_"));
    }
}
