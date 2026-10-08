//! ReadMediaFile full chain: resize/crop/limits/sensitive and outside-workspace
//! guards, base64 and magic-number sniffing, capability gating
//! (input_image=false), rollout stores no base64.

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

/// Programmatically generate a PNG (rgb/rgba optional)
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

/// Hand-assemble a minimal valid PNG (IHDR declaring huge dimensions + empty
/// IEND, no image data): ReadMediaFile's pixel-cap check runs before decoding;
/// it should reject as soon as the dimensions are read.
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
    // The png crate only reports dimensions once it sees IDAT; an empty zlib stream as placeholder (not decoded, content irrelevant)
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
            fs_grant: None,
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
        "aspect-ratio-preserving resize"
    );
    assert_eq!(images[0].media_type, "image/png");
    assert!(tool::sniff_image(&decode64(&images[0].data_base64)).is_some());
    assert!(out.contains("original 4000×3000"), "{out}");
    assert!(out.contains("output 2000×1500"), "{out}");
    assert!(out.contains("KB)"), "{out}");

    // full_resolution skips the resize
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

    // Normal crop (original-image coordinates)
    let (out, is_error, images) = run(
        &dir,
        &state,
        serde_json::json!({"path": "r.png", "region": {"x": 100, "y": 50, "width": 500, "height": 400}}),
    )
    .await;
    assert!(!is_error, "{out}");
    assert_eq!((images[0].width, images[0].height), (500, 400), "{out}");
    assert!(out.contains("cropped"), "{out}");

    // Out-of-bounds clamping: (900,700)+500×400 → stops at (1000,800)
    let (out, is_error, images) = run(
        &dir,
        &state,
        serde_json::json!({"path": "r.png", "region": {"x": 900, "y": 700, "width": 500, "height": 400}}),
    )
    .await;
    assert!(!is_error, "{out}");
    assert_eq!((images[0].width, images[0].height), (100, 100), "{out}");

    // Fully disjoint → error
    let (out, is_error, _) = run(
        &dir,
        &state,
        serde_json::json!({"path": "r.png", "region": {"x": 2000, "y": 0, "width": 100, "height": 100}}),
    )
    .await;
    assert!(is_error, "{out}");
    assert!(out.contains("does not intersect"), "{out}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn jpeg_source_outputs_jpeg_and_pixel_cap() {
    let dir = temp_dir("jpeg");
    std::fs::write(dir.join("photo.jpg"), jpeg_bytes(640, 480)).unwrap();
    let state = SessionToolState::for_test();

    // A JPEG source without alpha → JPEG q85 output
    let (out, is_error, images) = run(&dir, &state, serde_json::json!({"path": "photo.jpg"})).await;
    assert!(!is_error, "{out}");
    assert_eq!(images[0].media_type, "image/jpeg", "{out}");

    // A fake PNG declaring 12000×9000: caught by the pixel cap before decoding
    std::fs::write(dir.join("huge.png"), crafted_png_with_dims(12000, 9000)).unwrap();
    let (out, is_error, _) = run(&dir, &state, serde_json::json!({"path": "huge.png"})).await;
    assert!(is_error, "{out}");
    assert!(out.contains("Image too large"), "{out}");
    assert!(out.contains("12000×9000"), "{out}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_image_sensitive_and_outside_guard() {
    let dir = temp_dir("guards");
    std::fs::write(dir.join("note.txt"), "plain text\n").unwrap();
    // A PNG named .env: the sensitive check runs before image detection
    std::fs::write(dir.join(".env"), png_bytes(4, 4, false)).unwrap();
    let state = SessionToolState::for_test();

    let (out, is_error, _) = run(&dir, &state, serde_json::json!({"path": "note.txt"})).await;
    assert!(is_error, "{out}");
    assert!(out.contains("Not a recognizable image"), "{out}");

    let (out, is_error, _) = run(&dir, &state, serde_json::json!({"path": ".env"})).await;
    assert!(is_error, "{out}");
    assert!(out.contains("sensitive file"), "{out}");

    // Outside: denied with the switch off, allowed after turning it on
    let outside = temp_dir("guards-outside");
    std::fs::write(outside.join("o.png"), png_bytes(8, 8, false)).unwrap();
    let rel = format!(
        "../{}/o.png",
        outside.file_name().unwrap().to_string_lossy()
    );
    let (out, is_error, _) = run(&dir, &state, serde_json::json!({"path": rel})).await;
    assert!(is_error, "{out}");
    assert!(out.contains("Path escapes the working directory"), "{out}");
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
            fs_grant: None,
        },
    )
    .await;
    assert!(is_error, "{out}");
    assert!(out.contains("PNG image"), "{out}");
    assert!(out.contains("ReadMediaFile"), "{out}");
}

// ---------- Pure functions ----------

/// Test-only base64 decode (counterpart to the hand-written encoder)
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
    assert_eq!(
        tool::sniff_image(b"\x89PNG"),
        None,
        "truncated header does not count"
    );
}

// ---------- End-to-end (mock driven) ----------

mod common;

/// Self-assembled environment for the media scenario: input_image configurable
/// (default false = the field is not declared; only an explicit true in
/// [[providers.models]] enables it)
fn setup_media(name: &str, input_image: bool) -> (PathBuf, PathBuf, PathBuf) {
    let port = pig_core::mock::start_mock_server();
    let dir = std::env::temp_dir().join(format!("pig-core-media-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("pic.png"), png_bytes(64, 48, false)).unwrap();
    let config_path = dir.join("config.toml");
    let input_image_line = if input_image {
        "input_image = true\n"
    } else {
        ""
    };
    let config = format!(
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
{input_image_line}"#
    );
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
            content: format!("{} read the image", pig_core::mock::SCENARIO_MEDIA_TRIGGER),
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
        "gate should not raise an approval"
    );
    let hit = events.iter().any(|e| {
        matches!(
            e,
            Event::ToolCallEnd { output, is_error, .. }
                if *is_error && output.contains("does not support image input")
        )
    });
    assert!(hit, "expected the capability guidance error: {events:?}");
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
        .expect("expected a ToolCallEnd");
    assert!(!is_error, "{output}");
    assert!(output.contains("Read image pic.png"), "{output}");
    assert!(output.contains("64×48"), "{output}");

    // The rollout persists only the text summary, no base64
    let sessions_dir = data_dir.join("sessions");
    let rollout = std::fs::read_dir(&sessions_dir)
        .expect("sessions directory")
        .flatten()
        .find(|e| e.path().extension().is_some_and(|x| x == "jsonl"))
        .expect("rollout file");
    let raw = std::fs::read_to_string(rollout.path()).unwrap();
    assert!(raw.contains("Read image"), "text summary persisted");
    assert!(
        !raw.contains("base64") && !raw.contains("iVBOR"),
        "base64 must not enter the rollout"
    );
    agent.shutdown();
}

// ---------- Paste-send pipeline (shared compression fn / rollout ImageRef / replay rebuild / capability projection) ----------

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
    // A large image is proportionally resized to 2000
    let big = png_bytes(4000, 3000, false);
    let comp = tool::compress_image_for_model(&big, "image/png").unwrap();
    assert_eq!((comp.width, comp.height), (2000, 1500));
    assert_eq!(comp.media_type, "image/png");
    assert_eq!(tool::sniff_image(&comp.bytes), Some("image/png"));

    // A JPEG source yields JPEG; an empty mime falls back to magic-number sniffing
    let comp = tool::compress_image_for_model(&jpeg_bytes(100, 80), "").unwrap();
    assert_eq!(comp.media_type, "image/jpeg");
    assert_eq!(
        (comp.width, comp.height),
        (100, 80),
        "small image is not upscaled"
    );

    // RGBA stays PNG (alpha present, no JPEG conversion)
    let comp = tool::compress_image_for_model(&png_bytes(64, 64, true), "image/png").unwrap();
    assert_eq!(comp.media_type, "image/png");

    // Non-image / over the pixel cap
    assert!(tool::compress_image_for_model(b"not an image", "").is_err());
    assert!(tool::compress_image_for_model(&crafted_png_with_dims(12000, 9000), "").is_err());
}

#[test]
fn rollout_user_images_serde_roundtrip_and_compat() {
    use pig_core::rollout::RolloutRecord;
    // New record with ImageRef round-trips
    let rec = RolloutRecord::User {
        text: "Look at the image".into(),
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
        _ => panic!("type mismatch after roundtrip"),
    }
}

#[test]
fn rebuild_history_rehydrates_images_and_degrades_missing() {
    let dir = temp_dir("rehydrate");
    let media = dir.join("s1.media");
    std::fs::create_dir_all(&media).unwrap();
    std::fs::write(media.join("1.png"), png_bytes(8, 8, false)).unwrap();

    let records = vec![pig_core::rollout::RolloutRecord::User {
        text: "Describe the image".into(),
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
        .expect("user message");
    assert_eq!(user.images.len(), 1, "existing image rehydrated");
    assert_eq!(user.images[0].media_type, "image/png");
    assert_eq!(
        tool::sniff_image(&decode64(&user.images[0].data_base64)),
        Some("image/png"),
        "byte-exact base64 roundtrip"
    );
    assert!(
        user.content
            .as_deref()
            .unwrap_or("")
            .contains("no longer available"),
        "placeholder for the missing image: {:?}",
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
    // Image supported: goes into images unchanged
    let mut text = "Look at this".to_string();
    let mut images = vec![img()];
    pig_core::session::project_images(&mut text, &mut images, &[], true);
    assert_eq!(images.len(), 1);
    assert!(!text.contains("were not sent"), "{text}");

    // Not supported: images cleared + text placeholder (carrying the media paths so the model can read them via ReadMediaFile)
    let mut text = "Look at this".to_string();
    let mut images = vec![img(), img()];
    let paths = vec![
        PathBuf::from("/tmp/s1.media/1.png"),
        PathBuf::from("/tmp/s1.media/2.png"),
    ];
    pig_core::session::project_images(&mut text, &mut images, &paths, false);
    assert!(images.is_empty());
    assert!(text.contains("2 image(s) were not sent"), "{text}");
    assert!(text.contains("ReadMediaFile"), "{text}");
    assert!(
        text.contains("/tmp/s1.media/1.png") && text.contains("/tmp/s1.media/2.png"),
        "placeholder should carry the media paths: {text}"
    );
}

// ---------- Paste-send end-to-end (mock driven; asserts the image payload via the request-body log) ----------

/// Paste e2e environment: input_image configurable; returns (request-body log,
/// working dir, data dir, agent).
/// The plain Read flow suffices (the mock's first request issues a Read tool
/// call; the second request has the image-bearing user message in history).
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
    let input_image_line = if input_image {
        "input_image = true\n"
    } else {
        ""
    };
    let config = format!(
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
{input_image_line}"#
    );
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
            content: "What is this image".to_string(),
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

    // The user bubble event carries the attachment numbers (clean body without an inlined link; m1 matches the media file-name index)
    let (user_text, nums) = events
        .iter()
        .find_map(|e| match e {
            Event::UserMessage {
                text, image_nums, ..
            } if image_nums.len() == 1 => Some((text.clone(), image_nums.clone())),
            _ => None,
        })
        .expect("UserMessage with one attachment number");
    assert_eq!(
        user_text, "What is this image",
        "event text is the clean body (attachment link no longer inlined): {user_text}"
    );
    assert_eq!(
        nums,
        vec![1],
        "attachment number m1 matches the media file name"
    );
    // Media directory layout: {data}/sessions/{sid}.media/1.png
    let stored = data_dir
        .join("sessions")
        .join(format!("{session_id}.media"))
        .join("1.png");
    assert!(
        stored.exists(),
        "compressed bytes persisted: {}",
        stored.display()
    );
    let stored_bytes = std::fs::read(&stored).unwrap();
    assert_eq!(tool::sniff_image(&stored_bytes), Some("image/png"));
    assert_eq!(
        tool::image_dimensions(&stored_bytes),
        Some((64, 48)),
        "small image is not resized"
    );
    // Rollout: ImageRef references the path, contains no base64
    let rollout = std::fs::read_to_string(
        data_dir
            .join("sessions")
            .join(format!("{session_id}.jsonl")),
    )
    .unwrap();
    assert!(rollout.contains("1.png"), "ImageRef persisted: {rollout}");
    assert!(!rollout.contains("data_base64"), "rollout stores no base64");
    // Request body (OpenAI split): the user message contains an image_url data URL
    let bodies = log.lock().unwrap().join("\n");
    assert!(
        bodies.contains("image_url"),
        "image payload sent: request body should contain image_url"
    );
    assert!(bodies.contains("data:image/png;base64,"), "{bodies}");
    // The link is UI-display-only: the text entering model history stays clean
    assert!(
        !bodies.contains("pig-code-composer"),
        "history carries no attachment link: {bodies}"
    );
    // Small image passes through (not resized/transcoded): no compression caption, no original persisted
    assert!(
        !bodies.contains("compressed to fit model limits"),
        "unchanged image gets no caption: {bodies}"
    );
    assert!(
        !data_dir
            .join("sessions")
            .join(format!("{session_id}.media"))
            .join("1.orig.png")
            .exists(),
        "unchanged image keeps no original file"
    );
    agent.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn paste_projection_when_model_lacks_input_image() {
    let (log, dir, _data, agent) = setup_paste("proj", false);
    let session_id = common::new_session(&agent, dir.clone()).await;
    let _events = send_paste_and_wait(&agent, &session_id, png_bytes(32, 32, false)).await;

    // Capability projection: the request body carries the placeholder text, no image_url; the media file is still persisted (visible on replay after switching models)
    let bodies = log.lock().unwrap().join("\n");
    assert!(
        bodies.contains("were not sent"),
        "projection placeholder enters the request body: {bodies}"
    );
    assert!(
        !bodies.contains("image_url"),
        "no image payload when unsupported"
    );
    let media = dir
        .join("data")
        .join("sessions")
        .join(format!("{session_id}.media"))
        .join("1.png");
    assert!(
        media.exists(),
        "media file still persisted: {}",
        media.display()
    );
    agent.shutdown();
}

/// Compression caption (kimi-code caption idea): when resizing/transcoding
/// changed the image → the request-body text carries a caption and the original
/// lands at `{n}.orig.{ext}` so ReadMediaFile region can view the high-res
/// part; also a regression check that media file numbering continues (the
/// second paste does not overwrite the first paste's ImageRef target).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn paste_caption_orig_and_sequential_media_names() {
    let (log, dir, data_dir, agent) = setup_paste("caption", true);
    let session_id = common::new_session(&agent, dir.clone()).await;
    // A large image triggers the resize (2100×100, longest side over 2000)
    let big = png_bytes(2100, 100, false);
    send_paste_and_wait(&agent, &session_id, big.clone()).await;
    let media = data_dir
        .join("sessions")
        .join(format!("{session_id}.media"));
    let compressed = std::fs::read(media.join("1.png")).unwrap();
    let dims = tool::image_dimensions(&compressed).unwrap();
    assert!(
        dims.0 <= 2000 && dims != (2100, 100),
        "resized within budget (resize preserves ratio; use actual value): {dims:?}"
    );
    assert_eq!(
        std::fs::read(media.join("1.orig.png")).unwrap(),
        big,
        "original persisted byte-exact"
    );
    let bodies = log.lock().unwrap().join("\n");
    assert!(
        bodies.contains("compressed to fit model limits"),
        "compression caption enters the request body: {bodies}"
    );
    assert!(
        bodies.contains("1.orig.png"),
        "caption carries the original path: {bodies}"
    );

    // Second paste: file names continue (2.png → attachment number 2), the first paste's 1.png is not overwritten (the old ImageRef stays valid)
    let events2 = send_paste_and_wait(&agent, &session_id, png_bytes(64, 48, false)).await;
    assert!(
        events2.iter().any(|e| matches!(
            e,
            Event::UserMessage { image_nums, .. } if image_nums == &[2]
        )),
        "second paste attachment number continues at m2"
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
        "both User records carry their ImageRef: {rollout}"
    );
    agent.shutdown();
}

/// Reopen-rehydrate e2e: paste → shutdown → new agent on the same data_dir →
/// OpenSession replay → send another message. With delete_media=true the media
/// directory is removed to simulate loss (degrades to a placeholder, no
/// further image payload).
async fn paste_resume(name: &str, delete_media: bool) {
    let (log, dir, data_dir, agent) = setup_paste(name, true);
    let session_id = common::new_session(&agent, dir.clone()).await;
    send_paste_and_wait(&agent, &session_id, png_bytes(64, 48, false)).await;
    let image_url_count = || log.lock().unwrap().join("\n").matches("image_url").count();
    let before = image_url_count();
    assert!(
        before > 0,
        "the paste turn should have sent the image payload"
    );
    agent.shutdown();

    let media = data_dir
        .join("sessions")
        .join(format!("{session_id}.media"));
    if delete_media {
        std::fs::remove_dir_all(&media).unwrap();
    }
    // Simulate a restart: new manager on the same data_dir (the mock server is still alive on the same port)
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
            Event::UserMessage { image_nums, .. } if image_nums == &[1]
        )),
        "replayed bubble carries the attachment number (same shape as live)"
    );
    // Drain leftover replay events to avoid interfering with later recv_until calls
    while tokio::time::timeout(std::time::Duration::from_millis(200), agent2.events.recv())
        .await
        .is_ok()
    {}
    // Continue the conversation: history is rebuilt from the rollout + media directory
    agent2
        .ops
        .send(Op::SendMessage {
            session_id: session_id.clone(),
            content: "Continue".to_string(),
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
        assert!(
            bodies.contains("no longer available"),
            "missing media degrades to a placeholder: {bodies}"
        );
        assert_eq!(
            image_url_count(),
            before,
            "no image payload after media loss"
        );
    } else {
        assert!(
            image_url_count() > before,
            "reopened history rebuild includes the images (rehydrate)"
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
