//! 会话分叉（ZCode forkAssistant 同语义）：以源会话前 N 个回合的历史派生
//! 新会话——纯对话复制，不动工作区、不中断源会话在飞回合（rollout 每行
//! flush，读盘即一致快照）。boundary 含第 N 回合本身。
//!
//! v1 明确不做（见 docs/PLAN.md）：`.agents/` 子代理上下文不复制
//!（task-notification 里的 record/result 绝对路径仍指源会话目录，源未删就能
//! 打开）；`.model-io.jsonl` 调试轨迹不复制；父会话不加分叉标记；子会话无
//! 「从对话中派生」回跳行（标题后缀「（分叉）」辨识）。

use std::path::Path;
use std::sync::{Arc, Mutex};

use pig_protocol::SessionMeta;

use crate::rollout::{Rollout, RolloutRecord, media_dir, now_secs};
use crate::store::Store;

/// 派生新会话并落库，返回新 session id（调用方按 OpenSession 冷路径打开）。
/// `turns` = 保留的回合数：截断在第 N+1 条 User 记录之前（TurnStats/
/// TurnChanges/StepUsage/Compact 随回合自然保留；ToolCall 记录自带回执无
/// 配对问题）。turns=0 钳为 1；超过源回合总数 = 全量复制。
pub fn fork_session(
    sessions_dir: &Path,
    store: &Arc<Mutex<Store>>,
    src_id: &str,
    turns: usize,
    id_counter: &mut u64,
) -> Result<String, String> {
    let src_path = sessions_dir.join(format!("{src_id}.jsonl"));
    let records = Rollout::load(&src_path)?;
    let src_meta = store
        .lock()
        .expect("store lock")
        .get_session(src_id)
        .ok_or_else(|| format!("源会话不存在: {src_id}"))?;

    let turns = turns.max(1);
    let mut kept = truncate_after_turns(&records, turns).to_vec();

    *id_counter += 1;
    let new_id = format!("s{}-{}", now_secs(), id_counter);

    // 图片是含源会话 id 的绝对路径（{sessions}/{src}.media/N.ext）：
    // 复制被引用文件到新媒体目录并改写记录，源会话删除后分叉图仍可用
    let src_media = media_dir(sessions_dir, src_id);
    let new_media = media_dir(sessions_dir, &new_id);
    copy_referenced_media(&src_media, &new_media, &mut kept)?;

    let meta = SessionMeta {
        id: new_id.clone(),
        title: format!("{}（分叉）", src_meta.title),
        // 分叉标题固定：自动命名不覆盖
        title_custom: true,
        cwd: src_meta.cwd.clone(),
        created_at: now_secs(),
        updated_at: now_secs(),
        pinned: false,
        archived: false,
        provider_id: src_meta.provider_id.clone(),
        model_id: src_meta.model_id.clone(),
        reasoning_level: src_meta.reasoning_level.clone(),
        exec_mode: src_meta.exec_mode,
        fs_read_outside: src_meta.fs_read_outside,
        fs_write_outside: src_meta.fs_write_outside,
    };
    let mut rollout = Rollout::create(sessions_dir, &meta)?;
    for record in &kept {
        // 旧 Meta 行跳过：Rollout::create 已按新 meta 写入
        if matches!(record, RolloutRecord::Meta { .. }) {
            continue;
        }
        rollout.append(record);
    }
    store.lock().expect("store lock").upsert_session(&meta);
    Ok(new_id)
}

/// 截断到前 N 个回合（含第 N 回合的全部记录）
fn truncate_after_turns(records: &[RolloutRecord], turns: usize) -> &[RolloutRecord] {
    let mut user_seen = 0usize;
    let cut = records
        .iter()
        .position(|record| {
            if matches!(record, RolloutRecord::User { .. }) {
                user_seen += 1;
                user_seen > turns
            } else {
                false
            }
        })
        .unwrap_or(records.len());
    &records[..cut]
}

/// 只复制被截断记录引用的媒体文件，并把 ImageRef.path 改写进新目录
fn copy_referenced_media(
    src_media: &Path,
    new_media: &Path,
    records: &mut [RolloutRecord],
) -> Result<(), String> {
    let names: Vec<std::ffi::OsString> = records
        .iter()
        .flat_map(|record| match record {
            RolloutRecord::User { images, .. } => images.as_slice(),
            _ => &[],
        })
        .filter_map(|image| image.path.file_name().map(|n| n.to_os_string()))
        .collect();
    if names.is_empty() {
        return Ok(());
    }
    std::fs::create_dir_all(new_media).map_err(|e| format!("创建分叉媒体目录失败: {e}"))?;
    for name in &names {
        let src = src_media.join(name);
        if src.is_file() {
            std::fs::copy(&src, new_media.join(name))
                .map_err(|e| format!("复制分叉媒体 {} 失败: {e}", src.display()))?;
        }
    }
    for record in records.iter_mut() {
        let RolloutRecord::User { images, .. } = record else {
            continue;
        };
        for image in images.iter_mut() {
            if let Some(name) = image.path.file_name() {
                image.path = new_media.join(name);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn user(text: &str) -> RolloutRecord {
        RolloutRecord::User {
            text: text.to_string(),
            files: vec![],
            images: vec![],
        }
    }

    fn text(t: &str) -> RolloutRecord {
        RolloutRecord::Text {
            text: t.to_string(),
        }
    }

    #[test]
    fn truncate_keeps_through_nth_turn() {
        let records = vec![
            RolloutRecord::Meta {
                id: "s1".into(),
                cwd: PathBuf::from("/tmp"),
                title: "t".into(),
                created_at: 1,
            },
            user("问1"),
            text("答1"),
            user("问2"),
            text("答2"),
            user("问3"),
        ];
        let kept = truncate_after_turns(&records, 1);
        assert_eq!(kept.len(), 3, "Meta + 第一回合");
        let kept = truncate_after_turns(&records, 2);
        assert_eq!(kept.len(), 5, "Meta + 前两回合");
        // 超过回合总数 = 全量
        let kept = truncate_after_turns(&records, 9);
        assert_eq!(kept.len(), records.len());
    }

    #[test]
    fn media_rewrite_points_into_new_dir() {
        let tmp = std::env::temp_dir().join(format!("pig-fork-test-{}", std::process::id()));
        let src_media = tmp.join("src.media");
        let new_media = tmp.join("new.media");
        std::fs::create_dir_all(&src_media).unwrap();
        std::fs::write(src_media.join("1.png"), b"png").unwrap();
        std::fs::write(src_media.join("2.png"), "未被引用".as_bytes()).unwrap();
        let mut records = vec![RolloutRecord::User {
            text: "带图".into(),
            files: vec![],
            images: vec![crate::rollout::ImageRef {
                path: src_media.join("1.png"),
                media_type: "image/png".into(),
                width: 1,
                height: 1,
            }],
        }];
        copy_referenced_media(&src_media, &new_media, &mut records).unwrap();
        let RolloutRecord::User { images, .. } = &records[0] else {
            panic!("应是 User 记录");
        };
        assert_eq!(
            images[0].path,
            new_media.join("1.png"),
            "路径应改写进新目录"
        );
        assert_eq!(std::fs::read(new_media.join("1.png")).unwrap(), b"png");
        assert!(!new_media.join("2.png").exists(), "未引用的文件不复制");
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
