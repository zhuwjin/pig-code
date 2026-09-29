use super::*;

/// agents-state.json：运行时可写的子代理覆盖（设置页改子代理模型用）
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct AgentsState {
    #[serde(default)]
    model_overrides: HashMap<String, ModelOverride>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct ModelOverride {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    thought_level: Option<String>,
}

pub(crate) fn state_path(data_dir: &Path) -> PathBuf {
    data_dir.join("agents-state.json")
}

/// 文件不存在/解析失败 = 空 state（不致命）
pub(crate) fn load_agents_state(data_dir: &Path) -> AgentsState {
    let Ok(raw) = std::fs::read_to_string(state_path(data_dir)) else {
        return AgentsState::default();
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

/// 读-改-写 agents-state.json：两个键都给 None 时删除该 name 条目。
/// 写前 create_dir_all，序列化 pretty。设置页 UI 用。
pub fn set_model_override(
    data_dir: &Path,
    name: &str,
    model: Option<&str>,
    thought_level: Option<&str>,
) -> Result<(), String> {
    let mut state = load_agents_state(data_dir);
    // 空白串按 None 处理
    let model = model.map(str::trim).filter(|m| !m.is_empty());
    let thought_level = thought_level.map(str::trim).filter(|l| !l.is_empty());
    match (model, thought_level) {
        (None, None) => {
            state.model_overrides.remove(name);
        }
        (model, thought_level) => {
            state.model_overrides.insert(
                name.to_string(),
                ModelOverride {
                    model: model.map(str::to_string),
                    thought_level: thought_level.map(str::to_string),
                },
            );
        }
    }
    std::fs::create_dir_all(data_dir)
        .map_err(|e| format!("创建数据目录失败 {}: {e}", data_dir.display()))?;
    let raw = serde_json::to_string_pretty(&state)
        .map_err(|e| format!("序列化 agents-state 失败: {e}"))?;
    std::fs::write(state_path(data_dir), raw)
        .map_err(|e| format!("写入 agents-state.json 失败: {e}"))
}

/// 加载全部子代理档案：内置 → 用户级 → 项目级，按 name 后者覆盖前者；
/// 再应用 agents-state.json 的模型覆盖；最终按 name 排序输出。
pub fn load_profiles(cwd: &Path, data_dir: &Path) -> Vec<AgentProfile> {
    let mut by_name: HashMap<String, AgentProfile> = HashMap::new();
    for profile in builtin_profiles() {
        by_name.insert(profile.name.clone(), profile);
    }
    // 目录优先级：项目级 > 用户级 > 内置；同名整体替换
    for (dir, source) in [
        (data_dir.join("agents"), AgentSource::User),
        (cwd.join(".pigcode").join("agents"), AgentSource::Project),
    ] {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue; // 目录不存在 = 空
        };
        let mut files: Vec<PathBuf> = entries
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "md"))
            .collect();
        files.sort(); // 同目录内按文件名稳定顺序，行为可预测
        for path in files {
            let Ok(content) = std::fs::read_to_string(&path) else {
                continue; // 读取失败跳过，不致命
            };
            match parse_agent_markdown(&content) {
                Ok(mut profile) => {
                    profile.source = source;
                    by_name.insert(profile.name.clone(), profile);
                }
                Err(e) => {
                    eprintln!("[agent] 跳过无法解析的子代理档案 {}: {e}", path.display());
                }
            }
        }
    }
    // state 覆盖：仅对「frontmatter 未显式指定 model」的档案生效（内置必然生效），
    // 因此在覆盖合并完成后统一应用——用户/项目档案同名替换内置也不会丢覆盖。
    let state = load_agents_state(data_dir);
    for (name, profile) in by_name.iter_mut() {
        if profile.model.is_some() {
            continue;
        }
        if let Some(override_) = state.model_overrides.get(name) {
            if let Some(model) = override_.model.as_ref().filter(|m| !m.trim().is_empty()) {
                profile.model = Some(model.clone());
            }
            if let Some(level) = override_
                .thought_level
                .as_ref()
                .filter(|l| !l.trim().is_empty())
            {
                profile.thought_level = Some(level.clone());
            }
        }
    }
    let mut profiles: Vec<AgentProfile> = by_name.into_values().collect();
    profiles.sort_by(|a, b| a.name.cmp(&b.name));
    profiles
}

/// 归一化：小写 + 去空白/破折号/下划线（"My Agent" ≈ "my-agent" ≈ "my_agent"）
pub(crate) fn normalize_name(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_whitespace() && *c != '-' && *c != '_')
        .flat_map(char::to_lowercase)
        .collect()
}
