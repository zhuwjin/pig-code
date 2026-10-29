//! Availability probe for the native process security environment (PSEC).
//!
//! The SDK no longer exposes a boolean "is PSEC usable here" predicate, so the
//! question is answered by doing the smallest real thing: build a PSEC
//! specification that grants nothing — no filesystem paths, no network, no UI
//! restrictions — and create it. An API set that is merely *present* is not
//! enough (the downlevel AppContainer route is always there); the create call is
//! what proves this host can honor a policy at all. It launches no process, and
//! the created environment is closed immediately.
//!
//! The answer cannot change within a process, so it is cached: `--probe` and
//! the settings page's readiness card ask for it repeatedly.

use std::sync::OnceLock;

use flatbuffers::FlatBufferBuilder;
use mxc_sdk::mxc_common::models::ContainerPolicy;
use mxc_sdk::mxc_common::ui_policy::resolve_ui_restrictions;
use mxc_sdk::process_container_common::PROCESS_SECURITY_ENVIRONMENT_FLAG_NONE;
use mxc_sdk::process_container_common::ProcessSecurityEnvironmentApi;
use mxc_sdk::process_container_common::SecurityEnvironmentStartupInfo;
use mxc_sdk::process_container_common::job_object::to_job_object_uilimit_mask;
use mxc_sdk::process_security_environment_spec::process_security_environment_layout::EndpointPolicyT;
use mxc_sdk::process_security_environment_spec::process_security_environment_layout::FilterAction;
use mxc_sdk::process_security_environment_spec::process_security_environment_layout::NetworkPolicyT;
use mxc_sdk::process_security_environment_spec::process_security_environment_layout::ProcessSecurityEnvironmentT;
use mxc_sdk::process_security_environment_spec::process_security_environment_layout::SchemaVersionT;
use mxc_sdk::process_security_environment_spec::process_security_environment_layout::finish_process_security_environment_buffer;

/// Whether this host can create a process security environment.
pub(crate) fn is_available() -> bool {
    static USABLE: OnceLock<bool> = OnceLock::new();
    *USABLE.get_or_init(|| {
        let policy = ContainerPolicy::default();
        let mut egress = EndpointPolicyT::default();
        egress.default_action = FilterAction::deny;
        let mut network = NetworkPolicyT::default();
        network.egress = Some(Box::new(egress));
        let mut specification = ProcessSecurityEnvironmentT::default();
        specification.version = SchemaVersionT { major: 1, minor: 0 };
        specification.disallow_win32k_system_calls = policy.ui.disable;
        specification.ui_restrictions = u64::from(to_job_object_uilimit_mask(
            &resolve_ui_restrictions(&policy.ui, &policy.base_process_ui),
        ));
        specification.network_policy = Some(Box::new(network));
        let mut builder = FlatBufferBuilder::with_capacity(1024);
        let specification = specification.pack(&mut builder);
        finish_process_security_environment_buffer(&mut builder, specification);
        ProcessSecurityEnvironmentApi::load()
            .and_then(|api| {
                api.create(
                    builder.finished_data(),
                    PROCESS_SECURITY_ENVIRONMENT_FLAG_NONE,
                )
            })
            .and_then(|environment| {
                // Creating the environment is the probe; the startup-attribute
                // initialization that follows is what a real launch needs next,
                // and it must not leak the environment on either path.
                let startup_info =
                    SecurityEnvironmentStartupInfo::new(Default::default(), environment.raw(), &[]);
                environment.close();
                startup_info.map(drop)
            })
            .is_ok()
    })
}
