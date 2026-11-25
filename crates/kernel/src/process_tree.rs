//! Subprocess lifecycle containment for one-shot code chunk execution.
//!
//! On Windows, `process_group` spawns each run with `CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW`
//! and terminates the tree via `taskkill /F /T /PID` on timeout or cancellation.
//! On Unix, a subprocess can leave its original process group via `setsid()` or `setpgid()`,
//! or double-fork so its parent becomes PID 1. [`ProcessTreeGuard`] combines process-group
//! signaling with per-run environment tagging, transitive PPID/PGID graph closure, and
//! (on Linux) pipe-inode and exclusive-scratch-directory matching so escaped descendants are
//! frozen and terminated before the run returns.

use std::time::Duration;

/// How long to wait for the process tree to be terminated and reaped.
const PROCESS_TREE_CLEANUP_TIMEOUT: Duration = Duration::from_secs(2);
/// Poll interval while waiting for the interpreter to exit.
const WAIT_POLL_INTERVAL: Duration = Duration::from_millis(20);
/// Poll interval while reaping terminated processes.
const REAP_POLL_INTERVAL: Duration = Duration::from_millis(10);

#[cfg(unix)]
mod unix {
    use std::collections::{BTreeMap, BTreeSet, VecDeque};
    #[cfg(target_os = "linux")]
    use std::ffi::OsString;
    #[cfg(target_os = "linux")]
    use std::fs;
    use std::io::{self, Read};
    use std::path::Path;
    #[cfg(target_os = "linux")]
    use std::path::PathBuf;
    use std::process::{ChildStderr, ChildStdout, Command, ExitStatus};
    use std::thread;
    use std::time::{Duration, Instant};

    use crate::process_group::GroupChild;
    use rustix::process::{Pid, Signal, getpgrp, kill_process, kill_process_group};

    use super::{PROCESS_TREE_CLEANUP_TIMEOUT, REAP_POLL_INTERVAL, WAIT_POLL_INTERVAL};

    /// Environment variable injected into each chunk run to identify escaped descendants.
    const RUN_MARKER_ENV: &str = "WORDCRAFT_KERNEL_RUN";
    /// Maximum freeze-and-discover rounds before moving to `SIGKILL`.
    const MAX_FREEZE_ROUNDS: usize = 8;
    /// Maximum wall time spent in freeze-and-discover before `SIGKILL`.
    const FREEZE_BUDGET: Duration = Duration::from_millis(250);
    /// Upper bound on processes inspected in one snapshot.
    const MAX_SNAPSHOT_PROCESSES: usize = 65_536;
    /// Upper bound on ancestor hops when protecting the host process chain.
    const MAX_ANCESTOR_HOPS: usize = 256;

    #[cfg(target_os = "linux")]
    const MAX_PROC_STAT_BYTES: u64 = 8 * 1024;
    #[cfg(target_os = "linux")]
    const MAX_PROC_ENVIRON_BYTES: u64 = 512 * 1024;
    #[cfg(target_os = "linux")]
    const MAX_PROC_FDS: usize = 4_096;
    #[cfg(not(target_os = "linux"))]
    const MAX_PS_OUTPUT_BYTES: u64 = 16 * 1024 * 1024;

    /// Per-run containment metadata shared across spawn, pipe setup, and teardown.
    pub(crate) struct RunContainment {
        marker_value: String,
        marker_entry: String,
        #[cfg(target_os = "linux")]
        scratch_dir: Option<PathBuf>,
        #[cfg(target_os = "linux")]
        pipe_targets: Vec<OsString>,
    }

    impl RunContainment {
        pub(crate) fn new(temp_dir: &Path, uses_exclusive_cwd: bool) -> io::Result<Self> {
            let mut random = [0_u8; 16];
            getrandom::fill(&mut random).map_err(|err| io::Error::other(format!("generate run containment token: {err}")))?;
            let marker_value = format!("{}-{:032x}", std::process::id(), u128::from_be_bytes(random));
            let marker_entry = format!("{RUN_MARKER_ENV}={marker_value}");
            #[cfg(target_os = "linux")]
            let scratch_dir = if uses_exclusive_cwd { fs::canonicalize(temp_dir).ok() } else { None };
            #[cfg(not(target_os = "linux"))]
            let _ = (temp_dir, uses_exclusive_cwd);
            Ok(Self {
                marker_value,
                marker_entry,
                #[cfg(target_os = "linux")]
                scratch_dir,
                #[cfg(target_os = "linux")]
                pipe_targets: Vec::new(),
            })
        }

        pub(crate) fn configure_command(&self, cmd: &mut Command) {
            cmd.env(RUN_MARKER_ENV, &self.marker_value);
        }

        #[cfg(target_os = "linux")]
        fn register_pipes(&mut self, stdout: Option<&ChildStdout>, stderr: Option<&ChildStderr>) {
            use std::os::fd::AsRawFd;
            for fd in [stdout.map(AsRawFd::as_raw_fd), stderr.map(AsRawFd::as_raw_fd)].into_iter().flatten() {
                if let Ok(target) = fs::read_link(format!("/proc/self/fd/{fd}")) {
                    self.pipe_targets.push(target.into_os_string());
                }
            }
        }

        #[cfg(not(target_os = "linux"))]
        fn register_pipes(&mut self, _stdout: Option<&ChildStdout>, _stderr: Option<&ChildStderr>) {}
    }

    /// Snapshot entry for one OS process.
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct ProcessRecord {
        pid: i32,
        ppid: i32,
        pgid: i32,
        is_zombie: bool,
        starttime: Option<u64>,
        tagged: bool,
    }

    /// Identity of a process scheduled for termination.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct TargetIdentity {
        pgid: i32,
        starttime: Option<u64>,
    }

    /// RAII guard that guarantees the interpreter and all of its descendants (including those
    /// that moved to a new process group or session) are stopped and reaped on every exit path.
    pub(crate) struct ProcessTreeGuard {
        child: GroupChild,
        containment: RunContainment,
        leader_pid: Option<i32>,
        #[cfg(target_os = "linux")]
        min_starttime: Option<u64>,
        leader_reaped: bool,
        cleaned_up: bool,
        exit_status: Option<ExitStatus>,
    }

    impl ProcessTreeGuard {
        pub(crate) fn new(child: GroupChild, containment: RunContainment) -> Self {
            let leader_pid = i32::try_from(child.id()).ok().filter(|&pid| pid > 1);
            #[cfg(target_os = "linux")]
            let min_starttime = leader_pid
                .and_then(|pid| read_linux_proc_stat(pid).map(|stat| stat.starttime))
                .or_else(|| i32::try_from(std::process::id()).ok().and_then(|pid| read_linux_proc_stat(pid).map(|stat| stat.starttime)));
            Self {
                child,
                containment,
                leader_pid,
                #[cfg(target_os = "linux")]
                min_starttime,
                leader_reaped: false,
                cleaned_up: false,
                exit_status: None,
            }
        }

        pub(crate) fn child_mut(&mut self) -> &mut GroupChild {
            &mut self.child
        }

        pub(crate) fn register_pipes(&mut self, stdout: Option<&ChildStdout>, stderr: Option<&ChildStderr>) {
            self.containment.register_pipes(stdout, stderr);
        }

        /// Wait for the direct interpreter to exit or for `timeout` to elapse.
        ///
        /// On Unix targets with `waitid(WNOWAIT)`, exit is observed without reaping the leader so
        /// its PID and PGID cannot be reused before [`terminate_and_reap`] signals the group.
        pub(crate) fn wait_with_timeout(&mut self, timeout: Duration) -> bool {
            let start = Instant::now();
            loop {
                match self.observe_leader_exit() {
                    LeaderState::Exited => return false,
                    LeaderState::Running => {
                        if start.elapsed() >= timeout {
                            return true;
                        }
                        thread::sleep(WAIT_POLL_INTERVAL);
                    }
                }
            }
        }

        /// Stop the original process group and any escaped descendants, reap the leader, and
        /// return the leader's exit status when it finished before timeout.
        pub(crate) fn terminate_and_reap(mut self) -> Option<ExitStatus> {
            self.cleanup();
            self.exit_status
        }

        fn observe_leader_exit(&mut self) -> LeaderState {
            if self.leader_reaped {
                return LeaderState::Exited;
            }
            #[cfg(not(any(target_os = "openbsd", target_os = "redox")))]
            if let Some(raw_pid) = self.leader_pid
                && let Some(pid) = rustix::process::Pid::from_raw(raw_pid)
            {
                let options =
                    rustix::process::WaitIdOptions::EXITED | rustix::process::WaitIdOptions::NOHANG | rustix::process::WaitIdOptions::NOWAIT;
                return match rustix::process::waitid(rustix::process::WaitId::Pid(pid), options) {
                    Ok(Some(_)) | Err(rustix::io::Errno::CHILD) => LeaderState::Exited,
                    Ok(None) => LeaderState::Running,
                    Err(_) => self.poll_try_wait(),
                };
            }
            self.poll_try_wait()
        }

        fn poll_try_wait(&mut self) -> LeaderState {
            match self.child.inner().try_wait() {
                Ok(Some(status)) => {
                    self.exit_status = Some(status);
                    self.leader_reaped = true;
                    LeaderState::Exited
                }
                Ok(None) => LeaderState::Running,
                Err(_) => {
                    self.leader_reaped = true;
                    LeaderState::Exited
                }
            }
        }

        fn cleanup(&mut self) {
            if self.cleaned_up {
                return;
            }
            self.cleaned_up = true;

            let mut tracked: BTreeMap<i32, TargetIdentity> = BTreeMap::new();
            let mut protected_pgids = BTreeSet::new();

            // Phase 1: freeze the original group and iteratively discover & freeze escaped
            // descendants before killing so a fork loop cannot outrun discovery.
            let freeze_started = Instant::now();
            for _ in 0..MAX_FREEZE_ROUNDS {
                if !self.leader_reaped
                    && let Some(raw_pgid) = self.leader_pid
                    && let Some(pgid) = Pid::from_raw(raw_pgid)
                {
                    let _ = kill_process_group(pgid, Signal::STOP);
                }

                let snapshot = self.capture_snapshot();
                let my_pid = i32::try_from(std::process::id()).unwrap_or(0);
                let (protected_pids, pgids) = protected_sets(&snapshot, my_pid);
                protected_pgids = pgids;

                let root_pid = if self.leader_reaped { None } else { self.leader_pid };
                let discovered = select_targets(&snapshot, root_pid, &protected_pids, &protected_pgids, &tracked);
                let mut added_any = false;

                for record in discovered {
                    if tracked.contains_key(&record.pid) {
                        continue;
                    }
                    added_any = true;
                    tracked.insert(record.pid, TargetIdentity { pgid: record.pgid, starttime: record.starttime });
                    signal_target(record.pid, record.pgid, &protected_pgids, Signal::STOP);
                }

                if !added_any || freeze_started.elapsed() >= FREEZE_BUDGET {
                    break;
                }
            }

            // Phase 2: kill the original process group (while leader is still unreaped so PGID
            // cannot have been recycled) and kill all discovered escaped targets.
            if !self.leader_reaped {
                let _ = self.child.kill();
            }
            for (&pid, &identity) in &tracked {
                signal_target(pid, identity.pgid, &protected_pgids, Signal::KILL);
            }

            // Phase 3: reap the leader once and never signal its PID/PGID again.
            self.reap_leader();

            // Phase 4: confirm escaped targets exited and sweep any straggler spawned right before
            // the freeze took effect.
            self.verify_and_sweep(&mut tracked);
        }

        fn reap_leader(&mut self) {
            if self.leader_reaped {
                return;
            }
            let started = Instant::now();
            loop {
                match self.child.inner().try_wait() {
                    Ok(Some(status)) => {
                        self.exit_status = Some(status);
                        self.leader_reaped = true;
                        return;
                    }
                    Err(_) => {
                        self.leader_reaped = true;
                        return;
                    }
                    Ok(None) if started.elapsed() >= PROCESS_TREE_CLEANUP_TIMEOUT => return,
                    Ok(None) => thread::sleep(REAP_POLL_INTERVAL),
                }
            }
        }

        fn verify_and_sweep(&self, tracked: &mut BTreeMap<i32, TargetIdentity>) {
            let started = Instant::now();
            let my_pid = i32::try_from(std::process::id()).unwrap_or(0);
            loop {
                let snapshot = self.capture_snapshot();
                let (protected_pids, protected_pgids) = protected_sets(&snapshot, my_pid);
                let by_pid: BTreeMap<i32, &ProcessRecord> = snapshot.iter().map(|rec| (rec.pid, rec)).collect();

                // Prune any target that exited, became a zombie, or whose PID was recycled.
                tracked.retain(|&pid, identity| {
                    by_pid.get(&pid).is_some_and(|rec| !rec.is_zombie && (identity.starttime.is_none() || rec.starttime == identity.starttime))
                });

                let extra = select_targets(&snapshot, None, &protected_pids, &protected_pgids, tracked);
                let mut found_new = false;
                for record in extra {
                    if tracked.contains_key(&record.pid) {
                        continue;
                    }
                    found_new = true;
                    tracked.insert(record.pid, TargetIdentity { pgid: record.pgid, starttime: record.starttime });
                    signal_target(record.pid, record.pgid, &protected_pgids, Signal::KILL);
                }

                if tracked.is_empty() && !found_new {
                    return;
                }
                for (&pid, &identity) in tracked.iter() {
                    signal_target(pid, identity.pgid, &protected_pgids, Signal::KILL);
                }
                if started.elapsed() >= PROCESS_TREE_CLEANUP_TIMEOUT {
                    return;
                }
                thread::sleep(REAP_POLL_INTERVAL);
            }
        }

        #[cfg(target_os = "linux")]
        fn capture_snapshot(&self) -> Vec<ProcessRecord> {
            capture_linux_snapshot(&self.containment, self.min_starttime)
        }

        #[cfg(not(target_os = "linux"))]
        fn capture_snapshot(&self) -> Vec<ProcessRecord> {
            capture_ps_snapshot(&self.containment.marker_entry)
        }
    }

    impl Drop for ProcessTreeGuard {
        fn drop(&mut self) {
            self.cleanup();
        }
    }

    enum LeaderState {
        Running,
        Exited,
    }

    fn signal_target(raw_pid: i32, raw_pgid: i32, protected_pgids: &BTreeSet<i32>, signal: Signal) {
        if raw_pid <= 1 {
            return;
        }
        if raw_pgid == raw_pid
            && raw_pgid > 1
            && !protected_pgids.contains(&raw_pgid)
            && let Some(pgid) = Pid::from_raw(raw_pgid)
        {
            let _ = kill_process_group(pgid, signal);
        }
        if let Some(pid) = Pid::from_raw(raw_pid) {
            let _ = kill_process(pid, signal);
        }
    }

    /// Build the set of PIDs and PGIDs that must never be signaled: our own process and every
    /// ancestor up to PID 1.
    fn protected_sets(snapshot: &[ProcessRecord], my_pid: i32) -> (BTreeSet<i32>, BTreeSet<i32>) {
        let mut protected_pids = BTreeSet::new();
        let mut protected_pgids = BTreeSet::new();
        protected_pids.insert(0);
        protected_pids.insert(1);
        protected_pgids.insert(0);
        protected_pgids.insert(1);
        if my_pid > 1 {
            protected_pids.insert(my_pid);
        }
        let own_pgid = Pid::as_raw(Some(getpgrp()));
        if own_pgid > 0 {
            protected_pgids.insert(own_pgid);
        }

        let by_pid: BTreeMap<i32, &ProcessRecord> = snapshot.iter().map(|rec| (rec.pid, rec)).collect();
        let mut current = my_pid;
        for _ in 0..MAX_ANCESTOR_HOPS {
            let Some(rec) = by_pid.get(&current) else {
                break;
            };
            if rec.pgid > 0 {
                protected_pgids.insert(rec.pgid);
            }
            if rec.ppid <= 1 || !protected_pids.insert(rec.ppid) {
                break;
            }
            current = rec.ppid;
        }

        (protected_pids, protected_pgids)
    }

    /// Select all non-zombie processes belonging to the run: the unreaped leader (if present),
    /// its original process group, tagged processes, any already-tracked live targets, every
    /// transitive PPID descendant of those processes, and members of any new non-protected
    /// process group created by a selected target.
    fn select_targets(
        snapshot: &[ProcessRecord],
        root_pid: Option<i32>,
        protected_pids: &BTreeSet<i32>,
        protected_pgids: &BTreeSet<i32>,
        existing: &BTreeMap<i32, TargetIdentity>,
    ) -> Vec<ProcessRecord> {
        let mut children_by_ppid: BTreeMap<i32, Vec<usize>> = BTreeMap::new();
        let mut by_pgid: BTreeMap<i32, Vec<usize>> = BTreeMap::new();
        for (idx, rec) in snapshot.iter().enumerate() {
            children_by_ppid.entry(rec.ppid).or_default().push(idx);
            by_pgid.entry(rec.pgid).or_default().push(idx);
        }

        let mut selected = vec![false; snapshot.len()];
        let mut queue = VecDeque::new();

        for (idx, rec) in snapshot.iter().enumerate() {
            if rec.pid <= 1 || protected_pids.contains(&rec.pid) {
                continue;
            }
            let is_root_or_group = root_pid.is_some_and(|root| rec.pid == root || rec.pgid == root);
            let is_existing = existing.get(&rec.pid).is_some_and(|id| !rec.is_zombie && (id.starttime.is_none() || rec.starttime == id.starttime));
            if is_root_or_group || rec.tagged || is_existing {
                selected[idx] = true;
                queue.push_back(idx);
            }
        }

        while let Some(idx) = queue.pop_front() {
            let rec = &snapshot[idx];
            if let Some(children) = children_by_ppid.get(&rec.pid) {
                for &child_idx in children {
                    let child = &snapshot[child_idx];
                    if !selected[child_idx] && child.pid > 1 && !protected_pids.contains(&child.pid) {
                        selected[child_idx] = true;
                        queue.push_back(child_idx);
                    }
                }
            }
            // If this target created its own process group (e.g. via setsid/setpgid), capture
            // any siblings/grand-children in that escaped group too.
            if rec.pgid == rec.pid
                && rec.pgid > 1
                && !protected_pgids.contains(&rec.pgid)
                && let Some(members) = by_pgid.get(&rec.pgid)
            {
                for &member_idx in members {
                    let member = &snapshot[member_idx];
                    if !selected[member_idx] && member.pid > 1 && !protected_pids.contains(&member.pid) {
                        selected[member_idx] = true;
                        queue.push_back(member_idx);
                    }
                }
            }
        }

        snapshot
            .iter()
            .enumerate()
            .filter_map(|(idx, rec)| if selected[idx] && !rec.is_zombie && Some(rec.pid) != root_pid { Some(rec.clone()) } else { None })
            .collect()
    }

    #[cfg(target_os = "linux")]
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct LinuxProcStat {
        is_zombie: bool,
        ppid: i32,
        pgid: i32,
        starttime: u64,
    }

    #[cfg(target_os = "linux")]
    fn parse_linux_proc_stat(content: &str) -> Option<LinuxProcStat> {
        let (_, rest) = content.rsplit_once(')')?;
        let mut fields = rest.split_whitespace();
        let state = fields.next()?;
        let ppid = fields.next()?.parse::<i32>().ok()?;
        let pgid = fields.next()?.parse::<i32>().ok()?;
        // `rest` starts at field 3 (state); field 22 (starttime) is index 19, i.e. 16 fields after pgid.
        let starttime = fields.nth(16)?.parse::<u64>().ok()?;
        Some(LinuxProcStat { is_zombie: state.starts_with('Z'), ppid, pgid, starttime })
    }

    #[cfg(target_os = "linux")]
    fn read_linux_proc_stat(pid: i32) -> Option<LinuxProcStat> {
        let bytes = read_file_bounded(&PathBuf::from(format!("/proc/{pid}/stat")), MAX_PROC_STAT_BYTES)?;
        let text = String::from_utf8_lossy(&bytes);
        parse_linux_proc_stat(&text)
    }

    #[cfg(target_os = "linux")]
    fn capture_linux_snapshot(containment: &RunContainment, min_starttime: Option<u64>) -> Vec<ProcessRecord> {
        let Ok(entries) = fs::read_dir("/proc") else {
            return Vec::new();
        };
        let my_pid = i32::try_from(std::process::id()).unwrap_or(0);
        let mut records = Vec::new();

        for entry in entries.flatten().take(MAX_SNAPSHOT_PROCESSES) {
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            let Ok(pid) = name.parse::<i32>() else {
                continue;
            };
            if pid <= 1 {
                continue;
            }
            let Some(stat) = read_linux_proc_stat(pid) else {
                continue;
            };

            let eligible_for_tag = pid != my_pid && !stat.is_zombie && min_starttime.is_none_or(|min| stat.starttime >= min);
            let tagged = eligible_for_tag && linux_process_matches_containment(pid, containment);
            records.push(ProcessRecord { pid, ppid: stat.ppid, pgid: stat.pgid, is_zombie: stat.is_zombie, starttime: Some(stat.starttime), tagged });
        }
        records
    }

    #[cfg(target_os = "linux")]
    fn linux_process_matches_containment(pid: i32, containment: &RunContainment) -> bool {
        if let Some(environ) = read_file_bounded(&PathBuf::from(format!("/proc/{pid}/environ")), MAX_PROC_ENVIRON_BYTES)
            && environ_contains_entry(&environ, containment.marker_entry.as_bytes())
        {
            return true;
        }
        if let Some(ref expected_cwd) = containment.scratch_dir
            && let Ok(cwd) = fs::read_link(format!("/proc/{pid}/cwd"))
            && cwd == *expected_cwd
        {
            return true;
        }
        if !containment.pipe_targets.is_empty()
            && let Ok(fds) = fs::read_dir(format!("/proc/{pid}/fd"))
        {
            for fd_entry in fds.flatten().take(MAX_PROC_FDS) {
                if let Ok(target) = fs::read_link(fd_entry.path())
                    && containment.pipe_targets.iter().any(|pipe| pipe.as_os_str() == target.as_os_str())
                {
                    return true;
                }
            }
        }
        false
    }

    #[cfg(target_os = "linux")]
    fn read_file_bounded(path: &Path, limit: u64) -> Option<Vec<u8>> {
        let file = fs::File::open(path).ok()?;
        let mut buf = Vec::new();
        let _ = file.take(limit).read_to_end(&mut buf).ok()?;
        Some(buf)
    }

    /// Match an exact NUL-delimited `KEY=VALUE` entry in a Linux `/proc/<pid>/environ` buffer.
    #[cfg(any(target_os = "linux", test))]
    fn environ_contains_entry(environ: &[u8], expected_entry: &[u8]) -> bool {
        environ.split(|&byte| byte == 0).any(|entry| entry == expected_entry)
    }

    #[cfg(not(target_os = "linux"))]
    fn capture_ps_snapshot(marker_entry: &str) -> Vec<ProcessRecord> {
        let mut cmd = if Path::new("/bin/ps").is_file() { Command::new("/bin/ps") } else { Command::new("ps") };
        #[cfg(target_os = "macos")]
        cmd.args(["-xww", "-E", "-o", "pid=,ppid=,pgid=,stat=,command="]);
        #[cfg(any(target_os = "freebsd", target_os = "openbsd", target_os = "netbsd", target_os = "dragonfly"))]
        cmd.args(["-xww", "-e", "-o", "pid=,ppid=,pgid=,stat=,command="]);
        #[cfg(not(any(target_os = "macos", target_os = "freebsd", target_os = "openbsd", target_os = "netbsd", target_os = "dragonfly")))]
        cmd.args(["-x", "-o", "pid=,ppid=,pgid=,stat=,args="]);

        cmd.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::null());
        let Ok(mut child) = cmd.spawn() else {
            return Vec::new();
        };
        let mut raw = Vec::new();
        if let Some(stdout) = child.stdout.take() {
            let _ = stdout.take(MAX_PS_OUTPUT_BYTES).read_to_end(&mut raw);
        }
        let _ = child.wait();
        let text = String::from_utf8_lossy(&raw);
        parse_ps_snapshot(&text, marker_entry)
    }

    /// Parse `ps -o pid=,ppid=,pgid=,stat=,command=` output into process records.
    #[cfg(any(not(target_os = "linux"), test))]
    fn parse_ps_snapshot(output: &str, marker_entry: &str) -> Vec<ProcessRecord> {
        let mut records = Vec::new();
        for line in output.lines().take(MAX_SNAPSHOT_PROCESSES) {
            if let Some(rec) = parse_ps_line(line, marker_entry) {
                records.push(rec);
            }
        }
        records
    }

    #[cfg(any(not(target_os = "linux"), test))]
    fn parse_ps_line(line: &str, marker_entry: &str) -> Option<ProcessRecord> {
        let trimmed = line.trim_start();
        let (pid_str, rest) = split_first_token(trimmed)?;
        let (ppid_str, rest) = split_first_token(rest)?;
        let (pgid_str, rest) = split_first_token(rest)?;
        let (stat_str, command_and_env) = split_first_token(rest)?;

        let pid = pid_str.parse::<i32>().ok()?;
        let ppid = ppid_str.parse::<i32>().ok()?;
        let pgid = pgid_str.parse::<i32>().ok()?;
        if pid <= 1 {
            return None;
        }
        let is_zombie = stat_str.starts_with('Z');
        let tagged = !is_zombie && token_present_in_ps_tail(command_and_env, marker_entry);
        Some(ProcessRecord { pid, ppid, pgid, is_zombie, starttime: None, tagged })
    }

    #[cfg(any(not(target_os = "linux"), test))]
    fn split_first_token(input: &str) -> Option<(&str, &str)> {
        let trimmed = input.trim_start();
        if trimmed.is_empty() {
            return None;
        }
        match trimmed.find(char::is_whitespace) {
            Some(idx) => Some((&trimmed[..idx], trimmed[idx..].trim_start())),
            None => Some((trimmed, "")),
        }
    }

    #[cfg(any(not(target_os = "linux"), test))]
    fn token_present_in_ps_tail(tail: &str, marker_entry: &str) -> bool {
        !marker_entry.is_empty() && tail.split_whitespace().any(|token| token == marker_entry)
    }

    /// Check whether a process is still actively running (treating zombies as already dead).
    #[cfg(test)]
    pub(crate) fn is_pid_running(pid: i32) -> io::Result<bool> {
        #[cfg(target_os = "linux")]
        {
            let stat = match fs::read_to_string(format!("/proc/{pid}/stat")) {
                Ok(stat) => stat,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
                Err(error) => return Err(error),
            };
            let Some((_, rest)) = stat.rsplit_once(')') else {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "unexpected /proc/<pid>/stat format"));
            };
            Ok(!rest.trim_start().starts_with('Z'))
        }
        #[cfg(not(target_os = "linux"))]
        {
            let Some(raw) = Pid::from_raw(pid) else {
                return Ok(false);
            };
            match rustix::process::test_kill_process(raw) {
                Ok(()) => {}
                Err(rustix::io::Errno::SRCH) => return Ok(false),
                Err(error) => return Err(io::Error::from(error)),
            }
            let ps_bin = if Path::new("/bin/ps").is_file() { "/bin/ps" } else { "ps" };
            let output = Command::new(ps_bin)
                .args(["-o", "stat=", "-p", &pid.to_string()])
                .stdin(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .output();
            if let Ok(output) = output {
                let state = String::from_utf8_lossy(&output.stdout);
                let trimmed = state.trim();
                if trimmed.is_empty() || trimmed.starts_with('Z') {
                    return Ok(false);
                }
            }
            Ok(true)
        }
    }

    #[cfg(test)]
    mod tests {
        use std::collections::{BTreeMap, BTreeSet};

        use super::{ProcessRecord, environ_contains_entry, parse_ps_snapshot, protected_sets, select_targets};

        #[cfg(target_os = "linux")]
        use super::parse_linux_proc_stat;

        #[cfg(target_os = "linux")]
        #[test]
        fn linux_proc_stat_handles_spaces_and_parentheses_in_comm() {
            let line = "4242 (py (helper) 1) S 1000 4242 4242 0 -1 4194304 1 0 0 0 0 0 0 0 20 0 1 0 987654321 100 10";
            let stat = parse_linux_proc_stat(line).expect("valid stat line");
            assert!(!stat.is_zombie);
            assert_eq!(stat.ppid, 1000);
            assert_eq!(stat.pgid, 4242);
            assert_eq!(stat.starttime, 987_654_321);

            let zombie = "4243 (defunct) Z 1 4243 4243 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 555 0 0";
            let zstat = parse_linux_proc_stat(zombie).expect("valid zombie line");
            assert!(zstat.is_zombie);
            assert_eq!(zstat.starttime, 555);
        }

        #[test]
        fn environ_matching_requires_exact_nul_delimited_entry() {
            let env = b"PATH=/usr/bin\0WORDCRAFT_KERNEL_RUN=abc123\0HOME=/tmp\0";
            assert!(environ_contains_entry(env, b"WORDCRAFT_KERNEL_RUN=abc123"));
            assert!(!environ_contains_entry(env, b"WORDCRAFT_KERNEL_RUN=abc12"));
            assert!(!environ_contains_entry(env, b"WORDCRAFT_KERNEL_RUN=abc1234"));
            assert!(!environ_contains_entry(b"OTHER=WORDCRAFT_KERNEL_RUN=abc123\0", b"WORDCRAFT_KERNEL_RUN=abc123"));
        }

        #[test]
        fn ps_snapshot_parses_rows_and_matches_exact_marker_token() {
            let output = "\
  100    1  100 Ss   /sbin/launchd
  200  100  200 S    python3 script.py WORDCRAFT_KERNEL_RUN=tok1
  201    1  201 S    python3 -c sleep FOO=1 WORDCRAFT_KERNEL_RUN=tok1 BAR=2
  202    1  202 S    python3 -c sleep WORDCRAFT_KERNEL_RUN=tok1_extra
  203  200  203 Z+   <defunct> WORDCRAFT_KERNEL_RUN=tok1
";
            let records = parse_ps_snapshot(output, "WORDCRAFT_KERNEL_RUN=tok1");
            assert_eq!(records.len(), 5);
            assert!(!records[0].tagged);
            assert!(records[1].tagged);
            assert!(records[2].tagged);
            assert!(!records[3].tagged, "prefix match must not trigger");
            assert!(records[4].is_zombie);
            assert!(!records[4].tagged, "zombies are excluded from active targets");
        }

        #[test]
        fn target_selection_closes_over_ppid_and_escaped_pgid_without_touching_ancestors() {
            let snapshot = vec![
                ProcessRecord { pid: 10, ppid: 1, pgid: 10, is_zombie: false, starttime: Some(1), tagged: false },
                ProcessRecord { pid: 20, ppid: 10, pgid: 20, is_zombie: false, starttime: Some(2), tagged: false },
                // Leader spawned by my_pid (20)
                ProcessRecord { pid: 30, ppid: 20, pgid: 30, is_zombie: false, starttime: Some(3), tagged: true },
                // Escaped session leader (cleared env, new pgid 40)
                ProcessRecord { pid: 40, ppid: 30, pgid: 40, is_zombie: false, starttime: Some(4), tagged: false },
                // Grandchild inside escaped group 40 whose parent already exited (ppid = 1)
                ProcessRecord { pid: 41, ppid: 1, pgid: 40, is_zombie: false, starttime: Some(5), tagged: false },
                // Double-forked orphan (ppid = 1, new pgid 50, carries env marker)
                ProcessRecord { pid: 50, ppid: 1, pgid: 50, is_zombie: false, starttime: Some(6), tagged: true },
                // Unrelated sibling spawned by my_pid (20) in another group
                ProcessRecord { pid: 60, ppid: 20, pgid: 60, is_zombie: false, starttime: Some(7), tagged: false },
            ];
            let (protected_pids, protected_pgids) = protected_sets(&snapshot, 20);
            assert!(protected_pids.contains(&10));
            assert!(protected_pids.contains(&20));
            assert!(protected_pgids.contains(&10));
            assert!(protected_pgids.contains(&20));

            let selected = select_targets(&snapshot, Some(30), &protected_pids, &protected_pgids, &BTreeMap::new());
            let pids: BTreeSet<i32> = selected.iter().map(|rec| rec.pid).collect();
            assert_eq!(pids, BTreeSet::from([40, 41, 50]));
            assert!(!pids.contains(&10));
            assert!(!pids.contains(&20));
            assert!(!pids.contains(&60));
        }
    }
}

#[cfg(not(unix))]
mod fallback {
    use std::io;
    use std::path::Path;
    use std::process::{ChildStderr, ChildStdout, Command, ExitStatus};
    use std::thread;
    use std::time::{Duration, Instant};

    use crate::process_group::GroupChild;

    use super::{PROCESS_TREE_CLEANUP_TIMEOUT, REAP_POLL_INTERVAL, WAIT_POLL_INTERVAL};

    pub(crate) struct RunContainment;

    impl RunContainment {
        pub(crate) fn new(_temp_dir: &Path, _uses_exclusive_cwd: bool) -> io::Result<Self> {
            Ok(Self)
        }

        pub(crate) fn configure_command(&self, _cmd: &mut Command) {}
    }

    pub(crate) struct ProcessTreeGuard {
        child: GroupChild,
        cleaned_up: bool,
        exit_status: Option<ExitStatus>,
    }

    impl ProcessTreeGuard {
        pub(crate) fn new(child: GroupChild, _containment: RunContainment) -> Self {
            Self { child, cleaned_up: false, exit_status: None }
        }

        pub(crate) fn child_mut(&mut self) -> &mut GroupChild {
            &mut self.child
        }

        pub(crate) fn register_pipes(&mut self, _stdout: Option<&ChildStdout>, _stderr: Option<&ChildStderr>) {}

        pub(crate) fn wait_with_timeout(&mut self, timeout: Duration) -> bool {
            let start = Instant::now();
            loop {
                match self.child.inner().try_wait() {
                    Ok(Some(status)) => {
                        self.exit_status = Some(status);
                        return false;
                    }
                    Ok(None) => {
                        if start.elapsed() >= timeout {
                            return true;
                        }
                        thread::sleep(WAIT_POLL_INTERVAL);
                    }
                    Err(_) => return false,
                }
            }
        }

        pub(crate) fn terminate_and_reap(mut self) -> Option<ExitStatus> {
            self.cleanup();
            self.exit_status
        }

        fn cleanup(&mut self) {
            if self.cleaned_up {
                return;
            }
            self.cleaned_up = true;
            if self.exit_status.is_some() {
                return;
            }
            let _ = self.child.kill();
            let started = Instant::now();
            loop {
                match self.child.try_wait() {
                    Ok(Some(_)) | Err(_) => return,
                    Ok(None) if started.elapsed() >= PROCESS_TREE_CLEANUP_TIMEOUT => return,
                    Ok(None) => thread::sleep(REAP_POLL_INTERVAL),
                }
            }
        }
    }

    impl Drop for ProcessTreeGuard {
        fn drop(&mut self) {
            self.cleanup();
        }
    }
}

#[cfg(all(unix, test))]
pub(crate) use unix::is_pid_running;
#[cfg(unix)]
pub(crate) use unix::{ProcessTreeGuard, RunContainment};

#[cfg(not(unix))]
pub(crate) use fallback::{ProcessTreeGuard, RunContainment};
