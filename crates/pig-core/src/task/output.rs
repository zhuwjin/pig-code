use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use pig_protocol::TaskStatus;

use crate::NoConsoleExt as _;
use crate::rollout::now_secs;

/// Total accumulated output cap per task (stdout+stderr combined): exceeding it
/// force-stops the task — guards against "output-spewing commands" filling the
/// foreground sink memory within the timeout window, or running forever after
/// going to the background (same 16MiB hard kill as kimi-code; the spill 10MB
/// persist cap is independent of this).
const MAX_TASK_OUTPUT_TOTAL: usize = 16 * 1024 * 1024;

use super::registry::{TaskRegistry, append_output};

/// Accumulated task output metering (shared by the two stdout/stderr readers): crossing the total cap triggers a force stop only once.
#[derive(Default)]
pub(super) struct OutputMeter {
    total: AtomicUsize,
    pub(super) capped: AtomicBool,
}

impl OutputMeter {
    /// Add n bytes; returns true meaning "crossed the cap this time and not previously flagged", so the caller performs the one-time force stop.
    fn add(&self, n: usize) -> bool {
        let prev = self.total.fetch_add(n, Ordering::Relaxed);
        prev + n > MAX_TASK_OUTPUT_TOTAL && !self.capped.swap(true, Ordering::Relaxed)
    }
}

/// One-time force stop for exceeding the output cap: set Killed (the watcher
/// writes Exited only when still Running, so it will not overwrite), append the
/// notice to output (the rolling window keeps the tail, so the notice ends up
/// last), and kill the process tree.
fn cap_kill(registry: &TaskRegistry, task_id: &str) {
    let pid = {
        let mut tasks = registry.lock().expect("task registry lock");
        let Some(entry) = tasks.iter_mut().find(|t| t.id == task_id) else {
            return;
        };
        if matches!(entry.status, TaskStatus::Running) {
            entry.status = TaskStatus::Killed;
            entry.reason =
                Some("Output exceeded the 16MiB limit; the task was force-stopped".to_string());
            entry.ended_at = Some(now_secs());
        }
        entry
            .output
            .push_str("\n[Output exceeded the 16MiB limit; the task was force-stopped. Redirect large output to a file (e.g. command > out.txt) and process it with Read/Grep]");
        entry.pid
    };
    if let Some(pid) = pid {
        kill_process_tree(pid);
    }
}

/// Kill the process tree: Windows taskkill /T (children included); unix kills
/// the process group first, then the pid as a fallback (spawn_shell's
/// process_group(0) makes the child its own group leader).
pub(crate) fn kill_process_tree(pid: u32) {
    if cfg!(target_os = "windows") {
        let _ = std::process::Command::new("taskkill")
            .no_console()
            .args(["/PID", &pid.to_string(), "/F", "/T"])
            .output();
    } else {
        let _ = std::process::Command::new("kill")
            .args(["-9", "--", &format!("-{pid}")])
            .output();
        let _ = std::process::Command::new("kill")
            .args(["-9", &pid.to_string()])
            .output();
    }
}

/// Pipe reader: chunk → decode (StreamDecoder: UTF-8 first / Windows GBK
/// fallback / cross-chunk suspension) → registry rolling output + spill persist
/// (raw bytes); when the sink is non-empty, also keep a full copy there
/// (foreground Completed needs stdout/stderr rendered separately, while the
/// registry copy is the merged stream). Crossing the 16MiB total cap
/// force-stops the task and stops reading.
pub(super) async fn read_into<R: tokio::io::AsyncRead + Unpin>(
    mut reader: R,
    registry: TaskRegistry,
    task_id: String,
    sink: Option<Arc<Mutex<String>>>,
    meter: Arc<OutputMeter>,
) {
    let mut decoder = pig_utils::text::StreamDecoder::new();
    let mut buf = [0u8; 4096];
    loop {
        match tokio::io::AsyncReadExt::read(&mut reader, &mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let text = decoder.push(&buf[..n]);
                append_output(&registry, &task_id, &buf[..n], &text);
                if let Some(sink) = &sink {
                    sink.lock().expect("stream sink lock").push_str(&text);
                }
                if meter.add(n) {
                    cap_kill(&registry, &task_id);
                    break;
                }
            }
        }
    }
    // Stream ended: flush any leftover incomplete tail bytes (not to the spill — raw bytes were already persisted per chunk)
    let tail = decoder.finish();
    if !tail.is_empty() {
        append_output(&registry, &task_id, b"", &tail);
    }
}
