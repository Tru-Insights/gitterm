//! Scratch probe for a deployed gitterm-agent: handshake + optional ListDir.
//! Usage: cargo run --example agent_probe -- <endpoint> <token_ref> [root]

use gitterm::agentd::client::{RemoteAgentBackend, RemoteAgentClientConfig};

#[tokio::main]
async fn main() {
    let endpoint = std::env::args()
        .nth(1)
        .expect("usage: agent_probe <endpoint> <token_ref> [root]");
    let token_ref = std::env::args()
        .nth(2)
        .expect("usage: agent_probe <endpoint> <token_ref> [root]");
    let root = std::env::args().nth(3);

    let backend = RemoteAgentBackend::new(RemoteAgentClientConfig {
        remote_id: "probe".to_string(),
        name: "probe".to_string(),
        endpoint,
        token_ref,
    });

    let hs = backend.handshake().await.expect("handshake failed");
    println!(
        "handshake ok: {} v{} protocol={} capabilities={:?}",
        hs.agent_name, hs.agent_version, hs.protocol_version, hs.capabilities
    );

    if let Some(root) = root {
        let dir = backend
            .list_dir("probe".to_string(), root.clone(), root, false)
            .await
            .expect("list_dir failed");
        println!(
            "list_dir ok: {} entries under {}",
            dir.entries.len(),
            dir.current_dir
        );
        for entry in dir.entries.iter().take(12) {
            println!("  {} {}", if entry.is_dir { "d" } else { "-" }, entry.name);
        }
    }
}
