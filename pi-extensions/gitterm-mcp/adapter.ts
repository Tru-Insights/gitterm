import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { createMcpAdapter } from "pi-mcp-adapter";

// GitTerm exports one URL/token pair per MCP server into every terminal it
// spawns (see `task_mcp::TaskMcpConnection::terminal_environment` and
// `browser_mcp` in the Rust app). This extension attaches whichever servers
// are present, so a pi session launched by GitTerm — a task session in
// particular — gets the task-control tools without global pi configuration.
// Outside GitTerm none of the variables exist and the extension is inert.

const TASK_MCP_URL_ENV = "GITTERM_V5_TASK_MCP_URL";
const TASK_MCP_TOKEN_ENV = "GITTERM_V5_TASK_MCP_TOKEN";

// V4 GitTerm is still in daily use; its browser server uses the V4 names.
const BROWSER_MCP_ENVS = [
  { url: "GITTERM_V5_BROWSER_MCP_URL", token: "GITTERM_V5_BROWSER_MCP_TOKEN" },
  { url: "GITTERM_V4_BROWSER_MCP_URL", token: "GITTERM_V4_BROWSER_MCP_TOKEN" },
];

interface HttpServer {
  url: string;
  auth: "bearer";
  bearerTokenEnv: string;
  directTools: true;
  toolPrefix: "none";
  requestTimeoutMs?: number;
}

function serverFromEnv(urlEnv: string, tokenEnv: string): HttpServer | undefined {
  const url = process.env[urlEnv];
  const token = process.env[tokenEnv];
  if (!url || !token) return undefined;
  return { url, auth: "bearer", bearerTokenEnv: tokenEnv, directTools: true, toolPrefix: "none" };
}

export function registerGitTermMcp(pi: ExtensionAPI): void {
  const mcpServers: Record<string, HttpServer> = {};

  const tasks = serverFromEnv(TASK_MCP_URL_ENV, TASK_MCP_TOKEN_ENV);
  if (tasks) {
    // Matches the Codex `tool_timeout_sec=300`: task_launch_session and
    // task_create_batch can wait on worktree creation.
    mcpServers["gitterm-tasks"] = { ...tasks, requestTimeoutMs: 300_000 };
  }

  const browser = BROWSER_MCP_ENVS.map((names) => serverFromEnv(names.url, names.token)).find(
    (server) => server !== undefined,
  );
  if (browser) mcpServers["gitterm-browser"] = browser;

  if (Object.keys(mcpServers).length === 0) return;

  createMcpAdapter({ config: { mcpServers } })(pi);
}
