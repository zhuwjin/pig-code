use pig_protocol::{
    DeliveryChannel, NotificationSeverity, TaskKind, TaskNotification, TaskNotificationOutput,
    TaskStatus,
};

use crate::store::BackgroundTask;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeliveryReceipt {
    pub channel: DeliveryChannel,
    pub confirmed: bool,
}

use super::registry::TaskEntry;

#[derive(Clone, Debug)]
pub enum TaskChange {
    Snapshot {
        session_id: String,
        task: Option<BackgroundTask>,
    },
    Terminal {
        session_id: String,
        owner_agent_id: Option<String>,
        task: Box<BackgroundTask>,
        notification: TaskNotification,
    },
}

pub(crate) fn publish_change(sender: &async_channel::Sender<TaskChange>, change: TaskChange) {
    let (session_id, task_id, terminal) = match &change {
        TaskChange::Snapshot { session_id, task } => (
            session_id.clone(),
            task.as_ref().map(|task| task.summary.id.clone()),
            false,
        ),
        TaskChange::Terminal {
            session_id, task, ..
        } => (session_id.clone(), Some(task.summary.id.clone()), true),
    };
    if let Err(error) = sender.send_blocking(change) {
        tracing::error!(
            %session_id,
            task_id = task_id.as_deref().unwrap_or(""),
            terminal,
            error = ?error,
            "task change could not reach the session manager"
        );
    }
}

pub fn snapshot_change(session_id: &str, entry: &TaskEntry) -> TaskChange {
    TaskChange::Snapshot {
        session_id: session_id.to_string(),
        task: Some(entry.background()),
    }
}

pub fn terminal_change(session_id: &str, entry: &TaskEntry) -> TaskChange {
    TaskChange::Terminal {
        session_id: session_id.to_string(),
        owner_agent_id: entry.owner_agent_id.clone(),
        task: Box::new(entry.background()),
        notification: terminal_notification(session_id, entry),
    }
}

pub fn terminal_notification(session_id: &str, entry: &TaskEntry) -> TaskNotification {
    let ended_at = entry.ended_at.unwrap_or(entry.started_at);
    let duration_ms = ended_at
        .saturating_sub(entry.started_at)
        .saturating_mul(1_000);
    let output_path = entry
        .result_path
        .clone()
        .or_else(|| entry.spill_path().map(std::path::Path::to_path_buf));
    let output = output_path.and_then(|path| {
        std::fs::metadata(&path)
            .ok()
            .map(|metadata| TaskNotificationOutput::File {
                path,
                bytes: metadata.len(),
            })
    });
    let output = output.or_else(|| {
        (!entry.output.is_empty()).then(|| TaskNotificationOutput::Preview {
            text: super::registry::tail_chars(&entry.output, 16_000),
            total_bytes: entry.output.len() as u64,
            truncated: entry.output.len() > 16_000,
        })
    });
    TaskNotification {
        id: notification_id(session_id, &entry.id, entry.status),
        task_id: entry.id.clone(),
        kind: entry.kind,
        status: entry.status,
        exit_code: entry.exit_code,
        severity: notification_severity(entry.status),
        description: entry.command.clone(),
        title: notification_title(entry.kind, entry.status),
        body: notification_body(
            &entry.command,
            entry.status,
            duration_ms,
            entry.reason.as_deref(),
            entry.agent_id.as_deref(),
        ),
        duration_ms: Some(duration_ms),
        turns: entry.turns,
        model: entry.model.clone(),
        agent_id: entry.agent_id.clone(),
        record_path: entry.record_path.clone(),
        output,
    }
}

pub fn background_notification(session_id: &str, task: &BackgroundTask) -> TaskNotification {
    let summary = &task.summary;
    let duration_ms = summary
        .ended_at_ms
        .unwrap_or(summary.started_at_ms)
        .saturating_sub(summary.started_at_ms);
    let output_path = task
        .result_path
        .clone()
        .or_else(|| summary.spill_path.clone());
    let output = output_path
        .and_then(|path| {
            std::fs::metadata(&path)
                .ok()
                .map(|metadata| TaskNotificationOutput::File {
                    path,
                    bytes: metadata.len(),
                })
        })
        .or_else(|| {
            (!summary.output_tail.is_empty()).then(|| TaskNotificationOutput::Preview {
                text: summary.output_tail.clone(),
                total_bytes: summary.output_tail.len() as u64,
                truncated: true,
            })
        });
    TaskNotification {
        id: notification_id(session_id, &summary.id, summary.status),
        task_id: summary.id.clone(),
        kind: summary.kind,
        status: summary.status,
        exit_code: summary.exit_code,
        severity: notification_severity(summary.status),
        description: summary.command.clone(),
        title: notification_title(summary.kind, summary.status),
        body: notification_body(
            &summary.command,
            summary.status,
            duration_ms,
            if summary.status == TaskStatus::Lost {
                Some(
                    "The previous process ended before a terminal state was observed. Do not assume the task completed; inspect its output and rerun or resume it as appropriate.",
                )
            } else {
                summary.reason.as_deref()
            },
            summary.agent_id.as_deref(),
        ),
        duration_ms: Some(duration_ms),
        turns: task.turns,
        model: task.model.clone(),
        agent_id: summary.agent_id.clone(),
        record_path: task.record_path.clone(),
        output,
    }
}

pub fn lost_notification(
    session_id: &str,
    task: &BackgroundTask,
    ended_at_ms: u64,
) -> TaskNotification {
    let mut task = task.clone();
    task.summary.status = TaskStatus::Lost;
    task.summary.exit_code = None;
    task.summary.ended_at_ms = Some(ended_at_ms);
    task.summary.reason = Some("The previous process ended before a terminal state was observed. Do not assume the task completed; inspect its output and rerun or resume it as appropriate.".to_string());
    background_notification(session_id, &task)
}

pub fn notification_id(session_id: &str, task_id: &str, status: TaskStatus) -> String {
    format!("task:{session_id}:{task_id}:{}", status.as_str())
}

pub fn notification_title(kind: TaskKind, status: TaskStatus) -> String {
    format!(
        "Background {} {}",
        kind.as_str(),
        status.as_str().replace('_', " ")
    )
}

pub fn notification_severity(status: TaskStatus) -> NotificationSeverity {
    if status == TaskStatus::Completed {
        NotificationSeverity::Info
    } else {
        NotificationSeverity::Warning
    }
}

pub fn notification_body(
    description: &str,
    status: TaskStatus,
    duration_ms: u64,
    reason: Option<&str>,
    agent_id: Option<&str>,
) -> String {
    let outcome = match (status, reason) {
        (TaskStatus::TimedOut, _) => format!("{description} timed out."),
        (TaskStatus::Killed, Some(reason)) => {
            format!("{description} was stopped. Reason: {reason}")
        }
        (_, Some(reason)) => format!("{description} {}. Reason: {reason}", status.as_str()),
        (_, None) => format!("{description} {}.", status.as_str()),
    };
    let mut body = format!("Wall time: {}\n{outcome}", human_duration(duration_ms));
    if status != TaskStatus::Completed
        && let Some(agent_id) = agent_id
    {
        body.push_str(&format!(
            "\n\nTo recover or continue this subagent, call Agent(resume=\"{agent_id}\", prompt=\"Pick up where you left off; redo the last tool call if its result was never observed.\").\nUse agent_id (\"{agent_id}\"), not task_id, when resuming. The subagent retains its full prior context."
        ));
    }
    body
}

pub fn human_duration(ms: u64) -> String {
    if ms >= 60_000 {
        format!("{}m {}s", ms / 60_000, (ms % 60_000) / 1000)
    } else {
        format!("{:.1}s", ms as f64 / 1000.0)
    }
}

pub fn active_task_reminder(registry: &super::registry::TaskRegistry) -> Option<String> {
    let tasks = registry.lock().expect("task registry lock");
    let mut lines = vec![
        "<background-tasks>".to_string(),
        "These tasks are still active after compaction. Use TaskOutput to inspect them or TaskWait to wait for a terminal result; do not assume they completed.".to_string(),
    ];
    let mut count = 0usize;
    for entry in tasks.iter().filter(|entry| {
        !entry.foreground && matches!(entry.status, TaskStatus::Queued | TaskStatus::Running)
    }) {
        if count == 16 {
            lines.push("Additional active tasks are available through TaskList.".to_string());
            break;
        }
        let description = entry.command.chars().take(500).collect::<String>();
        lines.push(format!(
            "<task id=\"{}\" kind=\"{}\" status=\"{}\">{}</task>",
            escape_xml(&entry.id),
            entry.kind.as_str(),
            entry.status.as_str(),
            escape_xml(&description),
        ));
        count += 1;
    }
    (count > 0).then(|| {
        lines.push("</background-tasks>".to_string());
        lines.join("\n")
    })
}

pub fn render_notification(notification: &TaskNotification) -> String {
    let mut lines = vec![format!(
        "<notification id=\"{}\" category=\"task\" type=\"task.{}\" source_kind=\"background_task\" source_id=\"{}\"{}>",
        escape_xml(&notification.id),
        notification.status.as_str(),
        escape_xml(&notification.task_id),
        notification
            .agent_id
            .as_ref()
            .map_or_else(String::new, |id| {
                format!(" agent_id=\"{}\"", escape_xml(id))
            })
    )];
    lines.push(format!("Title: {}", notification.title));
    lines.push(format!("Severity: {}", notification.severity.as_str()));
    if let Some(exit_code) = notification.exit_code {
        lines.push(format!("Exit code: {exit_code}"));
    }
    if let Some(turns) = notification.turns {
        lines.push(format!("Steps: {turns}"));
    }
    if let Some(model) = &notification.model {
        lines.push(format!("Model: {model}"));
    }
    lines.push(notification.body.clone());
    if let Some(output) = &notification.output {
        match output {
            TaskNotificationOutput::File { path, bytes } => {
                let path = path.display().to_string();
                lines.push(format!(
                    "<output-file path=\"{}\" bytes=\"{bytes}\">",
                    escape_xml(&path)
                ));
                lines.push(format!(
                    "Read the output file to retrieve the result: {}",
                    escape_xml(&path)
                ));
                lines.push("</output-file>".to_string());
            }
            TaskNotificationOutput::Preview {
                text,
                total_bytes,
                truncated,
            } => {
                lines.push(format!(
                    "<output-preview bytes=\"{}\" total_bytes=\"{total_bytes}\" truncated=\"{truncated}\">",
                    text.len()
                ));
                lines.push(if *truncated {
                    "Showing the tail of the available output. No persisted full output is available.".to_string()
                } else {
                    "No persisted full output is available; this preview is the currently buffered task output.".to_string()
                });
                lines.push(escape_xml(text));
                lines.push("</output-preview>".to_string());
            }
        }
    }
    lines.push("</notification>".to_string());
    lines.join("\n")
}

pub fn render_notification_batch(notifications: &[TaskNotification]) -> String {
    notifications
        .iter()
        .map(render_notification)
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn escape_xml(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notification() -> TaskNotification {
        TaskNotification {
            id: notification_id("s1", "b1", TaskStatus::Completed),
            task_id: "b1".into(),
            kind: TaskKind::Shell,
            status: TaskStatus::Completed,
            exit_code: Some(0),
            severity: NotificationSeverity::Info,
            description: "cargo test".into(),
            title: notification_title(TaskKind::Shell, TaskStatus::Completed),
            body: "cargo test completed.".into(),
            duration_ms: Some(1_250),
            turns: None,
            model: None,
            agent_id: None,
            record_path: None,
            output: Some(TaskNotificationOutput::Preview {
                text: "ok <xml> & more".into(),
                total_bytes: 15,
                truncated: false,
            }),
        }
    }

    #[test]
    fn notification_ids_are_stable_per_terminal_state() {
        assert_eq!(notification().id, "task:s1:b1:completed");
    }

    #[test]
    fn renderer_escapes_dynamic_output_and_preserves_structure() {
        let text = render_notification(&notification());
        assert!(text.contains("type=\"task.completed\""));
        assert!(text.contains("ok &lt;xml&gt; &amp; more"));
        assert!(!text.contains("ok <xml>"));
    }
}
