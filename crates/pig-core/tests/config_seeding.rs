//! Startup seeding of the sensitive-file deny groups.
//!
//! The groups are ordinary `[[sandbox.deny]]` configuration now, so a fresh
//! install has to get them written into its own `config.toml` **before** the
//! first command runs — otherwise it would be unprotected until the first save.
//! These tests spawn the real agent loop (the seeding lives in `agent_loop`, not
//! in the Windows-gated sandbox setup) and read the file back.

use crate::common;

use common::{recv_until, setup};
use pig_protocol::{Event, Op};
use std::path::Path;
use std::time::Duration;

fn spawn(config_path: &Path, cwd: &Path, data_dir: &Path) -> pig_core::AgentHandle {
    pig_core::spawn_agent_with_data_dir(
        Some(config_path.to_path_buf()),
        cwd.to_path_buf(),
        data_dir.to_path_buf(),
    )
}

/// The config snapshot the settings page receives (the same `AppConfig` a save
/// writes back).
async fn config_snapshot(agent: &pig_core::AgentHandle) -> pig_protocol::AppConfig {
    agent.ops.send_blocking(Op::GetConfig).unwrap();
    let events = recv_until(&agent.events, Duration::from_secs(20), |event| {
        matches!(event, Event::ConfigSnapshot { .. })
    })
    .await;
    events
        .into_iter()
        .find_map(|event| match event {
            Event::ConfigSnapshot { config } => Some(config),
            _ => None,
        })
        .expect("config snapshot")
}

/// A missing key is seeded at startup, and everything else in the file survives
/// (the mock provider block stays readable).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agent_startup_seeds_the_deny_groups() {
    let (config_path, cwd, data_dir) = setup("seed-startup");
    let agent = spawn(&config_path, &cwd, &data_dir);

    let config = config_snapshot(&agent).await;
    agent.shutdown();

    let text = std::fs::read_to_string(&config_path).expect("seeded config");
    assert!(text.contains("[[sandbox.deny]]"), "{text}");
    assert_eq!(
        config.sandbox.deny,
        Some(pig_core::config::default_deny_groups()),
        "the snapshot the settings page edits carries the seeded groups"
    );
    // The rewrite preserved the provider block it did not touch.
    assert_eq!(config.default_provider, "mock");
    assert_eq!(config.providers.len(), 1);
}

/// A file that already carries the key — even an empty list — is a decision, not
/// an unseeded install: the agent leaves it alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_existing_empty_table_is_not_reseeded() {
    let (config_path, cwd, data_dir) = setup("seed-respect");
    let mut text = std::fs::read_to_string(&config_path).unwrap();
    text.push_str("\n[sandbox]\ndeny = []\n");
    std::fs::write(&config_path, &text).unwrap();

    let agent = spawn(&config_path, &cwd, &data_dir);
    let config = config_snapshot(&agent).await;
    agent.shutdown();

    assert_eq!(config.sandbox.deny, Some(Vec::new()));
    assert_eq!(
        std::fs::read_to_string(&config_path).unwrap(),
        text,
        "the file must not be rewritten"
    );
}
