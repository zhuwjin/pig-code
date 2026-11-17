use std::io::Write as _;

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    let bind = std::env::args()
        .skip(1)
        .find_map(|argument| argument.strip_prefix("--bind=").map(str::to_string))
        .unwrap_or_else(|| "127.0.0.1:0".to_string());
    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .unwrap_or_else(|error| panic!("failed to bind Pig Agent worker on {bind}: {error}"));
    let address = listener.local_addr().expect("worker address");
    println!("PIG_AGENT_WORKER_ADDR={address}");
    let _ = std::io::stdout().flush();
    if let Err(error) = pig_core::agent::ipc::serve_ipc_worker(listener).await {
        eprintln!("Pig Agent worker failed: {error}");
        std::process::exit(1);
    }
}
