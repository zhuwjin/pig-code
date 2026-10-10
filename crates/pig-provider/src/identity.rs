// ---------------- Stainless-style client identity headers ----------------
// Model gateways fingerprint standard SDK clients (the Stainless-generated `openai` /
// `@anthropic-ai/sdk` npm packages) via User-Agent and the X-Stainless-* telemetry rows;
// a bare reqwest client sends neither, which reads as non-SDK traffic to WAFs and
// aggregators. We present the same header set a stock SDK client would send (verified
// against openai@7.19.0 / @anthropic-ai/sdk@0.129.0 wire behavior; see docs/PLAN.md).

/// The runtime we present (X-Stainless-Runtime / -Runtime-Version): the node version
/// the SDK would typically run on
const SDK_RUNTIME: &str = "node";
const SDK_RUNTIME_VERSION: &str = "v22.11.0";

/// The SDK package version we present per API format (X-Stainless-Package-Version)
const SDK_VERSION_OPENAI: &str = "7.19.0";
const SDK_VERSION_ANTHROPIC: &str = "0.129.0";

/// X-Stainless-Timeout we report: the SDK's default 10-minute request timeout
const SDK_TIMEOUT_SECS: u32 = 600;

/// The thinking beta the Anthropic SDK attaches when interleaved thinking is enabled
/// (pi sends the same one via params.betas -> anthropic-beta)
const ANTHROPIC_INTERLEAVED_THINKING_BETA: &str = "interleaved-thinking-2025-05-14";

/// pi-style user agent: `Pig Code (darwin 27.0.0; arm64)` — node's process.platform /
/// os.release() / process.arch vocabulary
fn sdk_user_agent() -> &'static str {
    static UA: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    UA.get_or_init(|| {
        let platform = match std::env::consts::OS {
            "macos" => "darwin",
            "windows" => "win32",
            other => other,
        };
        let arch = match std::env::consts::ARCH {
            "aarch64" => "arm64",
            "x86_64" => "x64",
            "x86" => "x32",
            "arm" => "arm",
            other => other,
        };
        format!("Pig Code ({platform} {}; {arch})", os_release())
    })
}

/// X-Stainless-OS value (Stainless platform normalization)
fn stainless_os() -> &'static str {
    static OS: std::sync::OnceLock<&'static str> = std::sync::OnceLock::new();
    OS.get_or_init(|| match std::env::consts::OS {
        "macos" => "MacOS",
        "windows" => "Windows",
        "linux" => "Linux",
        _ => "Unknown",
    })
}

/// X-Stainless-Arch value (Stainless arch normalization)
fn stainless_arch() -> &'static str {
    static ARCH: std::sync::OnceLock<&'static str> = std::sync::OnceLock::new();
    ARCH.get_or_init(|| match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "x64",
        "x86" => "x32",
        "arm" => "arm",
        _ => "other",
    })
}

/// Kernel/OS release for the user agent: the Darwin kernel version on macOS (node's
/// os.release() source), the kernel release on Linux, major.minor.build on Windows;
/// "unknown" when the OS API fails
fn os_release() -> String {
    #[cfg(unix)]
    {
        // SAFETY: uname writes into a local zeroed struct and reads no shared state
        unsafe {
            let mut uts: libc::utsname = std::mem::zeroed();
            if libc::uname(&mut uts) == 0 {
                let bytes = uts
                    .release
                    .iter()
                    .take_while(|c| **c != 0)
                    .map(|c| *c as u8)
                    .collect::<Vec<_>>();
                if let Ok(release) = String::from_utf8(bytes)
                    && !release.is_empty()
                {
                    return release;
                }
            }
        }
    }
    #[cfg(windows)]
    {
        // SAFETY: RtlGetVersion writes into a local struct and reads no shared state;
        // unlike GetVersionEx it is not subject to manifest-based version lies
        unsafe {
            let mut info: windows::Win32::System::SystemInformation::OSVERSIONINFOW =
                std::mem::zeroed();
            info.dwOSVersionInfoSize = std::mem::size_of_val(&info) as u32;
            // windows-rs 0.57 moved RtlGetVersion to the Wdk namespace (the
            // Win32::System::SystemInformation path used before never existed
            // and only compiled on targets where this cfg(windows) block is
            // compiled out)
            if windows::Wdk::System::SystemServices::RtlGetVersion(&mut info).is_ok() {
                return format!(
                    "{}.{}.{}",
                    info.dwMajorVersion, info.dwMinorVersion, info.dwBuildNumber
                );
            }
        }
    }
    "unknown".to_string()
}

/// The SDK-mimicking header rows shared by both API formats (name, value), plus the
/// Anthropic-only extras. Pure so tests can assert the shape without issuing requests.
fn sdk_header_rows(anthropic: bool, thinking: bool, retry: u32) -> Vec<(&'static str, String)> {
    let mut rows = vec![
        ("user-agent", sdk_user_agent().to_string()),
        ("accept", "application/json".to_string()),
        ("x-stainless-lang", "js".to_string()),
        (
            "x-stainless-package-version",
            if anthropic {
                SDK_VERSION_ANTHROPIC
            } else {
                SDK_VERSION_OPENAI
            }
            .to_string(),
        ),
        ("x-stainless-os", stainless_os().to_string()),
        ("x-stainless-arch", stainless_arch().to_string()),
        ("x-stainless-runtime", SDK_RUNTIME.to_string()),
        (
            "x-stainless-runtime-version",
            SDK_RUNTIME_VERSION.to_string(),
        ),
        ("x-stainless-retry-count", retry.to_string()),
        ("x-stainless-timeout", SDK_TIMEOUT_SECS.to_string()),
    ];
    if anthropic {
        // pi sets this on every Anthropic client (dangerouslyAllowBrowser) even on desktop
        rows.push(("anthropic-dangerous-direct-browser-access", "true".into()));
        if thinking {
            rows.push(("anthropic-beta", ANTHROPIC_INTERLEAVED_THINKING_BETA.into()));
        }
    }
    rows
}

/// Apply [`sdk_header_rows`] to a request builder (auth/content-type stay with the caller:
/// bearer_auth / x-api-key and .json() keep their existing per-format handling)
pub(crate) fn apply_sdk_headers(
    builder: reqwest::RequestBuilder,
    anthropic: bool,
    thinking: bool,
    retry: u32,
) -> reqwest::RequestBuilder {
    let mut builder = builder;
    for (name, value) in sdk_header_rows(anthropic, thinking, retry) {
        builder = builder.header(name, value);
    }
    builder
}

#[cfg(test)]
mod tests {
    use super::{sdk_header_rows, sdk_user_agent};

    /// SDK identity rows carry the telemetry set a stock Stainless client sends, with the
    /// per-format package version, the request's retry count, and the Anthropic-only
    /// extras (browser-access always, interleaved-thinking beta only with thinking)
    #[test]
    fn sdk_header_rows_match_sdk_wire_shape() {
        let openai = sdk_header_rows(false, false, 0);
        let get = |rows: &[(&str, String)], name: &str| {
            rows.iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| v.clone())
                .unwrap_or_else(|| panic!("missing header {name}"))
        };
        assert_eq!(get(&openai, "x-stainless-package-version"), "7.19.0");
        assert_eq!(get(&openai, "x-stainless-retry-count"), "0");
        assert_eq!(get(&openai, "accept"), "application/json");
        assert_eq!(get(&openai, "x-stainless-runtime"), "node");
        assert_eq!(get(&openai, "x-stainless-runtime-version"), "v22.11.0");
        assert_eq!(get(&openai, "x-stainless-timeout"), "600");
        assert!(
            openai
                .iter()
                .all(|(k, _)| *k != "anthropic-dangerous-direct-browser-access"),
            "openai format must not carry anthropic-only headers"
        );

        let anthropic = sdk_header_rows(true, false, 3);
        assert_eq!(get(&anthropic, "x-stainless-package-version"), "0.129.0");
        assert_eq!(get(&anthropic, "x-stainless-retry-count"), "3");
        assert_eq!(
            get(&anthropic, "anthropic-dangerous-direct-browser-access"),
            "true"
        );
        assert!(
            anthropic.iter().all(|(k, _)| *k != "anthropic-beta"),
            "without thinking there is no anthropic-beta"
        );

        let thinking = sdk_header_rows(true, true, 0);
        assert_eq!(
            get(&thinking, "anthropic-beta"),
            "interleaved-thinking-2025-05-14"
        );
    }

    /// The user agent follows pi's node vocabulary: `Pig Code ({platform} {release}; {arch})`
    #[test]
    fn sdk_user_agent_follows_pi_shape() {
        let ua = sdk_user_agent();
        assert!(
            ua.starts_with("Pig Code ("),
            "unexpected product prefix: {ua}"
        );
        assert!(ua.ends_with(')'), "unbalanced paren: {ua}");
        let inner = &ua["Pig Code (".len()..ua.len() - 1];
        let mut parts = inner.split("; ");
        let platform_release = parts.next().unwrap_or_default();
        let arch = parts.next().unwrap_or_default();
        assert!(
            platform_release.split(' ').count() == 2,
            "expected `<platform> <release>`: {platform_release:?} in {ua}"
        );
        assert!(!arch.is_empty(), "empty arch in {ua}");
        // Stainless rows must be consistent with the UA's environment
        let rows = sdk_header_rows(false, false, 0);
        let stainless_arch = rows
            .iter()
            .find(|(k, _)| *k == "x-stainless-arch")
            .map(|(_, v)| v.as_str())
            .unwrap();
        assert_eq!(
            stainless_arch, arch,
            "arch mismatch between UA and x-stainless-arch in {ua}"
        );
    }
}
