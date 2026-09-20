//! 单独运行的 mock provider，供 GUI 手动自测：
//! `cargo run -p pig-core --example mock_provider`
//! 然后在 ~/.pigcode/config.toml 指向打印出的 base_url 即可。

fn main() {
    let port = pig_core::mock::start_mock_server();
    println!("mock provider 已启动: base_url = \"http://127.0.0.1:{port}/v1\"");
    println!("行为: 首轮请求返回 read_file 工具调用（读取 {}），含工具结果后返回流式 Markdown（带 reasoning_content）。", pig_core::mock::MOCK_FILE_NAME);
    println!("agent 的工作目录里需要有 {} 文件。", pig_core::mock::MOCK_FILE_NAME);
    loop {
        std::thread::park();
    }
}
