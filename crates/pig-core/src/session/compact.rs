use super::*;

impl Session {
    /// compact：优先模型摘要；失败回退朴素截断。返回 false = 被取消。
    /// 历史重建为 system + 摘要消息 + 尾部原样消息（切在 user 边界，最多 4 条）。
    pub async fn run_compact(
        &mut self,
        config: Option<&ResolvedModel>,
        automatic: bool,
        tx: &async_channel::Sender<Event>,
        cancel: &CancellationToken,
    ) -> bool {
        const KEEP: usize = 4;
        if self.history.len() <= KEEP + 1 {
            self.emit(
                |session_id, seq| Event::ContextCompacted {
                    session_id,
                    seq,
                    omitted: 0,
                    note: "历史很短，无需压缩".to_string(),
                    automatic,
                },
                tx,
            );
            return true;
        }
        let changed: Vec<String> = self.tracker.tracked_paths();
        // 摘要是阻塞请求、期间无其他事件，先通知 UI 进入「正在压缩」态
        self.emit(
            |session_id, seq| Event::CompactStarted {
                session_id,
                seq,
                automatic,
            },
            tx,
        );

        let summary = match config {
            Some(config) => {
                let messages = build_summary_messages(&self.history, config);
                let tools = self.root_schemas();
                match provider::complete_messages(config, &messages, &tools, cancel).await {
                    Ok(summary) => Some(summary),
                    Err(_) if cancel.is_cancelled() => {
                        self.emit(|session_id, seq| Event::TurnAborted { session_id, seq }, tx);
                        return false;
                    }
                    Err(error) => {
                        eprintln!("[pig-core] 摘要请求失败，回退截断: {error}");
                        None
                    }
                }
            }
            None => None,
        };

        let changed_note = if changed.is_empty() {
            "期间无文件变更。".to_string()
        } else {
            format!("期间修改的文件: {}", changed.join(", "))
        };
        let tail = select_tail(&self.history, KEEP);
        let omitted = self.history.len() - 1 - tail.len();
        let note = match &summary {
            Some(summary) => format!(
                "[前文已压缩{}·模型摘要] 省略 {omitted} 条消息。

{summary}

使用说明：这份摘要是对前文对话的忠实记录——其中已完成的工作不要重做，其中已有的信息不要向用户重复询问；后台任务、文件内容等实时状态可能已变化，需要时用工具重新确认，不要凭摘要推断。{changed_note}",
                if automatic { "（自动）" } else { "" },
            ),
            None => format!(
                "[前文已压缩{}] 共 {omitted} 条消息被省略（摘要生成失败，已直接截断）。\
                 被省略的内容已不在上下文中，需要细节时用 Read/Grep 重新查证，不要凭印象推断。{changed_note}",
                if automatic { "（自动）" } else { "" },
            ),
        };

        self.history = vec![self.history[0].clone(), ChatMsg::system(note.clone())];
        self.history.extend(tail);
        // 水位重置为压缩后历史的估算值（下次真实采样校正）：不重置的话水位检查拿
        // 压缩前的旧高值，下一回合开头会立刻又触发一次自动压缩，把刚生成的摘要再压一遍
        let used_after = estimate_history_tokens(&self.history);
        self.last_total_tokens = Some(used_after);
        self.record(&RolloutRecord::Compact {
            note: note.clone(),
            omitted,
            automatic,
            used_after: Some(used_after),
        });
        // 容量 chip 即时刷新（模型未配置则无窗口可报，跳过）
        if let Some(config) = config {
            let (cache_read_total, input_total) = (self.cache_read_total, self.input_total);
            let total = config.context_window;
            self.emit(
                |session_id, seq| Event::ContextUsage {
                    session_id,
                    seq,
                    used: used_after,
                    total,
                    cache_read_total,
                    input_total,
                },
                tx,
            );
        }
        self.emit(
            |session_id, seq| Event::ContextCompacted {
                session_id,
                seq,
                omitted,
                note,
                automatic,
            },
            tx,
        );
        self.touch_index();
        true
    }
}

/// 压缩保留的尾部：最多 keep 条，且必须切在 user 边界上。
/// - 开头是 assistant（含 tool_calls）：Anthropic 端点要求首条非 system 消息
///   必须是 user（400），OpenAI 兼容端点也会拿到语义断裂的开头；
/// - 开头是 tool：孤儿 tool_result（无 tool_use 前置），两家都拒；
/// - 窗口内没有 user（自动压缩在长工具链中间触发）：退化为只保留最后一条
///   user 消息——当前回合的用户请求必须留在原样上下文里，不能全靠摘要兜底。
fn select_tail(history: &[ChatMsg], keep: usize) -> Vec<ChatMsg> {
    let mut tail: Vec<ChatMsg> = history[history.len().saturating_sub(keep)..].to_vec();
    while !tail.is_empty() && tail[0].role != "user" {
        tail.remove(0);
    }
    if tail.is_empty()
        && let Some(ix) = history.iter().rposition(|m| m.role == "user")
    {
        tail.push(history[ix].clone());
    }
    tail
}

/// 粗略估算历史的 token 量（~4 字符/token + 每条消息 4 token 开销）：
/// 只用于压缩后水位/容量显示的过渡值，偏低估——方向安全（不会误触发自动
/// 压缩），下一次真实采样的 Usage 会校正它。
fn estimate_history_tokens(history: &[ChatMsg]) -> u64 {
    let mut chars = 0usize;
    for msg in history {
        chars += msg.content.as_deref().unwrap_or("").chars().count();
        chars += msg.reasoning.as_deref().unwrap_or("").chars().count();
        if let Some(calls) = &msg.tool_calls {
            for call in calls {
                chars += call.function.arguments.chars().count() + 20;
            }
        }
    }
    (chars / 4 + history.len() * 4) as u64
}

pub const COMPACTION_MARKER: &str = "[COMPACTION]";
// ---- 会话自动命名（对齐 ZCode 的 title-generation sidecar）----

/// 摘要请求消息：冻结 system + 历史逐字原样 + 末尾一条指令 user 消息。
/// 与正常会话请求同一份 system/tools/历史字节 → 供应商前缀缓存命中上次回合
/// 写入的缓存（ZCode 摘要走同一条投影管线 / kimi-code 复用同一 history 数组
/// 的同款取舍；旧的「全部拼成单条大 user 消息 + 逐条截断」形态是缓存杀手，
/// 每次压缩全价输入且长 tool 输出有损）。tools 字段照带对齐缓存前缀。
/// 历史超预算时从头丢整条（kimi-code preShrink 同款），并裁到 user 边界——
/// 不以 tool 开头（孤儿 tool_result）且 Anthropic 首条必须是 user；
/// 自动压缩按构造不会超窗（触发点低于窗口减输出预留），这条路径主要护手动。
fn build_summary_messages(history: &[ChatMsg], config: &ResolvedModel) -> Vec<ChatMsg> {
    let budget = config
        .context_window
        .saturating_sub(config.max_output_tokens + 13_000);
    let mut head = 1usize; // history[0] = 冻结 system，必保留
    while history.len() - head > 1 && estimate_history_tokens(&history[head..]) > budget {
        head += 1;
    }
    let mut middle: Vec<ChatMsg> = history[head..].to_vec();
    // 裁到 user 边界（砍头后首条可能是 assistant/tool，两家端点都拒）
    while !middle.is_empty() && middle[0].role != "user" {
        middle.remove(0);
    }
    let mut messages = vec![history[0].clone()];
    messages.extend(middle);
    messages.push(ChatMsg::user(compaction_instruction()));
    messages
}

fn compaction_instruction() -> String {
    format!(
        "{COMPACTION_MARKER} 请把以上编程助手对话历史压缩成一份交接摘要，供同一助手在压缩后的上下文里继续工作。用中文，按以下分节输出（无内容的节略过）：
1. 用户目标：用户要做什么，明确提过的要求与偏好。
2. 已完成：已做完的工作与已验证的结论。
3. 文件变更：动过的文件（路径 + 一句话改动说明）。
4. 关键决策：技术选型与理由、用户否决过或纠正过的方向。
5. 进行中与待办：未完成的步骤、下一步计划、已知阻塞。
6. 重要上下文：正在跑的命令/后台任务、关键报错原文、环境要点。
只保留继续工作所需的信息；文件路径、命令、报错原文等硬信息原样保留，不要臆测补充。全文控制在 1200 字以内。
不要调用任何工具，直接输出摘要文本。
"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(text: &str) -> ChatMsg {
        ChatMsg::user(text.to_string())
    }

    fn assistant_text(text: &str) -> ChatMsg {
        ChatMsg::assistant(text.to_string(), vec![], None)
    }

    fn assistant_call(id: &str) -> ChatMsg {
        ChatMsg::assistant(
            String::new(),
            vec![crate::provider::ToolCall {
                id: id.into(),
                name: "Read".into(),
                arguments: "{}".into(),
            }],
            None,
        )
    }

    fn tool(id: &str) -> ChatMsg {
        ChatMsg::tool_result(id, "输出".to_string())
    }

    fn roles(msgs: &[ChatMsg]) -> Vec<&str> {
        msgs.iter().map(|m| m.role.as_str()).collect()
    }

    /// 窗口以 user 开头：原样保留（最常见的纯文本收尾形态）
    #[test]
    fn tail_starting_at_user_kept_as_is() {
        let history = vec![
            ChatMsg::system("s".into()),
            user("u1"),
            assistant_text("a1"),
            user("u2"),
            assistant_text("a2"),
            user("u3"),
            assistant_text("a3"),
        ];
        let tail = select_tail(&history, 4);
        assert_eq!(roles(&tail), ["user", "assistant", "user", "assistant"]);
    }

    /// 窗口以 tool/assistant 开头（工具链轮后）：裁到 user 边界——
    /// Anthropic 首条必须是 user，且孤儿 tool_result 两家都拒
    #[test]
    fn tail_trims_to_user_boundary() {
        let history = vec![
            ChatMsg::system("s".into()),
            user("u1"),
            assistant_call("c1"),
            tool("c1"),
            assistant_text("a1"),
            user("u2"),
            assistant_text("a2"),
        ];
        let tail = select_tail(&history, 4);
        assert_eq!(roles(&tail), ["user", "assistant"]);
        assert_eq!(tail[0].content.as_deref(), Some("u2"));
    }

    /// 窗口内没有 user（自动压缩在长工具链中间触发）：退化为只留最后一条
    /// user 消息——当前回合的用户请求必须原样保留，不能全靠摘要兜底
    #[test]
    fn tail_without_user_falls_back_to_last_user_message() {
        let history = vec![
            ChatMsg::system("s".into()),
            user("u1"),
            assistant_call("c1"),
            tool("c1"),
            assistant_call("c2"),
            tool("c2"),
        ];
        let tail = select_tail(&history, 4);
        assert_eq!(roles(&tail), ["user"]);
        assert_eq!(tail[0].content.as_deref(), Some("u1"));
    }

    /// 历史里一条 user 都没有（防御）：返回空，不造孤儿段
    #[test]
    fn tail_without_any_user_returns_empty() {
        let history = vec![
            ChatMsg::system("s".into()),
            assistant_call("c1"),
            tool("c1"),
        ];
        assert!(select_tail(&history, 4).is_empty());
    }

    /// 估算：覆盖正文/思考/工具参数与每条开销，单调增长
    #[test]
    fn estimate_counts_content_reasoning_and_arguments() {
        let base = estimate_history_tokens(&[user("1234")]);
        // 4 字符正文 /4 = 1 + 每条 4 = 5
        assert_eq!(base, 5);
        let with_reasoning = estimate_history_tokens(&[ChatMsg::assistant(
            "1234".into(),
            vec![],
            Some("12345678".into()),
        )]);
        // (4+8)/4 = 3 + 4 = 7
        assert_eq!(with_reasoning, 7);
        let with_call = estimate_history_tokens(&[assistant_call("c1")]);
        // 参数 "{}" 2 字符 + 调用常量 20 = 22/4 = 5 + 4 = 9
        assert_eq!(with_call, 9);
    }

    fn test_model(context_window: u64) -> crate::provider::ResolvedModel {
        crate::provider::ResolvedModel {
            base_url: "http://x".into(),
            api_key: "k".into(),
            model: "m".into(),
            context_window,
            max_output_tokens: 1000,
            api_format: pig_protocol::ApiFormat::OpenAiChat,
            reasoning_params: None,
            cap_web_search: false,
            web_search_tool: None,
            input_image: false,
            provider_name: "t".into(),
        }
    }

    /// 摘要请求形态：冻结 system 打头、历史逐字原样居中、指令 user 收尾——
    /// 与会话请求前缀逐字节一致是缓存命中前提，不得拼盘/截断
    #[test]
    fn summary_messages_wrap_verbatim_history() {
        let history = vec![
            ChatMsg::system("s".into()),
            user("u1"),
            assistant_call("c1"),
            tool("c1"),
            assistant_text("a1"),
            user("u2"),
            assistant_text("a2"),
        ];
        let messages = build_summary_messages(&history, &test_model(128_000));
        assert_eq!(
            roles(&messages),
            [
                "system",
                "user",
                "assistant",
                "tool",
                "assistant",
                "user",
                "assistant",
                "user"
            ]
        );
        // 中间段与历史逐条同内容（原样，无截断无重组）
        for (a, b) in messages[1..messages.len() - 1].iter().zip(&history[1..]) {
            assert_eq!(a.content, b.content);
            assert_eq!(a.tool_call_id, b.tool_call_id);
        }
        let instruction = &messages[messages.len() - 1];
        assert_eq!(instruction.role, "user");
        assert!(
            instruction
                .content
                .as_deref()
                .is_some_and(|c| c.contains(COMPACTION_MARKER) && c.contains("不要调用任何工具")),
            "指令收尾: {instruction:?}"
        );
    }

    /// 历史超预算：从头丢整条直到装下，且裁到 user 边界（不以 tool/assistant 开头）
    #[test]
    fn summary_messages_pre_shrink_drops_from_head_at_user_boundary() {
        let history = vec![
            ChatMsg::system("s".into()),
            user("u1"),
            assistant_call("c1"),
            tool("c1"),
            assistant_text("a1"),
            user("u2"),
            assistant_text("a2"),
        ];
        // 预算极小：只装得下尾部一两条
        let messages = build_summary_messages(&history, &test_model(1_100));
        assert_eq!(messages[0].role, "system");
        assert_eq!(
            messages[1].role,
            "user",
            "砍头后首条必须是 user: {}",
            roles(&messages).join(",")
        );
        assert!(
            messages.len() < history.len() + 1,
            "应已丢弃前缀: {messages:?}"
        );
    }

    /// 砍头+裁边后中间全空：只剩 system + 指令（摘要仍能产出，请求合法）
    #[test]
    fn summary_messages_fallback_to_instruction_only() {
        let history = vec![
            ChatMsg::system("s".into()),
            assistant_call("c1"),
            tool("c1"),
        ];
        let messages = build_summary_messages(&history, &test_model(1_100));
        assert_eq!(roles(&messages), ["system", "user"]);
    }
}
