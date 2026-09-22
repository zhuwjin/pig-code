//! 工具层单元测试：Edit 失败分支、路径逃逸、write/diff/revert。

use pig_core::task::SessionToolState;
use pig_core::tool::{self, ChangeTracker, ToolContext};
use pig_core::provider::ToolCall;

fn call(name: &str, args: serde_json::Value) -> ToolCall {
    ToolCall {
        id: "t1".into(),
        name: name.into(),
        arguments: args.to_string(),
    }
}

fn temp_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("pig-core-tools-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.canonicalize().unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn edit_not_found_and_not_unique() {
    let dir = temp_dir("edit");
    std::fs::write(dir.join("a.txt"), "foo\nfoo\nbar\n").unwrap();
    let mut tracker = ChangeTracker::default();
    let state = SessionToolState::for_test();

    let (_, is_error, _, _) = tool::execute(
        &call("Edit", serde_json::json!({"path": "a.txt", "old_string": "baz", "new_string": "x"})),
        ToolContext { cwd: &dir, tracker: &mut tracker, state: &state },
    )
    .await;
    assert!(is_error);

    let (output, is_error, _, _) = tool::execute(
        &call("Edit", serde_json::json!({"path": "a.txt", "old_string": "foo", "new_string": "x"})),
        ToolContext { cwd: &dir, tracker: &mut tracker, state: &state },
    )
    .await;
    assert!(is_error);
    assert!(output.contains("2 次"), "应提示多处匹配: {output}");

    let (_, is_error, change, _) = tool::execute(
        &call("Edit", serde_json::json!({"path": "a.txt", "old_string": "bar", "new_string": "x"})),
        ToolContext { cwd: &dir, tracker: &mut tracker, state: &state },
    )
    .await;
    assert!(!is_error);
    let change = change.expect("Edit 应产生 FileChange");
    assert_eq!(change.path, "a.txt");
    assert_eq!((change.additions, change.deletions), (1, 1));
    assert!(change.unified_diff.contains("-bar") && change.unified_diff.contains("+x"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn path_escape_rejected() {
    let dir = temp_dir("escape");
    let mut tracker = ChangeTracker::default();
    let state = SessionToolState::for_test();

    for (tool_name, args) in [
        ("Read", serde_json::json!({"path": "../outside.txt"})),
        ("Write", serde_json::json!({"path": "../outside.txt", "content": "x"})),
        ("Edit", serde_json::json!({"path": "../outside.txt", "old_string": "a", "new_string": "b"})),
        ("Read", serde_json::json!({"path": "sub/../../outside.txt"})),
    ] {
        let (output, is_error, _, _) = tool::execute(
            &call(tool_name, args),
            ToolContext { cwd: &dir, tracker: &mut tracker, state: &state },
        )
        .await;
        assert!(is_error, "{tool_name} 越界应失败");
        assert!(
            output.contains("越出工作目录") || output.contains("不存在") || output.contains("读取失败"),
            "错误信息: {output}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn write_diff_revert_cycle() {
    let dir = temp_dir("write");
    let mut tracker = ChangeTracker::default();
    let state = SessionToolState::for_test();

    let (_, is_error, change, _) = tool::execute(
        &call("Write", serde_json::json!({"path": "sub/new.txt", "content": "a\nb\n"})),
        ToolContext { cwd: &dir, tracker: &mut tracker, state: &state },
    )
    .await;
    assert!(!is_error);
    let change = change.expect("write 应产生 FileChange");
    assert_eq!(change.path, "sub/new.txt", "路径用正斜杠相对化");
    assert_eq!((change.additions, change.deletions), (2, 0));

    // 第二次修改 diff 仍相对原始快照（不存在 → 全新增）
    let (_, is_error, change, _) = tool::execute(
        &call("Edit", serde_json::json!({"path": "sub/new.txt", "old_string": "b", "new_string": "B"})),
        ToolContext { cwd: &dir, tracker: &mut tracker, state: &state },
    )
    .await;
    assert!(!is_error);
    let change = change.unwrap();
    assert_eq!((change.additions, change.deletions), (2, 0), "仍是原始→当前: {}", change.unified_diff);

    tracker.revert(&dir.join("sub/new.txt")).unwrap();
    assert!(!dir.join("sub/new.txt").exists(), "新建文件撤销即删除");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn edit_produces_per_edit_diff() {
    let dir = temp_dir("per-edit-diff");
    let mut tracker = ChangeTracker::default();
    let state = SessionToolState::for_test();

    // Write 的本次编辑 diff：不存在 → 全量新增
    let (_, is_error, _, edit) = tool::execute(
        &call("Write", serde_json::json!({"path": "f.txt", "content": "a\nb\nc\n"})),
        ToolContext { cwd: &dir, tracker: &mut tracker, state: &state },
    )
    .await;
    assert!(!is_error);
    let edit = edit.expect("Write 应带本次编辑 diff");
    assert_eq!(edit.path, "f.txt");
    assert_eq!((edit.additions, edit.deletions), (3, 0));
    assert!(edit.unified_diff.contains("+a"), "diff 内容: {}", edit.unified_diff);

    // Edit 的本次编辑 diff 只反映这一次替换（1 增 1 删），
    // 与会话累计口径的 file_change（相对原始快照）区分开
    let (_, is_error, change, edit) = tool::execute(
        &call("Edit", serde_json::json!({"path": "f.txt", "old_string": "b", "new_string": "B"})),
        ToolContext { cwd: &dir, tracker: &mut tracker, state: &state },
    )
    .await;
    assert!(!is_error);
    let edit = edit.expect("Edit 应带本次编辑 diff");
    assert_eq!((edit.additions, edit.deletions), (1, 1), "仅本次替换: {}", edit.unified_diff);
    assert!(edit.unified_diff.contains("-b") && edit.unified_diff.contains("+B"));
    let change = change.expect("累计 diff 仍存在");
    assert_eq!((change.additions, change.deletions), (3, 0), "累计口径不变（原始不存在→当前）");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fallback_edit_diff_from_arguments() {
    let dir = temp_dir("fallback-diff");

    // Edit：用 old_string/new_string 现算，只含被替换片段
    let edit = tool::fallback_edit_diff(
        &dir,
        "Edit",
        &serde_json::json!({"path": "sub/f.txt", "old_string": "a\nb\nc", "new_string": "a\nB\nc"}).to_string(),
    )
    .expect("Edit 参数应能兜底出 diff");
    assert_eq!(edit.path, "sub/f.txt");
    assert_eq!((edit.additions, edit.deletions), (1, 1));
    assert!(edit.unified_diff.contains("-b") && edit.unified_diff.contains("+B"));

    // Write：按新建文件兜底（全量新增）
    let edit = tool::fallback_edit_diff(
        &dir,
        "Write",
        &serde_json::json!({"path": "n.txt", "content": "x\ny\n"}).to_string(),
    )
    .expect("Write 参数应能兜底出 diff");
    assert_eq!((edit.additions, edit.deletions), (2, 0));

    // 非写改工具 / 缺参数：无兜底
    assert!(tool::fallback_edit_diff(&dir, "Bash", r#"{"command": "ls"}"#).is_none());
    assert!(tool::fallback_edit_diff(&dir, "Edit", r#"{"path": "f.txt"}"#).is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn turn_changes_are_per_turn_not_cumulative() {
    let dir = temp_dir("turn-changes");
    let mut tracker = ChangeTracker::default();
    let state = SessionToolState::for_test();

    // 「第一轮」Write 3 行：本轮净额 = 全量新增
    let (_, is_error, _, _) = tool::execute(
        &call("Write", serde_json::json!({"path": "f.txt", "content": "a\nb\nc\n"})),
        ToolContext { cwd: &dir, tracker: &mut tracker, state: &state },
    )
    .await;
    assert!(!is_error);
    let changes = tracker.take_turn_changes(&dir);
    assert_eq!(changes.len(), 1);
    assert_eq!((changes[0].additions, changes[0].deletions), (3, 0));
    assert!(tracker.take_turn_changes(&dir).is_empty(), "take 后应清空");

    // 「第二轮」Edit 1 行：只算本轮（1 增 1 删），不是会话累计口径
    let (_, is_error, _, _) = tool::execute(
        &call("Edit", serde_json::json!({"path": "f.txt", "old_string": "b", "new_string": "B"})),
        ToolContext { cwd: &dir, tracker: &mut tracker, state: &state },
    )
    .await;
    assert!(!is_error);
    let changes = tracker.take_turn_changes(&dir);
    assert_eq!(changes.len(), 1);
    assert_eq!((changes[0].additions, changes[0].deletions), (1, 1));

    // 「第三轮」同一轮内改回原文：turn 首末内容一致，净额归零不产出
    let (_, is_error, _, _) = tool::execute(
        &call("Edit", serde_json::json!({"path": "f.txt", "old_string": "B", "new_string": "b"})),
        ToolContext { cwd: &dir, tracker: &mut tracker, state: &state },
    )
    .await;
    assert!(!is_error);
    let (_, is_error, _, _) = tool::execute(
        &call("Edit", serde_json::json!({"path": "f.txt", "old_string": "b", "new_string": "B"})),
        ToolContext { cwd: &dir, tracker: &mut tracker, state: &state },
    )
    .await;
    assert!(!is_error);
    assert!(tracker.take_turn_changes(&dir).is_empty(), "轮内改回原文净额应为零");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn revert_modified_file_restores_content() {
    let dir = temp_dir("revert");
    std::fs::write(dir.join("m.txt"), "original\n").unwrap();
    let mut tracker = ChangeTracker::default();
    let state = SessionToolState::for_test();

    tool::execute(
        &call("Edit", serde_json::json!({"path": "m.txt", "old_string": "original", "new_string": "changed"})),
        ToolContext { cwd: &dir, tracker: &mut tracker, state: &state },
    )
    .await;
    assert_eq!(std::fs::read_to_string(dir.join("m.txt")).unwrap(), "changed\n");

    tracker.revert(&dir.join("m.txt")).unwrap();
    assert_eq!(std::fs::read_to_string(dir.join("m.txt")).unwrap(), "original\n");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn glob_and_grep() {
    let dir = temp_dir("search");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/a.rs"), "fn main() {}\n// TODO 修复\n").unwrap();
    std::fs::write(dir.join("src/b.md"), "TODO 文档\n").unwrap();
    let mut tracker = ChangeTracker::default();
    let state = SessionToolState::for_test();

    let (out, is_error, _, _) = tool::execute(
        &call("Glob", serde_json::json!({"pattern": "**/*.rs"})),
        ToolContext { cwd: &dir, tracker: &mut tracker, state: &state },
    )
    .await;
    assert!(!is_error);
    assert!(out.contains("src/a.rs") && !out.contains("b.md"), "{out}");

    let (out, is_error, _, _) = tool::execute(
        &call("Grep", serde_json::json!({"pattern": "TODO"})),
        ToolContext { cwd: &dir, tracker: &mut tracker, state: &state },
    )
    .await;
    assert!(!is_error);
    assert!(out.contains("src/a.rs:2:") && out.contains("src/b.md:1:"), "{out}");

    let (out, _, _, _) = tool::execute(
        &call("Grep", serde_json::json!({"pattern": "TODO", "include": "*.rs"})),
        ToolContext { cwd: &dir, tracker: &mut tracker, state: &state },
    )
    .await;
    assert!(out.contains("a.rs") && !out.contains("b.md"), "include 过滤: {out}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn todo_list_read_write_replace() {
    let dir = temp_dir("todo");
    let mut tracker = ChangeTracker::default();
    let state = SessionToolState::for_test();

    // 空读
    let (out, is_error, _, _) = tool::execute(
        &call("TodoList", serde_json::json!({})),
        ToolContext { cwd: &dir, tracker: &mut tracker, state: &state },
    )
    .await;
    assert!(!is_error);
    assert_eq!(out, "当前没有待办事项");

    // 写入（状态挂在 ctx 句柄上，跨调用保持）
    let (out, is_error, _, _) = tool::execute(
        &call("TodoList", serde_json::json!({"todos": [
            {"content": "读代码", "status": "done"},
            {"content": "改实现", "status": "in_progress"},
            {"content": "跑测试", "status": "pending"},
        ]})),
        ToolContext { cwd: &dir, tracker: &mut tracker, state: &state },
    )
    .await;
    assert!(!is_error);
    assert!(out.contains("1. [done] 读代码"), "{out}");
    assert!(out.contains("2. [in_progress] 改实现"), "{out}");

    // 回读
    let (out, is_error, _, _) = tool::execute(
        &call("TodoList", serde_json::json!({})),
        ToolContext { cwd: &dir, tracker: &mut tracker, state: &state },
    )
    .await;
    assert!(!is_error);
    assert!(out.contains("3. [pending] 跑测试"), "{out}");

    // 整体替换：旧项应全部消失
    let (out, _, _, _) = tool::execute(
        &call("TodoList", serde_json::json!({"todos": [{"content": "收尾", "status": "pending"}]})),
        ToolContext { cwd: &dir, tracker: &mut tracker, state: &state },
    )
    .await;
    assert!(out.contains("1. [pending] 收尾"), "{out}");
    assert!(!out.contains("读代码"), "整体替换后旧项应消失: {out}");

    // 非法 status 报错，且清单保持替换前的内容
    let (out, is_error, _, _) = tool::execute(
        &call("TodoList", serde_json::json!({"todos": [{"content": "x", "status": "doing"}]})),
        ToolContext { cwd: &dir, tracker: &mut tracker, state: &state },
    )
    .await;
    assert!(is_error, "非法 status 应报错: {out}");
    let (out, _, _, _) = tool::execute(
        &call("TodoList", serde_json::json!({})),
        ToolContext { cwd: &dir, tracker: &mut tracker, state: &state },
    )
    .await;
    assert!(out.contains("1. [pending] 收尾"), "写入失败后清单应保持不变: {out}");
}

#[test]
fn fetch_url_extract_text_strips_non_content() {
    let html = "<html><head><style>body{color:red}</style><script>var x=1;</script></head>\
        <body><nav>菜单</nav><main><h1>标题</h1><p>第一段</p><p>第二段</p>\
        <script>ignore()</script><noscript>备用</noscript><svg><text>图标</text></svg>\
        </main><footer>页脚</footer></body></html>";
    let text = tool::extract_text(html);
    assert!(text.contains("标题"), "{text}");
    assert!(text.contains("第一段"), "{text}");
    assert!(text.contains("第二段"), "{text}");
    assert!(!text.contains("var x"), "head script 应剔除: {text}");
    assert!(!text.contains("color:red"), "style 应剔除: {text}");
    assert!(!text.contains("ignore()"), "正文内 script 应剔除: {text}");
    assert!(!text.contains("备用"), "noscript 应剔除: {text}");
    assert!(!text.contains("图标"), "svg 应剔除: {text}");
    assert!(!text.contains("菜单"), "优先 main，nav 不应出现: {text}");
    assert!(!text.contains("页脚"), "优先 main，footer 不应出现: {text}");
    assert!(text.contains("标题\n第一段"), "块级元素之间应换行: {text:?}");
}

#[test]
fn fetch_url_extract_text_body_fallback_and_blank_collapse() {
    let html = "<html><body><div><p>甲</p></div><div><p>乙</p>\n\n\n<p>丙</p></div></body></html>";
    let text = tool::extract_text(html);
    assert_eq!(text, "甲\n乙\n丙", "连续空行应折叠: {text:?}");
}

#[test]
fn fetch_url_is_private_host_ranges() {
    for host in [
        "localhost", "LOCALHOST", "localhost.", "127.0.0.1", "127.5.5.5", "::1", "[::1]",
        "0.0.0.0", "10.0.0.1", "10.255.255.255", "192.168.1.1", "172.16.0.1", "172.31.255.1",
        "169.254.1.1",
    ] {
        assert!(tool::is_private_host(host), "{host} 应判定为私网");
    }
    for host in [
        "example.com", "8.8.8.8", "1.1.1.1", "172.15.0.1", "172.32.0.1", "11.0.0.1",
        "192.167.1.1", "10x.example.com",
    ] {
        assert!(!tool::is_private_host(host), "{host} 应放行");
    }
}

/// Bash run_in_background 全生命周期：启动 → 注册表可见 → Exited(0) → TaskOutput
/// 含输出；sleep 后台任务 TaskStop → Killed（重复停止报错）；TaskList 渲染两个 id。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn background_bash_task_lifecycle() {
    let dir = temp_dir("bgtask");
    let mut tracker = ChangeTracker::default();
    let state = SessionToolState::for_test();

    let task_id_of = |out: &str| {
        out.strip_prefix("已在后台启动，task_id: ")
            .and_then(|rest| rest.split('。').next())
            .expect("返回文案应含 task_id")
            .to_string()
    };

    // 后台 echo：立即返回 task_id
    let (out, is_error, _, _) = tool::execute(
        &call("Bash", serde_json::json!({"command": "echo bg-marker", "run_in_background": true})),
        ToolContext { cwd: &dir, tracker: &mut tracker, state: &state },
    )
    .await;
    assert!(!is_error, "{out}");
    let task1 = task_id_of(&out);

    // 轮询注册表至 Exited(0)（带超时）
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let done = {
            let tasks = state.tasks.lock().expect("tasks lock");
            matches!(
                tasks.iter().find(|t| t.id == task1).map(|t| &t.status),
                Some(pig_protocol::TaskStatus::Exited(0))
            )
        };
        if done {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "等待后台任务退出超时");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    // TaskOutput 含输出
    let (out, is_error, _, _) = tool::execute(
        &call("TaskOutput", serde_json::json!({"task_id": task1})),
        ToolContext { cwd: &dir, tracker: &mut tracker, state: &state },
    )
    .await;
    assert!(!is_error, "{out}");
    assert!(out.contains("bg-marker"), "{out}");

    // sleep 30 后台启动 → TaskStop → Killed
    let (out, is_error, _, _) = tool::execute(
        &call("Bash", serde_json::json!({"command": "sleep 30", "run_in_background": true})),
        ToolContext { cwd: &dir, tracker: &mut tracker, state: &state },
    )
    .await;
    assert!(!is_error, "{out}");
    let task2 = task_id_of(&out);
    let (out, is_error, _, _) = tool::execute(
        &call("TaskStop", serde_json::json!({"task_id": task2})),
        ToolContext { cwd: &dir, tracker: &mut tracker, state: &state },
    )
    .await;
    assert!(!is_error, "{out}");
    {
        let tasks = state.tasks.lock().expect("tasks lock");
        let entry = tasks.iter().find(|t| t.id == task2).expect("task2 存在");
        assert!(
            matches!(entry.status, pig_protocol::TaskStatus::Killed),
            "应为 Killed: {:?}",
            entry.status
        );
        assert!(entry.ended_at.is_some());
    }
    // 重复停止 → 「任务已结束」错误
    let (_, is_error, _, _) = tool::execute(
        &call("TaskStop", serde_json::json!({"task_id": task2})),
        ToolContext { cwd: &dir, tracker: &mut tracker, state: &state },
    )
    .await;
    assert!(is_error, "已停止的任务再停应报错");

    // TaskList 渲染包含两个 task_id
    let (out, is_error, _, _) = tool::execute(
        &call("TaskList", serde_json::json!({})),
        ToolContext { cwd: &dir, tracker: &mut tracker, state: &state },
    )
    .await;
    assert!(!is_error, "{out}");
    assert!(out.contains(&task1) && out.contains(&task2), "{out}");
}

#[test]
fn parse_questions_validates_shape() {
    // 正常：单选 1 题（header/description）+ 多选 1 题
    let questions = tool::parse_questions(&serde_json::json!({"questions": [
        {"question": "选方案", "header": "方案", "options": [{"label": "A"}, {"label": "B", "description": "备选"}]},
        {"question": "选范围", "multi_select": true, "options": [{"label": "x"}, {"label": "y"}, {"label": "z"}]},
    ]}))
    .expect("合法参数");
    assert_eq!(questions.len(), 2);
    assert_eq!(questions[0].question, "选方案");
    assert_eq!(questions[0].header.as_deref(), Some("方案"));
    assert!(!questions[0].multi_select);
    assert_eq!(questions[0].options.len(), 2);
    assert_eq!(questions[0].options[1].description.as_deref(), Some("备选"));
    assert!(questions[1].multi_select);
    assert_eq!(questions[1].options.len(), 3);

    // 题数越界：0 题 / 5 题
    assert!(tool::parse_questions(&serde_json::json!({"questions": []})).is_err());
    let five = serde_json::json!({"questions": (0..5)
        .map(|i| serde_json::json!({"question": format!("q{i}"), "options": [{"label": "a"}, {"label": "b"}]}))
        .collect::<Vec<_>>()});
    assert!(tool::parse_questions(&five).is_err());

    // 选项数越界：1 个 / 5 个
    assert!(tool::parse_questions(&serde_json::json!({"questions": [{"question": "q", "options": [{"label": "a"}]}]})).is_err());
    assert!(tool::parse_questions(&serde_json::json!({"questions": [{"question": "q", "options": [
        {"label": "1"}, {"label": "2"}, {"label": "3"}, {"label": "4"}, {"label": "5"}
    ]}]}))
    .is_err());

    // 空 label / 空 question / 缺 options / 缺 questions
    assert!(tool::parse_questions(&serde_json::json!({"questions": [{"question": "q", "options": [{"label": " "}, {"label": "b"}]}]})).is_err());
    assert!(tool::parse_questions(&serde_json::json!({"questions": [{"question": " ", "options": [{"label": "a"}, {"label": "b"}]}]})).is_err());
    assert!(tool::parse_questions(&serde_json::json!({"questions": [{"question": "q"}]})).is_err());
    assert!(tool::parse_questions(&serde_json::json!({})).is_err());
}
