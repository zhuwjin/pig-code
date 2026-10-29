//! Windows sandbox helper: the trusted parent re-enters this binary instead of
//! running a workload through the native launcher directly.
//!
//! The parent (`task::sandbox::windows`) passes one bounded, environment-carried
//! launch request; this process validates it, removes the transport variables,
//! probes the host's native sandbox contract, and launches the workload with the
//! helper's own stdio inherited. The helper never interprets model text and
//! never falls back to an unsandboxed launch.
//!
//! On non-Windows targets this is a stub so `cargo build` works everywhere
//! without compiling any Windows implementation.

fn main() {
    #[cfg(target_os = "windows")]
    {
        std::process::exit(pig_core::windows_sandbox_helper_main());
    }
    #[cfg(not(target_os = "windows"))]
    {
        eprintln!("pig-windows-sandbox is only supported on Windows");
        std::process::exit(1);
    }
}
