//! Approval queue view: lists pending `jarvis-policy` approval requests and
//! lets the operator approve or deny each one.

use gpui::{
    ClickEvent, Context, IntoElement, KeyBinding, ParentElement, Render, Role, SharedString,
    Styled, Window, actions, div, prelude::*, rgb,
};

use crate::policy_client::{ApprovalRequest, PolicyClient, request_to_core};
use anyhow::Context as _;
use jarvis_core::approval::ApprovalDecision as CoreApprovalDecision;

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
    /// Last observed `jarvis-policy` emergency-stop state (I48-D). A decision
    /// published just before the stop still yields HTTP 200/Approved, but the
    /// tool execution is fail-closed at `jarvis-mcp-gw`. Kept separate from
    /// `status`/`error` so existing approval display logic is unchanged.
    kill_switch_active: bool,
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
            kill_switch_active: false,
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
                // The health probe is a second, independent loopback call; a
                // failure there must not change the queue result or its error.
                let probe_client = client.clone();
                // Blocking HTTP must run on the background thread pool, never
                // on GPUI's foreground executor (it would freeze the window).
                let result = cx
                    .background_executor()
                    .spawn(async move { client.pending() })
                    .await;
                let kill_switch_active = cx
                    .background_executor()
                    .spawn(async move { probe_client.kill_switch_active() })
                    .await;

                let stopped = weak
                    .update(cx, |view, cx| {
                        view.apply_poll(result);
                        view.kill_switch_active = kill_switch_active;
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
        let label: SharedString = if approve { "承認" } else { "却下" }.into();
        // Project the target through the canonical request boundary at decision
        // time: the row id is a tolerant wire value, the POST URL needs the
        // canonical UUID. Fail closed when the row vanished or its id is bad.
        let canonical_id = match decision_target(&self.pending, &id) {
            Ok(id) => id,
            Err(error) => {
                self.status = format!("{label}に失敗しました").into();
                self.error = Some(format!("{error:#}").into());
                self.action_status = true;
                cx.notify();
                return;
            }
        };
        let client = self.client.clone();
        // Name the operator's intent with the canonical core decision type.
        // Only the fields consumed by the unchanged HTTP body are read below.
        let intent = operator_decision(approve);
        self.status = format!("{label}を送信中… ({id})").into();

        cx.spawn(async move |weak, cx| {
            let decide_client = client.clone();
            let probe_client = client.clone();
            let decide_id = canonical_id.clone();
            let result = cx
                .background_executor()
                .spawn(async move {
                    match &intent {
                        CoreApprovalDecision::Approved { approved_by, .. } => {
                            decide_client.approve(&decide_id, approved_by)
                        }
                        CoreApprovalDecision::Denied { reason } => {
                            decide_client.deny(&decide_id, reason)
                        }
                        CoreApprovalDecision::Pending | CoreApprovalDecision::Expired => {
                            unreachable!("an operator action is never Pending or Expired")
                        }
                    }
                })
                .await;
            let refresh = cx
                .background_executor()
                .spawn(async move { client.pending() })
                .await;
            // A stop landing mid-commit still yields a published 200/Approved
            // decision. Probe the service so the operator sees that the tool
            // execution will be fail-closed at the gateway (I48-D).
            let kill_switch_active = cx
                .background_executor()
                .spawn(async move { probe_client.kill_switch_active() })
                .await;

            let _ = weak.update(cx, |view, cx| {
                // Refresh the queue first; then set the decision outcome so
                // the poll result cannot overwrite it (P0-8).
                view.apply_poll(refresh);
                view.kill_switch_active = kill_switch_active;
                match result {
                    Ok(decision) => {
                        // The service's tagged decision JSON crossed the core
                        // boundary in the client; project it onto the UI view
                        // instead of echoing the pre-send intent.
                        let status =
                            status_for_decision_with_kill_switch(&decision, kill_switch_active);
                        view.status = status.into();
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
            .child(if self.kill_switch_active {
                div()
                    .bg(rgb(0x5a3a10))
                    .p_2()
                    .text_sm()
                    .child("非常停止中：承認済みのツール実行も mcp-gw の再判定で拒否されます")
            } else {
                div()
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

/// Resolve the operator's target through the canonical request boundary.
///
/// Row ids originate in the tolerant UI wire DTO, so they are projected onto
/// the core request type at decision time. A stale or malformed id fails
/// closed instead of being interpolated into a POST URL as-is.
fn decision_target(pending: &[ApprovalRequest], id: &str) -> anyhow::Result<String> {
    let wire = pending
        .iter()
        .find(|request| request.id == id)
        .context("approval request is no longer pending")?;
    Ok(request_to_core(wire)?.id.to_string())
}

/// UI-facing projection of the canonical core decision.
///
/// `decision_from_json` parses the POST response into the core type; this is
/// the UI-side view used to render the operator status text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UiDecision {
    Approved,
    Denied,
    Pending,
    Expired,
}

impl From<&CoreApprovalDecision> for UiDecision {
    fn from(decision: &CoreApprovalDecision) -> Self {
        match decision {
            CoreApprovalDecision::Approved { .. } => Self::Approved,
            CoreApprovalDecision::Denied { .. } => Self::Denied,
            CoreApprovalDecision::Pending => Self::Pending,
            CoreApprovalDecision::Expired => Self::Expired,
        }
    }
}

/// Operator-visible status for the canonical decision returned by the service.
fn status_for_decision(decision: &CoreApprovalDecision) -> &'static str {
    match UiDecision::from(decision) {
        UiDecision::Approved => "承認しました",
        UiDecision::Denied => "却下しました",
        UiDecision::Pending => "保留中",
        UiDecision::Expired => "期限切れ",
    }
}

/// Operator-visible status that also accounts for the emergency-stop state
/// observed after a decision (I48-D). A commit landed mid-stop is still
/// published as HTTP 200 + Approved; `jarvis-mcp-gw` re-checks the kill switch
/// before executing the tool and fails closed. Report both facts instead of
/// presenting a plain success that the operator would trust.
fn status_for_decision_with_kill_switch(
    decision: &CoreApprovalDecision,
    kill_switch_active: bool,
) -> &'static str {
    if kill_switch_active && matches!(decision, CoreApprovalDecision::Approved { .. }) {
        "承認済み・実行は非常停止中"
    } else {
        status_for_decision(decision)
    }
}

/// Represent the operator action with the canonical core decision type.
///
/// The values read from this type are exactly the ones the existing HTTP
/// contract sends (`approve`: `approved_by`, `deny`: `reason`). The endpoint
/// paths, body shapes, status strings and expiry logic are unchanged.
fn operator_decision(approve: bool) -> CoreApprovalDecision {
    if approve {
        CoreApprovalDecision::Approved {
            approved_by: "koppe".into(),
            approved_at: chrono::Utc::now(),
        }
    } else {
        CoreApprovalDecision::Denied {
            reason: "operator denied".into(),
        }
    }
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
    use super::{
        ApprovalsView, CoreApprovalDecision, UiDecision, decision_target, format_args, is_expired,
        operator_decision, status_for_decision, status_for_decision_with_kill_switch,
    };
    use crate::policy_client::{ApprovalRequest, PolicyClient, request_to_core};
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
            kill_switch_active: false,
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

    /// TC-U9: the canonical core decision carries exactly the values the
    /// unchanged approve/deny HTTP bodies use.
    #[test]
    fn operator_decision_keeps_http_contract_values() {
        let approved = operator_decision(true);
        match approved {
            CoreApprovalDecision::Approved { approved_by, .. } => {
                assert_eq!(approved_by, "koppe");
            }
            other => panic!("expected Approved, got {other:?}"),
        }
        let denied = operator_decision(false);
        match denied {
            CoreApprovalDecision::Denied { reason } => assert_eq!(reason, "operator denied"),
            other => panic!("expected Denied, got {other:?}"),
        }
    }

    /// TC-U10: every field the row renders survives the core boundary, so the
    /// display projection stays faithful to the canonical domain type.
    #[test]
    fn core_request_keeps_display_fields() {
        let wire = request_with_expiry("3f9a1c2e-7b4d-4a1f-9c33-0d2e5f6a7b88", None);
        let core = request_to_core(&wire).expect("valid uuid converts");
        assert_eq!(core.principal, wire.principal);
        assert_eq!(core.action, wire.action);
        assert_eq!(core.resource, wire.resource);
        assert_eq!(core.context, wire.context);
    }

    /// I30-D regression: the decision target is canonicalized at decision
    /// time through `request_to_core`; a stale or malformed wire id fails closed.
    #[test]
    fn decision_target_projects_wire_id_through_core_at_decision_time() {
        let id = "3f9a1c2e-7b4d-4a1f-9c33-0d2e5f6a7b88";
        let pending = vec![request_with_expiry(id, None)];
        assert_eq!(decision_target(&pending, id).expect("canonical id"), id);

        let legacy = vec![request_with_expiry("not-a-uuid", None)];
        assert!(
            decision_target(&legacy, "not-a-uuid").is_err(),
            "a non-UUID wire id must not reach the POST URL"
        );
        assert!(
            decision_target(&pending, "missing-id").is_err(),
            "a stale row must not be decided"
        );
    }

    /// I30-D regression: the canonical core decision parsed from the service
    /// response is projected onto the UI decision and its status text.
    #[test]
    fn core_decision_projects_to_ui_decision_and_status() {
        let now = chrono::Utc::now();
        let cases = [
            (
                CoreApprovalDecision::Approved {
                    approved_by: "koppe".into(),
                    approved_at: now,
                },
                UiDecision::Approved,
                "承認しました",
            ),
            (
                CoreApprovalDecision::Denied {
                    reason: "because".into(),
                },
                UiDecision::Denied,
                "却下しました",
            ),
            (CoreApprovalDecision::Pending, UiDecision::Pending, "保留中"),
            (
                CoreApprovalDecision::Expired,
                UiDecision::Expired,
                "期限切れ",
            ),
        ];
        for (core, ui, expected_status) in cases {
            assert_eq!(UiDecision::from(&core), ui);
            assert_eq!(status_for_decision(&core), expected_status);
        }
    }

    /// I48-D: a published approval shown while the kill switch is active must
    /// report the fail-closed execution state; other decisions and an inactive
    /// switch keep their existing status text.
    #[test]
    fn approved_status_reports_halted_execution_while_kill_switch_active() {
        let approved = CoreApprovalDecision::Approved {
            approved_by: "koppe".into(),
            approved_at: chrono::Utc::now(),
        };
        assert_eq!(status_for_decision(&approved), "承認しました");
        assert_eq!(
            status_for_decision_with_kill_switch(&approved, true),
            "承認済み・実行は非常停止中"
        );
        assert_eq!(
            status_for_decision_with_kill_switch(&approved, false),
            "承認しました"
        );

        let denied = CoreApprovalDecision::Denied {
            reason: "because".into(),
        };
        assert_eq!(
            status_for_decision_with_kill_switch(&denied, true),
            "却下しました"
        );
        assert_eq!(
            status_for_decision_with_kill_switch(&CoreApprovalDecision::Pending, true),
            "保留中"
        );
    }
}
