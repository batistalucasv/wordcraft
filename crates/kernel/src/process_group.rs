//! Pure-safe process-group wrapper for spawning and terminating interpreter trees.
//!
//! - On Unix, spawns the child in its own process group (`process_group(0)`) and kills the whole
//!   group via `kill_process_group`.
//! - On Windows, spawns with `CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW` and terminates the
//!   process tree via `taskkill /F /T /PID` when still running.
//! - On wasm, process spawning returns `ErrorKind::Unsupported`.

use std::io;
use std::process::{Child, Command, ExitStatus};

pub(crate) struct GroupChild {
    child: Child,
    #[cfg(unix)]
    pgid: Option<i32>,
}

impl GroupChild {
    #[cfg(unix)]
    pub(crate) fn id(&self) -> u32 {
        self.child.id()
    }

    pub(crate) fn inner(&mut self) -> &mut Child {
        &mut self.child
    }

    pub(crate) fn kill(&mut self) -> io::Result<()> {
        if let Ok(Some(_)) = self.child.try_wait() {
            return Ok(());
        }
        #[cfg(unix)]
        {
            if let Some(raw_pgid) = self.pgid.filter(|&pgid| pgid > 1)
                && let Some(pgid) = rustix::process::Pid::from_raw(raw_pgid)
            {
                let _ = rustix::process::kill_process_group(pgid, rustix::process::Signal::KILL);
            }
            self.child.kill()
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            use std::process::Stdio;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            let pid = self.child.id().to_string();
            let _ = Command::new("taskkill")
                .args(["/F", "/T", "/PID", &pid])
                .creation_flags(CREATE_NO_WINDOW)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            if let Ok(Some(_)) = self.child.try_wait() {
                return Ok(());
            }
            self.child.kill()
        }
        #[cfg(not(any(unix, windows)))]
        {
            self.child.kill()
        }
    }

    pub(crate) fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }
}

pub(crate) trait CommandGroup {
    fn group_spawn(&mut self) -> io::Result<GroupChild>;
}

impl CommandGroup for Command {
    fn group_spawn(&mut self) -> io::Result<GroupChild> {
        #[cfg(target_family = "wasm")]
        {
            Err(io::Error::new(io::ErrorKind::Unsupported, "process groups are not available on wasm"))
        }
        #[cfg(all(unix, not(target_family = "wasm")))]
        {
            use std::os::unix::process::CommandExt;
            self.process_group(0);
            let child = self.spawn()?;
            let pgid = i32::try_from(child.id()).ok();
            Ok(GroupChild { child, pgid })
        }
        #[cfg(all(windows, not(target_family = "wasm")))]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            self.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
            let child = self.spawn()?;
            Ok(GroupChild { child })
        }
        #[cfg(not(any(unix, windows, target_family = "wasm")))]
        {
            let child = self.spawn()?;
            Ok(GroupChild { child })
        }
    }
}
