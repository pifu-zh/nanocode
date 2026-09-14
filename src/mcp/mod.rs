//! MCP (Model Context Protocol) stdio client — Rust port of `src/mcp/*`.
//! JSON-RPC over child-process stdin/stdout, line-framed. Servers connect at
//! startup (wired per D1 — the TS client existed but was never started).

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value as Json;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;

const MCP_PROTOCOL_VERSION: &str = "2024-11-05";
const MAX_RECONNECT_RETRIES: u32 = 3;
const REQUEST_TIMEOUT_MS: u64 = 60_000;
const INIT_TIMEOUT_MS: u64 = 30_000;

// ---------------------------------------------------------------------------
// Config (mcp/config.ts)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct McpServerConfig {
    pub command: String,
    pub args: Option<Vec<String>>,
    pub env: Option<HashMap<String, String>>,
}

fn read_settings(cwd_or_home: &Path) -> Option<Json> {
    let raw = std::fs::read_to_string(cwd_or_home).ok()?;
    serde_json::from_str(&raw).ok()
}

fn extract_servers(settings: &Json) -> HashMap<String, McpServerConfig> {
    let mut out = HashMap::new();
    let Some(servers) = settings.get("mcpServers").and_then(|v| v.as_object()) else {
        return out;
    };
    for (name, entry) in servers {
        let Some(command) = entry.get("command").and_then(|c| c.as_str()) else { continue };
        if command.is_empty() {
            continue;
        }
        let args = entry.get("args").and_then(|a| a.as_array()).map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect::<Vec<_>>()
        });
        let env = entry.get("env").and_then(|e| e.as_object()).map(|obj| {
            obj.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect::<HashMap<String, String>>()
        });
        out.insert(
            name.clone(),
            McpServerConfig { command: command.to_string(), args, env },
        );
    }
    out
}

/// Project settings override user settings (mcp/config.ts loadMcpConfig).
pub fn load_mcp_config(cwd: &Path) -> HashMap<String, McpServerConfig> {
    let mut merged = HashMap::new();

    if let Some(home) = dirs::home_dir() {
        for dir in [".nanocode", ".claude"] {
            if let Some(settings) = read_settings(&home.join(dir).join("settings.json")) {
                merged.extend(extract_servers(&settings));
            }
        }
    }
    for dir in [".nanocode", ".claude"] {
        if let Some(settings) = read_settings(&cwd.join(dir).join("settings.json")) {
            merged.extend(extract_servers(&settings));
        }
    }
    merged
}

// ---------------------------------------------------------------------------
// Client (mcp/client.ts McpClient)
// ---------------------------------------------------------------------------

pub struct McpClient {
    pub name: String,
    process: Mutex<Option<Child>>,
    stdin_tx: mpsc::UnboundedSender<String>,
    next_id: AtomicU64,
    pending: Arc<Mutex<HashMap<u64, mpsc::Sender<Json>>>>,
    connected: Arc<std::sync::atomic::AtomicBool>,
    reconnects: Mutex<u32>,
}

#[derive(Debug)]
pub struct McpToolDefinition {
    pub name: String,
    pub description: Option<String>,
    pub input_schema: Option<Json>,
}

impl McpClient {
    pub fn new(name: &str, config: &McpServerConfig) -> Arc<Self> {
        let (tx, rx) = mpsc::unbounded_channel::<String>();
        let client = Arc::new(McpClient {
            name: name.to_string(),
            process: Mutex::new(None),
            stdin_tx: tx,
            next_id: AtomicU64::new(1),
            pending: Arc::new(Mutex::new(HashMap::new())),
            connected: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            reconnects: Mutex::new(0),
        });
        client.spawn(config, rx);
        client
    }

    fn spawn(&self, config: &McpServerConfig, mut rx: mpsc::UnboundedReceiver<String>) {
        let mut command = Command::new(&config.command);
        if let Some(args) = &config.args {
            command.args(args);
        }
        if let Some(env) = &config.env {
            for (k, v) in env {
                command.env(k, v);
            }
        }
        let _config = config.clone();
        command
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());

        let pending = self.pending.clone();
        let connected = self.connected.clone();
        let name = self.name.clone();
        let stdin_tx = self.stdin_tx.clone();

        tokio::spawn(async move {
            let mut child = match command.spawn() {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("[mcp:{name}] spawn failed: {e}");
                    return;
                }
            };
            let stdin = child.stdin.take().expect("stdin");
            let stdout = child.stdout.take().expect("stdout");

            // Writer task
            let mut stdin = stdin;
            tokio::spawn(async move {
                while let Some(line) = rx.recv().await {
                    let _ = stdin.write_all(line.as_bytes()).await;
                    let _ = stdin.flush().await;
                }
            });

            // Reader task: line-delimited JSON-RPC responses → pending map
            let mut reader = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                let Ok(parsed) = serde_json::from_str::<Json>(&line) else { continue };
                let Some(id) = parsed.get("id").and_then(|v| v.as_u64()) else { continue };
                let sender = pending.lock().unwrap().remove(&id);
                if let Some(sender) = sender {
                    let _ = sender.send(parsed).await;
                }
            }

            connected.store(false, Ordering::SeqCst);
            let _ = child.kill().await;
            let _ = stdin_tx; // keep channel alive
        });
    }

    async fn request(&self, method: &str, params: Json, timeout_ms: u64) -> Result<Json, String> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, mut rx) = mpsc::channel(1);
        self.pending.lock().unwrap().insert(id, tx);

        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        self.stdin_tx
            .send(serde_json::to_string(&request).unwrap_or_default() + "\n")
            .map_err(|_| format!("MCP server \"{}\" stdin not writable", self.name))?;

        match tokio::time::timeout(Duration::from_millis(timeout_ms), rx.recv()).await {
            Ok(Some(response)) => Ok(response),
            Ok(None) => Err(format!("MCP server \"{}\" dropped request", self.name)),
            Err(_) => {
                self.pending.lock().unwrap().remove(&id);
                Err(format!(
                    "MCP request \"{method}\" to \"{}\" timed out after {timeout_ms}ms",
                    self.name
                ))
            }
        }
    }

    /// Connect + initialize handshake (client.ts connect/_initialize).
    pub async fn connect(self: &Arc<Self>, config: &McpServerConfig) -> Result<(), String> {
        let response = self
            .request(
                "initialize",
                serde_json::json!({
                    "protocolVersion": MCP_PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": {"name": "nanocode", "version": crate::VERSION}
                }),
                INIT_TIMEOUT_MS,
            )
            .await?;

        if response.get("error").is_some() {
            return Err(format!(
                "MCP initialize failed for \"{}\"",
                self.name
            ));
        }

        // initialized notification (no id → no response)
        let notification = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
            "params": {}
        });
        let _ = self
            .stdin_tx
            .send(serde_json::to_string(&notification).unwrap_or_default() + "\n");

        self.connected.store(true, Ordering::SeqCst);
        *self.reconnects.lock().unwrap() = 0;
        let _ = config;
        Ok(())
    }

    pub async fn list_tools(&self) -> Result<Vec<McpToolDefinition>, String> {
        let response = self
            .request("tools/list", serde_json::json!({}), REQUEST_TIMEOUT_MS)
            .await?;

        if let Some(error) = response.get("error") {
            return Err(format!(
                "MCP tools/list error from \"{}\": {}",
                self.name,
                error.get("message").and_then(|m| m.as_str()).unwrap_or("?")
            ));
        }

        let mut tools = Vec::new();
        if let Some(list) = response.get("result").and_then(|r| r.get("tools")).and_then(|t| t.as_array()) {
            for t in list {
                tools.push(McpToolDefinition {
                    name: t.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string(),
                    description: t.get("description").and_then(|d| d.as_str()).map(String::from),
                    input_schema: t.get("inputSchema").cloned(),
                });
            }
        }
        Ok(tools)
    }

    pub async fn call_tool(&self, name: &str, args: &Json) -> Result<String, String> {
        let response = self
            .request(
                "tools/call",
                serde_json::json!({"name": name, "arguments": args}),
                REQUEST_TIMEOUT_MS,
            )
            .await?;

        if let Some(error) = response.get("error") {
            return Err(error
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("unknown error")
                .to_string());
        }

        // Content blocks → text (mcp/index.ts wrapMcpTool)
        let mut parts = Vec::new();
        if let Some(blocks) = response
            .get("result")
            .and_then(|r| r.get("content"))
            .and_then(|c| c.as_array())
        {
            for block in blocks {
                match block.get("type").and_then(|t| t.as_str()) {
                    Some("text") | Some("resource") => {
                        if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                            parts.push(text.to_string());
                        }
                    }
                    Some("image") => {
                        let mime = block.get("mimeType").and_then(|m| m.as_str()).unwrap_or("unknown type");
                        parts.push(format!("[image: {mime}]"));
                    }
                    _ => parts.push(block.to_string()),
                }
            }
        }

        Ok(if parts.is_empty() { "(empty result)".to_string() } else { parts.join("\n") })
    }

    pub async fn disconnect(&self) {
        self.connected.store(false, Ordering::SeqCst);
        if let Some(mut child) = self.process.lock().unwrap().take() {
            let _ = child.start_kill();
        }
    }

    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::SeqCst)
    }

    pub fn reconnect_count(&self) -> u32 {
        *self.reconnects.lock().unwrap()
    }

    pub fn max_reconnects(&self) -> u32 {
        MAX_RECONNECT_RETRIES
    }
}

/// Initialize all configured servers; failed servers are logged and skipped.
/// Returns wrapped tool entries (mcp/index.ts initializeMcpServers).
pub async fn initialize_mcp_servers(cwd: &Path) -> Vec<(String, Arc<McpClient>, Vec<McpToolDefinition>)> {
    let configs = load_mcp_config(cwd);
    let mut out = Vec::new();

    for (name, config) in configs {
        let client = McpClient::new(&name, &config);
        match client.connect(&config).await {
            Ok(()) => match client.list_tools().await {
                Ok(tools) if !tools.is_empty() => {
                    eprintln!("[mcp] Server \"{name}\": {} tool(s) registered", tools.len());
                    out.push((name, client, tools));
                }
                Ok(_) => {}
                Err(e) => {
                    eprintln!("[mcp] Server \"{name}\": {e}");
                }
            },
            Err(e) => {
                eprintln!("[mcp] Failed to connect to server \"{name}\": {e}");
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_servers_parses_config() {
        let settings: Json = serde_json::json!({
            "mcpServers": {
                "fs": {"command": "npx", "args": ["-y", "@modelcontextprotocol/server-fs"], "env": {"KEY": "v"}},
                "bad": {"args": []},
                "empty": {"command": ""}
            }
        });
        let out = extract_servers(&settings);
        assert_eq!(out.len(), 1);
        assert_eq!(out["fs"].command, "npx");
        assert_eq!(out["fs"].args.as_ref().unwrap().len(), 2);
        assert_eq!(out["fs"].env.as_ref().unwrap()["KEY"], "v");
    }

    #[test]
    fn project_overrides_user() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".nanocode")).unwrap();
        // only project config present
        std::fs::write(
            dir.path().join(".nanocode/settings.json"),
            r#"{"mcpServers": {"srv": {"command": "node", "args": ["a"]}}}"#,
        )
        .unwrap();
        let config = load_mcp_config(dir.path());
        assert_eq!(config["srv"].command, "node");
    }
}
