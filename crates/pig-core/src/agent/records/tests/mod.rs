use super::*;

mod compact;
mod identity;
mod media;
mod store;

fn write_meta(path: &Path) {
    append_agent_record(
        path,
        &serde_json::json!({
            "type": "meta",
            "agent_id": "a1",
            "profile": "general-purpose",
            "description": "roundtrip",
            "model": "m",
            "provider": "p",
            "created_at": 1,
        }),
    )
    .expect("write meta");
}

/// A real minimal PNG so the rooted media validation accepts it.
fn png_bytes() -> Vec<u8> {
    let image = image::DynamicImage::new_rgb8(1, 1);
    let mut buf = std::io::Cursor::new(Vec::new());
    image
        .write_to(&mut buf, image::ImageFormat::Png)
        .expect("encode png");
    buf.into_inner()
}

fn image_ref(path: &Path, label: &str) -> serde_json::Value {
    serde_json::json!({
        "path": path,
        "media_type": "image/png",
        "width": 1,
        "height": 1,
        "label": label,
    })
}

fn pdf_document(label: &str) -> pig_provider::ChatDocument {
    pig_provider::ChatDocument {
        media_type: "application/pdf".into(),
        data_base64: pig_utils::image::base64_encode(b"%PDF-1.4\n%%EOF\n"),
        filename: label.into(),
        label: Some(label.into()),
        token_estimate: None,
    }
}

/// Unit tests use a representative window; the persisted token cost is still
/// computed by the production writer.
const TEST_CONTEXT_WINDOW: u64 = 128_000;

fn append_test_message(path: &Path, msg: &mut ChatMsg) -> Result<(), String> {
    append_agent_message(path, msg, TEST_CONTEXT_WINDOW)
}

fn identity_temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pig-agent-identity-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("identity temp dir");
    dir
}

fn seed_context(dir: &Path, file_id: &str, meta_id: &str) {
    std::fs::create_dir_all(dir).expect("agents dir");
    append_agent_record(
        &dir.join(format!("{file_id}.jsonl")),
        &serde_json::json!({
            "type": "meta",
            "agent_id": meta_id,
            "profile": "general-purpose",
            "description": "identity fixture",
            "model": "m",
            "provider": "p",
            "created_at": 1,
        }),
    )
    .expect("meta");
    append_agent_record(
        &dir.join(format!("{file_id}.jsonl")),
        &serde_json::json!({
            "type": "msg",
            "msg": ChatMsg::user("hello".into()),
        }),
    )
    .expect("user");
}
