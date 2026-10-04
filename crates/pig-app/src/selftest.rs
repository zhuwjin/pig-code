use super::*;

/// 自测环境：mock provider + 临时配置/工作目录 + 隔离数据目录。
pub(crate) struct SelftestEnv {
    pub(crate) config_path: PathBuf,
    pub(crate) cwd: PathBuf,
}

pub(crate) fn setup_selftest() -> SelftestEnv {
    let port = pig_core::mock::start_mock_server();
    let dir = std::env::temp_dir().join(format!("pig-app-selftest-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create selftest dir");
    std::fs::write(
        dir.join(pig_core::mock::MOCK_FILE_NAME),
        pig_core::mock::MOCK_FILE_CONTENT,
    )
    .expect("write mock file");
    // 数据目录必须在工作区**外面**（对齐生产 ~/.pigcode）：放在工作区内会被
    // Grep/Glob 工具搜到——rollout/model-io 落盘里存着历轮用户消息原文，
    // 子代理 grep 工作区会把其中的 mock 触发词（ECHO_HISTORY 等）带回请求体，
    // 抢先命中 mock 的内容路由分支（selftest 曾因此因子代理结论被
    // echo_history_response 截胡而挂）
    let data_dir =
        std::env::temp_dir().join(format!("pig-app-selftest-data-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data_dir);
    std::fs::create_dir_all(&data_dir).expect("create data dir");
    // agent 通过 PIG_DATA_DIR 找到隔离数据目录
    unsafe { std::env::set_var("PIG_DATA_DIR", &data_dir) };
    let config_path = dir.join("config.toml");
    std::fs::write(
        &config_path,
        format!(
            r#"default_provider = "mock"
default_model = "mock-model"

[[providers]]
id = "mock"
name = "Mock"
base_url = "http://127.0.0.1:{port}/v1"
api_key = "mock-key"
api_format = "OpenAiChat"
enabled = true

[[providers.models]]
id = "mock-model"
context_window = 128000
max_output_tokens = 8192
reasoning_levels = ["low", "high"]
default_reasoning_level = "low"

[[providers]]
id = "anthropic"
name = "AnthropicMock"
base_url = "http://127.0.0.1:{port}/v1"
api_key = "mock-key"
api_format = "AnthropicMessages"
enabled = true

[[providers.models]]
id = "mock-model"
context_window = 200000
max_output_tokens = 8192
reasoning_levels = ["high", "max"]
"#
        ),
    )
    .expect("write config");
    SelftestEnv {
        config_path,
        cwd: dir,
    }
}

/// PIG_SELFTEST=1：会话A完整修改链 → 会话B并行对话 → 切回A → 模拟重启 resume → @搜索。
pub(crate) async fn run_selftest(view: Entity<AppView>, cx: &mut AsyncApp) {
    use std::time::Duration;
    macro_rules! timer {
        ($ms:expr) => {
            cx.background_executor().timer(Duration::from_millis($ms))
        };
    }
    macro_rules! app {
        ($f:expr) => {
            view.update(cx, $f)
        };
    }

    timer!(800).await;

    // hero 态断言：无会话、hero 展示
    let is_hero = app!(|app: &mut AppView, cx| app.debug_is_hero(cx));
    assert!(is_hero, "启动应进入 hero 态");
    println!("[selftest] hero 态 OK");

    // hero 发送首条消息 → 自动建会话
    app!(|app: &mut AppView, cx| {
        app.exec_mode = pig_protocol::ExecMode::ConfirmBeforeEdit;
        app.hero_send(
            format!(
                "{} 创建并修改文件，然后跑个命令",
                pig_core::mock::SCENARIO_B_TRIGGER
            ),
            vec![],
            vec![],
            pig_protocol::ExecMode::ConfirmBeforeEdit,
            cx,
        );
    });
    let session_a = loop {
        timer!(200).await;
        let current = app!(|app: &mut AppView, _| app.current.clone());
        if let Some(id) = current {
            break id;
        }
    };
    println!("[selftest] hero 发送 → 会话 A 建立: {session_a}");

    // 会话 A：场景 B（审批×3）
    let mut approvals = 0u32;
    let mut waited = 0u64;
    loop {
        timer!(200).await;
        waited += 200;
        assert!(waited < 60_000, "会话 A 回合超时");
        let approved = app!(|app: &mut AppView, cx| {
            let views = app.views.get(&session_a)?;
            let pending = views.thread.read(cx).pending_approval();
            pending.map(|_| {
                views.thread.update(cx, |thread, cx| {
                    thread.decide_pending(pig_protocol::ApprovalDecision::Allow, cx);
                });
            })
        });
        if approved == Some(()) {
            approvals += 1;
            continue;
        }
        let done = app!(|app: &mut AppView, cx| {
            let views = app.views.get(&session_a)?;
            let streaming = views.thread.read(cx).is_streaming();
            let (tool_done, text, _, tool_output) = views.thread.read(cx).debug_last_assistant();
            if !streaming && waited > 1000 && tool_done {
                assert!(
                    text.contains(pig_core::mock::SCENARIO_B_MARKER),
                    "A 文本标记: {text}"
                );
                assert!(tool_output.contains(pig_core::mock::SCENARIO_B_BASH_MARKER));
                return Some(());
            }
            None
        });
        if done == Some(()) {
            break;
        }
    }
    assert_eq!(approvals, 3, "场景 B 三次审批");
    // hero → 会话态切换断言
    let is_hero = app!(|app: &mut AppView, cx| app.debug_is_hero(cx));
    assert!(!is_hero, "发送后应进入会话态");
    println!("[selftest] 会话 A 场景 B 完成（审批×3），输入框已沉底");

    // 调用轨迹面板：mock 回合的多步调用应已落 model-io 记录；面板加载、
    // 折叠/展开两态渲染（构造元素树不 panic 即过）
    app!(|app: &mut AppView, cx| {
        app.open_right_tab(RightTab::Trajectory, cx);
        let records = app.trajectory.as_ref().map(|s| s.records.len());
        assert!(
            records.is_some_and(|n| n >= 2),
            "场景 B 多步调用应落多条 model-io 记录: {records:?}"
        );
        let _ = app.render_trajectory_panel(cx);
    });
    app!(|app: &mut AppView, cx| {
        // 逐行展开：模拟点开首条调用的首行，重渲染展开态
        if let Some(state) = &mut app.trajectory {
            let key = format!("{}:0", state.records[0].turn);
            state.expanded.insert(key);
        }
        let _ = app.render_trajectory_panel(cx);
        app.close_right_tab(RightTab::Trajectory, cx);
    });
    println!("[selftest] 调用轨迹面板加载/展开渲染 OK");

    // 工作区视图：会话 cwd 应出现在工作区列表，且按工作区分组正确
    let cwd_str = app!(|app: &mut AppView, _| app.cwd.display().to_string());
    let (has_cwd, grouped) = app!(|app: &mut AppView, cx| {
        let sidebar = app.sidebar.read(cx);
        (
            sidebar.debug_workspaces().contains(&cwd_str),
            sidebar
                .debug_workspace_sessions(&cwd_str)
                .contains(&session_a),
        )
    });
    assert!(has_cwd, "工作区列表应包含会话 cwd");
    assert!(grouped, "工作区视图应按 cwd 分组会话");

    // 添加/移除工作区
    let extra = std::env::temp_dir().join(format!("pig-app-ws-{}", std::process::id()));
    std::fs::create_dir_all(&extra).unwrap();
    let extra_str = extra.display().to_string();
    app!(|app: &mut AppView, _| app.agent.add_workspace(extra.clone()));
    let mut waited = 0u64;
    loop {
        timer!(200).await;
        waited += 200;
        assert!(waited < 10_000, "添加工作区超时");
        let has = app!(|app: &mut AppView, cx| {
            app.sidebar.read(cx).debug_workspaces().contains(&extra_str)
        });
        if has {
            break;
        }
    }
    app!(|app: &mut AppView, _| app.agent.remove_workspace(extra.clone()));
    let mut waited = 0u64;
    loop {
        timer!(200).await;
        waited += 200;
        assert!(waited < 10_000, "移除工作区超时");
        let has = app!(|app: &mut AppView, cx| {
            app.sidebar.read(cx).debug_workspaces().contains(&extra_str)
        });
        if !has {
            break;
        }
    }
    println!("[selftest] 工作区列表 OK（会话 cwd 自动出现 + 手动增删）");

    // 会话 B：新建 + 场景 A
    app!(|app: &mut AppView, _| app
        .agent
        .new_session(app.cwd.clone(), None, None, None, None));
    let session_b = loop {
        timer!(200).await;
        let current = app!(|app: &mut AppView, _| app.current.clone());
        if let Some(id) = current
            && id != session_a
        {
            break id;
        }
    };
    println!("[selftest] 会话 B 就绪: {session_b}");
    app!(|app: &mut AppView, _| {
        app.agent.send_message(
            session_b.clone(),
            "读一下 README.mock.md 并总结".to_string(),
            vec![],
            vec![],
            pig_protocol::ExecMode::AutoEdit,
        );
    });
    timer!(300).await; // 等第一个回合开跑
    app!(|app: &mut AppView, _| {
        app.agent.send_message(
            session_b.clone(),
            "ECHO_HISTORY".to_string(),
            vec![],
            vec![],
            pig_protocol::ExecMode::AutoEdit,
        );
    });
    let mut saw_queued = false;
    let mut waited = 0u64;
    loop {
        timer!(200).await;
        waited += 200;
        assert!(waited < 60_000, "排队流程超时");
        let (queued_len, streaming, text) = app!(|app: &mut AppView, cx| {
            let Some(views) = app.views.get(&session_b) else {
                return (0, false, String::new());
            };
            let thread = views.thread.read(cx);
            let (_, text, _, _) = thread.debug_last_assistant();
            (thread.debug_queued().len(), thread.is_streaming(), text)
        });
        saw_queued |= queued_len > 0;
        if !streaming && text.contains("HISTORY_COUNT:6") {
            break;
        }
    }
    assert!(saw_queued, "应出现排队芯片");
    println!("[selftest] 会话 B 完成，消息排队 OK（自动接续，第二轮历史=6）");

    // turn 导航条：会话 B 有 2 轮用户消息，面板已绘制（宽度非零）且达到断点。
    // bounds 由 prepaint 记录：数据就绪不等于帧已绘制，等绘制循环跑完
    let mut waited = 0u64;
    let (mut nav_turns, mut nav_pane_w) = app!(|app: &mut AppView, cx| {
        let views = app.views.get(&session_b).expect("B 视图在内存");
        views.thread.read(cx).debug_nav_state()
    });
    loop {
        if nav_pane_w >= 720. || waited >= 10_000 {
            break;
        }
        timer!(200).await;
        waited += 200;
        (nav_turns, nav_pane_w) = app!(|app: &mut AppView, cx| {
            let views = app.views.get(&session_b).expect("B 视图在内存");
            views.thread.read(cx).debug_nav_state()
        });
    }
    assert!(nav_turns >= 2, "会话 B 应有 ≥2 轮用户消息");
    assert!(
        nav_pane_w >= 720.,
        "消息面板宽 {nav_pane_w} 应 ≥720（导航条断点）"
    );
    println!("[selftest] turn 导航条可见条件 OK（{nav_turns} 轮，面板宽 {nav_pane_w:.0}）");

    // 贴底时活动项应为最后一轮用户消息（回归：曾按「离视口顶最近」在贴底时
    // 高亮到更早轮次——底部视口里多条用户消息同时可见，离顶最近的偏早）
    let mut waited = 0u64;
    loop {
        timer!(200).await;
        waited += 200;
        let (active, offset_y, max_offset_y, view_h, user_rows) = app!(|app: &mut AppView, cx| {
            let views = app.views.get(&session_b).expect("B 视图在内存");
            views.thread.read(cx).debug_nav_active_detail()
        });
        let last_ix = user_rows.last().map(|(ix, _, _)| *ix);
        if last_ix.is_some() && active == last_ix {
            break;
        }
        assert!(
            waited < 10_000,
            "贴底时活动项应为最后一轮: active={active:?} last={last_ix:?} \
             offset_y={offset_y:.1} max_offset_y={max_offset_y:.1} 视口高={view_h:.1} \
             用户行={user_rows:?}"
        );
    }
    println!("[selftest] turn 导航条活动项 OK（贴底 = 最后一轮）");

    // 切回 A：内存状态应原样保留
    app!(|app: &mut AppView, cx| app.switch_session(session_a.clone(), cx));
    let current = app!(|app: &mut AppView, _| app.current.clone());
    assert_eq!(current.as_ref(), Some(&session_a));
    let kept = app!(|app: &mut AppView, cx| {
        let views = app.views.get(&session_a).expect("A 视图在内存");
        let (_, text, _, _) = views.thread.read(cx).debug_last_assistant();
        text.contains(pig_core::mock::SCENARIO_B_MARKER)
    });
    assert!(kept, "切回 A 后内容应保留");
    println!("[selftest] 会话切换 OK");

    // 模拟重启：drop manager 重 spawn + ListSessions/OpenSession 重放
    app!(|app: &mut AppView, cx| app.restart_agent(cx));
    // 等自动打开最近会话（B，updated_at 最新），再显式切到 A 触发重放
    let mut waited = 0u64;
    loop {
        timer!(200).await;
        waited += 200;
        assert!(waited < 10_000, "重启后自动打开会话超时");
        let ready = app!(|app: &mut AppView, _| app.current.is_some());
        if ready {
            break;
        }
    }
    app!(|app: &mut AppView, cx| app.switch_session(session_a.clone(), cx));
    let mut waited = 0u64;
    loop {
        timer!(200).await;
        waited += 200;
        assert!(waited < 30_000, "重启后重放超时");
        let done = app!(|app: &mut AppView, cx| {
            let views = app.views.get(&session_a)?;
            let (tool_done, text, _, _) = views.thread.read(cx).debug_last_assistant();
            let review_state = views.review.read(cx).debug_state();
            (tool_done
                && text.contains(pig_core::mock::SCENARIO_B_MARKER)
                && review_state.0 == 1
                && review_state.1 == 3)
                .then_some(())
        });
        if done == Some(()) {
            break;
        }
    }
    println!("[selftest] 模拟重启 resume OK（消息+工具卡+diff 全恢复）");

    // @搜索：真实文件
    app!(|app: &mut AppView, _| {
        app.agent
            .search_files(session_a.clone(), "hello".to_string());
    });
    let mut waited = 0u64;
    loop {
        timer!(200).await;
        waited += 200;
        assert!(waited < 10_000, "@搜索超时");
        let found = app!(|app: &mut AppView, cx| {
            app.composer
                .read(cx)
                .debug_mention_results()
                .iter()
                .any(|r| r.contains("hello.txt"))
        });
        if found {
            break;
        }
    }
    println!("[selftest] @搜索 OK");

    // compact（模型摘要）
    app!(|app: &mut AppView, _| app.agent.compact(session_a.clone()));
    let mut waited = 0u64;
    loop {
        timer!(200).await;
        waited += 200;
        assert!(waited < 15_000, "compact 超时");
        let compacted = app!(|app: &mut AppView, cx| {
            app.views
                .get(&session_a)
                .map(|views| views.thread.read(cx).debug_system_notes())
                .unwrap_or_default()
                .iter()
                .any(|note| {
                    note.contains("模型摘要") && note.contains(pig_core::mock::SUMMARY_MARKER)
                })
        });
        if compacted {
            break;
        }
    }
    println!("[selftest] 模型摘要 compact OK");

    // 场景 C：计划模式闭环
    app!(|app: &mut AppView, _| app
        .agent
        .new_session(app.cwd.clone(), None, None, None, None));
    let session_c = loop {
        timer!(200).await;
        let current = app!(|app: &mut AppView, _| app.current.clone());
        if let Some(id) = current
            && id != session_a
        {
            break id;
        }
    };
    app!(|app: &mut AppView, _| {
        app.exec_mode = pig_protocol::ExecMode::Plan;
        app.agent
            .set_exec_mode(session_c.clone(), pig_protocol::ExecMode::Plan);
        app.agent.send_message(
            session_c.clone(),
            format!("{} 给我一个改造计划", pig_core::mock::SCENARIO_C_TRIGGER),
            vec![],
            vec![],
            pig_protocol::ExecMode::Plan,
        );
    });
    let mut waited = 0u64;
    loop {
        timer!(200).await;
        waited += 200;
        assert!(waited < 30_000, "场景 C 计划超时");
        let ready = app!(|app: &mut AppView, cx| {
            let views = app.views.get(&session_c)?;
            let thread = views.thread.read(cx);
            let (_, text, _, _) = thread.debug_last_assistant();
            (thread.is_plan_pending() && text.contains(pig_core::mock::PLAN_MARKER)).then_some(())
        });
        if ready == Some(()) {
            break;
        }
    }
    println!("[selftest] 计划模式输出计划，执行计划按钮出现");

    // 点「执行计划」（走与按钮相同路径）
    app!(|app: &mut AppView, cx| {
        let views = app.views.get(&session_c).expect("C 视图");
        views
            .thread
            .update(cx, |thread, cx| thread.trigger_execute_plan(cx));
    });
    let mode = app!(|app: &mut AppView, cx| app.composer.read(cx).debug_exec_mode());
    assert_eq!(
        mode,
        pig_protocol::ExecMode::ConfirmBeforeEdit,
        "模式应切到变更前确认"
    );

    // 场景 B 工具链执行（ConfirmBeforeEdit → 3 次审批）
    let mut approvals = 0u32;
    let mut waited = 0u64;
    loop {
        timer!(200).await;
        waited += 200;
        assert!(waited < 60_000, "场景 C 执行超时");
        let approved = app!(|app: &mut AppView, cx| {
            let views = app.views.get(&session_c)?;
            views.thread.read(cx).pending_approval().map(|_| {
                views.thread.update(cx, |thread, cx| {
                    thread.decide_pending(pig_protocol::ApprovalDecision::Allow, cx);
                });
            })
        });
        if approved == Some(()) {
            approvals += 1;
            continue;
        }
        let done = app!(|app: &mut AppView, cx| {
            let views = app.views.get(&session_c)?;
            let thread = views.thread.read(cx);
            let (tool_done, text, _, _) = thread.debug_last_assistant();
            (!thread.is_streaming()
                && waited > 1000
                && tool_done
                && text.contains(pig_core::mock::SCENARIO_B_MARKER))
            .then_some(())
        });
        if done == Some(()) {
            break;
        }
    }
    assert_eq!(approvals, 3, "执行计划后场景 B 三次审批");
    println!("[selftest] 计划确认 → 模式切换 → 工具链执行 OK");

    // 水位条
    let usage = app!(|app: &mut AppView, cx| app.composer.read(cx).debug_context_usage());
    assert_eq!(usage, Some((142, 128_000)), "水位条数据: {usage:?}");
    println!("[selftest] 上下文水位条 OK");

    // 设置页数据：ConfigSnapshot 已收到、composer 模型列表已填充
    let (provider_count, model_count) = app!(|app: &mut AppView, cx| {
        (
            app.debug_config().map(|c| c.providers.len()).unwrap_or(0),
            app.composer.read(cx).debug_model_count(),
        )
    });
    assert_eq!(provider_count, 2, "ConfigSnapshot 应含 2 个供应商");
    assert_eq!(model_count, 2, "composer 应列出 2 个模型");
    println!("[selftest] ConfigSnapshot + 模型列表 OK");

    // Anthropic 供应商端到端：会话 D 切到 anthropic 模型跑场景 B
    app!(|app: &mut AppView, _| app
        .agent
        .new_session(app.cwd.clone(), None, None, None, None));
    let session_d = loop {
        timer!(200).await;
        let current = app!(|app: &mut AppView, _| app.current.clone());
        if let Some(id) = current
            && id != session_a
            && id != session_b
        {
            break id;
        }
    };
    app!(|app: &mut AppView, _| {
        app.agent.set_model(
            session_d.clone(),
            "anthropic".to_string(),
            "mock-model".to_string(),
            None,
        );
        app.agent.send_message(
            session_d.clone(),
            format!(
                "{} 创建并修改文件，然后跑个命令",
                pig_core::mock::SCENARIO_B_TRIGGER
            ),
            vec![],
            vec![],
            pig_protocol::ExecMode::AutoEdit,
        );
    });
    let mut approvals = 0u32;
    let mut waited = 0u64;
    loop {
        timer!(200).await;
        waited += 200;
        assert!(waited < 60_000, "Anthropic 场景 B 超时");
        let approved = app!(|app: &mut AppView, cx| {
            let views = app.views.get(&session_d)?;
            views.thread.read(cx).pending_approval().map(|_| {
                views.thread.update(cx, |thread, cx| {
                    thread.decide_pending(pig_protocol::ApprovalDecision::Allow, cx);
                });
            })
        });
        if approved == Some(()) {
            approvals += 1;
            continue;
        }
        let done = app!(|app: &mut AppView, cx| {
            let views = app.views.get(&session_d)?;
            let thread = views.thread.read(cx);
            let (tool_done, text, thinking, _) = thread.debug_last_assistant();
            let _ = thinking;
            (!thread.is_streaming()
                && waited > 1000
                && tool_done
                && text.contains(pig_core::mock::SCENARIO_B_MARKER))
            .then_some(())
        });
        if done == Some(()) {
            break;
        }
    }
    assert_eq!(approvals, 1, "AutoEdit 下仅 Bash 审批");
    println!("[selftest] Anthropic 供应商端到端 OK");

    // AskUserQuestion：会话 E 走 SCENARIO_Q → 问题条出现 → 选选项 → 提交 → marker + 工具卡
    app!(|app: &mut AppView, _| app
        .agent
        .new_session(app.cwd.clone(), None, None, None, None));
    let session_e = loop {
        timer!(200).await;
        let current = app!(|app: &mut AppView, _| app.current.clone());
        if let Some(id) = current
            && id != session_a
            && id != session_b
            && id != session_c
            && id != session_d
        {
            break id;
        }
    };
    app!(|app: &mut AppView, _| {
        app.agent.send_message(
            session_e.clone(),
            format!("{} 帮我决定实现方案", pig_core::mock::SCENARIO_Q_TRIGGER),
            vec![],
            vec![],
            pig_protocol::ExecMode::AutoEdit,
        );
    });
    // 等问题条出现（问题与审批互斥，问题优先，AutoEdit 下 AskUserQuestion 免审批）
    let mut waited = 0u64;
    loop {
        timer!(200).await;
        waited += 200;
        assert!(waited < 30_000, "问题条出现超时");
        let has =
            app!(|app: &mut AppView, cx| { app.composer.read(cx).debug_question().is_some() });
        if has {
            break;
        }
    }
    println!("[selftest] AskUserQuestion 问题条出现 OK");
    // 向导分页：第 1 题选「方案 A」→ 下一题 → 第 2 题选「要」→ 提交
    let q1 = app!(|app: &mut AppView, cx| app.composer.read(cx).debug_question());
    assert_eq!(q1.as_deref(), Some("选择实现方案"), "首题题干: {q1:?}");
    app!(|app: &mut AppView, cx| {
        app.composer.update(cx, |composer, cx| {
            composer.debug_select_question_option(0, 0, cx);
            composer.debug_next_question_page(cx);
        });
    });
    let q2 = app!(|app: &mut AppView, cx| app.composer.read(cx).debug_question());
    assert_eq!(
        q2.as_deref(),
        Some("需要跑测试吗"),
        "翻页后应显示第 2 题: {q2:?}"
    );
    println!("[selftest] AskUserQuestion 翻页 OK");
    app!(|app: &mut AppView, cx| {
        app.composer.update(cx, |composer, cx| {
            composer.debug_select_question_option(1, 0, cx);
            composer.debug_submit_question(cx);
        });
    });
    let mut waited = 0u64;
    loop {
        timer!(200).await;
        waited += 200;
        assert!(waited < 30_000, "AskUserQuestion 回合超时");
        let done = app!(|app: &mut AppView, cx| {
            let views = app.views.get(&session_e)?;
            let thread = views.thread.read(cx);
            let (tool_done, text, _, tool_output) = thread.debug_last_assistant();
            if !thread.is_streaming() && waited > 1000 && tool_done {
                assert!(
                    text.contains(pig_core::mock::MOCK_Q_MARKER),
                    "E 文本标记: {text}"
                );
                return Some(tool_output);
            }
            None
        });
        if let Some(tool_output) = done {
            assert!(
                tool_output.contains("方案 A"),
                "工具输出应含第 1 题答案: {tool_output}"
            );
            assert!(
                tool_output.contains("需要跑测试吗：要"),
                "工具输出应含第 2 题答案: {tool_output}"
            );
            break;
        }
    }
    let has_card = app!(|app: &mut AppView, cx| {
        let views = app.views.get(&session_e)?;
        Some(views.thread.read(cx).debug_has_tool_call("AskUserQuestion"))
    });
    assert_eq!(has_card, Some(true), "工具卡应显示 AskUserQuestion");
    println!("[selftest] AskUserQuestion 提问场景 OK");

    // 右侧面板：默认收起 → 面板按钮直开（无 tab 时显示菜单页）→ 开改动 tab →
    // 快捷键再触发收起（tab 保留）→ × 关尽 tab 后面板自动收起
    let right_initial = app!(|app: &mut AppView, _| app.right_open);
    assert!(!right_initial, "右侧面板默认应收起");
    app!(|app: &mut AppView, cx| app.toggle_right_panel(cx));
    let (open, active) = app!(|app: &mut AppView, _| (app.right_open, app.right_active.clone()));
    assert!(open && active.is_none(), "面板展开且无 tab 时应显示菜单页");
    app!(|app: &mut AppView, cx| app.open_right_tab(RightTab::Changes, cx));
    let (open, active) = app!(|app: &mut AppView, _| (app.right_open, app.right_active.clone()));
    assert!(open && active == Some(RightTab::Changes), "改动 tab 应打开");
    app!(|app: &mut AppView, cx| app.toggle_right_tab(RightTab::Changes, cx));
    let (open, kept) = app!(|app: &mut AppView, _| {
        (app.right_open, app.right_active == Some(RightTab::Changes))
    });
    assert!(!open && kept, "再次触发应收起面板并保留 tab");
    app!(|app: &mut AppView, cx| app.close_right_tab(RightTab::Changes, cx));
    let (open, active, tabs) = app!(|app: &mut AppView, _| {
        (
            app.right_open,
            app.right_active.clone(),
            app.right_tabs.len(),
        )
    });
    assert!(
        !open && active.is_none() && tabs == 0,
        "面板收起状态下关 tab 不改变收起状态；tab 清空"
    );
    // 面板展开时关掉最后一个 tab：面板自动收起
    app!(|app: &mut AppView, cx| {
        app.open_right_tab(RightTab::Changes, cx);
        app.close_right_tab(RightTab::Changes, cx);
    });
    let (open, active) = app!(|app: &mut AppView, _| (app.right_open, app.right_active.clone()));
    assert!(
        !open && active.is_none(),
        "关尽最后一个 tab 后面板应自动收起"
    );
    app!(|app: &mut AppView, cx| app.toggle_right_panel(cx));
    println!("[selftest] 右侧面板开合 OK");

    // 加面板菜单（标签页栏 "+"）：点开打开、再点收起
    app!(|app: &mut AppView, cx| {
        app.toggle_right_menu(&ClickEvent::default(), cx);
    });
    let menu_open = app!(|app: &mut AppView, _| app.right_menu_open);
    assert!(menu_open, "菜单应打开");
    app!(|app: &mut AppView, cx| {
        app.toggle_right_menu(&ClickEvent::default(), cx);
    });
    let menu_closed = app!(|app: &mut AppView, _| !app.right_menu_open);
    assert!(menu_closed, "再点应收起菜单");
    println!("[selftest] 右侧面板菜单 OK");

    // 改动 chip：点击改为直接打开右侧改动面板（不再弹层）
    app!(|app: &mut AppView, cx| {
        app.composer
            .update(cx, |_, cx| cx.emit(ComposerEvent::OpenChanges));
    });
    let (open, active) = app!(|app: &mut AppView, _| (app.right_open, app.right_active.clone()));
    assert!(
        open && active == Some(RightTab::Changes),
        "改动 chip 应打开右侧面板并激活改动 tab"
    );
    println!("[selftest] 改动 chip → 右侧改动面板 OK");

    // 会话管理：首条消息自动命名 → 手动重命名 → 删除
    app!(|app: &mut AppView, _| app
        .agent
        .new_session(app.cwd.clone(), None, None, None, None));
    let session_f = loop {
        timer!(200).await;
        let current = app!(|app: &mut AppView, _| app.current.clone());
        if let Some(id) = current
            && id != session_a
            && id != session_b
            && id != session_c
            && id != session_d
            && id != session_e
        {
            break id;
        }
    };
    // 首条消息（≥10 字）触发自动命名 sidecar；mock 回 {"title": MOCK_TITLE}
    app!(|app: &mut AppView, _| {
        app.agent.send_message(
            session_f.clone(),
            "帮我梳理这个项目的模块结构并给出重构建议".to_string(),
            vec![],
            vec![],
            pig_protocol::ExecMode::AutoEdit,
        );
    });
    let mut waited = 0u64;
    loop {
        timer!(200).await;
        waited += 200;
        assert!(waited < 30_000, "自动命名超时");
        let done = app!(|app: &mut AppView, cx| {
            let Some(views) = app.views.get(&session_f) else {
                return false;
            };
            let thread = views.thread.read(cx);
            let (_, text, _, _) = thread.debug_last_assistant();
            let title = app
                .metas
                .iter()
                .find(|m| m.id == session_f)
                .map(|m| m.title.clone());
            !thread.is_streaming()
                && waited > 1000
                && !text.is_empty()
                && title.as_deref() == Some(pig_core::mock::MOCK_TITLE)
        });
        if done {
            break;
        }
    }
    println!("[selftest] 首条消息自动命名 OK（mock 标题替换 30 字符种子）");

    // 手动重命名：core 落库（title_custom）后 SessionList 全量刷新回来仍是新名，
    // 才算真正持久化（本地补丁只管即时显示）
    app!(|app: &mut AppView, cx| app.rename_session(&session_f, "手动改名F", cx));
    let mut waited = 0u64;
    loop {
        timer!(200).await;
        waited += 200;
        assert!(waited < 10_000, "重命名持久化超时");
        let done = app!(|app: &mut AppView, _| {
            app.metas
                .iter()
                .find(|m| m.id == session_f)
                .map(|m| m.title.as_str())
                == Some("手动改名F")
        });
        if done {
            break;
        }
    }
    println!("[selftest] 会话手动重命名 OK");

    // 删除会话：视图/列表/rollout 文件全清理；删当前会话自动切走
    let data_dir =
        std::path::PathBuf::from(std::env::var("PIG_DATA_DIR").expect("selftest 数据目录"));
    let jsonl = data_dir.join("sessions").join(format!("{session_f}.jsonl"));
    assert!(jsonl.exists(), "删除前 rollout 应存在: {}", jsonl.display());
    app!(|app: &mut AppView, cx| app.delete_session(&session_f, cx));
    let mut waited = 0u64;
    loop {
        timer!(200).await;
        waited += 200;
        assert!(waited < 10_000, "删除会话超时");
        let gone = app!(|app: &mut AppView, _| {
            !app.metas.iter().any(|m| m.id == session_f) && !app.views.contains_key(&session_f)
        });
        if gone && !jsonl.exists() {
            break;
        }
    }
    assert!(
        app!(|app: &mut AppView, _| app.current.clone()) != Some(session_f),
        "删除当前会话后应切走"
    );
    println!("[selftest] 会话删除 OK（视图+列表+rollout 全清理）");

    // ---- 新会话模型选择不被工作区种子冲掉（回归：曾「切模型→选工作区→发送」
    // 被 apply_hero_defaults 用工作区旧模型覆盖）----
    // 前置：显式用 mock 建一个会话并完成回合，成为工作区最新种子
    app!(|app: &mut AppView, _| app.agent.new_session(
        app.cwd.clone(),
        Some("mock".to_string()),
        Some("mock-model".to_string()),
        None,
        None,
    ));
    let known: Vec<String> =
        app!(|app: &mut AppView, _| { app.metas.iter().map(|m| m.id.clone()).collect() });
    let seed_id = loop {
        timer!(200).await;
        let found = app!(|app: &mut AppView, _| {
            let Some(sid) = &app.current else { return None };
            (!known.contains(sid)).then(|| sid.clone())
        });
        if let Some(id) = found {
            break id;
        }
    };
    app!(|app: &mut AppView, _| {
        app.agent.send_message(
            seed_id.clone(),
            "种子会话打个卡".to_string(),
            vec![],
            vec![],
            pig_protocol::ExecMode::AutoEdit,
        );
    });
    let mut waited = 0u64;
    loop {
        timer!(200).await;
        waited += 200;
        assert!(waited < 30_000, "模型种子会话超时");
        let done = app!(|app: &mut AppView, cx| {
            let Some(views) = app.views.get(&seed_id) else {
                return false;
            };
            !views.thread.read(cx).is_streaming() && waited > 1000
        });
        if done {
            break;
        }
    }

    // 前置断言：未显式选模型时，hero 默认值来自工作区种子（此时最新 = 刚建的 mock 会话）
    app!(|app: &mut AppView, cx| app.enter_hero(cx));
    timer!(400).await;
    let seeded = app!(|app: &mut AppView, _| app.current_model.clone());
    assert_eq!(
        seeded,
        Some(("mock".to_string(), "mock-model".to_string())),
        "未选模型时 hero 默认值应来自工作区种子: {seeded:?}"
    );

    // 变体 2：hero → 切 anthropic → 再选工作区（触发 apply_hero_defaults）→ 发送
    app!(|app: &mut AppView, cx| app.enter_hero(cx));
    app!(|app: &mut AppView, cx| {
        app.composer.update(cx, |_, cx| {
            cx.emit(ComposerEvent::SetModel {
                provider_id: "anthropic".to_string(),
                model_id: "mock-model".to_string(),
            });
        });
    });
    timer!(400).await;
    let cwd_str = app!(|app: &mut AppView, _| app.cwd.display().to_string());
    app!(|app: &mut AppView, cx| {
        app.composer.update(cx, |_, cx| {
            cx.emit(ComposerEvent::SelectCwd(cwd_str.clone()));
        });
    });
    timer!(400).await;
    let picked = app!(|app: &mut AppView, _| (app.current_model.clone(), app.hero_cwd.is_some()));
    assert_eq!(
        picked,
        (
            Some(("anthropic".to_string(), "mock-model".to_string())),
            true
        ),
        "选工作区后用户已选的模型不应被种子覆盖: {picked:?}"
    );
    app!(|app: &mut AppView, cx| {
        app.hero_send(
            "模型选择回归 v2".to_string(),
            vec![],
            vec![],
            pig_protocol::ExecMode::AutoEdit,
            cx,
        );
    });
    let mut waited = 0u64;
    let v2_id = loop {
        timer!(200).await;
        waited += 200;
        assert!(waited < 20_000, "v2 会话建立超时");
        let found = app!(|app: &mut AppView, cx| {
            let Some(sid) = &app.current else { return None };
            if known.contains(sid) {
                return None;
            }
            let streaming = app
                .views
                .get(sid)
                .map(|v| v.thread.read(cx).is_streaming())
                .unwrap_or(true);
            (!streaming).then(|| sid.clone())
        });
        if let Some(id) = found {
            break id;
        }
    };
    let v2 = app!(|app: &mut AppView, _| {
        app.metas
            .iter()
            .find(|m| m.id == v2_id)
            .map(|m| (m.provider_id.clone(), m.model_id.clone()))
    });
    assert_eq!(
        v2,
        Some((
            Some("anthropic".to_string()),
            Some("mock-model".to_string())
        )),
        "切模型→选工作区→发送：新会话应使用用户选择的模型: {v2:?}"
    );
    println!("[selftest] hero 切模型后选工作区，模型选择保留 OK");

    // 变体 1：hero → 切 anthropic → 直接发送（无工作区选择）
    app!(|app: &mut AppView, cx| app.enter_hero(cx));
    app!(|app: &mut AppView, cx| {
        app.composer.update(cx, |_, cx| {
            cx.emit(ComposerEvent::SetModel {
                provider_id: "anthropic".to_string(),
                model_id: "mock-model".to_string(),
            });
        });
    });
    timer!(400).await;
    app!(|app: &mut AppView, cx| {
        app.hero_send(
            "模型选择回归 v1".to_string(),
            vec![],
            vec![],
            pig_protocol::ExecMode::AutoEdit,
            cx,
        );
    });
    let mut waited = 0u64;
    let v1_id = loop {
        timer!(200).await;
        waited += 200;
        assert!(waited < 20_000, "v1 会话建立超时");
        let found = app!(|app: &mut AppView, cx| {
            let Some(sid) = &app.current else { return None };
            if sid == &v2_id || known.contains(sid) {
                return None;
            }
            let streaming = app
                .views
                .get(sid)
                .map(|v| v.thread.read(cx).is_streaming())
                .unwrap_or(true);
            (!streaming).then(|| sid.clone())
        });
        if let Some(id) = found {
            break id;
        }
    };
    let v1 = app!(|app: &mut AppView, _| {
        app.metas
            .iter()
            .find(|m| m.id == v1_id)
            .map(|m| (m.provider_id.clone(), m.model_id.clone()))
    });
    assert_eq!(
        v1,
        Some((
            Some("anthropic".to_string()),
            Some("mock-model".to_string())
        )),
        "hero 切模型直接发送：新会话应使用用户选择的模型: {v1:?}"
    );
    println!("[selftest] 新会话模型选择（用户选择优先/种子兜底）OK");

    // 变体 3（用户实际流程）：工作区行点 +（NewTaskInWorkspace，预设 cwd 进
    // hero）→ 切模型 → 发送。
    // 前置：再显式建一个 mock 会话并完成回合——v1/v2 的 anthropic 会话已成为
    // 工作区最新种子，会与用户选择同为 anthropic，断言无法区分「保留选择」
    // 与「还原成种子」，必须把种子刷回 mock
    app!(|app: &mut AppView, _| app.agent.new_session(
        app.cwd.clone(),
        Some("mock".to_string()),
        Some("mock-model".to_string()),
        None,
        None,
    ));
    let known3: Vec<String> =
        app!(|app: &mut AppView, _| { app.metas.iter().map(|m| m.id.clone()).collect() });
    let seed3_id = loop {
        timer!(200).await;
        let found = app!(|app: &mut AppView, _| {
            let Some(sid) = &app.current else { return None };
            (!known3.contains(sid)).then(|| sid.clone())
        });
        if let Some(id) = found {
            break id;
        }
    };
    app!(|app: &mut AppView, _| {
        app.agent.send_message(
            seed3_id.clone(),
            "v3 前置种子会话".to_string(),
            vec![],
            vec![],
            pig_protocol::ExecMode::AutoEdit,
        );
    });
    let mut waited = 0u64;
    loop {
        timer!(200).await;
        waited += 200;
        assert!(waited < 30_000, "v3 种子会话超时");
        let done = app!(|app: &mut AppView, cx| {
            let Some(views) = app.views.get(&seed3_id) else {
                return false;
            };
            !views.thread.read(cx).is_streaming() && waited > 1000
        });
        if done {
            break;
        }
    }
    // 确认种子生效：进 hero 后默认模型应是 mock（工作区最新）
    app!(|app: &mut AppView, cx| {
        // SidebarEvent::NewTaskInWorkspace 的 handler 本体（直调需要 window）
        app.hero_cwd = Some(app.cwd.clone());
        app.enter_hero(cx);
    });
    timer!(400).await;
    let seeded3 = app!(|app: &mut AppView, _| app.current_model.clone());
    assert_eq!(
        seeded3,
        Some(("mock".to_string(), "mock-model".to_string())),
        "v3 前置：工作区种子应为 mock: {seeded3:?}"
    );
    // 切 anthropic → 发送
    app!(|app: &mut AppView, cx| {
        app.composer.update(cx, |_, cx| {
            cx.emit(ComposerEvent::SetModel {
                provider_id: "anthropic".to_string(),
                model_id: "mock-model".to_string(),
            });
        });
    });
    timer!(400).await;
    app!(|app: &mut AppView, cx| {
        app.hero_send(
            "模型选择回归 v3".to_string(),
            vec![],
            vec![],
            pig_protocol::ExecMode::AutoEdit,
            cx,
        );
    });
    let mut waited = 0u64;
    let v3_id = loop {
        timer!(200).await;
        waited += 200;
        assert!(waited < 20_000, "v3 会话建立超时");
        let found = app!(|app: &mut AppView, cx| {
            let Some(sid) = &app.current else { return None };
            if sid == &v1_id || sid == &v2_id || known.contains(sid) || sid == &seed3_id {
                return None;
            }
            let streaming = app
                .views
                .get(sid)
                .map(|v| v.thread.read(cx).is_streaming())
                .unwrap_or(true);
            (!streaming).then(|| sid.clone())
        });
        if let Some(id) = found {
            break id;
        }
    };
    let v3 = app!(|app: &mut AppView, _| {
        app.metas
            .iter()
            .find(|m| m.id == v3_id)
            .map(|m| (m.provider_id.clone(), m.model_id.clone()))
    });
    assert_eq!(
        v3,
        Some((
            Some("anthropic".to_string()),
            Some("mock-model".to_string())
        )),
        "工作区点+→切模型→发送：新会话应使用用户选择的模型: {v3:?}"
    );
    println!("[selftest] 工作区点+新建后切模型，模型选择保留 OK");

    // 切模型时思考等级落点（优先级）：模型默认档 > 继承（需新模型支持）
    // > 继承档失效时启发式。selftest 配置：mock 有默认 low，anthropic 无默认
    // 1) 默认档优先：当前 high（mock 也支持 high）→ 切 mock 仍落到默认 low
    app!(|app: &mut AppView, cx| {
        app.composer.update(cx, |_, cx| {
            cx.emit(ComposerEvent::SetReasoning(Some("high".to_string())));
        });
    });
    timer!(200).await;
    let had_level = app!(|app: &mut AppView, _| app.reasoning_level.clone());
    assert_eq!(had_level, Some("high".to_string()), "前置：等级应为 high");
    app!(|app: &mut AppView, cx| {
        app.composer.update(cx, |_, cx| {
            cx.emit(ComposerEvent::SetModel {
                provider_id: "mock".to_string(),
                model_id: "mock-model".to_string(),
            });
        });
    });
    timer!(200).await;
    let (level, meta_level) = app!(|app: &mut AppView, _| {
        let meta_level = app
            .metas
            .iter()
            .find(|m| m.id == v3_id)
            .and_then(|m| m.reasoning_level.clone());
        (app.reasoning_level.clone(), meta_level)
    });
    assert_eq!(
        level,
        Some("low".to_string()),
        "模型默认档应优先于可继承的等级: level={level:?}"
    );
    assert_eq!(
        meta_level,
        Some("low".to_string()),
        "落点等级应写穿 meta: meta_level={meta_level:?}"
    );

    // 2) 未配默认 → 继承：anthropic 无默认，max 在其等级表内 → 切过去保持 max
    app!(|app: &mut AppView, cx| {
        app.composer.update(cx, |_, cx| {
            cx.emit(ComposerEvent::SetReasoning(Some("max".to_string())));
        });
    });
    app!(|app: &mut AppView, cx| {
        app.composer.update(cx, |_, cx| {
            cx.emit(ComposerEvent::SetModel {
                provider_id: "anthropic".to_string(),
                model_id: "mock-model".to_string(),
            });
        });
    });
    timer!(200).await;
    let level = app!(|app: &mut AppView, _| app.reasoning_level.clone());
    assert_eq!(
        level,
        Some("max".to_string()),
        "未配默认且等级被支持时应继承: level={level:?}"
    );
    println!("[selftest] 切模型思考等级落点（默认档优先/继承）OK");

    // 子代理场景（A3）：前台 Agent 卡——运行中出现进度行、收尾后原摘要保留；
    // 随后后台子代理完成 → 合成 <task-notification> 用户消息到达（通知卡渲染路径）
    let before_current = app!(|app: &mut AppView, _| app.current.clone());
    app!(|app: &mut AppView, _| app
        .agent
        .new_session(app.cwd.clone(), None, None, None, None));
    let session_c = loop {
        timer!(200).await;
        let current = app!(|app: &mut AppView, _| app.current.clone());
        if let Some(cur) = current
            && Some(&cur) != before_current.as_ref()
        {
            break cur;
        }
    };
    app!(|app: &mut AppView, _| {
        // 钉到 mock（OpenAI 格式）供应商：前面的切模型测试把当前选择留在了
        // anthropic，而 mock 的子代理场景只有 OpenAI 格式分支
        app.agent.set_model(
            session_c.clone(),
            "mock".to_string(),
            "mock-model".to_string(),
            None,
        );
        app.agent.send_message(
            session_c.clone(),
            format!("{} GREP", pig_core::mock::SUBAGENT_TRIGGER),
            vec![],
            vec![],
            pig_protocol::ExecMode::AutoEdit,
        );
    });
    let mut saw_progress = false;
    let mut waited = 0u64;
    loop {
        timer!(100).await;
        waited += 100;
        assert!(waited < 60_000, "前台子代理场景超时");
        let state = app!(|app: &mut AppView, cx| {
            let views = app.views.get(&session_c)?;
            let thread = views.thread.read(cx);
            let (_, _, _, tool_output) = thread.debug_last_assistant();
            Some((
                thread.is_streaming(),
                thread.debug_agent_card(),
                tool_output,
            ))
        });
        let Some((streaming, card, tool_output)) = state else {
            continue;
        };
        let Some((summary, live_note, done)) = card else {
            continue;
        };
        saw_progress |= !done && live_note.is_some();
        if done && !streaming {
            assert!(saw_progress, "运行中应出现过进度行（SubagentProgress）");
            assert!(
                summary.contains("子代理 explore"),
                "收尾后原摘要应保留（不被进度覆盖）: {summary}"
            );
            assert!(live_note.is_none(), "收尾后进度行应清空: {live_note:?}");
            assert!(
                tool_output.contains(pig_core::mock::SUBAGENT_CHILD_DONE),
                "Agent 卡输出应含子代理结论: {tool_output}"
            );
            // A3c：SubagentCard 事件已把代理卡元信息写到卡片上（agent_id + 副标题）
            let card_meta = app!(|app: &mut AppView, cx| {
                let views = app.views.get(&session_c)?;
                views.thread.read(cx).debug_agent_card_meta()
            });
            let Some((card_agent_id, card_subtitle)) = card_meta else {
                panic!("Agent 卡应有代理卡元信息（SubagentCard 事件）");
            };
            assert!(!card_agent_id.is_empty(), "代理卡 agent_id 应非空");
            assert!(
                card_subtitle.contains("explore"),
                "代理卡副标题应含 profile: {card_subtitle}"
            );
            break;
        }
    }
    println!("[selftest] 子代理前台卡片（进度行出现/原摘要保留/收尾清行/代理卡元信息）OK");

    // 同会话发后台子代理：running 回执收尾 → 子代理完成后 core 注入通知并唤醒收尾
    app!(|app: &mut AppView, _| {
        app.agent.send_message(
            session_c.clone(),
            format!("{} BG", pig_core::mock::SUBAGENT_TRIGGER),
            vec![],
            vec![],
            pig_protocol::ExecMode::AutoEdit,
        );
    });
    let mut waited = 0u64;
    loop {
        timer!(200).await;
        waited += 200;
        assert!(waited < 60_000, "后台子代理通知超时");
        let (streaming, has_notification) = app!(|app: &mut AppView, cx| {
            let Some(views) = app.views.get(&session_c) else {
                return (true, false);
            };
            let thread = views.thread.read(cx);
            (thread.is_streaming(), thread.debug_has_task_notification())
        });
        if has_notification && !streaming {
            break;
        }
    }
    println!("[selftest] 后台子代理完成 → task-notification 合成消息到达 OK");

    // A3b：通知开标签的结构化 meta 解析（气泡渲染数据源）
    let (agent_id, title, duration_ms, record, result) = {
        let mut waited = 0u64;
        loop {
            timer!(200).await;
            waited += 200;
            assert!(waited < 10_000, "通知 meta 解析超时");
            let found = app!(|app: &mut AppView, cx| {
                let views = app.views.get(&session_c)?;
                views.thread.read(cx).debug_task_notification_meta()
            });
            if let Some(meta) = found {
                break meta;
            }
        }
    };
    assert!(
        title.contains("子代理自测委派"),
        "通知标题应为 description: {title}"
    );
    // A3c/A3d：耗时、上下文记录路径与结果文件路径属性
    assert!(duration_ms.is_some(), "通知应带 duration_ms");
    let record = record.expect("通知应带 record 记录路径");
    assert!(
        record.contains(".agents/") || record.contains(".agents\\"),
        "record 应为子代理上下文 JSONL: {record}"
    );
    let result_path = result.expect("通知应带 result 结果路径");
    assert!(
        result_path.ends_with(".result.md"),
        "result 应为结果全文文件: {result_path}"
    );
    println!("[selftest] 通知气泡结构化 meta 解析 OK（{agent_id} · {title} · {duration_ms:?}ms）");

    // 走通知卡点击的同一路径开「子代理」tab → Op::LoadSubagent → SubagentHistory
    app!(|app: &mut AppView, cx| {
        app.open_subagent_tab(session_c.clone(), agent_id.clone(), title.clone(), cx);
    });
    let mut waited = 0u64;
    loop {
        timer!(200).await;
        waited += 200;
        assert!(waited < 15_000, "子代理历史加载超时");
        let state = app!(|app: &mut AppView, cx| app.debug_subagent_tab(cx));
        if let Some((tab_title, items)) = state {
            assert!(
                items >= 3,
                "子代理对话应有 user/tool/assistant 各至少一条: items={items}"
            );
            assert!(
                tab_title.contains("子代理自测委派"),
                "tab 标题应为 meta.description: {tab_title}"
            );
            break;
        }
    }
    println!("[selftest] 子代理 tab（点击开面板 + 历史加载 ≥3 行）OK");

    // A3d：面板实时输出——新会话发 BG，后台子代理运行中经代理卡同路径开 tab：
    // running 指示出现 → SubagentActivity 增量追加 → finished 后 running 消失
    let before_current = app!(|app: &mut AppView, _| app.current.clone());
    app!(|app: &mut AppView, _| app
        .agent
        .new_session(app.cwd.clone(), None, None, None, None));
    let session_d = loop {
        timer!(200).await;
        let current = app!(|app: &mut AppView, _| app.current.clone());
        if let Some(cur) = current
            && Some(&cur) != before_current.as_ref()
        {
            break cur;
        }
    };
    app!(|app: &mut AppView, _| {
        app.agent.set_model(
            session_d.clone(),
            "mock".to_string(),
            "mock-model".to_string(),
            None,
        );
        app.agent.send_message(
            session_d.clone(),
            format!("{} BG", pig_core::mock::SUBAGENT_TRIGGER),
            vec![],
            vec![],
            pig_protocol::ExecMode::AutoEdit,
        );
    });
    // 等 bg Agent 卡的代理卡元信息（SubagentCard 先于子代理执行到达，带 agent_id）
    let bg_agent_id = {
        let mut waited = 0u64;
        loop {
            timer!(50).await;
            waited += 50;
            assert!(waited < 15_000, "后台代理卡元信息超时");
            let meta = app!(|app: &mut AppView, cx| {
                let views = app.views.get(&session_d)?;
                views.thread.read(cx).debug_agent_card_meta()
            });
            if let Some((id, _)) = meta {
                break id;
            }
        }
    };
    // 立即开 tab（此刻子代理大概率仍在跑：mock 子侧 ≥2 次请求 × 50ms/片）
    app!(|app: &mut AppView, cx| {
        app.open_subagent_tab(
            session_d.clone(),
            bg_agent_id.clone(),
            "子代理自测委派".to_string(),
            cx,
        );
    });
    let mut saw_running = false;
    let mut saw_card_running = false;
    let mut saw_following = false;
    let mut saw_agent_chip = false;
    let mut initial_items = None;
    let mut waited = 0u64;
    loop {
        timer!(100).await;
        waited += 100;
        assert!(waited < 30_000, "面板实时输出超时");
        let state = app!(|app: &mut AppView, cx| {
            let live = app.debug_subagent_live(&bg_agent_id, cx);
            let scroll = app.debug_subagent_scroll(&bg_agent_id, cx);
            let card = app
                .views
                .get(&session_d)
                .and_then(|views| views.thread.read(cx).debug_agent_card_state());
            let chips = app.composer.read(cx).debug_task_chips();
            (live, scroll, card, chips)
        });
        let (Some((running, items, appends)), scroll_state, card_state, chips) = state else {
            continue;
        };
        saw_running |= running;
        // 追加活动期间跟随态不应丢（A3f）
        if let Some((following, _)) = scroll_state {
            saw_following |= following;
        }
        // A3e①：工具回执早已收尾（done）但子代理仍在跑（!finished）→ 卡应转圈
        if let Some((_, done, finished)) = &card_state {
            saw_card_running |= *done && !*finished;
        }
        // A3g：BG 在跑时「后台 Agent」chip 应出现
        saw_agent_chip |= chips.1;
        if initial_items.is_none() && items > 0 {
            initial_items = Some(items);
        }
        if !running && items > 0 {
            // 收尾（finished → running=false + 全量重拉收口）
            assert!(saw_running, "运行期间应见过 running=true（运行中指示）");
            assert!(appends >= 1, "应有 SubagentActivity 增量追加");
            assert!(
                Some(items) >= initial_items,
                "收尾重拉后 items 不应变少: {items} < {initial_items:?}"
            );
            break;
        }
    }
    assert!(saw_following, "追加活动期间 following 应保持 true");
    // 卡的终态：finished 落位（不再转圈）
    let (_, card_done, card_finished) = app!(|app: &mut AppView, cx| {
        app.views
            .get(&session_d)
            .and_then(|views| views.thread.read(cx).debug_agent_card_state())
            .expect("D 应有代理卡")
    });
    assert!(card_done && card_finished, "子代理结束后卡应落终态");
    assert!(
        saw_card_running,
        "应见过「工具收尾但子代理在跑」的转圈窗口（done=true 且 finished=false）"
    );
    println!("[selftest] 子代理面板实时输出（running 指示/活动追加/收尾消失）OK");
    println!("[selftest] 后台代理卡运行态机（转圈窗口→终态）OK");

    // A3f：面板跟随滚动——追加活动期间 following 未丢（上面已断言），收尾后贴底
    //（面板内容可能不足一屏：此时 max_offset=0，at_bottom 恒真，断言退化为
    // 跟随态检查；真实溢出场景的跟随/浮钮靠人工验证）
    let mut waited = 0u64;
    loop {
        timer!(200).await;
        waited += 200;
        assert!(waited < 10_000, "面板贴底超时");
        let scroll = app!(|app: &mut AppView, cx| app.debug_subagent_scroll(&bg_agent_id, cx));
        let Some((following, at_bottom)) = scroll else {
            continue;
        };
        if at_bottom {
            assert!(following, "贴底时应处于跟随态");
            break;
        }
    }
    println!("[selftest] 子代理面板跟随滚动（following 保持 + 贴底）OK");

    // A3g：chip 按类型拆分——BG 在跑时见过「后台 Agent」chip；
    // 会话 D 无 Bash 任务，「后台 Bash」chip 不应出现
    assert!(saw_agent_chip, "BG 在跑时应出现「后台 Agent」chip");
    let chips = app!(|app: &mut AppView, cx| app.composer.read(cx).debug_task_chips());
    assert_eq!(chips, (false, true), "会话 D 应只有 Agent chip: {chips:?}");
    // agent 任务行数据带 agent_id（与代理卡一致）
    let task_agent_ids = app!(|app: &mut AppView, cx| app.composer.read(cx).debug_agent_task_ids());
    assert!(
        task_agent_ids.iter().any(|id| id == &bg_agent_id),
        "Agent 任务行应带 agent_id: {task_agent_ids:?}"
    );
    // 任务行点击的事件路径：ComposerEvent::OpenSubagent → open_subagent_tab 聚焦 tab
    app!(|app: &mut AppView, cx| {
        app.composer.update(cx, |_, cx| {
            cx.emit(ComposerEvent::OpenSubagent {
                agent_id: bg_agent_id.clone(),
                title: "子代理 explore: 子代理自测委派".to_string(),
            });
        });
    });
    let tab_active = app!(|app: &mut AppView, cx| app.debug_subagent_tab(cx).is_some());
    assert!(tab_active, "OpenSubagent 后应有激活的子代理 tab");
    // 回归：打开「后台 Agent」弹层走真实渲染帧不炸——palette_open 漏 AgentTasks
    // 会落进 render_popup 的 unreachable（用户实机踩到的崩溃）
    let popup_open = app!(|app: &mut AppView, cx| {
        app.composer
            .update(cx, |composer, cx| composer.debug_open_agent_tasks_popup(cx))
    });
    assert!(popup_open, "Agent 弹层应打开");
    timer!(200).await;
    app!(|app: &mut AppView, cx| {
        app.composer
            .update(cx, |composer, cx| composer.debug_close_popup(cx));
    });
    println!("[selftest] 后台 Agent 弹层渲染（palette 路由回归）OK");
    println!("[selftest] 后台任务 chip 拆分（Agent chip 显隐/agent_id/点行开 tab）OK");

    // A3e②：模拟重启重开会话 D——代理卡元信息从 rollout 回放重建（不退化成
    // 原始输出卡），且后台代理回放即落终态（core 补发 finished，不转圈）
    app!(|app: &mut AppView, cx| app.restart_agent(cx));
    let mut waited = 0u64;
    loop {
        timer!(200).await;
        waited += 200;
        assert!(waited < 10_000, "重启后自动打开会话超时");
        let ready = app!(|app: &mut AppView, _| app.current.is_some());
        if ready {
            break;
        }
    }
    app!(|app: &mut AppView, cx| app.switch_session(session_d.clone(), cx));
    let mut waited = 0u64;
    loop {
        timer!(200).await;
        waited += 200;
        assert!(waited < 30_000, "重启后重放代理卡超时");
        let state = app!(|app: &mut AppView, cx| {
            let views = app.views.get(&session_d)?;
            let thread = views.thread.read(cx);
            Some((
                thread.debug_agent_card_meta(),
                thread.debug_agent_card_state(),
            ))
        });
        let Some((meta, card_state)) = state else {
            continue;
        };
        let (Some((card_agent, subtitle)), Some((_, done, finished))) = (meta, card_state) else {
            continue;
        };
        assert_eq!(card_agent, bg_agent_id, "回放应重建同一代理卡");
        assert!(
            subtitle.contains("explore"),
            "副标题应含 profile: {subtitle}"
        );
        assert!(
            done && finished,
            "回放的后台代理卡应直接落终态: done={done} finished={finished}"
        );
        break;
    }
    println!("[selftest] 代理卡回放重建（meta 保留 + 落终态不转圈）OK");

    // 三栏最小宽度钳制（纯函数）：侧栏 ≥200、右面板 ≥280、为中心区保留 ≥480
    assert_eq!(
        clamp_dock_widths(1280., 220., 300., true, true),
        (220., 300.),
        "区间内不动"
    );
    assert_eq!(
        clamp_dock_widths(1280., 100., 50., true, true),
        (200., 280.),
        "低于各自最小值拉回"
    );
    assert_eq!(
        clamp_dock_widths(1280., 900., 300., true, true),
        (500., 300.),
        "左栏封顶为中心区留 480，右栏不受牵连"
    );
    assert_eq!(
        clamp_dock_widths(1280., 900., 300., true, false),
        (800., 300.),
        "收起的栏不占预算"
    );
    assert_eq!(
        clamp_dock_widths(960., 400., 400., true, true),
        (200., 280.),
        "窗口最小宽时两侧同时越界：左先让位，一遍收敛到全最小"
    );
    assert_eq!(
        clamp_dock_widths(0., 220., 300., true, true),
        (220., 300.),
        "首帧未测量不动作"
    );
    println!("[selftest] 三栏最小宽度钳制 OK");

    println!("SELFTEST PASS");
    std::process::exit(0);
}
