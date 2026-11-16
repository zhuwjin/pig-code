use super::*;

mod compact;
mod identity;
mod media;
mod store;

fn write_meta(path: &Path) {
    append_agent_conversation_record(
        path,
        &crate::rollout::RolloutRecord::subagent_meta(
            "a1".into(),
            "s-test".into(),
            PathBuf::from("/tmp"),
            "roundtrip".into(),
            "general-purpose".into(),
            "p".into(),
            "m".into(),
            1,
            1,
        ),
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

fn append_test_tool_result(
    path: &Path,
    call: &pig_provider::ToolCall,
    msg: &mut ChatMsg,
) -> Result<(), String> {
    append_agent_tool_result(
        path,
        call,
        &format!("execution-{}", call.id),
        msg,
        TEST_CONTEXT_WINDOW,
        AgentMsgMeta::default(),
    )
}

#[allow(clippy::too_many_arguments)]
fn append_test_compact(
    path: &Path,
    note: impl Into<String>,
    omitted: usize,
    automatic: bool,
    summary: Option<&str>,
    tail_len: Option<usize>,
    preserved_user: Option<usize>,
    trigger: Option<&str>,
    used_before: Option<u64>,
    used_after: Option<u64>,
) -> Result<(), String> {
    append_agent_conversation_record(
        path,
        &crate::rollout::RolloutRecord::Compact {
            note: note.into(),
            omitted,
            automatic,
            summary: summary.map(str::to_string),
            tail_len,
            preserved_user,
            trigger: trigger.map(str::to_string),
            used_before,
            used_after,
        },
    )
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
    let path = dir.join(format!("{file_id}.jsonl"));
    append_agent_conversation_record(
        &path,
        &crate::rollout::RolloutRecord::subagent_meta(
            meta_id.into(),
            "s-test".into(),
            PathBuf::from("/tmp"),
            "identity fixture".into(),
            "general-purpose".into(),
            "p".into(),
            "m".into(),
            1,
            1,
        ),
    )
    .expect("meta");
    let mut user = ChatMsg::user("hello".into());
    append_agent_message(&path, &mut user, TEST_CONTEXT_WINDOW).expect("user");
}
