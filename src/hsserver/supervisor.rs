use anyhow::{bail, Context, Result};
use nix::fcntl::{open, OFlag};
use nix::libc;
use nix::pty::{grantpt, posix_openpt, ptsname, unlockpt, PtyMaster};
use nix::sys::stat::Mode;
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::Pid;
use nix::unistd::{close, dup2, execvp, fork, getpid, setsid, tcsetpgrp, ForkResult};
use std::collections::HashMap;
use std::ffi::CString;
use std::fmt;
use std::str::FromStr;
use std::time::{Duration, Instant};

const SCRIPT: &str = include_str!("../pyhotstart.py");
const SCRIPT_PATH: &str = "/tmp/pyhotstart.py";

// For TIOCSCTTY
nix::ioctl_write_int_bad!(ioctl_set_ctty, libc::TIOCSCTTY);

// Pair of child_id and Pid
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ChildId {
    id: u32,
    pid: Pid,
}

impl fmt::Display for ChildId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "({},{})", self.id, self.pid.as_raw())
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct ParseChildIdError;

impl std::fmt::Display for ParseChildIdError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid ChildId")
    }
}

impl std::error::Error for ParseChildIdError {}

impl FromStr for ChildId {
    type Err = ParseChildIdError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (x, y) = s
            .strip_prefix('(')
            .and_then(|s| s.strip_suffix(')'))
            .and_then(|s| s.split_once(','))
            .ok_or(ParseChildIdError)?;

        let id = x.parse::<u32>().map_err(|_| ParseChildIdError)?;
        let pid = y.parse::<libc::pid_t>().map_err(|_| ParseChildIdError)?;

        Ok(ChildId {
            id,
            pid: Pid::from_raw(pid),
        })
    }
}

impl ChildId {
    fn new(id: u32, pid: Pid) -> Self {
        ChildId { id, pid }
    }
    pub fn get_pid(&self) -> Pid {
        self.pid
    }
}

pub struct Supervisor {
    next_child_id: u32,
    running_children: HashMap<Pid, u32>,
    exit_info: ExitInfoRecord,
}

impl Supervisor {
    pub fn new() -> Self {
        Supervisor {
            next_child_id: 1,
            running_children: HashMap::new(),
            exit_info: ExitInfoRecord::new(128),
        }
    }

    pub fn spawn_interpreter(
        &mut self,
        prelude_code: Option<&str>,
    ) -> Result<(ChildId, PtyMaster)> {
        let interpreter = Interpreter::spawn(prelude_code)?;
        let child_id = self.next_child_id;
        self.next_child_id += 1;
        self.running_children.insert(interpreter.pid, child_id);
        Ok((
            ChildId::new(child_id, interpreter.pid),
            interpreter.pty_master_fd,
        ))
    }

    pub fn get_exit_code(&mut self, child_id: ChildId) -> Result<i32> {
        // First, check if we already have the exit code recorded
        if let Some(code) = self.exit_info.get(child_id.id) {
            return Ok(code);
        }

        // Not recorded yet - if block is true, try to wait.
        self.wait(Some(child_id.pid), None)?;
        if let Some(code) = self.exit_info.get(child_id.id) {
            return Ok(code);
        }
        bail!("could not get exit code for child {}", child_id);
    }

    pub fn kill(&mut self, child_id: ChildId) -> Result<i32> {
        if self.running_children.contains_key(&child_id.pid) {
            // Send SIGTERM to request graceful termination
            let _ = nix::sys::signal::kill(child_id.pid, nix::sys::signal::SIGTERM);

            let start = Instant::now();
            let timeout = Duration::from_secs(2);

            // Wait for child to exit
            let mut status = -1;
            while start.elapsed() < timeout {
                self.wait(Some(child_id.pid), Some(WaitPidFlag::WNOHANG))?;
                if let Some(code) = self.exit_info.get(child_id.id) {
                    status = code;
                } else {
                    // Not exited yet, wait a bit longer
                    std::thread::sleep(Duration::from_millis(20));
                }
            }

            // If still running after timeout, send SIGKILL and block until exited
            if status == -1 {
                let _ = nix::sys::signal::kill(child_id.pid, nix::sys::signal::SIGKILL);
                self.wait(Some(child_id.pid), None)?;
            }
        }
        self.exit_info.get(child_id.id).context("missing exit info")
    }

    pub fn handle_sigchld(&mut self) -> Result<()> {
        self.wait(None, Some(WaitPidFlag::WNOHANG))
    }

    fn wait(&mut self, pid: Option<Pid>, options: Option<WaitPidFlag>) -> Result<()> {
        loop {
            match waitpid(pid, options) {
                Ok(WaitStatus::Exited(pid, code)) => self.child_exit(&pid, code)?,
                Ok(WaitStatus::Signaled(pid, signal, _)) => {
                    self.child_exit(&pid, 128 + signal as i32)?
                }
                Ok(WaitStatus::StillAlive) => break,
                Ok(_) => break,
                Err(nix::Error::ECHILD) => break,
                Err(e) => {
                    bail!(e)
                }
            }
        }
        Ok(())
    }
    fn child_exit(&mut self, pid: &Pid, exit_code: i32) -> Result<()> {
        let id = self
            .running_children
            .remove(pid)
            .with_context(|| format!("unrecognized pid {}", pid))?;
        self.exit_info.set(id, exit_code);
        Ok(())
    }
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        let child_ids: Vec<ChildId> = self
            .running_children
            .iter()
            .map(|(pid, id)| ChildId::new(*id, *pid))
            .collect();
        for id in child_ids {
            if let Err(e) = self.kill(id) {
                eprintln!("Failed to kill child process {}: {}", id, e);
            }
        }
    }
}

// A ring buffer that stores exit information for child processes.
struct ExitInfoRecord {
    child_ids: Vec<u32>,
    exit_codes: Vec<i32>,
    limit: usize,
    start: usize, // start index of the ring buffer
    count: usize, // number of elements currently in the ring
}

impl ExitInfoRecord {
    fn new(limit: usize) -> Self {
        Self {
            child_ids: vec![0; limit],
            exit_codes: vec![0; limit],
            limit,
            start: 0,
            count: 0,
        }
    }

    fn set(&mut self, child_id: u32, exit_code: i32) {
        if self.count < self.limit {
            self.child_ids[self.count] = child_id;
            self.exit_codes[self.count] = exit_code;
            self.count += 1;
        } else {
            // Overwrite the oldest entry
            self.child_ids[self.start] = child_id;
            self.exit_codes[self.start] = exit_code;
            self.start = (self.start + 1) % self.limit;
        }
    }

    fn get(&self, child_id: u32) -> Option<i32> {
        self.child_ids
            .iter()
            .enumerate()
            .find(|&(_, &id)| id == child_id)
            .map(|(i, _)| self.exit_codes[i])
    }
}

struct Interpreter {
    pid: Pid,
    pty_master_fd: PtyMaster,
}

impl Interpreter {
    fn spawn(prelude_code: Option<&str>) -> Result<Self> {
        let master_fd =
            posix_openpt(OFlag::O_RDWR | OFlag::O_CLOEXEC).context("Failed to open PTY master")?;
        grantpt(&master_fd).context("Failed to grant PTY")?;
        unlockpt(&master_fd).context("Failed to unlock PTY")?;

        let slave_name = unsafe { ptsname(&master_fd) }.context("Failed to get PTY slave name")?;
        let slave_path: &str = slave_name.as_ref();

        match unsafe { fork() }.context("fork failed")? {
            ForkResult::Parent { child } => Ok(Interpreter {
                pid: child,
                pty_master_fd: master_fd,
            }),
            ForkResult::Child => {
                // Child: setsid, set controlling TTY
                setsid().expect("setsid failed");

                // Attach tty slave device to stdin, stdout, stderr
                {
                    let slave_fd = open(
                        std::path::Path::new(slave_path),
                        OFlag::O_RDWR,
                        Mode::empty(),
                    )
                    .expect("Failed to open pty slave");
                    dup2(slave_fd, 0).expect("dup2 stdin failed");
                    dup2(slave_fd, 1).expect("dup2 stdout failed");
                    dup2(slave_fd, 2).expect("dup2 stderr failed");
                    if slave_fd > 2 {
                        close(slave_fd).expect("failed to close pty slave fd");
                    }
                }

                // TIOCSCTTY to acquire controlling terminal
                unsafe { ioctl_set_ctty(0, 0) }.expect("ioctl(TIOCSCTTY) failed");

                // Set foreground process group
                let pid = getpid();
                tcsetpgrp(std::io::stdin(), pid).expect("tcsetpgrp failed");

                // Prepare python command
                let script_with_prelude = SCRIPT.replace("# prelude", prelude_code.unwrap_or(""));
                std::fs::write(SCRIPT_PATH, script_with_prelude).context("Failed to write to temp file")?;
                let python = CString::new("python3").unwrap();
                let args = [
                    python.clone(),
                    CString::new(SCRIPT_PATH).unwrap(),
                ];
                execvp(&python, &args).expect("execvp failed");
                unreachable!()
            }
        }
    }
}
