use futures_util::future::BoxFuture;
use pig_protocol::{ApprovalDecision, Event};

use super::{PendingApprovals, emit_bg};

#[derive(Clone, Debug)]
pub(crate) struct ApprovalRequest {
    pub session_id: String,
    pub request_id: String,
    pub tool: String,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> ApprovalRequest {
        ApprovalRequest {
            session_id: "s1".into(),
            request_id: "s1-approval-1".into(),
            tool: "Write".into(),
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
}
