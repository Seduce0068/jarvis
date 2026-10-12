//! Sandbox execution for MCP tools

use anyhow::Result;
use serde_json::Value;
use std::time::Duration;
use tokio::process::Command;

#[derive(Clone)]
pub struct Sandbox {
    sandbox_type: crate::config::SandboxType,
    config: Value,
}

impl Sandbox {
    pub async fn new(config: &crate::config::McpGatewayConfig) -> Result<Self> {
        Ok(Self {
            sandbox_type: config.sandbox_type.clone(),
            config: config.sandbox_config.clone(),
        })
    }

    pub async fn execute(
        &self,
        tool: &str,
        args: Value,
        tool_config: &crate::allowlist::AllowedTool,
    ) -> Result<Value> {
        match self.sandbox_type {
            crate::config::SandboxType::None => self.execute_native(tool, args).await,
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

    async fn execute_native(&self, tool: &str, args: Value) -> Result<Value> {
        // Direct execution - only for trusted tools
        let output = Command::new("opencode")
            .arg("mcp")
            .arg("call")
            .arg(tool)
            .arg(serde_json::to_string(&args)?)
            .output()
            .await?;

        if !output.status.success() {
            return Err(anyhow::anyhow!(
                "Tool failed: {}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }

        Ok(serde_json::from_slice(&output.stdout)?)
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
