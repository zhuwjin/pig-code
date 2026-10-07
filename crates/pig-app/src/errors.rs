//! Core structured errors (pig_protocol::CoreError) → localized single-line text.
//!
//! Architecture convention (Clean/Hexagonal): core is language-neutral; errors
//! carry semantics in the enum and the verbatim diagnostic in the English detail
//! (passed through from upstream); localization is deferred to UI render points,
//! uniformly via this module's `core_error_text`. The final shape is "localized
//! message: English detail" — the detail is a verifiable appendix of the original
//! text, never translated.

use pig_protocol::CoreError;

/// CoreError → user-visible text: a t!("errors.*") localized message plus the
/// detail interpolated inline verbatim. Unit variants are plain text; Unknown (the
/// forward-compatibility fallback when deserialization meets an unknown kind) gets
/// a generic message.
pub fn core_error_text(e: &CoreError) -> String {
    core_error_text_in(e, &rust_i18n::locale())
}

/// Mapping implementation for a given locale: production uses the process's
/// current locale; tests fetch strings directly per locale for assertions without
/// flipping the process-global locale (avoiding cross-pollution with concurrent
/// test threads).
fn core_error_text_in(e: &CoreError, locale: &str) -> String {
    match e {
        CoreError::ConfigParse { detail } => {
            rust_i18n::t!("errors.config_parse", locale = locale, e = detail).into_owned()
        }
        CoreError::ConfigRead { path, detail } => rust_i18n::t!(
            "errors.config_read",
            locale = locale,
            path = path,
            e = detail
        )
        .into_owned(),
        CoreError::ConfigParseFile { path, detail } => rust_i18n::t!(
            "errors.config_parse_file",
            locale = locale,
            path = path,
            e = detail
        )
        .into_owned(),
        CoreError::ConfigSerialize { detail } => {
            rust_i18n::t!("errors.config_serialize", locale = locale, e = detail).into_owned()
        }
        CoreError::ConfigWrite { path, detail } => rust_i18n::t!(
            "errors.config_write",
            locale = locale,
            path = path,
            e = detail
        )
        .into_owned(),
        CoreError::ForkSourceMissing { id } => {
            rust_i18n::t!("errors.fork_source_missing", locale = locale, id = id).into_owned()
        }
        CoreError::ForkMediaDirCreate { detail } => {
            rust_i18n::t!("errors.fork_media_dir_create", locale = locale, e = detail).into_owned()
        }
        CoreError::ForkMediaCopy { path, detail } => rust_i18n::t!(
            "errors.fork_media_copy",
            locale = locale,
            path = path,
            e = detail
        )
        .into_owned(),
        CoreError::SessionNotFoundOpen => {
            rust_i18n::t!("errors.session_not_found_open", locale = locale).into_owned()
        }
        CoreError::SessionNotFound => {
            rust_i18n::t!("errors.session_not_found", locale = locale).into_owned()
        }
        CoreError::NoModelConfigured => {
            rust_i18n::t!("errors.no_model_configured", locale = locale).into_owned()
        }
        CoreError::CompactUnavailable => {
            rust_i18n::t!("errors.compact_unavailable", locale = locale).into_owned()
        }
        CoreError::ProviderNotFound => {
            rust_i18n::t!("errors.provider_not_found", locale = locale).into_owned()
        }
        CoreError::RevertTurnInFlight => {
            rust_i18n::t!("errors.revert_turn_in_flight", locale = locale).into_owned()
        }
        CoreError::RevertNotModified => {
            rust_i18n::t!("errors.revert_not_modified", locale = locale).into_owned()
        }
        CoreError::RevertWrite { path, detail } => rust_i18n::t!(
            "errors.revert_write",
            locale = locale,
            path = path,
            e = detail
        )
        .into_owned(),
        CoreError::RevertDelete { path, detail } => rust_i18n::t!(
            "errors.revert_delete",
            locale = locale,
            path = path,
            e = detail
        )
        .into_owned(),
        CoreError::GitTask { detail } => {
            rust_i18n::t!("errors.git_task", locale = locale, e = detail).into_owned()
        }
        CoreError::SubagentRead { detail } => {
            rust_i18n::t!("errors.subagent_read", locale = locale, e = detail).into_owned()
        }
        CoreError::SubagentReadTask { detail } => {
            rust_i18n::t!("errors.subagent_read_task", locale = locale, e = detail).into_owned()
        }
        CoreError::Network { detail } => {
            rust_i18n::t!("errors.network", locale = locale, e = detail).into_owned()
        }
        CoreError::StreamRead { detail } => {
            rust_i18n::t!("errors.stream_read", locale = locale, e = detail).into_owned()
        }
        CoreError::RolloutNoMeta { id } => {
            rust_i18n::t!("errors.rollout_no_meta", locale = locale, id = id).into_owned()
        }
        CoreError::SessionsDirCreate { detail } => {
            rust_i18n::t!("errors.sessions_dir_create", locale = locale, e = detail).into_owned()
        }
        CoreError::RolloutCreate { path, detail } => rust_i18n::t!(
            "errors.rollout_create",
            locale = locale,
            path = path,
            e = detail
        )
        .into_owned(),
        CoreError::RolloutOpen { path, detail } => rust_i18n::t!(
            "errors.rollout_open",
            locale = locale,
            path = path,
            e = detail
        )
        .into_owned(),
        CoreError::RolloutRead { path, detail } => rust_i18n::t!(
            "errors.rollout_read",
            locale = locale,
            path = path,
            e = detail
        )
        .into_owned(),
        CoreError::RolloutLineParse { detail } => {
            rust_i18n::t!("errors.rollout_line_parse", locale = locale, e = detail).into_owned()
        }
        CoreError::DataDirCreate { path, detail } => rust_i18n::t!(
            "errors.data_dir_create",
            locale = locale,
            path = path,
            e = detail
        )
        .into_owned(),
        CoreError::SkillsStateSerialize { detail } => {
            rust_i18n::t!("errors.skills_state_serialize", locale = locale, e = detail).into_owned()
        }
        CoreError::SkillsStateWrite { detail } => {
            rust_i18n::t!("errors.skills_state_write", locale = locale, e = detail).into_owned()
        }
        CoreError::GitSpawn { detail } => {
            rust_i18n::t!("errors.git_spawn", locale = locale, e = detail).into_owned()
        }
        CoreError::GitCheckout { detail } => {
            rust_i18n::t!("errors.git_checkout", locale = locale, e = detail).into_owned()
        }
        CoreError::McpInitialize { detail } => {
            rust_i18n::t!("errors.mcp.initialize", locale = locale, e = detail).into_owned()
        }
        CoreError::McpSpawn { command, detail } => rust_i18n::t!(
            "errors.mcp.spawn",
            locale = locale,
            command = command,
            e = detail
        )
        .into_owned(),
        CoreError::McpNotifyWrite {
            name,
            method,
            detail,
        } => rust_i18n::t!(
            "errors.mcp.notify_write",
            locale = locale,
            name = name,
            method = method,
            e = detail
        )
        .into_owned(),
        CoreError::McpInvalidUrl { detail } => {
            rust_i18n::t!("errors.mcp.invalid_url", locale = locale, e = detail).into_owned()
        }
        CoreError::McpHeaderBlocked { key } => {
            rust_i18n::t!("errors.mcp.header_blocked", locale = locale, key = key).into_owned()
        }
        CoreError::McpHeaderNameInvalid { key } => {
            rust_i18n::t!("errors.mcp.header_name_invalid", locale = locale, key = key).into_owned()
        }
        CoreError::McpHeaderValueInvalid { key } => rust_i18n::t!(
            "errors.mcp.header_value_invalid",
            locale = locale,
            key = key
        )
        .into_owned(),
        CoreError::PermissionsParse { detail } => {
            rust_i18n::t!("errors.permissions_parse", locale = locale, e = detail).into_owned()
        }
        CoreError::Internal { detail } => {
            rust_i18n::t!("errors.internal", locale = locale, e = detail).into_owned()
        }
        CoreError::Unknown => rust_i18n::t!("errors.unknown", locale = locale).into_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Samples of every CoreError variant (one representative value per variant):
    /// when a new variant misses its mapping, this list fails to compile
    /// (core_error_text_in's match has no wildcard arm); here the runtime texts
    /// are each exercised too.
    fn all_variants() -> Vec<CoreError> {
        let d = || "upstream detail".to_string();
        let p = || "/tmp/x".to_string();
        vec![
            CoreError::ConfigParse { detail: d() },
            CoreError::ConfigRead {
                path: p(),
                detail: d(),
            },
            CoreError::ConfigParseFile {
                path: p(),
                detail: d(),
            },
            CoreError::ConfigSerialize { detail: d() },
            CoreError::ConfigWrite {
                path: p(),
                detail: d(),
            },
            CoreError::ForkSourceMissing { id: "s1".into() },
            CoreError::ForkMediaDirCreate { detail: d() },
            CoreError::ForkMediaCopy {
                path: p(),
                detail: d(),
            },
            CoreError::SessionNotFoundOpen,
            CoreError::SessionNotFound,
            CoreError::NoModelConfigured,
            CoreError::CompactUnavailable,
            CoreError::ProviderNotFound,
            CoreError::RevertTurnInFlight,
            CoreError::RevertNotModified,
            CoreError::RevertWrite {
                path: p(),
                detail: d(),
            },
            CoreError::RevertDelete {
                path: p(),
                detail: d(),
            },
            CoreError::GitTask { detail: d() },
            CoreError::SubagentRead { detail: d() },
            CoreError::SubagentReadTask { detail: d() },
            CoreError::Network { detail: d() },
            CoreError::StreamRead { detail: d() },
            CoreError::RolloutNoMeta { id: "s1".into() },
            CoreError::SessionsDirCreate { detail: d() },
            CoreError::RolloutCreate {
                path: p(),
                detail: d(),
            },
            CoreError::RolloutOpen {
                path: p(),
                detail: d(),
            },
            CoreError::RolloutRead {
                path: p(),
                detail: d(),
            },
            CoreError::RolloutLineParse { detail: d() },
            CoreError::DataDirCreate {
                path: p(),
                detail: d(),
            },
            CoreError::SkillsStateSerialize { detail: d() },
            CoreError::SkillsStateWrite { detail: d() },
            CoreError::GitSpawn { detail: d() },
            CoreError::GitCheckout { detail: d() },
            CoreError::McpInitialize { detail: d() },
            CoreError::McpSpawn {
                command: "npx mcp-x".into(),
                detail: d(),
            },
            CoreError::McpNotifyWrite {
                name: "fs".into(),
                method: "notifications/initialized".into(),
                detail: d(),
            },
            CoreError::McpInvalidUrl { detail: d() },
            CoreError::McpHeaderBlocked {
                key: "Authorization".into(),
            },
            CoreError::McpHeaderNameInvalid {
                key: "X Bad".into(),
            },
            CoreError::McpHeaderValueInvalid {
                key: "X-Key".into(),
            },
            CoreError::PermissionsParse { detail: d() },
            CoreError::Internal { detail: d() },
            CoreError::Unknown,
        ]
    }

    /// Smoke: every variant × {zh-CN, en} fetched once — non-empty, with no
    /// unreplaced %{ interpolation leftovers.
    #[test]
    fn all_variants_localize_in_both_languages() {
        for e in all_variants() {
            for locale in ["zh-CN", "en"] {
                let text = core_error_text_in(&e, locale);
                assert!(!text.is_empty(), "{locale} text for {e:?} is empty");
                assert!(
                    !text.contains("%{"),
                    "{locale} text for {e:?} has unreplaced interpolation: {text}"
                );
            }
        }
    }

    /// Needle assertions on representative variants: localized message + English
    /// detail inlined verbatim.
    #[test]
    fn representative_variants_carry_expected_text() {
        let network = CoreError::Network {
            detail: "connection refused".into(),
        };
        let zh = core_error_text_in(&network, "zh-CN");
        assert!(
            zh.contains("网络错误"),
            "zh Network should contain \"网络错误\": {zh}"
        );
        assert!(
            zh.contains("connection refused"),
            "detail should be inlined verbatim: {zh}"
        );
        let en = core_error_text_in(&network, "en");
        assert!(
            en.contains("Network error"),
            "en Network should contain \"Network error\": {en}"
        );

        let config = CoreError::ConfigParse {
            detail: "bad toml".into(),
        };
        let zh = core_error_text_in(&config, "zh-CN");
        assert!(zh.contains("配置解析失败"), "zh ConfigParse needle: {zh}");
        assert!(
            zh.contains("~/.pigcode/config.toml"),
            "should carry the fix hint: {zh}"
        );

        // Unit variants are plain text; Unknown is the fallback
        assert_eq!(
            core_error_text_in(&CoreError::SessionNotFound, "zh-CN"),
            "会话不存在"
        );
        assert_eq!(core_error_text_in(&CoreError::Unknown, "zh-CN"), "未知错误");
        assert_eq!(
            core_error_text_in(&CoreError::Unknown, "en"),
            "Unknown error"
        );

        // MCP variants go through errors.mcp.* (command/name/method multi-param
        // interpolation)
        let spawn = CoreError::McpSpawn {
            command: "npx mcp-x".into(),
            detail: "exit 1".into(),
        };
        let zh = core_error_text_in(&spawn, "zh-CN");
        assert!(
            zh.contains("启动失败") && zh.contains("npx mcp-x") && zh.contains("exit 1"),
            "{zh}"
        );
    }
}
