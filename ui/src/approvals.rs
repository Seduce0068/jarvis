//! Approval queue view: lists pending `jarvis-policy` approval requests and
//! lets the operator approve or deny each one.

use gpui::{
    ClickEvent, Context, IntoElement, KeyBinding, ParentElement, Render, Role, SharedString,
    Styled, Window, actions, div, prelude::*, rgb,
};

use crate::policy_client::{ApprovalRequest, PolicyClient};

// Tab navigation for the focusable approval buttons. GPUI has no default Tab
// binding, so these are registered here to keep the console's keyboard support
// self-contained (the app entry point does not bind any keys).
actions!(approvals, [Tab, TabPrev]);

/// Background refresh cadence for the pending queue.
const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(3);

pub struct ApprovalsView {
    client: PolicyClient,
    pending: Vec<ApprovalRequest>,
    status: SharedString,
    /// Last polling error, shown in the header until it clears.
    error: Option<SharedString>,
    /// Set once the first poll completes so the first frame shows a hint.
    loaded: bool,
    /// True while `status` reflects an operator decision (P0-8): the next
    /// poll must not overwrite it with a queue count.
    action_status: bool,
}

impl ApprovalsView {
    pub fn new(base_url: String, token: Option<String>, cx: &mut Context<Self>) -> Self {
        // Without a Tab binding the focusable buttons below are unreachable by
        // keyboard, which would make Enter/Space activation useless in practice.
        cx.bind_keys([
            KeyBinding::new("tab", Tab, None),
            KeyBinding::new("shift-tab", TabPrev, None),
        ]);

        let view = Self {
            client: PolicyClient::new_with_token(base_url, token),
            pending: Vec::new(),
            status: "承認要求を読み込み中…".into(),
            error: None,
            loaded: false,
            action_status: false,
        };
        view.spawn_poller(cx);
        view
    }

    /// Poll the policy service until the window goes away.
    fn spawn_poller(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |weak, cx| {
            loop {
                // Window closed: stop polling.
                let Some(entity) = weak.upgrade() else {
                    return;
                };
                let client = entity.read_with(cx, |view, _| view.client.clone());
                // Blocking HTTP must run on the background thread pool, never
                // on GPUI's foreground executor (it would freeze the window).
                let result = cx
                    .background_executor()
                    .spawn(async move { client.pending() })
                    .await;

                let stopped = weak
                    .update(cx, |view, cx| {
                        view.apply_poll(result);
                        cx.notify();
                    })
                    .is_err();
                if stopped {
                    return;
                }

                cx.background_executor().timer(POLL_INTERVAL).await;
            }
        })
        .detach();
    }

    /// Fold a poll result into the view state.
    ///
    /// This only updates the queue; `status` / `error` are owned by the most
    /// recent operator action. Clearing `error` here would hide an approve/
    /// deny failure on the very next 3-second refresh (P0-8).
    fn apply_poll(&mut self, result: anyhow::Result<Vec<ApprovalRequest>>) {
        self.loaded = true;
        match result {
            Ok(pending) => {
                // Preserve a status/error set by `decide`; only report the
                // count when nothing more important is pending.
                if !self.action_status {
                    self.status = format!("保留中: {} 件", pending.len()).into();
                } else {
                    // The decision message has been shown for one refresh
                    // cycle; subsequent polls report the queue count again.
                    self.action_status = false;
                }
                self.pending = pending;
            }
            Err(error) => {
                // Keep the last known list visible, but surface the failure so
                // a stale queue is not mistaken for an empty one.
                self.status = "jarvis-policy に接続できません".into();
                self.error = Some(format!("{error:#}").into());
            }
        }
    }

    /// Apply an operator decision, then refresh immediately.
    fn decide(&mut self, id: String, approve: bool, cx: &mut Context<Self>) {
        let client = self.client.clone();
        let label: SharedString = if approve { "承認" } else { "却下" }.into();
        self.status = format!("{label}を送信中… ({id})").into();

        cx.spawn(async move |weak, cx| {
            let decide_client = client.clone();
            let decide_id = id.clone();
            let result = cx
                .background_executor()
                .spawn(async move {
                    if approve {
                        decide_client.approve(&decide_id, "koppe")
                    } else {
                        decide_client.deny(&decide_id, "operator denied")
                    }
                })
                .await;
            let refresh = cx
                .background_executor()
                .spawn(async move { client.pending() })
                .await;

            let _ = weak.update(cx, |view, cx| {
                // Refresh the queue first; then set the decision outcome so
                // the poll result cannot overwrite it (P0-8).
                view.apply_poll(refresh);
                match result {
                    Ok(()) => {
                        view.status = format!("{label}しました").into();
                        view.error = None;
                        view.action_status = true;
                    }
                    Err(error) => {
                        // The request may already be decided or expired; show
                        // the reason rather than dropping it silently.
                        view.status = format!("{label}に失敗しました").into();
                        view.error = Some(format!("{error:#}").into());
                        view.action_status = true;
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn render_row(&self, request: &ApprovalRequest, cx: &mut Context<Self>) -> impl IntoElement {
        let expires = request
            .expires_at
            .map(|t| t.format("%H:%M:%S").to_string())
            .unwrap_or_else(|| "-".to_string());
        // An elapsed deadline is easy to miss in a dense queue; highlight it
        // so the operator does not approve a stale command by accident. This
        // is display-only: decide/on_click/keyboard handling stay unchanged.
        let expired = is_expired(request, chrono::Utc::now());

        let approve_id = request.id.clone();
        let deny_id = request.id.clone();

        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap_3()
            .p_3()
            .bg(if expired {
                rgb(0x3a2020)
            } else {
                rgb(0x2a2a2a)
            })
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_lg()
                            .child(format!("{}  ·  {}", request.principal, request.resource)),
                    )
                    .when(expired, |col| {
                        col.child(
                            div()
                                .px_2()
                                .py_1()
                                .bg(rgb(0x8b2f2f))
                                .text_color(rgb(0xffffff))
                                .text_sm()
                                .child("期限切れ"),
                        )
                    })
                    .child(muted(format!(
                        "action={}  期限={}  id={}",
                        request.action, expires, request.id
                    )))
                    // Show the full argument payload: truncating here would
                    // let an operator approve a command they never saw.
                    .child(muted(format_args(&request.context))),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .child(
                        div()
                            .id(SharedString::from(format!("approve-{}", request.id)))
                            // GPUI synthesizes a click from Enter/Space while a
                            // focusable element with click listeners is focused,
                            // so the existing on_click handler covers keyboard
                            // activation too. Adding on_key_down here would
                            // submit the decision twice.
                            .focusable()
                            .tab_stop(true)
                            .role(Role::Button)
                            .aria_label(SharedString::from(format!("承認: {}", request.id)))
                            .focus_visible(|style| style.bg(rgb(0x1b5e20)))
                            .px_3()
                            .py_1()
                            .bg(rgb(0x2e7d32))
                            .text_color(rgb(0xffffff))
                            .cursor_pointer()
                            .child("承認")
                            .on_click(cx.listener(move |view, _: &ClickEvent, _window, cx| {
                                view.decide(approve_id.clone(), true, cx)
                            })),
                    )
                    .child(
                        div()
                            .id(SharedString::from(format!("deny-{}", request.id)))
                            .focusable()
                            .tab_stop(true)
                            .role(Role::Button)
                            .aria_label(SharedString::from(format!("却下: {}", request.id)))
                            .focus_visible(|style| style.bg(rgb(0xa52626)))
                            .px_3()
                            .py_1()
                            .bg(rgb(0x8b2f2f))
                            .text_color(rgb(0xffffff))
                            .cursor_pointer()
                            .child("却下")
                            .on_click(cx.listener(move |view, _: &ClickEvent, _window, cx| {
                                view.decide(deny_id.clone(), false, cx)
                            })),
                    ),
            )
    }
}

impl Render for ApprovalsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Clone so the row listeners can take `cx` mutably while rendering.
        let pending = self.pending.clone();

        let mut list = div()
            .id(SharedString::from("approval-list"))
            .flex()
            .flex_col()
            .gap_2()
            .overflow_y_scroll();
        if !self.loaded {
            list = list.child(muted("読み込み中…"));
        } else if pending.is_empty() {
            list = list.child(muted("保留中の承認要求はありません。"));
        } else {
            for request in &pending {
                list = list.child(self.render_row(request, cx));
            }
        }

        div()
            .on_action(cx.listener(|_, _: &Tab, window, cx| window.focus_next(cx)))
            .on_action(cx.listener(|_, _: &TabPrev, window, cx| window.focus_prev(cx)))
            .flex()
            .flex_col()
            .size_full()
            .bg(rgb(0x1e1e1e))
            .text_color(rgb(0xe0e0e0))
            .p_4()
            .gap_3()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .child(div().text_xl().child("JARVIS 承認コンソール"))
                    .child(muted(self.status.clone())),
            )
            .child(match &self.error {
                Some(error) => div().bg(rgb(0x4a1f1f)).p_2().text_sm().child(error.clone()),
                None => div(),
            })
            .child(div().flex_1().child(list))
    }
}

fn muted(text: impl Into<SharedString>) -> impl IntoElement {
    div().text_sm().text_color(rgb(0x9a9a9a)).child(text.into())
}

/// Format the approval payload for display. Deliberately untruncated: an
/// operator must see the whole command before deciding.
fn format_args(context: &serde_json::Value) -> String {
    let body = serde_json::to_string_pretty(context).unwrap_or_else(|_| context.to_string());
    format!("args: {body}")
}

/// Pure expiry predicate shared by row rendering and regression tests.
/// A deadline exactly equal to `now` is still considered valid.
fn is_expired(request: &ApprovalRequest, now: chrono::DateTime<chrono::Utc>) -> bool {
    request
        .expires_at
        .is_some_and(|expires_at| expires_at < now)
}

#[cfg(test)]
mod tests {
    use super::{ApprovalsView, format_args, is_expired};
    use crate::policy_client::{ApprovalRequest, PolicyClient};
    use serde_json::json;

    #[test]
    fn approval_args_are_never_truncated() {
        let long = "あ".repeat(500);
        let context = json!({ "cmd": long });
        let rendered = format_args(&context);
        assert!(
            rendered.contains(&long),
            "full payload must be visible before approval"
        );
        assert!(rendered.starts_with("args: {"));
    }

    #[test]
    fn simple_context_is_rendered() {
        let expected = format!(
            "args: {}",
            serde_json::to_string_pretty(&json!({"url": "https://example.com/"})).unwrap()
        );
        assert_eq!(
            format_args(&json!({"url": "https://example.com/"})),
            expected
        );
    }

    fn view_with(status: &str, error: Option<&str>, action_status: bool) -> ApprovalsView {
        ApprovalsView {
            client: PolicyClient::new("http://127.0.0.1:1"),
            pending: Vec::new(),
            status: status.into(),
            error: error.map(|text| text.to_string().into()),
            loaded: true,
            action_status,
        }
    }

    fn request_with_expiry(
        id: &str,
        expires_at: Option<chrono::DateTime<chrono::Utc>>,
    ) -> ApprovalRequest {
        ApprovalRequest {
            id: id.into(),
            principal: "jarvis".into(),
            action: "execute".into(),
            resource: "tool:git.commit".into(),
            context: json!({}),
            requested_at: None,
            expires_at,
        }
    }

    /// TC-U2: the decision message survives one poll; the flag clears there
    /// and the queue count is reported from the following poll onward.
    #[test]
    fn action_status_clears_after_one_poll() {
        let mut view = view_with("承認しました", None, true);
        let pending = vec![request_with_expiry("a", None)];

        view.apply_poll(Ok(pending.clone()));
        assert!(!view.action_status);
        assert_eq!(view.status.to_string(), "承認しました");
        assert_eq!(view.pending.len(), 1);

        view.apply_poll(Ok(pending));
        assert!(!view.action_status);
        assert_eq!(view.status.to_string(), "保留中: 1 件");
    }

    /// TC-U3 / P0-8: a decide failure must not be cleared by the next poll.
    #[test]
    fn decision_error_survives_next_poll() {
        let mut view = view_with("却下に失敗しました", Some("POST returned HTTP 409"), true);

        view.apply_poll(Ok(vec![request_with_expiry("b", None)]));
        assert_eq!(view.status.to_string(), "却下に失敗しました");
        assert_eq!(
            view.error.as_ref().map(|value| value.to_string()),
            Some("POST returned HTTP 409".to_string())
        );
        assert_eq!(view.pending.len(), 1);
    }

    /// TC-U3: a polling error keeps the last known queue but shows the cause.
    #[test]
    fn poll_failure_keeps_queue_and_shows_error() {
        let mut view = view_with("保留中: 1 件", None, false);
        view.pending.push(request_with_expiry("keep", None));

        view.apply_poll(Err(anyhow::anyhow!("connection refused")));
        assert_eq!(view.pending.len(), 1);
        assert_eq!(view.status.to_string(), "jarvis-policy に接続できません");
        assert!(
            view.error
                .as_ref()
                .is_some_and(|value| value.to_string().contains("connection refused"))
        );
    }

    /// TC-U4: expiry is a strict less-than against the injected clock.
    #[test]
    fn expiry_predicate_boundaries() {
        let now = chrono::DateTime::from_timestamp(1_700_000_000, 0).expect("valid timestamp");
        let past = request_with_expiry("past", Some(now - chrono::Duration::seconds(1)));
        let exact = request_with_expiry("exact", Some(now));
        let future = request_with_expiry("future", Some(now + chrono::Duration::seconds(1)));
        let none = request_with_expiry("none", None);

        assert!(is_expired(&past, now));
        assert!(!is_expired(&exact, now));
        assert!(!is_expired(&future, now));
        assert!(!is_expired(&none, now));
    }
}
