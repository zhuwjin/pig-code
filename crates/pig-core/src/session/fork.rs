//! Session fork (same semantics as ZCode's forkAssistant): derive a new session from the first
//! N turns of the source session's history — a pure conversation copy; does not touch the workspace
//! and does not interrupt the source session's in-flight turn (rollout flushes every line, so
//! reading from disk is a consistent snapshot). The boundary includes the Nth turn itself.
//!
//! Explicitly out of scope for v1 (see docs/PLAN.md): `.agents/` subagent contexts are not copied
//! (the absolute record/result paths in task notifications still point into the source session's
//! directory; they remain openable while the source exists); the `.model-io.jsonl` debug trace is
//! not copied; the parent session gets no fork marker; the child session has no "derived from
//! conversation" jump-back row (identified by the " (fork)" title suffix).

use std::path::Path;
use std::sync::{Arc, Mutex};

use pig_protocol::{CoreError, SessionMeta};

use crate::rollout::{Rollout, RolloutRecord, now_secs};
use crate::store::Store;
use pig_utils::media_dir;

/// Derive a new session and persist it, returning the new session id (the caller opens it via the OpenSession cold path).
/// `turns` = number of turns to keep: truncation happens before the (N+1)-th User record (TurnStats/
/// TurnChanges/StepUsage/Compact are naturally kept with their turns; ToolCall records carry their
/// own receipts, so there is no pairing problem). turns=0 is clamped to 1; exceeding the source's total turn count = full copy.
pub fn fork_session(
    sessions_dir: &Path,
    store: &Arc<Mutex<Store>>,
    src_id: &str,
    turns: usize,
    id_counter: &mut u64,
) -> Result<String, CoreError> {
    let src_path = sessions_dir.join(format!("{src_id}.jsonl"));
    let records = Rollout::load(&src_path)?;
    let src_meta = store
        .lock()
        .expect("store lock")
        .get_session(src_id)
        .ok_or_else(|| CoreError::ForkSourceMissing {
            id: src_id.to_string(),
        })?;

    let turns = turns.max(1);
    let mut kept = truncate_after_turns(&records, turns).to_vec();

    *id_counter += 1;
    let new_id = format!("s{}-{}", now_secs(), id_counter);

    // Images are absolute paths containing the source session id ({sessions}/{src}.media/N.ext):
    // copy the referenced files into the new media directory and rewrite the records, so forked images stay usable after the source session is deleted
    let src_media = media_dir(sessions_dir, src_id);
    let new_media = media_dir(sessions_dir, &new_id);
    copy_referenced_media(&src_media, &new_media, &mut kept)?;

    let meta = SessionMeta {
        id: new_id.clone(),
        // The fork title is persisted data (written into the sessions table/rollout meta) with a fixed English suffix;
        // an empty title (new-session seed) gets no suffix, preserving the empty-string sentinel semantics
        title: if src_meta.title.is_empty() {
            String::new()
        } else {
            format!("{} (fork)", src_meta.title)
        },
        // Fork titles are fixed: auto-naming does not override them
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
        plan_enabled: false,
        fs_read_outside: src_meta.fs_read_outside,
        fs_write_outside: src_meta.fs_write_outside,
    };
    let mut rollout = Rollout::create(sessions_dir, &meta)?;
    for record in &kept {
        // Old Meta lines are skipped: Rollout::create already wrote the new meta
        if matches!(record, RolloutRecord::Meta { .. }) {
            continue;
        }
        rollout.append(record);
    }
    store.lock().expect("store lock").upsert_session(&meta);
    Ok(new_id)
}

/// Truncate to the first N turns (including all records of the Nth turn)
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

/// Copy only the media files referenced by the kept records, rewriting ImageRef.path into the new directory
fn copy_referenced_media(
    src_media: &Path,
    new_media: &Path,
    records: &mut [RolloutRecord],
) -> Result<(), CoreError> {
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
    std::fs::create_dir_all(new_media).map_err(|e| CoreError::ForkMediaDirCreate {
        detail: e.to_string(),
    })?;
    for name in &names {
        let src = src_media.join(name);
        if src.is_file() {
            std::fs::copy(&src, new_media.join(name)).map_err(|e| CoreError::ForkMediaCopy {
                path: src.display().to_string(),
                detail: e.to_string(),
            })?;
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
            user("q1"),
            text("a1"),
            user("q2"),
            text("a2"),
            user("q3"),
        ];
        let kept = truncate_after_turns(&records, 1);
        assert_eq!(kept.len(), 3, "Meta + first turn");
        let kept = truncate_after_turns(&records, 2);
        assert_eq!(kept.len(), 5, "Meta + first two turns");
        // Exceeding the total turn count = full copy
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
        std::fs::write(src_media.join("2.png"), "unreferenced".as_bytes()).unwrap();
        let mut records = vec![RolloutRecord::User {
            text: "with image".into(),
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
            panic!("should be a User record");
        };
        assert_eq!(
            images[0].path,
            new_media.join("1.png"),
            "path should be rewritten into the new directory"
        );
        assert_eq!(std::fs::read(new_media.join("1.png")).unwrap(), b"png");
        assert!(
            !new_media.join("2.png").exists(),
            "unreferenced files are not copied"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
