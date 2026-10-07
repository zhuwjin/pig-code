//! ModelLookup → ModelInfo full-chain integration test (first access hits the
//! real network at models.dev, afterwards served from the disk cache). Ignored
//! by default; run manually: cargo test -p pig-core -- --ignored

#[test]
#[ignore = "hits real network at models.dev"]
fn lookup_then_cache_roundtrip() {
    let dir = std::env::temp_dir().join(format!("pig-lookup-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let agent = pig_core::spawn_agent_with_data_dir(None, dir.clone(), dir.clone());

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        use pig_protocol::{Event, Op};

        let lookup = |id: &str| Op::ModelLookup { id: id.to_string() };
        agent
            .ops
            .send(lookup("claude-sonnet-4-6"))
            .await
            .expect("send lookup");

        let first = loop {
            let event =
                tokio::time::timeout(std::time::Duration::from_secs(60), agent.events.recv())
                    .await
                    .expect("should respond within 60s")
                    .expect("channel open");
            if let Event::ModelInfo { .. } = &event {
                break event;
            }
        };
        let Event::ModelInfo { id, info } = first else {
            unreachable!()
        };
        assert_eq!(id, "claude-sonnet-4-6");
        let info = info.expect("models.dev lists claude-sonnet-4-6");
        assert!(info.context.expect("context present") > 100_000);
        assert!(info.output.expect("output present") > 1_000);
        assert!(
            !info.reasoning_levels.is_empty(),
            "claude should carry reasoning levels: {info:?}"
        );

        // Cache file persisted; the second lookup hits the cache and replies directly
        assert!(dir.join("models-dev-cache.json").exists());
        agent
            .ops
            .send(lookup("gpt-4o"))
            .await
            .expect("send lookup 2");
        loop {
            let event =
                tokio::time::timeout(std::time::Duration::from_secs(10), agent.events.recv())
                    .await
                    .expect("cache hit should reply immediately")
                    .expect("channel open");
            if let Event::ModelInfo { id, info } = &event {
                assert_eq!(id, "gpt-4o");
                let info = info.as_ref().expect("gpt-4o listed");
                assert!(info.context.expect("context") > 100_000);
                break;
            }
        }

        // deepseek-flash: officially listed with the image input modality (the UI ticks "image" accordingly)
        agent
            .ops
            .send(lookup("deepseek-flash"))
            .await
            .expect("send lookup 3");
        loop {
            let event =
                tokio::time::timeout(std::time::Duration::from_secs(10), agent.events.recv())
                    .await
                    .expect("cache hit should reply immediately")
                    .expect("channel open");
            if let Event::ModelInfo { id, info } = &event {
                assert_eq!(id, "deepseek-flash");
                let info = info.as_ref().expect("deepseek-flash listed");
                assert!(
                    info.input_modalities.iter().any(|m| m == "image"),
                    "deepseek-flash should carry image input modality: {info:?}"
                );
                assert_eq!(
                    info.structured_output,
                    Some(true),
                    "deepseek-flash supports structured output: {info:?}"
                );
                assert!(
                    !info.reasoning_levels.is_empty(),
                    "deepseek-flash should carry reasoning levels: {info:?}"
                );
                break;
            }
        }
    });
    agent.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}
