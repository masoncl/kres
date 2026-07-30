//! Configuration: which MCP servers are registered for this session.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::McpError;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// Executable to launch (PATH lookup applies if it's a bare name).
    pub command: String,
    /// Optional command args.
    #[serde(default)]
    pub args: Vec<String>,
    /// Optional env vars (merged on top of inherited env).
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Optional working directory.
    #[serde(default)]
    pub cwd: Option<PathBuf>,
}

impl ServerConfig {
    /// Return a config suitable for source-workspace MCP lookups.
    ///
    /// semcode indexes and resolves symbols relative to its process
    /// cwd, so workspace-owned callers must force semcode-like
    /// servers into the active source tree. For other servers, fill
    /// cwd only when the operator did not configure one explicitly.
    pub fn with_workspace_cwd(&self, server_name: &str, workspace: &Path) -> Self {
        let mut cfg = self.clone();
        if cfg.cwd.is_none() || server_name == "semcode" || cfg.command.contains("semcode") {
            cfg.cwd = Some(workspace.to_path_buf());
        }
        cfg
    }
}

/// Parsed `mcp.json` — the top-level shape is `{"mcpServers": {...}}`
/// to match and the wider MCP ecosystem.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerRegistry {
    #[serde(rename = "mcpServers", default)]
    pub servers: BTreeMap<String, ServerConfig>,
}

impl ServerRegistry {
    pub fn load_from_file(path: &Path) -> Result<Self, McpError> {
        let raw = std::fs::read_to_string(path)?;
        let parsed: ServerRegistry =
            serde_json::from_str(&raw).map_err(|source| McpError::Json {
                server: path.display().to_string(),
                source,
            })?;
        Ok(parsed)
    }

    pub fn get(&self, name: &str) -> Result<&ServerConfig, McpError> {
        self.servers
            .get(name)
            .ok_or_else(|| McpError::UnknownServer {
                server: name.to_string(),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_mcp_json() {
        let raw = r#"{"mcpServers": {"semcode": {"command": "semcode-mcp"}}}"#;
        let r: ServerRegistry = serde_json::from_str(raw).unwrap();
        assert_eq!(r.servers.len(), 1);
        let s = r.get("semcode").unwrap();
        assert_eq!(s.command, "semcode-mcp");
        assert!(s.args.is_empty());
        assert!(s.env.is_empty());
        assert!(s.cwd.is_none());
    }

    #[test]
    fn parses_full_shape() {
        let raw = r#"{
            "mcpServers": {
                "x": {
                    "command": "/usr/bin/x",
                    "args": ["--foo", "bar"],
                    "env": {"X_ENV": "1"},
                    "cwd": "/tmp"
                }
            }
        }"#;
        let r: ServerRegistry = serde_json::from_str(raw).unwrap();
        let s = r.get("x").unwrap();
        assert_eq!(s.args, vec!["--foo", "bar"]);
        assert_eq!(s.env.get("X_ENV").unwrap(), "1");
        assert_eq!(s.cwd.as_deref(), Some(Path::new("/tmp")));
    }

    #[test]
    fn workspace_cwd_forces_semcode_to_workspace() {
        let cfg = ServerConfig {
            command: "semcode-mcp".to_string(),
            args: Vec::new(),
            env: BTreeMap::new(),
            cwd: Some(PathBuf::from("/old")),
        };

        let got = cfg.with_workspace_cwd("semcode", Path::new("/src/linux"));
        assert_eq!(got.cwd.as_deref(), Some(Path::new("/src/linux")));
    }

    #[test]
    fn workspace_cwd_preserves_explicit_non_semcode_cwd() {
        let cfg = ServerConfig {
            command: "other-mcp".to_string(),
            args: Vec::new(),
            env: BTreeMap::new(),
            cwd: Some(PathBuf::from("/server")),
        };

        let got = cfg.with_workspace_cwd("other", Path::new("/src/linux"));
        assert_eq!(got.cwd.as_deref(), Some(Path::new("/server")));
    }

    #[test]
    fn unknown_server_errors() {
        let r = ServerRegistry::default();
        match r.get("missing").unwrap_err() {
            McpError::UnknownServer { server } => assert_eq!(server, "missing"),
            other => panic!("wrong: {other}"),
        }
    }
}
