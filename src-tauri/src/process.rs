// Tracker for launched ShardX child processes; keyed by profile_id.

use anyhow::Result;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;
use tokio::process::Child;

#[cfg(windows)]
fn taskkill_tree(pid: u32, force: bool) -> bool {
    use std::os::windows::process::CommandExt;
    let pid = pid.to_string();
    let mut command = std::process::Command::new("taskkill");
    command.args(["/PID", &pid, "/T"]);
    if force {
        command.arg("/F");
    }
    command
        .creation_flags(0x08000000)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(windows)]
pub fn kill_stale_user_data_processes(udd: &std::path::Path) {
    let needle = format!("--user-data-dir={}", udd.display()).replace('\'', "''");
    let script = format!(
        "$n='{}'; Get-CimInstance Win32_Process | Where-Object {{ $_.CommandLine -and $_.CommandLine.Contains($n) }} | ForEach-Object {{ Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }}",
        needle
    );
    use std::os::windows::process::CommandExt;
    let _ = std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .creation_flags(0x08000000)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

#[cfg(not(windows))]
pub fn kill_stale_user_data_processes(_udd: &std::path::Path) {}

#[cfg(windows)]
struct ProcessJob(windows_sys::Win32::Foundation::HANDLE);

#[cfg(windows)]
unsafe impl Send for ProcessJob {}

#[cfg(windows)]
impl ProcessJob {
    fn assign(pid: u32) -> Option<Self> {
        use std::mem::{size_of, zeroed};
        use windows_sys::Win32::{
            Foundation::{CloseHandle, INVALID_HANDLE_VALUE},
            System::{
                JobObjects::{
                    AssignProcessToJobObject, CreateJobObjectW, SetInformationJobObject,
                    JobObjectExtendedLimitInformation, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
                    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                },
                Threading::{OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE},
            },
        };

        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() || job == INVALID_HANDLE_VALUE {
                return None;
            }
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = zeroed();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &limits as *const _ as *const _,
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            ) == 0
            {
                CloseHandle(job);
                return None;
            }
            let process = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid);
            if process.is_null() || process == INVALID_HANDLE_VALUE {
                CloseHandle(job);
                return None;
            }
            let assigned = AssignProcessToJobObject(job, process) != 0;
            CloseHandle(process);
            if !assigned {
                CloseHandle(job);
                return None;
            }
            Some(Self(job))
        }
    }

    fn terminate(&self) {
        unsafe {
            windows_sys::Win32::System::JobObjects::TerminateJobObject(self.0, 1);
        }
    }
}

#[cfg(windows)]
impl Drop for ProcessJob {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

pub struct Tracker {
    inner: Mutex<HashMap<String, ChildEntry>>,
}

struct ChildEntry {
    pid: u32,
    killer: tokio::sync::mpsc::Sender<()>,
    /// Set once DevToolsActivePort is read; None for UI launches.
    cdp: Option<CdpInfo>,
    /// Process start; serialised as elapsed ms in RunningProfile.
    started_at: Instant,
}

/// CDP endpoint for an API-launched profile.
#[derive(Debug, Clone, serde::Serialize)]
pub struct CdpInfo {
    pub port: u16,
    pub http_url: String,
    /// ws://127.0.0.1:<port>/devtools/browser/<id> for Puppeteer/Playwright.
    pub web_socket_debugger_url: String,
}

impl Tracker {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }

    /// Take a spawned child + monitor it; entry removed on exit/kill.
    pub fn track(self: &'static Self, profile_id: String, mut child: Child, temporary: bool) -> u32 {
        let pid = child.id().unwrap_or(0);
        #[cfg(windows)]
        let process_job = ProcessJob::assign(pid);
        let (tx, mut rx) = tokio::sync::mpsc::channel::<()>(1);

        {
            let mut g = self.inner.lock().unwrap();
            g.insert(
                profile_id.clone(),
                ChildEntry { pid, killer: tx, cdp: None, started_at: Instant::now() },
            );
        }

        // Lease heartbeat: while a real (non-temporary) profile is open, keep
        // its cloud lease fresh so other devices see it as "in use".  The loop
        // exits once the profile is no longer tracked (i.e. it has closed).
        if !temporary {
            let hb_id = profile_id.clone();
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                    if !Self::shared().is_running(&hb_id) {
                        break;
                    }
                    // Ignore errors: sync may be off/unreachable, TTL covers gaps.
                    let _ = crate::sync::acquire_lease(&hb_id).await;
                }
            });
        }

        // Graceful shutdown (SIGTERM / taskkill WM_CLOSE) → 5s → hard kill.
        // Graceful path flushes session state so next launch skips the restore prompt.
        let started_at = Instant::now();
        tokio::spawn(async move {
            tokio::select! {
                _ = child.wait() => {}
                _ = rx.recv() => {
                    #[cfg(unix)]
                    {
                        if let Some(p) = child.id() {
                            // SAFETY: libc::kill on a child pid we own.
                            unsafe { libc::kill(p as libc::pid_t, libc::SIGTERM); }
                        }
                    }
                    #[cfg(windows)]
                    if let Some(p) = child.id() {
                        // Ask the root tree to close first so it can flush cookies/session state.
                        taskkill_tree(p, false);
                    }
                    let graceful = tokio::time::timeout(
                        std::time::Duration::from_secs(5),
                        child.wait(),
                    ).await;
                    if graceful.is_err() {
                        #[cfg(windows)]
                        if let Some(job) = process_job.as_ref() {
                            job.terminate();
                        }
                        #[cfg(windows)]
                        if process_job.is_none() {
                            if let Some(p) = child.id() {
                                taskkill_tree(p, true);
                            }
                        }
                        let _ = child.kill().await;
                        let _ = child.wait().await;
                    }
                }
            }
            if let Ok(mut g) = Self::shared().inner.lock() {
                g.remove(&profile_id);
            }
            // Bump the persisted total runtime; non-temporary only (temp
            // profiles get deleted next line so their counter is moot).
            if !temporary {
                let elapsed_ms = started_at.elapsed().as_millis() as u64;
                if let Err(e) = crate::profile::add_runtime(&profile_id, elapsed_ms) {
                    eprintln!("[launcher] add_runtime({profile_id}) failed: {e}");
                }
                let sync_profile_id = profile_id.clone();
                tokio::spawn(async move {
                    // Release our lease first so another device can Start this
                    // profile immediately, then push the closed session's data.
                    crate::sync::release_lease(&sync_profile_id).await;
                    crate::sync::sync_after_profile_close(sync_profile_id).await;
                });
            }
            // Tear down temporary profile (config + udd) on close.
            if temporary {
                match crate::profile::delete(&profile_id) {
                    Ok(()) => eprintln!("[launcher] temporary profile {profile_id} deleted on close"),
                    Err(e) => eprintln!("[launcher] temporary profile {profile_id} cleanup failed: {e}"),
                }
            }
        });

        pid
    }

    /// Attach CDP to a tracked profile; no-op if the profile already exited.
    pub fn set_cdp(&self, profile_id: &str, cdp: CdpInfo) {
        if let Ok(mut g) = self.inner.lock() {
            if let Some(e) = g.get_mut(profile_id) {
                e.cdp = Some(cdp);
            }
        }
    }

    /// CDP endpoint when the profile was launched with remote debugging.
    pub fn cdp(&self, profile_id: &str) -> Option<CdpInfo> {
        self.inner.lock().ok()?.get(profile_id)?.cdp.clone()
    }

    pub fn is_running(&self, profile_id: &str) -> bool {
        self.inner
            .lock()
            .map(|g| g.contains_key(profile_id))
            .unwrap_or(false)
    }

    pub fn running(&self) -> Vec<RunningProfile> {
        let g = self.inner.lock().unwrap();
        g.iter()
            .map(|(id, e)| RunningProfile {
                profile_id: id.clone(),
                pid: e.pid,
                cdp: e.cdp.clone(),
                uptime_ms: e.started_at.elapsed().as_millis() as u64,
            })
            .collect()
    }

    pub async fn kill(&self, profile_id: &str) -> Result<bool> {
        let killer = {
            let g = self.inner.lock().unwrap();
            g.get(profile_id).map(|e| e.killer.clone())
        };
        if let Some(k) = killer {
            let _ = k.send(()).await;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    pub fn shared() -> &'static Tracker {
        static INSTANCE: std::sync::OnceLock<Tracker> = std::sync::OnceLock::new();
        INSTANCE.get_or_init(Tracker::new)
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct RunningProfile {
    pub profile_id: String,
    pub pid: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cdp: Option<CdpInfo>,
    /// Milliseconds since the engine was spawned; frontend formats as
    /// "1h 23m" / "12m 30s" / "45s".
    pub uptime_ms: u64,
}
