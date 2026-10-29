use std::path::{Path, PathBuf};

use pig_protocol::{AppConfig, CoreError, SandboxDenyGroup};

use super::persistence::{harden_default_data_dir, write_private_file};

pub fn default_path() -> PathBuf {
    pig_utils::data_dir().join("config.toml")
}

/// The sensitive-file deny groups a fresh install starts with.
///
/// These were hardcoded rules in `task::sandbox::deny_rules` until 2026-10-20.
/// They are now ordinary `[[sandbox.deny]]` configuration: [`ensure_seeded_deny_rules`]
/// writes them into the user's own config on first launch, and from then on they
/// are edited or deleted there like any other rule (deleting a group is how a
/// user switches that protection off — a repository of example `.env` files is
/// the usual reason).
///
/// They mirror the names the Read/Write/Edit tools refuse
/// (`tool::is_sensitive_file`), expressed as path globs so both sandbox backends
/// share one matching vocabulary.
pub fn default_deny_groups() -> Vec<SandboxDenyGroup> {
    let group = |globs: &[&str], exceptions: &[&str]| SandboxDenyGroup {
        globs: globs.iter().map(|glob| glob.to_string()).collect(),
        exceptions: exceptions.iter().map(|glob| glob.to_string()).collect(),
        write_exceptions: Vec::new(),
    };
    vec![
        group(
            &["**/.env", "**/.env.*"],
            &["**/.env.example", "**/.env.sample", "**/.env.template"],
        ),
        // Private-key names with the `id_rsa-*` / `id_rsa_*` / `id_rsa.*`
        // variants; public keys are exempted.
        group(
            &[
                "**/id_rsa",
                "**/id_rsa.*",
                "**/id_rsa-*",
                "**/id_rsa_*",
                "**/id_ed25519",
                "**/id_ed25519.*",
                "**/id_ed25519-*",
                "**/id_ed25519_*",
                "**/id_ecdsa",
                "**/id_ecdsa.*",
                "**/id_ecdsa-*",
                "**/id_ecdsa_*",
                "**/id_dsa",
                "**/id_dsa.*",
                "**/id_dsa-*",
                "**/id_dsa_*",
            ],
            &["**/*.pub"],
        ),
        group(&["**/.aws/credentials", "**/.gcp/credentials"], &[]),
    ]
}

/// Seed the sensitive-file deny groups into the user's config file.
///
/// Runs at agent startup **before the config is loaded**, on every platform
/// (the rule table is platform-neutral: macOS enforces it through Seatbelt,
/// Windows through the walker). Seeding lazily — on the first save — would leave
/// a fresh install unprotected until then.
///
/// The key is the marker: `[sandbox] deny` absent means "never seeded" and gets
/// [`default_deny_groups`]; present — including an empty list — means the user
/// owns the table now, so the file is left alone. That is what makes deleting
/// every entry a real off switch.
///
/// Idempotent and minimal about the rest: the file is parsed and written back
/// with only that key inserted. A file that cannot be parsed is left untouched
/// (the load path reports the real error instead of this one clobbering it).
/// The write is atomic and the file is private (0600 on Unix).
///
/// Returns whether the file was written.
pub fn ensure_seeded_deny_rules(path: &Path) -> Result<bool, CoreError> {
    harden_default_data_dir(path);
    let mut root = match std::fs::read_to_string(path) {
        Ok(raw) => raw
            .parse::<toml::Value>()
            .map_err(|error| CoreError::ConfigParseFile {
                path: path.display().to_string(),
                detail: error.to_string(),
            })?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            // A fresh install has no file at all: start from the app defaults,
            // so the written file is a complete, loadable configuration (a bare
            // `[sandbox]` table would fail the load — `providers` is required).
            toml::Value::try_from(AppConfig::default()).map_err(|error| {
                CoreError::ConfigSerialize {
                    detail: error.to_string(),
                }
            })?
        }
        Err(error) => {
            return Err(CoreError::ConfigRead {
                path: path.display().to_string(),
                detail: error.to_string(),
            });
        }
    };
    let Some(table) = root.as_table_mut() else {
        // The root of a TOML document is always a table; nothing to seed into.
        return Ok(false);
    };
    if let Some(sandbox) = table.get("sandbox") {
        let Some(sandbox) = sandbox.as_table() else {
            return Ok(false);
        };
        if sandbox.contains_key("deny") {
            return Ok(false);
        }
    }
    let seeded = toml::Value::try_from(default_deny_groups()).map_err(|error| {
        CoreError::ConfigSerialize {
            detail: error.to_string(),
        }
    })?;
    let sandbox = table
        .entry("sandbox".to_string())
        .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
    let Some(sandbox) = sandbox.as_table_mut() else {
        return Ok(false);
    };
    sandbox.insert("deny".to_string(), seeded);
    let raw = toml::to_string_pretty(&root).map_err(|error| CoreError::ConfigSerialize {
        detail: error.to_string(),
    })?;
    write_private_file(path, raw.as_bytes())?;
    harden_default_data_dir(path);
    tracing::info!(
        "seeded the sensitive-file deny groups into {}",
        path.display()
    );
    Ok(true)
}
