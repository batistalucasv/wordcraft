//! Minimal `command-group` shim for wasm builds, where processes cannot be spawned.

use std::process::{Child, Command, ExitStatus};

pub struct GroupChild(Child);

impl GroupChild {
    pub fn inner(&mut self) -> &mut Child {
        &mut self.0
    }

    pub fn kill(&mut self) -> std::io::Result<()> {
        self.0.kill()
    }

    pub fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
        self.0.try_wait()
    }
}

pub trait CommandGroup {
    fn group_spawn(&mut self) -> std::io::Result<GroupChild>;
}

impl CommandGroup for Command {
    fn group_spawn(&mut self) -> std::io::Result<GroupChild> {
        Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "process groups are not available on wasm"))
    }
}
