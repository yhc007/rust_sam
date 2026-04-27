//! MCP (Model Context Protocol) client — spawns external MCP servers and
//! communicates with them over stdio using JSON-RPC.

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tracing::{info, warn};

use sam_core::McpServerConfig;

use crate::types::ToolDefinition;

/// A running MCP server process with JSON-RPC communication over stdio.
pub struct McpClient {
    pub name: String,
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
    /// Cached tool definitions from this server.
    pub cached_tools: Vec<ToolDefinition>,
}

impl McpClient {
    /// Spawn an MCP server process and perform the initialization handshake.
    pub async fn spawn(config: &McpServerConfig) -> anyhow::Result<Self> {
        info!(name = %config.name, command = %config.command, "spawning MCP server");

        let mut cmd = Command::new(&config.command);
        cmd.args(&config.args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        for (k, v) in &config.env {
            cmd.env(k, v);
        }

        let mut child = cmd.spawn()?;

        let stdin = child.stdin.take()
            .ok_or_else(|| anyhow::anyhow!("failed to capture stdin of MCP server '{}'", config.name))?;
        let stdout = child.stdout.take()
            .ok_or_else(|| anyhow::anyhow!("failed to capture stdout of MCP server '{}'", config.name))?;

        let mut client = Self {
            name: config.name.clone(),
            child,
            stdin,
            stdout: BufReader::new(stdout),
            next_id: 1,
            cached_tools: Vec::new(),
        };

        // Send initialize request.
        let _init_resp = client.send_request("initialize", json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {
                "name": "sam-agent",
                "version": "0.1.0"
            }
        })).await?;

        // Send initialized notification (no id, no response expected).
        client.send_notification("notifications/initialized", json!({})).await?;

        // Discover tools.
        let tools = client.list_tools().await?;
        client.cached_tools = tools;

        info!(
            name = %client.name,
            tool_count = client.cached_tools.len(),
            "MCP server ready"
        );

        Ok(client)
    }

    /// List tools available from this MCP server.
    pub async fn list_tools(&mut self) -> anyhow::Result<Vec<ToolDefinition>> {
        let resp = self.send_request("tools/list", json!({})).await?;

        let tools_array = resp["tools"]
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("MCP server '{}' returned invalid tools/list response", self.name))?;

        let mut tools = Vec::new();
        for tool in tools_array {
            let name = tool["name"].as_str().unwrap_or("").to_string();
            let description = tool["description"].as_str().unwrap_or("").to_string();
            let input_schema = tool.get("inputSchema")
                .cloned()
                .unwrap_or(json!({"type": "object", "properties": {}}));

            if !name.is_empty() {
                tools.push(ToolDefinition {
                    name,
                    description,
                    input_schema,
                });
            }
        }

        Ok(tools)
    }

    /// Call a tool on this MCP server.
    pub async fn call_tool(&mut self, name: &str, arguments: Value) -> anyhow::Result<String> {
        let resp = self.send_request("tools/call", json!({
            "name": name,
            "arguments": arguments
        })).await?;

        // Extract text content from the result.
        if let Some(content) = resp["content"].as_array() {
            let mut result = String::new();
            for block in content {
                if block["type"].as_str() == Some("text") {
                    if let Some(text) = block["text"].as_str() {
                        if !result.is_empty() {
                            result.push('\n');
                        }
                        result.push_str(text);
                    }
                }
            }
            if result.is_empty() {
                Ok(serde_json::to_string_pretty(&resp)?)
            } else {
                Ok(result)
            }
        } else if let Some(text) = resp.as_str() {
            Ok(text.to_string())
        } else {
            Ok(serde_json::to_string_pretty(&resp)?)
        }
    }

    /// Check if this server provides a tool with the given name.
    pub fn has_tool(&self, name: &str) -> bool {
        self.cached_tools.iter().any(|t| t.name == name)
    }

    /// Send a JSON-RPC request and wait for a response.
    async fn send_request(&mut self, method: &str, params: Value) -> anyhow::Result<Value> {
        let id = self.next_id;
        self.next_id += 1;

        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        });

        let mut msg = serde_json::to_string(&request)?;
        msg.push('\n');
        self.stdin.write_all(msg.as_bytes()).await?;
        self.stdin.flush().await?;

        // Read response lines until we get one with a matching id.
        loop {
            let mut line = String::new();
            let bytes_read = self.stdout.read_line(&mut line).await?;
            if bytes_read == 0 {
                return Err(anyhow::anyhow!(
                    "MCP server '{}' closed stdout unexpectedly",
                    self.name
                ));
            }

            let line = line.trim();
            if line.is_empty() {
                continue;
            }

            let resp: Value = match serde_json::from_str(line) {
                Ok(v) => v,
                Err(_) => continue, // skip non-JSON lines
            };

            // Check if this is a response (has id field).
            if let Some(resp_id) = resp.get("id") {
                if resp_id.as_u64() == Some(id) {
                    if let Some(error) = resp.get("error") {
                        let msg = error["message"].as_str().unwrap_or("unknown error");
                        let code = error["code"].as_i64().unwrap_or(-1);
                        return Err(anyhow::anyhow!(
                            "MCP server '{}' error (code {}): {}",
                            self.name, code, msg
                        ));
                    }
                    return Ok(resp.get("result").cloned().unwrap_or(Value::Null));
                }
            }
            // Otherwise it might be a notification — skip it.
        }
    }

    /// Send a JSON-RPC notification (no id, no response expected).
    async fn send_notification(&mut self, method: &str, params: Value) -> anyhow::Result<()> {
        let notification = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params
        });

        let mut msg = serde_json::to_string(&notification)?;
        msg.push('\n');
        self.stdin.write_all(msg.as_bytes()).await?;
        self.stdin.flush().await?;
        Ok(())
    }

    /// Shutdown the MCP server process.
    pub async fn shutdown(&mut self) {
        // Try graceful shutdown first.
        let _ = self.send_notification("notifications/cancelled", json!({})).await;
        let _ = self.child.kill().await;
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        // Best-effort kill on drop (non-async).
        let _ = self.child.start_kill();
    }
}

/// Spawn all configured MCP servers. Returns the clients that started
/// successfully (logs warnings for failures).
pub async fn spawn_all(configs: &[McpServerConfig]) -> Vec<McpClient> {
    let mut clients = Vec::new();
    for config in configs {
        match McpClient::spawn(config).await {
            Ok(client) => clients.push(client),
            Err(e) => {
                warn!(
                    name = %config.name,
                    error = %e,
                    "failed to spawn MCP server, skipping"
                );
            }
        }
    }
    clients
}
