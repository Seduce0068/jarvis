//! Tool execution for MCP tools.
//!
//! Runtime-agnostic design (ADR-0002): read-only tools are implemented
//! natively in Rust and everything else is delegated to upstream MCP
//! servers registered with the gateway. There is no agent-runtime CLI
//! fallback; the former firejail/bwrap/docker/gvisor wrappers around the
//! `opencode` CLI were removed (ADR-0002).

use crate::error::McpGatewayError;
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::future::Future;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, ChildStderr, ChildStdout, Command};

/// Maximum bytes returned by a native read/git handler.
const NATIVE_MAX_OUTPUT: usize = 1024 * 1024;

/// `web.fetch` のデフォルト/上限サイズと最大リダイレクト数。
const WEB_FETCH_DEFAULT_MAX_BYTES: usize = 256 * 1024;
const WEB_FETCH_HARD_MAX_BYTES: usize = 1024 * 1024;
const WEB_FETCH_MAX_REDIRECTS: usize = 3;

/// Caller-input failures are typed as `McpGatewayError::ClientError` so the
/// transport adapters keep mapping them to HTTP 400. `ClientError` renders as
/// its message verbatim, so existing error-text expectations stay unchanged.
fn client_error(message: impl Into<String>) -> anyhow::Error {
    McpGatewayError::ClientError(message.into()).into()
}

/// Attach context to a caller-input failure without losing the typed 400
/// mapping. The context text becomes the complete, unchanged error message.
fn client_context<T, E>(result: std::result::Result<T, E>, context: String) -> Result<T>
where
    E: std::error::Error + Send + Sync + 'static,
{
    result.map_err(|err| client_context_error(err, context))
}

/// Same as [`client_context`] for an error that is already unwrapped. Using
/// `anyhow`'s context renderer keeps the exact message the former bare
/// `.context(..)` produced while stamping the error as a `ClientError`.
fn client_context_error<E>(error: E, context: impl Into<String>) -> anyhow::Error
where
    E: std::error::Error + Send + Sync + 'static,
{
    client_error(
        anyhow::Error::new(error)
            .context(context.into())
            .to_string(),
    )
}

/// Read a child pipe incrementally and keep at most `cap` bytes in memory.
///
/// (c) The old code used `Child::output()`, which buffers the *entire* git
/// stdout/stderr before `NATIVE_MAX_OUTPUT` was checked, so a huge diff could
/// exhaust memory before the cap was ever enforced. Reading in fixed-size
/// chunks lets us stop buffering at the cap. We keep draining (discarding)
/// the remainder up to EOF so the child never blocks on a full pipe and can
/// still exit cleanly; `truncated` records that the cap was hit.
async fn read_output_capped<R: AsyncRead + Unpin>(
    reader: &mut R,
    cap: usize,
) -> std::io::Result<(Vec<u8>, bool)> {
    let mut chunk = [0u8; 8 * 1024];
    let mut out = Vec::new();
    let mut truncated = false;
    loop {
        let n = reader.read(&mut chunk).await?;
        if n == 0 {
            return Ok((out, truncated));
        }
        if truncated {
            continue;
        }
        if out.len() + n > cap {
            let take = cap.saturating_sub(out.len());
            out.extend_from_slice(&chunk[..take]);
            truncated = true;
        } else {
            out.extend_from_slice(&chunk[..n]);
        }
    }
}

/// Collect a spawned child's output with a hard memory cap and a wall-clock
/// deadline.
///
/// (b) tokio 1.x does not kill a child when its `Child` handle is dropped, so
/// simply letting `tokio::time::timeout` drop the collection future would
/// orphan the process and leak its stdio fds. On timeout we explicitly
/// `start_kill()` *and* `wait()`: that kills the process and reaps it. The
/// command also sets `kill_on_drop(true)` as a backstop for any other early
/// return while the child is still alive.
async fn collect_child_output(
    child: &mut Child,
    stdout: &mut ChildStdout,
    stderr: &mut ChildStderr,
    timeout: Duration,
    cap: usize,
) -> Result<(Vec<u8>, Vec<u8>, std::process::ExitStatus, bool)> {
    let collect = async {
        // Both pipes are read concurrently: a git invocation that fills its
        // stderr pipe while we drain stdout would otherwise deadlock.
        let (stdout, stderr) = tokio::join!(
            read_output_capped(stdout, cap),
            read_output_capped(stderr, cap),
        );
        let (stdout, truncated) = stdout?;
        let (stderr, _) = stderr?;
        let status = child.wait().await?;
        Ok::<_, std::io::Error>((stdout, stderr, status, truncated))
    };

    // Bind first so the `collect` future (which borrows `child`) is dropped
    // before the timeout branch uses `child` to kill and reap.
    let result = tokio::time::timeout(timeout, collect).await;
    match result {
        Ok(result) => result.context("wait for git failed"),
        Err(_) => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            Err(client_error("git timed out"))
        }
    }
}

#[derive(Clone)]
pub struct Sandbox {
    base_dir: PathBuf,
    /// Hosts enforced on every `web.fetch` hop, derived from the gateway
    /// config (TOML key or env override). An unset/invalid value becomes
    /// `DenyAll`, so no configuration path can silently restore unlimited
    /// egress.
    egress_allowlist: EgressAllowlist,
}

impl Sandbox {
    pub async fn new(config: &crate::config::McpGatewayConfig) -> Result<Self> {
        let base_dir =
            std::env::var("JARVIS_MCP_GW_BASE_DIR").unwrap_or_else(|_| "/home/koppe".to_string());
        // `config.sandbox_config` is kept in the gateway config for
        // compatibility; the sandbox itself no longer shells out (ADR-0002).
        let _ = &config.sandbox_config;
        let egress_allowlist = EgressAllowlist::from_config(&config.egress_allowlist);
        Ok(Self {
            base_dir: PathBuf::from(base_dir),
            egress_allowlist,
        })
    }

    /// Test/embedding constructor with an explicit workspace root. The
    /// `Unrestricted` value is a deliberate test-only bypass so direct
    /// handler tests need no config; production always goes through
    /// [`Self::new`] with fail-closed config parsing.
    #[allow(dead_code)]
    fn with_base(base: PathBuf) -> Self {
        Self {
            base_dir: base,
            egress_allowlist: EgressAllowlist::Unrestricted,
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
            "filesystem.read" | "filesystem.list" | "git.status" | "git.diff" | "web.fetch"
        ) {
            return self.execute_native(tool, args, tool_config).await;
        }
        // Runtime-agnostic fallback (ADR-0002): there is no agent-runtime CLI
        // to shell out to. Unimplemented tools are delegated to upstream MCP
        // servers registered with the gateway, and error out clearly here.
        // The tool name came from the caller, so this is a 400-class mistake.
        Err(client_error(format!(
            "no native handler for tool '{tool}'; register an upstream MCP server for it"
        )))
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
            other => Err(client_error(format!(
                "no native handler for tool '{other}'; register an upstream MCP server for it"
            ))),
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
            .ok_or_else(|| client_error("missing 'url' argument"))?;
        let mut url = validate_http_url(raw)?;
        let allowlist = &self.egress_allowlist;
        let max_bytes = web_max_bytes(args);
        let timeout_ms = tool_config.timeout_ms.clamp(1_000, 15_000);
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .user_agent("jarvis-mcp-gw/web-fetch")
            // A proxy would re-resolve the host itself and bypass the IP
            // pinning below, so `web.fetch` always connects directly.
            .no_proxy()
            .build()
            .context("build http client")?;
        let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);
        let mut redirects = 0usize;
        loop {
            let dest = ensure_public_host(&url).await?;
            // Applied on every hop, so a redirect cannot escape the allowlist.
            ensure_host_allowed(&url, allowlist)?;
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err(client_error("web.fetch timed out"));
            }
            // Pin the TCP connection to the addresses validated above. Without
            // this, reqwest resolves the DNS name a second time and a rebinding
            // attacker can swap in a private IP after the check (TOCTOU).
            // `resolve_to_addrs` keeps the hostname in the URI, so the Host
            // header and HTTPS SNI / certificate verification stay bound to the
            // real host; rewriting the URL host to an IP would break TLS.
            // IP literals need no pinning (reqwest never re-resolves them).
            let hop_client = fetch_client_for_dest(&dest, &client)?;
            let resp = client_context(
                tokio::time::timeout(remaining, hop_client.get(url.clone()).send()).await,
                "web.fetch request timed out".to_string(),
            )?;
            let resp = client_context(resp, format!("GET {url}"))?;
            let status = resp.status();
            // A redirect must always carry a usable Location. Letting a 3xx
            // without one fall through would return the intermediate body as
            // though it were the fetched content (SSRF fail-open), so abort
            // explicitly instead of continuing with the normal response path.
            if status.is_redirection() {
                let location = resp
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|value| value.to_str().ok());
                let next = redirect_target(&url, location)?;
                redirects += 1;
                if redirects > WEB_FETCH_MAX_REDIRECTS {
                    return Err(client_error(format!(
                        "too many redirects (> {WEB_FETCH_MAX_REDIRECTS})"
                    )));
                }
                url = next;
                continue;
            }
            let content_type = resp
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .unwrap_or("")
                .to_string();
            if !is_textual_content_type(&content_type) {
                let message = format!("unsupported content type: '{content_type}'");
                return Err(client_error(message));
            }
            if let Some(len) = resp.content_length()
                && len as usize > max_bytes
            {
                let message = format!("response too large ({len} bytes > {max_bytes})");
                return Err(client_error(message));
            }
            let mut resp = resp;
            let mut body: Vec<u8> = Vec::new();
            let mut truncated = false;
            loop {
                let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                if remaining.is_zero() {
                    return Err(client_error("web.fetch body timed out"));
                }
                match tokio::time::timeout(remaining, resp.chunk()).await {
                    Err(_) => return Err(client_error("web.fetch body timed out")),
                    Ok(Err(err)) => return Err(client_context_error(err, "read response body")),
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
        let path_display = path.display().to_string();
        let meta = client_context(std::fs::metadata(&path), format!("stat {path_display}"))?;
        if meta.is_dir() {
            let message = format!("path is a directory: {path_display}");
            return Err(client_error(message));
        }
        if meta.len() > NATIVE_MAX_OUTPUT as u64 {
            let message = format!("file too large to read ({} bytes)", meta.len());
            return Err(client_error(message));
        }
        let content = client_context(
            std::fs::read_to_string(&path),
            format!("read {path_display}"),
        )?;
        Ok(json!({"path": path_display, "content": content}))
    }

    fn native_list(&self, args: &Value) -> Result<Value> {
        let path = self.resolve_path(args)?;
        let path_display = path.display().to_string();
        let mut entries = Vec::new();
        let dir = client_context(std::fs::read_dir(&path), format!("read_dir {path_display}"))?;
        for entry in dir {
            let entry = entry.map_err(|err| client_error(err.to_string()))?;
            let file_type = entry
                .file_type()
                .map_err(|err| client_error(err.to_string()))?;
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
        Ok(json!({"path": path_display, "entries": entries}))
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
        // (b) A dropped `Child` does not terminate its process in tokio 1.x.
        // Kill-on-drop is a backstop for early returns; the timeout path also
        // kills and reaps explicitly (see `collect_child_output`).
        cmd.kill_on_drop(true);
        let mut child = cmd
            // `Command::output()` used to set stdin to null; keep that so git
            // never blocks reading a caller's terminal.
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .with_context(|| format!("spawn git {:?}", git_args))?;
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("git stdout pipe missing"))?;
        let mut stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow::anyhow!("git stderr pipe missing"))?;

        // (c) Stream and cap both pipes instead of buffering the whole output
        // before checking NATIVE_MAX_OUTPUT. A timeout is a caller-input
        // failure (client error); a spawn/wait failure stays internal.
        let (stdout, stderr, status, truncated) = collect_child_output(
            &mut child,
            &mut stdout,
            &mut stderr,
            timeout,
            NATIVE_MAX_OUTPUT,
        )
        .await?;

        if !status.success() {
            return Err(client_error(format!(
                "git {:?} failed: {}",
                git_args,
                String::from_utf8_lossy(&stderr)
            )));
        }
        if truncated || stdout.len() > NATIVE_MAX_OUTPUT {
            return Err(client_error(format!(
                "git output too large ({} bytes)",
                stdout.len()
            )));
        }
        Ok(json!({
            "path": path.display().to_string(),
            "stdout": String::from_utf8_lossy(&stdout),
        }))
    }

    /// Reject paths that are known to carry credentials even though they may
    /// live beneath the configured base directory. Every component of the
    /// canonicalized path is inspected, not just `file_name()`, so secrets
    /// reached through credential directories (`.ssh/id_rsa`,
    /// `.aws/credentials`, `.config/gcloud/...`, ...) are refused too. This is
    /// defense in depth: `secret_scan` keeps such files out of the repo today,
    /// but a future accidental `.env` must not become readable via the MCP tool.
    fn reject_sensitive_path(path: &Path) -> Result<()> {
        /// Directory component names whose entire subtree holds credentials.
        const SENSITIVE_DIRS: &[&str] = &[".ssh", ".aws", ".gnupg", ".kube"];

        // Inspect *every* component. `to_str()` skipped any component that was
        // not valid UTF-8, so a name like `token\xff` bypassed the sensitive
        // check entirely. `to_string_lossy()` keeps the valid ASCII portion
        // (including `token`, `secret`, ...) and only replaces invalid bytes
        // with U+FFFD, so those names are still matched and refused.
        let components: Vec<String> = path
            .components()
            .map(|c| c.as_os_str().to_string_lossy().to_ascii_lowercase())
            .collect();

        for (idx, component) in components.iter().enumerate() {
            // `.config/gcloud` is sensitive only as an adjacent pair; a bare
            // `gcloud` segment elsewhere is not treated as a credential dir.
            let is_gcloud_config = component == ".config"
                && components.get(idx + 1).map(String::as_str) == Some("gcloud");
            let is_sensitive_dir = SENSITIVE_DIRS.contains(&component.as_str());
            let is_sensitive_name = component == ".env"
                || component.starts_with(".env.")
                || component.contains("bws.env")
                || component.contains("token")
                || component.contains("secret")
                || component.starts_with("id_rsa")
                || component.starts_with("id_ed25519")
                || component.starts_with("id_ecdsa")
                || component.starts_with("id_dsa")
                || component.contains("known_hosts")
                || component.contains("credentials")
                || component.ends_with(".pem")
                || component.ends_with(".key");
            if is_gcloud_config || is_sensitive_dir || is_sensitive_name {
                let message = format!("refusing to read sensitive file: {}", path.display());
                return Err(client_error(message));
            }
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
        // A caller-supplied path that cannot be canonicalized (e.g. it does
        // not exist) is invalid input; the base directory itself is config.
        let resolved = client_context(
            std::fs::canonicalize(&joined),
            format!("canonicalize {}", joined.display()),
        )?;
        if !resolved.starts_with(&base) {
            let message = format!("path escapes workspace: {}", resolved.display());
            return Err(client_error(message));
        }
        Self::reject_sensitive_path(&resolved)?;
        Ok(resolved)
    }
}

fn web_max_bytes(args: &Value) -> usize {
    let requested = args
        .get("max_bytes")
        .and_then(Value::as_u64)
        .unwrap_or(WEB_FETCH_DEFAULT_MAX_BYTES as u64);
    // Clamp in u64 space first so a huge value cannot wrap when cast to
    // usize on narrower targets, then convert to the typed constant.
    let clamped = requested.clamp(1024, WEB_FETCH_HARD_MAX_BYTES as u64);
    usize::try_from(clamped).unwrap_or(WEB_FETCH_HARD_MAX_BYTES)
}

/// Destination host allowlist for `web.fetch`.
///
/// Production values come from
/// [`crate::config::McpGatewayConfig::egress_allowlist`] (TOML key or the
/// `JARVIS_MCP_GW_EGRESS_ALLOWLIST` env override). An unset, blank or invalid
/// value becomes `DenyAll`: every fetch is refused instead of silently
/// reverting to unrestricted egress.
#[derive(Clone, Debug)]
enum EgressAllowlist {
    /// Explicit test-only bypass used by [`Sandbox::with_base`]. The config
    /// path never derives this variant from an empty value.
    Unrestricted,
    /// No valid host patterns: all destinations are refused.
    DenyAll,
    Hosts(Vec<String>),
}

impl EgressAllowlist {
    /// Build from the config field. Parsing and validation live in
    /// [`crate::config::parse_egress_allowlist`] so TOML and env share one
    /// fail-closed representation.
    fn from_config(raw: &str) -> Self {
        match crate::config::parse_egress_allowlist(raw) {
            Some(hosts) => Self::Hosts(hosts),
            None => Self::DenyAll,
        }
    }

    fn allows(&self, host: &str) -> bool {
        match self {
            Self::Unrestricted => true,
            Self::DenyAll => false,
            Self::Hosts(entries) => {
                let host = host.trim().to_ascii_lowercase();
                entries.iter().any(|entry| match entry.strip_prefix("*.") {
                    // `*.example.com` covers `a.example.com` and deeper
                    // subdomains, but not the apex or `notexample.com`.
                    Some(base) => host
                        .strip_suffix(base)
                        .is_some_and(|prefix| prefix.ends_with('.')),
                    None => host == *entry,
                })
            }
        }
    }
}

/// Enforce the egress allowlist on the URL host, in addition to the existing
/// public-IP/SSRF checks. Config-sourced empty/invalid values are `DenyAll`,
/// so only the explicit test-only `Unrestricted` variant is a no-op.
fn ensure_host_allowed(url: &reqwest::Url, allowlist: &EgressAllowlist) -> Result<()> {
    let host = url
        .host_str()
        .ok_or_else(|| client_error("url has no host"))?;
    if matches!(allowlist, EgressAllowlist::Unrestricted) {
        return Ok(());
    }
    if matches!(allowlist, EgressAllowlist::DenyAll) {
        return Err(client_error(format!(
            "host '{host}' is denied: egress allowlist is empty or invalid"
        )));
    }
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    // DNS names only: an IP literal cannot be named in the allowlist, so
    // reject it outright rather than trying to match it as a hostname.
    if bare.parse::<std::net::IpAddr>().is_ok() {
        let message =
            format!("host '{host}' is an IP literal and is not allowed by the egress allowlist");
        return Err(client_error(message));
    }
    if !allowlist.allows(bare) {
        let message = format!("host '{host}' is not in the egress allowlist");
        return Err(client_error(message));
    }
    Ok(())
}

fn validate_http_url(raw: &str) -> Result<reqwest::Url> {
    let url = client_context(reqwest::Url::parse(raw), format!("invalid url: {raw}"))?;
    match url.scheme() {
        "http" | "https" => {}
        other => {
            let message = format!("unsupported scheme '{other}' (http/https only)");
            return Err(client_error(message));
        }
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(client_error("credentials in URL are not allowed"));
    }
    if url.host_str().is_none() {
        return Err(client_error("url has no host"));
    }
    Ok(url)
}

/// Resolve a redirect header into a target URL. `None` means the header was
/// absent or not valid UTF-8; both are hard errors so a 3xx can never be
/// mistaken for a successful fetch.
fn redirect_target(base: &reqwest::Url, location: Option<&str>) -> Result<reqwest::Url> {
    let Some(location) = location else {
        return Err(client_error("redirect response has no Location header"));
    };
    resolve_redirect(base, location)
}

fn resolve_redirect(base: &reqwest::Url, location: &str) -> Result<reqwest::Url> {
    // An empty (or whitespace-only) Location would resolve back to the base
    // URL and spin until the redirect cap, or silently reuse the current hop.
    if location.trim().is_empty() {
        return Err(client_error(
            "redirect response has an empty Location header",
        ));
    }
    let next = client_context(
        base.join(location),
        format!("invalid redirect location: {location}"),
    )?;
    validate_http_url(next.as_str())
}

/// Public = globally routable according to the standard library's IANA-based
/// classification. Replacing the hand-written denylist with `is_global()`
/// closes the SSRF gap for IETF special-purpose ranges (192.0.0.0/24,
/// 192.88.99.0/24, 198.18.0.0/15, 240.0.0.0/4, 100.64.0.0/10, ...), which the
/// old list treated as public and that DNS resolution could return.
fn is_public_ip(ip: &std::net::IpAddr) -> bool {
    use std::net::IpAddr;

    // `IpAddr::is_global()` is still unstable on this toolchain, so mirror
    // its IANA/IETF classification explicitly. This closes the SSRF gap for
    // special-purpose ranges the old hand-written list treated as public.
    fn v4_public(o: [u8; 4]) -> bool {
        let (a, b, c, _) = (o[0], o[1], o[2], o[3]);
        if a == 0 || a == 10 || a == 127 || a >= 224 {
            return false;
        }
        if a == 169 && b == 254 {
            return false;
        }
        if a == 172 && (16..=31).contains(&b) {
            return false;
        }
        if a == 192 && b == 168 {
            return false;
        }
        if a == 100 && (64..=127).contains(&b) {
            return false;
        }
        if a == 192 && b == 0 && c == 0 {
            return false;
        }
        if a == 192 && b == 0 && c == 2 {
            return false;
        }
        if a == 192 && b == 88 && c == 99 {
            return false;
        }
        if a == 198 && (b == 18 || b == 19) {
            return false;
        }
        if a == 203 && b == 0 && c == 113 {
            return false;
        }
        true
    }

    match ip {
        IpAddr::V4(v4) => v4_public(v4.octets()),
        IpAddr::V6(v6) => {
            if let Some(mapped) = v6.to_ipv4_mapped() {
                return v4_public(mapped.octets());
            }
            let s = v6.segments();
            if v6.is_unspecified() || v6.is_loopback() {
                return false;
            }
            if s[0] & 0xfe00 == 0xfc00 {
                return false;
            } // unique local
            if s[0] & 0xffc0 == 0xfe80 {
                return false;
            } // link local
            if s[0] == 0x2001 && s[1] == 0x0db8 {
                return false;
            } // documentation
            if s[0] == 0 && s[1] == 0 && s[2] == 0 && s[3] == 0 && s[4] == 0 && s[5] == 0xffff {
                return false;
            }
            true
        }
    }
}

/// Validated destination of one `web.fetch` hop. Resolver results are carried
/// forward so the real connection can be pinned to exactly the addresses that
/// passed the public-IP check, closing the DNS-rebinding / TOCTOU window
/// between validation and reqwest's own name resolution.
#[derive(Clone, Debug)]
pub(crate) enum ValidatedDest {
    /// The URL host is an IP literal: reqwest connects to it directly and
    /// never re-resolves it, so the checked literal is already the destination.
    Literal,
    /// A DNS name. `host` is the authority reqwest will resolve (kept in the
    /// URI for a correct Host header and HTTPS SNI / certificate validation);
    /// `addrs` are the validated public addresses the connection is limited to.
    Resolved {
        host: String,
        addrs: Vec<std::net::SocketAddr>,
    },
}

/// DNS resolution seam for [`ensure_public_host_with`]. A plain function
/// pointer (rather than a generic parameter) keeps call sites monomorphic and
/// lets tests inject a deterministic resolver without any network access. The
/// higher-ranked lifetime ties the returned future to the borrowed host.
pub(crate) type DnsResolver =
    for<'a> fn(
        &'a str,
        u16,
    ) -> Pin<Box<dyn Future<Output = std::io::Result<Vec<SocketAddr>>> + Send + 'a>>;

/// Production resolver: thin adapter over `tokio::net::lookup_host` that
/// collects the iterator into the `Vec<SocketAddr>` the seam returns.
fn lookup_host_resolver<'a>(
    host: &'a str,
    port: u16,
) -> Pin<Box<dyn Future<Output = std::io::Result<Vec<SocketAddr>>> + Send + 'a>> {
    Box::pin(async move {
        let resolved = tokio::net::lookup_host((host, port)).await?;
        Ok(resolved.collect())
    })
}

/// Validation core with an injectable DNS resolver. All fail-closed checks and
/// error strings are identical to the former inline implementation.
pub(crate) async fn ensure_public_host_with(
    url: &reqwest::Url,
    resolver: DnsResolver,
) -> Result<ValidatedDest> {
    let host = url
        .host_str()
        .ok_or_else(|| client_error("url has no host"))?;
    let port = url.port_or_known_default().unwrap_or(80);
    // Literal IPs (including bracketed IPv6) are checked directly without DNS
    // and need no pinning: reqwest never re-resolves an IP literal.
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = bare.parse::<std::net::IpAddr>() {
        if !is_public_ip(&ip) {
            let message = format!("refusing to fetch non-public address {ip} for host {host}");
            return Err(client_error(message));
        }
        return Ok(ValidatedDest::Literal);
    }
    // The host came from caller input, so resolution failures are client
    // errors (400), not gateway-internal failures (500).
    let addrs = client_context(
        resolver(bare, port).await,
        format!("dns lookup failed: {host}"),
    )?;
    if addrs.is_empty() {
        let message = format!("dns lookup returned no addresses for {host}");
        return Err(client_error(message));
    }
    for addr in &addrs {
        if !is_public_ip(&addr.ip()) {
            let message = format!(
                "refusing to fetch non-public address {} for host {host}",
                addr.ip()
            );
            return Err(client_error(message));
        }
    }
    Ok(ValidatedDest::Resolved {
        host: host.to_string(),
        addrs,
    })
}

/// Unchanged production entry point: delegates to the testable core using the
/// real `tokio::net::lookup_host` resolver.
async fn ensure_public_host(url: &reqwest::Url) -> Result<ValidatedDest> {
    ensure_public_host_with(url, lookup_host_resolver).await
}

/// Build the per-hop HTTP client. DNS-named hosts are pinned to the exact
/// addresses returned by `ensure_public_host` through reqwest's DNS override,
/// so no second resolution can redirect the connection to a different IP. The
/// hostname stays in the URI (only its resolution is overridden), which keeps
/// the Host header and HTTPS SNI / certificate validation bound to the real
/// host. Proxies are disabled for the same reason a proxy would re-resolve the
/// host and bypass the pin. IP literals reuse the base client unchanged.
fn fetch_client_for_dest(dest: &ValidatedDest, base: &reqwest::Client) -> Result<reqwest::Client> {
    match dest {
        ValidatedDest::Literal => Ok(base.clone()),
        ValidatedDest::Resolved { host, addrs } => reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .user_agent("jarvis-mcp-gw/web-fetch")
            .no_proxy()
            .resolve_to_addrs(host, addrs)
            .build()
            .context("build pinned http client"),
    }
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
            // IETF reserved / special-purpose ranges: `is_global()` denies
            // these even though the old hand-written denylist allowed them.
            "192.0.0.1",
            "192.88.99.1",
            "198.18.0.1",
            "240.0.0.1",
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

    /// (a) The IPv4-mapped form must inherit the same `is_global()` verdict as
    /// the native IPv4 address, otherwise a mapped reserved address slips
    /// through the DNS path of `ensure_public_host`.
    #[test]
    fn blocks_ipv4_mapped_non_public_ranges() {
        use std::net::IpAddr;
        for ip in [
            "::ffff:127.0.0.1",
            "::ffff:192.0.0.1",
            "::ffff:192.88.99.1",
            "::ffff:198.18.0.1",
            "::ffff:240.0.0.1",
        ] {
            assert!(
                !is_public_ip(&ip.parse::<IpAddr>().unwrap()),
                "mapped {ip} should be blocked"
            );
        }
        let public_mapped = "::ffff:8.8.8.8".parse::<IpAddr>().unwrap();
        assert!(is_public_ip(&public_mapped));
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

    #[tokio::test]
    async fn validated_dest_for_ip_literals_skips_dns() {
        // IP literals are validated locally (no DNS) and classified as
        // `Literal`, so no pinning / second resolution is involved.
        for public in ["https://8.8.8.8/", "https://[2001:4860:4860::8888]/"] {
            let url = validate_http_url(public).unwrap();
            assert!(
                matches!(
                    ensure_public_host(&url).await.unwrap(),
                    ValidatedDest::Literal
                ),
                "{public} should be a literal destination"
            );
        }
        for private in ["https://127.0.0.1/", "https://[::1]/", "https://10.0.0.1/"] {
            let url = validate_http_url(private).unwrap();
            let err = ensure_public_host(&url).await.unwrap_err();
            assert!(err.to_string().contains("non-public"), "{err}");
        }
    }

    // Deterministic resolvers injected through the `ensure_public_host_with`
    // seam. They are plain `fn` items so they coerce to the higher-ranked
    // `DnsResolver` pointer type; no network is ever consulted.
    fn failing_resolver<'a>(
        _host: &'a str,
        _port: u16,
    ) -> Pin<Box<dyn Future<Output = std::io::Result<Vec<SocketAddr>>> + Send + 'a>> {
        Box::pin(async {
            Err::<Vec<SocketAddr>, _>(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "synthetic dns failure",
            ))
        })
    }

    fn empty_resolver<'a>(
        _host: &'a str,
        _port: u16,
    ) -> Pin<Box<dyn Future<Output = std::io::Result<Vec<SocketAddr>>> + Send + 'a>> {
        Box::pin(async { Ok(Vec::new()) })
    }

    fn mixed_resolver<'a>(
        _host: &'a str,
        _port: u16,
    ) -> Pin<Box<dyn Future<Output = std::io::Result<Vec<SocketAddr>>> + Send + 'a>> {
        Box::pin(async {
            Ok(vec![
                "93.184.216.34:443".parse().unwrap(),
                "127.0.0.1:443".parse().unwrap(),
                "10.0.0.1:443".parse().unwrap(),
            ])
        })
    }

    fn public_resolver<'a>(
        _host: &'a str,
        _port: u16,
    ) -> Pin<Box<dyn Future<Output = std::io::Result<Vec<SocketAddr>>> + Send + 'a>> {
        Box::pin(async {
            Ok(vec![
                "93.184.216.34:443".parse().unwrap(),
                "[2606:2800:220:1:248:1893:25c8:1946]:443".parse().unwrap(),
            ])
        })
    }

    fn never_call_resolver<'a>(
        _host: &'a str,
        _port: u16,
    ) -> Pin<Box<dyn Future<Output = std::io::Result<Vec<SocketAddr>>> + Send + 'a>> {
        Box::pin(async {
            Err::<Vec<SocketAddr>, _>(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "IP literal host must not reach the DNS resolver",
            ))
        })
    }

    /// (a) A resolver error is mapped to the same typed client error (HTTP
    /// 400) as a real `lookup_host` failure, keeping the error vocabulary.
    #[tokio::test]
    async fn ensure_public_host_dns_error_is_client_error() {
        let url = validate_http_url("https://dns-failure.example/").unwrap();
        let err = ensure_public_host_with(&url, failing_resolver)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("dns lookup failed"), "{err}");
        assert_client_error(&err);
    }

    /// (b) An empty resolution must fail closed rather than produce an empty
    /// pinning set that would let reqwest resolve the host on its own.
    #[tokio::test]
    async fn ensure_public_host_empty_dns_result_is_client_error() {
        let url = validate_http_url("https://no-addresses.example/").unwrap();
        let err = ensure_public_host_with(&url, empty_resolver)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("dns lookup returned no addresses"),
            "{err}"
        );
        assert_client_error(&err);
    }

    /// (c) One private address among public ones must still reject the whole
    /// set: every pinned address is checked before any connection is allowed.
    #[tokio::test]
    async fn ensure_public_host_rejects_mixed_public_and_private_addresses() {
        let url = validate_http_url("https://mixed.example/").unwrap();
        let err = ensure_public_host_with(&url, mixed_resolver)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("non-public"), "{err}");
        assert_client_error(&err);
    }

    /// (d) All-public resolutions are preserved verbatim for pinning, and an
    /// IP literal never reaches the resolver (no redundant DNS / no TOCTOU).
    #[tokio::test]
    async fn ensure_public_host_pins_all_public_addresses() {
        let url = validate_http_url("https://public.example/").unwrap();
        let dest = ensure_public_host_with(&url, public_resolver)
            .await
            .unwrap();
        let expected = vec![
            "93.184.216.34:443".parse().unwrap(),
            "[2606:2800:220:1:248:1893:25c8:1946]:443".parse().unwrap(),
        ];
        match dest {
            ValidatedDest::Resolved { host, addrs } => {
                assert_eq!(host, "public.example");
                assert_eq!(addrs, expected);
            }
            other => panic!("expected a pinned resolved destination, got {other:?}"),
        }

        let literal = validate_http_url("https://8.8.8.8/").unwrap();
        assert!(matches!(
            ensure_public_host_with(&literal, never_call_resolver)
                .await
                .unwrap(),
            ValidatedDest::Literal
        ));
    }

    #[test]
    fn pinned_client_builds_for_resolved_dest() {
        let base = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let resolved = ValidatedDest::Resolved {
            host: "example.com".to_string(),
            addrs: vec!["93.184.216.34:443".parse().unwrap()],
        };
        // Building the pinned client must succeed without a network call; the
        // DNS override leaves the URI hostname intact for SNI/cert checks.
        assert!(fetch_client_for_dest(&resolved, &base).is_ok());
        assert!(fetch_client_for_dest(&ValidatedDest::Literal, &base).is_ok());
    }

    #[test]
    fn resolves_relative_redirects() {
        let base = validate_http_url("https://example.com/a/b").unwrap();
        let next = resolve_redirect(&base, "../c").unwrap();
        assert_eq!(next.as_str(), "https://example.com/c");
        let abs = resolve_redirect(&base, "https://other.example/d").unwrap();
        assert_eq!(abs.as_str(), "https://other.example/d");
    }

    #[test]
    fn redirect_without_usable_location_fails_closed() {
        let base = validate_http_url("https://example.com/a/b").unwrap();
        // A missing header (or a non-UTF-8 value) surfaces as `None` and must
        // abort instead of falling through to the normal 3xx response path.
        let err = redirect_target(&base, None).unwrap_err();
        assert!(err.to_string().contains("no Location"), "{err}");
        // Empty and whitespace-only values are equally unusable.
        for empty in ["", "   ", "\t"] {
            let err = redirect_target(&base, Some(empty)).unwrap_err();
            assert!(err.to_string().contains("empty Location"), "{err}");
        }
        // A valid relative Location still resolves exactly as before.
        let ok = redirect_target(&base, Some("../c")).unwrap();
        assert_eq!(ok.as_str(), "https://example.com/c");
    }

    #[test]
    fn redirect_with_unparseable_location_is_rejected() {
        let base = validate_http_url("https://example.com/a/b").unwrap();
        for bad in ["http://[invalid", "http://", "file:///etc/passwd"] {
            assert!(
                resolve_redirect(&base, bad).is_err(),
                "{bad} must be rejected as a redirect target"
            );
        }
    }

    #[test]
    fn egress_allowlist_parse_and_match() {
        // The production config path is fail-closed: unset/blank, whitespace,
        // empty entries and malformed patterns all become `DenyAll`, so no
        // destination is allowed.
        for raw in ["", "   ", ",", "http://example.com", "bad host", "*."] {
            let list = EgressAllowlist::from_config(raw);
            assert!(!list.allows("anything.example"), "{raw}");
        }

        let list = EgressAllowlist::from_config(" example.com , *.trusted.org ");
        // Exact match, case-insensitive on both sides, without suffix confusion.
        assert!(list.allows("example.com"));
        assert!(list.allows("EXAMPLE.COM"));
        assert!(!list.allows("notexample.com"));
        // `*.` covers subdomains (including nested) but not the apex.
        assert!(list.allows("api.trusted.org"));
        assert!(list.allows("a.b.trusted.org"));
        assert!(!list.allows("trusted.org"));
        assert!(!list.allows("nottrusted.org"));
        // Anything outside the list is refused.
        assert!(!list.allows("attacker.example"));
    }

    #[test]
    fn egress_allowlist_allows_and_rejects_urls() {
        let list = EgressAllowlist::from_config("example.com,*.trusted.org");
        let sub = validate_http_url("https://api.trusted.org/resource").unwrap();
        assert!(ensure_host_allowed(&sub, &list).is_ok());
        let exact = validate_http_url("https://example.com/").unwrap();
        assert!(ensure_host_allowed(&exact, &list).is_ok());

        let denied = validate_http_url("https://attacker.example/").unwrap();
        let err = ensure_host_allowed(&denied, &list).unwrap_err();
        assert!(err.to_string().contains("egress allowlist"), "{err}");

        // IP literals are refused even when the address itself is public.
        let v4 = validate_http_url("https://8.8.8.8/").unwrap();
        let err = ensure_host_allowed(&v4, &list).unwrap_err();
        assert!(err.to_string().contains("IP literal"), "{err}");
        let v6 = validate_http_url("https://[2001:4860:4860::8888]/").unwrap();
        assert!(ensure_host_allowed(&v6, &list).is_err());

        // An empty config is `DenyAll`, not a bypass: the same destinations
        // are refused instead of being silently allowed.
        let deny_all = EgressAllowlist::from_config("");
        assert!(ensure_host_allowed(&denied, &deny_all).is_err());
        assert!(ensure_host_allowed(&exact, &deny_all).is_err());
        assert!(ensure_host_allowed(&v4, &deny_all).is_err());
    }

    #[test]
    fn redirect_target_is_checked_against_allowlist() {
        let list = EgressAllowlist::from_config("example.com");
        let base = validate_http_url("https://example.com/a").unwrap();
        // A relative redirect stays on the allowed host.
        let inside = resolve_redirect(&base, "../b").unwrap();
        assert!(ensure_host_allowed(&inside, &list).is_ok());
        // An absolute redirect to another host is caught by the same check
        // that the fetch loop applies on every hop.
        let outside = resolve_redirect(&base, "https://attacker.example/x").unwrap();
        assert!(ensure_host_allowed(&outside, &list).is_err());
    }

    fn assert_client_error(error: &anyhow::Error) {
        assert!(
            matches!(
                error.downcast_ref::<McpGatewayError>(),
                Some(McpGatewayError::ClientError(_))
            ),
            "expected a typed ClientError (HTTP 400), got: {error:?}"
        );
    }

    #[test]
    fn caller_input_errors_are_typed_client_errors() {
        for raw in [
            "file:///etc/passwd",
            "not a url",
            "https://user:pass@example.com",
        ] {
            let err = validate_http_url(raw).unwrap_err();
            assert_client_error(&err);
        }
        let base = validate_http_url("https://example.com/a").unwrap();
        assert_client_error(&redirect_target(&base, None).unwrap_err());
        assert_client_error(&resolve_redirect(&base, "http://[invalid").unwrap_err());
    }

    #[test]
    fn file_too_large_is_typed_client_error() {
        let (base, sandbox) = fixture();
        let big = base.join("big.bin");
        std::fs::write(&big, vec![b'x'; NATIVE_MAX_OUTPUT + 1]).unwrap();
        let err = sandbox
            .native_read(&json!({ "path": "big.bin" }))
            .unwrap_err();
        assert!(err.to_string().contains("file too large"), "{err}");
        assert_client_error(&err);
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn egress_allowlist_from_config_is_fail_closed() {
        let url = validate_http_url("https://example.com/").unwrap();
        for raw in ["", "   ", ",", "http://example.com", "bad host", "*."] {
            let list = EgressAllowlist::from_config(raw);
            let err = ensure_host_allowed(&url, &list).unwrap_err();
            assert!(err.to_string().contains("egress allowlist"), "{err}");
            assert_client_error(&err);
        }
    }

    #[test]
    fn egress_allowlist_from_config_is_enforced() {
        let list = EgressAllowlist::from_config(" example.com , *.trusted.org ");
        let allowed = validate_http_url("https://api.trusted.org/x").unwrap();
        assert!(ensure_host_allowed(&allowed, &list).is_ok());
        let denied = validate_http_url("https://attacker.example/").unwrap();
        let err = ensure_host_allowed(&denied, &list).unwrap_err();
        assert!(err.to_string().contains("egress allowlist"), "{err}");
        assert_client_error(&err);
    }

    #[tokio::test]
    async fn sandbox_wires_config_egress_allowlist() {
        let config = crate::config::McpGatewayConfig {
            egress_allowlist: "example.com".to_string(),
            ..Default::default()
        };
        let sandbox = Sandbox::new(&config).await.expect("sandbox starts");
        match sandbox.egress_allowlist {
            EgressAllowlist::Hosts(ref hosts) => {
                assert_eq!(hosts.len(), 1);
                assert_eq!(hosts[0], "example.com");
            }
            other => panic!("unexpected allowlist: {other:?}"),
        }
        let empty = Sandbox::new(&crate::config::McpGatewayConfig::default())
            .await
            .expect("sandbox starts");
        assert!(matches!(empty.egress_allowlist, EgressAllowlist::DenyAll));
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
        // u64::MAX must clamp, not wrap around on a narrower usize.
        assert_eq!(
            web_max_bytes(&json!({"max_bytes": u64::MAX})),
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

    /// (b) Regression: after a timeout the child must be killed and reaped,
    /// not left as an orphan holding its stdio fds. Uses `sleep` instead of
    /// `git` so the test is deterministic and needs no repository; production
    /// and test share the same `collect_child_output` helper.
    #[tokio::test]
    async fn timeout_kills_and_reaps_child() {
        let mut cmd = Command::new("sleep");
        crate::subprocess::apply_minimal_env(&mut cmd);
        cmd.arg("30").kill_on_drop(true);
        let mut child = cmd
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("spawn sleep");
        let pid = child.id().expect("child has a pid");
        let mut stdout = child.stdout.take().unwrap();
        let mut stderr = child.stderr.take().unwrap();

        let error = collect_child_output(
            &mut child,
            &mut stdout,
            &mut stderr,
            Duration::from_millis(100),
            1024,
        )
        .await
        .expect_err("a still-running child hits the deadline");
        assert!(error.to_string().contains("timed out"), "{error}");

        // `wait()` in the timeout branch reaped the child; on Linux a killed
        // and reaped process no longer has a /proc entry.
        let proc_path = std::path::PathBuf::from(format!("/proc/{pid}"));
        assert!(
            !proc_path.exists(),
            "child {pid} survived the timeout and was not reaped"
        );
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

    #[test]
    fn sensitive_path_components_are_refused() {
        let base = Path::new("/home/user/workspace");
        for relative in [
            ".ssh/id_rsa",
            ".ssh/id_rsa.pub",
            ".ssh/known_hosts",
            ".aws/credentials",
            ".aws/credentials.json",
            ".gnupg/private-keys-v1.d/ABCDEF.key",
            ".config/gcloud/settings.db",
            ".kube/config",
        ] {
            let path = base.join(relative);
            assert!(
                Sandbox::reject_sensitive_path(&path).is_err(),
                "{relative} must be refused"
            );
        }
        // No sensitive component: benign names and a bare `.config` segment.
        for relative in [
            "notes.txt",
            ".config/user-settings.toml",
            "docs/ssh-notes.md",
        ] {
            assert!(
                Sandbox::reject_sensitive_path(&base.join(relative)).is_ok(),
                "{relative} must be allowed"
            );
        }
    }

    #[test]
    fn refuses_read_under_sensitive_directory() {
        let (base, sandbox) = fixture();
        let ssh = base.join(".ssh");
        std::fs::create_dir_all(&ssh).unwrap();
        std::fs::write(ssh.join("id_rsa"), b"PRIVATE KEY").unwrap();
        let err = sandbox
            .native_read(&json!({"path": ".ssh/id_rsa"}))
            .expect_err(".ssh/id_rsa must be refused end to end");
        assert!(err.to_string().contains("sensitive"), "{err}");
        let _ = std::fs::remove_dir_all(base);
    }

    /// (b) Regression: a non-UTF-8 filename used to be dropped by
    /// `filter_map(|c| c.as_os_str().to_str())`, so `token\xff.txt` escaped
    /// the sensitive check and `filesystem.read` returned its contents. The
    /// file is created in the test's temporary workspace, and an ASCII symlink
    /// lets the JSON `path` argument reach it: canonicalization resolves the
    /// symlink to the real non-UTF-8 component before the check runs.
    #[cfg(unix)]
    #[test]
    fn non_utf8_sensitive_filename_is_refused_by_native_read() {
        use std::os::unix::ffi::OsStringExt;

        let (base, sandbox) = fixture();
        let raw_name = std::ffi::OsString::from_vec(b"token\xff.txt".to_vec());
        let probe_file = base.join(raw_name);
        std::fs::write(&probe_file, b"probe value").unwrap();
        let alias = base.join("alias.txt");
        std::os::unix::fs::symlink(&probe_file, &alias).unwrap();

        let err = sandbox
            .native_read(&json!({"path": "alias.txt"}))
            .expect_err("non-UTF-8 'token\\xff' filename must be refused");
        assert!(err.to_string().contains("sensitive"), "{err}");
        let _ = std::fs::remove_dir_all(base);
    }

    /// V-140: `is_textual_content_type` is the sole gate that decides whether a
    /// fetched HTTP body is rendered into an agent-visible response. A miss
    /// for a binary type would feed non-text bytes into the prompt; a false
    /// positive for a harmless type would needlessly fail a legitimate fetch.
    /// This pure test pins the classification without any network I/O.
    #[test]
    fn textual_content_type_classification_is_pinned() {
        for ct in [
            "text/plain",
            "text/plain; charset=utf-8",
            "TEXT/HTML",
            "application/json",
            "application/problem+json",
            "application/xml",
            "application/xhtml+xml",
            "application/javascript",
            "text/ecmascript",
        ] {
            assert!(is_textual_content_type(ct), "must accept {ct}");
        }
        for ct in [
            "application/octet-stream",
            "image/png",
            "application/pdf",
            "application/zip",
            "audio/mpeg",
            "video/mp4",
            "",
        ] {
            assert!(!is_textual_content_type(ct), "must reject {ct}");
        }
    }
}
