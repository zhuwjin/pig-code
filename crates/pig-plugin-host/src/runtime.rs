use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use rquickjs::{Context, Function, Runtime};
use serde_json::{Map, Value};

pub struct HostCall {
    data: Value,
}

impl HostCall {
    pub fn data(&self) -> &Value {
        &self.data
    }

    pub fn resolve(self, _value: Value) {}

    pub fn reject(self, error: String) {
        tracing::warn!(%error, "plugin host call rejected");
    }
}

type ChannelHandler = Arc<dyn Fn(HostCall) + Send + Sync>;

struct HostChannelInner {
    params: Mutex<Map<String, Value>>,
    handlers: Mutex<HashMap<String, ChannelHandler>>,
    inbound_tx: Mutex<Option<mpsc::Sender<(String, Value)>>>,
}

#[derive(Clone)]
pub struct HostChannel {
    inner: Arc<HostChannelInner>,
}

impl HostChannel {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(HostChannelInner {
                params: Mutex::new(Map::new()),
                handlers: Mutex::new(HashMap::new()),
                inbound_tx: Mutex::new(None),
            }),
        }
    }

    pub fn set_params(&self, params: Map<String, Value>) {
        *self
            .inner
            .params
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = params;
    }

    pub fn handle<F>(&self, channel: &str, handler: F)
    where
        F: Fn(HostCall) + Send + Sync + 'static,
    {
        self.inner
            .handlers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(channel.to_string(), Arc::new(handler));
    }

    pub fn send(&self, channel: &str, data: Value) -> Result<(), String> {
        let sender = self
            .inner
            .inbound_tx
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
            .ok_or_else(|| "plugin runtime is not attached".to_string())?;
        sender
            .send((channel.to_string(), data))
            .map_err(|_| "plugin runtime thread exited".to_string())
    }

    fn dispatch_from_js(&self, channel: &str, json: &str) {
        let handler = self
            .inner
            .handlers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(channel)
            .cloned();
        let Some(handler) = handler else {
            return;
        };
        let data = serde_json::from_str::<Value>(json).unwrap_or(Value::Null);
        handler(HostCall { data });
    }

    fn params_json(&self) -> String {
        let params = self
            .inner
            .params
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        serde_json::to_string(&*params).unwrap_or_else(|_| "{}".to_string())
    }

    fn attach_sender(&self, tx: mpsc::Sender<(String, Value)>) {
        *self
            .inner
            .inbound_tx
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(tx);
    }
}

impl Default for HostChannel {
    fn default() -> Self {
        Self::new()
    }
}

pub struct EngineLimits {
    pub memory_bytes: usize,
    pub load_budget: Duration,
    pub call_budget: Duration,
}

impl Default for EngineLimits {
    fn default() -> Self {
        Self {
            memory_bytes: 64 * 1024 * 1024,
            load_budget: Duration::from_secs(3),
            call_budget: Duration::from_millis(750),
        }
    }
}

pub struct PluginEngine {
    stop: Arc<AtomicBool>,
    tx: Option<mpsc::Sender<(String, Value)>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for PluginEngine {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.tx.take();
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }
}

const HOST_BOOTSTRAP_JS: &str = r#"
(function() {
  const listeners = new Map();
  let nextTimerId = 1;
  const timers = new Map();

  globalThis.__pigHost = {
    available() { return true; },
    get params() { return JSON.parse(globalThis.__pigHostParamsJson()); },
    post(channel, data) {
      globalThis.__pigHostPost(String(channel), JSON.stringify(data === undefined ? null : data));
    },
    on(channel, listener) {
      const key = String(channel);
      if (!listeners.has(key)) listeners.set(key, new Set());
      listeners.get(key).add(listener);
      return () => {
        const set = listeners.get(key);
        if (set) set.delete(listener);
      };
    }
  };

  globalThis.__pigDeliver = function(channel, json) {
    const set = listeners.get(String(channel));
    if (!set || set.size === 0) return;
    const data = JSON.parse(json);
    for (const listener of [...set]) {
      listener(data);
    }
  };

  globalThis.setTimeout = function(fn, delay) {
    const id = nextTimerId++;
    const ms = Math.max(0, Number(delay) || 0);
    timers.set(id, fn);
    globalThis.__pigScheduleTimer(id, ms);
    return id;
  };

  globalThis.clearTimeout = function(id) {
    timers.delete(Number(id));
    globalThis.__pigCancelTimer(Number(id));
  };

  globalThis.__pigFireTimer = function(id) {
    const fn = timers.get(id);
    if (!fn) return;
    timers.delete(id);
    fn();
  };

  globalThis.queueMicrotask = function(fn) {
    Promise.resolve().then(fn);
  };

  globalThis.console = {
    log() {},
    info() {},
    warn() {},
    error() {},
    debug() {},
  };
})();
"#;

impl PluginEngine {
    pub fn start<F>(
        source: String,
        host: HostChannel,
        limits: EngineLimits,
        on_error: F,
    ) -> Result<Self, String>
    where
        F: Fn(String) + Send + 'static,
    {
        let (tx, rx) = mpsc::channel::<(String, Value)>();
        let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<(), String>>(1);
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        host.attach_sender(tx.clone());

        let thread = std::thread::Builder::new()
            .name("pig-plugin-js".to_string())
            .spawn(move || {
                let run_result = run_js_thread(
                    source,
                    host,
                    limits,
                    rx,
                    stop_thread,
                    ready_tx,
                    &on_error,
                );
                if let Err(error) = run_result {
                    on_error(error);
                }
            })
            .map_err(|error| format!("spawn plugin runtime thread: {error}"))?;

        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                stop,
                tx: Some(tx),
                thread: Some(thread),
            }),
            Ok(Err(error)) => {
                let _ = thread.join();
                Err(error)
            }
            Err(_) => {
                let _ = thread.join();
                Err("plugin runtime thread terminated during startup".to_string())
            }
        }
    }
}

fn now_epoch_ms(origin: Instant) -> u64 {
    origin.elapsed().as_millis().min(u64::MAX as u128) as u64
}

fn run_js_thread<F>(
    source: String,
    host: HostChannel,
    limits: EngineLimits,
    rx: mpsc::Receiver<(String, Value)>,
    stop: Arc<AtomicBool>,
    ready_tx: mpsc::SyncSender<Result<(), String>>,
    on_error: &F,
) -> Result<(), String>
where
    F: Fn(String),
{
    let origin = Instant::now();
    let deadline_ms = Arc::new(AtomicU64::new(0));
    let runtime = Runtime::new().map_err(|error| format!("init QuickJS runtime: {error}"))?;
    runtime.set_memory_limit(limits.memory_bytes);
    let interrupt_deadline = deadline_ms.clone();
    let interrupt_stop = stop.clone();
    runtime.set_interrupt_handler(Some(Box::new(move || {
        if interrupt_stop.load(Ordering::Relaxed) {
            return true;
        }
        let limit = interrupt_deadline.load(Ordering::Relaxed);
        limit != 0 && now_epoch_ms(origin) >= limit
    })));
    let context = Context::full(&runtime).map_err(|error| format!("init QuickJS context: {error}"))?;

    let timers: Arc<Mutex<HashMap<u64, Instant>>> = Arc::new(Mutex::new(HashMap::new()));

    let startup = (|| -> Result<(), String> {
        let load_deadline = now_epoch_ms(origin)
            .saturating_add(limits.load_budget.as_millis() as u64)
            .max(1);
        deadline_ms.store(load_deadline, Ordering::Relaxed);

        context.with(|ctx| -> Result<(), String> {
            let globals = ctx.globals();
            let post_host = host.clone();
            let post_fn = Function::new(ctx.clone(), move |channel: String, json: String| {
                post_host.dispatch_from_js(&channel, &json);
            })
            .map_err(|error| error.to_string())?;
            globals
                .set("__pigHostPost", post_fn)
                .map_err(|error| error.to_string())?;

            let params_host = host.clone();
            let params_fn = Function::new(ctx.clone(), move || params_host.params_json())
                .map_err(|error| error.to_string())?;
            globals
                .set("__pigHostParamsJson", params_fn)
                .map_err(|error| error.to_string())?;

            let schedule_timers = timers.clone();
            let schedule_fn = Function::new(ctx.clone(), move |id: u64, delay_ms: u64| {
                schedule_timers
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .insert(id, Instant::now() + Duration::from_millis(delay_ms));
            })
            .map_err(|error| error.to_string())?;
            globals
                .set("__pigScheduleTimer", schedule_fn)
                .map_err(|error| error.to_string())?;

            let cancel_timers = timers.clone();
            let cancel_fn = Function::new(ctx.clone(), move |id: u64| {
                cancel_timers
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .remove(&id);
            })
            .map_err(|error| error.to_string())?;
            globals
                .set("__pigCancelTimer", cancel_fn)
                .map_err(|error| error.to_string())?;

            ctx.eval::<(), _>(HOST_BOOTSTRAP_JS)
                .map_err(|error| format!("plugin bootstrap error: {error}"))?;
            ctx.eval::<(), _>(source.as_str())
                .map_err(|error| format!("plugin load error: {error}"))?;
            Ok(())
        })?;
        drain_jobs(&runtime)?;
        deadline_ms.store(0, Ordering::Relaxed);
        Ok(())
    })();

    if let Err(error) = startup {
        let _ = ready_tx.send(Err(error));
        return Ok(());
    }
    let _ = ready_tx.send(Ok(()));

    while !stop.load(Ordering::Relaxed) {
        let timeout = next_timer_wait(&timers).unwrap_or(Duration::from_millis(250));
        match rx.recv_timeout(timeout) {
            Ok((channel, payload)) => {
                let encoded = serde_json::to_string(&payload).unwrap_or_else(|_| "null".to_string());
                let call_deadline = now_epoch_ms(origin)
                    .saturating_add(limits.call_budget.as_millis() as u64)
                    .max(1);
                deadline_ms.store(call_deadline, Ordering::Relaxed);
                let result = context.with(|ctx| -> Result<(), String> {
                    let deliver: Function = ctx
                        .globals()
                        .get("__pigDeliver")
                        .map_err(|error| error.to_string())?;
                    deliver
                        .call::<_, ()>((channel, encoded))
                        .map_err(|error| format!("plugin message handler error: {error}"))?;
                    Ok(())
                });
                let drained = result.and_then(|()| drain_jobs(&runtime));
                deadline_ms.store(0, Ordering::Relaxed);
                if let Err(error) = drained {
                    on_error(error);
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }

        let due = take_due_timers(&timers);
        for timer_id in due {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            let call_deadline = now_epoch_ms(origin)
                .saturating_add(limits.call_budget.as_millis() as u64)
                .max(1);
            deadline_ms.store(call_deadline, Ordering::Relaxed);
            let result = context.with(|ctx| -> Result<(), String> {
                let fire: Function = ctx
                    .globals()
                    .get("__pigFireTimer")
                    .map_err(|error| error.to_string())?;
                fire.call::<_, ()>((timer_id,))
                    .map_err(|error| format!("plugin timer error: {error}"))?;
                Ok(())
            });
            let drained = result.and_then(|()| drain_jobs(&runtime));
            deadline_ms.store(0, Ordering::Relaxed);
            if let Err(error) = drained {
                on_error(error);
                return Ok(());
            }
        }
    }
    Ok(())
}

fn drain_jobs(runtime: &Runtime) -> Result<(), String> {
    while runtime.is_job_pending() {
        runtime
            .execute_pending_job()
            .map_err(|error| format!("plugin microtask error: {error:?}"))?;
    }
    Ok(())
}

fn next_timer_wait(timers: &Mutex<HashMap<u64, Instant>>) -> Option<Duration> {
    let now = Instant::now();
    let guard = timers
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let earliest = guard.values().copied().min()?;
    Some(earliest.saturating_duration_since(now).min(Duration::from_millis(250)))
}

fn take_due_timers(timers: &Mutex<HashMap<u64, Instant>>) -> Vec<u64> {
    let now = Instant::now();
    let mut guard = timers
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let due: Vec<u64> = guard
        .iter()
        .filter(|(_, at)| **at <= now)
        .map(|(id, _)| *id)
        .collect();
    for id in &due {
        guard.remove(id);
    }
    due
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_channel_round_trips_messages_and_enforces_budget() {
        let host = HostChannel::new();
        host.set_params(
            serde_json::json!({ "plugin": { "instance": "demo" } })
                .as_object()
                .unwrap()
                .clone(),
        );
        let (seen_tx, seen_rx) = mpsc::channel::<Value>();
        host.handle("plugin.protocol", move |call| {
            let _ = seen_tx.send(call.data().clone());
        });
        let source = r#"
            __pigHost.on("plugin.protocol", (msg) => {
                __pigHost.post("plugin.protocol", { echoed: msg.ping, instance: __pigHost.params.plugin.instance });
            });
            __pigHost.post("plugin.protocol", { ready: true });
        "#;
        let engine = PluginEngine::start(
            source.to_string(),
            host.clone(),
            EngineLimits::default(),
            |err| panic!("unexpected js error: {err}"),
        )
        .expect("start engine");

        assert_eq!(
            seen_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            serde_json::json!({ "ready": true })
        );
        host.send("plugin.protocol", serde_json::json!({ "ping": "pong" }))
            .expect("send inbound");
        assert_eq!(
            seen_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            serde_json::json!({ "echoed": "pong", "instance": "demo" })
        );
        drop(engine);

        // Infinite loop must trip the load budget interrupt handler cleanly.
        let timeout_host = HostChannel::new();
        let err = PluginEngine::start(
            "while (true) {}".to_string(),
            timeout_host,
            EngineLimits {
                memory_bytes: 16 * 1024 * 1024,
                load_budget: Duration::from_millis(80),
                call_budget: Duration::from_millis(80),
            },
            |_| {},
        )
        .err()
        .expect("infinite loop must fail");
        assert!(!err.is_empty());
    }
}
