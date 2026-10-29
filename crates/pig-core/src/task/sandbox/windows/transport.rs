//! Bounded parent-to-helper transport for the Windows sandbox launch request.
//!
//! The request cannot travel on the command line: Windows caps it at 32767
//! characters and the payload may contain model command text. Instead the
//! trusted parent splits one versioned JSON document across ASCII-only
//! environment variables, and the helper removes every transport variable
//! before it creates the sandboxed process.
//!
//! Limits (mirroring Codex's launcher transport, with the tighter bounds this
//! project's handoff document specifies): payload <= 1,000,000 UTF-8 bytes,
//! chunk <= 4096 bytes, chunk count declared.

use std::collections::BTreeMap;
use std::ffi::OsString;

use serde::Deserialize;
use serde::Serialize;

use crate::task::sandbox::SandboxEntry;
use crate::task::sandbox::SandboxKind;

/// Transport variable namespace. ASCII-only by construction; the helper
/// rejects payloads that arrive through anything else.
pub(crate) const ENV_PREFIX: &str = "PIGCODE_SANDBOX_LAUNCH_";
const COUNT_KEY: &str = "PIGCODE_SANDBOX_LAUNCH_COUNT";
const BYTES_KEY: &str = "PIGCODE_SANDBOX_LAUNCH_BYTES";
const CHUNK_BYTES: usize = 4096;
const MAX_BYTES: usize = 1_000_000;
/// A chunk backs off at most 3 bytes to land on a char boundary, so every
/// non-final chunk carries at least `CHUNK_BYTES - 3` bytes.
const MAX_CHUNKS: usize = MAX_BYTES / (CHUNK_BYTES - 3) + 1;

/// Current transport/payload version. The helper rejects anything else instead
/// of guessing at an older or newer shape.
///
/// Version 2 replaced the flattened `writable_roots` / `readable_roots` /
/// `implicit_read_roots` / `ephemeral_write_roots` lists with the policy
/// itself (`kind` + `entries`), so the helper resolves the policy exactly as
/// Codex's sandbox does.
///
/// Version 4 added the access dimension to a rule group's exceptions
/// (`write_exceptions`): an older helper would drop them and keep denying the
/// excepted path, which is the safe direction but not the configured one, so
/// the version gate rejects the mismatch instead of running half a policy.
///
/// Version 5 added `credential_dirs`: an older helper would keep the built-in
/// list, which is again safe but silently ignores configuration, so the
/// version gate rejects the mismatch.
pub(crate) const VERSION: u32 = 5;

/// One launch request: the exact argv plus the abstract policy the helper
/// materializes into a native sandbox request.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub(crate) struct LaunchRequest {
    pub(crate) version: u32,
    pub(crate) program: String,
    pub(crate) args: Vec<String>,
    pub(crate) working_directory: String,
    /// The policy kind: `Restricted` (entries are the whole policy) or
    /// `Unrestricted` (the helper never sees this one; Full mode does not
    /// sandbox at all).
    pub(crate) kind: SandboxKind,
    /// Grants and denials, materialized by the helper.
    pub(crate) entries: Vec<SandboxEntry>,
    pub(crate) deny_sensitive_files: bool,
    pub(crate) network_allow: bool,
    /// The compiled read-deny rules, as scoped groups (globs + that group's own
    /// exceptions). The parent normalizes configuration against the workspace;
    /// the helper only compiles and walks. Both backends enforce the same set.
    #[serde(default)]
    pub(crate) deny_groups: Vec<crate::task::sandbox::deny_rules::DenyRuleGroup>,
    /// `[sandbox] credential_dirs`: `None` keeps the built-in list, `Some([])`
    /// turns the profile credential-directory protection off.
    #[serde(default)]
    pub(crate) credential_dirs: Option<Vec<String>>,
    /// Diagnostic: ask MXC to record the accesses it denied.
    #[serde(default)]
    /// Roots the sensitive-file walk covers: the session's normal write set,
    /// sent even for a read-only launch so both profiles produce the same deny
    /// list.
    pub(crate) deny_scan_roots: Vec<String>,
}

/// Human-readable rejection reason; never contains payload bytes.
pub(crate) type DecodeError = String;

fn is_transport_key(key: &str) -> bool {
    // `get` rather than a byte slice: an environment name is caller-controlled
    // text, and slicing at a fixed byte index panics when it lands inside a
    // multi-byte character. The helper must reject such a name, not abort.
    key.get(..ENV_PREFIX.len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(ENV_PREFIX))
}

/// Encode `request` into `env`, replacing any previous transport variables.
///
/// Existing transport variables are removed only after the new payload has
/// passed validation, so a failure cannot leave a half-written request behind.
pub(crate) fn encode(
    request: &LaunchRequest,
    env: &mut Vec<(OsString, OsString)>,
) -> std::io::Result<()> {
    let payload = serde_json::to_string(request)?;
    encode_raw(&payload, env)
}

fn encode_raw(payload: &str, env: &mut Vec<(OsString, OsString)>) -> std::io::Result<()> {
    let invalid = |message: &str| std::io::Error::new(std::io::ErrorKind::InvalidInput, message);
    if payload.is_empty() {
        return Err(invalid("sandbox launch payload is empty"));
    }
    if payload.len() > MAX_BYTES {
        return Err(invalid("sandbox launch payload exceeds the size limit"));
    }
    if payload.contains('\0') {
        return Err(invalid("sandbox launch payload contains a NUL"));
    }

    let mut chunks: Vec<&str> = Vec::new();
    let mut start = 0;
    while start < payload.len() {
        let mut end = (start + CHUNK_BYTES).min(payload.len());
        while !payload.is_char_boundary(end) {
            end -= 1;
        }
        chunks.push(&payload[start..end]);
        start = end;
    }
    if chunks.len() > MAX_CHUNKS {
        return Err(invalid("sandbox launch payload needs too many chunks"));
    }

    env.retain(|(name, _)| !name.to_str().is_some_and(is_transport_key));
    env.push((
        OsString::from(COUNT_KEY),
        OsString::from(chunks.len().to_string()),
    ));
    env.push((
        OsString::from(BYTES_KEY),
        OsString::from(payload.len().to_string()),
    ));
    for (index, chunk) in chunks.into_iter().enumerate() {
        env.push((
            OsString::from(format!("{ENV_PREFIX}{index}")),
            OsString::from(chunk),
        ));
    }
    Ok(())
}

/// Decode the request and remove every transport variable, even when decoding
/// fails, so the workload can never observe launcher state.
pub(crate) fn decode_and_scrub(
    env: &mut BTreeMap<String, String>,
) -> Result<LaunchRequest, DecodeError> {
    let decoded = decode(env);
    env.retain(|key, _| !is_transport_key(key));
    decoded
}

fn decode(env: &BTreeMap<String, String>) -> Result<LaunchRequest, DecodeError> {
    let mut count: Option<usize> = None;
    let mut bytes: Option<usize> = None;
    let mut chunks: Vec<Option<&str>> = Vec::new();
    let mut seen_transport = 0usize;

    for (key, value) in env {
        if !is_transport_key(key) {
            continue;
        }
        // Windows environment names are case-insensitive; canonicalize so
        // `pigcode_sandbox_launch_count` cannot become a second chunk source.
        let canonical = key.to_ascii_uppercase();
        seen_transport += 1;
        if seen_transport > MAX_CHUNKS + 2 {
            return Err("sandbox launch environment has too many transport variables".into());
        }
        if value.len() > CHUNK_BYTES {
            return Err("sandbox launch environment value is too long".into());
        }
        if canonical == COUNT_KEY {
            if count.replace(parse_count(&canonical, value)?).is_some() {
                return Err("duplicate sandbox launch chunk count".into());
            }
            continue;
        }
        if canonical == BYTES_KEY {
            if bytes.replace(parse_count(&canonical, value)?).is_some() {
                return Err("duplicate sandbox launch payload length".into());
            }
            continue;
        }
        let index = canonical
            .strip_prefix(ENV_PREFIX)
            .ok_or_else(|| "unknown sandbox launch variable".to_string())?;
        let index: usize = index
            .parse()
            .map_err(|_| "invalid sandbox launch chunk index".to_string())?;
        if index > MAX_CHUNKS {
            return Err("sandbox launch chunk index out of range".into());
        }
        if chunks.len() < index + 1 {
            chunks.resize(index + 1, None);
        }
        if chunks[index].replace(value.as_str()).is_some() {
            return Err("duplicate sandbox launch chunk".into());
        }
    }

    let count = count.ok_or("missing sandbox launch chunk count")?;
    let bytes = bytes.ok_or("missing sandbox launch payload length")?;
    if !(1..=MAX_CHUNKS).contains(&count) || bytes == 0 || bytes > MAX_BYTES {
        return Err("invalid sandbox launch payload size".into());
    }
    if chunks.len() != count {
        return Err("sandbox launch chunk count mismatch".into());
    }
    let mut payload = String::with_capacity(bytes);
    for chunk in chunks {
        let chunk = chunk.ok_or("missing sandbox launch chunk")?;
        if chunk.is_empty() {
            return Err("empty sandbox launch chunk".into());
        }
        payload.push_str(chunk);
    }
    if payload.len() != bytes {
        return Err("sandbox launch payload length mismatch".into());
    }

    let request: LaunchRequest =
        serde_json::from_str(&payload).map_err(|_| "invalid sandbox launch request".to_string())?;
    if request.version != VERSION {
        return Err("unsupported sandbox launch request version".into());
    }
    Ok(request)
}

fn parse_count(key: &str, value: &str) -> Result<usize, DecodeError> {
    value.parse().map_err(|_| format!("invalid {key} value"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> LaunchRequest {
        LaunchRequest {
            version: VERSION,
            program: "C:\\Program Files\\Git\\bin\\bash.exe".into(),
            args: vec!["-c".into(), "printf '%s' \"$1\"".into()],
            working_directory: "C:\\work".into(),
            kind: SandboxKind::Restricted,
            entries: vec![
                SandboxEntry::read_root(),
                SandboxEntry::write(std::path::PathBuf::from("C:\\work")),
                SandboxEntry::write(std::path::PathBuf::from("C:\\Temp")),
            ],
            deny_sensitive_files: true,
            network_allow: true,
            deny_groups: Vec::new(),
            credential_dirs: None,
            deny_scan_roots: Vec::new(),
        }
    }

    fn env_map(env: &[(OsString, OsString)]) -> BTreeMap<String, String> {
        env.iter()
            .map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    value.to_string_lossy().into_owned(),
                )
            })
            .collect()
    }

    #[test]
    fn round_trips_exact_argv_and_policy() {
        let original = request();
        let mut env = Vec::new();
        encode(&original, &mut env).expect("encode");
        let mut map = env_map(&env);
        let decoded = decode_and_scrub(&mut map).expect("decode");

        assert_eq!(decoded, original);
        assert!(map.is_empty(), "transport variables must be scrubbed");
    }

    #[test]
    fn splits_large_payloads_into_bounded_chunks() {
        let mut original = request();
        original.args = vec!["x".repeat(9000)];
        let mut env = Vec::new();
        encode(&original, &mut env).expect("encode");

        let chunks: Vec<_> = env
            .iter()
            .filter(|(key, _)| {
                let key = key.to_string_lossy();
                key.starts_with(ENV_PREFIX)
                    && !key.eq_ignore_ascii_case(COUNT_KEY)
                    && !key.eq_ignore_ascii_case(BYTES_KEY)
            })
            .collect();
        assert!(chunks.len() >= 3, "chunks: {}", chunks.len());
        for (_, value) in &chunks {
            assert!(value.len() <= CHUNK_BYTES);
        }

        let mut map = env_map(&env);
        assert_eq!(decode_and_scrub(&mut map).unwrap(), original);
    }

    #[test]
    fn replaces_stale_transport_variables() {
        let mut env = vec![
            (OsString::from("KEEP"), OsString::from("1")),
            (
                OsString::from(format!("{ENV_PREFIX}0")),
                OsString::from("stale"),
            ),
            (OsString::from(COUNT_KEY), OsString::from("9")),
        ];
        encode(&request(), &mut env).expect("encode");

        let keys: Vec<String> = env
            .iter()
            .map(|(key, _)| key.to_string_lossy().into_owned())
            .collect();
        assert_eq!(keys.iter().filter(|key| *key == COUNT_KEY).count(), 1);
        assert_eq!(keys.iter().filter(|key| *key == "KEEP").count(), 1);

        let mut map = env_map(&env);
        assert!(decode_and_scrub(&mut map).is_ok());
        assert_eq!(map.get("KEEP").map(String::as_str), Some("1"));
    }

    #[test]
    fn rejects_missing_count() {
        let mut env = Vec::new();
        encode(&request(), &mut env).unwrap();
        let mut map = env_map(&env);
        map.retain(|key, _| key != COUNT_KEY);

        assert!(decode_and_scrub(&mut map).is_err());
        assert!(map.keys().all(|key| !is_transport_key(key)));
    }

    #[test]
    fn rejects_duplicate_case_variant_chunks() {
        let mut env = Vec::new();
        encode(&request(), &mut env).unwrap();
        let mut map = env_map(&env);
        let key = format!("{ENV_PREFIX}0").to_ascii_lowercase();
        map.insert(key, "tampered".into());

        assert!(decode_and_scrub(&mut map).is_err());
    }

    #[test]
    fn rejects_truncated_and_oversized_payloads() {
        let mut env = Vec::new();
        encode(&request(), &mut env).unwrap();

        let mut truncated = env_map(&env);
        truncated.insert(BYTES_KEY.into(), "1".into());
        assert!(decode_and_scrub(&mut truncated).is_err());

        let mut oversized = env_map(&env);
        oversized.insert(BYTES_KEY.into(), (MAX_BYTES + 1).to_string());
        assert!(decode_and_scrub(&mut oversized).is_err());
    }

    #[test]
    fn rejects_unknown_version() {
        let mut request = request();
        request.version = VERSION + 1;
        let mut env = Vec::new();
        encode(&request, &mut env).unwrap();

        assert!(decode_and_scrub(&mut env_map(&env)).is_err());
    }

    #[test]
    fn rejects_malformed_json_and_nul_payloads() {
        let mut env = Vec::new();
        encode_raw("not json", &mut env).unwrap();
        assert!(decode_and_scrub(&mut env_map(&env)).is_err());

        let mut env = Vec::new();
        assert!(encode_raw("a\0b", &mut env).is_err());
    }

    #[test]
    fn rejects_chunk_count_mismatch() {
        let mut env = Vec::new();
        encode(&request(), &mut env).unwrap();
        let mut map = env_map(&env);
        map.insert(COUNT_KEY.into(), "7".into());

        assert!(decode_and_scrub(&mut map).is_err());
    }

    /// A non-ASCII environment name used to panic the prefix check by slicing
    /// at a multi-byte boundary; it must simply not be a transport variable.
    #[test]
    fn non_ascii_environment_names_are_not_transport_variables() {
        assert!(!is_transport_key("PIGCODE_SANDBOX_LAUNの"));
        assert!(!is_transport_key("变量名"));
        assert!(!is_transport_key("PIGCODE_SANDB"));
        assert!(is_transport_key("pigcode_sandbox_launch_count"));
    }
}
