//! Fine-grained path-aware tool concurrency scheduler (modeled on Kimi Code's
//! `ToolAccesses` conflict graph).
//!
//! Consecutive tool calls that do not require serial interception/approval and
//! whose filesystem access footprints do not conflict are grouped into one
//! concurrent [`ExecutionWave::Parallel`] wave:
//! - Read + Read (even on the same file) → concurrent
//! - Read + Search (`Glob` / `Grep`) → concurrent
//! - Write (`Write` / `Edit` / `ApplyPatch`) + Write on disjoint paths → concurrent
//! - Write + Read/Search on disjoint paths → concurrent
//! - Write + Read/Search/Write on overlapping paths (equal, ancestor, or descendant) → barrier
//! - Shell, mutating MCP/plugin tools, approval-required calls, matching hooks,
//!   Plan-mode rejections, and session-intercepted tools → [`ToolAccessPlan::Exclusive`]

use std::ops::Range;
use std::path::{Path, PathBuf};

use pig_provider::ToolCall;

use crate::tool;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PathAccessKind {
    Read,
    Write,
    Search,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PathAccess {
    pub path: PathBuf,
    pub kind: PathAccessKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ToolAccessPlan {
    /// No filesystem conflict (pure metadata/network/deferred-search/read-only MCP).
    Stateless,
    /// Declared filesystem accesses that may execute concurrently with any
    /// non-conflicting call.
    Paths(Vec<PathAccess>),
    /// Must execute alone as a serial sync barrier.
    Exclusive,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ExecutionWave {
    /// Contiguous range of mutually non-conflicting tool calls.
    Parallel(Range<usize>),
    /// One exclusive sync-point call.
    Serial(usize),
}

/// Tools that carry session-layer interception or mutation semantics and must
/// always run on the serial path.
const EXCLUSIVE_TOOLS: &[&str] = &[
    "TodoList",
    "TaskStop",
    "TaskWait",
    "AskUserQuestion",
    "EnterPlanMode",
    "ExitPlanMode",
    "Agent",
    "AgentSwarm",
    "SendMessage",
];

/// Classify one tool call's concurrency plan against current session policy.
#[allow(clippy::too_many_arguments)]
pub(crate) fn classify_tool_access(
    call: &ToolCall,
    tools: &[Box<dyn tool::Tool>],
    permissions: pig_protocol::PermissionSettings,
    plan_enabled: bool,
    media: tool::MediaCapabilities,
    permission_rules: &crate::permissions::PermissionRules,
    state: &crate::task::SessionToolState,
    cwd: &Path,
    hooks: &crate::hooks::HookSet,
) -> ToolAccessPlan {
    // Unwrap `CallTool` so concurrency classification inspects the real target
    // MCP/plugin tool (read-only runtime tools stay concurrent; mutating,
    // denied, hooked, or malformed calls remain Exclusive).
    if let Some(unwrapped) = tool::unwrap_call_tool(call) {
        let Ok((target_name, target_args)) = unwrapped else {
            return ToolAccessPlan::Exclusive;
        };
        let target_call = ToolCall {
            id: call.id.clone(),
            name: target_name,
            arguments: target_args,
        };
        if target_call.name == tool::CALL_TOOL {
            return ToolAccessPlan::Exclusive;
        }
        return classify_tool_access(
            &target_call,
            tools,
            permissions,
            plan_enabled,
            media,
            permission_rules,
            state,
            cwd,
            hooks,
        );
    }
    if EXCLUSIVE_TOOLS.contains(&call.name.as_str()) {
        return ToolAccessPlan::Exclusive;
    }
    if hooks.matches_tool(&call.name) {
        return ToolAccessPlan::Exclusive;
    }
    if call.name == "FetchURL" {
        if permissions.sandbox.mode != pig_protocol::SandboxMode::DangerFullAccess
            && crate::task::network_blocked_for(state)
        {
            return ToolAccessPlan::Exclusive;
        }
        let subject = tool::approval_subject(call);
        if permission_rules.deny_hit(&call.name, &subject).is_some() {
            return ToolAccessPlan::Exclusive;
        }
        return ToolAccessPlan::Stateless;
    }
    if tool::fs_outside_intent(state, cwd, &call.name, &call.arguments).is_some() {
        return ToolAccessPlan::Exclusive;
    }
    let targets = tool::file_tool_targets(cwd, call);
    if targets
        .iter()
        .any(|target| crate::task::credential_grant(state, target) != crate::task::CredentialGrant::Outside)
    {
        return ToolAccessPlan::Exclusive;
    }
    if call.name == "ReadMediaFile" && !media.any() {
        return ToolAccessPlan::Exclusive;
    }
    let Some(tool_ref) = tools.iter().find(|t| t.name() == call.name.as_str()) else {
        return ToolAccessPlan::Exclusive;
    };
    if tool::requires_approval(tool_ref.as_ref(), permissions) {
        return ToolAccessPlan::Exclusive;
    }
    if plan_enabled && !tool_ref.read_only() {
        return ToolAccessPlan::Exclusive;
    }

    // Runtime MCP & plugin tools: read-only tools are stateless-concurrent when
    // no deny rule matches; mutating runtime tools stay exclusive.
    if call.name.starts_with("mcp__") || tool_ref.is_plugin() {
        if !tool_ref.read_only()
            || permission_rules
                .deny_hit(&call.name, &call.name)
                .is_some()
        {
            return ToolAccessPlan::Exclusive;
        }
        return ToolAccessPlan::Stateless;
    }

    // Built-in path-bound write tools (`Write`, `Edit`, `ApplyPatch`) can join
    // concurrent waves when they target non-sensitive, non-denied workspace
    // files that do not overlap with other calls in the wave.
    if matches!(
        call.name.as_str(),
        "Write" | "Edit" | tool::APPLY_PATCH_TOOL
    ) {
        if targets.is_empty() {
            return ToolAccessPlan::Exclusive;
        }
        let subject = tool::approval_subject(call);
        if permission_rules.deny_hit(&call.name, &subject).is_some() {
            return ToolAccessPlan::Exclusive;
        }
        for target in &targets {
            if tool::is_sensitive_file(target) {
                return ToolAccessPlan::Exclusive;
            }
            if let Some(rel) = target
                .strip_prefix(cwd)
                .ok()
                .map(|p| p.to_string_lossy().replace('\\', "/"))
                && permission_rules.deny_hit(&call.name, &rel).is_some()
            {
                return ToolAccessPlan::Exclusive;
            }
        }
        return ToolAccessPlan::Paths(
            targets
                .into_iter()
                .map(|path| PathAccess {
                    path,
                    kind: PathAccessKind::Write,
                })
                .collect(),
        );
    }

    if !tool_ref.read_only() {
        return ToolAccessPlan::Exclusive;
    }

    match call.name.as_str() {
        "Read" | "ReadMediaFile" => {
            if targets.is_empty() {
                ToolAccessPlan::Stateless
            } else {
                ToolAccessPlan::Paths(
                    targets
                        .into_iter()
                        .map(|path| PathAccess {
                            path,
                            kind: PathAccessKind::Read,
                        })
                        .collect(),
                )
            }
        }
        "Glob" | "Grep" => {
            let root = targets.into_iter().next().unwrap_or_else(|| {
                cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf())
            });
            ToolAccessPlan::Paths(vec![PathAccess {
                path: root,
                kind: PathAccessKind::Search,
            }])
        }
        _ => ToolAccessPlan::Stateless,
    }
}

/// Two normalized paths overlap when they are equal or one is an ancestor of
/// the other. On Windows, comparison is ASCII case-insensitive.
fn paths_overlap(a: &Path, b: &Path) -> bool {
    let norm = |p: &Path| -> PathBuf {
        let s = p.to_string_lossy().replace('\\', "/");
        let trimmed = s.trim_end_matches('/');
        #[cfg(target_os = "windows")]
        {
            PathBuf::from(trimmed.to_ascii_lowercase())
        }
        #[cfg(not(target_os = "windows"))]
        {
            PathBuf::from(trimmed)
        }
    };
    let a = norm(a);
    let b = norm(b);
    a == b || a.starts_with(&b) || b.starts_with(&a)
}

/// True when two access plans cannot safely execute concurrently.
pub(crate) fn access_plans_conflict(a: &ToolAccessPlan, b: &ToolAccessPlan) -> bool {
    match (a, b) {
        (ToolAccessPlan::Exclusive, _) | (_, ToolAccessPlan::Exclusive) => true,
        (ToolAccessPlan::Stateless, _) | (_, ToolAccessPlan::Stateless) => false,
        (ToolAccessPlan::Paths(pa), ToolAccessPlan::Paths(pb)) => pa.iter().any(|left| {
            pb.iter().any(|right| {
                (left.kind == PathAccessKind::Write || right.kind == PathAccessKind::Write)
                    && paths_overlap(&left.path, &right.path)
            })
        }),
    }
}

/// Partition an ordered sequence of tool calls into contiguous execution waves
/// preserving original call order.
pub(crate) fn plan_execution_waves(plans: &[ToolAccessPlan]) -> Vec<ExecutionWave> {
    let mut waves = Vec::new();
    let mut i = 0usize;
    while i < plans.len() {
        if matches!(plans[i], ToolAccessPlan::Exclusive) {
            waves.push(ExecutionWave::Serial(i));
            i += 1;
            continue;
        }
        let start = i;
        let mut end = i + 1;
        while end < plans.len() {
            if matches!(plans[end], ToolAccessPlan::Exclusive) {
                break;
            }
            let conflicts = (start..end)
                .any(|prev| access_plans_conflict(&plans[prev], &plans[end]));
            if conflicts {
                break;
            }
            end += 1;
        }
        waves.push(ExecutionWave::Parallel(start..end));
        i = end;
    }
    waves
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(path: &str) -> ToolAccessPlan {
        ToolAccessPlan::Paths(vec![PathAccess {
            path: PathBuf::from(path),
            kind: PathAccessKind::Read,
        }])
    }

    fn write(path: &str) -> ToolAccessPlan {
        ToolAccessPlan::Paths(vec![PathAccess {
            path: PathBuf::from(path),
            kind: PathAccessKind::Write,
        }])
    }

    fn search(path: &str) -> ToolAccessPlan {
        ToolAccessPlan::Paths(vec![PathAccess {
            path: PathBuf::from(path),
            kind: PathAccessKind::Search,
        }])
    }

    #[test]
    fn disjoint_writes_and_reads_share_one_parallel_wave() {
        let plans = vec![
            read("/ws/a.rs"),
            write("/ws/b.rs"),
            write("/ws/c.rs"),
            read("/ws/d.rs"),
        ];
        assert_eq!(
            plan_execution_waves(&plans),
            vec![ExecutionWave::Parallel(0..4)]
        );
    }

    #[test]
    fn overlapping_write_and_read_split_into_ordered_waves() {
        let plans = vec![
            read("/ws/a.rs"),
            write("/ws/a.rs"),
            read("/ws/a.rs"),
        ];
        assert_eq!(
            plan_execution_waves(&plans),
            vec![
                ExecutionWave::Parallel(0..1),
                ExecutionWave::Parallel(1..2),
                ExecutionWave::Parallel(2..3),
            ]
        );
    }

    #[test]
    fn search_under_ancestor_dir_conflicts_with_write_inside_it() {
        let plans = vec![
            search("/ws/src"),
            write("/ws/src/lib.rs"),
            read("/ws/README.md"),
        ];
        assert_eq!(
            plan_execution_waves(&plans),
            vec![
                ExecutionWave::Parallel(0..1),
                ExecutionWave::Parallel(1..3),
            ]
        );
    }

    #[test]
    fn exclusive_calls_create_serial_barriers() {
        let plans = vec![
            read("/ws/a.rs"),
            ToolAccessPlan::Exclusive,
            write("/ws/b.rs"),
            write("/ws/c.rs"),
        ];
        assert_eq!(
            plan_execution_waves(&plans),
            vec![
                ExecutionWave::Parallel(0..1),
                ExecutionWave::Serial(1),
                ExecutionWave::Parallel(2..4),
            ]
        );
    }

    #[test]
    fn multi_file_patch_conflicts_only_when_paths_overlap() {
        let patch_ab = ToolAccessPlan::Paths(vec![
            PathAccess {
                path: PathBuf::from("/ws/a.rs"),
                kind: PathAccessKind::Write,
            },
            PathAccess {
                path: PathBuf::from("/ws/b.rs"),
                kind: PathAccessKind::Write,
            },
        ]);
        assert!(!access_plans_conflict(&patch_ab, &write("/ws/c.rs")));
        assert!(access_plans_conflict(&patch_ab, &read("/ws/b.rs")));
    }
}
