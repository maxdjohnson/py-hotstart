use anyhow::{bail, Context, Result};
use nix::fcntl::{open, OFlag};
use std::os::unix::net::UnixStream;
use std::fs::File;
use nix::libc;
use nix::pty::{grantpt, posix_openpt, ptsname, unlockpt};
use nix::sys::stat::Mode;
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::Pid;
use nix::unistd::{close, dup2, execvp, fork, getpid, setsid, tcsetpgrp, ForkResult};
use std::collections::HashMap;
use std::ffi::CString;
use std::os::fd::{AsRawFd, IntoRawFd, FromRawFd};
use std::time::{Duration, Instant};
use crate::interpreter::{ChildId, Interpreter};

const SCRIPT: &str = include_str!("../pyhotstart.py");
const SCRIPT_PATH: &str = "/tmp/pyhotstart.py";

// For TIOCSCTTY
nix::ioctl_write_int_bad!(ioctl_set_ctty, libc::TIOCSCTTY);

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
    ) -> Result<Interpreter> {
        let interpreter = spawn(self.next_child_id, prelude_code)?;
        let child_id = self.next_child_id;
        self.next_child_id += 1;
        self.running_children.insert(interpreter.id().pid, child_id);
        Ok(interpreter)
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

    pub fn kill(&mut self, child_id: &ChildId) -> Result<i32> {
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
            if let Err(e) = self.kill(&id) {
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

fn spawn(id: u32, prelude_code: Option<&str>) -> Result<Interpreter> {
    // Set up dedicated PTY for interpreter's stdio
    let master_fd =
        posix_openpt(OFlag::O_RDWR | OFlag::O_CLOEXEC).context("Failed to open PTY master")?;
    grantpt(&master_fd).context("Failed to grant PTY")?;
    unlockpt(&master_fd).context("Failed to unlock PTY")?;

    let slave_name = unsafe { ptsname(&master_fd) }.context("Failed to get PTY slave name")?;
    let slave_path: &str = slave_name.as_ref();

    // Create a separate stream for sending instructions to the running interpreter.
    let (control_r, control_w) = UnixStream::pair().context("Failed to create control socket pair")?;
    debug_assert!(control_r.as_raw_fd() > 3, "control_r fd is too low");

    match unsafe { fork() }.context("fork failed")? {
        ForkResult::Parent { child } => Ok(Interpreter::new(
            ChildId::new(id, child),
            control_w,
            unsafe { File::from_raw_fd(master_fd.into_raw_fd()) },
        )),
        ForkResult::Child => {
            // Child: setsid, set controlling TTY
            setsid().expect("setsid failed");

            // Attach tty slave device to stdin, stdout, stderr
            {
                // Open slave fd
                let slave_fd = open(
                    std::path::Path::new(slave_path),
                    OFlag::O_RDWR,
                    Mode::empty(),
                )
                .expect("Failed to open pty slave");

                // Assign to stdin, stdout, stderr
                dup2(slave_fd, 0).expect("dup2 stdin failed");
                dup2(slave_fd, 1).expect("dup2 stdout failed");
                dup2(slave_fd, 2).expect("dup2 stderr failed");
                if slave_fd > 2 {
                    close(slave_fd).expect("failed to close pty slave fd");
                }
            }

            // Dup control_r fd to 3 so that it survives exec and can be used by interpreter
            dup2(control_r.as_raw_fd(), 3).expect("dup2 control failed");

            // TIOCSCTTY to acquire controlling terminal
            unsafe { ioctl_set_ctty(0, 0) }.expect("ioctl(TIOCSCTTY) failed");

            // Set foreground process group
            let pid = getpid();
            tcsetpgrp(std::io::stdin(), pid).expect("tcsetpgrp failed");

            // Prepare python command
            let script_with_prelude = SCRIPT.replace("# prelude", prelude_code.unwrap_or(""));
            std::fs::write(SCRIPT_PATH, script_with_prelude)
                .context("Failed to write to temp file")?;
            let python = CString::new("python3").unwrap();
            let args = [python.clone(), CString::new(SCRIPT_PATH).unwrap()];
            execvp(&python, &args).expect("execvp failed");
            unreachable!()
        }
    }
}
