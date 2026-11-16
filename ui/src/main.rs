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

fn main() {
    let base_url = std::env::var("JARVIS_POLICY_HTTP_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:8081".to_string());
    let approval_token = std::env::var("JARVIS_POLICY_APPROVAL_TOKEN").ok();

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
            |_window, cx| {
                cx.new(|cx| ApprovalsView::new(base_url.clone(), approval_token.clone(), cx))
            },
        )
        .expect("open the JARVIS approval window");
        cx.activate(true);
    });
}
