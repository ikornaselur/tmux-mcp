use std::process::Stdio;

use anyhow::Result;
use rmcp::{
    ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{ServerCapabilities, ServerInfo},
    schemars, tool, tool_handler, tool_router,
    transport::stdio,
};
use serde::Deserialize;
use tokio::process::Command;
use tracing_subscriber::{self, EnvFilter};

#[derive(Debug, Clone)]
struct TmuxMcp {
    tool_router: ToolRouter<Self>,
    /// The pane ID (e.g. %47) this server process is running in, from $TMUX_PANE.
    current_pane_id: Option<String>,
}

// -- Tool parameter types --

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ListWindowsRequest {
    #[schemars(description = "Optional session name to filter by. If omitted, lists windows from all sessions.")]
    session: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct GetPaneContentsRequest {
    #[schemars(
        description = "Target in tmux format. Can be:\n- \"session:window\" to get all panes in a window\n- \"session:window.pane\" to get a specific pane\nExamples: \"API:5\", \"API:5.1\"\nSession is optional — if omitted from the target (e.g., \"5\" or \"5.1\"), the current session is used.\nIf target is omitted entirely, defaults to the current window."
    )]
    target: Option<String>,

    #[schemars(
        description = "Number of lines of scrollback history to include. 0 means visible area only. Defaults to 1000."
    )]
    scroll_back_lines: Option<u32>,
}

// -- Helpers --

async fn run_tmux(args: &[&str]) -> Result<String, String> {
    let output = Command::new("tmux")
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| format!("Failed to run tmux: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("tmux error: {stderr}"));
    }

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Given a pane ID like %47, query tmux for session:window or session:window.pane.
async fn resolve_pane_id(pane_id: &str, format: &str) -> Result<String, String> {
    run_tmux(&["display-message", "-t", pane_id, "-p", format])
        .await
        .map(|s| s.trim().to_string())
}

async fn capture_pane(target: &str, scroll_back: u32) -> String {
    let start_line = if scroll_back > 0 {
        format!("-{scroll_back}")
    } else {
        "0".to_string()
    };

    match run_tmux(&["capture-pane", "-p", "-J", "-t", target, "-S", &start_line]).await {
        Ok(contents) => contents,
        Err(e) => format!("Error capturing {target}: {e}\n"),
    }
}

// -- Tool implementations --

#[tool_router]
impl TmuxMcp {
    fn new() -> Self {
        let current_pane_id = std::env::var("TMUX_PANE").ok();
        Self {
            tool_router: Self::tool_router(),
            current_pane_id,
        }
    }

    #[tool(description = "List all tmux sessions with their properties")]
    async fn list_sessions(&self) -> String {
        let format = "#{session_name}\t#{session_windows} windows\t#{session_created}\t#{?session_attached,attached,detached}";
        match run_tmux(&["list-sessions", "-F", format]).await {
            Ok(output) => output,
            Err(e) => e,
        }
    }

    #[tool(description = "List tmux windows. Optionally filter by session name.")]
    async fn list_windows(
        &self,
        Parameters(req): Parameters<ListWindowsRequest>,
    ) -> String {
        let format = "#{session_name}:#{window_index}\t#{window_name}\t#{window_panes} panes\t#{?window_active,active,}";

        let result = match &req.session {
            Some(session) => {
                let target = format!("{session}:");
                run_tmux(&["list-windows", "-t", &target, "-F", format]).await
            }
            None => run_tmux(&["list-windows", "-a", "-F", format]).await,
        };

        let output = match result {
            Ok(output) => output,
            Err(e) => return e,
        };

        // Mark the window this MCP server is running in
        let current = match &self.current_pane_id {
            Some(pane_id) => resolve_pane_id(pane_id, "#{session_name}:#{window_index}").await.ok(),
            None => None,
        };

        output
            .lines()
            .map(|line| {
                let key = line.split('\t').next().unwrap_or("");
                if current.as_deref() == Some(key) {
                    format!("{line}\t<-- current")
                } else {
                    line.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[tool(
        description = "Get the current tmux session name and window that this MCP server is running in."
    )]
    async fn get_current_session(&self) -> String {
        let Some(pane_id) = &self.current_pane_id else {
            return "Not running inside tmux (TMUX_PANE not set)".into();
        };

        match resolve_pane_id(pane_id, "#{session_name}:#{window_index} (window: #{window_name})")
            .await
        {
            Ok(info) => info,
            Err(e) => e,
        }
    }

    #[tool(
        description = "Get the current tmux window index and name that this MCP server is running in."
    )]
    async fn get_current_window(&self) -> String {
        let Some(pane_id) = &self.current_pane_id else {
            return "Not running inside tmux (TMUX_PANE not set)".into();
        };

        match resolve_pane_id(
            pane_id,
            "#{session_name}:#{window_index}\t#{window_name}\t#{window_panes} panes",
        )
        .await
        {
            Ok(info) => info,
            Err(e) => e,
        }
    }

    #[tool(
        description = "Get the contents of a tmux pane or all panes in a window. Supports scrollback history. If target is omitted, defaults to the current window."
    )]
    async fn get_pane_contents(
        &self,
        Parameters(req): Parameters<GetPaneContentsRequest>,
    ) -> String {
        let scroll_back = req.scroll_back_lines.unwrap_or(1000);

        // Resolve target, defaulting to current window.
        // If a target is provided without a session prefix (no ':'), prepend the current session.
        let target = match req.target {
            Some(t) if t.contains(':') => t,
            Some(t) => {
                let Some(pane_id) = &self.current_pane_id else {
                    return "Target has no session prefix and not running inside tmux".into();
                };
                match resolve_pane_id(pane_id, "#{session_name}").await {
                    Ok(session) => format!("{session}:{t}"),
                    Err(e) => return e,
                }
            }
            None => {
                let Some(pane_id) = &self.current_pane_id else {
                    return "No target specified and not running inside tmux".into();
                };
                match resolve_pane_id(pane_id, "#{session_name}:#{window_index}").await {
                    Ok(t) => t,
                    Err(e) => return e,
                }
            }
        };

        // Check if target includes a pane specifier (has a dot)
        let has_pane = target.contains('.');

        if has_pane {
            // Single pane
            capture_pane(&target, scroll_back).await
        } else {
            // Whole window — list panes then capture each
            let pane_format = "#{session_name}:#{window_index}.#{pane_index}\t#{pane_title}\t#{pane_width}x#{pane_height}\t#{?pane_active,active,}";

            let panes = match run_tmux(&["list-panes", "-t", &target, "-F", pane_format]).await {
                Ok(p) => p,
                Err(e) => return e,
            };

            let mut output = String::new();
            for line in panes.lines() {
                let pane_target = line.split('\t').next().unwrap_or("");
                if pane_target.is_empty() {
                    continue;
                }

                output.push_str(&format!("=== Pane {pane_target} ({}) ===\n", line));
                output.push_str(&capture_pane(pane_target, scroll_back).await);
                output.push('\n');
            }
            output
        }
    }
}

#[tool_handler]
impl ServerHandler for TmuxMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some(
                "MCP server for interacting with tmux sessions, windows, and panes. \
                 Use list_sessions to discover sessions, list_windows to see windows, \
                 and get_pane_contents to read terminal output including scrollback history."
                    .into(),
            ),
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            ..Default::default()
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive(tracing::Level::DEBUG.into()))
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    tracing::info!("Starting tmux-mcp server");

    let service = TmuxMcp::new().serve(stdio()).await.inspect_err(|e| {
        tracing::error!("serving error: {:?}", e);
    })?;

    service.waiting().await?;
    Ok(())
}
