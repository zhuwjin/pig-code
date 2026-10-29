//! Windows sandbox readiness: what this host can actually enforce.
//!
//! The Windows tier is MXC's process security environment (PSEC). It needs no
//! provisioning, so this is a capability probe rather than a setup state
//! machine: a host that fails the probe fails closed (see
//! `native::ensure_available`), and there is nothing for the user to install.

use super::native;

/// Read-only readiness snapshot behind the settings page's status card.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WindowsSandboxReadiness {
    /// The host's native PSEC contract works, so model commands can run
    /// sandboxed.
    pub psec_usable: bool,
    /// That PSEC can also enforce concrete denied paths. The session policy
    /// always denies sensitive files, and `native::ensure_available` fails
    /// closed when the host cannot express them — so PSEC alone is not enough.
    pub psec_deny_paths: bool,
}

/// Snapshot taken by the settings page. Never mutates state.
pub fn readiness() -> WindowsSandboxReadiness {
    WindowsSandboxReadiness {
        psec_usable: native::process_security_environment_usable(),
        psec_deny_paths: native::deny_paths_supported(),
    }
}

impl WindowsSandboxReadiness {
    /// Whether the next model command can actually run sandboxed.
    pub fn usable(&self) -> bool {
        self.psec_usable && self.psec_deny_paths
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_host_needs_both_halves_of_the_contract() {
        assert!(
            WindowsSandboxReadiness {
                psec_usable: true,
                psec_deny_paths: true,
            }
            .usable()
        );
        // Deny paths are mandatory: the sensitive-file policy cannot be
        // expressed without them.
        assert!(
            !WindowsSandboxReadiness {
                psec_usable: true,
                psec_deny_paths: false,
            }
            .usable()
        );
        assert!(
            !WindowsSandboxReadiness {
                psec_usable: false,
                psec_deny_paths: true,
            }
            .usable()
        );
    }
}
