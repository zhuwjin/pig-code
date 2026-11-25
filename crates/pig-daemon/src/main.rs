use std::path::PathBuf;

use pig_daemon::{DaemonConfig, serve};

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    let mut config = DaemonConfig {
        data_dir: PathBuf::new(),
        cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        config_path: None,
        selftest: std::env::var_os("PIG_SELFTEST").is_some(),
    };
    for argument in std::env::args().skip(1) {
        if let Some(path) = argument.strip_prefix("--data-dir=") {
            config.data_dir = PathBuf::from(path);
        } else if let Some(path) = argument.strip_prefix("--cwd=") {
            config.cwd = PathBuf::from(path);
        } else if let Some(path) = argument.strip_prefix("--config=") {
            config.config_path = Some(PathBuf::from(path));
        }
    }
    if config.data_dir.as_os_str().is_empty() {
        eprintln!("pig-daemon requires --data-dir");
        std::process::exit(2);
    }
    if let Err(error) = serve(config).await {
        eprintln!("pig-daemon failed: {error}");
        std::process::exit(1);
    }
}
