use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use pig_protocol::TaskStatus;

/// Total accumulated output cap per task (stdout+stderr combined): exceeding it
/// force-stops the task — guards against "output-spewing commands" filling the
/// foreground sink memory within the timeout window, or running forever after
/// going to the background (same 16MiB hard kill as kimi-code; the spill 10MB
/// persist cap is independent of this).
const MAX_TASK_OUTPUT_TOTAL: usize = 16 * 1024 * 1024;

/// Cap on live incremental output streamed to the UI for a single tool call
/// (the full output still goes to the command sink and spill file).
const MAX_LIVE_STREAM_BYTES: usize = 64 * 1024;

/// Incremental live output callback for an active tool execution.
pub type OutputDeltaSink = Arc<dyn Fn(&str) + Send + Sync>;

tokio::task_local! {
    static TOOL_OUTPUT_STREAM: OutputDeltaSink;
}

/// Bind a live output delta sink for the duration of a single tool invocation.
pub(crate) async fn with_tool_output_stream<F: Future>(
    sink: OutputDeltaSink,
    fut: F,
) -> F::Output {
    TOOL_OUTPUT_STREAM.scope(sink, fut).await
}

/// Read the active tool invocation's live output sink, if any.
pub(crate) fn current_output_stream() -> Option<OutputDeltaSink> {
    TOOL_OUTPUT_STREAM.try_with(Arc::clone).ok()
}

use super::registry::{TaskRegistry, append_output};

/// Accumulated task output metering (shared by the two stdout/stderr readers): crossing the total cap triggers a force stop only once.
#[derive(Default)]
pub(super) struct OutputMeter {
    total: AtomicUsize,
    streamed: AtomicUsize,
    pub(super) capped: AtomicBool,
}

impl OutputMeter {
    /// Add n bytes; returns true meaning "crossed the cap this time and not previously flagged", so the caller performs the one-time force stop.
    fn add(&self, n: usize) -> bool {
        let prev = self.total.fetch_add(n, Ordering::Relaxed);
        prev + n > MAX_TASK_OUTPUT_TOTAL && !self.capped.swap(true, Ordering::Relaxed)
    }

    fn take_live_budget(&self, len: usize) -> usize {
        let prev = self.streamed.fetch_add(len, Ordering::Relaxed);
        MAX_LIVE_STREAM_BYTES.saturating_sub(prev).min(len)
    }
}

/// One-time force stop for exceeding the output cap: set Killed (the watcher
/// writes Exited only when still Running, so it will not overwrite), append the
/// notice to output (the rolling window keeps the tail, so the notice ends up
/// last), and kill the process tree.
fn cap_kill(registry: &TaskRegistry, task_id: &str) {
    let (pid, process) = {
        let mut tasks = registry.lock().expect("task registry lock");
        let Some(entry) = tasks.iter_mut().find(|t| t.id == task_id) else {
            return;
        };
        if matches!(entry.status, TaskStatus::Running) {
            entry.status = TaskStatus::Killed;
            entry.reason =
                Some("Output exceeded the 16MiB limit; the task was force-stopped".to_string());
            entry.ended_at = None;
        }
        entry
            .output
            .push_str("\n[Output exceeded the 16MiB limit; the task was force-stopped. Redirect large output to a file (e.g. command > out.txt) and process it with Read/Grep]");
        (entry.pid, entry.process.clone())
    };
    let result = if let Some(process) = process {
        process.terminate()
    } else if let Some(pid) = pid {
        Err(format!("process {pid} has no managed ProcessTree"))
    } else {
        return;
    };
    if let Err(error) = result {
        super::registry::note_output(
            registry,
            task_id,
            &format!("\n[Failed to terminate the output-capped process tree: {error}]"),
        );
    }
}

pub(super) fn emit_live_chunk(
    live_stream: &Option<OutputDeltaSink>,
    meter: &OutputMeter,
    text: &str,
) {
    let Some(stream) = live_stream else {
        return;
    };
    if text.is_empty() {
        return;
    }
    let allowed = meter.take_live_budget(text.len());
    if allowed == 0 {
        return;
    }
    let mut end = allowed.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    if end > 0 {
        stream(&text[..end]);
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
    live_stream: Option<OutputDeltaSink>,
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
                emit_live_chunk(&live_stream, &meter, &text);
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
        if let Some(sink) = &sink {
            sink.lock().expect("stream sink lock").push_str(&tail);
        }
        emit_live_chunk(&live_stream, &meter, &tail);
    }
}
