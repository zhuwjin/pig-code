use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

#[derive(Deserialize)]
struct Matrix {
    references: BTreeMap<String, Reference>,
    module_inventory: BTreeMap<String, ModuleEntry>,
    acceptance_rows: Vec<AcceptanceRow>,
    #[serde(rename = "final")]
    final_delivery: FinalDelivery,
}

#[derive(Deserialize)]
struct Reference {
    sha: String,
}

#[derive(Deserialize)]
struct ModuleEntry {
    paths: Vec<String>,
    acceptance: Vec<String>,
    public_surface: Vec<String>,
}

#[derive(Deserialize)]
struct AcceptanceRow {
    id: String,
    reference_semantics: String,
    pig_invariants: Vec<String>,
    automated_tests: Vec<String>,
    references: Vec<RowReference>,
    existing_evidence: Vec<String>,
    missing_proof: Vec<String>,
    status: RowStatus,
}

#[derive(Deserialize)]
struct RowReference {
    project: String,
    path: String,
}

#[derive(Deserialize)]
struct FinalDelivery {
    status: RowStatus,
    commit: Option<String>,
    local_commands: BTreeMap<String, Option<String>>,
    ci: BTreeMap<String, Option<String>>,
    issue: String,
}

#[derive(Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum RowStatus {
    Unimplemented,
    Implemented,
    Tested,
    Verified,
}

fn load_matrix() -> (PathBuf, Matrix) {
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let bytes = fs::read(workspace.join(".pigcode/core-architecture-acceptance.json"))
        .expect("read core architecture acceptance matrix");
    let matrix = serde_json::from_slice(&bytes).expect("parse core architecture acceptance matrix");
    (workspace, matrix)
}

fn is_commit(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn production_sources(workspace: &Path) -> Vec<(String, String)> {
    fn visit(workspace: &Path, directory: &Path, output: &mut Vec<(String, String)>) {
        for entry in fs::read_dir(directory).expect("read source directory") {
            let path = entry.expect("source entry").path();
            if path.is_dir() {
                if !matches!(
                    path.file_name().and_then(|name| name.to_str()),
                    Some("tests" | "mock")
                ) {
                    visit(workspace, &path, output);
                }
                continue;
            }
            if path.extension().and_then(|extension| extension.to_str()) != Some("rs") {
                continue;
            }
            let file_name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default();
            if file_name == "tests.rs"
                || file_name.ends_with("_tests.rs")
                || file_name.ends_with("_test.rs")
            {
                continue;
            }
            let relative = path
                .strip_prefix(workspace)
                .expect("workspace-relative source")
                .to_string_lossy()
                .replace('\\', "/");
            let source = fs::read_to_string(&path).expect("read Rust source");
            let production = source
                .split("\n#[cfg(test)]")
                .next()
                .unwrap_or_default()
                .to_string();
            output.push((relative, production));
        }
    }
    let mut sources = Vec::new();
    for crate_name in ["pig-core", "pig-provider"] {
        let root = workspace.join("crates").join(crate_name).join("src");
        visit(workspace, &root, &mut sources);
    }
    sources
}

fn core_production_sources(workspace: &Path) -> Vec<(String, String)> {
    production_sources(workspace)
        .into_iter()
        .filter_map(|(path, source)| {
            path.strip_prefix("crates/pig-core/src/")
                .map(|path| (path.to_string(), source))
        })
        .collect()
}

fn top_level_entry(path: &str) -> String {
    Path::new(path)
        .strip_prefix("crates/pig-core/src")
        .expect("module path stays in pig-core src")
        .components()
        .next()
        .expect("module path has a top-level entry")
        .as_os_str()
        .to_string_lossy()
        .into_owned()
}

#[test]
fn matrix_covers_every_core_entry_and_existing_path() {
    let (workspace, matrix) = load_matrix();
    let source = workspace.join("crates/pig-core/src");
    let actual: BTreeSet<String> = fs::read_dir(&source)
        .expect("read pig-core source entries")
        .map(|entry| {
            entry
                .expect("read pig-core source entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    let mut covered = BTreeSet::new();
    for (name, entry) in &matrix.module_inventory {
        assert!(!entry.paths.is_empty(), "{name} has no source paths");
        assert!(!entry.acceptance.is_empty(), "{name} has no acceptance row");
        for path in &entry.paths {
            assert!(workspace.join(path).exists(), "missing module path: {path}");
            covered.insert(top_level_entry(path));
        }
    }
    assert_eq!(covered, actual, "core module inventory must be exhaustive");
    for row in &matrix.acceptance_rows {
        assert!(
            !row.reference_semantics.trim().is_empty(),
            "{} has no concrete reference semantics",
            row.id
        );
        assert!(
            !row.pig_invariants.is_empty()
                && row
                    .pig_invariants
                    .iter()
                    .all(|item| !item.trim().is_empty()),
            "{} has no Pig invariants",
            row.id
        );
        assert!(
            !row.automated_tests.is_empty()
                && row
                    .automated_tests
                    .iter()
                    .all(|item| !item.trim().is_empty()),
            "{} has no named automated tests",
            row.id
        );
        for evidence in &row.existing_evidence {
            assert!(
                workspace.join(evidence).exists(),
                "{} references missing evidence: {evidence}",
                row.id
            );
        }
    }
}

#[test]
fn public_modules_and_acceptance_links_are_complete() {
    let (workspace, matrix) = load_matrix();
    let lib = fs::read_to_string(workspace.join("crates/pig-core/src/lib.rs"))
        .expect("read pig-core crate root");
    let public_modules: BTreeSet<String> = lib
        .lines()
        .filter_map(|line| line.strip_prefix("pub mod "))
        .filter_map(|line| line.strip_suffix(';'))
        .map(str::to_string)
        .collect();
    let inventoried: BTreeSet<String> = matrix
        .module_inventory
        .values()
        .flat_map(|entry| entry.public_surface.iter())
        .filter(|surface| public_modules.contains(*surface))
        .cloned()
        .collect();
    assert_eq!(
        inventoried, public_modules,
        "every public module must be classified"
    );

    let row_ids: BTreeSet<String> = matrix
        .acceptance_rows
        .iter()
        .map(|row| row.id.clone())
        .collect();
    assert_eq!(
        row_ids.len(),
        matrix.acceptance_rows.len(),
        "acceptance IDs must be unique"
    );
    for (name, entry) in &matrix.module_inventory {
        for id in &entry.acceptance {
            assert!(
                row_ids.contains(id),
                "{name} references unknown acceptance row {id}"
            );
        }
    }
    for (project, reference) in &matrix.references {
        assert!(
            is_commit(&reference.sha),
            "{project} must be pinned to a full commit SHA"
        );
    }
    for row in &matrix.acceptance_rows {
        for reference in &row.references {
            assert!(
                matrix.references.contains_key(&reference.project),
                "{} references unknown project {}",
                row.id,
                reference.project
            );
            let path = Path::new(&reference.path);
            assert!(
                !reference.path.is_empty() && !path.is_absolute(),
                "invalid reference path"
            );
            assert!(
                path.components().all(|component| matches!(
                    component,
                    std::path::Component::Normal(_) | std::path::Component::CurDir
                )),
                "reference path must stay inside its repository: {}",
                reference.path
            );
        }
    }
}

#[test]
fn verified_status_requires_complete_delivery_evidence() {
    let (_, matrix) = load_matrix();
    for row in &matrix.acceptance_rows {
        if row.status == RowStatus::Verified {
            assert!(
                !row.existing_evidence.is_empty(),
                "{} has no evidence",
                row.id
            );
            assert!(
                row.missing_proof.is_empty(),
                "{} still has missing proof",
                row.id
            );
        }
    }
    if matrix.final_delivery.status != RowStatus::Verified {
        return;
    }
    assert!(
        matrix
            .acceptance_rows
            .iter()
            .all(|row| row.status == RowStatus::Verified),
        "final delivery cannot pass before every acceptance row"
    );
    let commit = matrix
        .final_delivery
        .commit
        .as_deref()
        .expect("verified delivery needs a final commit");
    assert!(
        commit == "HEAD" || is_commit(commit),
        "final delivery needs HEAD or a full commit SHA"
    );
    for command in [
        "cargo check --all-targets",
        "cargo test --workspace",
        "nsh check && nsh test",
    ] {
        assert_eq!(
            matrix
                .final_delivery
                .local_commands
                .get(command)
                .and_then(Option::as_deref),
            Some("exit 0"),
            "missing successful command evidence: {command}"
        );
    }
    for platform in ["macos", "windows", "linux"] {
        let proof = matrix
            .final_delivery
            .ci
            .get(platform)
            .and_then(Option::as_deref)
            .unwrap_or_default();
        assert!(
            proof.starts_with("https://"),
            "missing CI evidence for {platform}"
        );
    }
    assert!(
        matrix.final_delivery.issue.starts_with("https://"),
        "issue evidence is required"
    );
}

#[test]
fn legacy_execute_control_plane_stays_removed() {
    let (workspace, _) = load_matrix();
    for path in [
        "crates/pig-core/src/agent/execution.rs",
        "crates/pig-core/src/agent/backend.rs",
        "crates/pig-core/src/agent/ipc.rs",
    ] {
        let source =
            fs::read_to_string(workspace.join(path)).expect("read execution control source");
        for forbidden in [
            "fn execute(",
            ".execute(",
            "execute_reserved",
            "execute_inner",
            "AgentExecutionActivation::Resume",
        ] {
            assert!(
                !source.contains(forbidden),
                "legacy execute-only control returned in {path}: {forbidden}"
            );
        }
    }
}

#[test]
fn production_tools_cannot_bypass_the_permission_gate() {
    let (workspace, _) = load_matrix();
    for (path, source) in core_production_sources(&workspace) {
        if path == "session/gate.rs" {
            continue;
        }
        for forbidden in [
            "tool::execute(",
            "tool::execute_with_extra(",
            "crate::tool::execute(",
            "crate::tool::execute_with_extra(",
        ] {
            assert!(
                !source.contains(forbidden),
                "{path} bypasses the permission gate with {forbidden}"
            );
        }
    }
}

#[test]
fn session_has_no_backend_or_provider_special_cases() {
    let (workspace, _) = load_matrix();
    for (path, source) in core_production_sources(&workspace) {
        if !path.starts_with("session/") {
            continue;
        }
        for forbidden in [
            "IpcAgentExecutionHandle",
            "ApiFormat::",
            "provider_id ==",
            "provider_id.starts_with",
        ] {
            assert!(
                !source.contains(forbidden),
                "{path} contains a backend/provider special case: {forbidden}"
            );
        }
    }
}

#[test]
fn domain_modules_cannot_own_runtime_control_state() {
    let (workspace, _) = load_matrix();
    for (path, source) in core_production_sources(&workspace) {
        if !(path.starts_with("plugins/")
            || path.starts_with("hooks/")
            || path == "tool.rs"
            || path.starts_with("tool/"))
        {
            continue;
        }
        for forbidden in ["AgentRuntime", "RuntimeState", "ExecutionTable"] {
            assert!(
                !source.contains(forbidden),
                "{path} owns or mutates runtime control state through {forbidden}"
            );
        }
    }
}

#[test]
fn runtime_command_capacity_is_bounded() {
    let (workspace, _) = load_matrix();
    for (path, source) in core_production_sources(&workspace) {
        if !path.starts_with("agent/runtime/") {
            continue;
        }
        assert!(
            !source.contains("mpsc::unbounded_channel"),
            "{path} introduces an unbounded runtime command queue"
        );
    }
    let control =
        fs::read_to_string(workspace.join("crates/pig-core/src/agent/runtime/control.rs"))
            .expect("runtime control source");
    assert!(
        control.contains("mpsc::channel(256)"),
        "the runtime must have an explicit command capacity"
    );
}

#[test]
fn catalog_recovery_uses_the_runtime_authority() {
    let (workspace, _) = load_matrix();
    let source = fs::read_to_string(workspace.join("crates/pig-core/src/session/catalog.rs"))
        .expect("catalog source");
    assert!(source.contains("runtime_snapshot()"));
    assert!(!source.contains("std::fs::read_dir"));
    assert!(!source.contains("read_agent("));
    assert!(!source.contains("recover_lost_child("));
}

#[test]
fn provider_event_queues_are_bounded_in_production() {
    let (workspace, _) = load_matrix();
    let mut checked = 0;
    for (path, source) in production_sources(&workspace) {
        if !path.starts_with("crates/pig-provider/src/") {
            continue;
        }
        checked += 1;
        assert!(
            !source.contains("mpsc::unbounded_channel")
                && !source.contains("async_channel::unbounded"),
            "{path} introduces an unbounded provider event queue"
        );
    }
    assert!(checked > 0, "provider production sources must be scanned");
}

#[test]
fn task_notification_failures_are_observable() {
    let (workspace, _) = load_matrix();
    for (path, source) in core_production_sources(&workspace) {
        if path.starts_with("task/") {
            for forbidden in [
                "let _ = self.task_notify.send_blocking",
                "let _ = self.state.task_notify.send_blocking",
                "let _ = state.task_notify.send_blocking",
                "let _ = notify.send_blocking",
            ] {
                assert!(
                    !source.contains(forbidden),
                    "{path} silently ignores task notification failure"
                );
            }
        }
    }
}

#[test]
fn durable_history_has_a_shared_non_destructive_quota() {
    let (workspace, _) = load_matrix();
    for path in [
        "crates/pig-core/src/rollout/mod.rs",
        "crates/pig-core/src/agent/records/conversation.rs",
        "crates/pig-core/src/session/persistence.rs",
    ] {
        let source = fs::read_to_string(workspace.join(path)).expect("history source");
        assert!(
            source.contains("HistoryLimitExceeded")
                || source.contains("ensure_history_capacity")
                || source.contains("durable_history_failure"),
            "{path} must participate in the bounded-history contract"
        );
    }
}
