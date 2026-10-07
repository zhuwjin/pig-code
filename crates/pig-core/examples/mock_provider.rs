//! A standalone mock provider for GUI manual self-testing:
//! `cargo run -p pig-core --example mock_provider`
//! then point ~/.pigcode/config.toml at the printed base_url.

fn main() {
    let port = pig_core::mock::start_mock_server();
    println!("mock provider started: base_url = \"http://127.0.0.1:{port}/v1\"");
    println!(
        "Behavior: the first request returns a Read tool call (reading {}); once the tool result is in, it streams Markdown (with reasoning_content).",
        pig_core::mock::MOCK_FILE_NAME
    );
    println!(
        "The agent's working directory must contain the {} file.",
        pig_core::mock::MOCK_FILE_NAME
    );
    loop {
        std::thread::park();
    }
}
