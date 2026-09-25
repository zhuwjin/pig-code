//! ReadMediaFile 全链路：缩放/裁剪/上限/敏感与区外防护、base64 与魔数嗅探、
//! 能力门控（input_image=false）、rollout 不落 base64。

use std::path::PathBuf;

use pig_core::provider::ToolCall;
use pig_core::task::SessionToolState;
use pig_core::tool::{self, ChangeTracker, ToolContext};
use pig_protocol::{Event, ExecMode, Op};

fn call(name: &str, args: serde_json::Value) -> ToolCall {
    ToolCall {
        id: "t1".into(),
        name: name.into(),
        arguments: args.to_string(),
    }
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pig-core-media-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.canonicalize().unwrap()
}

/// 程序化生成 PNG（rgb/rgba 可选）
fn png_bytes(w: u32, h: u32, rgba: bool) -> Vec<u8> {
    let image = if rgba {
        image::DynamicImage::new_rgba8(w, h)
    } else {
        image::DynamicImage::new_rgb8(w, h)
    };
    let mut buf = std::io::Cursor::new(Vec::new());
    image.write_to(&mut buf, image::ImageFormat::Png).unwrap();
    buf.into_inner()
}

fn jpeg_bytes(w: u32, h: u32) -> Vec<u8> {
    let mut buf = std::io::Cursor::new(Vec::new());
    image::DynamicImage::new_rgb8(w, h)
        .write_to(&mut buf, image::ImageFormat::Jpeg)
        .unwrap();
    buf.into_inner()
}

/// 手工拼一个最小合法 PNG（IHDR 声明巨大尺寸 + 空 IEND，不带图像数据）：
/// ReadMediaFile 的像素上限检查在解码前，读到尺寸就应拒绝。
fn crafted_png_with_dims(w: u32, h: u32) -> Vec<u8> {
    fn crc32(bytes: &[u8]) -> u32 {
        let mut crc: u32 = 0xFFFF_FFFF;
        for &b in bytes {
            crc ^= b as u32;
            for _ in 0..8 {
                crc = if crc & 1 == 1 {
                    (crc >> 1) ^ 0xEDB8_8320
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }
    let mut chunk = |name: &[u8; 4], data: &[u8]| {
        let mut out = (data.len() as u32).to_be_bytes().to_vec();
        let mut body = name.to_vec();
        body.extend_from_slice(data);
        let crc = crc32(&body);
        out.extend_from_slice(&body);
        out.extend_from_slice(&crc.to_be_bytes());
        out
    };
    let mut ihdr = w.to_be_bytes().to_vec();
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]); // 8bit truecolor
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    out.extend(chunk(b"IHDR", &ihdr));
    // png crate 读到 IDAT 才肯报尺寸；空 zlib 流占位（不解码，内容无所谓）
    out.extend(chunk(
        b"IDAT",
        &[0x78, 0x9c, 0x03, 0x00, 0x00, 0x00, 0x00, 0x01],
    ));
    out.extend(chunk(b"IEND", &[]));
    out
}

async fn run(
    dir: &std::path::Path,
    state: &SessionToolState,
    args: serde_json::Value,
) -> (String, bool, Vec<tool::ToolImage>) {
    let mut tracker = ChangeTracker::default();
    let (out, is_error, _, _, images) = tool::execute(
        &call("ReadMediaFile", args),
        ToolContext {
            cwd: dir,
            tracker: &mut tracker,
            state,
        },
    )
    .await;
    (out, is_error, images)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resize_to_2000_and_summary_text() {
    let dir = temp_dir("resize");
    std::fs::write(dir.join("big.png"), png_bytes(4000, 3000, false)).unwrap();
    let state = SessionToolState::for_test();

    let (out, is_error, images) = run(&dir, &state, serde_json::json!({"path": "big.png"})).await;
    assert!(!is_error, "{out}");
    assert_eq!(images.len(), 1);
    assert_eq!(
        (images[0].width, images[0].height),
        (2000, 1500),
        "等比缩放"
    );
    assert_eq!(images[0].media_type, "image/png");
    assert!(tool::sniff_image(&decode64(&images[0].data_base64)).is_some());
    assert!(out.contains("原始 4000×3000"), "{out}");
    assert!(out.contains("输出 2000×1500"), "{out}");
    assert!(out.contains("KB）"), "{out}");

    // full_resolution 不缩放
    let (out, is_error, images) = run(
        &dir,
        &state,
        serde_json::json!({"path": "big.png", "full_resolution": true}),
    )
    .await;
    assert!(!is_error, "{out}");
    assert_eq!((images[0].width, images[0].height), (4000, 3000), "{out}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn region_crop_clamp_and_disjoint() {
    let dir = temp_dir("region");
    std::fs::write(dir.join("r.png"), png_bytes(1000, 800, false)).unwrap();
    let state = SessionToolState::for_test();

    // 正常裁剪（原图坐标）
    let (out, is_error, images) = run(
        &dir,
        &state,
        serde_json::json!({"path": "r.png", "region": {"x": 100, "y": 50, "width": 500, "height": 400}}),
    )
    .await;
    assert!(!is_error, "{out}");
    assert_eq!((images[0].width, images[0].height), (500, 400), "{out}");
    assert!(out.contains("裁剪"), "{out}");

    // 越界夹紧：(900,700)+500×400 → 只到 (1000,800)
    let (out, is_error, images) = run(
        &dir,
        &state,
        serde_json::json!({"path": "r.png", "region": {"x": 900, "y": 700, "width": 500, "height": 400}}),
    )
    .await;
    assert!(!is_error, "{out}");
    assert_eq!((images[0].width, images[0].height), (100, 100), "{out}");

    // 完全不相交 → 报错
    let (out, is_error, _) = run(
        &dir,
        &state,
        serde_json::json!({"path": "r.png", "region": {"x": 2000, "y": 0, "width": 100, "height": 100}}),
    )
    .await;
    assert!(is_error, "{out}");
    assert!(out.contains("不相交"), "{out}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn jpeg_source_outputs_jpeg_and_pixel_cap() {
    let dir = temp_dir("jpeg");
    std::fs::write(dir.join("photo.jpg"), jpeg_bytes(640, 480)).unwrap();
    let state = SessionToolState::for_test();

    // 无 alpha 的 JPEG 源 → JPEG q85 输出
    let (out, is_error, images) = run(&dir, &state, serde_json::json!({"path": "photo.jpg"})).await;
    assert!(!is_error, "{out}");
    assert_eq!(images[0].media_type, "image/jpeg", "{out}");

    // 声明 12000×9000 的伪 PNG：解码前被像素上限拦下
    std::fs::write(dir.join("huge.png"), crafted_png_with_dims(12000, 9000)).unwrap();
    let (out, is_error, _) = run(&dir, &state, serde_json::json!({"path": "huge.png"})).await;
    assert!(is_error, "{out}");
    assert!(out.contains("图片过大"), "{out}");
    assert!(out.contains("12000×9000"), "{out}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_image_sensitive_and_outside_guard() {
    let dir = temp_dir("guards");
    std::fs::write(dir.join("note.txt"), "plain text\n").unwrap();
    // 名为 .env 的 PNG：敏感检查先于图片识别
    std::fs::write(dir.join(".env"), png_bytes(4, 4, false)).unwrap();
    let state = SessionToolState::for_test();

    let (out, is_error, _) = run(&dir, &state, serde_json::json!({"path": "note.txt"})).await;
    assert!(is_error, "{out}");
    assert!(out.contains("不是可识别的图片"), "{out}");

    let (out, is_error, _) = run(&dir, &state, serde_json::json!({"path": ".env"})).await;
    assert!(is_error, "{out}");
    assert!(out.contains("敏感文件"), "{out}");

    // 区外：开关关拒绝、开后放行
    let outside = temp_dir("guards-outside");
    std::fs::write(outside.join("o.png"), png_bytes(8, 8, false)).unwrap();
    let rel = format!(
        "../{}/o.png",
        outside.file_name().unwrap().to_string_lossy()
    );
    let (out, is_error, _) = run(&dir, &state, serde_json::json!({"path": rel})).await;
    assert!(is_error, "{out}");
    assert!(out.contains("越出工作目录"), "{out}");
    state
        .fs_read_outside
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let (out, is_error, images) = run(&dir, &state, serde_json::json!({"path": rel})).await;
    assert!(!is_error, "{out}");
    assert_eq!((images[0].width, images[0].height), (8, 8));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_tool_redirects_images() {
    let dir = temp_dir("read-redirect");
    std::fs::write(dir.join("pic.png"), png_bytes(8, 8, false)).unwrap();
    let mut tracker = ChangeTracker::default();
    let state = SessionToolState::for_test();
    let (out, is_error, _, _, _) = tool::execute(
        &call("Read", serde_json::json!({"path": "pic.png"})),
        ToolContext {
            cwd: &dir,
            tracker: &mut tracker,
            state: &state,
        },
    )
    .await;
    assert!(is_error, "{out}");
    assert!(out.contains("PNG 图片"), "{out}");
    assert!(out.contains("ReadMediaFile"), "{out}");
}

// ---------- 纯函数 ----------

/// 测试用 base64 解码（对照手写 encoder）
fn decode64(s: &str) -> Vec<u8> {
    let table: Vec<i32> = (0..256)
        .map(|c| {
            "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/"
                .find(char::from(c as u8))
                .map(|i| i as i32)
                .unwrap_or(-1)
        })
        .collect();
    let mut out = Vec::new();
    let mut acc = 0u32;
    let mut bits = 0;
    for c in s.chars().filter(|c| *c != '=') {
        acc = (acc << 6) | table[c as usize] as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    out
}

#[test]
fn base64_known_vectors() {
    assert_eq!(tool::base64_encode(b""), "");
    assert_eq!(tool::base64_encode(b"M"), "TQ==");
    assert_eq!(tool::base64_encode(b"Ma"), "TWE=");
    assert_eq!(tool::base64_encode(b"Man"), "TWFu");
    assert_eq!(tool::base64_encode(b"hello world"), "aGVsbG8gd29ybGQ=");
    let roundtrip: Vec<u8> = (0..=255).collect();
    assert_eq!(decode64(&tool::base64_encode(&roundtrip)), roundtrip);
}

#[test]
fn sniff_image_magic_numbers() {
    assert_eq!(
        tool::sniff_image(&png_bytes(1, 1, false)),
        Some("image/png")
    );
    assert_eq!(tool::sniff_image(&jpeg_bytes(1, 1)), Some("image/jpeg"));
    assert_eq!(tool::sniff_image(b"GIF89a...."), Some("image/gif"));
    assert_eq!(
        tool::sniff_image(b"RIFF\x00\x00\x00\x00WEBPvp8 "),
        Some("image/webp")
    );
    assert_eq!(tool::sniff_image(b"plain text"), None);
    assert_eq!(tool::sniff_image(b"\x89PNG"), None, "截断头不算");
}

// ---------- 端到端（mock 驱动） ----------

mod common;

/// media 场景自搭环境：input_image 可配（legacy [provider] 格式无此字段=默认 false，
/// 新格式 [[providers.models]] 里显式给 true）
fn setup_media(name: &str, input_image: bool) -> (PathBuf, PathBuf, PathBuf) {
    let port = pig_core::mock::start_mock_server();
    let dir = std::env::temp_dir().join(format!("pig-core-media-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("pic.png"), png_bytes(64, 48, false)).unwrap();
    let config_path = dir.join("config.toml");
    let config = if input_image {
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
"#
        )
    } else {
        format!(
            "[provider]\nbase_url = \"http://127.0.0.1:{port}/v1\"\napi_key = \"mock-key\"\nmodel = \"mock-model\"\n"
        )
    };
    std::fs::write(&config_path, config).unwrap();
    let data_dir = dir.join("data");
    (config_path, dir, data_dir)
}

async fn run_media_scenario(
    input_image: bool,
    name: &str,
) -> (Vec<Event>, PathBuf, PathBuf, pig_core::AgentHandle) {
    let (config_path, dir, data_dir) = setup_media(name, input_image);
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), dir.clone(), data_dir.clone());
    let session_id = common::new_session(&agent, dir.clone()).await;
    agent
        .ops
        .send(Op::SendMessage {
            session_id,
            content: format!("{} 读图片", pig_core::mock::SCENARIO_MEDIA_TRIGGER),
            files: vec![],
            mode: ExecMode::AutoEdit,
        })
        .await
        .unwrap();
    let events = common::recv_until(&agent.events, std::time::Duration::from_secs(20), |e| {
        matches!(e, Event::TurnComplete { .. })
    })
    .await;
    (events, dir, data_dir, agent)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn media_gate_blocks_when_model_lacks_input_image() {
    let (events, _dir, _data, agent) = run_media_scenario(false, "gate").await;
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, Event::ApprovalRequested { .. })),
        "门控不弹审批"
    );
    let hit = events.iter().any(|e| {
        matches!(
            e,
            Event::ToolCallEnd { output, is_error, .. }
                if *is_error && output.contains("不支持图片输入")
        )
    });
    assert!(hit, "应收到能力引导错误: {events:?}");
    agent.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn media_flows_and_rollout_stays_text_only() {
    let (events, _dir, data_dir, agent) = run_media_scenario(true, "flow").await;
    let (output, is_error) = events
        .iter()
        .find_map(|e| match e {
            Event::ToolCallEnd {
                output, is_error, ..
            } => Some((output.clone(), *is_error)),
            _ => None,
        })
        .expect("有 ToolCallEnd");
    assert!(!is_error, "{output}");
    assert!(output.contains("已读取图片 pic.png"), "{output}");
    assert!(output.contains("64×48"), "{output}");

    // rollout 只落文本摘要，不落 base64
    let sessions_dir = data_dir.join("sessions");
    let rollout = std::fs::read_dir(&sessions_dir)
        .expect("sessions 目录")
        .flatten()
        .find(|e| e.path().extension().is_some_and(|x| x == "jsonl"))
        .expect("rollout 文件");
    let raw = std::fs::read_to_string(rollout.path()).unwrap();
    assert!(raw.contains("已读取图片"), "文本摘要落盘");
    assert!(
        !raw.contains("base64") && !raw.contains("iVBOR"),
        "base64 不进 rollout"
    );
    agent.shutdown();
}
