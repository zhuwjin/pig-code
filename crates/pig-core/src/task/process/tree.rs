use std::io;
use std::ops::{Deref, DerefMut};
use std::time::Duration;

pub(crate) struct ManagedChild {
    child: tokio::process::Child,
    process: ProcessTask,
}

impl ManagedChild {
    pub(crate) fn process(&self) -> ProcessTask {
        self.process.clone()
    }
}

impl Deref for ManagedChild {
    type Target = tokio::process::Child;

    fn deref(&self) -> &Self::Target {
        &self.child
    }
}

impl DerefMut for ManagedChild {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.child
    }
}

#[derive(Clone)]
pub(crate) struct ProcessTask {
    tree: ProcessTree,
    quiescent: std::sync::Arc<tokio::sync::watch::Sender<bool>>,
    watching: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl ProcessTask {
    fn new(tree: ProcessTree) -> Self {
        let (quiescent, _) = tokio::sync::watch::channel(false);
        Self {
            tree,
            quiescent: std::sync::Arc::new(quiescent),
            watching: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    pub(crate) fn subscribe(&self) -> tokio::sync::watch::Receiver<bool> {
        self.quiescent.subscribe()
    }

    pub(crate) fn is_quiescent(&self) -> bool {
        *self.quiescent.borrow()
    }

    pub(crate) fn mark_quiescent(&self) {
        self.quiescent.send_replace(true);
    }

    pub(crate) fn watch(&self) {
        self.watching
            .store(true, std::sync::atomic::Ordering::Release);
    }

    pub(crate) fn stop_watching(&self) {
        self.watching
            .store(false, std::sync::atomic::Ordering::Release);
    }

    pub(crate) fn has_watcher(&self) -> bool {
        self.watching.load(std::sync::atomic::Ordering::Acquire)
    }

    pub(crate) fn terminate(&self) -> Result<(), String> {
        self.tree.terminate().map_err(|error| error.to_string())
    }

    pub(crate) async fn wait_empty(&self) -> Result<(), String> {
        loop {
            if self.tree.is_empty().map_err(|error| error.to_string())? {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

pub(crate) fn spawn(command: &mut tokio::process::Command) -> io::Result<ManagedChild> {
    #[cfg(windows)]
    let job = windows::Job::create()?;
    #[cfg(windows)]
    {
        command.creation_flags(windows::CREATE_SUSPENDED.0);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.process_group(0);
    }
    let child = command.spawn()?;
    let pid = child.id().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "spawned process has no process id",
        )
    })?;
    #[cfg(unix)]
    let tree = ProcessTree::new(pid)?;
    #[cfg(windows)]
    let tree = ProcessTree { job: job.into() };
    #[cfg(windows)]
    if let Err(error) = windows::assign_and_resume(pid, &tree.job) {
        let mut child = child;
        let cleanup = tree
            .terminate()
            .err()
            .into_iter()
            .chain(child.start_kill().err());
        let details = cleanup.map(|error| error.to_string()).collect::<Vec<_>>();
        return Err(if details.is_empty() {
            error
        } else {
            io::Error::new(
                error.kind(),
                format!(
                    "{error}; failed to clean up the suspended child: {}",
                    details.join("; ")
                ),
            )
        });
    }
    let process = ProcessTask::new(tree);
    Ok(ManagedChild { child, process })
}

#[cfg(unix)]
#[derive(Clone)]
pub(crate) struct ProcessTree {
    inner: std::sync::Arc<UnixProcess>,
}

#[cfg(unix)]
struct UnixProcess {
    pid: i32,
}

#[cfg(unix)]
impl Drop for UnixProcess {
    fn drop(&mut self) {
        if let Err(error) = terminate_pid(self.pid) {
            tracing::warn!(pid = self.pid, %error, "failed to release Unix process group");
        }
    }
}

#[cfg(unix)]
fn terminate_pid(pid: i32) -> io::Result<()> {
    let group = ProcessTree::signal(-pid);
    let direct = ProcessTree::signal(pid);
    match (group, direct) {
        (Ok(_), Ok(_)) => Ok(()),
        (Err(error), _) | (_, Err(error)) => Err(error),
    }
}

#[cfg(unix)]
impl ProcessTree {
    fn new(pid: u32) -> io::Result<Self> {
        let pid = i32::try_from(pid)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "process id exceeds i32"))?;
        Ok(Self {
            inner: std::sync::Arc::new(UnixProcess { pid }),
        })
    }

    fn signal(pid: i32) -> io::Result<bool> {
        // SAFETY: kill is called with a validated signal and does not access memory.
        let result = unsafe { libc::kill(pid, libc::SIGKILL) };
        if result == 0 {
            return Ok(true);
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            Ok(false)
        } else {
            Err(error)
        }
    }

    fn terminate(&self) -> io::Result<()> {
        terminate_pid(self.inner.pid)
    }

    fn is_empty(&self) -> io::Result<bool> {
        // SAFETY: signal zero queries process-group existence without delivering a signal.
        let result = unsafe { libc::kill(-self.inner.pid, 0) };
        if result == 0 {
            return Ok(false);
        }
        let error = io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::ESRCH) => Ok(true),
            Some(libc::EPERM) => Ok(false),
            _ => Err(error),
        }
    }
}

#[cfg(windows)]
#[derive(Clone)]
pub(crate) struct ProcessTree {
    job: std::sync::Arc<windows::Job>,
}

#[cfg(windows)]
impl ProcessTree {
    fn terminate(&self) -> io::Result<()> {
        self.job.terminate()
    }

    fn is_empty(&self) -> io::Result<bool> {
        self.job.is_empty()
    }
}

#[cfg(windows)]
mod windows {
    use super::*;
    use ::windows::Win32::Foundation::{CloseHandle, HANDLE};
    use ::windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
    };
    use ::windows::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JobObjectBasicAccountingInformation, JobObjectExtendedLimitInformation,
        QueryInformationJobObject, SetInformationJobObject, TerminateJobObject,
    };
    pub(super) use ::windows::Win32::System::Threading::CREATE_SUSPENDED;
    use ::windows::Win32::System::Threading::{
        OpenProcess, OpenThread, PROCESS_SET_QUOTA, PROCESS_TERMINATE, ResumeThread,
        THREAD_SUSPEND_RESUME,
    };

    struct OwnedHandle(HANDLE);

    impl OwnedHandle {
        fn raw(&self) -> HANDLE {
            self.0
        }
    }

    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            // SAFETY: this wrapper uniquely owns a valid Win32 handle.
            if let Err(error) = unsafe { CloseHandle(self.0) } {
                tracing::warn!(%error, "failed to close Windows process handle");
            }
        }
    }

    pub(super) struct Job {
        handle: HANDLE,
    }

    // SAFETY: a Job handle is an OS synchronization object usable from any thread.
    unsafe impl Send for Job {}
    // SAFETY: Job operations are serialized by the kernel and take only immutable handles.
    unsafe impl Sync for Job {}

    impl Job {
        pub(super) fn create() -> io::Result<Self> {
            // SAFETY: null attributes and name request an anonymous job owned by this process.
            let handle = unsafe { CreateJobObjectW(None, ::windows::core::PCWSTR::null()) }
                .map_err(io::Error::other)?;
            let job = Self { handle };
            let mut information = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            information.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            // SAFETY: information points to an initialized value of the declared size.
            unsafe {
                SetInformationJobObject(
                    job.handle,
                    JobObjectExtendedLimitInformation,
                    &information as *const _ as *const core::ffi::c_void,
                    size_of_val(&information) as u32,
                )
            }
            .map_err(io::Error::other)?;
            Ok(job)
        }

        pub(super) fn terminate(&self) -> io::Result<()> {
            // SAFETY: the Job remains alive for the duration of this call.
            unsafe { TerminateJobObject(self.handle, 1) }.map_err(io::Error::other)
        }

        pub(super) fn is_empty(&self) -> io::Result<bool> {
            let mut information = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
            // SAFETY: information points to writable storage of the declared size.
            unsafe {
                QueryInformationJobObject(
                    self.handle,
                    JobObjectBasicAccountingInformation,
                    &mut information as *mut _ as *mut core::ffi::c_void,
                    size_of_val(&information) as u32,
                    None,
                )
            }
            .map_err(io::Error::other)?;
            Ok(information.ActiveProcesses == 0)
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            // SAFETY: this Job owns its handle; the configured kill-on-close limit applies.
            if let Err(error) = unsafe { CloseHandle(self.handle) } {
                tracing::warn!(%error, "failed to close Windows Job handle");
            }
        }
    }

    fn resume_main_thread(pid: u32) -> io::Result<()> {
        // SAFETY: snapshot creation has no pointer arguments.
        let snapshot = OwnedHandle(
            unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) }.map_err(io::Error::other)?,
        );
        let mut entry = THREADENTRY32 {
            dwSize: size_of::<THREADENTRY32>() as u32,
            ..Default::default()
        };
        // SAFETY: entry is writable and its dwSize field matches the allocation.
        unsafe { Thread32First(snapshot.raw(), &mut entry) }.map_err(io::Error::other)?;
        loop {
            if entry.th32OwnerProcessID == pid {
                // SAFETY: the thread id came from a live system snapshot.
                let thread = OwnedHandle(
                    unsafe { OpenThread(THREAD_SUSPEND_RESUME, false, entry.th32ThreadID) }
                        .map_err(io::Error::other)?,
                );
                // SAFETY: this is the initial thread of the child created with CREATE_SUSPENDED.
                if unsafe { ResumeThread(thread.raw()) } == u32::MAX {
                    return Err(io::Error::last_os_error());
                }
                return Ok(());
            }
            // SAFETY: entry remains valid writable storage for every enumeration step.
            if unsafe { Thread32Next(snapshot.raw(), &mut entry) }.is_ok() {
                continue;
            }
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(18) {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("no initial thread found for suspended process {pid}"),
                ));
            }
            return Err(error);
        }
    }

    pub(super) fn assign_and_resume(pid: u32, job: &Job) -> io::Result<()> {
        // SAFETY: the suspended child pid was returned by Command::spawn.
        let process = OwnedHandle(
            unsafe { OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, false, pid) }
                .map_err(io::Error::other)?,
        );
        // SAFETY: both handles are valid and the child has not started user code.
        unsafe { AssignProcessToJobObject(job.handle, process.raw()) }.map_err(io::Error::other)?;
        resume_main_thread(pid)
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn windows_job_waits_for_a_real_descendant_tree() {
        let root = std::env::temp_dir().join(format!(
            "pig-process-tree-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("test clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).expect("test directory");
        let started = root.join("started");
        let late = root.join("late");
        let script = root.join("grandchild.cmd");
        std::fs::write(
            &script,
            format!(
                "@echo off\r\necho started > \"{}\"\r\nping -n 2 127.0.0.1 >nul\r\necho late > \"{}\"\r\n",
                started.display(),
                late.display()
            ),
        )
        .expect("test script");
        let mut command = tokio::process::Command::new("cmd.exe");
        command.arg("/C").arg(&script).kill_on_drop(true);
        let child = spawn(&mut command).expect("managed child");
        let process = child.process();
        tokio::time::timeout(Duration::from_secs(5), async {
            while !started.exists() {
                if process.tree.is_empty().expect("query Job") {
                    panic!("Job became empty before the descendant started");
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("descendant start");
        process.terminate().expect("terminate Job");
        let mut child = child;
        tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .expect("direct exit")
            .expect("direct status");
        tokio::time::timeout(Duration::from_secs(5), process.wait_empty())
            .await
            .expect("tree exit")
            .expect("Job empty");
        tokio::time::sleep(Duration::from_millis(1200)).await;
        assert!(!late.exists(), "descendant survived Job termination");
        let _ = std::fs::remove_dir_all(root);
    }
}
