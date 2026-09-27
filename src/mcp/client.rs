//! Lightweight MCP client — JSON-RPC over stdio.
//!
//! Spawns senclaw MCP server subprocesses and communicates via stdin/stdout.
//! Mirrors the TS `child_process.spawn()` + JSON-RPC pattern used in the
//! original sema-core MCP harness.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tracing::{debug, info, warn};

// ===== JSON-RPC types =====

#[derive(Debug, Serialize)]
struct JsonRpcRequest {
    jsonrpc: &'static str,
    id: u64,
    method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    params: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct JsonRpcResponse {
    #[allow(dead_code)]
    jsonrpc: Option<String>,
    #[allow(dead_code)]
    id: Option<u64>,
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    error: Option<JsonRpcError>,
}

#[derive(Debug, Deserialize)]
struct JsonRpcError {
    #[allow(dead_code)]
    code: i64,
    message: String,
}

// ===== Tool info =====

#[derive(Debug, Deserialize)]
pub struct McpToolInfo {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default, rename = "inputSchema")]
    pub input_schema: Value,
}

// ===== Client =====

/// A running MCP server subprocess with JSON-RPC communication.
pub struct McpClient {
    _name: String,
    child: Child,
    next_id: u64,
    reader: BufReader<tokio::process::ChildStdout>,
    writer: tokio::process::ChildStdin,
    request_timeout: Duration,
}

impl McpClient {
    /// Spawn the MCP server subprocess and perform the initialize handshake.
    pub async fn spawn(
        name: &str,
        command: &str,
        args: &[String],
        env: &HashMap<String, String>,
        request_timeout: Duration,
    ) -> Result<Self> {
        info!("MCP spawn: {name} ({command} {})", args.join(" "));

        let mut cmd = Command::new(command);
        cmd.args(args);
        cmd.envs(env);
        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::inherit());
        cmd.kill_on_drop(true);

        let mut child = cmd
            .spawn()
            .with_context(|| format!("Failed to spawn MCP server: {name}"))?;

        let stdout = child.stdout.take().context("MCP stdout pipe")?;
        let stdin = child.stdin.take().context("MCP stdin pipe")?;
        let reader = BufReader::new(stdout);

        let mut client = Self {
            _name: name.to_string(),
            child,
            next_id: 1,
            reader,
            writer: stdin,
            request_timeout,
        };

        // Initialize handshake
        client
            .request(
                "initialize",
                Some(serde_json::json!({
                    "protocolVersion": "0.1.0",
                    "capabilities": {},
                    "clientInfo": {
                        "name": "senclaw-engine",
                        "version": "0.1.0"
                    }
                })),
            )
            .await?;

        info!("MCP {name} initialized");
        Ok(client)
    }

    /// List available tools from this MCP server.
    pub async fn list_tools(&mut self) -> Result<Vec<McpToolInfo>> {
        let response = self.request("tools/list", None).await?;
        let tools: Vec<McpToolInfo> = response
            .get("tools")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();
        debug!("MCP {}: {} tool(s) available", self._name, tools.len());
        Ok(tools)
    }

    /// Call a tool on the MCP server.
    pub async fn call_tool(&mut self, name: &str, arguments: Value) -> Result<Value> {
        self.request(
            "tools/call",
            Some(serde_json::json!({
                "name": name,
                "arguments": arguments,
            })),
        )
        .await
    }

    /// Send a JSON-RPC request and wait for the response with timeout.
    async fn request(&mut self, method: &str, params: Option<Value>) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;

        let req = JsonRpcRequest {
            jsonrpc: "2.0",
            id,
            method: method.to_string(),
            params,
        };

        let mut body = serde_json::to_vec(&req)?;
        body.push(b'\n');
        self.writer.write_all(&body).await?;

        // Read response line with timeout
        let timeout = self.request_timeout;
        let mut line = String::new();

        match tokio::time::timeout(timeout, self.reader.read_line(&mut line)).await {
            Ok(Ok(0)) => {
                // EOF - subprocess likely crashed
                bail!("MCP subprocess closed connection (method: {})", method);
            }
            Ok(Ok(_)) => {
                // Successfully read line
            }
            Ok(Err(e)) => {
                bail!("MCP read error ({}): {}", method, e);
            }
            Err(_) => {
                // Timeout
                bail!(
                    "MCP request timeout after {:?} (method: {})",
                    timeout,
                    method
                );
            }
        }

        let resp: JsonRpcResponse = serde_json::from_str(&line)
            .with_context(|| format!("MCP JSON-RPC parse error: {line}"))?;

        if let Some(err) = resp.error {
            bail!("MCP error ({}): {}", method, err.message);
        }

        Ok(resp.result.unwrap_or(Value::Null))
    }

    /// Check if process has exited.
    pub fn try_wait(&mut self) -> Option<std::process::ExitStatus> {
        self.child.try_wait().ok().flatten()
    }

    /// Check if the subprocess is still alive and responsive.
    pub fn is_alive(&mut self) -> bool {
        // Check if process has exited
        if let Some(_status) = self.try_wait() {
            return false;
        }
        true
    }

    /// Get the subprocess ID for debugging.
    pub fn pid(&self) -> Option<u32> {
        self.child.id()
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        // kill_on_drop is set, so the process will be killed automatically.
        info!("MCP {} — subprocess dropped", self._name);
    }
}

/// Registry of active MCP clients keyed by server name.
#[derive(Default)]
pub struct McpRegistry {
    clients: HashMap<String, McpClient>,
}

impl McpRegistry {
    pub fn new() -> Self {
        Self {
            clients: HashMap::new(),
        }
    }

    /// Spawn or replace an MCP server.
    pub async fn spawn(
        &mut self,
        name: &str,
        command: &str,
        args: &[String],
        env: &HashMap<String, String>,
        request_timeout: Duration,
    ) -> Result<Vec<McpToolInfo>> {
        // Kill old instance if present (drops it, releasing OS resources).
        self.kill(name);
        let mut client = McpClient::spawn(name, command, args, env, request_timeout).await?;
        let tools = client.list_tools().await.unwrap_or_default();
        self.clients.insert(name.to_string(), client);
        Ok(tools)
    }

    /// Spawn without holding &mut self across .await.
    /// Returns the client so the caller can insert it under the lock.
    pub async fn spawn_client(
        name: &str,
        command: &str,
        args: &[String],
        env: &HashMap<String, String>,
        request_timeout: Duration,
    ) -> Result<(McpClient, Vec<McpToolInfo>)> {
        let mut client = McpClient::spawn(name, command, args, env, request_timeout).await?;
        let tools = client.list_tools().await.unwrap_or_default();
        Ok((client, tools))
    }

    /// Kill a specific MCP server by name.
    pub fn kill(&mut self, name: &str) {
        if let Some(_client) = self.clients.remove(name) {
            info!("MCP {} — killed", name);
        }
    }

    /// Kill all MCP servers.
    pub fn kill_all(&mut self) {
        let names: Vec<String> = self.clients.keys().cloned().collect();
        for name in &names {
            self.kill(name);
        }
    }
}

impl Drop for McpRegistry {
    fn drop(&mut self) {
        self.kill_all();
    }
}

/// Shared, thread-safe MCP client registry.
#[derive(Clone)]
pub struct SharedMcpRegistry {
    pub(crate) inner: Arc<std::sync::Mutex<McpRegistry>>,
}

impl SharedMcpRegistry {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(std::sync::Mutex::new(McpRegistry::new())),
        }
    }

    pub async fn spawn(
        &self,
        name: &str,
        command: &str,
        args: &[String],
        env: &HashMap<String, String>,
        request_timeout: Duration,
    ) -> Result<Vec<McpToolInfo>> {
        // Kill old instance under the lock, then spawn outside it
        // so MutexGuard doesn't live across .await.
        {
            let mut reg = self.inner.lock().unwrap();
            reg.kill(name);
        }
        let (client, tools) =
            McpRegistry::spawn_client(name, command, args, env, request_timeout).await?;
        let mut reg = self.inner.lock().unwrap();
        reg.clients.insert(name.to_string(), client);
        Ok(tools)
    }

    /// True when a server of this name is already spawned. Lets callers skip a
    /// respawn — [`Self::spawn`] deliberately kills and replaces.
    pub fn has_server(&self, name: &str) -> bool {
        self.inner.lock().unwrap().clients.contains_key(name)
    }

    pub fn kill(&self, name: &str) {
        let mut reg = self.inner.lock().unwrap();
        reg.kill(name);
    }

    pub fn kill_all(&self) {
        let mut reg = self.inner.lock().unwrap();
        reg.kill_all();
    }

    /// Health check all MCP servers and remove dead ones.
    /// Returns count of servers that were removed.
    pub fn health_check(&self) -> usize {
        let mut reg = self.inner.lock().unwrap();
        let mut dead_servers = Vec::new();

        for (name, client) in reg.clients.iter_mut() {
            if !client.is_alive() {
                warn!(
                    "[MCP] Health check: server '{}' (pid: {:?}) is dead, removing",
                    name,
                    client.pid()
                );
                dead_servers.push(name.clone());
            }
        }

        let dead_count = dead_servers.len();
        for name in dead_servers {
            reg.clients.remove(&name);
        }

        dead_count
    }

    /// Get list of active server names.
    pub fn list_servers(&self) -> Vec<String> {
        let reg = self.inner.lock().unwrap();
        reg.clients.keys().cloned().collect()
    }

    /// Get server health status.
    pub fn get_server_status(&self, server_name: &str) -> Option<bool> {
        let mut reg = self.inner.lock().unwrap();
        reg.clients
            .get_mut(server_name)
            .map(|client| client.is_alive())
    }

    /// Call a tool on a specific MCP server by server name and short tool name.
    /// Includes watchdog monitoring to detect and handle hung subprocesses.
    pub async fn call_tool(
        &self,
        server_name: &str,
        tool_name: &str,
        arguments: serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        // We cannot hold the MutexGuard across .await, so we serialise the
        // request manually: grab the client, send the request, release the guard,
        // then read the response.
        //
        // McpClient uses a BufReader over stdout; the IO must happen with the
        // client held. Since McpClient::call_tool takes &mut self we need the
        // guard for the whole call. To avoid the Send issue we run the blocking
        // portion in spawn_blocking.
        let inner = self.inner.clone();
        let server = server_name.to_string();
        let tool = tool_name.to_string();

        // Check server health before calling
        {
            let mut reg = inner.lock().unwrap();
            if let Some(client) = reg.clients.get_mut(&server) {
                if !client.is_alive() {
                    warn!(
                        "[MCP] Server '{}' subprocess is dead, cannot call tool '{}'",
                        server, tool
                    );
                    return Err(anyhow::anyhow!(
                        "MCP server '{}' subprocess is not alive",
                        server
                    ));
                }
            }
        }

        let inner_clone = inner.clone();
        let server_clone = server.clone();
        let tool_clone = tool.clone();

        let result = tokio::task::spawn_blocking(move || {
            let mut reg = inner_clone.lock().unwrap();
            let client = reg
                .clients
                .get_mut(&server_clone)
                .ok_or_else(|| anyhow::anyhow!("MCP server '{server_clone}' not in registry"))?;
            // call_tool is async, but we are inside spawn_blocking.
            // Use Handle::current().block_on to drive the future.
            tokio::runtime::Handle::current().block_on(client.call_tool(&tool_clone, arguments))
        })
        .await;

        match result {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(e)) => {
                // Tool call failed - check if subprocess is still alive
                let mut reg = inner.lock().unwrap();
                if let Some(client) = reg.clients.get_mut(&server) {
                    if !client.is_alive() {
                        warn!(
                            "[MCP] Server '{}' subprocess died during tool '{}': {}",
                            server, tool, e
                        );
                        // Remove dead client
                        reg.clients.remove(&server);
                    }
                }
                Err(e)
            }
            Err(e) => {
                // spawn_blocking failed (likely panic or cancellation)
                warn!(
                    "[MCP] spawn_blocking failed for server '{}' tool '{}': {}",
                    server, tool, e
                );
                Err(anyhow::anyhow!("spawn_blocking failed: {e}"))
            }
        }
    }

    /// Start a background watchdog task that periodically checks MCP server health.
    /// This task will run until the registry is dropped or the token is cancelled.
    pub fn start_watchdog(
        &self,
        interval_secs: u64,
        cancel_token: tokio_util::sync::CancellationToken,
    ) {
        let registry = self.inner.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(interval_secs));
            loop {
                tokio::select! {
                    _ = cancel_token.cancelled() => {
                        info!("[MCP] Watchdog task cancelled");
                        break;
                    }
                    _ = interval.tick() => {
                        let dead_servers = {
                            let mut reg = registry.lock().unwrap();
                            let mut dead = Vec::new();

                            for (name, client) in reg.clients.iter_mut() {
                                if !client.is_alive() {
                                    warn!("[MCP] Watchdog: server '{}' (pid: {:?}) is dead", name, client.pid());
                                    dead.push(name.clone());
                                }
                            }

                            dead
                        };

                        if !dead_servers.is_empty() {
                            info!("[MCP] Watchdog: removing {} dead server(s)", dead_servers.len());
                            let mut reg = registry.lock().unwrap();
                            for name in dead_servers {
                                reg.clients.remove(&name);
                            }
                        }
                    }
                }
            }
        });
    }
}
