use std::io::Write as _;

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    let bind = std::env::args()
        .skip(1)
        .find_map(|argument| argument.strip_prefix("--bind=").map(str::to_string))
        .unwrap_or_else(|| "127.0.0.1:0".to_string());
    let state_dir = std::env::args()
        .skip(1)
        .find_map(|argument| argument.strip_prefix("--state-dir=").map(str::to_string))
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| panic!("Pig Agent worker requires --state-dir"));
    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .unwrap_or_else(|error| panic!("failed to bind Pig Agent worker on {bind}: {error}"));
    let address = listener.local_addr().expect("worker address");
    println!("PIG_AGENT_WORKER_ADDR={address}");
    let _ = std::io::stdout().flush();
    if let Err(error) = pig_core::agent::ipc::serve_ipc_worker(listener, state_dir).await {
        eprintln!("Pig Agent worker failed: {error}");
        std::process::exit(1);
    }
}
