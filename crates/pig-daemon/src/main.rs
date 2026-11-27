use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use pig_daemon::relay::{
    load_saved_relay_credentials, login_headless, logout_headless, pair_headless,
    relay_credentials_path,
};
use pig_daemon::{
    DAEMON_PROTOCOL_VERSION, DaemonClient, DaemonConfig, DaemonEndpoint, DaemonMessage,
    serve_with_ready,
};

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

fn print_help() {
    println!(
        "pig-daemon — persistent Pig Code core daemon\n\n\
         USAGE:\n  \
           pig-daemon [OPTIONS]\n  \
           pig-daemon relay <pair|login|status|logout> [OPTIONS]\n\n\
         DAEMON OPTIONS:\n  \
           --data-dir=<PATH>       Data directory (default: ~/.pigcode)\n  \
           --cwd=<PATH>            Default workspace directory\n  \
           --config=<PATH>         Explicit config.toml path\n  \
           --daemonize             Start detached background daemon and print endpoint JSON\n  \
           --discover              Print running daemon endpoint JSON or exit 1\n  \
           --print-endpoint        Print endpoint JSON on startup\n  \
           --selftest              Start isolated self-test daemon with mock provider\n  \
           -h, --help              Show this help message\n\n\
         RELAY SUBCOMMANDS:\n  \
           pig-daemon relay pair <RELAY_URL> [--name <DEVICE_NAME>] [--data-dir <PATH>]\n  \
           pig-daemon relay login <RELAY_URL> --username <USER> [--password <PASS>] [--name <DEVICE_NAME>] [--register] [--data-dir <PATH>]\n  \
           pig-daemon relay status [--data-dir <PATH>]\n  \
           pig-daemon relay logout [--data-dir <PATH>]"
    );
}

async fn call_live_daemon_rpc(
    endpoint: DaemonEndpoint,
    method: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let client = DaemonClient::connect(endpoint, None).await?;
    let id = uuid::Uuid::new_v4().to_string();
    client.send_rpc(id.clone(), method, params)?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if Instant::now() >= deadline {
            return Err(format!("timed out waiting for local daemon `{method}`"));
        }
        let Ok(msg) = tokio::time::timeout(Duration::from_secs(1), client.recv()).await else {
            continue;
        };
        let msg = msg.map_err(|error| error.to_string())?;
        if let DaemonMessage::RpcResult {
            id: resp_id,
            result,
            error,
        } = msg
            && resp_id == id
        {
            if let Some(err) = error {
                return Err(err);
            }
            return Ok(result.unwrap_or(serde_json::Value::Null));
        }
    }
}

async fn notify_live_daemon_reload(data_dir: &Path) -> bool {
    let Some(endpoint) = read_live_endpoint(data_dir) else {
        return false;
    };
    call_live_daemon_rpc(endpoint, "relay.reload", serde_json::json!({}))
        .await
        .is_ok()
}

async fn run_relay_cli(args: &[String]) -> Result<(), String> {
    let Some(subcmd) = args.first().map(String::as_str) else {
        print_help();
        return Ok(());
    };
    if matches!(subcmd, "--help" | "-h") {
        print_help();
        return Ok(());
    }

    let mut data_dir = pig_utils::data_dir();
    let mut relay_url: Option<String> = None;
    let mut device_name: Option<String> = None;
    let mut username: Option<String> = None;
    let mut password: Option<String> = std::env::var("PIG_RELAY_PASSWORD").ok();
    let mut register = false;

    let mut iter = args[1..].iter();
    while let Some(arg) = iter.next() {
        if let Some(val) = arg.strip_prefix("--data-dir=") {
            data_dir = PathBuf::from(val);
        } else if arg == "--data-dir" {
            data_dir = PathBuf::from(iter.next().ok_or("--data-dir requires a path")?);
        } else if let Some(val) = arg.strip_prefix("--name=") {
            device_name = Some(val.to_string());
        } else if arg == "--name" || arg == "--device-name" {
            device_name = Some(iter.next().ok_or("--name requires a value")?.clone());
        } else if let Some(val) = arg.strip_prefix("--device-name=") {
            device_name = Some(val.to_string());
        } else if let Some(val) = arg.strip_prefix("--url=") {
            relay_url = Some(val.to_string());
        } else if arg == "--url" {
            relay_url = Some(iter.next().ok_or("--url requires a relay URL")?.clone());
        } else if let Some(val) = arg.strip_prefix("--username=") {
            username = Some(val.to_string());
        } else if arg == "--username" || arg == "-u" {
            username = Some(iter.next().ok_or("--username requires a value")?.clone());
        } else if let Some(val) = arg.strip_prefix("--password=") {
            password = Some(val.to_string());
        } else if arg == "--password" || arg == "-p" {
            password = Some(iter.next().ok_or("--password requires a value")?.clone());
        } else if arg == "--register" {
            register = true;
        } else if matches!(arg.as_str(), "--help" | "-h") {
            print_help();
            return Ok(());
        } else if !arg.starts_with('-') && relay_url.is_none() {
            relay_url = Some(arg.clone());
        } else {
            return Err(format!("unknown relay argument: {arg}"));
        }
    }

    match subcmd {
        "pair" => {
            let url = relay_url.ok_or(
                "missing relay URL (usage: pig-daemon relay pair <RELAY_URL> [--name <DEVICE_NAME>])",
            )?;
            let creds = pair_headless(&data_dir, &url, device_name.as_deref()).await?;
            println!(
                "Paired device \"{}\" (account: {}, device_id: {})",
                creds.device_name, creds.username, creds.device_id
            );
            println!(
                "Saved credentials to {}",
                relay_credentials_path(&data_dir).display()
            );
            if notify_live_daemon_reload(&data_dir).await {
                println!("Reloaded running pig-daemon — tunnel is connecting now.");
            } else {
                println!("Start pig-daemon (or `pig-daemon --daemonize`) to bring this device online.");
            }
            Ok(())
        }
        "login" => {
            let url = relay_url.ok_or(
                "missing relay URL (usage: pig-daemon relay login <RELAY_URL> --username <USER> [--password <PASS>] [--register])",
            )?;
            let user = username.ok_or("missing --username")?;
            let pass = match password {
                Some(p) if !p.is_empty() => p,
                _ => {
                    eprint!("Password: ");
                    let _ = std::io::stderr().flush();
                    let mut line = String::new();
                    std::io::stdin()
                        .read_line(&mut line)
                        .map_err(|error| error.to_string())?;
                    line.trim_end_matches(['\r', '\n']).to_string()
                }
            };
            let creds = login_headless(
                &data_dir,
                &url,
                &user,
                &pass,
                device_name.as_deref(),
                register,
            )
            .await?;
            println!(
                "Signed in as {} on {} (device: \"{}\", id: {})",
                creds.username, creds.relay_url, creds.device_name, creds.device_id
            );
            if notify_live_daemon_reload(&data_dir).await {
                println!("Reloaded running pig-daemon — tunnel is connecting now.");
            } else {
                println!("Start pig-daemon (or `pig-daemon --daemonize`) to bring this device online.");
            }
            Ok(())
        }
        "status" => {
            if let Some(endpoint) = read_live_endpoint(&data_dir)
                && let Ok(status) =
                    call_live_daemon_rpc(endpoint, "relay.status", serde_json::json!({})).await
            {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&status).unwrap_or_else(|_| status.to_string())
                );
                return Ok(());
            }
            if let Some(creds) = load_saved_relay_credentials(&data_dir) {
                println!("daemon:      stopped");
                println!("relay_url:   {}", creds.relay_url);
                println!("username:    {}", creds.username);
                println!("device_name: {}", creds.device_name);
                println!("device_id:   {}", creds.device_id);
            } else {
                println!("Not paired with any relay server.");
            }
            Ok(())
        }
        "logout" => {
            if let Some(endpoint) = read_live_endpoint(&data_dir)
                && call_live_daemon_rpc(endpoint, "relay.logout", serde_json::json!({}))
                    .await
                    .is_ok()
            {
                println!("Signed out and disconnected running pig-daemon.");
                return Ok(());
            }
            logout_headless(&data_dir).await?;
            println!("Removed saved relay credentials.");
            Ok(())
        }
        other => Err(format!(
            "unknown relay subcommand `{other}` (expected pair, login, status, or logout)"
        )),
    }
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    let raw_args: Vec<String> = std::env::args().skip(1).collect();
    if raw_args.first().map(String::as_str) == Some("relay") {
        if let Err(error) = run_relay_cli(&raw_args[1..]).await {
            eprintln!("pig-daemon relay failed: {error}");
            std::process::exit(1);
        }
        return;
    }
    if raw_args.iter().any(|arg| arg == "--help" || arg == "-h") {
        print_help();
        return;
    }

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
    for argument in raw_args {
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
