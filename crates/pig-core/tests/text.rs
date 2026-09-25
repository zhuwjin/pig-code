//! 文本管线与文件工具边界防护测试：text 模块编解码、Read/Write/Edit 的
//! 编码/行尾保留、敏感文件与符号链接防护、ChangeTracker 字节级快照。

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

// ---------- text 模块 ----------

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
    // 2 个 CRLF、无孤立 LF → 主导 CRLF，视图归一为 LF
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
    assert_eq!(doc.encoding, FileEncoding::Utf16Le, "应被启发式识别");
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
    assert!(err.contains("二进制"), "{err}");
}

#[test]
fn decode_control_chars_rejected() {
    // 0x01-0x08 循环填满：控制字符占比 100%，且无 NUL
    let bytes: Vec<u8> = (0..200).map(|i| 0x01 + (i % 8) as u8).collect();
    let err = text::decode(&bytes).unwrap_err();
    assert!(err.contains("二进制"), "{err}");

    // 正常文本里夹少量控制字符不误判（占比远低于 30%）
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
        "GBK 字节不是合法 UTF-8"
    );
    let doc = text::decode(&bytes).unwrap();
    assert_eq!(doc.encoding, FileEncoding::Gbk);
    assert_eq!(doc.text, "中文测试，编码识别\n");

    // 非法 UTF-8 且 GBK round-trip 不复原（孤立 lead byte → 替换符）
    let err = text::decode(&[0x81, 0x82, 0x83]).unwrap_err();
    assert!(err.contains("无法识别的文本编码"), "{err}");
}

#[test]
fn decode_crlf_dominant_and_mixed() {
    let doc = text::decode(b"a\r\nb\r\nc\n").unwrap();
    assert_eq!(doc.line_ending, LineEnding::Crlf);
    assert_eq!(doc.text, "a\nb\nc\n", "视图一律归一为 LF");

    // 孤立 \n 多 → 主导 LF；残留的 \r\n 也归一（孤立 \r 保留）
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
    // 入参自带 \r\n 时先归一，避免 \r\r\n
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
    assert!(err.contains("GBK 无法编码"), "{err}");
}

#[test]
fn decode_utf16_odd_trailing_byte_is_lossy() {
    let mut bytes = text::encode("hi\n", FileEncoding::Utf16Le, true, LineEnding::Lf).unwrap();
    bytes.push(0x61); // 奇数尾字节
    let doc = text::decode(&bytes).unwrap();
    assert_eq!(doc.text, "hi\n");
    assert!(doc.lossy, "奇数尾字节丢弃应置 lossy");
}

// ---------- Read ----------

async fn run_tool(
    dir: &std::path::Path,
    name: &str,
    args: serde_json::Value,
) -> (
    String,
    bool,
    Option<tool::FileChange>,
    Option<pig_protocol::EditDiff>,
) {
    let mut tracker = ChangeTracker::default();
    let state = SessionToolState::for_test();
    tool::execute(
        &call(name, args),
        ToolContext {
            cwd: dir,
            tracker: &mut tracker,
            state: &state,
        },
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_has_line_numbers() {
    let dir = temp_dir("read-lines");
    std::fs::write(dir.join("a.txt"), "alpha\nbeta\ngamma\n").unwrap();

    let (out, is_error, _, _) = run_tool(&dir, "Read", serde_json::json!({"path": "a.txt"})).await;
    assert!(!is_error, "{out}");
    assert!(out.contains("1\talpha"), "{out}");
    assert!(out.contains("2\tbeta"), "{out}");
    assert!(out.contains("3\tgamma"), "{out}");

    // offset 起算行号
    let (out, is_error, _, _) = run_tool(
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

    let (out, is_error, _, _) =
        run_tool(&dir, "Read", serde_json::json!({"path": "win.txt"})).await;
    assert!(!is_error, "{out}");
    assert!(!out.contains('\r'), "输出不应含 CR: {out:?}");
    assert!(out.contains("1\ta"), "{out}");
    assert!(
        out.contains("[文件信息:") && out.contains("行尾=CRLF"),
        "应有文件信息行: {out}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_utf16le_and_gbk() {
    let dir = temp_dir("read-enc");
    let utf16 = text::encode("你好\n世界\n", FileEncoding::Utf16Le, true, LineEnding::Lf).unwrap();
    std::fs::write(dir.join("u16.txt"), &utf16).unwrap();
    std::fs::write(dir.join("g.txt"), gbk_bytes("中文内容\n第二行\n")).unwrap();

    let (out, is_error, _, _) =
        run_tool(&dir, "Read", serde_json::json!({"path": "u16.txt"})).await;
    assert!(!is_error, "{out}");
    assert!(out.contains("1\t你好"), "{out}");
    assert!(out.contains("编码=UTF-16LE"), "{out}");

    let (out, is_error, _, _) = run_tool(&dir, "Read", serde_json::json!({"path": "g.txt"})).await;
    assert!(!is_error, "{out}");
    assert!(out.contains("中文内容"), "{out}");
    assert!(out.contains("编码=GBK"), "{out}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_binary_rejected() {
    let dir = temp_dir("read-bin");
    std::fs::write(dir.join("bin.dat"), b"PK\x03\x04\x00\x01binary").unwrap();

    let (out, is_error, _, _) =
        run_tool(&dir, "Read", serde_json::json!({"path": "bin.dat"})).await;
    assert!(is_error, "{out}");
    assert!(out.contains("二进制"), "{out}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_long_line_truncated() {
    let dir = temp_dir("read-long-line");
    let long = "x".repeat(3000);
    std::fs::write(dir.join("long.txt"), format!("{long}\nshort\n")).unwrap();

    let (out, is_error, _, _) =
        run_tool(&dir, "Read", serde_json::json!({"path": "long.txt"})).await;
    assert!(!is_error, "{out}");
    assert!(
        out.contains("[...本行已截断，共 3000 字符]"),
        "应有单行截断标记: {}",
        &out[..out.len().min(300)]
    );
    assert!(out.contains("2\tshort"), "{out}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_char_budget_truncates() {
    let dir = temp_dir("read-budget");
    // 3000 行 × 50 字符 ≈ 15 万字符，超过 10 万预算（行数上限 2000 也不会先到）
    let content: String = (1..=3000)
        .map(|i| format!("line {i:04} {}\n", "y".repeat(40)))
        .collect();
    std::fs::write(dir.join("big.txt"), content).unwrap();

    let (out, is_error, _, _) =
        run_tool(&dir, "Read", serde_json::json!({"path": "big.txt"})).await;
    assert!(!is_error, "{out}");
    assert!(out.contains("[已截断: 显示 1-"), "应有截断提示: {out}");
    assert!(out.contains("共 3000 行"), "{out}");
    assert!(out.len() < 120_000, "输出应受字符预算约束: {}", out.len());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_empty_and_missing_file() {
    let dir = temp_dir("read-empty");
    std::fs::write(dir.join("empty.txt"), "").unwrap();
    std::fs::write(dir.join("a.rs"), "fn a() {}\n").unwrap();
    std::fs::write(dir.join("b.rs"), "fn b() {}\n").unwrap();

    let (out, is_error, _, _) =
        run_tool(&dir, "Read", serde_json::json!({"path": "empty.txt"})).await;
    assert!(!is_error, "{out}");
    assert_eq!(out, "（空文件）");

    let (out, is_error, _, _) =
        run_tool(&dir, "Read", serde_json::json!({"path": "missing.rs"})).await;
    assert!(is_error, "{out}");
    assert!(out.contains("文件不存在: missing.rs"), "{out}");
    assert!(
        out.contains("a.rs") && out.contains("b.rs"),
        "应列同目录文件: {out}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_env_rejected_but_template_allowed() {
    let dir = temp_dir("read-env");
    std::fs::write(dir.join(".env"), "SECRET=1\n").unwrap();
    std::fs::write(dir.join(".env.local"), "SECRET=2\n").unwrap();
    std::fs::write(dir.join(".env.example"), "SECRET=\n").unwrap();

    for path in [".env", ".env.local"] {
        let (out, is_error, _, _) = run_tool(&dir, "Read", serde_json::json!({"path": path})).await;
        assert!(is_error, "{path} 应拒绝: {out}");
        assert!(out.contains("敏感文件"), "{out}");
        assert!(!out.contains("SECRET=1"), "内容不应泄露: {out}");
    }
    let (out, is_error, _, _) =
        run_tool(&dir, "Read", serde_json::json!({"path": ".env.example"})).await;
    assert!(!is_error, "模板文件应可读: {out}");
}

// ---------- Write ----------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn write_preserves_crlf() {
    let dir = temp_dir("write-crlf");
    std::fs::write(dir.join("win.txt"), b"a\r\nb\r\n").unwrap();

    let (out, is_error, _, _) = run_tool(
        &dir,
        "Write",
        serde_json::json!({"path": "win.txt", "content": "x\ny\n"}),
    )
    .await;
    assert!(!is_error, "{out}");
    assert!(out.contains("CRLF"), "输出应说明保留了 CRLF: {out}");
    assert_eq!(
        std::fs::read(dir.join("win.txt")).unwrap(),
        b"x\r\ny\r\n",
        "写回应保持 CRLF"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn write_preserves_gbk() {
    let dir = temp_dir("write-gbk");
    std::fs::write(dir.join("g.txt"), gbk_bytes("旧中文\n")).unwrap();

    let (out, is_error, _, _) = run_tool(
        &dir,
        "Write",
        serde_json::json!({"path": "g.txt", "content": "新的中文\n"}),
    )
    .await;
    assert!(!is_error, "{out}");
    assert!(out.contains("GBK"), "{out}");
    assert_eq!(
        std::fs::read(dir.join("g.txt")).unwrap(),
        gbk_bytes("新的中文\n"),
        "写回应是 GBK 字节"
    );

    // GBK 文件写入不可编码字符 → 拒绝且文件不动
    let before = std::fs::read(dir.join("g.txt")).unwrap();
    let (out, is_error, _, _) = run_tool(
        &dir,
        "Write",
        serde_json::json!({"path": "g.txt", "content": "带 emoji 🎉\n"}),
    )
    .await;
    assert!(is_error, "{out}");
    assert!(out.contains("GBK"), "{out}");
    assert_eq!(
        std::fs::read(dir.join("g.txt")).unwrap(),
        before,
        "拒绝写入时文件不动"
    );
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn write_symlink_escape_rejected() {
    let dir = temp_dir("write-symlink");
    let outside = temp_dir("write-symlink-outside");
    std::fs::write(outside.join("secret.txt"), "top secret\n").unwrap();
    std::os::unix::fs::symlink(outside.join("secret.txt"), dir.join("link.txt")).unwrap();

    let (out, is_error, _, _) = run_tool(
        &dir,
        "Write",
        serde_json::json!({"path": "link.txt", "content": "pwned\n"}),
    )
    .await;
    assert!(is_error, "{out}");
    assert!(out.contains("越出工作目录"), "{out}");
    assert_eq!(
        std::fs::read_to_string(outside.join("secret.txt")).unwrap(),
        "top secret\n",
        "符号链接目标不应被改写"
    );

    let (out, is_error, _, _) =
        run_tool(&dir, "Read", serde_json::json!({"path": "link.txt"})).await;
    assert!(is_error, "{out}");
    assert!(out.contains("越出工作目录"), "{out}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn write_env_rejected() {
    let dir = temp_dir("write-env");
    let (out, is_error, _, _) = run_tool(
        &dir,
        "Write",
        serde_json::json!({"path": ".env", "content": "SECRET=1\n"}),
    )
    .await;
    assert!(is_error, "{out}");
    assert!(out.contains("敏感文件"), "{out}");
    assert!(!dir.join(".env").exists(), "拒绝后不应落盘");
}

// ---------- Edit ----------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn edit_crlf_preserved_and_diff_clean() {
    let dir = temp_dir("edit-crlf");
    std::fs::write(dir.join("win.txt"), b"a\r\nb\r\nc\r\n").unwrap();

    let (out, is_error, _, edit) = run_tool(
        &dir,
        "Edit",
        serde_json::json!({"path": "win.txt", "old_string": "b", "new_string": "B"}),
    )
    .await;
    assert!(!is_error, "{out}");
    assert_eq!(
        std::fs::read(dir.join("win.txt")).unwrap(),
        b"a\r\nB\r\nc\r\n",
        "编辑后磁盘仍是 CRLF"
    );
    let edit = edit.expect("Edit 应带本次编辑 diff");
    assert_eq!(
        (edit.additions, edit.deletions),
        (1, 1),
        "diff 只反映本次替换"
    );
    assert!(
        !edit.unified_diff.contains('\r'),
        "diff 基于 LF 视图应干净: {}",
        edit.unified_diff
    );
    assert!(edit.unified_diff.contains("-b") && edit.unified_diff.contains("+B"));

    // CRLF 文件用 \r\n 的 old_string 匹配不上时，报错应引导用 LF
    let (out, is_error, _, _) = run_tool(
        &dir,
        "Edit",
        serde_json::json!({"path": "win.txt", "old_string": "a\r\nB", "new_string": "x"}),
    )
    .await;
    assert!(is_error, "{out}");
    assert!(out.contains("CRLF 行尾"), "应提示用 LF: {out}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn edit_replace_all_counts() {
    let dir = temp_dir("edit-all");
    std::fs::write(dir.join("f.txt"), "foo\nfoo\nfoo\n").unwrap();

    // 不设 replace_all：多处匹配报错，保留「出现 N 次」并引导 replace_all
    let (out, is_error, _, _) = run_tool(
        &dir,
        "Edit",
        serde_json::json!({"path": "f.txt", "old_string": "foo", "new_string": "bar"}),
    )
    .await;
    assert!(is_error, "{out}");
    assert!(out.contains("出现 3 次"), "{out}");
    assert!(out.contains("replace_all=true"), "{out}");

    let (out, is_error, _, _) = run_tool(
        &dir,
        "Edit",
        serde_json::json!({"path": "f.txt", "old_string": "foo", "new_string": "bar", "replace_all": true}),
    )
    .await;
    assert!(!is_error, "{out}");
    assert!(out.contains("替换 3 处"), "{out}");
    assert_eq!(
        std::fs::read_to_string(dir.join("f.txt")).unwrap(),
        "bar\nbar\nbar\n"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn edit_old_equals_new_rejected() {
    let dir = temp_dir("edit-same");
    std::fs::write(dir.join("f.txt"), "same\n").unwrap();

    let (out, is_error, _, _) = run_tool(
        &dir,
        "Edit",
        serde_json::json!({"path": "f.txt", "old_string": "same", "new_string": "same"}),
    )
    .await;
    assert!(is_error, "{out}");
    assert!(out.contains("相同"), "{out}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn edit_empty_new_swallows_trailing_newline() {
    let dir = temp_dir("edit-delete-line");
    std::fs::write(dir.join("f.txt"), "a\nb\nc\n").unwrap();

    // old_string 占整行（不含 \n）→ 连行尾 \n 一起删，不留空行
    let (out, is_error, _, _) = run_tool(
        &dir,
        "Edit",
        serde_json::json!({"path": "f.txt", "old_string": "b", "new_string": ""}),
    )
    .await;
    assert!(!is_error, "{out}");
    assert_eq!(
        std::fs::read_to_string(dir.join("f.txt")).unwrap(),
        "a\nc\n"
    );

    // 文件末尾无 \n 可吞时保持原样拼接
    std::fs::write(dir.join("g.txt"), "x\ny").unwrap();
    let (_, is_error, _, _) = run_tool(
        &dir,
        "Edit",
        serde_json::json!({"path": "g.txt", "old_string": "y", "new_string": ""}),
    )
    .await;
    assert!(!is_error);
    assert_eq!(std::fs::read_to_string(dir.join("g.txt")).unwrap(), "x\n");

    // replace_all 逐处同样吞换行
    std::fs::write(dir.join("h.txt"), "keep\nDEL\nkeep\nDEL\n").unwrap();
    let (out, is_error, _, _) = run_tool(
        &dir,
        "Edit",
        serde_json::json!({"path": "h.txt", "old_string": "DEL", "new_string": "", "replace_all": true}),
    )
    .await;
    assert!(!is_error, "{out}");
    assert!(out.contains("替换 2 处"), "{out}");
    assert_eq!(
        std::fs::read_to_string(dir.join("h.txt")).unwrap(),
        "keep\nkeep\n"
    );
}

// ---------- ChangeTracker ----------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn revert_gbk_file_is_byte_exact() {
    let dir = temp_dir("revert-gbk");
    // GBK 编码 + CRLF 行尾的 fixture
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
    let (_, is_error, _, _) = tool::execute(
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
    assert_ne!(std::fs::read(&file).unwrap(), original, "编辑应改变字节");

    tracker.revert(&file).unwrap();
    assert_eq!(
        std::fs::read(&file).unwrap(),
        original,
        "GBK/CRLF 文件 revert 应字节级还原"
    );
}

#[test]
fn snapshot_store_codec_roundtrip() {
    // UTF-8 直通（无前缀，兼容旧数据）
    let text = "abc\n中文\n";
    let stored = tool::snapshot_to_store(text.as_bytes());
    assert_eq!(stored, text);
    assert_eq!(tool::snapshot_from_store(&stored), text.as_bytes());

    // 非 UTF-8 走 hex
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
    let stored = tracker.original(&file).expect("已追踪").expect("文件存在");
    assert!(
        stored.starts_with("pigcode:hex:"),
        "GBK 字节应 hex 化: {stored}"
    );

    // 模拟重启：restore 进新 tracker，revert 应还原字节
    std::fs::write(&file, b"corrupted").unwrap();
    let mut revived = ChangeTracker::default();
    revived.restore(vec![(file.clone(), Some(stored))]);
    revived.revert(&file).unwrap();
    assert_eq!(std::fs::read(&file).unwrap(), bytes);

    // UTF-8 文件：original() 直出文本，restore 后 revert 同样字节精确
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
        assert!(tool::is_sensitive_file(Path::new(name)), "{name} 应判敏感");
    }
    for name in [
        ".env.example",
        ".env.sample",
        ".env.template",
        "id_rsa.pub",
        "id_ed25519.pub",
        "env.txt",
        "my_id_rsa_notes.md", // 前缀不在文件名开头
        "credentials",
    ] {
        assert!(
            !tool::is_sensitive_file(Path::new(name)),
            "{name} 不应判敏感"
        );
    }
    // 云凭据看父目录名
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
