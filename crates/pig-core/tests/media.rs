//! ReadMediaFile full chain: resize/crop/limits/sensitive and outside-workspace
//! guards, base64 and magic-number sniffing, capability gating
//! (input_image=false), rollout stores no base64.

use std::path::PathBuf;

use pig_core::task::SessionToolState;
use pig_core::tool::{self, ChangeTracker, ToolContext};
use pig_protocol::{Event, ExecMode, Op};
use pig_provider::ToolCall;
use pig_utils::pdf::MAX_PDF_FILE_BYTES;

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
    let (out, is_error, _, _, media) = tool::execute(
        &call("ReadMediaFile", args),
        ToolContext {
            cwd: dir,
            tracker: &mut tracker,
            state,
            fs_grant: None,
            sandbox_override: Default::default(),
            sandbox_profile: Default::default(),
            additional_permissions: Default::default(),
        },
    )
    .await;
    (out, is_error, media.images)
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
async fn non_image_and_sensitive_guard() {
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
    assert!(
        out.contains("sensitive file") || out.contains("sandbox deny rule"),
        "{out}"
    );

    // Outside: reads are full-disk, so the image loads without any switch
    let outside = temp_dir("guards-outside");
    std::fs::write(outside.join("o.png"), png_bytes(8, 8, false)).unwrap();
    let rel = format!(
        "../{}/o.png",
        outside.file_name().unwrap().to_string_lossy()
    );
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
            sandbox_override: Default::default(),
            sandbox_profile: Default::default(),
            additional_permissions: Default::default(),
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

use crate::common;

/// Self-assembled environment for the media scenario: input_image configurable
/// (default false = the field is not declared; only an explicit true in
/// [[providers.models]] enables it)
fn setup_media(
    name: &str,
    input_image: bool,
) -> (PathBuf, PathBuf, PathBuf, pig_provider::mock::RequestLog) {
    let (port, log) = pig_provider::mock::start_mock_server_with_log();
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
    (config_path, dir, data_dir, log)
}

async fn run_media_scenario(
    input_image: bool,
    name: &str,
) -> (Vec<Event>, PathBuf, PathBuf, pig_core::AgentHandle) {
    let (config_path, dir, data_dir, _log) = setup_media(name, input_image);
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), dir.clone(), data_dir.clone());
    let session_id = common::new_session(&agent, dir.clone()).await;
    agent
        .ops
        .send(Op::SendMessage {
            session_id,
            content: format!(
                "{} read the image",
                pig_provider::mock::SCENARIO_MEDIA_TRIGGER
            ),
            files: vec![],
            images: vec![],
            permissions: ExecMode::Auto.permissions(),
            delivery: pig_protocol::MessageDelivery::Queue,
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
                if *is_error && output.contains("neither image nor PDF input")
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
        sent_at_ms: 0,
        text: "Look at the image".into(),
        files: vec![],
        images: vec![pig_core::rollout::ImageRef {
            path: PathBuf::from("/tmp/x.media/1.png"),
            media_type: "image/png".into(),
            width: 8,
            height: 8,
            label: None,
        }],
        sent_text: None,
        sent_images: vec![],
        model_text: None,
        sent_input_image: None,
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
        sent_at_ms: 0,
        text: "Describe the image".into(),
        files: vec![],
        images: vec![
            pig_core::rollout::ImageRef {
                path: media.join("1.png"),
                media_type: "image/png".into(),
                width: 8,
                height: 8,
                label: None,
            },
            pig_core::rollout::ImageRef {
                path: media.join("gone.png"),
                media_type: "image/png".into(),
                width: 8,
                height: 8,
                label: None,
            },
        ],
        sent_text: None,
        sent_images: vec![],
        model_text: None,
        sent_input_image: None,
    }];
    let (history, _) = pig_core::rollout::rebuild_history_with_media_root(
        &records,
        "sys".into(),
        None,
        &media,
        None,
    );
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
    use pig_provider::ChatImage;
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
    let (port, log) = pig_provider::mock::start_mock_server_with_log();
    let dir = std::env::temp_dir().join(format!("pig-core-paste-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join(pig_provider::mock::MOCK_FILE_NAME),
        pig_provider::mock::MOCK_FILE_CONTENT,
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
            permissions: ExecMode::Auto.permissions(),
            delivery: pig_protocol::MessageDelivery::Queue,
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

fn logged_chat_requests(log: &std::sync::Mutex<Vec<String>>) -> Vec<serde_json::Value> {
    log.lock()
        .expect("request log")
        .iter()
        .filter_map(|body| serde_json::from_str::<serde_json::Value>(body).ok())
        .filter(|request| request["stream"] == true && request["messages"][0]["role"] == "system")
        .collect()
}

async fn assert_paste_prefix_after_restart(input_image: bool, name: &str) {
    let (log, dir, data_dir, agent) = setup_paste(name, input_image);
    let session_id = common::new_session(&agent, dir.clone()).await;
    send_paste_and_wait(&agent, &session_id, png_bytes(2100, 100, false)).await;
    let before = logged_chat_requests(&log)
        .last()
        .expect("request before restart")
        .clone();
    assert!(
        before
            .to_string()
            .contains("compressed to fit model limits"),
        "the scenario must exercise the compression caption"
    );
    agent.shutdown();

    let agent = pig_core::spawn_agent_with_data_dir(
        Some(dir.join("config.toml")),
        dir.clone(),
        data_dir.clone(),
    );
    agent
        .ops
        .send(Op::OpenSession {
            session_id: session_id.clone(),
        })
        .await
        .unwrap();
    common::recv_until(&agent.events, std::time::Duration::from_secs(10), |event| {
        matches!(
            event,
            Event::TurnComplete {
                duration_ms: 0,
                stats: None,
                finish_reason: None,
                ..
            }
        )
    })
    .await;
    agent
        .ops
        .send(Op::SendMessage {
            session_id,
            content: "continue after image restart".into(),
            files: vec![],
            images: vec![],
            permissions: ExecMode::Auto.permissions(),
            delivery: pig_protocol::MessageDelivery::Queue,
        })
        .await
        .unwrap();
    common::recv_until(&agent.events, std::time::Duration::from_secs(20), |event| {
        matches!(event, Event::TurnComplete { .. })
    })
    .await;
    let after = logged_chat_requests(&log)
        .last()
        .expect("request after restart")
        .clone();
    let before_messages = before["messages"].as_array().expect("before messages");
    let after_messages = after["messages"].as_array().expect("after messages");
    assert!(after_messages.len() > before_messages.len());
    for (index, message) in before_messages.iter().enumerate() {
        assert_eq!(
            message, &after_messages[index],
            "old message {index} changed"
        );
    }
    assert_eq!(before["tools"], after["tools"]);
    agent.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn paste_vision_prefix_survives_restart() {
    assert_paste_prefix_after_restart(true, "restart-vision").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn paste_nonvision_prefix_survives_restart() {
    assert_paste_prefix_after_restart(false, "restart-nonvision").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn paste_nonvision_then_vision_rehydrates_after_restart() {
    let (log, dir, data_dir, agent) = setup_paste("switch-capability", false);
    let session_id = common::new_session(&agent, dir.clone()).await;
    send_paste_and_wait(&agent, &session_id, png_bytes(32, 32, false)).await;
    agent.shutdown();

    let config_path = dir.join("config.toml");
    let config = std::fs::read_to_string(&config_path).unwrap().replace(
        "max_output_tokens = 8192",
        "max_output_tokens = 8192\ninput_image = true",
    );
    std::fs::write(&config_path, config).unwrap();
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), dir.clone(), data_dir.clone());
    agent
        .ops
        .send(Op::OpenSession {
            session_id: session_id.clone(),
        })
        .await
        .unwrap();
    common::recv_until(&agent.events, std::time::Duration::from_secs(10), |event| {
        matches!(
            event,
            Event::TurnComplete {
                duration_ms: 0,
                stats: None,
                finish_reason: None,
                ..
            }
        )
    })
    .await;
    agent
        .ops
        .send(Op::SendMessage {
            session_id: session_id.clone(),
            content: "continue with vision enabled".into(),
            files: vec![],
            images: vec![],
            permissions: ExecMode::Auto.permissions(),
            delivery: pig_protocol::MessageDelivery::Queue,
        })
        .await
        .unwrap();
    common::recv_until(&agent.events, std::time::Duration::from_secs(20), |event| {
        matches!(event, Event::TurnComplete { .. })
    })
    .await;
    let after = logged_chat_requests(&log)
        .last()
        .expect("request after capability switch")
        .clone()
        .to_string();
    let rollout = std::fs::read_to_string(
        data_dir
            .join("sessions")
            .join(format!("{session_id}.jsonl")),
    )
    .unwrap();
    assert!(
        after.contains("image_url"),
        "image should be rehydrated: {after}\nrollout: {rollout}"
    );
    assert!(
        !after.contains("were not sent"),
        "the old non-vision placeholder must be replaced: {after}"
    );
    agent.shutdown();
}

fn setup_paste_pair(
    name: &str,
    initial_vision: bool,
) -> (
    std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    PathBuf,
    PathBuf,
    pig_core::AgentHandle,
) {
    let (port, log) = pig_provider::mock::start_mock_server_with_log();
    let dir =
        std::env::temp_dir().join(format!("pig-core-paste-pair-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join(pig_provider::mock::MOCK_FILE_NAME),
        pig_provider::mock::MOCK_FILE_CONTENT,
    )
    .unwrap();
    let initial = if initial_vision {
        "input_image = true\n"
    } else {
        ""
    };
    let other = if initial_vision {
        ""
    } else {
        "input_image = true\n"
    };
    let config_path = dir.join("config.toml");
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
{initial}
[[providers.models]]
id = "other-model"
context_window = 128000
max_output_tokens = 8192
{other}"#
        ),
    )
    .unwrap();
    let data_dir = dir.join("data");
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), dir.clone(), data_dir.clone());
    (log, dir, data_dir, agent)
}

async fn assert_live_model_switch_prefix(initial_vision: bool, name: &str) {
    let (log, dir, data_dir, agent) = setup_paste_pair(name, initial_vision);
    let session_id = common::new_session(&agent, dir.clone()).await;
    send_paste_and_wait(&agent, &session_id, png_bytes(64, 48, false)).await;
    agent
        .ops
        .send(Op::SetModel {
            session_id: session_id.clone(),
            provider_id: "mock".into(),
            model_id: "other-model".into(),
            reasoning_level: None,
        })
        .await
        .unwrap();
    agent
        .ops
        .send(Op::SendMessage {
            session_id: session_id.clone(),
            content: "after live capability switch".into(),
            files: vec![],
            images: vec![],
            permissions: ExecMode::Auto.permissions(),
            delivery: pig_protocol::MessageDelivery::Queue,
        })
        .await
        .unwrap();
    common::recv_until(&agent.events, std::time::Duration::from_secs(20), |event| {
        matches!(event, Event::TurnComplete { .. })
    })
    .await;
    let before = logged_chat_requests(&log)
        .last()
        .expect("request after live switch")
        .clone()
        .to_string();
    if initial_vision {
        assert!(
            !before.contains("image_url"),
            "live switch must remove images"
        );
        assert!(before.contains("were not sent"));
    } else {
        assert!(
            before.contains("image_url"),
            "live switch must restore images"
        );
        assert!(!before.contains("were not sent"));
    }
    agent.shutdown();

    let agent = pig_core::spawn_agent_with_data_dir(
        Some(dir.join("config.toml")),
        dir.clone(),
        data_dir.clone(),
    );
    agent
        .ops
        .send(Op::OpenSession {
            session_id: session_id.clone(),
        })
        .await
        .unwrap();
    common::recv_until(&agent.events, std::time::Duration::from_secs(10), |event| {
        matches!(
            event,
            Event::TurnComplete {
                duration_ms: 0,
                stats: None,
                finish_reason: None,
                ..
            }
        )
    })
    .await;
    agent
        .ops
        .send(Op::SendMessage {
            session_id,
            content: "after switch and restart".into(),
            files: vec![],
            images: vec![],
            permissions: ExecMode::Auto.permissions(),
            delivery: pig_protocol::MessageDelivery::Queue,
        })
        .await
        .unwrap();
    common::recv_until(&agent.events, std::time::Duration::from_secs(20), |event| {
        matches!(event, Event::TurnComplete { .. })
    })
    .await;
    let before = logged_chat_requests(&log)
        .iter()
        .rfind(|request| {
            let text = request.to_string();
            text.contains("after live capability switch")
                && !text.contains("after switch and restart")
        })
        .expect("request before restart")
        .clone();
    let after = logged_chat_requests(&log)
        .last()
        .expect("request after restart")
        .clone();
    let before_messages = before["messages"].as_array().expect("before messages");
    let after_messages = after["messages"].as_array().expect("after messages");
    assert!(after_messages.len() > before_messages.len());
    for (index, message) in before_messages.iter().enumerate() {
        assert_eq!(
            message, &after_messages[index],
            "old message {index} changed"
        );
    }
    assert_eq!(before["tools"], after["tools"]);
    agent.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_nonvision_to_vision_then_restart_keeps_prefix() {
    assert_live_model_switch_prefix(false, "live-off-on").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_vision_to_nonvision_then_restart_keeps_prefix() {
    assert_live_model_switch_prefix(true, "live-on-off").await;
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
            permissions: ExecMode::Auto.permissions(),
            delivery: pig_protocol::MessageDelivery::Queue,
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

// ---------- ReadMediaFile PDF (documents) ----------

fn pdf_bytes() -> Vec<u8> {
    b"%PDF-1.4\n1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n2 0 obj\n<< /Type /Pages /Kids [] /Count 0 >>\nendobj\ntrailer\n<< /Root 1 0 R >>\n%%EOF\n".to_vec()
}

fn pdf_config(
    port: u16,
    format: pig_protocol::ApiFormat,
    input_image: bool,
    input_pdf: bool,
) -> String {
    let format = match format {
        pig_protocol::ApiFormat::OpenAiChat => "OpenAiChat",
        pig_protocol::ApiFormat::OpenAiResponses => "OpenAiResponses",
        pig_protocol::ApiFormat::AnthropicMessages => "AnthropicMessages",
    };
    format!(
        r#"default_provider = "mock"
default_model = "mock-model"

[[providers]]
id = "mock"
name = "Mock"
base_url = "http://127.0.0.1:{port}/v1"
api_key = "mock-key"
api_format = "{format}"
enabled = true

[[providers.models]]
id = "mock-model"
context_window = 128000
max_output_tokens = 8192
input_image = {input_image}
input_pdf = {input_pdf}
"#
    )
}

fn setup_pdf(
    name: &str,
    format: pig_protocol::ApiFormat,
    input_image: bool,
    input_pdf: bool,
) -> (PathBuf, PathBuf, PathBuf, pig_provider::mock::RequestLog) {
    let (port, log) = pig_provider::mock::start_mock_server_with_log();
    let dir = temp_dir(&format!("pdf-{name}"));
    // The mock's media scenario names pic.png; the tool sniffs magic bytes, so
    // the PDF bytes can live under that name.
    std::fs::write(dir.join("pic.png"), pdf_bytes()).unwrap();
    let config_path = dir.join("config.toml");
    std::fs::write(
        &config_path,
        pdf_config(port, format, input_image, input_pdf),
    )
    .unwrap();
    let data_dir = dir.join("data");
    (config_path, dir, data_dir, log)
}

async fn run_pdf_turn(agent: &pig_core::AgentHandle, session_id: &str) -> Vec<Event> {
    agent
        .ops
        .send(Op::SendMessage {
            session_id: session_id.to_string(),
            content: format!(
                "{} read the document",
                pig_provider::mock::SCENARIO_MEDIA_TRIGGER
            ),
            files: vec![],
            images: vec![],
            permissions: ExecMode::Auto.permissions(),
            delivery: pig_protocol::MessageDelivery::Queue,
        })
        .await
        .unwrap();
    common::recv_until(&agent.events, std::time::Duration::from_secs(20), |e| {
        matches!(e, Event::TurnComplete { .. })
    })
    .await
}

fn pdf_wire_marker(format: pig_protocol::ApiFormat) -> &'static str {
    match format {
        pig_protocol::ApiFormat::OpenAiChat => "\"file_data\":\"data:application/pdf;base64,",
        pig_protocol::ApiFormat::OpenAiResponses => "\"type\":\"input_file\"",
        pig_protocol::ApiFormat::AnthropicMessages => "\"type\":\"document\"",
    }
}

async fn pdf_round_trips_live_restart_and_rollout(format: pig_protocol::ApiFormat, label: &str) {
    let (config_path, dir, data_dir, log) = setup_pdf(label, format, false, true);
    let agent = pig_core::spawn_agent_with_data_dir(
        Some(config_path.clone()),
        dir.clone(),
        data_dir.clone(),
    );
    let session_id = common::new_session(&agent, dir.clone()).await;
    let events = run_pdf_turn(&agent, &session_id).await;
    assert!(
        events.iter().any(|e| matches!(
            e,
            Event::ToolCallEnd { output, is_error: false, .. }
                if output.contains("Read PDF")
        )),
        "the PDF tool result succeeds: {events:#?}"
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            Event::TextDone { full_text, .. } if full_text.contains(pig_provider::mock::MEDIA_MARKER)
        )),
        "the turn completes after the document: {events:#?}"
    );
    // Persisted reference + bytes beside the rollout, never base64 in JSONL.
    let media_dir = data_dir
        .join("sessions")
        .join(format!("{session_id}.media"));
    let pdf_file = media_dir.join("1.pdf");
    assert!(pdf_file.is_file(), "document persisted at {pdf_file:?}");
    assert_eq!(std::fs::read(&pdf_file).unwrap(), pdf_bytes());
    let rollout = std::fs::read_to_string(
        data_dir
            .join("sessions")
            .join(format!("{session_id}.jsonl")),
    )
    .unwrap();
    assert!(rollout.contains("\"documents\""), "{rollout}");
    assert!(
        !rollout.contains("data_base64") && !rollout.contains("JVBERi"),
        "no document bytes in the rollout: {rollout}"
    );
    let live = log
        .lock()
        .unwrap()
        .iter()
        .rev()
        .find(|body| body.contains(pdf_wire_marker(format)))
        .cloned()
        .expect("live request carries the provider document block");
    let before_restart = log.lock().unwrap().len();
    agent.shutdown();

    // Restart: the rehydrated document reaches the next request and the media
    // reference survives replay.
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), dir.clone(), data_dir.clone());
    agent
        .ops
        .send(Op::OpenSession {
            session_id: session_id.clone(),
        })
        .await
        .unwrap();
    common::recv_until(&agent.events, std::time::Duration::from_secs(10), |e| {
        matches!(
            e,
            Event::TurnComplete {
                duration_ms: 0,
                stats: None,
                ..
            }
        )
    })
    .await;
    run_pdf_turn(&agent, &session_id).await;
    let bodies = log.lock().unwrap().clone();
    let restart = &bodies[before_restart..];
    let rehydrated = restart
        .iter()
        .rev()
        .find(|body| body.contains(pdf_wire_marker(format)))
        .unwrap_or_else(|| panic!("restart request lost the document: {restart:#?}"));
    assert!(
        rehydrated.contains("Read PDF"),
        "the tool result text survives replay"
    );
    // The rebuilt prefix must be byte-identical to the live projection: the
    // document filename/label/payload all have to round-trip unchanged.
    let key = if matches!(format, pig_protocol::ApiFormat::OpenAiResponses) {
        "input"
    } else {
        "messages"
    };
    let live_json: serde_json::Value = serde_json::from_str(&live).unwrap();
    let restart_json: serde_json::Value = serde_json::from_str(rehydrated).unwrap();
    let live_messages = live_json[key].as_array().expect("live messages");
    let restart_messages = restart_json[key].as_array().expect("restart messages");
    assert!(
        restart_messages.len() > live_messages.len(),
        "restart appends the new turn: {} -> {}",
        live_messages.len(),
        restart_messages.len()
    );
    assert_eq!(
        &restart_messages[..live_messages.len()],
        live_messages,
        "the restarted history prefix must equal the live projection byte for byte"
    );
    agent.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pdf_document_round_trips_chat() {
    pdf_round_trips_live_restart_and_rollout(pig_protocol::ApiFormat::OpenAiChat, "chat").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pdf_document_round_trips_responses() {
    pdf_round_trips_live_restart_and_rollout(pig_protocol::ApiFormat::OpenAiResponses, "responses")
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pdf_document_round_trips_anthropic() {
    pdf_round_trips_live_restart_and_rollout(
        pig_protocol::ApiFormat::AnthropicMessages,
        "anthropic",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pdf_capability_off_degrades_without_writing_media() {
    let (config_path, dir, data_dir, log) =
        setup_pdf("cap-off", pig_protocol::ApiFormat::OpenAiChat, true, false);
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), dir.clone(), data_dir.clone());
    let session_id = common::new_session(&agent, dir.clone()).await;
    let events = run_pdf_turn(&agent, &session_id).await;
    assert!(
        events.iter().any(|e| matches!(
            e,
            Event::ToolCallEnd { output, is_error: false, .. }
                if output.contains("PDF document(s) were not sent")
                    && output.contains("does not support PDF input")
        )),
        "capability-off PDF degrades to deterministic text: {events:#?}"
    );
    let media_dir = data_dir
        .join("sessions")
        .join(format!("{session_id}.media"));
    assert!(
        !media_dir.join("1.pdf").exists(),
        "no document/media write when the capability is off"
    );
    let bodies = log.lock().unwrap().clone();
    assert!(
        bodies.iter().all(|body| {
            !body.contains("JVBERi")
                && !body.contains("\"file_data\"")
                && !body.contains("\"input_file\"")
                && !body.contains("\"type\":\"document\"")
                && !body.contains("\"type\":\"file\"")
        }),
        "the unsupported document block/base64 never reaches the wire: {bodies:#?}"
    );
    agent.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pdf_only_model_degrades_image_input() {
    let (config_path, dir, data_dir, _log) = setup_pdf(
        "pdf-only-image",
        pig_protocol::ApiFormat::OpenAiChat,
        false,
        true,
    );
    std::fs::write(dir.join("pic.png"), png_bytes(32, 24, false)).unwrap();
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), dir.clone(), data_dir.clone());
    let session_id = common::new_session(&agent, dir.clone()).await;
    let events = run_pdf_turn(&agent, &session_id).await;
    assert!(
        events.iter().any(|e| matches!(
            e,
            Event::ToolCallEnd { output, is_error: false, .. }
                if output.contains("image(s) were not sent")
                    && output.contains("does not support image input")
        )),
        "an image under a PDF-only model degrades to text: {events:#?}"
    );
    let media_dir = data_dir
        .join("sessions")
        .join(format!("{session_id}.media"));
    assert!(
        !media_dir.join("1.png").exists(),
        "the unsupported image is not persisted"
    );
    agent.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn corrupt_pdf_is_rejected_deterministically() {
    let dir = temp_dir("pdf-corrupt");
    std::fs::write(dir.join("broken.pdf"), b"%PDF-1.4\ntruncated without eof").unwrap();
    let state = SessionToolState::for_test();
    let mut tracker = ChangeTracker::default();
    let (output, is_error, _, _, _) = tool::execute(
        &call("ReadMediaFile", serde_json::json!({"path": "broken.pdf"})),
        ToolContext {
            cwd: &dir,
            tracker: &mut tracker,
            state: &state,
            fs_grant: None,
            sandbox_override: Default::default(),
            sandbox_profile: Default::default(),
            additional_permissions: Default::default(),
        },
    )
    .await;
    assert!(is_error);
    assert!(
        output.contains("truncated or corrupted"),
        "deterministic corrupt-PDF guidance: {output}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_tool_points_pdfs_at_read_media_file() {
    let dir = temp_dir("pdf-read-guidance");
    std::fs::write(dir.join("doc.pdf"), pdf_bytes()).unwrap();
    let state = SessionToolState::for_test();
    let mut tracker = ChangeTracker::default();
    let (output, is_error, _, _, _) = tool::execute(
        &call("Read", serde_json::json!({"path": "doc.pdf"})),
        ToolContext {
            cwd: &dir,
            tracker: &mut tracker,
            state: &state,
            fs_grant: None,
            sandbox_override: Default::default(),
            sandbox_profile: Default::default(),
            additional_permissions: Default::default(),
        },
    )
    .await;
    assert!(is_error);
    assert!(
        output.contains("ReadMediaFile") && output.contains("PDF"),
        "Read only adds English PDF guidance: {output}"
    );
}

/// The marker bytes inside ordinary text do not make the file a PDF: Read
/// still returns the text and ReadMediaFile refuses it as unrecognized.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn text_with_pdf_markers_stays_text() {
    let dir = temp_dir("pdf-text-markers");
    let body = format!(
        "heading\nembedded %PDF-1.4 marker\n{}\n%%EOF\n",
        "x".repeat(64)
    );
    std::fs::write(dir.join("notes.md"), &body).unwrap();
    let state = SessionToolState::for_test();
    let mut tracker = ChangeTracker::default();
    let (output, is_error, ..) = tool::execute(
        &call("Read", serde_json::json!({"path": "notes.md"})),
        ToolContext {
            cwd: &dir,
            tracker: &mut tracker,
            state: &state,
            fs_grant: None,
            sandbox_override: Default::default(),
            sandbox_profile: Default::default(),
            additional_permissions: Default::default(),
        },
    )
    .await;
    assert!(!is_error, "Read must return the text: {output}");
    assert!(output.contains("embedded %PDF-1.4 marker"), "{output}");

    let (output, is_error, ..) = tool::execute(
        &call("ReadMediaFile", serde_json::json!({"path": "notes.md"})),
        ToolContext {
            cwd: &dir,
            tracker: &mut tracker,
            state: &state,
            fs_grant: None,
            sandbox_override: Default::default(),
            sandbox_profile: Default::default(),
            additional_permissions: Default::default(),
        },
    )
    .await;
    assert!(is_error);
    assert!(
        output.contains("Not a recognizable image"),
        "plain text must not classify as a PDF: {output}"
    );
    assert!(
        !output.contains("truncated or corrupted"),
        "no PDF guidance for a plain text file: {output}"
    );
}

/// The header rule tolerates a UTF-8 BOM / ASCII whitespace prefix; a header
/// at byte 900 is still inside the probe window.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pdf_header_after_whitespace_offset_is_accepted() {
    let dir = temp_dir("pdf-offset");
    let mut bytes = vec![b' '; 900];
    bytes.extend_from_slice(&pdf_bytes());
    std::fs::write(dir.join("offset.pdf"), &bytes).unwrap();
    let state = SessionToolState::for_test();
    let mut tracker = ChangeTracker::default();
    let (output, is_error, _, _, media) = tool::execute(
        &call("ReadMediaFile", serde_json::json!({"path": "offset.pdf"})),
        ToolContext {
            cwd: &dir,
            tracker: &mut tracker,
            state: &state,
            fs_grant: None,
            sandbox_override: Default::default(),
            sandbox_profile: Default::default(),
            additional_permissions: Default::default(),
        },
    )
    .await;
    assert!(!is_error, "{output}");
    assert_eq!(media.documents.len(), 1);
    assert_eq!(media.documents[0].bytes, bytes);
}

/// The size cap is enforced on the bounded read itself, so a PDF just over
/// 10MiB is rejected without reading the whole file.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pdf_over_ten_megabytes_is_rejected() {
    let dir = temp_dir("pdf-too-large");
    let mut bytes = Vec::with_capacity(MAX_PDF_FILE_BYTES as usize + 7);
    bytes.extend_from_slice(b"%PDF-1.4\n");
    bytes.resize(MAX_PDF_FILE_BYTES as usize, b' ');
    bytes.extend_from_slice(b"\n%%EOF\n");
    assert!(bytes.len() as u64 > MAX_PDF_FILE_BYTES);
    std::fs::write(dir.join("large.pdf"), &bytes).unwrap();
    let state = SessionToolState::for_test();
    let mut tracker = ChangeTracker::default();
    let (output, is_error, ..) = tool::execute(
        &call("ReadMediaFile", serde_json::json!({"path": "large.pdf"})),
        ToolContext {
            cwd: &dir,
            tracker: &mut tracker,
            state: &state,
            fs_grant: None,
            sandbox_override: Default::default(),
            sandbox_profile: Default::default(),
            additional_permissions: Default::default(),
        },
    )
    .await;
    assert!(is_error, "{output}");
    assert!(output.contains("10MB limit"), "{output}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_pdf_after_restart_degrades() {
    let (config_path, dir, data_dir, log) =
        setup_pdf("missing", pig_protocol::ApiFormat::OpenAiChat, false, true);
    let agent = pig_core::spawn_agent_with_data_dir(
        Some(config_path.clone()),
        dir.clone(),
        data_dir.clone(),
    );
    let session_id = common::new_session(&agent, dir.clone()).await;
    run_pdf_turn(&agent, &session_id).await;
    agent.shutdown();
    std::fs::remove_file(
        data_dir
            .join("sessions")
            .join(format!("{session_id}.media"))
            .join("1.pdf"),
    )
    .unwrap();
    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), dir.clone(), data_dir);
    agent
        .ops
        .send(Op::OpenSession {
            session_id: session_id.clone(),
        })
        .await
        .unwrap();
    common::recv_until(&agent.events, std::time::Duration::from_secs(10), |e| {
        matches!(
            e,
            Event::TurnComplete {
                duration_ms: 0,
                stats: None,
                ..
            }
        )
    })
    .await;
    run_pdf_turn(&agent, &session_id).await;
    let bodies = log.lock().unwrap().join("\n");
    assert!(
        bodies.contains("PDF document(s) no longer available"),
        "missing document degrades deterministically: {bodies}"
    );
    agent.shutdown();
}

/// A corrupt/truncated persisted PDF is skipped by rehydration and produces
/// the same deterministic note as a missing file instead of reaching the wire.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn corrupt_pdf_after_restart_degrades() {
    let (config_path, dir, data_dir, log) = setup_pdf(
        "corrupt-restart",
        pig_protocol::ApiFormat::OpenAiChat,
        false,
        true,
    );
    let agent = pig_core::spawn_agent_with_data_dir(
        Some(config_path.clone()),
        dir.clone(),
        data_dir.clone(),
    );
    let session_id = common::new_session(&agent, dir.clone()).await;
    run_pdf_turn(&agent, &session_id).await;
    agent.shutdown();
    let media = data_dir
        .join("sessions")
        .join(format!("{session_id}.media"))
        .join("1.pdf");
    let mut bytes = std::fs::read(&media).unwrap();
    let eof = bytes
        .windows(5)
        .position(|window| window == b"%%EOF")
        .expect("fixture has an EOF marker");
    bytes.truncate(eof);
    std::fs::write(&media, &bytes).unwrap();
    let live_requests = log.lock().unwrap().len();

    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), dir.clone(), data_dir);
    agent
        .ops
        .send(Op::OpenSession {
            session_id: session_id.clone(),
        })
        .await
        .unwrap();
    common::recv_until(&agent.events, std::time::Duration::from_secs(10), |e| {
        matches!(
            e,
            Event::TurnComplete {
                duration_ms: 0,
                stats: None,
                ..
            }
        )
    })
    .await;
    run_pdf_turn(&agent, &session_id).await;
    let bodies: Vec<String> = log.lock().unwrap()[live_requests..].to_vec();
    assert!(
        bodies
            .iter()
            .any(|body| body.contains("PDF document(s) no longer available")),
        "corrupt document degrades deterministically: {bodies:#?}"
    );
    assert!(
        bodies
            .iter()
            .all(|body| !body.contains(pdf_wire_marker(pig_protocol::ApiFormat::OpenAiChat))),
        "corrupt document bytes never reach the wire: {bodies:#?}"
    );
    agent.shutdown();
}

/// A live model switch reprojects documents out of and back into the same
/// durable history (projected_input_pdf participates in the rebuild).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pdf_reprojects_across_live_model_capability_switches() {
    let (port, log) = pig_provider::mock::start_mock_server_with_log();
    let dir = temp_dir("pdf-reproject");
    std::fs::write(dir.join("pic.png"), pdf_bytes()).unwrap();
    let config_path = dir.join("config.toml");
    std::fs::write(
        &config_path,
        format!(
            r#"default_provider = "mock"
default_model = "pdf-model"

[[providers]]
id = "mock"
name = "Mock"
base_url = "http://127.0.0.1:{port}/v1"
api_key = "mock-key"
api_format = "OpenAiChat"
enabled = true

[[providers.models]]
id = "pdf-model"
context_window = 128000
max_output_tokens = 8192
input_pdf = true

[[providers.models]]
id = "text-model"
context_window = 128000
max_output_tokens = 8192
"#
        ),
    )
    .unwrap();
    let data_dir = dir.join("data");
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), dir.clone(), data_dir.clone());
    let session_id = common::new_session(&agent, dir.clone()).await;
    run_pdf_turn(&agent, &session_id).await;

    agent
        .ops
        .send(Op::SetModel {
            session_id: session_id.clone(),
            provider_id: "mock".into(),
            model_id: "text-model".into(),
            reasoning_level: None,
        })
        .await
        .unwrap();
    agent
        .ops
        .send(Op::SendMessage {
            session_id: session_id.clone(),
            content: "continue without pdf".into(),
            files: vec![],
            images: vec![],
            permissions: ExecMode::Auto.permissions(),
            delivery: pig_protocol::MessageDelivery::Queue,
        })
        .await
        .unwrap();
    common::recv_until(&agent.events, std::time::Duration::from_secs(20), |e| {
        matches!(e, Event::TurnComplete { .. } | Event::Error { .. })
    })
    .await;
    let bodies = log.lock().unwrap().clone();
    let without = bodies
        .iter()
        .rev()
        .find(|body| body.contains("continue without pdf"))
        .expect("non-pdf request");
    assert!(
        !without.contains(pdf_wire_marker(pig_protocol::ApiFormat::OpenAiChat)),
        "the switched model does not receive a document block"
    );
    assert!(
        without.contains("PDF document(s) were not sent"),
        "the reprojected history carries the degradation: {without}"
    );

    agent
        .ops
        .send(Op::SetModel {
            session_id: session_id.clone(),
            provider_id: "mock".into(),
            model_id: "pdf-model".into(),
            reasoning_level: None,
        })
        .await
        .unwrap();
    agent
        .ops
        .send(Op::SendMessage {
            session_id: session_id.clone(),
            content: "continue with pdf".into(),
            files: vec![],
            images: vec![],
            permissions: ExecMode::Auto.permissions(),
            delivery: pig_protocol::MessageDelivery::Queue,
        })
        .await
        .unwrap();
    common::recv_until(&agent.events, std::time::Duration::from_secs(20), |e| {
        matches!(e, Event::TurnComplete { .. } | Event::Error { .. })
    })
    .await;
    let bodies = log.lock().unwrap().clone();
    let with = bodies
        .iter()
        .rev()
        .find(|body| body.contains("continue with pdf"))
        .expect("pdf request");
    assert!(
        with.contains(pdf_wire_marker(pig_protocol::ApiFormat::OpenAiChat)),
        "switching back rehydrates the document: {with}"
    );
    agent.shutdown();
}

/// A model switch with unchanged media capabilities still changes the context
/// window. An opaque PDF's cached reserve must be repriced for the new window
/// (never kept at the old window's value, and never re-scanned), while a
/// visible page-tree cost stays put. The next request's composition proves the
/// live fallback was reconciled; the persisted tag proves it survives restart.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pdf_window_fallback_is_repriced_after_a_model_switch() {
    let (port, log) = pig_provider::mock::start_mock_server_with_log();
    let dir = temp_dir("pdf-window-switch");
    // Header + EOF but no page tree: the bounded window fallback applies.
    std::fs::write(
        dir.join("pic.png"),
        b"%PDF-1.4\n1 0 obj << >> endobj\n%%EOF\n",
    )
    .unwrap();
    let config_path = dir.join("config.toml");
    std::fs::write(
        &config_path,
        format!(
            r#"default_provider = "mock"
default_model = "wide-model"

[[providers]]
id = "mock"
name = "Mock"
base_url = "http://127.0.0.1:{port}/v1"
api_key = "mock-key"
api_format = "OpenAiChat"
enabled = true

[[providers.models]]
id = "wide-model"
context_window = 1000000
max_output_tokens = 8192
input_pdf = true

[[providers.models]]
id = "narrow-model"
context_window = 64000
max_output_tokens = 8192
input_pdf = true
"#
        ),
    )
    .unwrap();
    let data_dir = dir.join("data");
    let agent = pig_core::spawn_agent_with_data_dir(
        Some(config_path.clone()),
        dir.clone(),
        data_dir.clone(),
    );
    let session_id = common::new_session(&agent, dir.clone()).await;
    let first = run_pdf_turn(&agent, &session_id).await;
    let breakdowns = |events: &[Event]| {
        events
            .iter()
            .filter_map(|event| match event {
                Event::ContextBreakdownChanged { breakdown, .. } => Some(*breakdown),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    let wide = breakdowns(&first).last().copied().expect("composition");
    let wide_reserve = pig_utils::pdf::unknown_document_token_reserve(1_000_000);
    assert!(
        wide.messages >= wide_reserve,
        "the wide model carries the wide fallback: {wide:?}"
    );

    agent
        .ops
        .send(Op::SetModel {
            session_id: session_id.clone(),
            provider_id: "mock".into(),
            model_id: "narrow-model".into(),
            reasoning_level: None,
        })
        .await
        .unwrap();
    agent
        .ops
        .send(Op::SendMessage {
            session_id: session_id.clone(),
            content: "after the window switch".into(),
            files: vec![],
            images: vec![],
            permissions: ExecMode::Auto.permissions(),
            delivery: pig_protocol::MessageDelivery::Queue,
        })
        .await
        .unwrap();
    let second = common::recv_until(&agent.events, std::time::Duration::from_secs(20), |event| {
        matches!(event, Event::TurnComplete { .. })
    })
    .await;
    assert!(
        !second
            .iter()
            .any(|event| matches!(event, Event::ContextCompacted { .. })),
        "the reconciled window must not force a compaction: {second:#?}"
    );
    let narrow = breakdowns(&second)
        .last()
        .copied()
        .expect("composition after switch");
    let narrow_reserve = pig_utils::pdf::unknown_document_token_reserve(64_000);
    assert!(
        narrow.messages < wide.messages - 200_000,
        "the cached fallback was repriced downward for the narrow window: {wide:?} -> {narrow:?}"
    );
    assert!(
        narrow.messages < narrow_reserve + 100_000,
        "the narrow request is priced near its own reserve, not the old one: {narrow:?}"
    );
    // Same capabilities: the document still reaches the switched model.
    let bodies = log.lock().unwrap().clone();
    let request = bodies
        .iter()
        .rev()
        .find(|body| body.contains("after the window switch"))
        .expect("request after the window switch");
    assert!(
        request.contains(pdf_wire_marker(pig_protocol::ApiFormat::OpenAiChat)),
        "the switched model still receives the document: {request}"
    );
    agent.shutdown();

    // A restart resolves the narrow model again; rehydration must reprice the
    // durable fallback tag rather than revive the wide-window reserve. The next
    // turn's composition proves the loaded history carries the new price.
    let agent = pig_core::spawn_agent_with_data_dir(
        Some(dir.join("config.toml")),
        dir.clone(),
        data_dir.clone(),
    );
    agent
        .ops
        .send(Op::OpenSession {
            session_id: session_id.clone(),
        })
        .await
        .expect("open session");
    common::recv_until(&agent.events, std::time::Duration::from_secs(20), |event| {
        matches!(event, Event::SessionConfigured { .. })
    })
    .await;
    agent
        .ops
        .send(Op::SendMessage {
            session_id: session_id.clone(),
            content: "after restart".into(),
            files: vec![],
            images: vec![],
            permissions: ExecMode::Auto.permissions(),
            delivery: pig_protocol::MessageDelivery::Queue,
        })
        .await
        .unwrap();
    // Drain the replay's synthetic turn events first: they also end with
    // TurnComplete and would otherwise satisfy the wait before the real turn.
    common::recv_until(
        &agent.events,
        std::time::Duration::from_secs(20),
        |event| matches!(event, Event::UserMessage { text, .. } if text == "after restart"),
    )
    .await;
    let reopened = common::recv_until(&agent.events, std::time::Duration::from_secs(20), |event| {
        matches!(event, Event::TurnComplete { .. })
    })
    .await;
    assert!(
        !reopened
            .iter()
            .any(|event| matches!(event, Event::ContextCompacted { .. })),
        "the restored fallback is repriced before the preflight: {reopened:#?}"
    );
    let restored = breakdowns(&reopened)
        .last()
        .copied()
        .unwrap_or_else(|| panic!("composition after restart: {reopened:#?}"));
    assert!(
        restored.messages < narrow_reserve + 100_000,
        "restart reprices the durable fallback for the current window: {restored:?}"
    );
    agent.shutdown();
}

/// Manual /compact after a model switch must reproject media before the
/// summary request; otherwise an unsupported PDF makes the sidecar fail and
/// compact silently falls back to truncation.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn manual_compact_reprojects_documents_after_a_model_switch() {
    let (port, log) = pig_provider::mock::start_mock_server_with_log();
    let dir = temp_dir("pdf-compact-reproject");
    std::fs::write(dir.join("pic.png"), pdf_bytes()).unwrap();
    let config_path = dir.join("config.toml");
    std::fs::write(
        &config_path,
        format!(
            r#"default_provider = "mock"
default_model = "pdf-model"

[[providers]]
id = "mock"
name = "Mock"
base_url = "http://127.0.0.1:{port}/v1"
api_key = "mock-key"
api_format = "OpenAiChat"
enabled = true

[[providers.models]]
id = "pdf-model"
context_window = 128000
max_output_tokens = 8192
input_pdf = true

[[providers.models]]
id = "text-model"
context_window = 128000
max_output_tokens = 8192
"#
        ),
    )
    .unwrap();
    let data_dir = dir.join("data");
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), dir.clone(), data_dir.clone());
    let session_id = common::new_session(&agent, dir.clone()).await;
    run_pdf_turn(&agent, &session_id).await;
    // A second complete segment so manual compaction has an older segment to
    // summarize and a current tail to keep.
    agent
        .ops
        .send(Op::SendMessage {
            session_id: session_id.clone(),
            content: "second turn before compact".into(),
            files: vec![],
            images: vec![],
            permissions: ExecMode::Auto.permissions(),
            delivery: pig_protocol::MessageDelivery::Queue,
        })
        .await
        .unwrap();
    common::recv_until(&agent.events, std::time::Duration::from_secs(20), |e| {
        matches!(e, Event::TurnComplete { .. } | Event::Error { .. })
    })
    .await;

    agent
        .ops
        .send(Op::SetModel {
            session_id: session_id.clone(),
            provider_id: "mock".into(),
            model_id: "text-model".into(),
            reasoning_level: None,
        })
        .await
        .unwrap();
    // No intervening message: the compact handler itself must align the
    // projected capabilities.
    agent
        .ops
        .send(Op::Compact {
            session_id: session_id.clone(),
            instruction: None,
        })
        .await
        .unwrap();
    let events = common::recv_until(&agent.events, std::time::Duration::from_secs(10), |e| {
        matches!(e, Event::ContextCompacted { .. } | Event::Error { .. })
    })
    .await;
    let Some(Event::ContextCompacted { summary, note, .. }) = events.last() else {
        panic!("compact failed: {events:#?}");
    };
    assert!(
        summary
            .as_deref()
            .is_some_and(|summary| summary.contains(pig_provider::mock::SUMMARY_MARKER)),
        "the summary succeeded instead of falling back: {events:#?}"
    );
    assert!(note.contains("model summary"), "{note}");
    let bodies = log.lock().unwrap().clone();
    let summary_request = bodies
        .iter()
        .rev()
        .find(|body| body.contains("[COMPACTION]"))
        .expect("summary request");
    assert!(
        !summary_request.contains(pdf_wire_marker(pig_protocol::ApiFormat::OpenAiChat))
            && !summary_request.contains("\"type\":\"file\""),
        "the unsupported PDF is reprojected before the summary: {summary_request}"
    );
    assert!(
        summary_request.contains("PDF document(s) were not sent"),
        "the reprojected history carries the deterministic degradation: {summary_request}"
    );

    agent
        .ops
        .send(Op::SendMessage {
            session_id: session_id.clone(),
            content: "after compact".into(),
            files: vec![],
            images: vec![],
            permissions: ExecMode::Auto.permissions(),
            delivery: pig_protocol::MessageDelivery::Queue,
        })
        .await
        .unwrap();
    common::recv_until(&agent.events, std::time::Duration::from_secs(20), |e| {
        matches!(e, Event::TurnComplete { .. } | Event::Error { .. })
    })
    .await;
    let bodies = log.lock().unwrap().clone();
    let after = bodies
        .iter()
        .rev()
        .find(|body| body.contains("after compact"))
        .expect("post-compact request");
    assert!(
        !after.contains(pdf_wire_marker(pig_protocol::ApiFormat::OpenAiChat)),
        "the post-compact prefix remains document-free: {after}"
    );
    agent.shutdown();
}

/// A split-turn compaction preserves an attached image on the active user:
/// the compact record stores only an index, the rebuilt User rehydrates the
/// payload, and the live/restart request prefixes stay byte-identical.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn split_compact_preserves_user_image_across_restart() {
    let (log, dir, data_dir, agent) = setup_paste("split-compact", true);
    let session_id = common::new_session(&agent, dir.clone()).await;
    send_paste_and_wait(&agent, &session_id, png_bytes(64, 48, false)).await;

    agent
        .ops
        .send(Op::Compact {
            session_id: session_id.clone(),
            instruction: None,
        })
        .await
        .unwrap();
    let compacted = common::recv_until(&agent.events, std::time::Duration::from_secs(10), |e| {
        matches!(e, Event::ContextCompacted { .. })
    })
    .await;
    assert!(
        compacted.iter().any(|e| matches!(
            e,
            Event::ContextCompacted {
                omitted,
                summary: Some(_),
                ..
            } if *omitted > 0
        )),
        "the single-turn compact succeeded: {compacted:#?}"
    );
    let rollout = std::fs::read_to_string(
        data_dir
            .join("sessions")
            .join(format!("{session_id}.jsonl")),
    )
    .unwrap();
    let compact: serde_json::Value = serde_json::from_str(
        rollout
            .lines()
            .find(|line| line.contains("\"type\":\"compact\""))
            .expect("compact record"),
    )
    .unwrap();
    assert!(
        compact["preserved_user"].is_u64(),
        "the active user is preserved by index: {compact}"
    );
    assert!(!rollout.contains("data_base64"), "rollout stores no base64");

    agent
        .ops
        .send(Op::SendMessage {
            session_id: session_id.clone(),
            content: "post compact image".into(),
            files: vec![],
            images: vec![],
            permissions: ExecMode::Auto.permissions(),
            delivery: pig_protocol::MessageDelivery::Queue,
        })
        .await
        .unwrap();
    common::recv_until(&agent.events, std::time::Duration::from_secs(20), |e| {
        matches!(e, Event::TurnComplete { .. })
    })
    .await;
    let live = log
        .lock()
        .unwrap()
        .iter()
        .find(|body| body.contains("post compact image"))
        .cloned()
        .expect("live post-compact request");
    let live_json: serde_json::Value = serde_json::from_str(&live).unwrap();
    let live_messages = live_json["messages"].as_array().expect("live messages");
    assert!(
        live.contains("data:image/png;base64,"),
        "the preserved user keeps its image payload: {live}"
    );
    assert!(
        live.contains("What is this image") && live.contains("Earlier context compacted"),
        "the preserved user and the note are in context: {live}"
    );
    agent.shutdown();

    let config_path = dir.join("config.toml");
    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), dir.clone(), data_dir);
    agent
        .ops
        .send(Op::OpenSession {
            session_id: session_id.clone(),
        })
        .await
        .unwrap();
    common::recv_until(&agent.events, std::time::Duration::from_secs(10), |e| {
        matches!(
            e,
            Event::TurnComplete {
                duration_ms: 0,
                stats: None,
                ..
            }
        )
    })
    .await;
    agent
        .ops
        .send(Op::SendMessage {
            session_id: session_id.clone(),
            content: "after restart image".into(),
            files: vec![],
            images: vec![],
            permissions: ExecMode::Auto.permissions(),
            delivery: pig_protocol::MessageDelivery::Queue,
        })
        .await
        .unwrap();
    common::recv_until(&agent.events, std::time::Duration::from_secs(20), |e| {
        matches!(e, Event::TurnComplete { .. })
    })
    .await;
    let restart = log
        .lock()
        .unwrap()
        .iter()
        .find(|body| body.contains("after restart image"))
        .cloned()
        .expect("restart request");
    let restart_json: serde_json::Value = serde_json::from_str(&restart).unwrap();
    let restart_messages = restart_json["messages"]
        .as_array()
        .expect("restart messages");
    assert!(
        restart_messages.len() > live_messages.len(),
        "restart appends the new turn"
    );
    assert_eq!(
        &restart_messages[..live_messages.len()],
        live_messages,
        "the compacted image prefix must be byte-identical after restart"
    );
    assert!(
        restart.contains("data:image/png;base64,"),
        "restart rehydrates the preserved image: {restart}"
    );
    agent.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn model_io_tracks_document_counts_without_base64() {
    let (config_path, dir, data_dir, _log) =
        setup_pdf("model-io", pig_protocol::ApiFormat::OpenAiChat, false, true);
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), dir.clone(), data_dir.clone());
    let session_id = common::new_session(&agent, dir.clone()).await;
    run_pdf_turn(&agent, &session_id).await;
    agent.shutdown();
    let trace = std::fs::read_to_string(
        data_dir
            .join("sessions")
            .join(format!("{session_id}.model-io.jsonl")),
    )
    .unwrap();
    assert!(
        trace.contains("\"documents\":1"),
        "model-io records the document count: {trace}"
    );
    assert!(
        !trace.contains("JVBERi") && !trace.contains("data_base64"),
        "model-io never records document bytes: {trace}"
    );
}

// ---------- Root tool image persistence failure ----------

async fn run_logged_media_turn(agent: &pig_core::AgentHandle, session_id: &str) -> Vec<Event> {
    agent
        .ops
        .send(Op::SendMessage {
            session_id: session_id.to_string(),
            content: format!(
                "{} read the image",
                pig_provider::mock::SCENARIO_MEDIA_TRIGGER
            ),
            files: vec![],
            images: vec![],
            permissions: ExecMode::Auto.permissions(),
            delivery: pig_protocol::MessageDelivery::Queue,
        })
        .await
        .unwrap();
    common::recv_until(&agent.events, std::time::Duration::from_secs(20), |e| {
        matches!(e, Event::TurnComplete { .. } | Event::Error { .. })
    })
    .await
}

fn tool_call_record(rollout: &str, tool: &str) -> serde_json::Value {
    rollout
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|record| record["type"] == "tool_call" && record["tool"] == tool)
        .expect("tool call record")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn normal_tool_image_keeps_live_payload_refs_and_restart_rehydration() {
    let (config_path, dir, data_dir, log) = setup_media("img-normal", true);
    let agent = pig_core::spawn_agent_with_data_dir(
        Some(config_path.clone()),
        dir.clone(),
        data_dir.clone(),
    );
    let session_id = common::new_session(&agent, dir.clone()).await;
    let events = run_logged_media_turn(&agent, &session_id).await;
    let output = events
        .iter()
        .find_map(|e| match e {
            Event::ToolCallEnd { output, .. } => Some(output.clone()),
            _ => None,
        })
        .expect("media tool end");
    assert!(output.contains("Read image pic.png"), "{output}");
    assert!(!output.contains("could not be saved"), "{output}");
    let rollout = std::fs::read_to_string(
        data_dir
            .join("sessions")
            .join(format!("{session_id}.jsonl")),
    )
    .unwrap();
    let record = tool_call_record(&rollout, "ReadMediaFile");
    assert_eq!(
        record["images"].as_array().map(Vec::len),
        Some(1),
        "successful images keep a durable ref: {rollout}"
    );
    assert_eq!(record["output"].as_str(), Some(output.as_str()));
    let media_dir = data_dir
        .join("sessions")
        .join(format!("{session_id}.media"));
    assert!(media_dir.join("1.png").is_file());
    let bodies = log.lock().unwrap().join("\n");
    assert!(
        bodies.contains("image_url") && bodies.contains("data:image/png;base64,"),
        "the live follow-up request carries the image: {bodies}"
    );
    agent.shutdown();

    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), dir.clone(), data_dir.clone());
    let replay = {
        agent
            .ops
            .send(Op::OpenSession {
                session_id: session_id.clone(),
            })
            .await
            .unwrap();
        common::recv_until(&agent.events, std::time::Duration::from_secs(10), |e| {
            matches!(
                e,
                Event::TurnComplete {
                    duration_ms: 0,
                    stats: None,
                    ..
                }
            )
        })
        .await
    };
    assert!(
        replay.iter().any(|event| matches!(
            event,
            Event::ToolCallEnd { output, .. } if output.contains("Read image pic.png")
        )),
        "replay restores the image tool result: {replay:#?}"
    );
    agent
        .ops
        .send(Op::SendMessage {
            session_id: session_id.clone(),
            content: "continue after image".into(),
            files: vec![],
            images: vec![],
            permissions: ExecMode::Auto.permissions(),
            delivery: pig_protocol::MessageDelivery::Queue,
        })
        .await
        .unwrap();
    common::recv_until(&agent.events, std::time::Duration::from_secs(20), |e| {
        matches!(e, Event::TurnComplete { .. } | Event::Error { .. })
    })
    .await;
    let bodies = log.lock().unwrap().clone();
    let restart = bodies
        .iter()
        .rev()
        .find(|body| body.contains("continue after image"))
        .expect("post-restart request");
    assert!(
        restart.contains("image_url") && restart.contains("data:image/png;base64,"),
        "restart rehydrates the image: {restart}"
    );
    agent.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tool_image_write_failure_degrades_live_and_rollout_identically() {
    let (config_path, dir, data_dir, log) = setup_media("img-write-fail", true);
    let agent = pig_core::spawn_agent_with_data_dir(
        Some(config_path.clone()),
        dir.clone(),
        data_dir.clone(),
    );
    let session_id = common::new_session(&agent, dir.clone()).await;
    // Force create_dir_all/write failure by occupying the media path with a
    // regular file.
    let media_path = data_dir
        .join("sessions")
        .join(format!("{session_id}.media"));
    std::fs::create_dir_all(media_path.parent().unwrap()).unwrap();
    std::fs::write(&media_path, b"not a directory").unwrap();

    let events = run_logged_media_turn(&agent, &session_id).await;
    let (event_output, is_error) = events
        .iter()
        .find_map(|e| match e {
            Event::ToolCallEnd {
                output, is_error, ..
            } => Some((output.clone(), *is_error)),
            _ => None,
        })
        .expect("media tool end");
    assert!(!is_error, "{event_output}");
    assert!(
        event_output
            .contains("1 image(s) could not be saved and were removed from this tool result"),
        "{event_output}"
    );

    let rollout = std::fs::read_to_string(
        data_dir
            .join("sessions")
            .join(format!("{session_id}.jsonl")),
    )
    .unwrap();
    let record = tool_call_record(&rollout, "ReadMediaFile");
    assert!(
        record["images"].as_array().is_none_or(Vec::is_empty),
        "the durable record carries no refs: {rollout}"
    );
    assert_eq!(
        record["output"].as_str(),
        Some(event_output.as_str()),
        "live and durable output are identical: {rollout}"
    );

    // The first follow-up step must not carry the image; it sees the same note
    // the rollout persists.
    let streaming: Vec<String> = log
        .lock()
        .unwrap()
        .iter()
        .filter(|body| body.contains("\"stream\":true"))
        .cloned()
        .collect();
    assert_eq!(streaming.len(), 2, "{streaming:#?}");
    assert!(
        !streaming[1].contains("image_url") && !streaming[1].contains("data:image"),
        "the live follow-up has no image payload: {}",
        streaming[1]
    );
    assert!(
        streaming[1].contains("could not be saved and were removed"),
        "the model sees the degradation note: {}",
        streaming[1]
    );

    // Restart: the rollout's degraded text is the only source; no refs and no
    // image payload reappear.
    agent.shutdown();
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), dir.clone(), data_dir.clone());
    let replay = {
        agent
            .ops
            .send(Op::OpenSession {
                session_id: session_id.clone(),
            })
            .await
            .unwrap();
        common::recv_until(&agent.events, std::time::Duration::from_secs(10), |e| {
            matches!(
                e,
                Event::TurnComplete {
                    duration_ms: 0,
                    stats: None,
                    ..
                }
            )
        })
        .await
    };
    assert!(
        replay.iter().any(|event| matches!(
            event,
            Event::ToolCallEnd { output, .. }
                if output.contains("could not be saved and were removed")
        )),
        "replay keeps the degradation text: {replay:#?}"
    );
    agent
        .ops
        .send(Op::SendMessage {
            session_id: session_id.clone(),
            content: "continue after failed image".into(),
            files: vec![],
            images: vec![],
            permissions: ExecMode::Auto.permissions(),
            delivery: pig_protocol::MessageDelivery::Queue,
        })
        .await
        .unwrap();
    common::recv_until(&agent.events, std::time::Duration::from_secs(20), |e| {
        matches!(e, Event::TurnComplete { .. } | Event::Error { .. })
    })
    .await;
    let bodies = log.lock().unwrap().clone();
    let restart = bodies
        .iter()
        .rev()
        .find(|body| body.contains("continue after failed image"))
        .expect("post-restart request");
    assert!(
        !restart.contains("image_url") && !restart.contains("data:image"),
        "no image reappears after restart: {restart}"
    );
    assert!(
        restart.contains("could not be saved and were removed"),
        "the restart request carries the durable note: {restart}"
    );
    agent.shutdown();
}

// ---------- Root tool PDF persistence failure ----------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tool_pdf_write_failure_degrades_live_and_rollout_identically() {
    let (config_path, dir, data_dir, log) = setup_pdf(
        "pdf-write-fail",
        pig_protocol::ApiFormat::OpenAiChat,
        false,
        true,
    );
    let agent = pig_core::spawn_agent_with_data_dir(
        Some(config_path.clone()),
        dir.clone(),
        data_dir.clone(),
    );
    let session_id = common::new_session(&agent, dir.clone()).await;
    // Force create_dir_all/write failure by occupying the media path with a
    // regular file (the same fault as the image regression).
    let media_path = data_dir
        .join("sessions")
        .join(format!("{session_id}.media"));
    std::fs::create_dir_all(media_path.parent().unwrap()).unwrap();
    std::fs::write(&media_path, b"not a directory").unwrap();

    let events = run_pdf_turn(&agent, &session_id).await;
    let (event_output, is_error) = events
        .iter()
        .find_map(|e| match e {
            Event::ToolCallEnd {
                output, is_error, ..
            } => Some((output.clone(), *is_error)),
            _ => None,
        })
        .expect("pdf tool end");
    assert!(!is_error, "{event_output}");
    assert!(event_output.contains("Read PDF"), "{event_output}");
    assert!(
        event_output.contains(
            "1 PDF document(s) could not be saved and were removed from this tool result"
        ),
        "{event_output}"
    );

    let rollout = std::fs::read_to_string(
        data_dir
            .join("sessions")
            .join(format!("{session_id}.jsonl")),
    )
    .unwrap();
    let record = tool_call_record(&rollout, "ReadMediaFile");
    assert!(
        record["documents"].as_array().is_none_or(Vec::is_empty),
        "the durable record carries no refs: {rollout}"
    );
    assert_eq!(
        record["output"].as_str(),
        Some(event_output.as_str()),
        "live and durable output are identical: {rollout}"
    );

    // The first follow-up step must not carry the document; it sees the same
    // note the rollout persists.
    let streaming: Vec<String> = log
        .lock()
        .unwrap()
        .iter()
        .filter(|body| body.contains("\"stream\":true"))
        .cloned()
        .collect();
    assert_eq!(streaming.len(), 2, "{streaming:#?}");
    assert!(
        !streaming[1].contains(pdf_wire_marker(pig_protocol::ApiFormat::OpenAiChat)),
        "the live follow-up has no document payload: {}",
        streaming[1]
    );
    assert!(
        streaming[1].contains("could not be saved and were removed"),
        "the model sees the degradation note: {}",
        streaming[1]
    );

    // Restart: the rollout's degraded text is the only source; no refs and no
    // document payload reappear.
    agent.shutdown();
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), dir.clone(), data_dir.clone());
    let replay = {
        agent
            .ops
            .send(Op::OpenSession {
                session_id: session_id.clone(),
            })
            .await
            .unwrap();
        common::recv_until(&agent.events, std::time::Duration::from_secs(10), |e| {
            matches!(
                e,
                Event::TurnComplete {
                    duration_ms: 0,
                    stats: None,
                    ..
                }
            )
        })
        .await
    };
    assert!(
        replay.iter().any(|event| matches!(
            event,
            Event::ToolCallEnd { output, .. }
                if output.contains("could not be saved and were removed")
        )),
        "replay keeps the degradation text: {replay:#?}"
    );
    agent
        .ops
        .send(Op::SendMessage {
            session_id: session_id.clone(),
            content: "continue after failed pdf".into(),
            files: vec![],
            images: vec![],
            permissions: ExecMode::Auto.permissions(),
            delivery: pig_protocol::MessageDelivery::Queue,
        })
        .await
        .unwrap();
    common::recv_until(&agent.events, std::time::Duration::from_secs(20), |e| {
        matches!(e, Event::TurnComplete { .. } | Event::Error { .. })
    })
    .await;
    let bodies = log.lock().unwrap().clone();
    let restart = bodies
        .iter()
        .rev()
        .find(|body| body.contains("continue after failed pdf"))
        .expect("post-restart request");
    assert!(
        !restart.contains(pdf_wire_marker(pig_protocol::ApiFormat::OpenAiChat)),
        "no document reappears after restart: {restart}"
    );
    assert!(
        restart.contains("could not be saved and were removed"),
        "the restart request carries the durable note: {restart}"
    );
    agent.shutdown();
}

/// Rehydrated images are validated, not merely read: corrupt bytes, a media
/// type that disagrees with the sniffed header, and undecodable dimensions all
/// degrade exactly like a missing file.
#[test]
fn rebuild_rejects_corrupt_or_mismatched_images() {
    let dir = temp_dir("validate-images");
    let media = dir.join("s1.media");
    std::fs::create_dir_all(&media).unwrap();
    std::fs::write(media.join("1.png"), b"not an image").unwrap();
    std::fs::write(media.join("2.png"), jpeg_bytes(8, 8)).unwrap();
    let mut signature_only = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
    signature_only.extend_from_slice(b"garbage");
    std::fs::write(media.join("3.png"), signature_only).unwrap();
    let image = |name: &str| pig_core::rollout::ImageRef {
        path: media.join(name),
        media_type: "image/png".into(),
        width: 8,
        height: 8,
        label: None,
    };
    let records = vec![pig_core::rollout::RolloutRecord::User {
        sent_at_ms: 0,
        text: "images".into(),
        files: vec![],
        images: vec![image("1.png"), image("2.png"), image("3.png")],
        sent_text: None,
        sent_images: vec![],
        model_text: None,
        sent_input_image: None,
    }];
    let (history, _) = pig_core::rollout::rebuild_history_with_media_root(
        &records,
        "sys".into(),
        None,
        &media,
        None,
    );
    let user = history.iter().find(|m| m.role == "user").expect("user");
    assert!(user.images.is_empty(), "{:?}", user.images);
    assert!(
        user.content
            .as_deref()
            .is_some_and(|text| text.contains("3 image(s) no longer available")),
        "{:?}",
        user.content
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A rollout ref cannot point the model at a file outside the owning session's
/// media root; in-root media still loads.
#[test]
fn rebuild_contains_media_paths_in_the_session_root() {
    let dir = temp_dir("media-root");
    let media = dir.join("s1.media");
    let outside = dir.join("outside");
    std::fs::create_dir_all(&media).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(media.join("1.png"), png_bytes(8, 8, false)).unwrap();
    std::fs::write(outside.join("2.png"), png_bytes(8, 8, false)).unwrap();
    let image = |path: PathBuf| pig_core::rollout::ImageRef {
        path,
        media_type: "image/png".into(),
        width: 8,
        height: 8,
        label: None,
    };
    let records = vec![pig_core::rollout::RolloutRecord::User {
        sent_at_ms: 0,
        text: "images".into(),
        files: vec![],
        images: vec![image(media.join("1.png")), image(outside.join("2.png"))],
        sent_text: None,
        sent_images: vec![],
        model_text: None,
        sent_input_image: None,
    }];
    let (history, _) = pig_core::rollout::rebuild_history_with_media_root(
        &records,
        "sys".into(),
        None,
        &media,
        None,
    );
    let user = history.iter().find(|m| m.role == "user").expect("user");
    assert_eq!(user.images.len(), 1, "only the contained image loads");
    assert!(
        user.content
            .as_deref()
            .is_some_and(|text| text.contains("1 image(s) no longer available")),
        "{:?}",
        user.content
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Media creation is collision-safe and never overwrites an existing numbered
/// file; persisted bytes are revalidated on the way in.
#[test]
fn media_writes_allocate_unique_names_and_validate_bytes() {
    let dir = temp_dir("atomic-media");
    let image = pig_provider::ChatImage {
        media_type: "image/png".into(),
        data_base64: tool::base64_encode(&png_bytes(8, 8, false)),
        label: Some("Image 1".into()),
    };
    let first = pig_core::rollout::persist_chat_images(&dir, std::slice::from_ref(&image))
        .expect("first persist");
    let second = pig_core::rollout::persist_chat_images(&dir, std::slice::from_ref(&image))
        .expect("second persist");
    assert_ne!(first[0].path, second[0].path);
    assert!(first[0].path.is_file() && second[0].path.is_file());
    assert_eq!(first[0].width, 8);
    assert_eq!(first[0].height, 8);
    let mismatch = pig_provider::ChatImage {
        media_type: "image/png".into(),
        data_base64: tool::base64_encode(&jpeg_bytes(8, 8)),
        label: None,
    };
    let mismatch_ref =
        pig_core::rollout::persist_chat_images(&dir, &[mismatch]).expect("persisted as recorded");
    assert!(
        pig_core::rollout::rehydrate_image(&mismatch_ref[0], &dir).is_none(),
        "a media type that disagrees with the bytes degrades on session rehydration"
    );
    assert!(
        pig_core::rollout::rehydrate_image(&first[0], &dir).is_some(),
        "the valid image still rehydrates"
    );
    let mut invalid_pdf = pig_provider::ChatDocument {
        media_type: "application/pdf".into(),
        data_base64: tool::base64_encode(b"not a pdf"),
        filename: "x.pdf".into(),
        label: None,
        token_estimate: None,
    };
    assert!(
        pig_core::rollout::persist_chat_documents(
            &dir,
            std::slice::from_mut(&mut invalid_pdf),
            128_000
        )
        .is_err(),
        "corrupt PDF bytes are refused instead of persisted"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// PDF document refs obey the same owning-root containment as images.
#[test]
fn document_paths_stay_in_the_session_media_root() {
    let dir = temp_dir("document-root");
    let media = dir.join("s1.media");
    let outside = dir.join("outside");
    std::fs::create_dir_all(&media).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(media.join("1.pdf"), pdf_bytes()).unwrap();
    std::fs::write(outside.join("2.pdf"), pdf_bytes()).unwrap();
    let document = |path: PathBuf| pig_core::rollout::DocumentRef {
        path,
        media_type: "application/pdf".into(),
        bytes: 0,
        token_estimate: None,
        label: Some("doc.pdf".into()),
    };
    let records = vec![pig_core::rollout::RolloutRecord::ToolCall {
        execution_id: "execution-document".into(),
        tool: "ReadMediaFile".into(),
        summary: "doc".into(),
        arguments: "{}".into(),
        output: "doc".into(),
        is_error: false,
        stopped: false,
        edit: None,
        agent_card: None,
        agent_cards: vec![],
        images: vec![],
        documents: vec![
            document(media.join("1.pdf")),
            document(outside.join("2.pdf")),
        ],
        delivered_notification_ids: vec![],
        recovery: None,
    }];
    let (history, _) = pig_core::rollout::rebuild_history_with_media_root(
        &records,
        "sys".into(),
        Some(tool::MediaCapabilities::new(false, true)),
        &media,
        Some(128_000),
    );
    let result = history
        .iter()
        .find(|message| message.role == "tool")
        .expect("tool result");
    assert_eq!(result.documents.len(), 1, "only the contained PDF loads");
    assert_eq!(
        result.documents[0].token_estimate,
        Some(pig_provider::DocumentTokenEstimate::measured(
            pig_utils::pdf::PDF_TOKENS_PER_PAGE
        )),
        "an old ref is priced once from the visible page tree at load"
    );
    assert!(
        result
            .content
            .as_deref()
            .is_some_and(|text| text.contains("1 PDF document(s) no longer available")),
        "{:?}",
        result.content
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The PDF cost is computed exactly once at persistence: the live message and
/// the durable reference share it, and rehydration trusts a measured cost
/// instead of decoding/scanning the payload again. An opaque page tree stores
/// a window fallback tagged with its source window, so a different consuming
/// window reprices it without touching the bytes.
#[test]
fn document_persistence_caches_the_token_cost_and_rehydration_reuses_it() {
    let dir = temp_dir("document-tokens");
    let two_pages: &[u8] = b"%PDF-1.4\n\
        1 0 obj << /Type /Pages /Count 2 >> endobj\n\
        2 0 obj << /Type /Page >> endobj\n\
        3 0 obj << /Type /Page >> endobj\n%%EOF\n";
    let mut paged = pig_provider::ChatDocument {
        media_type: "application/pdf".into(),
        data_base64: tool::base64_encode(two_pages),
        filename: "paged.pdf".into(),
        label: None,
        token_estimate: None,
    };
    let refs =
        pig_core::rollout::persist_chat_documents(&dir, std::slice::from_mut(&mut paged), 128_000)
            .expect("persist paged PDF");
    let measured =
        pig_provider::DocumentTokenEstimate::measured(2 * pig_utils::pdf::PDF_TOKENS_PER_PAGE);
    assert_eq!(paged.token_estimate, Some(measured));
    assert_eq!(refs[0].token_estimate, Some(measured));

    // A measured persisted cost wins over a rescan, even when the bytes would
    // price differently, and a different window never changes it.
    let mut cached_ref = refs[0].clone();
    cached_ref.token_estimate = Some(pig_provider::DocumentTokenEstimate::measured(1_234));
    let rehydrated = pig_core::rollout::rehydrate_documents(&[cached_ref], &dir, Some(8_000));
    assert_eq!(
        rehydrated[0].token_estimate,
        Some(pig_provider::DocumentTokenEstimate::measured(1_234)),
        "a measured cost is window-independent"
    );

    let mut opaque = pig_provider::ChatDocument {
        media_type: "application/pdf".into(),
        data_base64: tool::base64_encode(b"%PDF-1.4\n%%EOF\n"),
        filename: "opaque.pdf".into(),
        label: None,
        token_estimate: None,
    };
    let refs =
        pig_core::rollout::persist_chat_documents(&dir, std::slice::from_mut(&mut opaque), 128_000)
            .expect("persist opaque PDF");
    assert_eq!(
        opaque.token_estimate,
        Some(pig_provider::DocumentTokenEstimate::window_fallback(
            pig_utils::pdf::unknown_document_token_reserve(128_000),
            128_000
        ))
    );
    assert_eq!(refs[0].token_estimate, opaque.token_estimate);

    // Rehydration for a different model window reprices the fallback, keeping
    // the measured/fallback distinction instead of carrying the stale reserve.
    let rehydrated = pig_core::rollout::rehydrate_documents(&refs, &dir, Some(8_000));
    assert_eq!(
        rehydrated[0].token_estimate,
        Some(pig_provider::DocumentTokenEstimate::window_fallback(
            pig_utils::pdf::unknown_document_token_reserve(8_000),
            8_000
        ))
    );
    let _ = std::fs::remove_dir_all(&dir);
}
