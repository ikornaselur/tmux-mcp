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

const MAX_NAME_LEN: usize = 20;
const MAX_CMD_LEN: usize = 16;

#[derive(Debug, Clone)]
struct TmuxMcp {
    tool_router: ToolRouter<Self>,
    /// The pane ID (e.g. %47) this server process is running in, from $TMUX_PANE.
    current_pane_id: Option<String>,
}

// -- Helper types and functions --

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max.saturating_sub(3)).collect();
        format!("{truncated}...")
    }
}

fn align_columns(rows: &[Vec<String>]) -> Vec<String> {
    if rows.is_empty() {
        return Vec::new();
    }
    let col_count = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    let mut widths = vec![0usize; col_count];
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.len());
        }
    }
    rows.iter()
        .map(|row| {
            row.iter()
                .enumerate()
                .map(|(i, cell)| {
                    if i + 1 < row.len() {
                        format!("{:width$}", cell, width = widths[i])
                    } else {
                        cell.to_string()
                    }
                })
                .collect::<Vec<_>>()
                .join("  ")
        })
        .collect()
}

struct PaneInfo {
    pane_index: u32,
    width: u32,
    height: u32,
    current_command: String,
    is_active: bool,
    pane_id: String,
}

async fn fetch_panes(session: &str, window_index: &str) -> Result<Vec<PaneInfo>, String> {
    let target = format!("{session}:{window_index}");
    let format =
        "#{pane_index}\t#{pane_width}\t#{pane_height}\t#{pane_current_command}\t#{?pane_active,1,0}\t#{pane_id}";
    let output = run_tmux(&["list-panes", "-t", &target, "-F", format]).await?;
    let mut panes = Vec::new();
    for line in output.lines() {
        let fields: Vec<&str> = line.split('\t').collect();
        if fields.len() < 6 {
            continue;
        }
        panes.push(PaneInfo {
            pane_index: fields[0].parse().unwrap_or(0),
            width: fields[1].parse().unwrap_or(0),
            height: fields[2].parse().unwrap_or(0),
            current_command: fields[3].to_string(),
            is_active: fields[4] == "1",
            pane_id: fields[5].to_string(),
        });
    }
    Ok(panes)
}

// -- Tool parameter types --

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ListSessionsRequest {
    #[schemars(
        description = "When true, show a full tree with sessions, windows, and panes. Defaults to false."
    )]
    verbose: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ListWindowsRequest {
    #[schemars(
        description = "Optional session name to filter by. If omitted, lists windows from all sessions."
    )]
    session: Option<String>,

    #[schemars(
        description = "When true, show pane details beneath each window. Defaults to false."
    )]
    verbose: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct GetPaneContentsRequest {
    #[schemars(
        description = "Target pane. Formats:\n- \"x\" - pane x in current window\n- \"y.x\" - pane x in window y (current session)\n- \"sess:y.x\" - pane x in window y in session sess\nExamples: \"1\", \"5.1\", \"API:5.1\""
    )]
    target: String,

    #[schemars(
        description = "Number of lines of scrollback history to include. 0 means visible area only. Defaults to 1000."
    )]
    scroll_back_lines: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct GetWindowContentsRequest {
    #[schemars(
        description = "Target window. Formats:\n- \"y\" - window y in current session\n- \"sess:y\" - window y in session sess\nExamples: \"5\", \"API:5\"\nIf omitted, defaults to the current window."
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

    #[tool(
        description = "List all tmux sessions with their properties. Set verbose=true for a full tree showing sessions, windows, and panes."
    )]
    async fn list_sessions(
        &self,
        Parameters(req): Parameters<ListSessionsRequest>,
    ) -> String {
        let verbose = req.verbose.unwrap_or(false);

        let format =
            "#{session_name}\t#{session_windows}\t#{?session_attached,attached,detached}";
        let output = match run_tmux(&["list-sessions", "-F", format]).await {
            Ok(o) => o,
            Err(e) => return e,
        };

        struct SessionRow {
            name: String,
            window_count: String,
            state: String,
        }

        let sessions: Vec<SessionRow> = output
            .lines()
            .filter_map(|line| {
                let f: Vec<&str> = line.split('\t').collect();
                if f.len() < 3 {
                    return None;
                }
                Some(SessionRow {
                    name: f[0].to_string(),
                    window_count: f[1].to_string(),
                    state: f[2].to_string(),
                })
            })
            .collect();

        if !verbose {
            let rows: Vec<Vec<String>> = sessions
                .iter()
                .map(|s| {
                    vec![
                        truncate(&s.name, MAX_NAME_LEN),
                        format!("({})", s.state),
                        format!(
                            "{} window{}",
                            s.window_count,
                            if s.window_count == "1" { "" } else { "s" }
                        ),
                    ]
                })
                .collect();
            return align_columns(&rows).join("\n");
        }

        // Verbose: full tree
        let mut out = String::new();
        for session in &sessions {
            let wcount: u32 = session.window_count.parse().unwrap_or(0);
            out.push_str(&format!(
                "{} ({})  {} window{}\n",
                truncate(&session.name, MAX_NAME_LEN),
                session.state,
                wcount,
                if wcount == 1 { "" } else { "s" }
            ));

            // Fetch windows for this session
            let win_format = "#{window_index}\t#{window_name}\t#{window_panes}";
            let wins = match run_tmux(&[
                "list-windows",
                "-t",
                &format!("{}:", session.name),
                "-F",
                win_format,
            ])
            .await
            {
                Ok(w) => w,
                Err(_) => continue,
            };

            for win_line in wins.lines() {
                let wf: Vec<&str> = win_line.split('\t').collect();
                if wf.len() < 3 {
                    continue;
                }
                let win_idx = wf[0];
                let win_name = wf[1];
                let pane_count: u32 = wf[2].parse().unwrap_or(0);

                out.push_str(&format!(
                    "  {}:  {}  {} pane{}\n",
                    win_idx,
                    truncate(win_name, MAX_NAME_LEN),
                    pane_count,
                    if pane_count == 1 { "" } else { "s" }
                ));

                // Fetch panes
                let panes = match fetch_panes(&session.name, win_idx).await {
                    Ok(p) => p,
                    Err(_) => continue,
                };

                let pane_rows: Vec<Vec<String>> = panes
                    .iter()
                    .map(|p| {
                        let mut cols = vec![
                            format!("    .{}", p.pane_index),
                            format!("{}x{}", p.width, p.height),
                            truncate(&p.current_command, MAX_CMD_LEN),
                        ];
                        let mut suffix = String::new();
                        if p.is_active {
                            suffix.push_str("(active)");
                        }
                        if self.current_pane_id.as_deref() == Some(p.pane_id.as_str()) {
                            if !suffix.is_empty() {
                                suffix.push_str("  ");
                            }
                            suffix.push_str("<-- current");
                        }
                        if !suffix.is_empty() {
                            cols.push(suffix);
                        }
                        cols
                    })
                    .collect();

                for line in align_columns(&pane_rows) {
                    out.push_str(&line);
                    out.push('\n');
                }
            }
        }

        out.trim_end().to_string()
    }

    #[tool(
        description = "List tmux windows. Optionally filter by session name. Set verbose=true to include pane details beneath each window."
    )]
    async fn list_windows(
        &self,
        Parameters(req): Parameters<ListWindowsRequest>,
    ) -> String {
        let verbose = req.verbose.unwrap_or(false);

        let format = "#{session_name}\t#{window_index}\t#{window_name}\t#{window_panes}\t#{?window_active,active,}";

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

        // Resolve current window/pane for markers
        let current_window = match &self.current_pane_id {
            Some(pane_id) => {
                resolve_pane_id(pane_id, "#{session_name}:#{window_index}")
                    .await
                    .ok()
            }
            None => None,
        };

        struct WinRow {
            session: String,
            index: String,
            name: String,
            pane_count: String,
            active: String,
        }

        let windows: Vec<WinRow> = output
            .lines()
            .filter_map(|line| {
                let f: Vec<&str> = line.split('\t').collect();
                if f.len() < 5 {
                    return None;
                }
                Some(WinRow {
                    session: f[0].to_string(),
                    index: f[1].to_string(),
                    name: f[2].to_string(),
                    pane_count: f[3].to_string(),
                    active: f[4].to_string(),
                })
            })
            .collect();

        if !verbose {
            let rows: Vec<Vec<String>> = windows
                .iter()
                .map(|w| {
                    let key = format!("{}:{}", w.session, w.index);
                    let pcount: u32 = w.pane_count.parse().unwrap_or(0);
                    let mut cols = vec![
                        key.clone(),
                        truncate(&w.name, MAX_NAME_LEN),
                        format!(
                            "{} pane{}",
                            pcount,
                            if pcount == 1 { "" } else { "s" }
                        ),
                    ];
                    if w.active == "active" {
                        cols.push("active".to_string());
                    }
                    if current_window.as_deref() == Some(&key) {
                        cols.push("<-- current".to_string());
                    }
                    cols
                })
                .collect();
            return align_columns(&rows).join("\n");
        }

        // Verbose: windows with panes expanded
        let mut out = String::new();
        for w in &windows {
            let key = format!("{}:{}", w.session, w.index);
            let pcount: u32 = w.pane_count.parse().unwrap_or(0);
            let is_current_window = current_window.as_deref() == Some(&key);

            out.push_str(&format!(
                "{}:  {}  {} pane{}",
                key,
                truncate(&w.name, MAX_NAME_LEN),
                pcount,
                if pcount == 1 { "" } else { "s" }
            ));
            if w.active == "active" {
                out.push_str("  active");
            }
            out.push('\n');

            // Fetch panes if this window is accessible
            let panes = match fetch_panes(&w.session, &w.index).await {
                Ok(p) => p,
                Err(_) => continue,
            };

            let pane_rows: Vec<Vec<String>> = panes
                .iter()
                .map(|p| {
                    let mut cols = vec![
                        format!("  .{}", p.pane_index),
                        format!("{}x{}", p.width, p.height),
                        truncate(&p.current_command, MAX_CMD_LEN),
                    ];
                    let mut suffix = String::new();
                    if p.is_active {
                        suffix.push_str("(active)");
                    }
                    if is_current_window
                        && self.current_pane_id.as_deref() == Some(p.pane_id.as_str())
                    {
                        if !suffix.is_empty() {
                            suffix.push_str("  ");
                        }
                        suffix.push_str("<-- current");
                    }
                    if !suffix.is_empty() {
                        cols.push(suffix);
                    }
                    cols
                })
                .collect();

            for line in align_columns(&pane_rows) {
                out.push_str(&line);
                out.push('\n');
            }
        }

        out.trim_end().to_string()
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
        description = "Get the contents of a specific tmux pane. Supports scrollback history."
    )]
    async fn get_pane_contents(
        &self,
        Parameters(req): Parameters<GetPaneContentsRequest>,
    ) -> String {
        let scroll_back = req.scroll_back_lines.unwrap_or(1000);
        let t = req.target.trim().to_string();

        // Resolve target to session:window.pane format.
        let target = if t.contains(':') {
            // "sess:y.x" - fully qualified
            if !t.contains('.') {
                return format!("Invalid target \"{t}\": expected \"sess:window.pane\" but no pane specifier found. Use get_window_contents to read an entire window.");
            }
            t
        } else if t.contains('.') {
            // "y.x" - window.pane, prepend current session
            let Some(pane_id) = &self.current_pane_id else {
                return "Not running inside tmux".into();
            };
            match resolve_pane_id(pane_id, "#{session_name}").await {
                Ok(session) => format!("{session}:{t}"),
                Err(e) => return e,
            }
        } else {
            // "x" - bare pane index, prepend current session:window
            let Some(pane_id) = &self.current_pane_id else {
                return "Not running inside tmux".into();
            };
            match resolve_pane_id(pane_id, "#{session_name}:#{window_index}").await {
                Ok(current_window) => format!("{current_window}.{t}"),
                Err(e) => return e,
            }
        };

        capture_pane(&target, scroll_back).await
    }

    #[tool(
        description = "Get the contents of all panes in a tmux window. Supports scrollback history. If target is omitted, defaults to the current window."
    )]
    async fn get_window_contents(
        &self,
        Parameters(req): Parameters<GetWindowContentsRequest>,
    ) -> String {
        let scroll_back = req.scroll_back_lines.unwrap_or(1000);

        let target = match req.target {
            Some(t) if t.contains(':') => t,
            Some(t) => {
                // Bare window index, prepend current session
                let Some(pane_id) = &self.current_pane_id else {
                    return "Not running inside tmux".into();
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

#[tool_handler]
impl ServerHandler for TmuxMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some(
                "MCP server for interacting with tmux sessions, windows, and panes. \
                 Use list_sessions to discover sessions, list_windows to see windows, \
                 get_pane_contents to read a specific pane, and get_window_contents to read all panes in a window."
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
