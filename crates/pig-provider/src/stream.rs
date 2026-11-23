use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::chat::{ChatMsg, ResolvedModel};
use crate::core_error_en;
use crate::events::{CallControl, ProviderEvent, RetryNotice, RetryReason};
use crate::registry::{ProviderRegistry, no_driver_error};

pub async fn stream_chat(
    config: ResolvedModel,
    messages: Vec<ChatMsg>,
    tools: Vec<serde_json::Value>,
    tx: tokio::sync::mpsc::Sender<ProviderEvent>,
    control: CallControl,
) {
    stream_chat_with_registry(
        ProviderRegistry::global(),
        config,
        messages,
        tools,
        tx,
        control,
    )
    .await
}

pub async fn stream_chat_with_registry(
    registry: &ProviderRegistry,
    config: ResolvedModel,
    messages: Vec<ChatMsg>,
    tools: Vec<serde_json::Value>,
    tx: tokio::sync::mpsc::Sender<ProviderEvent>,
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
        let (itx, mut irx) = tokio::sync::mpsc::channel::<ProviderEvent>(256);
        let saw_content = Arc::new(AtomicBool::new(false));
        let flag = saw_content.clone();
        let forward = tokio::spawn(async move {
            let mut held_finish = None;
            // Usage is the accounting boundary: hold it until the attempt is
            // known not to be an empty-completion retry, otherwise both the
            // discarded attempt and its replacement are counted.
            let mut held_usage = None;
            while let Some(event) = irx.recv().await {
                match &event {
                    ProviderEvent::Finished(reason) => {
                        held_finish = Some(*reason);
                        continue;
                    }
                    ProviderEvent::Usage { .. } => {
                        held_usage = Some(event);
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
                if tx.send(event).await.is_err() {
                    break;
                }
            }
            (tx, held_finish, held_usage)
        });
        let result = match registry.resolve(&config) {
            Some(driver) => {
                driver
                    .stream(&config, messages.clone(), tools.clone(), &itx, &control)
                    .await
            }
            None => Err(no_driver_error(&config)),
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
                    let _ = tx
                        .send(ProviderEvent::Retrying(RetryNotice {
                            attempt: 1,
                            max_attempts: 1,
                            delay: Duration::ZERO,
                            reason: RetryReason::InBand(core_error_en(&call_error.error)),
                        }))
                        .await;
                    continue;
                }
                if let Some(usage) = held_usage {
                    let _ = tx.send(usage).await;
                }
                let _ = tx.send(ProviderEvent::Failed(call_error.error)).await;
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
                    let _ = tx
                        .send(ProviderEvent::Retrying(RetryNotice {
                            attempt: 1,
                            max_attempts: 1,
                            delay: Duration::ZERO,
                            reason: RetryReason::EmptyCompletion,
                        }))
                        .await;
                    continue;
                }
                // The final attempt's usage is the only one that counts; the
                // retried attempt's held usage was dropped with `continue`.
                if let Some(usage) = held_usage {
                    let _ = tx.send(usage).await;
                }
                if let Some(reason) = held_finish {
                    let _ = tx.send(ProviderEvent::Finished(reason)).await;
                }
                return;
            }
        }
    }
}
