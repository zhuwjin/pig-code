use super::*;

impl Session {
    /// compact：优先模型摘要；失败回退朴素截断。返回 false = 被取消。
    /// 历史重建为 system + 摘要消息 + 最近 4 条原样消息。
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
        let omitted = self.history.len() - 1 - KEEP;
        let changed: Vec<String> = self.tracker.tracked_paths();

        let summary = match config {
            Some(config) => {
                let prompt_text = compaction_prompt(&self.history);
                match provider::complete_text(config, prompt_text, cancel).await {
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

        let mut tail: Vec<ChatMsg> = self.history[self.history.len() - KEEP..].to_vec();
        // 真实 API 不接受没有 assistant 前置的孤儿 tool 消息，裁到安全边界
        while tail.first().is_some_and(|m| m.role == "tool") {
            tail.remove(0);
        }
        self.history = vec![self.history[0].clone(), ChatMsg::system(note.clone())];
        self.history.extend(tail);
        self.record(&RolloutRecord::Compact {
            note: note.clone(),
            omitted,
        });
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

pub const COMPACTION_MARKER: &str = "[COMPACTION]";
// ---- 会话自动命名（对齐 ZCode 的 title-generation sidecar）----

fn compaction_prompt(history: &[ChatMsg]) -> String {
    let mut out = format!(
        "{COMPACTION_MARKER} 请把以下编程助手对话历史压缩成一份交接摘要，供同一助手在压缩后的上下文里继续工作。用中文，按以下分节输出（无内容的节略过）：
1. 用户目标：用户要做什么，明确提过的要求与偏好。
2. 已完成：已做完的工作与已验证的结论。
3. 文件变更：动过的文件（路径 + 一句话改动说明）。
4. 关键决策：技术选型与理由、用户否决过或纠正过的方向。
5. 进行中与待办：未完成的步骤、下一步计划、已知阻塞。
6. 重要上下文：正在跑的命令/后台任务、关键报错原文、环境要点。
只保留继续工作所需的信息；文件路径、命令、报错原文等硬信息原样保留，不要臆测补充。全文控制在 1200 字以内。
"
    );
    for msg in history {
        let role = &msg.role;
        if let Some(content) = &msg.content {
            let content: String = content.chars().take(2000).collect();
            out.push_str(&format!(
                "--- {role} ---
{content}
"
            ));
        }
        if let Some(calls) = &msg.tool_calls {
            for call in calls {
                let args: String = call.function.arguments.chars().take(200).collect();
                out.push_str(&format!(
                    "--- {role} [tool_call {}] ---
{args}
",
                    call.function.name
                ));
            }
        }
    }
    out
}
