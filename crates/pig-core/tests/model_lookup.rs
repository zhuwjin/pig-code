//! ModelLookup → ModelInfo 全链路集成测试（首次访问 models.dev 真实网络，
//! 之后走磁盘缓存）。默认忽略，手动验证：cargo test -p pig-core -- --ignored

#[test]
#[ignore = "访问 models.dev 真实网络"]
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
                    .expect("60s 内应有响应")
                    .expect("channel open");
            if let Event::ModelInfo { .. } = &event {
                break event;
            }
        };
        let Event::ModelInfo { id, info } = first else {
            unreachable!()
        };
        assert_eq!(id, "claude-sonnet-4-6");
        let info = info.expect("models.dev 收录 claude-sonnet-4-6");
        assert!(info.context.expect("context 存在") > 100_000);
        assert!(info.output.expect("output 存在") > 1_000);
        assert!(
            !info.reasoning_levels.is_empty(),
            "claude 应带推理等级: {info:?}"
        );

        // 缓存文件已落盘；第二次查询命中缓存直接回
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
                    .expect("缓存命中应秒回")
                    .expect("channel open");
            if let Event::ModelInfo { id, info } = &event {
                assert_eq!(id, "gpt-4o");
                let info = info.as_ref().expect("gpt-4o 收录");
                assert!(info.context.expect("context") > 100_000);
                break;
            }
        }

        // deepseek-flash：官方收录带 image 输入模态（UI 据此勾"图片"）
        agent
            .ops
            .send(lookup("deepseek-flash"))
            .await
            .expect("send lookup 3");
        loop {
            let event =
                tokio::time::timeout(std::time::Duration::from_secs(10), agent.events.recv())
                    .await
                    .expect("缓存命中应秒回")
                    .expect("channel open");
            if let Event::ModelInfo { id, info } = &event {
                assert_eq!(id, "deepseek-flash");
                let info = info.as_ref().expect("deepseek-flash 收录");
                assert!(
                    info.input_modalities.iter().any(|m| m == "image"),
                    "deepseek-flash 应带 image 输入: {info:?}"
                );
                assert_eq!(
                    info.structured_output,
                    Some(true),
                    "deepseek-flash 支持结构化输出: {info:?}"
                );
                assert!(
                    !info.reasoning_levels.is_empty(),
                    "deepseek-flash 应带推理等级: {info:?}"
                );
                break;
            }
        }
    });
    agent.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}
