//! 手动项目列表持久化：{data_dir}/projects.json

use std::path::{Path, PathBuf};

use pig_protocol::ProjectMeta;

use crate::rollout::now_secs;

pub struct ProjectStore {
    path: PathBuf,
    pub projects: Vec<ProjectMeta>,
}

impl ProjectStore {
    pub fn load(data_dir: &Path) -> Self {
        let path = data_dir.join("projects.json");
        let projects = std::fs::read_to_string(&path)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default();
        Self { path, projects }
    }

    fn save(&self) {
        let _ = std::fs::create_dir_all(self.path.parent().expect("data dir"));
        if let Ok(raw) = serde_json::to_string_pretty(&self.projects) {
            let _ = std::fs::write(&self.path, raw);
        }
    }

    /// 幂等：已存在则忽略。
    pub fn add(&mut self, path: PathBuf) {
        if self.projects.iter().any(|p| p.path == path) {
            return;
        }
        self.projects.push(ProjectMeta {
            path,
            added_at: now_secs(),
        });
        self.save();
    }

    pub fn remove(&mut self, path: &Path) {
        self.projects.retain(|p| p.path != path);
        self.save();
    }
}
