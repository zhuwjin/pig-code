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
    assert!(is_commit(commit), "final delivery needs a full commit SHA");
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
