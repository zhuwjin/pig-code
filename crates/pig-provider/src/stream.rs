use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use pig_protocol::ApiFormat;

use crate::chat::{ChatMsg, ResolvedModel};
use crate::events::{CallControl, ProviderEvent, RetryNotice, RetryReason};
use crate::{anthropic, core_error_en, openai, responses};

pub async fn stream_chat(
    config: ResolvedModel,
    messages: Vec<ChatMsg>,
    tools: Vec<serde_json::Value>,
    tx: tokio::sync::mpsc::UnboundedSender<ProviderEvent>,
    control: CallControl,
) {
    // Empty-completion guard (same anomaly class as ZCode's
    // empty-completion-retry): a 200 stream that ends with no text, no
    // reasoning/native blocks and no tool calls gets ONE silent retry. Events stream through
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
            let mut held_finish = None;
            // Usage is the accounting boundary: hold it until the attempt is
            // known not to be an empty-completion retry, otherwise both the
            // discarded attempt and its replacement are counted.
            let mut held_usage = Vec::new();
            while let Some(event) = irx.recv().await {
                match &event {
                    ProviderEvent::Finished(reason) => {
                        held_finish = Some(*reason);
                        continue;
                    }
                    ProviderEvent::Usage { .. } => {
                        held_usage.push(event);
                        continue;
                    }
                    _ => {}
                }
                let has_content = match &event {
                    ProviderEvent::Text(_)
                    | ProviderEvent::Reasoning(_)
                    | ProviderEvent::ToolCalls(_) => true,
                    ProviderEvent::AssistantReplay(replay) => !replay.items.is_empty(),
                    _ => false,
                };
                if has_content {
                    flag.store(true, Ordering::Relaxed);
                }
                if tx.send(event).is_err() {
                    break;
                }
            }
            (tx, held_finish, held_usage)
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
            ApiFormat::OpenAiResponses => {
                responses::stream_responses(
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
        let Ok((back, held_finish, held_usage)) = forward.await else {
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
                for usage in held_usage {
                    let _ = tx.send(usage);
                }
                let _ = tx.send(ProviderEvent::Failed(call_error.error));
                return;
            }
            Ok(()) => {
                let empty = !saw_content.load(Ordering::Relaxed);
                if empty
                    && matches!(held_finish, Some(pig_protocol::FinishReason::Stop))
                    && !retried
                    && !control.is_cancelled()
                {
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
                // The final attempt's usage is the only one that counts; the
                // retried attempt's held usage was dropped with `continue`.
                for usage in held_usage {
                    let _ = tx.send(usage);
                }
                if let Some(reason) = held_finish {
                    let _ = tx.send(ProviderEvent::Finished(reason));
                }
                return;
            }
        }
    }
}
