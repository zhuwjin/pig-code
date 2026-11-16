//! Session fork (same semantics as ZCode's forkAssistant): derive a new session from the first
//! N turns of the source session's history — a pure conversation copy; does not touch the workspace
//! and does not interrupt the source session's in-flight turn (records are
//! appended as they become durable, and a read-only snapshot ignores an
//! in-flight final line). The boundary includes the Nth turn itself.
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
    // Read-only snapshot: a running source may be mid-append, so an
    // unterminated final line is ignored instead of being repaired here.
    let records = Rollout::load_snapshot(&src_path)?;
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
        permissions: src_meta.permissions,
        plan_enabled: false,
    };
    let mut rollout = Rollout::create(sessions_dir, &meta)?;
    for record in &kept {
        // Old Meta lines are skipped: Rollout::create already wrote the new meta
        if matches!(record, RolloutRecord::Meta { .. }) {
            continue;
        }
        rollout
            .append(record)
            .map_err(|error| error.into_core(rollout.path()))?;
    }
    rollout
        .sync()
        .map_err(|error| error.into_core(rollout.path()))?;
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
    let mut names: std::collections::BTreeSet<std::ffi::OsString> =
        std::collections::BTreeSet::new();
    for record in records.iter() {
        match record {
            RolloutRecord::User {
                images,
                sent_images,
                ..
            } => {
                for image in images.iter().chain(sent_images.iter()) {
                    if let Some(name) = image.path.file_name() {
                        names.insert(name.to_os_string());
                    }
                }
            }
            RolloutRecord::ToolCall {
                images, documents, ..
            } => {
                for image in images {
                    if let Some(name) = image.path.file_name() {
                        names.insert(name.to_os_string());
                    }
                }
                for document in documents {
                    if let Some(name) = document.path.file_name() {
                        names.insert(name.to_os_string());
                    }
                }
            }
            _ => {}
        }
    }
    let stems: std::collections::BTreeSet<String> = names
        .iter()
        .filter_map(|name| {
            let name = name.to_string_lossy();
            name.split('.').next().map(str::to_string)
        })
        .collect();
    if let Ok(entries) = std::fs::read_dir(src_media) {
        for entry in entries.filter_map(Result::ok) {
            let name = entry.file_name();
            let text = name.to_string_lossy();
            let mut parts = text.split('.');
            let stem = parts.next().unwrap_or_default();
            if parts.next() == Some("orig") && stems.contains(stem) {
                names.insert(name);
            }
        }
    }
    if !names.is_empty() {
        std::fs::create_dir_all(new_media).map_err(|e| CoreError::ForkMediaDirCreate {
            detail: e.to_string(),
        })?;
        for name in &names {
            let src = src_media.join(name);
            if src.is_file() {
                // Atomic: a crash while copying must not leave a partial media
                // file that a forked session would later treat as valid.
                let temp = new_media.join(format!(
                    ".{}.{}.tmp",
                    name.to_string_lossy(),
                    std::process::id()
                ));
                let result = std::fs::copy(&src, &temp)
                    .and_then(|_| {
                        std::fs::OpenOptions::new()
                            .read(true)
                            .write(true)
                            .open(&temp)?
                            .sync_all()
                    })
                    .and_then(|()| std::fs::rename(&temp, new_media.join(name)));
                if let Err(error) = result {
                    let _ = std::fs::remove_file(&temp);
                    return Err(CoreError::ForkMediaCopy {
                        path: src.display().to_string(),
                        detail: error.to_string(),
                    });
                }
            }
        }
    }
    let src_prefix = src_media.to_string_lossy().into_owned();
    let new_prefix = new_media.to_string_lossy().into_owned();
    for record in records.iter_mut() {
        match record {
            RolloutRecord::User {
                images,
                sent_images,
                sent_text,
                model_text,
                ..
            } => {
                for image in images.iter_mut().chain(sent_images.iter_mut()) {
                    if let Some(name) = image.path.file_name() {
                        image.path = new_media.join(name);
                    }
                }
                if let Some(text) = sent_text {
                    *text = text.replace(&src_prefix, &new_prefix);
                }
                if let Some(text) = model_text {
                    *text = text.replace(&src_prefix, &new_prefix);
                }
            }
            RolloutRecord::ToolCall {
                images, documents, ..
            } => {
                for image in images {
                    if let Some(name) = image.path.file_name() {
                        image.path = new_media.join(name);
                    }
                }
                for document in documents {
                    if let Some(name) = document.path.file_name() {
                        document.path = new_media.join(name);
                    }
                }
            }
            _ => {}
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
            sent_at_ms: 0,
            text: text.to_string(),
            files: vec![],
            images: vec![],
            sent_text: None,
            sent_images: vec![],
            model_text: None,
            sent_input_image: None,
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
                kind: crate::rollout::ConversationKind::Root,
                parent_id: None,
                delegation_depth: None,
                context_mode: None,
                inherited_record_count: None,
                profile: None,
                description: None,
                provider: None,
                model: None,
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
            sent_at_ms: 0,
            text: "with image".into(),
            files: vec![],
            images: vec![crate::rollout::ImageRef {
                path: src_media.join("1.png"),
                media_type: "image/png".into(),
                width: 1,
                height: 1,
                label: None,
            }],
            sent_text: None,
            sent_images: vec![],
            model_text: None,
            sent_input_image: None,
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

    #[test]
    fn media_rewrite_covers_sent_text_and_tool_images() {
        let tmp = std::env::temp_dir().join(format!("pig-fork-sent-media-{}", std::process::id()));
        let src_media = tmp.join("src.media");
        let new_media = tmp.join("new.media");
        std::fs::create_dir_all(&src_media).unwrap();
        std::fs::write(src_media.join("1.png"), b"user").unwrap();
        std::fs::write(src_media.join("1.orig.png"), b"original").unwrap();
        std::fs::write(src_media.join("2.png"), b"tool").unwrap();
        let image = |name: &str| crate::rollout::ImageRef {
            path: src_media.join(name),
            media_type: "image/png".into(),
            width: 1,
            height: 1,
            label: Some(name.into()),
        };
        let records = vec![
            RolloutRecord::User {
                sent_at_ms: 0,
                text: "user".into(),
                files: vec![],
                images: vec![],
                sent_text: Some(format!(
                    "image at {} and {}",
                    src_media.join("1.png").display(),
                    src_media.join("1.orig.png").display()
                )),
                sent_images: vec![image("1.png")],
                model_text: Some(format!(
                    "original at {}",
                    src_media.join("1.orig.png").display()
                )),
                sent_input_image: None,
            },
            RolloutRecord::ToolCall {
                tool: "ReadMediaFile".into(),
                summary: "image".into(),
                arguments: "{}".into(),
                output: "image".into(),
                is_error: false,
                stopped: false,
                edit: None,
                agent_card: None,
                agent_cards: vec![],
                images: vec![image("2.png")],
                documents: vec![],
                delivered_notification_ids: vec![],
            },
        ];
        let mut records = records;
        copy_referenced_media(&src_media, &new_media, &mut records).unwrap();
        let RolloutRecord::User {
            sent_text,
            sent_images,
            model_text,
            ..
        } = &records[0]
        else {
            panic!("user record");
        };
        assert_eq!(sent_images[0].path, new_media.join("1.png"));
        assert_eq!(
            sent_text.as_deref(),
            Some(
                format!(
                    "image at {} and {}",
                    new_media.join("1.png").display(),
                    new_media.join("1.orig.png").display()
                )
                .as_str()
            )
        );
        assert_eq!(
            model_text.as_deref(),
            Some(format!("original at {}", new_media.join("1.orig.png").display()).as_str())
        );
        let RolloutRecord::ToolCall { images, .. } = &records[1] else {
            panic!("tool record");
        };
        assert_eq!(images[0].path, new_media.join("2.png"));
        assert_eq!(std::fs::read(new_media.join("1.png")).unwrap(), b"user");
        assert_eq!(
            std::fs::read(new_media.join("1.orig.png")).unwrap(),
            b"original"
        );
        assert_eq!(std::fs::read(new_media.join("2.png")).unwrap(), b"tool");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn media_rewrite_copies_and_rewrites_tool_documents() {
        let tmp = std::env::temp_dir().join(format!("pig-fork-docs-{}", std::process::id()));
        let src_media = tmp.join("src.media");
        let new_media = tmp.join("new.media");
        std::fs::create_dir_all(&src_media).unwrap();
        std::fs::write(src_media.join("1.pdf"), b"%PDF-1.4\n%%EOF\n").unwrap();
        let mut records = vec![RolloutRecord::ToolCall {
            tool: "ReadMediaFile".into(),
            summary: "doc.pdf".into(),
            arguments: "{}".into(),
            output: "Read PDF doc.pdf".into(),
            is_error: false,
            stopped: false,
            edit: None,
            agent_card: None,
            agent_cards: vec![],
            images: vec![],
            documents: vec![crate::rollout::DocumentRef {
                path: src_media.join("1.pdf"),
                media_type: "application/pdf".into(),
                bytes: 13,
                token_estimate: Some(pig_provider::DocumentTokenEstimate::measured(3_000)),
                label: Some("doc.pdf".into()),
            }],
            delivered_notification_ids: vec![],
        }];
        copy_referenced_media(&src_media, &new_media, &mut records).unwrap();
        let RolloutRecord::ToolCall { documents, .. } = &records[0] else {
            panic!("tool record");
        };
        assert_eq!(documents[0].path, new_media.join("1.pdf"));
        assert_eq!(documents[0].label.as_deref(), Some("doc.pdf"));
        assert_eq!(
            std::fs::read(new_media.join("1.pdf")).unwrap(),
            b"%PDF-1.4\n%%EOF\n"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn sent_text_rewrite_does_not_require_image_refs() {
        let tmp = std::env::temp_dir().join(format!("pig-fork-text-only-{}", std::process::id()));
        let src_media = tmp.join("src.media");
        let new_media = tmp.join("new.media");
        let mut records = vec![RolloutRecord::User {
            sent_at_ms: 0,
            text: "user".into(),
            files: vec![],
            images: vec![],
            sent_text: Some(format!(
                "original at {}",
                src_media.join("1.orig.png").display()
            )),
            sent_images: vec![],
            model_text: None,
            sent_input_image: None,
        }];
        copy_referenced_media(&src_media, &new_media, &mut records).unwrap();
        let RolloutRecord::User { sent_text, .. } = &records[0] else {
            panic!("user record");
        };
        assert_eq!(
            sent_text.as_deref(),
            Some(format!("original at {}", new_media.join("1.orig.png").display()).as_str())
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
