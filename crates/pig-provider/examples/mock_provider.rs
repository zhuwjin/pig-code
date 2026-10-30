//! A standalone mock provider for GUI manual self-testing:
//! `cargo run -p pig-provider --example mock_provider`
//! then point ~/.pigcodets/config.toml at the printed base_url.

fn main() {
    let port = pig_provider::mock::start_mock_server();
    println!("mock provider started: base_url = \"http://127.0.0.1:{port}/v1\"");
    println!(
        "Behavior: the first request returns a Read tool call (reading {}); once the tool result is in, it streams Markdown (with reasoning_content).",
        pig_provider::mock::MOCK_FILE_NAME
    );
    println!(
        "The agent's working directory must contain the {} file.",
        pig_provider::mock::MOCK_FILE_NAME
    );
    println!(
        "Streaming-UI demo: send \"{}\" and watch Write/Edit/Bash/Grep/Read cards grow argument by argument (writes {} into the working directory).",
        pig_provider::mock::STREAM_ALL_TRIGGER,
        pig_provider::mock::STREAM_ALL_FILE,
    );
    loop {
        std::thread::park();
    }
}
