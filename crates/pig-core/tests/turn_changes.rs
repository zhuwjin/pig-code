mod common;

use common::{new_session, recv_until, setup};
use pig_core::mock;
use pig_protocol::{Event, ExecMode, Op};
use std::time::Duration;

/// After a turn of writes ends (scenario B: Write + Edit on the same file), the
/// per-turn change event should be emitted: each file is accounted as the net
/// delta "before the first write this turn → now" (newly created = all
/// additions), and it is restored on rollout replay.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn turn_file_changes_emitted_and_replayed() {
    let (config_path, cwd, data_dir) = setup("turn-changes");
    let agent = pig_core::spawn_agent_with_data_dir(
        Some(config_path.clone()),
        cwd.clone(),
        data_dir.clone(),
    );
    let events = agent.events.clone();

    let session_id = new_session(&agent, cwd.clone()).await;
    agent
        .ops
        .send(Op::SendMessage {
            session_id: session_id.clone(),
            content: format!("{} edit a file", mock::SCENARIO_B_TRIGGER),
            files: vec![],
            images: vec![],
            mode: ExecMode::FullAccess,
        })
        .await
        .unwrap();
    let collected = recv_until(&events, Duration::from_secs(20), |e| {
        matches!(e, Event::TurnComplete { .. })
    })
    .await;
    let files = collected.iter().find_map(|e| match e {
        Event::TurnFileChanges { files, .. } => Some(files.clone()),
        _ => None,
    });
    let files = files.expect("turn end should emit TurnFileChanges");
    assert_eq!(
        files.len(),
        1,
        "scenario B touches exactly one file: {files:?}"
    );
    let change = &files[0];
    assert_eq!(change.path, mock::SCENARIO_B_FILE);
    // The file did not exist before the first write this turn → all 3 final lines count as additions (Write 3 lines + Edit modifies a line without adding one)
    assert_eq!(
        (change.additions, change.deletions),
        (3, 0),
        "net-change accounting: {}",
        change.unified_diff
    );
    agent.shutdown();

    // Simulate a restart and reopen the session: the per-turn changes panel is restored from rollout replay
    let agent2 = pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir);
    let events2 = agent2.events.clone();
    agent2
        .ops
        .send(Op::OpenSession {
            session_id: session_id.clone(),
        })
        .await
        .unwrap();
    let collected = recv_until(&events2, Duration::from_secs(20), |e| {
        // Replay-end sentinel: the TurnComplete with stats=None (the replayed
        // TurnComplete for TurnStats also has duration_ms=0 and can race ahead of
        // TurnFileChanges, truncating collection early)
        matches!(
            e,
            Event::TurnComplete {
                stats: None,
                duration_ms: 0,
                ..
            }
        )
    })
    .await;
    let replayed = collected.iter().find_map(|e| match e {
        Event::TurnFileChanges { files, .. } => Some(files.clone()),
        _ => None,
    });
    let replayed = replayed.expect("replay should restore TurnFileChanges");
    assert_eq!(replayed.len(), 1);
    assert_eq!(replayed[0].path, mock::SCENARIO_B_FILE);
    assert_eq!((replayed[0].additions, replayed[0].deletions), (3, 0));
    agent2.shutdown();
}
