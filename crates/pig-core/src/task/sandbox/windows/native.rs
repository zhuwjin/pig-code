//! Native launch through Microsoft MXC's process security environment (PSEC).
//!
//! PSEC is the Windows tier: it enforces read, write and deny lists in the
//! kernel while leaving host ACLs alone, which is what `docs/WINDOWS_SANDBOX.md`
//! requires. Its contract is not enabled on every Windows build, so availability
//! is probed at run time (`availability::is_available`, plus
//! `BaseContainerRunner::supports_native_denied_paths` when the policy carries
//! denies) and never inferred from a build number. A host that fails the probe
//! fails closed: model Bash is not started unsandboxed.

use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;

use mxc_sdk::mxc_common::cmdline::CommandLineContext;
use mxc_sdk::mxc_common::cmdline::cmdline_from_argv_for_context;
use mxc_sdk::mxc_common::logger::Logger;
use mxc_sdk::mxc_common::logger::Mode;
use mxc_sdk::mxc_common::models::BaseProcessUiConfig;
use mxc_sdk::mxc_common::models::ClipboardPolicy;
use mxc_sdk::mxc_common::models::ContainerPolicy;
use mxc_sdk::mxc_common::models::ExecutionRequest;
use mxc_sdk::mxc_common::models::FallbackPolicy;
use mxc_sdk::mxc_common::models::LifecycleConfig;
use mxc_sdk::mxc_common::models::NetworkAction;
use mxc_sdk::mxc_common::models::NetworkEgressPolicy;
use mxc_sdk::mxc_common::models::NetworkIngressPolicy;
use mxc_sdk::mxc_common::models::UiPolicy;
use mxc_sdk::mxc_common::sandbox_process::SandboxBackend;
use mxc_sdk::mxc_common::sandbox_process::StdioMode;
use mxc_sdk::process_container_common::base_container_runner::BaseContainerRunner;

use super::policy::LaunchPlan;

/// Whether the host can create a native process security environment.
pub(crate) fn process_security_environment_usable() -> bool {
    super::availability::is_available()
}

/// Whether the host's PSEC can enforce concrete denied paths.
pub(crate) fn deny_paths_supported() -> bool {
    BaseContainerRunner::supports_native_denied_paths()
}

/// Launch the workload behind the sandbox and return its exit code.
pub(crate) fn launch(plan: &LaunchPlan, env: &BTreeMap<String, String>) -> Result<i32, String> {
    ensure_available(plan)?;
    let execution = build_request(plan, env)?;
    launch_with_process_security_environment(&execution)
}

/// Fail closed with an English reason when the host cannot run the sandbox.
fn ensure_available(plan: &LaunchPlan) -> Result<(), String> {
    if !process_security_environment_usable() {
        return Err(
            "this Windows build cannot create a process security environment, so the command \
             cannot be sandboxed; refusing to run it unsandboxed (see docs/WINDOWS_SANDBOX.md)"
                .to_string(),
        );
    }
    if !plan.denied_paths.is_empty() && !deny_paths_supported() {
        return Err(
            "this Windows build's process security environment cannot enforce denied paths, \
             which the sensitive-file policy requires; refusing to run the command unsandboxed"
                .to_string(),
        );
    }
    Ok(())
}

fn build_request(
    plan: &LaunchPlan,
    env: &BTreeMap<String, String>,
) -> Result<ExecutionRequest, String> {
    let mut argv = Vec::with_capacity(plan.args.len() + 1);
    argv.push(plan.program.clone());
    argv.extend(plan.args.iter().cloned());
    let script_code =
        cmdline_from_argv_for_context(&argv, CommandLineContext::WindowsCreateProcess)
            .map_err(|error| format!("cannot build the sandboxed command line: {error}"))?;
    let working_directory = unicode_path(&plan.working_directory)?;
    // The network policy is two directional defaults: `Block` denies both
    // directions (and host loopback with them), `Allow` leaves both open. The
    // old single `default_network_policy` + `allow_local_network` pair no longer
    // exists in the SDK's policy.
    let network_action = if plan.network_allow {
        NetworkAction::Allow
    } else {
        NetworkAction::Deny
    };
    // The parent's environment, passed verbatim. `None` would let the backend
    // substitute its own default block, which is not what a model command sees;
    // an empty list is rejected by the Windows process container.
    let env: Vec<String> = env
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect();

    Ok(ExecutionRequest {
        container_id: container_name(),
        env: (!env.is_empty()).then_some(env),
        script_code,
        working_directory,
        // 0 = wait for the workload; the pig-core task registry owns timeouts
        // and cancellation, and a second timer here would race it.
        script_timeout: 0,
        policy: normalize_policy(ContainerPolicy {
            capabilities: vec!["registryRead".to_string()],
            readwrite_paths: unicode_paths(&plan.readwrite_paths)?,
            readonly_paths: unicode_paths(&plan.all_readonly_paths())?,
            denied_paths: unicode_paths(&plan.denied_paths)?,
            // This backend never mutates host ACLs; a host that would need the
            // DACL fallback fails closed instead.
            fallback: FallbackPolicy {
                allow_dacl_mutation: false,
            },
            network_egress: Some(NetworkEgressPolicy {
                default: network_action,
                ..Default::default()
            }),
            network_ingress: Some(NetworkIngressPolicy {
                default: network_action,
                host_loopback: network_action,
            }),
            network_specified: true,
            network_mode_specified: true,
            // PowerShell/cmd need Win32k while their DLLs start; clipboard,
            // input injection and desktop control stay restricted.
            ui: UiPolicy {
                disable: false,
                clipboard: ClipboardPolicy::None,
                injection: false,
            },
            base_process_ui: BaseProcessUiConfig {
                isolation: "desktop".to_string(),
                ..Default::default()
            },
            ..Default::default()
        })?,
        lifecycle: LifecycleConfig {
            destroy_on_exit: true,
            preserve_policy: false,
        },
        ..Default::default()
    })
}

/// Merge aliases that resolve to the same object before the policy reaches MXC.
///
/// The metadata carve-outs make this matter: a workspace directory can be
/// reachable through two spellings (a junction, a differently-cased path), and
/// MXC resolves a same-object conflict by tightening both entries to the most
/// restrictive intent. Without this call a read-write grant and a read-only
/// carve-out for the same object reach the backend as independent entries whose
/// order decides the outcome. It also fails the launch when a path cannot be
/// inspected at all while denied entries are present, which is what we want.
fn normalize_policy(policy: ContainerPolicy) -> Result<ContainerPolicy, String> {
    let mut logger = Logger::new(Mode::Buffer);
    mxc_sdk::mxc_common::filesystem_object::normalize_object_conflicts(&policy, &mut logger)
        .map_err(|error| format!("the sandbox policy could not be normalized: {error}"))?
        .map_or(Ok(policy), Ok)
}

fn launch_with_process_security_environment(request: &ExecutionRequest) -> Result<i32, String> {
    const MAX_CREATE_ATTEMPTS: usize = 3;
    for attempt in 0..MAX_CREATE_ATTEMPTS {
        let mut logger = Logger::new(Mode::Buffer);
        let mut runner = BaseContainerRunner::new();
        let _launch_guard = psec_launch_guard()?;
        match runner.spawn(request, &mut logger, StdioMode::Inherit) {
            Ok(mut child) => {
                drop(_launch_guard);
                return child
                    .wait()
                    .map_err(|error| format!("the sandboxed command could not be run: {error}"));
            }
            Err(response) => {
                let retryable = transient_psec_creation_error(&response.error_message);
                if !retryable || attempt + 1 == MAX_CREATE_ATTEMPTS {
                    return Err(format!(
                        "the Windows sandbox refused to launch: {}",
                        response.error_message
                    ));
                }
                // This SDK error is returned by PSEC creation, before the
                // workload is launched, so retrying cannot repeat a side effect.
                tracing::warn!(
                    attempt = attempt + 1,
                    "transient PSEC creation failure; retrying"
                );
                std::thread::sleep(std::time::Duration::from_millis(25 * (attempt as u64 + 1)));
            }
        }
    }
    unreachable!("the final PSEC attempt either returns a child or an error")
}

struct PsecLaunchGuard(windows::Win32::Foundation::HANDLE);

impl Drop for PsecLaunchGuard {
    fn drop(&mut self) {
        use windows::Win32::Foundation::CloseHandle;
        use windows::Win32::System::Threading::ReleaseMutex;
        unsafe {
            let _ = ReleaseMutex(self.0);
            let _ = CloseHandle(self.0);
        }
    }
}

fn psec_launch_guard() -> Result<PsecLaunchGuard, String> {
    use windows::Win32::Foundation::{WAIT_ABANDONED, WAIT_OBJECT_0, WAIT_TIMEOUT};
    use windows::Win32::System::Threading::{CreateMutexW, WaitForSingleObject};
    use windows::core::PCWSTR;

    let name: Vec<u16> = "Local\\pig-code-psec-launch-v1"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let handle = unsafe { CreateMutexW(None, false, PCWSTR(name.as_ptr())) }
        .map_err(|error| format!("cannot create the PSEC launch mutex: {error}"))?;
    let result = unsafe { WaitForSingleObject(handle, 10_000) };
    if result == WAIT_OBJECT_0 || result == WAIT_ABANDONED {
        return Ok(PsecLaunchGuard(handle));
    }
    unsafe {
        let _ = windows::Win32::Foundation::CloseHandle(handle);
    }
    if result == WAIT_TIMEOUT {
        Err("timed out waiting for another PSEC launch".to_string())
    } else {
        Err(format!(
            "failed to acquire the PSEC launch mutex: {result:?}"
        ))
    }
}

fn transient_psec_creation_error(error_message: &str) -> bool {
    error_message.contains("CreateProcessSecurityEnvironment failed (HRESULT = 0x80070003)")
}

/// Whether this very process can write `directory`, for `--probe`: a process
/// that can write its working directory but no user directory is confined by an
/// outer sandbox (for example a hosting terminal that sandboxes workspace
/// binaries), which is the usual reason the sandbox cannot initialise.
pub(crate) fn directory_writable(directory: &Path) -> bool {
    let probe = directory.join(format!("write-probe-{}", std::process::id()));
    std::fs::write(&probe, b"probe").is_ok() && std::fs::remove_file(&probe).is_ok()
}

/// AppContainer profile names allow letters, digits, `.`, `-` and `_`.
fn container_name() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    format!("pigcode-{}-{nanos}", std::process::id())
}

fn unicode_path(path: &Path) -> Result<String, String> {
    path.to_str()
        .map(str::to_string)
        .ok_or_else(|| "the Windows sandbox requires Unicode policy paths".to_string())
}

fn unicode_paths(paths: &[PathBuf]) -> Result<Vec<String>, String> {
    paths.iter().map(|path| unicode_path(path)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn container_names_are_unique_and_profile_safe() {
        let first = container_name();
        let second = container_name();
        assert_ne!(first, second);
        assert!(
            first
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')),
            "{first}"
        );
        assert!(first.len() < 64);
    }

    #[test]
    fn psec_retry_is_limited_to_the_pre_launch_transient_error() {
        assert!(transient_psec_creation_error(
            "failed to create the process security environment: CreateProcessSecurityEnvironment failed (HRESULT = 0x80070003)"
        ));
        assert!(!transient_psec_creation_error("permission denied"));
        assert!(!transient_psec_creation_error(
            "the sandboxed command could not be run: HRESULT = 0x80070003"
        ));
    }

    #[test]
    fn build_request_carries_exact_argv_and_policy() {
        let plan = LaunchPlan {
            program: "C:\\Program Files\\WindowsPowerShell\\v1.0\\powershell.exe".into(),
            args: vec![
                "-NoLogo".into(),
                "-Command".into(),
                "Get-Content 'a b'".into(),
            ],
            working_directory: std::env::temp_dir(),
            readwrite_paths: vec![std::env::temp_dir()],
            readonly_paths: Vec::new(),
            platform_read_paths: Vec::new(),
            optional_read_paths: Vec::new(),
            denied_paths: Vec::new(),
            metadata_readonly_paths: Vec::new(),
            network_allow: true,
        };
        let request = build_request(&plan, &BTreeMap::new()).expect("request");

        assert!(request.script_code.contains("powershell.exe"));
        assert!(request.script_code.contains("Get-Content"));
        assert_eq!(egress_default(&request), NetworkAction::Allow);
        assert_eq!(request.script_timeout, 0);
        assert!(
            !request.policy.fallback.allow_dacl_mutation,
            "this backend never mutates host ACLs"
        );
        assert!(!request.policy.ui.disable);
        assert!(!request.policy.ui.injection);
    }

    /// The network policy reaches MXC in both directions: an allowed profile
    /// leaves egress and ingress open (host loopback included), a blocked one
    /// denies both.
    #[test]
    fn network_policy_is_carried_to_the_native_request() {
        let plan = LaunchPlan {
            program: "cmd.exe".into(),
            args: vec!["/C".into(), "exit 0".into()],
            working_directory: std::env::temp_dir(),
            readwrite_paths: vec![std::env::temp_dir()],
            readonly_paths: Vec::new(),
            platform_read_paths: Vec::new(),
            optional_read_paths: Vec::new(),
            denied_paths: Vec::new(),
            metadata_readonly_paths: Vec::new(),
            network_allow: false,
        };
        let request = build_request(&plan, &BTreeMap::new()).expect("request");
        assert_eq!(egress_default(&request), NetworkAction::Deny);
        assert_eq!(ingress_default(&request), NetworkAction::Deny);
        assert_eq!(
            host_loopback(&request),
            NetworkAction::Deny,
            "a blocked network must not leave host loopback open"
        );

        let mut allowed = plan;
        allowed.network_allow = true;
        let request = build_request(&allowed, &BTreeMap::new()).expect("request");
        assert_eq!(egress_default(&request), NetworkAction::Allow);
        assert_eq!(ingress_default(&request), NetworkAction::Allow);
        assert_eq!(host_loopback(&request), NetworkAction::Allow);
    }

    fn egress_default(request: &ExecutionRequest) -> NetworkAction {
        request
            .policy
            .network_egress
            .as_ref()
            .expect("egress policy is always set")
            .default
    }

    fn ingress_default(request: &ExecutionRequest) -> NetworkAction {
        request
            .policy
            .network_ingress
            .as_ref()
            .expect("ingress policy is always set")
            .default
    }

    fn host_loopback(request: &ExecutionRequest) -> NetworkAction {
        request
            .policy
            .network_ingress
            .as_ref()
            .expect("ingress policy is always set")
            .host_loopback
    }

    /// The read-only profile reaches the native policy as an empty writable
    /// list, and every other list (read-only, deny) is still populated.
    #[test]
    fn read_only_profile_sends_no_writable_paths() {
        let plan = LaunchPlan {
            program: "cmd.exe".into(),
            args: vec!["/C".into(), "exit 0".into()],
            working_directory: std::env::temp_dir(),
            readwrite_paths: Vec::new(),
            readonly_paths: vec![std::env::temp_dir()],
            platform_read_paths: Vec::new(),
            optional_read_paths: Vec::new(),
            denied_paths: vec![std::env::temp_dir().join("secret")],
            metadata_readonly_paths: Vec::new(),
            network_allow: true,
        };
        let request = build_request(&plan, &BTreeMap::new()).expect("request");
        assert!(request.policy.readwrite_paths.is_empty());
        assert!(!request.policy.readonly_paths.is_empty());
        assert!(!request.policy.denied_paths.is_empty());
    }
}
