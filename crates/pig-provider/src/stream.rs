use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use pig_protocol::ApiFormat;

use crate::chat::{ChatMsg, ResolvedModel};
use crate::events::{CallControl, ProviderEvent, RetryNotice, RetryReason};
use crate::{anthropic, core_error_en, openai};

pub async fn stream_chat(
    config: ResolvedModel,
    messages: Vec<ChatMsg>,
    tools: Vec<serde_json::Value>,
    tx: tokio::sync::mpsc::UnboundedSender<ProviderEvent>,
    control: CallControl,
) {
    // Empty-completion guard (same anomaly class as ZCode's
    // empty-completion-retry): a 200 stream that ends with no text, no
    // reasoning and no tool calls gets ONE silent retry. Events stream through
    // live EXCEPT Finished, which the forwarder holds back until the outcome
    // is known — the session treats Finished as step end, so an empty attempt
    // must never emit Finished followed by the retry's content.
    let mut retried = false;
    let mut tx = tx;
    loop {
        let (itx, mut irx) = tokio::sync::mpsc::unbounded_channel::<ProviderEvent>();
        let saw_content = Arc::new(AtomicBool::new(false));
        let flag = saw_content.clone();
        let forward = tokio::spawn(async move {
            let mut held_finish = false;
            while let Some(event) = irx.recv().await {
                if matches!(event, ProviderEvent::Finished) {
                    held_finish = true;
                    continue;
                }
                if matches!(
                    event,
                    ProviderEvent::Text(_)
                        | ProviderEvent::Reasoning(_)
                        | ProviderEvent::ToolCalls(_)
                ) {
                    flag.store(true, Ordering::Relaxed);
                }
                if tx.send(event).is_err() {
                    break;
                }
            }
            (tx, held_finish)
        });
        let result = match config.api_format {
            ApiFormat::OpenAiChat => {
                openai::stream_openai(&config, messages.clone(), tools.clone(), &itx, &control)
                    .await
            }
            ApiFormat::AnthropicMessages => {
                anthropic::stream_anthropic(
                    &config,
                    messages.clone(),
                    tools.clone(),
                    &itx,
                    &control,
                )
                .await
            }
        };
        drop(itx);
        let Ok((back, held_finish)) = forward.await else {
            return;
        };
        tx = back;
        match result {
            Err(call_error) => {
                // Retryable in-band error (Anthropic overload/rate-limit
                // arriving as a 200 SSE error event): one silent retry, only
                // while nothing visible was forwarded yet
                if call_error.retryable
                    && !saw_content.load(Ordering::Relaxed)
                    && !retried
                    && !control.is_cancelled()
                {
                    retried = true;
                    tracing::warn!(
                        "provider reported a retryable in-band error, retrying once: {}",
                        core_error_en(&call_error.error)
                    );
                    let _ = tx.send(ProviderEvent::Retrying(RetryNotice {
                        attempt: 1,
                        max_attempts: 1,
                        delay: Duration::ZERO,
                        reason: RetryReason::InBand(core_error_en(&call_error.error)),
                    }));
                    continue;
                }
                let _ = tx.send(ProviderEvent::Failed(call_error.error));
                return;
            }
            Ok(()) => {
                let empty = !saw_content.load(Ordering::Relaxed);
                if empty && !retried && !control.is_cancelled() {
                    retried = true;
                    tracing::warn!(
                        "provider stream completed empty (no text/reasoning/tool calls), retrying once"
                    );
                    let _ = tx.send(ProviderEvent::Retrying(RetryNotice {
                        attempt: 1,
                        max_attempts: 1,
                        delay: Duration::ZERO,
                        reason: RetryReason::EmptyCompletion,
                    }));
                    continue;
                }
                if held_finish {
                    let _ = tx.send(ProviderEvent::Finished);
                }
                return;
            }
        }
    }
}
