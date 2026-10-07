//! Text pipeline and file tool boundary-guard tests: text module
//! encode/decode, encoding/line-ending preservation in Read/Write/Edit,
//! sensitive-file and symlink guards, byte-level ChangeTracker snapshots.

use pig_core::provider::ToolCall;
use pig_core::task::SessionToolState;
use pig_core::text::{self, FileEncoding, LineEnding};
use pig_core::tool::{self, ChangeTracker, ToolContext};

fn call(name: &str, args: serde_json::Value) -> ToolCall {
    ToolCall {
        id: "t1".into(),
        name: name.into(),
        arguments: args.to_string(),
    }
}

fn temp_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("pig-core-text-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.canonicalize().unwrap()
}

fn gbk_bytes(text_lf: &str) -> Vec<u8> {
    text::encode(text_lf, FileEncoding::Gbk, false, LineEnding::Lf).unwrap()
}

// ---------- text module ----------

#[test]
fn decode_utf8_bom() {
    let doc = text::decode(b"\xEF\xBB\xBFhello\n").unwrap();
    assert_eq!(doc.encoding, FileEncoding::Utf8);
    assert!(doc.bom);
    assert_eq!(doc.text, "hello\n");
    assert_eq!(doc.line_ending, LineEnding::Lf);
    assert!(!doc.lossy);
}

#[test]
fn decode_utf16le_with_bom() {
    let bytes = text::encode("hi\nthere\n", FileEncoding::Utf16Le, true, LineEnding::Crlf).unwrap();
    assert_eq!(&bytes[..2], &[0xFF, 0xFE]);
    let doc = text::decode(&bytes).unwrap();
    assert_eq!(doc.encoding, FileEncoding::Utf16Le);
    assert!(doc.bom);
    // 2 CRLFs, no lone LF → CRLF dominant; the view normalizes to LF
    assert_eq!(doc.text, "hi\nthere\n");
    assert_eq!(doc.line_ending, LineEnding::Crlf);
}

#[test]
fn decode_utf16le_without_bom_heuristic() {
    let bytes = text::encode(
        "hello world, this is plain ascii\n",
        FileEncoding::Utf16Le,
        false,
        LineEnding::Lf,
    )
    .unwrap();
    assert!(!bytes.starts_with(&[0xFF, 0xFE]));
    let doc = text::decode(&bytes).unwrap();
    assert_eq!(
        doc.encoding,
        FileEncoding::Utf16Le,
        "should be detected by the heuristic"
    );
    assert!(!doc.bom);
    assert_eq!(doc.text, "hello world, this is plain ascii\n");

    let bytes = text::encode(
        "hello world, this is plain ascii\n",
        FileEncoding::Utf16Be,
        false,
        LineEnding::Lf,
    )
    .unwrap();
    let doc = text::decode(&bytes).unwrap();
    assert_eq!(doc.encoding, FileEncoding::Utf16Be);
    assert_eq!(doc.text, "hello world, this is plain ascii\n");
}

#[test]
fn decode_binary_nul_rejected() {
    let err = text::decode(b"ab\x00cd").unwrap_err();
    assert!(err.contains("Binary file"), "{err}");
}

#[test]
fn decode_control_chars_rejected() {
    // Fill cyclically with 0x01-0x08: 100% control characters, and no NUL
    let bytes: Vec<u8> = (0..200).map(|i| 0x01 + (i % 8) as u8).collect();
    let err = text::decode(&bytes).unwrap_err();
    assert!(err.contains("Binary file"), "{err}");

    // A few control chars mixed into normal text must not misclassify (ratio far below 30%)
    let mut text_bytes = b"normal text line\n".to_vec();
    text_bytes.push(0x01);
    let doc = text::decode(&text_bytes).unwrap();
    assert_eq!(doc.encoding, FileEncoding::Utf8);
}

#[test]
fn decode_gbk_roundtrip_accept_and_reject() {
    let bytes = gbk_bytes("中文测试，编码识别\n");
    assert!(
        std::str::from_utf8(&bytes).is_err(),
        "GBK bytes are not valid UTF-8"
    );
    let doc = text::decode(&bytes).unwrap();
    assert_eq!(doc.encoding, FileEncoding::Gbk);
    assert_eq!(doc.text, "中文测试，编码识别\n");

    // Invalid UTF-8 and no GBK round-trip restoration (lone lead byte → replacement char)
    let err = text::decode(&[0x81, 0x82, 0x83]).unwrap_err();
    assert!(err.contains("Unrecognized text encoding"), "{err}");
}

#[test]
fn decode_crlf_dominant_and_mixed() {
    let doc = text::decode(b"a\r\nb\r\nc\n").unwrap();
    assert_eq!(doc.line_ending, LineEnding::Crlf);
    assert_eq!(doc.text, "a\nb\nc\n", "view is always normalized to LF");

    // More lone \n → LF dominant; leftover \r\n is also normalized (lone \r preserved)
    let doc = text::decode(b"a\nb\nc\r\nd\re\n").unwrap();
    assert_eq!(doc.line_ending, LineEnding::Lf);
    assert_eq!(doc.text, "a\nb\nc\nd\re\n");
}

#[test]
fn encode_restores_crlf_and_defensive_normalize() {
    assert_eq!(
        text::encode("a\nb\n", FileEncoding::Utf8, false, LineEnding::Crlf).unwrap(),
        b"a\r\nb\r\n"
    );
    // Input already carrying \r\n is normalized first to avoid \r\r\n
    assert_eq!(
        text::encode("a\r\nb\r\n", FileEncoding::Utf8, false, LineEnding::Crlf).unwrap(),
        b"a\r\nb\r\n"
    );
    assert_eq!(
        text::encode("a\r\nb", FileEncoding::Utf8, false, LineEnding::Lf).unwrap(),
        b"a\nb"
    );
}

#[test]
fn encode_utf16_roundtrip_with_bom() {
    let bytes = text::encode("你好\n", FileEncoding::Utf16Le, true, LineEnding::Lf).unwrap();
    assert_eq!(&bytes[..2], &[0xFF, 0xFE]);
    let doc = text::decode(&bytes).unwrap();
    assert_eq!(doc.text, "你好\n");
    assert_eq!(doc.encoding, FileEncoding::Utf16Le);
}

#[test]
fn encode_gbk_rejects_unencodable() {
    let err = text::encode(
        "emoji 🎉 写不进 GBK",
        FileEncoding::Gbk,
        false,
        LineEnding::Lf,
    )
    .unwrap_err();
    assert!(err.contains("GBK cannot encode"), "{err}");
}

#[test]
fn decode_utf16_odd_trailing_byte_is_lossy() {
    let mut bytes = text::encode("hi\n", FileEncoding::Utf16Le, true, LineEnding::Lf).unwrap();
    bytes.push(0x61); // odd trailing byte
    let doc = text::decode(&bytes).unwrap();
    assert_eq!(doc.text, "hi\n");
    assert!(doc.lossy, "dropping the odd trailing byte should set lossy");
}

// ---------- Read ----------

/// Execute with shared tracker/state (the freshness state registered by Read must be shared across Read→Write/Edit)
async fn run_tool_in(
    dir: &std::path::Path,
    tracker: &mut ChangeTracker,
    state: &SessionToolState,
    name: &str,
    args: serde_json::Value,
) -> (
    String,
    bool,
    Option<tool::FileChange>,
    Option<pig_protocol::EditDiff>,
    Vec<tool::ToolImage>,
) {
    tool::execute(
        &call(name, args),
        ToolContext {
            cwd: dir,
            tracker,
            state,
        },
    )
    .await
}

async fn run_tool(
    dir: &std::path::Path,
    name: &str,
    args: serde_json::Value,
) -> (
    String,
    bool,
    Option<tool::FileChange>,
    Option<pig_protocol::EditDiff>,
    Vec<tool::ToolImage>,
) {
    let mut tracker = ChangeTracker::default();
    let state = SessionToolState::for_test();
    run_tool_in(dir, &mut tracker, &state, name, args).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_has_line_numbers() {
    let dir = temp_dir("read-lines");
    std::fs::write(dir.join("a.txt"), "alpha\nbeta\ngamma\n").unwrap();

    let (out, is_error, _, _, _) =
        run_tool(&dir, "Read", serde_json::json!({"path": "a.txt"})).await;
    assert!(!is_error, "{out}");
    assert!(out.contains("1\talpha"), "{out}");
    assert!(out.contains("2\tbeta"), "{out}");
    assert!(out.contains("3\tgamma"), "{out}");

    // Line numbers count from the offset
    let (out, is_error, _, _, _) = run_tool(
        &dir,
        "Read",
        serde_json::json!({"path": "a.txt", "offset": 2}),
    )
    .await;
    assert!(!is_error, "{out}");
    assert!(out.starts_with("2\tbeta"), "{out}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_crlf_normalized_with_file_info() {
    let dir = temp_dir("read-crlf");
    std::fs::write(dir.join("win.txt"), b"a\r\nb\r\nc\r\n").unwrap();

    let (out, is_error, _, _, _) =
        run_tool(&dir, "Read", serde_json::json!({"path": "win.txt"})).await;
    assert!(!is_error, "{out}");
    assert!(!out.contains('\r'), "output should not contain CR: {out:?}");
    assert!(out.contains("1\ta"), "{out}");
    assert!(
        out.contains("[File info:") && out.contains("line endings=CRLF"),
        "should have a file info line: {out}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_utf16le_and_gbk() {
    let dir = temp_dir("read-enc");
    let utf16 = text::encode("你好\n世界\n", FileEncoding::Utf16Le, true, LineEnding::Lf).unwrap();
    std::fs::write(dir.join("u16.txt"), &utf16).unwrap();
    std::fs::write(dir.join("g.txt"), gbk_bytes("中文内容\n第二行\n")).unwrap();

    let (out, is_error, _, _, _) =
        run_tool(&dir, "Read", serde_json::json!({"path": "u16.txt"})).await;
    assert!(!is_error, "{out}");
    assert!(out.contains("1\t你好"), "{out}");
    assert!(out.contains("encoding=UTF-16LE"), "{out}");

    let (out, is_error, _, _, _) =
        run_tool(&dir, "Read", serde_json::json!({"path": "g.txt"})).await;
    assert!(!is_error, "{out}");
    assert!(out.contains("中文内容"), "{out}");
    assert!(out.contains("encoding=GBK"), "{out}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_binary_rejected() {
    let dir = temp_dir("read-bin");
    std::fs::write(dir.join("bin.dat"), b"PK\x03\x04\x00\x01binary").unwrap();

    let (out, is_error, _, _, _) =
        run_tool(&dir, "Read", serde_json::json!({"path": "bin.dat"})).await;
    assert!(is_error, "{out}");
    assert!(out.contains("Binary file"), "{out}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_long_line_truncated() {
    let dir = temp_dir("read-long-line");
    let long = "x".repeat(3000);
    std::fs::write(dir.join("long.txt"), format!("{long}\nshort\n")).unwrap();

    let (out, is_error, _, _, _) =
        run_tool(&dir, "Read", serde_json::json!({"path": "long.txt"})).await;
    assert!(!is_error, "{out}");
    assert!(
        out.contains("continue with column_offset=2000"),
        "truncation marker should carry continuation parameters: {}",
        &out[..out.len().min(300)]
    );
    assert!(out.contains("of 3000"), "{out}");
    assert!(out.contains("2\tshort"), "{out}");

    // Continuation: 1000 chars visible from 2000, fits the budget → no continuation marker, carries the range note
    let (out, _, _, _, _) = run_tool(
        &dir,
        "Read",
        serde_json::json!({"path": "long.txt", "column_offset": 2000}),
    )
    .await;
    assert!(
        !out.contains("continue with"),
        "the last segment should not carry a continuation marker: {out}"
    );
    assert!(out.contains("chars 2001-3000 of 3000"), "{out}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_char_budget_truncates() {
    let dir = temp_dir("read-budget");
    // 3000 lines × 50 chars ≈ 150k chars, over the 100k budget (the 2000-line cap would not hit first either)
    let content: String = (1..=3000)
        .map(|i| format!("line {i:04} {}\n", "y".repeat(40)))
        .collect();
    std::fs::write(dir.join("big.txt"), content).unwrap();

    let (out, is_error, _, _, _) =
        run_tool(&dir, "Read", serde_json::json!({"path": "big.txt"})).await;
    assert!(!is_error, "{out}");
    assert!(
        out.contains("[Truncated: showing lines 1-"),
        "should have a truncation notice: {out}"
    );
    assert!(out.contains("of 3000"), "{out}");
    assert!(
        out.len() < 120_000,
        "output should respect the character budget: {}",
        out.len()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_empty_and_missing_file() {
    let dir = temp_dir("read-empty");
    std::fs::write(dir.join("empty.txt"), "").unwrap();
    std::fs::write(dir.join("a.rs"), "fn a() {}\n").unwrap();
    std::fs::write(dir.join("b.rs"), "fn b() {}\n").unwrap();

    let (out, is_error, _, _, _) =
        run_tool(&dir, "Read", serde_json::json!({"path": "empty.txt"})).await;
    assert!(!is_error, "{out}");
    assert_eq!(out, "(empty file)");

    let (out, is_error, _, _, _) =
        run_tool(&dir, "Read", serde_json::json!({"path": "missing.rs"})).await;
    assert!(is_error, "{out}");
    assert!(out.contains("File not found: missing.rs"), "{out}");
    assert!(
        out.contains("a.rs") && out.contains("b.rs"),
        "should list sibling files: {out}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_env_rejected_but_template_allowed() {
    let dir = temp_dir("read-env");
    std::fs::write(dir.join(".env"), "SECRET=1\n").unwrap();
    std::fs::write(dir.join(".env.local"), "SECRET=2\n").unwrap();
    std::fs::write(dir.join(".env.example"), "SECRET=\n").unwrap();

    for path in [".env", ".env.local"] {
        let (out, is_error, _, _, _) =
            run_tool(&dir, "Read", serde_json::json!({"path": path})).await;
        assert!(is_error, "{path} should be rejected: {out}");
        assert!(out.contains("sensitive file"), "{out}");
        assert!(!out.contains("SECRET=1"), "content should not leak: {out}");
    }
    let (out, is_error, _, _, _) =
        run_tool(&dir, "Read", serde_json::json!({"path": ".env.example"})).await;
    assert!(!is_error, "template file should be readable: {out}");
}

// ---------- Write ----------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn write_preserves_crlf() {
    let dir = temp_dir("write-crlf");
    std::fs::write(dir.join("win.txt"), b"a\r\nb\r\n").unwrap();

    let mut tracker = ChangeTracker::default();
    let state = SessionToolState::for_test();
    // Pre-write freshness: an existing file must be Read first
    let (_, is_error, _, _, _) = run_tool_in(
        &dir,
        &mut tracker,
        &state,
        "Read",
        serde_json::json!({"path": "win.txt"}),
    )
    .await;
    assert!(!is_error);

    let (out, is_error, _, _, _) = run_tool_in(
        &dir,
        &mut tracker,
        &state,
        "Write",
        serde_json::json!({"path": "win.txt", "content": "x\ny\n"}),
    )
    .await;
    assert!(!is_error, "{out}");
    assert!(
        out.contains("CRLF"),
        "output should mention the preserved CRLF: {out}"
    );
    assert_eq!(
        std::fs::read(dir.join("win.txt")).unwrap(),
        b"x\r\ny\r\n",
        "write-back should keep CRLF"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn write_preserves_gbk() {
    let dir = temp_dir("write-gbk");
    std::fs::write(dir.join("g.txt"), gbk_bytes("旧中文\n")).unwrap();

    let mut tracker = ChangeTracker::default();
    let state = SessionToolState::for_test();
    let (_, is_error, _, _, _) = run_tool_in(
        &dir,
        &mut tracker,
        &state,
        "Read",
        serde_json::json!({"path": "g.txt"}),
    )
    .await;
    assert!(!is_error);

    let (out, is_error, _, _, _) = run_tool_in(
        &dir,
        &mut tracker,
        &state,
        "Write",
        serde_json::json!({"path": "g.txt", "content": "新的中文\n"}),
    )
    .await;
    assert!(!is_error, "{out}");
    assert!(out.contains("GBK"), "{out}");
    assert_eq!(
        std::fs::read(dir.join("g.txt")).unwrap(),
        gbk_bytes("新的中文\n"),
        "write-back should be GBK bytes"
    );

    // Writing an unencodable character into a GBK file → rejected, file untouched
    let before = std::fs::read(dir.join("g.txt")).unwrap();
    let (out, is_error, _, _, _) = run_tool_in(
        &dir,
        &mut tracker,
        &state,
        "Write",
        serde_json::json!({"path": "g.txt", "content": "带 emoji 🎉\n"}),
    )
    .await;
    assert!(is_error, "{out}");
    assert!(out.contains("GBK"), "{out}");
    assert_eq!(
        std::fs::read(dir.join("g.txt")).unwrap(),
        before,
        "file untouched when the write is rejected"
    );
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn write_symlink_escape_rejected() {
    let dir = temp_dir("write-symlink");
    let outside = temp_dir("write-symlink-outside");
    std::fs::write(outside.join("secret.txt"), "top secret\n").unwrap();
    std::os::unix::fs::symlink(outside.join("secret.txt"), dir.join("link.txt")).unwrap();

    let (out, is_error, _, _, _) = run_tool(
        &dir,
        "Write",
        serde_json::json!({"path": "link.txt", "content": "pwned\n"}),
    )
    .await;
    assert!(is_error, "{out}");
    assert!(out.contains("Path escapes the working directory"), "{out}");
    assert_eq!(
        std::fs::read_to_string(outside.join("secret.txt")).unwrap(),
        "top secret\n",
        "symlink target should not be rewritten"
    );

    let (out, is_error, _, _, _) =
        run_tool(&dir, "Read", serde_json::json!({"path": "link.txt"})).await;
    assert!(is_error, "{out}");
    assert!(out.contains("Path escapes the working directory"), "{out}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn write_env_rejected() {
    let dir = temp_dir("write-env");
    let (out, is_error, _, _, _) = run_tool(
        &dir,
        "Write",
        serde_json::json!({"path": ".env", "content": "SECRET=1\n"}),
    )
    .await;
    assert!(is_error, "{out}");
    assert!(out.contains("sensitive file"), "{out}");
    assert!(
        !dir.join(".env").exists(),
        "nothing should be written after rejection"
    );
}

// ---------- Edit ----------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn edit_crlf_preserved_and_diff_clean() {
    let dir = temp_dir("edit-crlf");
    std::fs::write(dir.join("win.txt"), b"a\r\nb\r\nc\r\n").unwrap();

    let mut tracker = ChangeTracker::default();
    let state = SessionToolState::for_test();
    // Pre-write freshness: Read before Edit
    let (_, is_error, _, _, _) = run_tool_in(
        &dir,
        &mut tracker,
        &state,
        "Read",
        serde_json::json!({"path": "win.txt"}),
    )
    .await;
    assert!(!is_error);

    let (out, is_error, _, edit, _) = run_tool_in(
        &dir,
        &mut tracker,
        &state,
        "Edit",
        serde_json::json!({"path": "win.txt", "old_string": "b", "new_string": "B"}),
    )
    .await;
    assert!(!is_error, "{out}");
    assert_eq!(
        std::fs::read(dir.join("win.txt")).unwrap(),
        b"a\r\nB\r\nc\r\n",
        "disk content stays CRLF after the edit"
    );
    let edit = edit.expect("Edit should carry the per-edit diff");
    assert_eq!(
        (edit.additions, edit.deletions),
        (1, 1),
        "diff reflects only this replacement"
    );
    assert!(
        !edit.unified_diff.contains('\r'),
        "diff should be clean on the LF view: {}",
        edit.unified_diff
    );
    assert!(edit.unified_diff.contains("-b") && edit.unified_diff.contains("+B"));

    // When an old_string with \r\n fails to match in a CRLF file, the error should advise using LF
    let (out, is_error, _, _, _) = run_tool_in(
        &dir,
        &mut tracker,
        &state,
        "Edit",
        serde_json::json!({"path": "win.txt", "old_string": "a\r\nB", "new_string": "x"}),
    )
    .await;
    assert!(is_error, "{out}");
    assert!(
        out.contains("CRLF line endings"),
        "should advise using LF: {out}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn edit_replace_all_counts() {
    let dir = temp_dir("edit-all");
    std::fs::write(dir.join("f.txt"), "foo\nfoo\nfoo\n").unwrap();

    let mut tracker = ChangeTracker::default();
    let state = SessionToolState::for_test();
    let (_, is_error, _, _, _) = run_tool_in(
        &dir,
        &mut tracker,
        &state,
        "Read",
        serde_json::json!({"path": "f.txt"}),
    )
    .await;
    assert!(!is_error);

    // Without replace_all: multiple matches error out, keeping "appears N times" and pointing to replace_all
    let (out, is_error, _, _, _) = run_tool_in(
        &dir,
        &mut tracker,
        &state,
        "Edit",
        serde_json::json!({"path": "f.txt", "old_string": "foo", "new_string": "bar"}),
    )
    .await;
    assert!(is_error, "{out}");
    assert!(out.contains("appears 3 times"), "{out}");
    assert!(out.contains("replace_all=true"), "{out}");

    let (out, is_error, _, _, _) = run_tool_in(
        &dir,
        &mut tracker,
        &state,
        "Edit",
        serde_json::json!({"path": "f.txt", "old_string": "foo", "new_string": "bar", "replace_all": true}),
    )
    .await;
    assert!(!is_error, "{out}");
    assert!(out.contains("replaced 3 occurrences"), "{out}");
    assert_eq!(
        std::fs::read_to_string(dir.join("f.txt")).unwrap(),
        "bar\nbar\nbar\n"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn edit_old_equals_new_rejected() {
    let dir = temp_dir("edit-same");
    std::fs::write(dir.join("f.txt"), "same\n").unwrap();

    let (out, is_error, _, _, _) = run_tool(
        &dir,
        "Edit",
        serde_json::json!({"path": "f.txt", "old_string": "same", "new_string": "same"}),
    )
    .await;
    assert!(is_error, "{out}");
    assert!(out.contains("identical"), "{out}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn edit_empty_new_swallows_trailing_newline() {
    let dir = temp_dir("edit-delete-line");
    std::fs::write(dir.join("f.txt"), "a\nb\nc\n").unwrap();
    std::fs::write(dir.join("g.txt"), "x\ny").unwrap();
    std::fs::write(dir.join("h.txt"), "keep\nDEL\nkeep\nDEL\n").unwrap();

    let mut tracker = ChangeTracker::default();
    let state = SessionToolState::for_test();
    for file in ["f.txt", "g.txt", "h.txt"] {
        let (_, is_error, _, _, _) = run_tool_in(
            &dir,
            &mut tracker,
            &state,
            "Read",
            serde_json::json!({"path": file}),
        )
        .await;
        assert!(!is_error);
    }

    // old_string spanning a whole line (without \n) → the trailing \n is deleted too, no empty line left
    let (out, is_error, _, _, _) = run_tool_in(
        &dir,
        &mut tracker,
        &state,
        "Edit",
        serde_json::json!({"path": "f.txt", "old_string": "b", "new_string": ""}),
    )
    .await;
    assert!(!is_error, "{out}");
    assert_eq!(
        std::fs::read_to_string(dir.join("f.txt")).unwrap(),
        "a\nc\n"
    );

    // When the file end has no \n to swallow, splice as-is
    let (_, is_error, _, _, _) = run_tool_in(
        &dir,
        &mut tracker,
        &state,
        "Edit",
        serde_json::json!({"path": "g.txt", "old_string": "y", "new_string": ""}),
    )
    .await;
    assert!(!is_error);
    assert_eq!(std::fs::read_to_string(dir.join("g.txt")).unwrap(), "x\n");

    // replace_all swallows the newline at each occurrence too
    let (out, is_error, _, _, _) = run_tool_in(
        &dir,
        &mut tracker,
        &state,
        "Edit",
        serde_json::json!({"path": "h.txt", "old_string": "DEL", "new_string": "", "replace_all": true}),
    )
    .await;
    assert!(!is_error, "{out}");
    assert!(out.contains("replaced 2 occurrences"), "{out}");
    assert_eq!(
        std::fs::read_to_string(dir.join("h.txt")).unwrap(),
        "keep\nkeep\n"
    );
}

// ---------- ChangeTracker ----------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn revert_gbk_file_is_byte_exact() {
    let dir = temp_dir("revert-gbk");
    // Fixture with GBK encoding + CRLF line endings
    let original = text::encode(
        "中文标题\n正文\n",
        FileEncoding::Gbk,
        false,
        LineEnding::Crlf,
    )
    .unwrap();
    let file = dir.join("g.txt");
    std::fs::write(&file, &original).unwrap();

    let mut tracker = ChangeTracker::default();
    let state = SessionToolState::for_test();
    // Freshness: Read before Edit
    let (_, is_error, _, _, _) = tool::execute(
        &call("Read", serde_json::json!({"path": "g.txt"})),
        ToolContext {
            cwd: &dir,
            tracker: &mut tracker,
            state: &state,
        },
    )
    .await;
    assert!(!is_error);
    let (_, is_error, _, _, _) = tool::execute(
        &call(
            "Edit",
            serde_json::json!({"path": "g.txt", "old_string": "正文", "new_string": "改过的正文"}),
        ),
        ToolContext {
            cwd: &dir,
            tracker: &mut tracker,
            state: &state,
        },
    )
    .await;
    assert!(!is_error);
    assert_ne!(
        std::fs::read(&file).unwrap(),
        original,
        "the edit should change the bytes"
    );

    tracker.revert(&file).unwrap();
    assert_eq!(
        std::fs::read(&file).unwrap(),
        original,
        "revert of a GBK/CRLF file should be byte-exact"
    );
}

#[test]
fn snapshot_store_codec_roundtrip() {
    // UTF-8 pass-through (valid UTF-8 text persisted as-is, no hex prefix)
    let text = "abc\n中文\n";
    let stored = tool::snapshot_to_store(text.as_bytes());
    assert_eq!(stored, text);
    assert_eq!(tool::snapshot_from_store(&stored), text.as_bytes());

    // Non-UTF-8 goes through hex
    let bytes: Vec<u8> = vec![0xFF, 0xFE, 0x01, 0x00, 0xD6, 0xD0];
    let stored = tool::snapshot_to_store(&bytes);
    assert!(stored.starts_with("pigcode:hex:"), "{stored}");
    assert_eq!(tool::snapshot_from_store(&stored), bytes);
}

#[test]
fn tracker_original_restore_roundtrip_gbk() {
    let dir = temp_dir("tracker-store");
    let file = dir.join("g.txt");
    let bytes = gbk_bytes("快照内容\n");
    std::fs::write(&file, &bytes).unwrap();

    let mut tracker = ChangeTracker::default();
    tracker.snapshot(&file).unwrap();
    let stored = tracker
        .original(&file)
        .expect("tracked")
        .expect("file exists");
    assert!(
        stored.starts_with("pigcode:hex:"),
        "GBK bytes should be hex-encoded: {stored}"
    );

    // Simulate a restart: restore into a new tracker; revert should restore the bytes
    std::fs::write(&file, b"corrupted").unwrap();
    let mut revived = ChangeTracker::default();
    revived.restore(vec![(file.clone(), Some(stored))]);
    revived.revert(&file).unwrap();
    assert_eq!(std::fs::read(&file).unwrap(), bytes);

    // UTF-8 file: original() returns the text directly; after restore, revert is equally byte-exact
    let utf8_file = dir.join("u.txt");
    std::fs::write(&utf8_file, "plain utf8\n").unwrap();
    let mut tracker = ChangeTracker::default();
    tracker.snapshot(&utf8_file).unwrap();
    let stored = tracker.original(&utf8_file).unwrap().unwrap();
    assert_eq!(stored, "plain utf8\n");
}

// ---------- is_sensitive_file ----------

#[test]
fn sensitive_file_patterns() {
    use std::path::Path;
    for name in [
        ".env",
        ".ENV",
        ".env.local",
        ".env.production",
        "id_rsa",
        "ID_ED25519",
        "id_ecdsa",
        "id_dsa",
        "id_rsa.bak",
        "id_ed25519-x",
        "id_rsa_old",
    ] {
        assert!(
            tool::is_sensitive_file(Path::new(name)),
            "{name} should be flagged as sensitive"
        );
    }
    for name in [
        ".env.example",
        ".env.sample",
        ".env.template",
        "id_rsa.pub",
        "id_ed25519.pub",
        "env.txt",
        "my_id_rsa_notes.md", // prefix not at the start of the file name
        "credentials",
    ] {
        assert!(
            !tool::is_sensitive_file(Path::new(name)),
            "{name} should not be flagged as sensitive"
        );
    }
    // Cloud credentials look at the parent directory name
    assert!(tool::is_sensitive_file(Path::new(
        "/home/u/.aws/credentials"
    )));
    assert!(tool::is_sensitive_file(Path::new(
        "/home/u/.gcp/credentials"
    )));
    assert!(!tool::is_sensitive_file(Path::new(
        "/home/u/app/credentials"
    )));
}

// ---------- 4.1 Dangling symlink blocking ----------

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dangling_symlink_rejected_fail_closed() {
    let dir = temp_dir("dangling-symlink");
    // A dangling link pointing to a nonexistent target
    let missing = std::env::temp_dir().join(format!(
        "pig-core-nonexistent-dir-xxxxx-{}/evil.txt",
        std::process::id()
    ));
    std::os::unix::fs::symlink(&missing, dir.join("link.txt")).unwrap();

    let (out, is_error, _, _, _) = run_tool(
        &dir,
        "Write",
        serde_json::json!({"path": "link.txt", "content": "pwned\n"}),
    )
    .await;
    assert!(is_error, "{out}");
    assert!(
        out.contains("Symlink points to a nonexistent target"),
        "{out}"
    );
    assert!(!missing.exists(), "the external file should not be created");

    // Reading the same path is also fail-closed
    let (out, is_error, _, _, _) =
        run_tool(&dir, "Read", serde_json::json!({"path": "link.txt"})).await;
    assert!(is_error, "{out}");
    assert!(
        out.contains("Symlink points to a nonexistent target"),
        "{out}"
    );
}

// ---------- 4.2 Edit tolerant match tiers ----------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn edit_strips_read_line_number_prefixes() {
    let dir = temp_dir("edit-strip-lineno");
    std::fs::write(dir.join("f.rs"), "fn main() {\n    foo();\n}\n").unwrap();
    std::fs::write(dir.join("g.rs"), "x = 1;\ny = 2;\n").unwrap();
    std::fs::write(dir.join("m.rs"), "dup\ndup\n").unwrap();

    let mut tracker = ChangeTracker::default();
    let state = SessionToolState::for_test();
    for file in ["f.rs", "g.rs", "m.rs"] {
        let (_, is_error, _, _, _) = run_tool_in(
            &dir,
            &mut tracker,
            &state,
            "Read",
            serde_json::json!({"path": file}),
        )
        .await;
        assert!(!is_error);
    }

    // The model copies the line number along from the Read output (the "2\t" prefix)
    let (out, is_error, _, _, _) = run_tool_in(
        &dir,
        &mut tracker,
        &state,
        "Edit",
        serde_json::json!({"path": "f.rs", "old_string": "2\t    foo();", "new_string": "    bar();"}),
    )
    .await;
    assert!(!is_error, "{out}");
    assert!(
        out.contains("tolerant match: line-number prefixes stripped"),
        "{out}"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("f.rs")).unwrap(),
        "fn main() {\n    bar();\n}\n"
    );

    // The "line-number:" prefix variant (grep style, no space after the colon)
    let (out, is_error, _, _, _) = run_tool_in(
        &dir,
        &mut tracker,
        &state,
        "Edit",
        serde_json::json!({"path": "g.rs", "old_string": "2:y = 2;", "new_string": "y = 3;"}),
    )
    .await;
    assert!(!is_error, "{out}");
    assert_eq!(
        std::fs::read_to_string(dir.join("g.rs")).unwrap(),
        "x = 1;\ny = 3;\n"
    );

    // Multiple matches after stripping → still reports "appears N times"
    let (out, is_error, _, _, _) = run_tool_in(
        &dir,
        &mut tracker,
        &state,
        "Edit",
        serde_json::json!({"path": "m.rs", "old_string": "1\tdup", "new_string": "x"}),
    )
    .await;
    assert!(is_error, "{out}");
    assert!(out.contains("appears 2 times"), "{out}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn edit_quote_normalization_follows_file_style() {
    let dir = temp_dir("edit-quotes");
    // A curly-quote file
    std::fs::write(dir.join("q.rs"), "let s = \u{201C}hello\u{201D};\n").unwrap();

    let mut tracker = ChangeTracker::default();
    let state = SessionToolState::for_test();
    let (_, is_error, _, _, _) = run_tool_in(
        &dir,
        &mut tracker,
        &state,
        "Read",
        serde_json::json!({"path": "q.rs"}),
    )
    .await;
    assert!(!is_error);

    // A straight-quote old_string hits; straight quotes in new_string are converted to curly
    let (out, is_error, _, _, _) = run_tool_in(
        &dir,
        &mut tracker,
        &state,
        "Edit",
        serde_json::json!({"path": "q.rs", "old_string": "let s = \"hello\";", "new_string": "let s = \"world\";"}),
    )
    .await;
    assert!(!is_error, "{out}");
    assert!(
        out.contains("tolerant match: quote style adjusted to match the file"),
        "{out}"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("q.rs")).unwrap(),
        "let s = \u{201C}world\u{201D};\n",
        "straight quotes should be converted to curly quotes in pairs"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn edit_replace_all_disables_fuzzy_tiers() {
    let dir = temp_dir("edit-fuzzy-off");
    std::fs::write(dir.join("f.txt"), "a\na\n").unwrap();

    let mut tracker = ChangeTracker::default();
    let state = SessionToolState::for_test();
    let (_, is_error, _, _, _) = run_tool_in(
        &dir,
        &mut tracker,
        &state,
        "Read",
        serde_json::json!({"path": "f.txt"}),
    )
    .await;
    assert!(!is_error);

    // replace_all + line-number prefix: tier-2 tolerant matching is skipped, exact miss → the error copy stays unchanged
    let (out, is_error, _, _, _) = run_tool_in(
        &dir,
        &mut tracker,
        &state,
        "Edit",
        serde_json::json!({"path": "f.txt", "old_string": "1\ta", "new_string": "b", "replace_all": true}),
    )
    .await;
    assert!(is_error, "{out}");
    assert!(out.contains("old_string not found"), "{out}");

    // When all three tiers miss, the not-found error copy stays unchanged
    let (out, is_error, _, _, _) = run_tool_in(
        &dir,
        &mut tracker,
        &state,
        "Edit",
        serde_json::json!({"path": "f.txt", "old_string": "zzz", "new_string": "b"}),
    )
    .await;
    assert!(is_error, "{out}");
    assert!(out.contains("old_string not found in f.txt"), "{out}");
    assert!(!out.contains("tolerant match"), "{out}");
}

// ---------- 5.1 read-file-state pre-write freshness ----------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn write_edit_requires_prior_read() {
    let dir = temp_dir("fresh-gate");
    std::fs::write(dir.join("exist.txt"), "old\n").unwrap();
    let mut tracker = ChangeTracker::default();
    let state = SessionToolState::for_test();

    // Writing before reading: both Edit / Write are rejected
    let (out, is_error, _, _, _) = run_tool_in(
        &dir,
        &mut tracker,
        &state,
        "Edit",
        serde_json::json!({"path": "exist.txt", "old_string": "old", "new_string": "new"}),
    )
    .await;
    assert!(is_error, "{out}");
    assert!(out.contains("has not been read"), "{out}");
    let (out, is_error, _, _, _) = run_tool_in(
        &dir,
        &mut tracker,
        &state,
        "Write",
        serde_json::json!({"path": "exist.txt", "content": "new\n"}),
    )
    .await;
    assert!(is_error, "{out}");
    assert!(out.contains("has not been read"), "{out}");

    // A new (nonexistent) file needs no Read
    let (_, is_error, _, _, _) = run_tool_in(
        &dir,
        &mut tracker,
        &state,
        "Write",
        serde_json::json!({"path": "new.txt", "content": "a\nb\n"}),
    )
    .await;
    assert!(!is_error, "{out}");

    // Editing a file right after writing it: allowed (the write refreshed the state)
    let (_, is_error, _, _, _) = run_tool_in(
        &dir,
        &mut tracker,
        &state,
        "Edit",
        serde_json::json!({"path": "new.txt", "old_string": "b", "new_string": "B"}),
    )
    .await;
    assert!(!is_error, "{out}");

    // Write is allowed after Read
    let (_, is_error, _, _, _) = run_tool_in(
        &dir,
        &mut tracker,
        &state,
        "Read",
        serde_json::json!({"path": "exist.txt"}),
    )
    .await;
    assert!(!is_error);
    let (_, is_error, _, _, _) = run_tool_in(
        &dir,
        &mut tracker,
        &state,
        "Write",
        serde_json::json!({"path": "exist.txt", "content": "new\n"}),
    )
    .await;
    assert!(!is_error, "{out}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn edit_blocked_after_external_modification() {
    let dir = temp_dir("fresh-stale");
    std::fs::write(dir.join("f.txt"), "v1\n").unwrap();
    let mut tracker = ChangeTracker::default();
    let state = SessionToolState::for_test();

    let (_, is_error, _, _, _) = run_tool_in(
        &dir,
        &mut tracker,
        &state,
        "Read",
        serde_json::json!({"path": "f.txt"}),
    )
    .await;
    assert!(!is_error);

    // An external process modified the file → Edit rejected
    std::fs::write(dir.join("f.txt"), "v2\n").unwrap();
    let (out, is_error, _, _, _) = run_tool_in(
        &dir,
        &mut tracker,
        &state,
        "Edit",
        serde_json::json!({"path": "f.txt", "old_string": "v1", "new_string": "v3"}),
    )
    .await;
    assert!(is_error, "{out}");
    assert!(out.contains("modified externally"), "{out}");

    // Allowed after a fresh Read
    let (_, is_error, _, _, _) = run_tool_in(
        &dir,
        &mut tracker,
        &state,
        "Read",
        serde_json::json!({"path": "f.txt"}),
    )
    .await;
    assert!(!is_error);
    let (_, is_error, _, _, _) = run_tool_in(
        &dir,
        &mut tracker,
        &state,
        "Edit",
        serde_json::json!({"path": "f.txt", "old_string": "v2", "new_string": "v3"}),
    )
    .await;
    assert!(!is_error, "{out}");
    assert_eq!(std::fs::read_to_string(dir.join("f.txt")).unwrap(), "v3\n");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mtime_touch_with_same_content_allowed() {
    let dir = temp_dir("fresh-mtime");
    std::fs::write(dir.join("f.txt"), "same content\n").unwrap();
    let mut tracker = ChangeTracker::default();
    let state = SessionToolState::for_test();

    let (_, is_error, _, _, _) = run_tool_in(
        &dir,
        &mut tracker,
        &state,
        "Read",
        serde_json::json!({"path": "f.txt"}),
    )
    .await;
    assert!(!is_error);

    // Rewriting the same content (mtime touched, hash unchanged) → allowed and the state is refreshed in passing
    std::thread::sleep(std::time::Duration::from_millis(10));
    std::fs::write(dir.join("f.txt"), "same content\n").unwrap();
    let (_, is_error, _, _, _) = run_tool_in(
        &dir,
        &mut tracker,
        &state,
        "Edit",
        serde_json::json!({"path": "f.txt", "old_string": "same", "new_string": "still same"}),
    )
    .await;
    assert!(!is_error, "same hash should be allowed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn partial_read_blocks_write_paged_read_clears() {
    let dir = temp_dir("fresh-partial");
    // Over 100k chars: a Read without arguments is guaranteed to be truncated by the budget
    let content: String = (1..=4000)
        .map(|i| format!("line {i:04} {}\n", "y".repeat(30)))
        .collect();
    std::fs::write(dir.join("big.txt"), content).unwrap();
    let mut tracker = ChangeTracker::default();
    let state = SessionToolState::for_test();

    let (out, is_error, _, _, _) = run_tool_in(
        &dir,
        &mut tracker,
        &state,
        "Read",
        serde_json::json!({"path": "big.txt"}),
    )
    .await;
    assert!(!is_error, "{out}");
    assert!(out.contains("[Truncated"), "{out}");

    // Incomplete view: both Edit/Write are rejected
    let (out, is_error, _, _, _) = run_tool_in(
        &dir,
        &mut tracker,
        &state,
        "Edit",
        serde_json::json!({"path": "big.txt", "old_string": "line 0001", "new_string": "x"}),
    )
    .await;
    assert!(is_error, "{out}");
    assert!(out.contains("incomplete view"), "{out}");

    // An explicitly paged Read does not count as partial: the registration
    // overrides to the full-view rule (freshness/hash still follow the last
    // record), and Write is allowed — same rule as ZCode: pagination
    // parameters mean the model knows it only viewed a window
    let (_, is_error, _, _, _) = run_tool_in(
        &dir,
        &mut tracker,
        &state,
        "Read",
        serde_json::json!({"path": "big.txt", "offset": 1, "limit": 50}),
    )
    .await;
    assert!(!is_error);
    let (_, is_error, _, _, _) = run_tool_in(
        &dir,
        &mut tracker,
        &state,
        "Write",
        serde_json::json!({"path": "big.txt", "content": "rewritten\n"}),
    )
    .await;
    assert!(!is_error, "{out}");
}

// ---------- Approval preview goes through the text pipeline (approval_detail) ----------

#[test]
fn approval_detail_edit_crlf_preview_clean() {
    let dir = temp_dir("approval-crlf");
    std::fs::write(dir.join("win.txt"), b"a\r\nb\r\nc\r\n").unwrap();

    let detail = pig_core::session::approval_detail(
        &call(
            "Edit",
            serde_json::json!({"path": "win.txt", "old_string": "b", "new_string": "B"}),
        ),
        &dir,
    );
    assert!(
        !detail.contains('\r'),
        "preview is based on the LF view: {detail:?}"
    );
    assert!(detail.contains("-b") && detail.contains("+B"), "{detail}");
    assert!(
        !detail.contains("-a") && !detail.contains("-c"),
        "unchanged lines should not enter the diff (no whole-file flip): {detail}"
    );
}

#[test]
fn approval_detail_edit_replace_all_and_fuzzy_notes() {
    let dir = temp_dir("approval-notes");
    std::fs::write(dir.join("all.txt"), "x\nx\nx\n").unwrap();
    let detail = pig_core::session::approval_detail(
        &call(
            "Edit",
            serde_json::json!({"path": "all.txt", "old_string": "x", "new_string": "y", "replace_all": true}),
        ),
        &dir,
    );
    assert!(
        detail.contains("(replace_all: replaced 3 occurrences)"),
        "{detail}"
    );

    // Tolerant tiers: an old_string with a Read line-number prefix hits tier 2; the detail notes it
    std::fs::write(dir.join("f.rs"), "fn a() {}\nlet x = 1;\n").unwrap();
    let detail = pig_core::session::approval_detail(
        &call(
            "Edit",
            serde_json::json!({"path": "f.rs", "old_string": "1\tfn a() {}", "new_string": "fn b() {}"}),
        ),
        &dir,
    );
    assert!(
        detail.contains("(tolerant match: line-number prefixes stripped)"),
        "{detail}"
    );
}

#[test]
fn approval_detail_write_decodes_existing_file() {
    let dir = temp_dir("approval-write");
    // CRLF: the preview must not flip the whole file (compared on the LF view)
    std::fs::write(dir.join("w.txt"), b"keep\r\nold\r\n").unwrap();
    let detail = pig_core::session::approval_detail(
        &call(
            "Write",
            serde_json::json!({"path": "w.txt", "content": "keep\nnew\n"}),
        ),
        &dir,
    );
    assert!(!detail.contains('\r'), "{detail:?}");
    assert!(
        detail.contains("-old") && detail.contains("+new"),
        "{detail}"
    );
    assert!(
        !detail.contains("-keep"),
        "unchanged lines stay out of the diff: {detail}"
    );

    // GBK: before uses the decoded text view, Chinese text is not garbled
    std::fs::write(dir.join("g.txt"), gbk_bytes("中文行\n旧行\n")).unwrap();
    let detail = pig_core::session::approval_detail(
        &call(
            "Write",
            serde_json::json!({"path": "g.txt", "content": "中文行\n新行\n"}),
        ),
        &dir,
    );
    assert!(
        detail.contains("-旧行") && detail.contains("+新行"),
        "{detail}"
    );
}
