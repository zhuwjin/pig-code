use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use pig_daemon::{DAEMON_PROTOCOL_VERSION, DaemonConfig, DaemonEndpoint, serve_with_ready};

struct TempDirGuard(PathBuf);

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn read_live_endpoint(data_dir: &Path) -> Option<DaemonEndpoint> {
    let bytes = std::fs::read(data_dir.join("daemon").join("endpoint.json")).ok()?;
    let endpoint: DaemonEndpoint = serde_json::from_slice(&bytes).ok()?;
    if endpoint.protocol != DAEMON_PROTOCOL_VERSION {
        return None;
    }
    std::net::TcpStream::connect_timeout(&endpoint.address, Duration::from_millis(150))
        .ok()
        .map(|_| endpoint)
}

fn daemonize_and_print(
    data_dir: &Path,
    cwd: &Path,
    config_path: Option<&Path>,
    selftest: bool,
) -> Result<(), String> {
    if let Some(endpoint) = read_live_endpoint(data_dir) {
        println!(
            "{}",
            serde_json::to_string(&endpoint).map_err(|error| error.to_string())?
        );
        return Ok(());
    }
    let previous_token = std::fs::read(data_dir.join("daemon").join("endpoint.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<DaemonEndpoint>(&bytes).ok())
        .map(|endpoint| endpoint.token);
    let executable = std::env::current_exe().map_err(|error| error.to_string())?;
    let mut command = std::process::Command::new(executable);
    command
        .arg(format!("--data-dir={}", data_dir.display()))
        .arg(format!("--cwd={}", cwd.display()))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    if let Some(config_path) = config_path {
        command.arg(format!("--config={}", config_path.display()));
    }
    if selftest {
        command.env("PIG_SELFTEST", "1");
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        command.creation_flags(DETACHED_PROCESS | CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("failed to launch detached pig-daemon: {error}"))?;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let child_exited = matches!(child.try_wait(), Ok(Some(_)));
        if let Some(endpoint) = read_live_endpoint(data_dir)
            && (Some(&endpoint.token) != previous_token.as_ref() || child_exited)
        {
            println!(
                "{}",
                serde_json::to_string(&endpoint).map_err(|error| error.to_string())?
            );
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err("detached pig-daemon did not publish a live endpoint".to_string());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn setup_selftest_dir() -> Result<(TempDirGuard, DaemonConfig), String> {
    let root = std::env::temp_dir().join(format!(
        "pig-daemon-selftest-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ));
    let workspace = root.join("workspace");
    let data_dir = root.join("data");
    std::fs::create_dir_all(&workspace).map_err(|error| error.to_string())?;
    std::fs::create_dir_all(&data_dir).map_err(|error| error.to_string())?;
    std::fs::write(
        workspace.join(pig_provider::mock::MOCK_FILE_NAME),
        pig_provider::mock::MOCK_FILE_CONTENT,
    )
    .map_err(|error| error.to_string())?;
    let port = pig_provider::mock::start_mock_server();
    let config_path = root.join("config.toml");
    std::fs::write(
        &config_path,
        format!(
            r#"default_provider = "mock"
default_model = "mock-model"

[[providers]]
id = "mock"
name = "Mock"
base_url = "http://127.0.0.1:{port}/v1"
api_key = "mock-key"
api_format = "OpenAiChat"
enabled = true

[[providers.models]]
id = "mock-model"
context_window = 128000
max_output_tokens = 8192
input_image = true
reasoning_levels = ["low", "high"]
default_reasoning_level = "low"
"#
        ),
    )
    .map_err(|error| error.to_string())?;
    let config = DaemonConfig {
        data_dir,
        cwd: workspace,
        config_path: Some(config_path),
        selftest: true,
    };
    Ok((TempDirGuard(root), config))
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    let mut config = DaemonConfig {
        data_dir: PathBuf::new(),
        cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        config_path: None,
        selftest: std::env::var_os("PIG_SELFTEST").is_some(),
    };
    let mut daemonize = false;
    let mut discover = false;
    let mut print_endpoint = false;
    let mut selftest_flag = false;
    for argument in std::env::args().skip(1) {
        if let Some(path) = argument.strip_prefix("--data-dir=") {
            config.data_dir = PathBuf::from(path);
        } else if let Some(path) = argument.strip_prefix("--cwd=") {
            config.cwd = PathBuf::from(path);
        } else if let Some(path) = argument.strip_prefix("--config=") {
            config.config_path = Some(PathBuf::from(path));
        } else if argument == "--daemonize" {
            daemonize = true;
        } else if argument == "--discover" {
            discover = true;
        } else if argument == "--print-endpoint" {
            print_endpoint = true;
        } else if argument == "--selftest" {
            selftest_flag = true;
            print_endpoint = true;
        }
    }
    let _selftest_guard = if selftest_flag && config.data_dir.as_os_str().is_empty() {
        match setup_selftest_dir() {
            Ok((guard, selftest_config)) => {
                config = selftest_config;
                Some(guard)
            }
            Err(error) => {
                eprintln!("pig-daemon selftest setup failed: {error}");
                std::process::exit(1);
            }
        }
    } else {
        if config.data_dir.as_os_str().is_empty() {
            config.data_dir = pig_utils::data_dir();
        }
        None
    };
    if discover {
        if let Some(endpoint) = read_live_endpoint(&config.data_dir) {
            match serde_json::to_string(&endpoint) {
                Ok(encoded) => {
                    println!("{encoded}");
                    return;
                }
                Err(error) => {
                    eprintln!("pig-daemon discover failed: {error}");
                    std::process::exit(1);
                }
            }
        }
        std::process::exit(1);
    }
    if daemonize {
        if let Err(error) = daemonize_and_print(
            &config.data_dir,
            &config.cwd,
            config.config_path.as_deref(),
            config.selftest,
        ) {
            eprintln!("pig-daemon daemonize failed: {error}");
            std::process::exit(1);
        }
        return;
    }
    let ready_tx = if print_endpoint {
        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            if let Ok(endpoint) = rx.await
                && let Ok(encoded) = serde_json::to_string(&endpoint)
            {
                let _ = writeln!(std::io::stdout(), "{encoded}");
                let _ = std::io::stdout().flush();
            }
        });
        Some(tx)
    } else {
        None
    };
    if let Err(error) = serve_with_ready(config, ready_tx).await {
        eprintln!("pig-daemon failed: {error}");
        std::process::exit(1);
    }
}
