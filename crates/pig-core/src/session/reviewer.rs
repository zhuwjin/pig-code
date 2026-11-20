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
            let (decision, feedback) = reply_rx.await.map_err(|_| ApprovalReviewError {
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
        Box::pin(async move { (self.evaluate)(&request) })
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
        }
    }

    #[tokio::test]
    async fn user_reviewer_resolves_only_its_own_request() {
        let pending = PendingApprovals::default();
        let reviewer = UserApprovalReviewer::new(pending.clone());
        let (events, receiver) = async_channel::unbounded();
        let review = tokio::spawn(async move { reviewer.review(request(), events).await });
        let Event::ApprovalRequested { request_id, .. } = receiver.recv().await.unwrap() else {
            panic!("expected approval event");
        };
        super::super::resolve_approval(&pending, &request_id, ApprovalDecision::Allow, None);
        let reply = review.await.unwrap().unwrap();
        assert_eq!(reply.decision, ApprovalDecision::Allow);
        assert!(pending.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn cancelling_a_review_removes_its_pending_request() {
        let pending = PendingApprovals::default();
        let reviewer = UserApprovalReviewer::new(pending.clone());
        let (events, receiver) = async_channel::unbounded();
        let review = tokio::spawn(async move { reviewer.review(request(), events).await });
        let _ = receiver.recv().await.unwrap();
        review.abort();
        let _ = review.await;
        assert!(pending.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn policy_reviewer_receives_structured_identity_and_action() {
        let reviewer = PolicyApprovalReviewer::new(|request| {
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
        let (events, _receiver) = async_channel::unbounded();
        let reply = reviewer
            .review(request(), events)
            .await
            .expect("policy reply");
        assert_eq!(reply.decision, ApprovalDecision::Reject);
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
}
