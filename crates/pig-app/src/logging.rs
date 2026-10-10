//! Logging bootstrap (tracing): a daily rolling file under `{data_dir}/logs`
//! (`pig-code.log.YYYY-MM-DD`), plus stderr in debug builds (release is a
//! GUI-subsystem exe with no console).
//!
//! Environment variables:
//! - `PIG_LOG` — EnvFilter for the main log (full syntax, e.g.
//!   `PIG_LOG=pig_core=debug,hyper=warn`). Defaults: debug builds
//!   `info,pig_code=debug,pig_core=debug`, release `info`.
//! - `PIG_LOG_API=1` — model API wire log (pig-core `api_log`: full request
//!   bodies + accumulated responses). Routed to its own `pig-api.log.*` file;
//!   the main log always excludes the `pig_api` target.

use std::sync::OnceLock;

use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::{Layer as _, SubscriberExt as _};
use tracing_subscriber::util::SubscriberInitExt as _;

/// Non-blocking writer guards: kept alive for the whole process so the
/// background flush thread keeps running (dropping them stops the writer).
static GUARDS: OnceLock<Vec<tracing_appender::non_blocking::WorkerGuard>> = OnceLock::new();

/// Initialize the global subscriber. Call once at process start, after the
/// selftest data-dir isolation (PIG_DATA_DIR) is in place.
pub fn init() {
    let log_dir = pig_utils::data_dir().join("logs");
    // The main log never carries API wire dumps: `pig_api=off` is appended to
    // the default, and also to a user-supplied PIG_LOG (the API log has its
    // own file; PIG_LOG_API is the single switch)
    let default = if cfg!(debug_assertions) {
        "info,pig_code=debug,pig_core=debug"
    } else {
        "info"
    };
    let main_filter = EnvFilter::try_from_env("PIG_LOG")
        .unwrap_or_else(|_| EnvFilter::new(default))
        .add_directive("pig_api=off".parse().expect("static directive"));

    let mut guards = Vec::new();

    let (file_writer, guard) =
        tracing_appender::non_blocking(tracing_appender::rolling::daily(&log_dir, "pig-code.log"));
    guards.push(guard);
    let file_layer = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_writer(file_writer)
        .with_filter(main_filter.clone());

    // Console output in debug builds only (release has no console to write to)
    let stderr_layer = cfg!(debug_assertions).then(|| {
        tracing_subscriber::fmt::layer()
            .with_writer(std::io::stderr)
            .with_filter(main_filter)
    });

    // Model API wire log: own daily file, only when PIG_LOG_API is on
    let api_layer = pig_provider::api_log::enabled().then(|| {
        let (writer, guard) = tracing_appender::non_blocking(tracing_appender::rolling::daily(
            &log_dir,
            "pig-api.log",
        ));
        guards.push(guard);
        tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_writer(writer)
            .with_filter(
                tracing_subscriber::filter::Targets::new()
                    .with_target("pig_api", tracing::Level::TRACE),
            )
    });

    tracing_subscriber::registry()
        .with(file_layer)
        .with(stderr_layer)
        .with(api_layer)
        .init();
    let _ = GUARDS.set(guards);
    tracing::info!("logging initialized (dir: {})", log_dir.display());
}
