//! JARVIS desktop console (GPUI).
//!
//! Phase 1 scope: the human-in-the-loop console for the `jarvis-mcp-gw`
//! approval flow (MCP-1). It polls `jarvis-policy` over localhost HTTP and
//! lets the operator approve or deny pending tool calls. No privileged
//! operations: every request goes to the loopback-only policy HTTP surface.
//!
//! See `adr/0005-gpui-desktop-adoption.md` for why this crate is standalone
//! (not a member of `rust/Cargo.toml`) and why GPUI was selected.

mod approvals;
mod policy_client;

use gpui::{App, Bounds, WindowBounds, WindowOptions, prelude::*, px, size};
use gpui_platform::application;

use approvals::ApprovalsView;
use policy_client::{
    DEFAULT_POLICY_HTTP_URL, ENV_APPROVAL_API_TOKEN, ENV_LEGACY_APPROVAL_TOKEN,
    ENV_POLICY_HTTP_URL, tokens_from_env,
};

fn main() {
    let base_url =
        std::env::var(ENV_POLICY_HTTP_URL).unwrap_or_else(|_| DEFAULT_POLICY_HTTP_URL.to_string());
    let tokens = tokens_from_env();
    if tokens.legacy_api_env_used() {
        // Names only — never log, echo, or embed the token value itself.
        let warning = format!(
            "warning: {ENV_LEGACY_APPROVAL_TOKEN} is deprecated; \
             set {ENV_APPROVAL_API_TOKEN} instead \
             (the legacy name is honoured temporarily)"
        );
        eprintln!("{warning}");
    }
    // `ApprovalsView::new` exposes a single Option<String> channel; the bundle
    // carries both scopes through it without changing that out-of-scope file.
    let token_arg = tokens.encode();

    application().run(move |cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(980.), px(680.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(gpui::TitlebarOptions {
                    title: Some("JARVIS — Approval Console".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            |_window, cx| cx.new(|cx| ApprovalsView::new(base_url.clone(), token_arg.clone(), cx)),
        )
        .expect("open the JARVIS approval window");
        cx.activate(true);
    });
}
