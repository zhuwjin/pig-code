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

fn tiff_bytes(w: u32, h: u32) -> Vec<u8> {
    let mut buf = std::io::Cursor::new(Vec::new());
    image::DynamicImage::new_rgb8(w, h)
        .write_to(&mut buf, image::ImageFormat::Tiff)
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
    let chunk = |name: &[u8; 4], data: &[u8]| {
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
            images: vec![],
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

// ---------- 粘贴发送管线（压缩共享函数 / rollout ImageRef / 回放重建 / 能力投影） ----------

#[test]
fn tiff_clipboard_bytes_convert_to_png() {
    let tiff = tiff_bytes(37, 23);
    let (png, width, height) = tool::convert_tiff_to_png(&tiff).unwrap();
    assert_eq!((width, height), (37, 23));
    assert_eq!(tool::sniff_image(&png), Some("image/png"));
    assert_ne!(png, tiff);
    assert!(tool::convert_tiff_to_png(b"not a tiff").is_err());
}

#[test]
fn compress_image_for_model_pipeline() {
    // 大图等比缩到 2000
    let big = png_bytes(4000, 3000, false);
    let comp = tool::compress_image_for_model(&big, "image/png").unwrap();
    assert_eq!((comp.width, comp.height), (2000, 1500));
    assert_eq!(comp.media_type, "image/png");
    assert_eq!(tool::sniff_image(&comp.bytes), Some("image/png"));

    // JPEG 源出 JPEG；mime 空串时走魔数嗅探
    let comp = tool::compress_image_for_model(&jpeg_bytes(100, 80), "").unwrap();
    assert_eq!(comp.media_type, "image/jpeg");
    assert_eq!((comp.width, comp.height), (100, 80), "小图不放大");

    // RGBA 保 PNG（有 alpha 不转 JPEG）
    let comp = tool::compress_image_for_model(&png_bytes(64, 64, true), "image/png").unwrap();
    assert_eq!(comp.media_type, "image/png");

    // 非图片/超像素上限
    assert!(tool::compress_image_for_model(b"not an image", "").is_err());
    assert!(tool::compress_image_for_model(&crafted_png_with_dims(12000, 9000), "").is_err());
}

#[test]
fn rollout_user_images_serde_roundtrip_and_compat() {
    use pig_core::rollout::RolloutRecord;
    // 新记录带 ImageRef 往返
    let rec = RolloutRecord::User {
        text: "看图".into(),
        files: vec![],
        images: vec![pig_core::rollout::ImageRef {
            path: PathBuf::from("/tmp/x.media/1.png"),
            media_type: "image/png".into(),
            width: 8,
            height: 8,
        }],
    };
    let json = serde_json::to_string(&rec).unwrap();
    assert!(json.contains("1.png"), "{json}");
    let back: RolloutRecord = serde_json::from_str(&json).unwrap();
    match back {
        RolloutRecord::User { images, .. } => {
            assert_eq!(images.len(), 1);
            assert_eq!(images[0].width, 8);
        }
        _ => panic!("类型往返"),
    }
    // 旧记录无 images 字段 → serde default 兼容
    let old: RolloutRecord =
        serde_json::from_str(r#"{"type":"user","text":"hi","files":[]}"#).unwrap();
    match old {
        RolloutRecord::User { images, .. } => assert!(images.is_empty(), "旧记录空图片"),
        _ => panic!(),
    }
}

#[test]
fn rebuild_history_rehydrates_images_and_degrades_missing() {
    let dir = temp_dir("rehydrate");
    let media = dir.join("s1.media");
    std::fs::create_dir_all(&media).unwrap();
    std::fs::write(media.join("1.png"), png_bytes(8, 8, false)).unwrap();

    let records = vec![pig_core::rollout::RolloutRecord::User {
        text: "看图说话".into(),
        files: vec![],
        images: vec![
            pig_core::rollout::ImageRef {
                path: media.join("1.png"),
                media_type: "image/png".into(),
                width: 8,
                height: 8,
            },
            pig_core::rollout::ImageRef {
                path: media.join("gone.png"),
                media_type: "image/png".into(),
                width: 8,
                height: 8,
            },
        ],
    }];
    let history = pig_core::rollout::rebuild_history(&records, "sys".into());
    let user = history
        .iter()
        .find(|m| m.role == "user")
        .expect("user 消息");
    assert_eq!(user.images.len(), 1, "存在的图重建");
    assert_eq!(user.images[0].media_type, "image/png");
    assert_eq!(
        tool::sniff_image(&decode64(&user.images[0].data_base64)),
        Some("image/png"),
        "base64 往返字节级"
    );
    assert!(
        user.content.as_deref().unwrap_or("").contains("图片已失效"),
        "丢失的图占位: {:?}",
        user.content
    );
}

#[test]
fn project_images_capability_projection() {
    use pig_core::provider::ChatImage;
    let img = || ChatImage {
        media_type: "image/png".into(),
        data_base64: "QUJD".into(),
        label: None,
    };
    // 支持图片：原样进 images
    let mut text = "看这个".to_string();
    let mut images = vec![img()];
    pig_core::session::project_images(&mut text, &mut images, &[], true);
    assert_eq!(images.len(), 1);
    assert!(!text.contains("未随消息发送"), "{text}");

    // 不支持：清空 images + 文本占位（带媒体路径，模型可用 ReadMediaFile 读）
    let mut text = "看这个".to_string();
    let mut images = vec![img(), img()];
    let paths = vec![
        PathBuf::from("/tmp/s1.media/1.png"),
        PathBuf::from("/tmp/s1.media/2.png"),
    ];
    pig_core::session::project_images(&mut text, &mut images, &paths, false);
    assert!(images.is_empty());
    assert!(text.contains("图片 2 张未随消息发送"), "{text}");
    assert!(text.contains("ReadMediaFile"), "{text}");
    assert!(
        text.contains("/tmp/s1.media/1.png") && text.contains("/tmp/s1.media/2.png"),
        "占位应带媒体路径: {text}"
    );
}

// ---------- 粘贴发送端到端（mock 驱动，请求体日志断言图片载荷） ----------

/// 粘贴 e2e 环境：input_image 可配；返回（请求体日志， 工作目录， 数据目录， agent）。
/// 用普通 Read 流程即可（mock 首请求发 Read 工具调用，第二请求带图的用户消息在 history 里）。
fn setup_paste(
    name: &str,
    input_image: bool,
) -> (
    std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    PathBuf,
    PathBuf,
    pig_core::AgentHandle,
) {
    let (port, log) = pig_core::mock::start_mock_server_with_log();
    let dir = std::env::temp_dir().join(format!("pig-core-paste-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join(pig_core::mock::MOCK_FILE_NAME),
        pig_core::mock::MOCK_FILE_CONTENT,
    )
    .unwrap();
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
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), dir.clone(), data_dir.clone());
    (log, dir, data_dir, agent)
}

async fn send_paste_and_wait(
    agent: &pig_core::AgentHandle,
    session_id: &str,
    image: Vec<u8>,
) -> Vec<Event> {
    agent
        .ops
        .send(Op::SendMessage {
            session_id: session_id.to_string(),
            content: "这张图是什么".to_string(),
            files: vec![],
            images: vec![pig_protocol::PendingImage {
                bytes: image,
                mime: "image/png".into(),
            }],
            mode: ExecMode::AutoEdit,
        })
        .await
        .unwrap();
    common::recv_until(&agent.events, std::time::Duration::from_secs(20), |e| {
        matches!(e, Event::TurnComplete { .. })
    })
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn paste_flow_persists_media_and_ships_image_payload() {
    let (log, dir, data_dir, agent) = setup_paste("e2e", true);
    let session_id = common::new_session(&agent, dir.clone()).await;
    let events = send_paste_and_wait(&agent, &session_id, png_bytes(64, 48, false)).await;

    // 用户气泡事件带图片张数 + markdown 附件链接（m1 与 media 文件名序号一致）
    let user_text = events
        .iter()
        .find_map(|e| match e {
            Event::UserMessage {
                text, image_count, ..
            } if *image_count == 1 => Some(text.clone()),
            _ => None,
        })
        .expect("UserMessage.image_count=1");
    assert!(
        user_text.contains("[图片 1](pig-code-composer://attachments/m1)"),
        "事件文本带附件链接: {user_text}"
    );
    // 媒体目录布局：{data}/sessions/{sid}.media/1.png
    let stored = data_dir
        .join("sessions")
        .join(format!("{session_id}.media"))
        .join("1.png");
    assert!(stored.exists(), "压缩字节落盘: {}", stored.display());
    let stored_bytes = std::fs::read(&stored).unwrap();
    assert_eq!(tool::sniff_image(&stored_bytes), Some("image/png"));
    assert_eq!(
        tool::image_dimensions(&stored_bytes),
        Some((64, 48)),
        "小图不缩放"
    );
    // rollout：ImageRef 引用路径，不含 base64
    let rollout = std::fs::read_to_string(
        data_dir
            .join("sessions")
            .join(format!("{session_id}.jsonl")),
    )
    .unwrap();
    assert!(rollout.contains("1.png"), "ImageRef 落盘: {rollout}");
    assert!(!rollout.contains("data_base64"), "rollout 不存 base64");
    // 请求体（OpenAI 拆分）：user 消息里有 image_url data URL
    let bodies = log.lock().unwrap().join("\n");
    assert!(
        bodies.contains("image_url"),
        "图片载荷发出: 请求体应有 image_url"
    );
    assert!(bodies.contains("data:image/png;base64,"), "{bodies}");
    // 链接只对 UI 展示：进模型 history 的文本保持干净
    assert!(
        !bodies.contains("pig-code-composer"),
        "history 不带附件链接: {bodies}"
    );
    // 小图直通（未缩放/转码）：不加压缩附注、不落原图
    assert!(
        !bodies.contains("已压缩以适应模型限制"),
        "未变化的图不加附注: {bodies}"
    );
    assert!(
        !data_dir
            .join("sessions")
            .join(format!("{session_id}.media"))
            .join("1.orig.png")
            .exists(),
        "未变化的图不落原图"
    );
    agent.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn paste_projection_when_model_lacks_input_image() {
    let (log, dir, _data, agent) = setup_paste("proj", false);
    let session_id = common::new_session(&agent, dir.clone()).await;
    let _events = send_paste_and_wait(&agent, &session_id, png_bytes(32, 32, false)).await;

    // 能力投影：请求体文本带占位、不带 image_url；媒体文件照样落盘（换模型后回放可见）
    let bodies = log.lock().unwrap().join("\n");
    assert!(
        bodies.contains("未随消息发送"),
        "投影占位进请求体: {bodies}"
    );
    assert!(!bodies.contains("image_url"), "不支持时不发图片载荷");
    let media = dir
        .join("data")
        .join("sessions")
        .join(format!("{session_id}.media"))
        .join("1.png");
    assert!(media.exists(), "媒体文件仍落盘: {}", media.display());
    agent.shutdown();
}

/// 压缩附注（kimi-code caption 思路）：缩放/转码改变了图 → 请求体文本带附注、
/// 原图落 `{n}.orig.{ext}` 供 ReadMediaFile region 看高清局部；
/// 同时回归媒体文件续排序号（第二轮粘贴不覆盖第一轮的 ImageRef 目标）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn paste_caption_orig_and_sequential_media_names() {
    let (log, dir, data_dir, agent) = setup_paste("caption", true);
    let session_id = common::new_session(&agent, dir.clone()).await;
    // 大图触发缩放（2100×100，最长边超 2000）
    let big = png_bytes(2100, 100, false);
    send_paste_and_wait(&agent, &session_id, big.clone()).await;
    let media = data_dir
        .join("sessions")
        .join(format!("{session_id}.media"));
    let compressed = std::fs::read(media.join("1.png")).unwrap();
    let dims = tool::image_dimensions(&compressed).unwrap();
    assert!(
        dims.0 <= 2000 && dims != (2100, 100),
        "缩放到预算内（resize 保比例，取实际值）: {dims:?}"
    );
    assert_eq!(
        std::fs::read(media.join("1.orig.png")).unwrap(),
        big,
        "原图字节级落盘"
    );
    let bodies = log.lock().unwrap().join("\n");
    assert!(
        bodies.contains("已压缩以适应模型限制"),
        "压缩附注进请求体: {bodies}"
    );
    assert!(bodies.contains("1.orig.png"), "附注带原图路径: {bodies}");

    // 第二轮粘贴：文件名续排（2.png），不覆盖第一轮的 1.png（旧 ImageRef 仍有效）
    let events2 = send_paste_and_wait(&agent, &session_id, png_bytes(64, 48, false)).await;
    assert!(
        events2.iter().any(|e| matches!(
            e,
            Event::UserMessage { text, .. }
                if text.contains("[图片 2](pig-code-composer://attachments/m2)")
        )),
        "第二轮链接序号续排 m2"
    );
    assert!(media.join("1.png").exists() && media.join("2.png").exists());
    let rollout = std::fs::read_to_string(
        data_dir
            .join("sessions")
            .join(format!("{session_id}.jsonl")),
    )
    .unwrap();
    assert!(
        rollout.contains("1.png") && rollout.contains("2.png"),
        "两条 User 记录各有 ImageRef: {rollout}"
    );
    agent.shutdown();
}

/// 重开 rehydrate e2e：paste → shutdown → 同 data_dir 起新 agent → OpenSession 回放
/// → 再发消息。delete_media=true 时删掉媒体目录模拟丢失（降级占位、不再发图片载荷）。
async fn paste_resume(name: &str, delete_media: bool) {
    let (log, dir, data_dir, agent) = setup_paste(name, true);
    let session_id = common::new_session(&agent, dir.clone()).await;
    send_paste_and_wait(&agent, &session_id, png_bytes(64, 48, false)).await;
    let image_url_count = || log.lock().unwrap().join("\n").matches("image_url").count();
    let before = image_url_count();
    assert!(before > 0, "paste 回合应发过图片载荷");
    agent.shutdown();

    let media = data_dir
        .join("sessions")
        .join(format!("{session_id}.media"));
    if delete_media {
        std::fs::remove_dir_all(&media).unwrap();
    }
    // 模拟重启：同一 data_dir 起新 manager（mock server 同一端口还活着）
    let agent2 = pig_core::spawn_agent_with_data_dir(
        Some(dir.join("config.toml")),
        dir.clone(),
        data_dir.clone(),
    );
    agent2
        .ops
        .send(Op::OpenSession {
            session_id: session_id.clone(),
        })
        .await
        .unwrap();
    let replay = common::recv_until(&agent2.events, std::time::Duration::from_secs(10), |e| {
        matches!(e, Event::TurnComplete { .. })
    })
    .await;
    assert!(
        replay.iter().any(|e| matches!(
            e,
            Event::UserMessage { image_count, text, .. }
                if *image_count == 1
                    && text.contains("[图片 1](pig-code-composer://attachments/m1)")
        )),
        "回放气泡带图片张数 + 附件链接（与 live 同形态）"
    );
    // 排空回放残余事件，避免干扰后面的 recv_until
    while tokio::time::timeout(std::time::Duration::from_millis(200), agent2.events.recv())
        .await
        .is_ok()
    {}
    // 继续对话：history 由 rollout + 媒体目录重建
    agent2
        .ops
        .send(Op::SendMessage {
            session_id: session_id.clone(),
            content: "接着说".to_string(),
            files: vec![],
            images: vec![],
            mode: ExecMode::AutoEdit,
        })
        .await
        .unwrap();
    common::recv_until(&agent2.events, std::time::Duration::from_secs(20), |e| {
        matches!(e, Event::TurnComplete { .. })
    })
    .await;
    if delete_media {
        let bodies = log.lock().unwrap().join("\n");
        assert!(bodies.contains("图片已失效"), "丢失降级占位: {bodies}");
        assert_eq!(image_url_count(), before, "媒体丢失后不再发图片载荷");
    } else {
        assert!(
            image_url_count() > before,
            "重开后历史重建带图（rehydrate）"
        );
    }
    agent2.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn paste_resume_rehydrates_images() {
    paste_resume("resume", false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn paste_resume_degrades_when_media_deleted() {
    paste_resume("resume-gone", true).await;
}
