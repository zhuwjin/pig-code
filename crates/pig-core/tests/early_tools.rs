//! End-to-end coverage of early read execution: a read-only call whose
//! arguments complete mid-stream starts executing before the assistant
//! message is committed, runs exactly once, and its wave joins the finished
//! result. Nothing of it is visible before the batch: a turn cancelled
//! mid-stream leaves no tool record behind.
//!
//! The early call is a `FetchURL` aimed at a probe the test owns: the request
//! reaching the probe while the mock still holds the stream is the proof, so
//! no assertion depends on how fast the machine runs.

mod common;

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use pig_protocol::{Event, MessageDelivery, Op};
use pig_provider::mock;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const PROBE_BODY: &str = "EARLY_FETCH_BODY";

/// A one-endpoint HTTP server that announces each request and answers only
/// when the test says so.
struct Probe {
    url: String,
    arrived: tokio::sync::mpsc::UnboundedReceiver<()>,
    hits: Arc<AtomicUsize>,
    answer: Arc<tokio::sync::Notify>,
}

impl Probe {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!(
            "http://127.0.0.1:{}/early",
            listener.local_addr().unwrap().port()
        );
        let (arrived_tx, arrived) = tokio::sync::mpsc::unbounded_channel();
        let hits = Arc::new(AtomicUsize::new(0));
        let answer = Arc::new(tokio::sync::Notify::new());
        let counter = hits.clone();
        let gate = answer.clone();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                counter.fetch_add(1, Ordering::SeqCst);
                let _ = arrived_tx.send(());
                let answer = gate.clone();
                tokio::spawn(async move {
                    // The request head, so the connection is a complete fetch.
                    let mut head = [0u8; 2048];
                    let _ = tokio::time::timeout(Duration::from_secs(10), socket.read(&mut head))
                        .await;
                    answer.notified().await;
                    let response = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{PROBE_BODY}",
                        PROBE_BODY.len()
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                });
            }
        });
        Self {
            url,
            arrived,
            hits,
            answer,
        }
    }

    async fn wait_for_request(&mut self) {
        tokio::time::timeout(Duration::from_secs(10), self.arrived.recv())
            .await
            .expect("the early fetch never reached the probe")
            .expect("probe channel");
    }

    fn requests(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }
}

async fn send(agent: &pig_core::AgentHandle, session_id: &str, text: &str) {
    agent
        .ops
        .send(Op::SendMessage {
            session_id: session_id.to_string(),
            content: text.to_string(),
            files: vec![],
            images: vec![],
            permissions: pig_protocol::ExecMode::Auto.permissions(),
            delivery: MessageDelivery::Queue,
        })
        .await
        .unwrap();
}

async fn turn_end(agent: &pig_core::AgentHandle) -> Vec<Event> {
    common::recv_until(&agent.events, Duration::from_secs(60), |event| {
        matches!(
            event,
            Event::Error { .. } | Event::TurnComplete { .. } | Event::TurnAborted { .. }
        )
    })
    .await
}

/// Run the turn until the mock holds the stream with the first call complete
/// and that call's fetch has reached the probe. Returns the events seen so
/// far, which end before any tool card could exist.
async fn hold_after_first_call_started(
    agent: &pig_core::AgentHandle,
    session_id: &str,
    stall: &mut mock::StallObserver,
    probe: &mut Probe,
) -> Vec<Event> {
    send(
        agent,
        session_id,
        &format!("{} {}", mock::EARLY_TOOL_TRIGGER, probe.url),
    )
    .await;
    assert!(
        stall.wait_stall_started(Duration::from_secs(10)).await,
        "the mock never held the stream"
    );
    let opened = common::recv_until(&agent.events, Duration::from_secs(10), |event| {
        matches!(event, Event::ToolInputStart { .. })
    })
    .await;
    probe.wait_for_request().await;
    assert!(
        stall.is_stalled(),
        "the stream must still be held when the early fetch arrives"
    );
    opened
}

fn tool_cards(events: &[Event]) -> (usize, Vec<String>) {
    let begins = events
        .iter()
        .filter(|event| matches!(event, Event::ToolCallBegin { .. }))
        .count();
    let ends = events
        .iter()
        .filter_map(|event| match event {
            Event::ToolCallEnd {
                item_id, output, ..
            } => Some(format!("{item_id} {output}")),
            _ => None,
        })
        .collect();
    (begins, ends)
}

fn records(data_dir: &Path, session_id: &str, kind: &str) -> usize {
    let rollout = std::fs::read_to_string(
        data_dir
            .join("sessions")
            .join(format!("{session_id}.jsonl")),
    )
    .expect("rollout");
    rollout.matches(&format!("\"type\":\"{kind}\"")).count()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_read_only_call_starts_before_the_stream_ends_and_runs_once() {
    let mut probe = Probe::start().await;
    let (port, _log, mut stall) = mock::start_mock_server_with_stall_observer();
    let (config_path, cwd, data_dir) = common::setup_on("early-tools", port);
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir.clone());
    let session_id = common::new_session(&agent, cwd.clone()).await;

    let opened = hold_after_first_call_started(&agent, &session_id, &mut stall, &mut probe).await;
    // The fetch already left while the model's answer was still streaming,
    // and nothing was shown for it: a card exists only once the batch runs.
    assert_eq!(probe.requests(), 1);
    assert_eq!(tool_cards(&opened).0, 0, "{opened:#?}");

    probe.answer.notify_one();
    stall.release();
    let events = turn_end(&agent).await;

    // Joined, not run a second time: one request, one card, one durable pair.
    assert_eq!(probe.requests(), 1, "the read must not run twice");
    let (begins, ends) = tool_cards(&events);
    assert_eq!(begins, 2, "{events:#?}");
    assert_eq!(ends.len(), 2, "{events:#?}");
    let fetched: Vec<&String> = ends
        .iter()
        .filter(|end| end.contains("call_early_first"))
        .collect();
    assert_eq!(fetched.len(), 1, "{ends:#?}");
    assert!(
        fetched[0].contains(PROBE_BODY),
        "the card carries the early result: {ends:#?}"
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::TurnComplete { .. })),
        "the turn continues to its answer: {events:#?}"
    );
    assert_eq!(records(&data_dir, &session_id, "tool_execution_start"), 2);
    assert_eq!(records(&data_dir, &session_id, "tool_call"), 2);
    agent.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_turn_cancelled_while_an_early_read_is_in_flight_leaves_no_trace() {
    let mut probe = Probe::start().await;
    let (port, _log, mut stall) = mock::start_mock_server_with_stall_observer();
    let (config_path, cwd, data_dir) = common::setup_on("early-tools-cancel", port);
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir.clone());
    let session_id = common::new_session(&agent, cwd.clone()).await;

    let opened = hold_after_first_call_started(&agent, &session_id, &mut stall, &mut probe).await;
    assert_eq!(probe.requests(), 1);

    agent
        .ops
        .send(Op::Interrupt {
            session_id: session_id.clone(),
        })
        .await
        .unwrap();
    let aborted = turn_end(&agent).await;
    stall.release();
    probe.answer.notify_one();

    assert!(
        aborted
            .iter()
            .any(|event| matches!(event, Event::TurnAborted { .. })),
        "{aborted:#?}"
    );
    // The batch never ran, so the user saw no tool card and the rollout has
    // no tool record: the early read leaves nothing to replay or reconcile.
    let (begins, ends) = tool_cards(&opened);
    assert!(begins == 0 && ends.is_empty(), "{opened:#?}");
    let (begins, ends) = tool_cards(&aborted);
    assert!(begins == 0 && ends.is_empty(), "{aborted:#?}");
    assert_eq!(records(&data_dir, &session_id, "tool_execution_start"), 0);
    assert_eq!(records(&data_dir, &session_id, "tool_call"), 0);

    // The next turn starts clean.
    send(&agent, &session_id, "continue after the interrupt").await;
    let next = turn_end(&agent).await;
    assert!(
        next.iter()
            .any(|event| matches!(event, Event::TurnComplete { .. })),
        "{next:#?}"
    );
    assert!(
        !next.iter().any(|event| matches!(event, Event::Error { .. })),
        "{next:#?}"
    );
    agent.shutdown();
}
