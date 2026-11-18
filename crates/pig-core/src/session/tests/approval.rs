use super::super::reviewer::ApprovalReviewError;
use super::*;
use pig_protocol::ExecMode;

fn write_call(path: &str) -> ToolCall {
    ToolCall {
        id: "call-approval-policy".into(),
        name: "Write".into(),
        arguments: serde_json::json!({"path": path, "content": "approved"}).to_string(),
    }
}

async fn run_write(session: &mut Session, path: &str) -> GatedToolOutcome {
    let tools = tool::all();
    let write = tools
        .iter()
        .find(|tool| tool.name() == "Write")
        .expect("Write tool");
    let (events, _receiver) = async_channel::unbounded();
    session
        .exec_tool_gated(
            &write_call(path),
            Some(write.as_ref()),
            "item-approval-policy",
            "turn-approval-policy",
            &events,
            &CancellationToken::new(),
        )
        .await
}

fn policy(reply: Result<ApprovalReply, ApprovalReviewError>) -> Arc<dyn ApprovalReviewer> {
    Arc::new(PolicyApprovalReviewer::new(move |_| reply.clone()))
}

#[tokio::test]
async fn registered_policy_receives_the_execution_permission_snapshot() {
    let fixture = TrustFixture::new("approval-permission-snapshot");
    let mut session = fixture.create(None);
    let permissions = PermissionSettings {
        sandbox: pig_protocol::SandboxSettings {
            mode: SandboxMode::ReadOnly,
        },
        approval: pig_protocol::ApprovalPolicy::OnRequest,
        reviewer: pig_protocol::ApprovalsReviewer::Policy,
    };
    session.permissions = permissions;
    let seen = Arc::new(Mutex::new(Vec::new()));
    let captured = seen.clone();
    let expected_cwd = fixture.workspace.clone();
    session.reviewer = Arc::new(PolicyApprovalReviewer::new(move |request| {
        assert_eq!(request.session_id, "s1");
        assert_eq!(request.agent_id, "s1");
        assert_eq!(request.turn_id.as_deref(), Some("turn-approval-policy"));
        assert_eq!(request.call_id, "call-approval-policy");
        assert_eq!(request.action["path"], "approved-by-policy.txt");
        assert_eq!(request.cwd, expected_cwd);
        captured
            .lock()
            .expect("captured permissions")
            .push(request.permissions);
        Ok(ApprovalReply {
            decision: ApprovalDecision::Allow,
            feedback: None,
        })
    }));

    let outcome = run_write(&mut session, "approved-by-policy.txt").await;
    assert!(matches!(outcome, GatedToolOutcome::Executed { .. }));
    assert_eq!(
        seen.lock().expect("captured permissions").as_slice(),
        &[permissions]
    );
    assert_eq!(
        std::fs::read_to_string(fixture.workspace.join("approved-by-policy.txt"))
            .expect("approved write"),
        "approved"
    );
}

#[tokio::test]
async fn deferred_policy_reaches_the_real_user_fallback() {
    let fixture = TrustFixture::new("approval-defer-to-user");
    let mut session = fixture.create(None);
    session.permissions = ExecMode::ReadOnly.permissions();
    session.reviewer = Arc::new(FallbackApprovalReviewer::new(
        policy(Ok(ApprovalReply {
            decision: ApprovalDecision::DeferToUser,
            feedback: None,
        })),
        Arc::new(UserApprovalReviewer::new(session.pending.clone())),
        std::time::Duration::from_secs(1),
    ));
    let (events, receiver) = async_channel::unbounded();
    let pending = session.pending.clone();
    let answer = async move {
        let Event::ApprovalRequested { request_id, .. } =
            receiver.recv().await.expect("user approval request")
        else {
            panic!("expected user approval request");
        };
        resolve_approval(&pending, &request_id, ApprovalDecision::Allow, None);
    };
    let tools = tool::all();
    let write = tools
        .iter()
        .find(|tool| tool.name() == "Write")
        .expect("Write tool");
    let call = write_call("approved-after-defer.txt");
    let cancel = CancellationToken::new();
    let execute = session.exec_tool_gated(
        &call,
        Some(write.as_ref()),
        "item-defer",
        "turn-defer",
        &events,
        &cancel,
    );
    let (outcome, ()) = tokio::join!(execute, answer);
    assert!(matches!(outcome, GatedToolOutcome::Executed { .. }));
    assert_eq!(
        std::fs::read_to_string(fixture.workspace.join("approved-after-defer.txt"))
            .expect("deferred write"),
        "approved"
    );
}

#[tokio::test]
async fn policy_rejection_and_abort_have_distinct_gate_outcomes() {
    for (decision, aborted) in [
        (ApprovalDecision::Reject, false),
        (ApprovalDecision::Abort, true),
    ] {
        let fixture = TrustFixture::new(if aborted {
            "approval-abort"
        } else {
            "approval-reject"
        });
        let mut session = fixture.create(None);
        session.permissions = ExecMode::ReadOnly.permissions();
        session.reviewer = policy(Ok(ApprovalReply {
            decision,
            feedback: None,
        }));
        let outcome = run_write(&mut session, "must-not-exist.txt").await;
        assert_eq!(
            matches!(outcome, GatedToolOutcome::Cancelled),
            aborted,
            "unexpected outcome for {decision:?}"
        );
        assert!(!fixture.workspace.join("must-not-exist.txt").exists());
    }
}

#[tokio::test]
async fn unavailable_reviewer_fails_closed_before_the_tool_runs() {
    let fixture = TrustFixture::new("approval-reviewer-unavailable");
    let mut session = fixture.create(None);
    session.permissions = ExecMode::ReadOnly.permissions();
    session.reviewer = policy(Err(ApprovalReviewError {
        detail: "policy service unavailable".into(),
    }));
    let outcome = run_write(&mut session, "must-not-exist.txt").await;
    let GatedToolOutcome::Rejected { note } = outcome else {
        panic!("unavailable reviewer must reject the tool");
    };
    assert!(note.contains("policy service unavailable"));
    assert!(!fixture.workspace.join("must-not-exist.txt").exists());
}

#[tokio::test]
async fn cancelling_the_real_gate_removes_the_pending_user_review() {
    let fixture = TrustFixture::new("approval-gate-cancel");
    let mut session = fixture.create(None);
    session.permissions = ExecMode::ReadOnly.permissions();
    let (events, receiver) = async_channel::unbounded();
    let cancel = CancellationToken::new();
    let cancelled = cancel.clone();
    let pending = session.pending.clone();
    let interrupt = async move {
        let _ = receiver.recv().await.expect("approval request");
        cancelled.cancel();
    };
    let tools = tool::all();
    let write = tools
        .iter()
        .find(|tool| tool.name() == "Write")
        .expect("Write tool");
    let call = write_call("must-not-exist.txt");
    let execute = session.exec_tool_gated(
        &call,
        Some(write.as_ref()),
        "item-cancel",
        "turn-cancel",
        &events,
        &cancel,
    );
    let (outcome, ()) = tokio::join!(execute, interrupt);
    assert!(matches!(outcome, GatedToolOutcome::Cancelled));
    assert!(pending.lock().expect("pending approvals").is_empty());
    assert!(!fixture.workspace.join("must-not-exist.txt").exists());
}
