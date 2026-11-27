use std::sync::Arc;

use futures_util::future::BoxFuture;
use pig_protocol::{ApprovalDecision, Event};

use super::{PendingApprovals, emit_bg};

#[derive(Clone, Debug)]
pub(crate) struct ApprovalRequest {
    pub session_id: String,
    pub agent_id: String,
    pub run_id: Option<String>,
    pub turn_id: Option<String>,
    pub call_id: String,
    pub request_id: String,
    pub cwd: std::path::PathBuf,
    pub permissions: pig_protocol::PermissionSettings,
    pub tool: String,
    pub action: serde_json::Value,
    pub detail: String,
    pub danger_key: Option<String>,
    pub requested_write_roots: Vec<String>,
    pub available_decisions: Vec<ApprovalDecision>,
    pub timing: Option<Arc<super::TurnTiming>>,
}

#[derive(Clone, Debug)]
pub(crate) struct ApprovalReply {
    pub decision: ApprovalDecision,
    pub feedback: Option<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct ApprovalReviewError {
    pub detail: String,
}

pub(crate) trait ApprovalReviewer: Send + Sync {
    fn review(
        &self,
        request: ApprovalRequest,
        events: async_channel::Sender<Event>,
    ) -> BoxFuture<'_, Result<ApprovalReply, ApprovalReviewError>>;
}

pub(crate) struct UserApprovalReviewer {
    pending: PendingApprovals,
}

impl UserApprovalReviewer {
    pub(crate) fn new(pending: PendingApprovals) -> Self {
        Self { pending }
    }
}

struct PendingReview {
    pending: PendingApprovals,
    request_id: String,
}

impl Drop for PendingReview {
    fn drop(&mut self) {
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&self.request_id);
    }
}

impl ApprovalReviewer for UserApprovalReviewer {
    fn review(
        &self,
        request: ApprovalRequest,
        events: async_channel::Sender<Event>,
    ) -> BoxFuture<'_, Result<ApprovalReply, ApprovalReviewError>> {
        Box::pin(async move {
            let _wait =
                super::TurnTiming::optional_guard(&request.timing, super::TimingKind::UserWait);
            let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
            self.pending
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(request.request_id.clone(), reply_tx);
            let _pending = PendingReview {
                pending: self.pending.clone(),
                request_id: request.request_id.clone(),
            };
            emit_bg(&request.session_id, &events, |session_id| {
                Event::ApprovalRequested {
                    session_id,
                    request_id: request.request_id.clone(),
                    tool: request.tool,
                    detail: request.detail,
                    danger_key: request.danger_key,
                    requested_write_roots: request.requested_write_roots,
                    available_decisions: request.available_decisions,
                }
            });
            let reply = reply_rx.await;
            emit_bg(&request.session_id, &events, |session_id| {
                Event::ApprovalResolved {
                    session_id,
                    request_id: request.request_id.clone(),
                }
            });
            let (decision, feedback) = reply.map_err(|_| ApprovalReviewError {
                detail: "the user approval channel closed without a decision".to_string(),
            })?;
            Ok(ApprovalReply { decision, feedback })
        })
    }
}

pub(crate) struct PolicyApprovalReviewer {
    evaluate:
        Arc<dyn Fn(&ApprovalRequest) -> Result<ApprovalReply, ApprovalReviewError> + Send + Sync>,
}

impl PolicyApprovalReviewer {
    pub(crate) fn new(
        evaluate: impl Fn(&ApprovalRequest) -> Result<ApprovalReply, ApprovalReviewError>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        Self {
            evaluate: Arc::new(evaluate),
        }
    }

    pub(crate) fn workspace_policy() -> Self {
        Self::new(|request| {
            let structured_identity = (
                &request.agent_id,
                &request.run_id,
                &request.turn_id,
                &request.call_id,
                &request.action,
            );
            let deny = request.permissions.approval == pig_protocol::ApprovalPolicy::Never
                || !request.cwd.is_absolute()
                || request.danger_key.is_some()
                || !request.requested_write_roots.is_empty()
                || !structured_identity.0.is_empty() && structured_identity.4.is_null();
            Ok(ApprovalReply {
                decision: if deny {
                    ApprovalDecision::Reject
                } else {
                    ApprovalDecision::Allow
                },
                feedback: None,
            })
        })
    }
}

impl ApprovalReviewer for PolicyApprovalReviewer {
    fn review(
        &self,
        request: ApprovalRequest,
        _events: async_channel::Sender<Event>,
    ) -> BoxFuture<'_, Result<ApprovalReply, ApprovalReviewError>> {
        Box::pin(async move {
            let _review =
                super::TurnTiming::optional_guard(&request.timing, super::TimingKind::Review);
            (self.evaluate)(&request)
        })
    }
}

/// Read-only, tool-free LLM approval reviewer (aligned with Codex Guardian /
/// DeepSeek Harness auto-review).
///
/// Structural invariants:
/// - Invokes `pig_provider::complete_text` with zero tools and no side effects,
///   making recursive approvals or tool execution impossible.
/// - Never overrides `ApprovalPolicy::Never`.
/// - High-risk verdicts or requests carrying a `danger_key` are never
///   auto-allowed: they downgrade to `DeferToUser` so a human decides.
/// - Unparseable responses, missing models, or provider errors defer/error cleanly
///   so `FallbackApprovalReviewer` escalates to the user.
pub(crate) struct AgentApprovalReviewer {
    resolve_model: Arc<dyn Fn() -> Option<pig_provider::ResolvedModel> + Send + Sync>,
}

#[derive(serde::Deserialize)]
struct AgentReviewVerdict {
    decision: String,
    #[serde(default)]
    risk: String,
    #[serde(default)]
    reason: Option<String>,
}

impl AgentApprovalReviewer {
    pub(crate) fn new(
        resolve_model: Arc<dyn Fn() -> Option<pig_provider::ResolvedModel> + Send + Sync>,
    ) -> Self {
        Self { resolve_model }
    }

    fn review_schema() -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "decision": { "type": "string", "enum": ["allow", "deny", "defer"] },
                "risk": { "type": "string", "enum": ["low", "medium", "high"] },
                "reason": { "type": "string" }
            },
            "required": ["decision", "risk", "reason"],
            "additionalProperties": false
        })
    }

    fn build_prompt(request: &ApprovalRequest) -> String {
        let payload = serde_json::json!({
            "session_id": request.session_id,
            "agent_id": request.agent_id,
            "tool": request.tool,
            "detail": request.detail,
            "action": request.action,
            "cwd": request.cwd.display().to_string(),
            "sandbox_mode": format!("{:?}", request.permissions.sandbox.mode),
            "danger_key": request.danger_key,
            "requested_write_roots": request.requested_write_roots,
        });
        format!(
            "You are a strict, read-only security approval reviewer for a coding agent.\n\
             You have NO tools and cannot run commands. Evaluate whether the requested action is safe.\n\
             Rules:\n\
             - Return ONLY a JSON object: {{\"decision\":\"allow\"|\"deny\"|\"defer\",\"risk\":\"low\"|\"medium\"|\"high\",\"reason\":\"...\"}}\n\
             - Choose \"deny\" for data exfiltration, credential theft, destructive operations, or unauthorized scope widening.\n\
             - Choose \"defer\" whenever uncertain or when human confirmation is appropriate.\n\
             - Choose \"allow\" only for clearly safe, scoped low/medium-risk actions.\n\n\
             Request:\n{}",
            serde_json::to_string_pretty(&payload).unwrap_or_default()
        )
    }

    fn parse_verdict(raw: &str, request: &ApprovalRequest) -> ApprovalReply {
        let trimmed = raw.trim();
        let cleaned = trimmed
            .strip_prefix("```json")
            .or_else(|| trimmed.strip_prefix("```"))
            .and_then(|s| s.strip_suffix("```"))
            .unwrap_or(trimmed)
            .trim();
        let Ok(verdict) = serde_json::from_str::<AgentReviewVerdict>(cleaned) else {
            return ApprovalReply {
                decision: ApprovalDecision::DeferToUser,
                feedback: None,
            };
        };
        let decision = verdict.decision.trim().to_ascii_lowercase();
        let risk = verdict.risk.trim().to_ascii_lowercase();
        let reason = verdict
            .reason
            .map(|r| r.trim().to_string())
            .filter(|r| !r.is_empty());
        match decision.as_str() {
            "allow" => {
                // High-risk assessments and destructive commands always require human review.
                if risk == "high" || request.danger_key.is_some() {
                    ApprovalReply {
                        decision: ApprovalDecision::DeferToUser,
                        feedback: reason,
                    }
                } else {
                    ApprovalReply {
                        decision: ApprovalDecision::Allow,
                        feedback: reason,
                    }
                }
            }
            "deny" | "reject" => ApprovalReply {
                decision: ApprovalDecision::Reject,
                feedback: reason.or_else(|| Some("Denied by automated review agent.".to_string())),
            },
            _ => ApprovalReply {
                decision: ApprovalDecision::DeferToUser,
                feedback: reason,
            },
        }
    }
}

impl ApprovalReviewer for AgentApprovalReviewer {
    fn review(
        &self,
        request: ApprovalRequest,
        _events: async_channel::Sender<Event>,
    ) -> BoxFuture<'_, Result<ApprovalReply, ApprovalReviewError>> {
        let resolve_model = self.resolve_model.clone();
        Box::pin(async move {
            let _review =
                super::TurnTiming::optional_guard(&request.timing, super::TimingKind::Review);
            if request.permissions.approval == pig_protocol::ApprovalPolicy::Never {
                return Ok(ApprovalReply {
                    decision: ApprovalDecision::Reject,
                    feedback: Some("Approval policy forbids automated allowance.".to_string()),
                });
            }
            let Some(mut model) = (resolve_model)() else {
                return Err(ApprovalReviewError {
                    detail: "no model configured for automated approval reviewer".to_string(),
                });
            };
            model.max_output_tokens = model.max_output_tokens.min(512);
            let prompt = Self::build_prompt(&request);
            let structured = model
                .cap_structured
                .then(|| pig_provider::StructuredOutput {
                    name: "approval_review_verdict",
                    schema: Self::review_schema(),
                });
            let cancel = tokio_util::sync::CancellationToken::new();
            let call = pig_provider::complete_text(&model, prompt, structured, &cancel)
                .await
                .map_err(|err| ApprovalReviewError {
                    detail: pig_provider::core_error_en(&err.error),
                })?;
            Ok(Self::parse_verdict(&call.text, &request))
        })
    }
}

pub(crate) struct FallbackApprovalReviewer {
    primary: Arc<dyn ApprovalReviewer>,
    fallback: Arc<dyn ApprovalReviewer>,
    timeout: std::time::Duration,
}

impl FallbackApprovalReviewer {
    pub(crate) fn new(
        primary: Arc<dyn ApprovalReviewer>,
        fallback: Arc<dyn ApprovalReviewer>,
        timeout: std::time::Duration,
    ) -> Self {
        Self {
            primary,
            fallback,
            timeout,
        }
    }
}

impl ApprovalReviewer for FallbackApprovalReviewer {
    fn review(
        &self,
        request: ApprovalRequest,
        events: async_channel::Sender<Event>,
    ) -> BoxFuture<'_, Result<ApprovalReply, ApprovalReviewError>> {
        let primary = self.primary.clone();
        let fallback = self.fallback.clone();
        let timeout = self.timeout;
        Box::pin(async move {
            match tokio::time::timeout(timeout, primary.review(request.clone(), events.clone()))
                .await
            {
                Ok(Ok(reply)) if reply.decision != ApprovalDecision::DeferToUser => Ok(reply),
                Ok(Ok(_)) | Ok(Err(_)) | Err(_) => match fallback.review(request, events).await {
                    Ok(ApprovalReply {
                        decision: ApprovalDecision::DeferToUser,
                        ..
                    }) => Err(ApprovalReviewError {
                        detail: "the user reviewer returned DeferToUser without a decision"
                            .to_string(),
                    }),
                    result => result,
                },
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> ApprovalRequest {
        ApprovalRequest {
            session_id: "s1".into(),
            agent_id: "a1".into(),
            run_id: Some("run-1".into()),
            turn_id: Some("turn-1".into()),
            call_id: "call-1".into(),
            request_id: "s1-approval-1".into(),
            cwd: std::env::temp_dir(),
            permissions: pig_protocol::PermissionSettings::default(),
            tool: "Write".into(),
            action: serde_json::json!({ "path": "file.txt" }),
            detail: "write file".into(),
            danger_key: None,
            requested_write_roots: vec![],
            available_decisions: vec![ApprovalDecision::Allow, ApprovalDecision::Reject],
            timing: None,
        }
    }

    #[tokio::test]
    async fn user_reviewer_resolves_only_its_own_request() {
        let pending = PendingApprovals::default();
        let reviewer = UserApprovalReviewer::new(pending.clone());
        let timing = Arc::new(super::super::TurnTiming::default());
        let mut request = request();
        request.timing = Some(timing.clone());
        let (events, receiver) = async_channel::unbounded();
        let review = tokio::spawn(async move { reviewer.review(request, events).await });
        let Event::ApprovalRequested { request_id, .. } = receiver.recv().await.unwrap() else {
            panic!("expected approval event");
        };
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        super::super::resolve_approval(&pending, &request_id, ApprovalDecision::Allow, None);
        let reply = review.await.unwrap().unwrap();
        assert_eq!(reply.decision, ApprovalDecision::Allow);
        assert!(pending.lock().unwrap().is_empty());
        assert!(timing.breakdown(100, 0).user_wait_ms > 0);
        assert_eq!(timing.breakdown(100, 0).review_ms, 0);
    }

    #[tokio::test]
    async fn cancelling_a_review_removes_its_pending_request() {
        let pending = PendingApprovals::default();
        let reviewer = UserApprovalReviewer::new(pending.clone());
        let timing = Arc::new(super::super::TurnTiming::default());
        let mut request = request();
        request.timing = Some(timing.clone());
        let (events, receiver) = async_channel::unbounded();
        let review = tokio::spawn(async move { reviewer.review(request, events).await });
        let _ = receiver.recv().await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        review.abort();
        let _ = review.await;
        assert!(pending.lock().unwrap().is_empty());
        assert!(timing.breakdown(100, 0).user_wait_ms > 0);
    }

    #[tokio::test]
    async fn rejected_user_review_still_records_the_wait() {
        let pending = PendingApprovals::default();
        let reviewer = UserApprovalReviewer::new(pending.clone());
        let timing = Arc::new(super::super::TurnTiming::default());
        let mut request = request();
        request.timing = Some(timing.clone());
        let (events, receiver) = async_channel::unbounded();
        let review = tokio::spawn(async move { reviewer.review(request, events).await });
        let Event::ApprovalRequested { request_id, .. } = receiver.recv().await.expect("request")
        else {
            panic!("expected approval event")
        };
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        super::super::resolve_approval(&pending, &request_id, ApprovalDecision::Reject, None);
        assert_eq!(
            review.await.expect("review task").expect("reply").decision,
            ApprovalDecision::Reject
        );
        assert!(timing.breakdown(100, 0).user_wait_ms > 0);
    }

    #[tokio::test]
    async fn policy_reviewer_receives_structured_identity_and_action() {
        let timing = Arc::new(super::super::TurnTiming::default());
        let reviewer = PolicyApprovalReviewer::new(|request| {
            std::thread::sleep(std::time::Duration::from_millis(5));
            assert_eq!(request.agent_id, "a1");
            assert_eq!(request.run_id.as_deref(), Some("run-1"));
            assert_eq!(request.turn_id.as_deref(), Some("turn-1"));
            assert_eq!(request.call_id, "call-1");
            assert_eq!(request.action["path"], "file.txt");
            assert_eq!(
                request.permissions,
                pig_protocol::PermissionSettings::default()
            );
            Ok(ApprovalReply {
                decision: ApprovalDecision::Reject,
                feedback: Some("policy denied".to_string()),
            })
        });
        let mut request = request();
        request.timing = Some(timing.clone());
        let (events, _receiver) = async_channel::unbounded();
        let reply = reviewer
            .review(request, events)
            .await
            .expect("policy reply");
        assert_eq!(reply.decision, ApprovalDecision::Reject);
        let breakdown = timing.breakdown(100, 0);
        assert!(breakdown.review_ms > 0);
        assert_eq!(breakdown.user_wait_ms, 0);
    }

    #[tokio::test]
    async fn workspace_policy_cannot_override_a_never_approval_policy() {
        let reviewer = PolicyApprovalReviewer::workspace_policy();
        let mut request = request();
        request.permissions.approval = pig_protocol::ApprovalPolicy::Never;
        let (events, _receiver) = async_channel::unbounded();
        let reply = reviewer
            .review(request, events)
            .await
            .expect("policy reply");
        assert_eq!(reply.decision, ApprovalDecision::Reject);
    }

    #[tokio::test]
    async fn stuck_primary_reviewer_times_out_to_user_fallback() {
        struct StuckReviewer;
        impl ApprovalReviewer for StuckReviewer {
            fn review(
                &self,
                _request: ApprovalRequest,
                _events: async_channel::Sender<Event>,
            ) -> BoxFuture<'_, Result<ApprovalReply, ApprovalReviewError>> {
                Box::pin(async move {
                    std::future::pending::<()>().await;
                    unreachable!()
                })
            }
        }

        let pending = PendingApprovals::default();
        let reviewer = FallbackApprovalReviewer::new(
            Arc::new(StuckReviewer),
            Arc::new(UserApprovalReviewer::new(pending.clone())),
            std::time::Duration::from_millis(20),
        );
        let (events, receiver) = async_channel::unbounded();
        let review = tokio::spawn(async move { reviewer.review(request(), events).await });
        let Event::ApprovalRequested { request_id, .. } =
            receiver.recv().await.expect("fallback event")
        else {
            panic!("expected user fallback approval");
        };
        super::super::resolve_approval(&pending, &request_id, ApprovalDecision::Allow, None);
        let reply = review.await.expect("review task").expect("fallback reply");
        assert_eq!(reply.decision, ApprovalDecision::Allow);
    }

    async fn start_review_mock_server(
        reply_content: &'static str,
    ) -> (
        String,
        Arc<std::sync::Mutex<Option<String>>>,
        tokio::task::JoinHandle<()>,
    ) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        let captured = Arc::new(std::sync::Mutex::new(None));
        let captured_clone = captured.clone();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let mut buf = vec![0u8; 16 * 1024];
            let n = socket.read(&mut buf).await.unwrap_or(0);
            let raw = String::from_utf8_lossy(&buf[..n]).to_string();
            *captured_clone.lock().unwrap() = Some(raw);
            let body = serde_json::json!({
                "id": "review-1",
                "choices": [{
                    "message": { "role": "assistant", "content": reply_content }
                }]
            })
            .to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.shutdown().await;
        });
        (format!("http://{addr}/v1"), captured, server)
    }

    fn review_model(base_url: String) -> pig_provider::ResolvedModel {
        pig_provider::ResolvedModel {
            base_url,
            api_key: "test-key".into(),
            model: "review-model".into(),
            context_window: 32_000,
            max_output_tokens: 1_024,
            api_format: pig_protocol::ApiFormat::OpenAiChat,
            reasoning_params: None,
            cap_structured: true,
            cap_strict_tools: false,
            cap_web_search: false,
            web_search_tool: None,
            input_image: false,
            input_pdf: false,
            anthropic_cache_control: false,
            replay_native_search: false,
            prompt_cache_key: None,
            provider_name: "mock".into(),
            tool_settings: Default::default(),
        }
    }

    #[tokio::test]
    async fn agent_reviewer_allows_low_risk_action_without_exposing_tools() {
        let (base_url, captured, server) = start_review_mock_server(
            r#"{"decision":"allow","risk":"low","reason":"Scoped workspace write"}"#,
        )
        .await;
        let model = review_model(base_url);
        let reviewer = AgentApprovalReviewer::new(Arc::new(move || Some(model.clone())));
        let (events, _rx) = async_channel::unbounded();
        let reply = reviewer
            .review(request(), events)
            .await
            .expect("agent review reply");
        server.await.expect("server");
        assert_eq!(reply.decision, ApprovalDecision::Allow);
        let raw_request = captured.lock().unwrap().clone().expect("captured request");
        assert!(
            !raw_request.contains("\"tools\""),
            "read-only agent reviewer must never send tools: {raw_request}"
        );
    }

    #[tokio::test]
    async fn agent_reviewer_downgrades_high_risk_allow_to_user_fallback() {
        let (base_url, _captured, server) = start_review_mock_server(
            r#"{"decision":"allow","risk":"high","reason":"Risky command"}"#,
        )
        .await;
        let model = review_model(base_url);
        let pending = PendingApprovals::default();
        let reviewer = FallbackApprovalReviewer::new(
            Arc::new(AgentApprovalReviewer::new(Arc::new(move || {
                Some(model.clone())
            }))),
            Arc::new(UserApprovalReviewer::new(pending.clone())),
            std::time::Duration::from_secs(5),
        );
        let (events, receiver) = async_channel::unbounded();
        let task = tokio::spawn(async move { reviewer.review(request(), events).await });
        let Event::ApprovalRequested { request_id, .. } =
            receiver.recv().await.expect("high risk must defer to user")
        else {
            panic!("expected ApprovalRequested fallback");
        };
        super::super::resolve_approval(&pending, &request_id, ApprovalDecision::Reject, None);
        let reply = task.await.expect("task").expect("reply");
        server.await.expect("server");
        assert_eq!(reply.decision, ApprovalDecision::Reject);
    }

    #[tokio::test]
    async fn agent_reviewer_rejects_deny_verdict_and_never_policy() {
        let (base_url, _captured, server) = start_review_mock_server(
            r#"{"decision":"deny","risk":"medium","reason":"Outside scope"}"#,
        )
        .await;
        let model = review_model(base_url);
        let reviewer = AgentApprovalReviewer::new(Arc::new(move || Some(model.clone())));
        let (events, _rx) = async_channel::unbounded();
        let reply = reviewer
            .review(request(), events.clone())
            .await
            .expect("deny reply");
        server.await.expect("server");
        assert_eq!(reply.decision, ApprovalDecision::Reject);
        assert_eq!(reply.feedback.as_deref(), Some("Outside scope"));

        let mut never_req = request();
        never_req.permissions.approval = pig_protocol::ApprovalPolicy::Never;
        let never_reviewer = AgentApprovalReviewer::new(Arc::new(|| None));
        let reply = never_reviewer
            .review(never_req, events)
            .await
            .expect("never policy reply");
        assert_eq!(reply.decision, ApprovalDecision::Reject);
    }
}
