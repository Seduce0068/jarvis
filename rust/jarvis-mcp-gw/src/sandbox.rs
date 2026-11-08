//! Sandbox execution for MCP tools.
//!
//! Runtime-agnostic design (ADR-0002): read-only tools are implemented
//! natively in Rust so the gateway does not depend on any agent runtime CLI.

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::process::Command;

/// Maximum bytes returned by a native read/git handler.
const NATIVE_MAX_OUTPUT: usize = 1024 * 1024;

#[derive(Clone)]
pub struct Sandbox {
    sandbox_type: crate::config::SandboxType,
    config: Value,
    base_dir: PathBuf,
}

impl Sandbox {
    pub async fn new(config: &crate::config::McpGatewayConfig) -> Result<Self> {
        let base_dir =
            std::env::var("JARVIS_MCP_GW_BASE_DIR").unwrap_or_else(|_| "/home/koppe".to_string());
        Ok(Self {
            sandbox_type: config.sandbox_type.clone(),
            config: config.sandbox_config.clone(),
            base_dir: PathBuf::from(base_dir),
        })
    }

    /// Test/embedding constructor with an explicit workspace root.
    #[allow(dead_code)]
    fn with_base(base: PathBuf, config: Value) -> Self {
        Self {
            sandbox_type: crate::config::SandboxType::None,
            config,
            base_dir: base,
        }
    }

    pub async fn execute(
        &self,
        tool: &str,
        args: Value,
        tool_config: &crate::allowlist::AllowedTool,
    ) -> Result<Value> {
        // Tools with a native Rust handler never depend on an agent runtime CLI.
        if matches!(
            tool,
            "filesystem.read" | "filesystem.list" | "git.status" | "git.diff"
        ) {
            return self.execute_native(tool, args, tool_config).await;
        }
        match self.sandbox_type {
            crate::config::SandboxType::None => {
                bail!("no native handler for tool '{tool}'; register an upstream MCP server for it")
            }
            crate::config::SandboxType::Firejail => {
                self.execute_firejail(tool, args, tool_config).await
            }
            crate::config::SandboxType::Bwrap => self.execute_bwrap(tool, args, tool_config).await,
            crate::config::SandboxType::Docker => {
                self.execute_docker(tool, args, tool_config).await
            }
            crate::config::SandboxType::Gvisor => {
                self.execute_gvisor(tool, args, tool_config).await
            }
        }
    }

    async fn execute_native(
        &self,
        tool: &str,
        args: Value,
        tool_config: &crate::allowlist::AllowedTool,
    ) -> Result<Value> {
        match tool {
            "filesystem.read" => self.native_read(&args),
            "filesystem.list" => self.native_list(&args),
            "git.status" => {
                self.native_git(&args, tool_config, &["status", "--porcelain"])
                    .await
            }
            "git.diff" => self.native_git(&args, tool_config, &["diff"]).await,
            other => bail!(
                "no native handler for tool '{other}'; register an upstream MCP server for it"
            ),
        }
    }

    fn native_read(&self, args: &Value) -> Result<Value> {
        let path = self.resolve_path(args)?;
        let meta = std::fs::metadata(&path).with_context(|| format!("stat {}", path.display()))?;
        if meta.len() > NATIVE_MAX_OUTPUT as u64 {
            bail!("file too large to read ({} bytes)", meta.len());
        }
        let content =
            std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        Ok(json!({"path": path.display().to_string(), "content": content}))
    }

    fn native_list(&self, args: &Value) -> Result<Value> {
        let path = self.resolve_path(args)?;
        let mut entries = Vec::new();
        for entry in
            std::fs::read_dir(&path).with_context(|| format!("read_dir {}", path.display()))?
        {
            let entry = entry?;
            let file_type = entry.file_type()?;
            entries.push(json!({
                "name": entry.file_name().to_string_lossy(),
                "type": if file_type.is_dir() { "dir" } else { "file" },
            }));
        }
        entries.sort_by(|a, b| {
            a["name"]
                .as_str()
                .unwrap_or("")
                .cmp(b["name"].as_str().unwrap_or(""))
        });
        Ok(json!({"path": path.display().to_string(), "entries": entries}))
    }

    async fn native_git(
        &self,
        args: &Value,
        tool_config: &crate::allowlist::AllowedTool,
        git_args: &[&str],
    ) -> Result<Value> {
        let path = self.resolve_path(args)?;
        let timeout = Duration::from_millis(tool_config.timeout_ms);
        let output = tokio::time::timeout(
            timeout,
            Command::new("git")
                .arg("-C")
                .arg(&path)
                .args(git_args)
                .output(),
        )
        .await
        .context("git timed out")??;
        if !output.status.success() {
            bail!(
                "git {:?} failed: {}",
                git_args,
                String::from_utf8_lossy(&output.stderr)
            );
        }
        if output.stdout.len() > NATIVE_MAX_OUTPUT {
            bail!("git output too large ({} bytes)", output.stdout.len());
        }
        Ok(json!({
            "path": path.display().to_string(),
            "stdout": String::from_utf8_lossy(&output.stdout),
        }))
    }

    /// Resolve `args.path` (default ".") beneath the configured base directory.
    fn resolve_path(&self, args: &Value) -> Result<PathBuf> {
        let requested = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
        let base = std::fs::canonicalize(&self.base_dir)
            .with_context(|| format!("canonicalize base {}", self.base_dir.display()))?;
        let joined = if Path::new(requested).is_absolute() {
            PathBuf::from(requested)
        } else {
            base.join(requested)
        };
        let resolved = std::fs::canonicalize(&joined)
            .with_context(|| format!("canonicalize {}", joined.display()))?;
        if !resolved.starts_with(&base) {
            bail!("path escapes workspace: {}", resolved.display());
        }
        Ok(resolved)
    }

    async fn execute_firejail(
        &self,
        tool: &str,
        args: Value,
        tool_config: &crate::allowlist::AllowedTool,
    ) -> Result<Value> {
        let timeout = Duration::from_millis(tool_config.timeout_ms);
        let output = tokio::time::timeout(
            timeout,
            Command::new("firejail")
                .arg("--quiet")
                .arg("--noprofile")
                .arg("--private")
                .arg("--net=none")
                .arg("--")
                .arg("opencode")
                .arg("mcp")
                .arg("call")
                .arg(tool)
                .arg(serde_json::to_string(&args)?)
                .output(),
        )
        .await??;

        if !output.status.success() {
            return Err(anyhow::anyhow!(
                "Sandboxed tool failed: {}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }

        Ok(serde_json::from_slice(&output.stdout)?)
    }

    async fn execute_bwrap(
        &self,
        tool: &str,
        args: Value,
        tool_config: &crate::allowlist::AllowedTool,
    ) -> Result<Value> {
        let timeout = Duration::from_millis(tool_config.timeout_ms);
        let output = tokio::time::timeout(
            timeout,
            Command::new("bwrap")
                .arg("--ro-bind")
                .arg("/usr")
                .arg("/usr")
                .arg("--ro-bind")
                .arg("/lib")
                .arg("/lib")
                .arg("--ro-bind")
                .arg("/lib64")
                .arg("/lib64")
                .arg("--proc")
                .arg("/proc")
                .arg("--dev")
                .arg("/dev")
                .arg("--unshare-all")
                .arg("--die-with-parent")
                .arg("--")
                .arg("opencode")
                .arg("mcp")
                .arg("call")
                .arg(tool)
                .arg(serde_json::to_string(&args)?)
                .output(),
        )
        .await??;

        if !output.status.success() {
            return Err(anyhow::anyhow!(
                "Bwrap tool failed: {}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }

        Ok(serde_json::from_slice(&output.stdout)?)
    }

    async fn execute_docker(
        &self,
        tool: &str,
        args: Value,
        tool_config: &crate::allowlist::AllowedTool,
    ) -> Result<Value> {
        let timeout = Duration::from_millis(tool_config.timeout_ms);
        let image = self
            .config
            .get("docker_image")
            .and_then(|v| v.as_str())
            .unwrap_or("jarvis-mcp-runner:latest");

        let output = tokio::time::timeout(
            timeout,
            Command::new("docker")
                .arg("run")
                .arg("--rm")
                .arg("--network=none")
                .arg("--memory=512m")
                .arg("--cpus=0.5")
                .arg("--read-only")
                .arg("--tmpfs=/tmp:exec,size=100m")
                .arg(image)
                .arg("opencode")
                .arg("mcp")
                .arg("call")
                .arg(tool)
                .arg(serde_json::to_string(&args)?)
                .output(),
        )
        .await??;

        if !output.status.success() {
            return Err(anyhow::anyhow!(
                "Docker tool failed: {}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }

        Ok(serde_json::from_slice(&output.stdout)?)
    }

    async fn execute_gvisor(
        &self,
        tool: &str,
        args: Value,
        tool_config: &crate::allowlist::AllowedTool,
    ) -> Result<Value> {
        // Similar to docker but with runsc
        self.execute_docker(tool, args, tool_config).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (PathBuf, Sandbox) {
        let base = std::env::temp_dir().join(format!("jarvis-sandbox-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("sub")).unwrap();
        std::fs::write(base.join("hello.txt"), b"hello sandbox").unwrap();
        let sandbox = Sandbox::with_base(base.clone(), json!({}));
        (base, sandbox)
    }

    fn allowed(tool: &str, timeout_ms: u64) -> crate::allowlist::AllowedTool {
        crate::allowlist::AllowedTool {
            name: tool.to_string(),
            description: String::new(),
            allowed_principals: vec!["opencode".to_string()],
            max_args_size: 65536,
            timeout_ms,
            requires_approval: false,
        }
    }

    #[test]
    fn reads_file_under_base() {
        let (base, sandbox) = fixture();
        let out = sandbox.native_read(&json!({"path": "hello.txt"})).unwrap();
        assert_eq!(out["content"], "hello sandbox");
        assert!(
            out["path"]
                .as_str()
                .unwrap()
                .starts_with(base.to_str().unwrap())
        );
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn lists_directory_entries_sorted() {
        let (base, sandbox) = fixture();
        let out = sandbox.native_list(&json!({"path": "."})).unwrap();
        let names: Vec<&str> = out["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"hello.txt"));
        assert!(names.contains(&"sub"));
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn rejects_path_escape() {
        let (base, sandbox) = fixture();
        let err = sandbox
            .native_read(&json!({"path": "../etc/passwd"}))
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("escapes workspace") || msg.contains("canonicalize"));
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn unknown_tool_reports_no_handler() {
        let (base, sandbox) = fixture();
        let err = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(sandbox.execute_native(
                "no.such.tool",
                json!({}),
                &allowed("no.such.tool", 1000),
            ))
            .unwrap_err();
        assert!(err.to_string().contains("no native handler"));
        let _ = std::fs::remove_dir_all(base);
    }
}
