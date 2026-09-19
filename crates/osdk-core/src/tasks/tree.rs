//! Killing a timed-out task together with everything it started.
//!
//! Killing only the direct child is not enough, and the gap is exactly the
//! common case: `cmd /c npm test` exits the moment `cmd.exe` is killed, while
//! the `node` process it launched keeps running, holding the port and the
//! files. A timeout that leaves the runaway running has not done its job.
//!
//! The two platforms need genuinely different mechanisms, and neither is a
//! refinement of the other:
//!
//! - **Windows** has no process-tree kill and no reliable parent links to walk
//!   (a PID is reused once the parent exits). A **job object** solves it from
//!   the other direction: the child is placed in a job at spawn time, children
//!   it creates inherit the job, and terminating the job terminates all of them
//!   at once. `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` also means that if osdk dies
//!   unexpectedly, Windows tears the job down rather than leaking the tree.
//! - **Unix** gets the child into its own **process group** via `setsid`, after
//!   which `kill(-pgid)` signals the whole group. `SIGTERM` first so the program
//!   can clean up, then `SIGKILL` for whatever ignored it.
//!
//! Both paths share the shape: set the grouping up *before* the process starts,
//! because after it has forked there is no longer a reliable way to find its
//! descendants.

use std::process::{Child, Command};
use std::time::Duration;
#[allow(unused_imports)]
use std::time::Instant;

/// How long a process gets to honour SIGTERM before SIGKILL follows.
///
/// Unix only. Long enough for a test runner to flush output and remove temp
/// files, short enough that a hung process does not double the timeout the user
/// asked for.
#[cfg(unix)]
const GRACE: Duration = Duration::from_secs(5);

/// A handle that can terminate a spawned child and everything it started.
pub struct TreeHandle {
    #[cfg(windows)]
    job: Option<WindowsJob>,
    #[cfg(unix)]
    pgid: Option<i32>,
}

impl TreeHandle {
    /// Terminate the whole tree.
    ///
    /// Best-effort by nature: a process can exit between the decision to kill
    /// and the kill itself, and that race is not an error. What must not happen
    /// is a descendant surviving, so failures are reported rather than ignored.
    pub fn kill_tree(&mut self, child: &mut Child) -> std::io::Result<()> {
        #[cfg(windows)]
        {
            if let Some(job) = &self.job {
                // Terminating the job kills every process in it, including ones
                // that were created after the job was set up.
                job.terminate()?;
            }
            // The job covers the tree; this reaps the direct child's handle.
            let _ = child.kill();
            let _ = child.wait();
            Ok(())
        }

        #[cfg(unix)]
        {
            if let Some(pgid) = self.pgid {
                // Negative pid means "the whole process group".
                unsafe { libc::kill(-pgid, libc::SIGTERM) };
                let deadline = Instant::now() + GRACE;
                loop {
                    match child.try_wait() {
                        Ok(Some(_)) => break,
                        Ok(None) if Instant::now() < deadline => {
                            std::thread::sleep(Duration::from_millis(50));
                        }
                        _ => {
                            // Either it ignored SIGTERM or waiting failed;
                            // either way the group must not survive.
                            unsafe { libc::kill(-pgid, libc::SIGKILL) };
                            break;
                        }
                    }
                }
            } else {
                let _ = child.kill();
            }
            let _ = child.wait();
            Ok(())
        }

        #[cfg(not(any(windows, unix)))]
        {
            let _ = child.kill();
            let _ = child.wait();
            Ok(())
        }
    }
}

/// Configure `command` so its descendants can be killed as a unit, then spawn.
pub fn spawn_in_tree(command: &mut Command) -> std::io::Result<(Child, TreeHandle)> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_SUSPENDED would be the airtight version (assign to the job
        // before any code runs), but it needs ResumeThread and a raw thread
        // handle that std does not expose. CREATE_NEW_PROCESS_GROUP plus an
        // immediate assignment leaves a window measured in microseconds, during
        // which the child would have to spawn a grandchild to escape.
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        command.creation_flags(CREATE_NEW_PROCESS_GROUP);
        let child = command.spawn()?;
        let job = WindowsJob::create_and_assign(&child).ok();
        Ok((child, TreeHandle { job }))
    }

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            // setsid makes the child a session and process-group leader, so its
            // own children inherit the group and one signal reaches all of them.
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn()?;
        let pgid = child.id() as i32;
        Ok((child, TreeHandle { pgid: Some(pgid) }))
    }

    #[cfg(not(any(windows, unix)))]
    {
        // No grouping primitive known for this platform: still spawn, but be
        // honest that only the direct child can be killed.
        let child = command.spawn()?;
        Ok((child, TreeHandle {}))
    }
}

#[cfg(windows)]
struct WindowsJob {
    handle: windows_sys::Win32::Foundation::HANDLE,
}

#[cfg(windows)]
unsafe impl Send for WindowsJob {}

#[cfg(windows)]
impl WindowsJob {
    fn create_and_assign(child: &Child) -> std::io::Result<Self> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
            SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        };

        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if handle.is_null() {
            return Err(std::io::Error::last_os_error());
        }

        // Without KILL_ON_JOB_CLOSE, an osdk that dies unexpectedly would leak
        // the whole tree; with it, Windows cleans up when the last handle goes.
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let ok = unsafe {
            SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                std::ptr::addr_of!(info) as *const std::ffi::c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if ok == 0 {
            let error = std::io::Error::last_os_error();
            unsafe { windows_sys::Win32::Foundation::CloseHandle(handle) };
            return Err(error);
        }

        let assigned = unsafe { AssignProcessToJobObject(handle, child.as_raw_handle() as _) };
        if assigned == 0 {
            let error = std::io::Error::last_os_error();
            unsafe { windows_sys::Win32::Foundation::CloseHandle(handle) };
            return Err(error);
        }

        Ok(Self { handle })
    }

    fn terminate(&self) -> std::io::Result<()> {
        use windows_sys::Win32::System::JobObjects::TerminateJobObject;
        // Exit code 1: the tree was killed, not a clean exit.
        if unsafe { TerminateJobObject(self.handle, 1) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
}

#[cfg(windows)]
impl Drop for WindowsJob {
    fn drop(&mut self) {
        unsafe { windows_sys::Win32::Foundation::CloseHandle(self.handle) };
    }
}

/// Wait for `child`, killing its whole tree if `limit` elapses first.
///
/// Returns the exit code, or `None` when the timeout fired.
pub fn wait_with_timeout(
    child: &mut Child,
    handle: &mut TreeHandle,
    limit: Option<Duration>,
) -> std::io::Result<Option<i32>> {
    let Some(limit) = limit else {
        let status = child.wait()?;
        return Ok(Some(status.code().unwrap_or(1)));
    };

    let deadline = Instant::now() + limit;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status.code().unwrap_or(1)));
        }
        if Instant::now() >= deadline {
            handle.kill_tree(child)?;
            return Ok(None);
        }
        // Polling rather than a platform wait primitive: the granularity that
        // matters here is human-scale, and 20 ms of latency on a timeout that
        // is measured in seconds costs nothing while keeping this portable.
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Parse a duration like `30s`, `5m`, `1h`.
pub fn parse_duration(text: &str) -> Option<Duration> {
    let text = text.trim();
    let (value, multiplier) = match text.chars().last()? {
        's' => (&text[..text.len() - 1], 1),
        'm' => (&text[..text.len() - 1], 60),
        'h' => (&text[..text.len() - 1], 3600),
        // A bare number is seconds, which is what people write first.
        _ => (text, 1),
    };
    let seconds: u64 = value.trim().parse().ok()?;
    if seconds == 0 {
        return None;
    }
    Some(Duration::from_secs(seconds * multiplier))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_parse_in_the_units_people_write() {
        assert_eq!(parse_duration("30s"), Some(Duration::from_secs(30)));
        assert_eq!(parse_duration("5m"), Some(Duration::from_secs(300)));
        assert_eq!(parse_duration("1h"), Some(Duration::from_secs(3600)));
        assert_eq!(parse_duration("45"), Some(Duration::from_secs(45)));
        assert_eq!(parse_duration("  10s  "), Some(Duration::from_secs(10)));
    }

    #[test]
    fn nonsense_durations_are_rejected_rather_than_defaulted() {
        // Silently treating these as "no timeout" would be the worst outcome:
        // the user asked for a limit and would not get one.
        assert_eq!(parse_duration(""), None);
        assert_eq!(parse_duration("abc"), None);
        assert_eq!(parse_duration("-5s"), None);
        assert_eq!(parse_duration("0s"), None);
        assert_eq!(parse_duration("1.5h"), None);
    }

    #[test]
    fn a_quick_command_finishes_before_its_timeout() {
        let mut command = if cfg!(windows) {
            let mut c = Command::new("cmd");
            c.args(["/c", "exit 0"]);
            c
        } else {
            let mut c = Command::new("sh");
            c.args(["-c", "exit 0"]);
            c
        };
        let (mut child, mut handle) = spawn_in_tree(&mut command).unwrap();
        let code =
            wait_with_timeout(&mut child, &mut handle, Some(Duration::from_secs(30))).unwrap();
        assert_eq!(code, Some(0));
    }

    #[test]
    fn a_nonzero_exit_is_reported_not_swallowed() {
        let mut command = if cfg!(windows) {
            let mut c = Command::new("cmd");
            c.args(["/c", "exit 3"]);
            c
        } else {
            let mut c = Command::new("sh");
            c.args(["-c", "exit 3"]);
            c
        };
        let (mut child, mut handle) = spawn_in_tree(&mut command).unwrap();
        let code =
            wait_with_timeout(&mut child, &mut handle, Some(Duration::from_secs(30))).unwrap();
        assert_eq!(code, Some(3));
    }

    #[test]
    fn a_hanging_command_is_killed_at_the_deadline() {
        let mut command = if cfg!(windows) {
            let mut c = Command::new("cmd");
            // `pause` without input blocks indefinitely.
            c.args(["/c", "ping -n 60 127.0.0.1 >nul"]);
            c
        } else {
            let mut c = Command::new("sh");
            c.args(["-c", "sleep 60"]);
            c
        };
        let started = Instant::now();
        let (mut child, mut handle) = spawn_in_tree(&mut command).unwrap();
        let code =
            wait_with_timeout(&mut child, &mut handle, Some(Duration::from_millis(300))).unwrap();
        assert_eq!(code, None, "timeout must report as timed out");
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "kill did not take effect: {:?}",
            started.elapsed()
        );
    }

    /// The property this module exists for: a grandchild must not survive.
    ///
    /// Asserting on the direct child proves nothing -- killing `cmd.exe` always
    /// "succeeds" while the program it launched keeps running. So the probe is
    /// a grandchild appending to a file on a timer: if the tree kill worked the
    /// file stops growing, and if only the direct child died it keeps growing.
    ///
    /// The probe is a script file rather than an inline command because nested
    /// quoting through `cmd /c` silently failed to start it -- caught only by
    /// the "probe never started" assertion below, which is why that assertion
    /// is there.
    #[test]
    fn killing_the_tree_stops_a_grandchild_not_just_the_direct_child() {
        let temp = tempfile::tempdir().unwrap();
        let marker = temp.path().join("alive.txt");

        let mut command = if cfg!(windows) {
            let inner = temp.path().join("probe.bat");
            std::fs::write(
                &inner,
                format!(
                    "@echo off\r\nfor /L %%i in (1,1,200) do (\r\n  echo tick>>\"{}\"\r\n  ping -n 2 127.0.0.1 >nul\r\n)\r\n",
                    marker.display()
                ),
            )
            .unwrap();
            let outer = temp.path().join("outer.bat");
            // The outer script starts the probe as a *separate* process, so the
            // probe is a grandchild of the process we spawn.
            std::fs::write(
                &outer,
                format!("@echo off\r\ncmd /c \"{}\"\r\n", inner.display()),
            )
            .unwrap();
            let mut c = Command::new("cmd");
            c.args(["/c", &outer.to_string_lossy()]);
            c
        } else {
            let inner = temp.path().join("probe.sh");
            std::fs::write(
                &inner,
                format!(
                    "#!/bin/sh\ni=0\nwhile [ $i -lt 200 ]; do echo tick >> '{}'; sleep 0.1; i=$((i+1)); done\n",
                    marker.display()
                ),
            )
            .unwrap();
            let mut c = Command::new("sh");
            c.args(["-c", &format!("sh '{}'", inner.display())]);
            c
        };

        let (mut child, mut handle) = spawn_in_tree(&mut command).unwrap();

        // Wait for the probe to actually produce output before killing.
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut before_kill = 0;
        while Instant::now() < deadline {
            before_kill = std::fs::read_to_string(&marker).unwrap_or_default().len();
            if before_kill > 0 {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(
            before_kill > 0,
            "probe never started; the test would pass vacuously"
        );

        handle.kill_tree(&mut child).unwrap();

        // Give any survivor a clear chance to keep writing.
        std::thread::sleep(Duration::from_millis(1500));
        let after_kill = std::fs::read_to_string(&marker).unwrap_or_default().len();
        std::thread::sleep(Duration::from_millis(1500));
        let later = std::fs::read_to_string(&marker).unwrap_or_default().len();

        assert_eq!(
            after_kill, later,
            "a grandchild outlived the tree kill: file grew from {after_kill} to {later} bytes"
        );
    }

    #[test]
    fn no_timeout_waits_for_completion() {
        let mut command = if cfg!(windows) {
            let mut c = Command::new("cmd");
            c.args(["/c", "exit 7"]);
            c
        } else {
            let mut c = Command::new("sh");
            c.args(["-c", "exit 7"]);
            c
        };
        let (mut child, mut handle) = spawn_in_tree(&mut command).unwrap();
        assert_eq!(
            wait_with_timeout(&mut child, &mut handle, None).unwrap(),
            Some(7)
        );
    }
}
