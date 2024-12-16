use anyhow::{Context, Result};
use nix::fcntl::OFlag;
use nix::pty::{grantpt, posix_openpt, ptsname, unlockpt, PtyMaster};
use nix::libc;
use nix::sys::socket::{ControlMessage, MsgFlags, sendmsg};
use nix::sys::stat::Mode;
use nix::unistd::{fork, ForkResult, setsid, dup2, getpid, execvp, tcsetpgrp, close};
use std::ffi::CString;
use std::fs;
use std::io::{Read, Write, IoSlice, IoSliceMut};
use nix::unistd::Pid;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::{IntoRawFd, FromRawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;

// For TIOCSCTTY
nix::ioctl_write_int_bad!(ioctl_set_ctty, libc::TIOCSCTTY);

const SOCKET_PATH: &str = "/tmp/py_hotstart.sock";

struct InterpreterState {
    pid: Pid,
    pty_master_fd: PtyMaster,
}

struct ServerState {
    listener: UnixListener,
    current_interpreter: Option<InterpreterState>,
    prelude_code: Option<String>,
}

impl ServerState {
    fn new() -> Result<ServerState> {
        if Path::new(SOCKET_PATH).exists() {
            fs::remove_file(SOCKET_PATH).ok();
        }

        let listener = UnixListener::bind(SOCKET_PATH)
            .context("Failed to bind Unix domain socket")?;
        let mut perms = fs::metadata(SOCKET_PATH)?.permissions();
        // Adjust permissions if needed (e.g. 0700)
        perms.set_mode(0o600);
        fs::set_permissions(SOCKET_PATH, perms)?;

        Ok(ServerState {
            listener,
            current_interpreter: None,
            prelude_code: None,
        })
    }

    fn spawn_interpreter(&mut self) -> Result<()> {
        let master_fd = posix_openpt(OFlag::O_RDWR | OFlag::O_CLOEXEC)
            .context("Failed to open PTY master")?;
        grantpt(&master_fd).context("Failed to grant PTY")?;
        unlockpt(&master_fd).context("Failed to unlock PTY")?;

        let slave_name = unsafe{ ptsname(&master_fd) }.context("Failed to get PTY slave name")?;
        let slave_path: &str = slave_name.as_ref();

        let prelude = self.prelude_code.clone();

        match unsafe { fork() }.context("fork failed")? {
            ForkResult::Parent { child } => {
                self.current_interpreter = Some(InterpreterState {
                    pid: child,
                    pty_master_fd: master_fd,
                });
                Ok(())
            }
            ForkResult::Child => {
                // Child: setsid, set controlling TTY
                setsid().expect("setsid failed");

                // Dup slave fd to stdin, stdout, stderr
                {
                    let slave_fd = nix::fcntl::open(
                        std::path::Path::new(slave_path),
                        OFlag::O_RDWR,
                        Mode::empty(),
                    ).expect("Failed to open pty slave");
                    dup2(slave_fd, 0).expect("dup2 stdin failed");
                    dup2(slave_fd, 1).expect("dup2 stdout failed");
                    dup2(slave_fd, 2).expect("dup2 stderr failed");
                    if slave_fd > 2 {
                        close(slave_fd).expect("failed to close pty slave fd");
                    }
                }

                // TIOCSCTTY to acquire controlling terminal
                unsafe {ioctl_set_ctty(0, 0)}.expect("ioctl(TIOCSCTTY) failed");

                // Set foreground process group
                let pid = getpid();
                tcsetpgrp(std::io::stdin(), pid).expect("tcsetpgrp failed");

                // Prepare python command
                let python = CString::new("python3").unwrap();
                let mut cmd = "import sys; code=sys.stdin.read(); exec(code, {'__name__':'__main__'})".to_string();
                if let Some(code) = prelude {
                    cmd = format!("exec({}); {}", json::stringify(code), cmd);
                }
                let args = [python.clone(), CString::new("-c").unwrap(), CString::new(cmd).unwrap()];
                execvp(&python, &args).expect("execvp failed");
                unreachable!()
            }
        }
    }

    fn handle_initialize(&mut self, prelude: &str) -> Result<()> {
        // In a real implementation, we’d kill the old interpreter gracefully and wait on it.
        if let Some(interp) = &self.current_interpreter {
            graceful_kill(interp.pid)?;
        }
        self.prelude_code = Some(prelude.to_string());
        self.spawn_interpreter()?;
        Ok(())
    }

    fn handle_run_request(&mut self, stream: &mut UnixStream) -> Result<()> {
        if let Some(interp) = self.current_interpreter.take() {
            // Send the FD to the client
            let iov = [IoSlice::new(b"OK")];
            let fds = [interp.pty_master_fd.as_raw_fd()];
            let cmsg = [ControlMessage::ScmRights(&fds)];

            let sent = sendmsg::<()>(
                stream.as_raw_fd(),
                &iov,
                &cmsg,
                MsgFlags::empty(),
                None
            ).context("Failed to sendmsg")?;

            if sent != 2 {
                eprintln!("Did not send all bytes expected");
            }

            // Spawn a new interpreter for next request
            self.spawn_interpreter()?;
        } else {
            // No interpreter ready
            stream.write_all(b"ERROR")?;
        }
        Ok(())
    }

    fn handle_exitcode_request(&mut self, stream: &mut UnixStream) -> Result<()> {
        // In a real implementation, we would wait on the old interpreter’s PID if we had stored it.
        // For now, just return 0.
        stream.write_all(b"0")?;
        Ok(())
    }

    fn run(&mut self) -> Result<()> {
        self.spawn_interpreter()?;

        loop {
            let (mut stream, _addr) = self.listener.accept().context("Accept failed")?;

            let mut buf = [0u8; 1024];
            let n = stream.read(&mut buf)?;
            if n == 0 {
                continue;
            }
            let req = String::from_utf8_lossy(&buf[..n]).trim().to_string();

            if req.starts_with("INIT ") {
                let prelude = req.strip_prefix("INIT ").unwrap();
                self.handle_initialize(prelude)?;
                stream.write_all(b"OK")?;
            } else if req == "RUN" {
                self.handle_run_request(&mut stream)?;
            } else if req == "EXITCODE" {
                self.handle_exitcode_request(&mut stream)?;
            } else {
                stream.write_all(b"UNKNOWN")?;
            }
        }
    }
}

fn main() -> Result<()> {
    let mut server = ServerState::new()?;
    server.run()?;
    Ok(())
}


fn graceful_kill(pid: Pid) -> Result<nix::sys::wait::WaitStatus> {
    // Send SIGTERM to request graceful termination
    let _ = nix::sys::signal::kill(pid, nix::sys::signal::SIGTERM);

    // Check if the process ends gracefully within a timeout
    // We'll do a simple retry loop with sleeps. In a real application,
    // you might use more precise timing or asynchronous I/O.
    use std::time::{Duration, Instant};
    let start = Instant::now();
    let timeout = Duration::from_secs(2);

    let mut status = nix::sys::wait::WaitStatus::StillAlive;
    while start.elapsed() < timeout {
        match nix::sys::wait::waitpid(pid, Some(nix::sys::wait::WaitPidFlag::WNOHANG)) {
            Ok(nix::sys::wait::WaitStatus::StillAlive) => {
                // Not exited yet, wait a bit longer
                std::thread::sleep(Duration::from_millis(20));
            }
            Ok(_status) => {
                // Interpreter has exited
                status = _status;
                break;
            }
            Err(e) => {
                eprintln!("Error waiting for interpreter to exit: {}", e);
                break;
            }
        }
    }

    if status != nix::sys::wait::WaitStatus::StillAlive {
        // Still running after graceful timeout, send SIGKILL
        let _ = nix::sys::signal::kill(pid, nix::sys::signal::SIGKILL);
        // Now wait without WNOHANG since SIGKILL should terminate it quickly
        status = nix::sys::wait::waitpid(pid, None).unwrap_or(status);
    }
    Ok(status)
}
