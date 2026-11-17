//! Tool execution for MCP tools.
//!
//! Runtime-agnostic design (ADR-0002): read-only tools are implemented
//! natively in Rust and everything else is delegated to upstream MCP
//! servers registered with the gateway. There is no agent-runtime CLI
//! fallback; the former firejail/bwrap/docker/gvisor wrappers around the
//! `opencode` CLI were removed (ADR-0002).

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::process::Command;

/// Maximum bytes returned by a native read/git handler.
const NATIVE_MAX_OUTPUT: usize = 1024 * 1024;

/// `web.fetch` のデフォルト/上限サイズと最大リダイレクト数。
const WEB_FETCH_DEFAULT_MAX_BYTES: usize = 256 * 1024;
const WEB_FETCH_HARD_MAX_BYTES: usize = 1024 * 1024;
const WEB_FETCH_MAX_REDIRECTS: usize = 3;

#[derive(Clone)]
pub struct Sandbox {
    base_dir: PathBuf,
}

impl Sandbox {
    pub async fn new(config: &crate::config::McpGatewayConfig) -> Result<Self> {
        let base_dir =
            std::env::var("JARVIS_MCP_GW_BASE_DIR").unwrap_or_else(|_| "/home/koppe".to_string());
        // `config.sandbox_config` is kept in the gateway config for
        // compatibility; the sandbox itself no longer shells out (ADR-0002).
        let _ = &config.sandbox_config;
        Ok(Self {
            base_dir: PathBuf::from(base_dir),
        })
    }

    /// Test/embedding constructor with an explicit workspace root.
    #[allow(dead_code)]
    fn with_base(base: PathBuf) -> Self {
        Self { base_dir: base }
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
            "filesystem.read" | "filesystem.list" | "git.status" | "git.diff" | "web.fetch"
        ) {
            return self.execute_native(tool, args, tool_config).await;
        }
        // Runtime-agnostic fallback (ADR-0002): there is no agent-runtime CLI
        // to shell out to. Unimplemented tools are delegated to upstream MCP
        // servers registered with the gateway, and error out clearly here.
        bail!("no native handler for tool '{tool}'; register an upstream MCP server for it")
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
            "web.fetch" => self.native_web_fetch(&args, tool_config).await,
            other => bail!(
                "no native handler for tool '{other}'; register an upstream MCP server for it"
            ),
        }
    }

    /// HTTP(S) GET with SSRF protections: public addresses only, manual
    /// redirect validation, textual content only, and a hard size cap.
    async fn native_web_fetch(
        &self,
        args: &Value,
        tool_config: &crate::allowlist::AllowedTool,
    ) -> Result<Value> {
        let raw = args
            .get("url")
            .and_then(Value::as_str)
            .context("missing 'url' argument")?;
        let mut url = validate_http_url(raw)?;
        let max_bytes = web_max_bytes(args);
        let timeout_ms = tool_config.timeout_ms.clamp(1_000, 15_000);
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .user_agent("jarvis-mcp-gw/web-fetch")
            .build()
            .context("build http client")?;
        let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);
        let mut redirects = 0usize;
        loop {
            ensure_public_host(&url).await?;
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                bail!("web.fetch timed out");
            }
            let resp = tokio::time::timeout(remaining, client.get(url.clone()).send())
                .await
                .context("web.fetch request timed out")?
                .with_context(|| format!("GET {url}"))?;
            let status = resp.status();
            if status.is_redirection()
                && let Some(location) = resp
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|value| value.to_str().ok())
            {
                redirects += 1;
                if redirects > WEB_FETCH_MAX_REDIRECTS {
                    bail!("too many redirects (> {WEB_FETCH_MAX_REDIRECTS})");
                }
                url = resolve_redirect(&url, location)?;
                continue;
            }
            let content_type = resp
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .unwrap_or("")
                .to_string();
            if !is_textual_content_type(&content_type) {
                bail!("unsupported content type: '{content_type}'");
            }
            if let Some(len) = resp.content_length()
                && len as usize > max_bytes
            {
                bail!("response too large ({len} bytes > {max_bytes})");
            }
            let mut resp = resp;
            let mut body: Vec<u8> = Vec::new();
            let mut truncated = false;
            loop {
                let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                if remaining.is_zero() {
                    bail!("web.fetch body timed out");
                }
                match tokio::time::timeout(remaining, resp.chunk()).await {
                    Err(_) => bail!("web.fetch body timed out"),
                    Ok(Err(err)) => return Err(err).context("read response body"),
                    Ok(Ok(None)) => break,
                    Ok(Ok(Some(chunk))) => {
                        if body.len() + chunk.len() > max_bytes {
                            let take = max_bytes.saturating_sub(body.len());
                            body.extend_from_slice(&chunk[..take]);
                            truncated = true;
                            break;
                        }
                        body.extend_from_slice(&chunk);
                    }
                }
            }
            let text = String::from_utf8_lossy(&body).to_string();
            return Ok(json!({
                "url": url.as_str(),
                "status": status.as_u16(),
                "content_type": content_type,
                "truncated": truncated,
                "body": text,
            }));
        }
    }

    fn native_read(&self, args: &Value) -> Result<Value> {
        let path = self.resolve_path(args)?;
        let meta = std::fs::metadata(&path).with_context(|| format!("stat {}", path.display()))?;
        if meta.is_dir() {
            bail!("path is a directory: {}", path.display());
        }
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
        let mut cmd = Command::new("git");
        cmd.arg("-C").arg(&path).args(git_args);
        // P0-5: do not leak service secrets (BWS_ACCESS_TOKEN) to children.
        crate::subprocess::apply_minimal_env(&mut cmd);
        let output = tokio::time::timeout(timeout, cmd.output())
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

    /// Reject paths that are known to carry credentials even though they may
    /// live beneath the configured base directory. This is defense in depth:
    /// `secret_scan` keeps such files out of the repo today, but a future
    /// accidental `.env` must not become readable through the MCP tool.
    fn reject_sensitive_path(path: &Path) -> Result<()> {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        let sensitive = name == ".env"
            || name.starts_with(".env.")
            || name.contains("bws.env")
            || name.contains("token")
            || name.contains("secret")
            || name.ends_with(".pem")
            || name.ends_with(".key");
        if sensitive {
            bail!("refusing to read sensitive file: {}", path.display());
        }
        Ok(())
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
        Self::reject_sensitive_path(&resolved)?;
        Ok(resolved)
    }
}

fn web_max_bytes(args: &Value) -> usize {
    let requested = args
        .get("max_bytes")
        .and_then(Value::as_u64)
        .unwrap_or(WEB_FETCH_DEFAULT_MAX_BYTES as u64) as usize;
    requested.clamp(1024, WEB_FETCH_HARD_MAX_BYTES)
}

fn validate_http_url(raw: &str) -> Result<reqwest::Url> {
    let url = reqwest::Url::parse(raw).with_context(|| format!("invalid url: {raw}"))?;
    match url.scheme() {
        "http" | "https" => {}
        other => bail!("unsupported scheme '{other}' (http/https only)"),
    }
    if !url.username().is_empty() || url.password().is_some() {
        bail!("credentials in URL are not allowed");
    }
    if url.host_str().is_none() {
        bail!("url has no host");
    }
    Ok(url)
}

fn resolve_redirect(base: &reqwest::Url, location: &str) -> Result<reqwest::Url> {
    let next = base
        .join(location)
        .with_context(|| format!("invalid redirect location: {location}"))?;
    validate_http_url(next.as_str())
}

fn is_public_ip(ip: &std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            !(v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_multicast()
                || v4.is_documentation()
                || v4.octets()[0] == 0
                || (v4.octets()[0] == 100 && (64..=127).contains(&v4.octets()[1])))
        }
        std::net::IpAddr::V6(v6) => {
            if let Some(mapped) = v6.to_ipv4_mapped() {
                return is_public_ip(&std::net::IpAddr::V4(mapped));
            }
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || v6.is_unique_local()
                || v6.is_unicast_link_local())
        }
    }
}

async fn ensure_public_host(url: &reqwest::Url) -> Result<()> {
    let host = url.host_str().context("url has no host")?;
    let port = url.port_or_known_default().unwrap_or(80);
    // Literal IPs (including bracketed IPv6) are checked directly without DNS.
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = bare.parse::<std::net::IpAddr>() {
        if !is_public_ip(&ip) {
            bail!("refusing to fetch non-public address {ip} for host {host}");
        }
        return Ok(());
    }
    let addrs = tokio::net::lookup_host((bare, port))
        .await
        .with_context(|| format!("dns lookup failed: {host}"))?;
    let mut any = false;
    for addr in addrs {
        any = true;
        if !is_public_ip(&addr.ip()) {
            bail!(
                "refusing to fetch non-public address {} for host {host}",
                addr.ip()
            );
        }
    }
    if !any {
        bail!("dns lookup returned no addresses for {host}");
    }
    Ok(())
}

fn is_textual_content_type(content_type: &str) -> bool {
    let ct = content_type.to_ascii_lowercase();
    ct.starts_with("text/")
        || ct.contains("json")
        || ct.contains("xml")
        || ct.contains("xhtml")
        || ct.contains("javascript")
        || ct.contains("ecmascript")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (PathBuf, Sandbox) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let base = std::env::temp_dir().join(format!("jarvis-sandbox-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("sub")).unwrap();
        std::fs::write(base.join("hello.txt"), b"hello sandbox").unwrap();
        let sandbox = Sandbox::with_base(base.clone());
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
    fn blocks_non_public_ip_ranges() {
        use std::net::IpAddr;
        for ip in [
            "127.0.0.1",
            "10.0.0.1",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.1.1",
            "0.0.0.0",
            "224.0.0.1",
            "100.64.0.1",
            "::1",
            "fc00::1",
            "fe80::1",
        ] {
            assert!(
                !is_public_ip(&ip.parse::<IpAddr>().unwrap()),
                "{ip} should be blocked"
            );
        }
        for ip in ["8.8.8.8", "1.1.1.1", "2001:4860:4860::8888"] {
            assert!(
                is_public_ip(&ip.parse::<IpAddr>().unwrap()),
                "{ip} should be allowed"
            );
        }
    }

    #[test]
    fn validates_fetch_url() {
        assert!(validate_http_url("https://example.com/path").is_ok());
        assert!(validate_http_url("http://example.com").is_ok());
        assert!(validate_http_url("file:///etc/passwd").is_err());
        assert!(validate_http_url("ftp://example.com").is_err());
        assert!(validate_http_url("https://user:pass@example.com").is_err());
        assert!(validate_http_url("not a url").is_err());
    }

    #[test]
    fn resolves_relative_redirects() {
        let base = validate_http_url("https://example.com/a/b").unwrap();
        let next = resolve_redirect(&base, "../c").unwrap();
        assert_eq!(next.as_str(), "https://example.com/c");
        let abs = resolve_redirect(&base, "https://other.example/d").unwrap();
        assert_eq!(abs.as_str(), "https://other.example/d");
    }

    /// 実ネットワーク検証（CIでは実行しない）: cargo test -p jarvis-mcp-gw --lib -- --ignored
    #[tokio::test]
    #[ignore = "requires public network"]
    async fn web_fetch_public_example_com() {
        let (base, sandbox) = fixture();
        let cfg = allowed("web.fetch", 10_000);
        let out = sandbox
            .execute(
                "web.fetch",
                json!({ "url": "https://example.com/", "max_bytes": 65536 }),
                &cfg,
            )
            .await
            .expect("public fetch should succeed");
        assert_eq!(out["status"], 200);
        assert!(
            out["body"]
                .as_str()
                .unwrap()
                .to_lowercase()
                .contains("example domain")
        );
        let _ = std::fs::remove_dir_all(base);
    }

    #[tokio::test]
    async fn web_fetch_rejects_loopback_targets() {
        let (base, sandbox) = fixture();
        let cfg = allowed("web.fetch", 2_000);
        for url in ["http://127.0.0.1:9/", "http://[::1]:9/"] {
            let err = sandbox
                .execute("web.fetch", json!({ "url": url }), &cfg)
                .await
                .expect_err("loopback URLs must be rejected");
            assert!(err.to_string().contains("non-public"), "{err}");
        }
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn caps_web_fetch_max_bytes() {
        assert_eq!(web_max_bytes(&json!({})), WEB_FETCH_DEFAULT_MAX_BYTES);
        assert_eq!(web_max_bytes(&json!({"max_bytes": 1})), 1024);
        assert_eq!(
            web_max_bytes(&json!({"max_bytes": 10 * 1024 * 1024})),
            WEB_FETCH_HARD_MAX_BYTES
        );
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

    #[test]
    fn sensitive_paths_are_refused() {
        let dir =
            std::env::temp_dir().join(format!("jarvis-mcp-gw-sensitive-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        for name in [".env", "bws.env", "api_token.txt", "server.key"] {
            let path = dir.join(name);
            std::fs::write(&path, b"x").unwrap();
            assert!(
                Sandbox::reject_sensitive_path(&path).is_err(),
                "{name} must be refused"
            );
        }
        let normal = dir.join("notes.txt");
        std::fs::write(&normal, b"x").unwrap();
        assert!(Sandbox::reject_sensitive_path(&normal).is_ok());
        let _ = std::fs::remove_dir_all(dir);
    }
}
