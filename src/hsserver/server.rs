use anyhow::{Context, Result, bail};
use nix::pty::{grantpt, posix_openpt, ptsname, unlockpt, PtyMaster};
use nix::libc;
use nix::sys::socket::{ControlMessage, MsgFlags, sendmsg};
use nix::sys::stat::Mode;
use nix::unistd::{fork, ForkResult, setsid, dup2, getpid, execvp, tcsetpgrp, close};
use std::ffi::CString;
use std::str::FromStr;
use std::fs;
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use std::io::{Read, Write, IoSlice};
use nix::unistd::Pid;
use std::os::fd::{AsFd, AsRawFd, IntoRawFd, OwnedFd, RawFd};
use nix::fcntl::{fcntl, FcntlArg, FdFlag, open, OFlag};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixStream, UnixListener};
use std::path::Path;
use signal_hook::low_level::pipe;
use signal_hook::consts::{SIGCHLD, SIGTERM, SIGINT};
use crate::hsserver::supervisor::{ChildId, Supervisor};

// For TIOCSCTTY
nix::ioctl_write_int_bad!(ioctl_set_ctty, libc::TIOCSCTTY);

const SOCKET_PATH: &str = "/tmp/py_hotstart.sock";

struct InterpreterState {
    id: ChildId,
    pty_master_fd: PtyMaster,
}

struct ServerState {
    listener: UnixListener,
    current_interpreter: Option<InterpreterState>,
    prelude_code: Option<String>,
    supervisor: Supervisor,
    sigchld_fd: UnixStream,
    sigterm_fd: UnixStream,
}

impl ServerState {
    fn new() -> Result<ServerState> {
        let (sigchld_fd, sigterm_fd)= {
            let (sigchld_r, sigchld_w) = UnixStream::pair()?;
            let (sigterm_r, sigterm_w) = UnixStream::pair()?;
            let sigint_w = sigterm_w.try_clone()?;
            for socket in &[&sigchld_r, &sigchld_w, &sigterm_r, &sigterm_w, &sigint_w] {
                let _ = socket.set_nonblocking(true).context("Failed to set socket to non-blocking")?;
            }
            pipe::register(SIGCHLD, sigchld_w)?;
            pipe::register(SIGTERM, sigterm_w)?;
            pipe::register(SIGINT, sigint_w)?;
            (sigchld_r, sigterm_r)
        };

        if Path::new(SOCKET_PATH).exists() {
            fs::remove_file(SOCKET_PATH).ok();
        }

        let listener = UnixListener::bind(SOCKET_PATH)
            .context("Failed to bind Unix domain socket")?;
        debug_assert!(has_cloexec(listener.as_raw_fd()), "O_CLOEXEC not set on listener");

        let mut perms = fs::metadata(SOCKET_PATH)?.permissions();
        // Adjust permissions if needed (e.g. 0700)
        perms.set_mode(0o600);
        fs::set_permissions(SOCKET_PATH, perms)?;

        Ok(ServerState {
            listener,
            current_interpreter: None,
            prelude_code: None,
            supervisor: Supervisor::new(),
            sigchld_fd,
            sigterm_fd,
        })
    }

    fn spawn_interpreter(&mut self) -> Result<()> {
        let (id, fd )= self.supervisor.spawn_interpreter(self.prelude_code.as_deref())?;
        self.current_interpreter = Some(InterpreterState{id, pty_master_fd: fd});
        Ok(())
    }

    fn handle_take(&mut self, stream: &mut UnixStream) -> Result<()> {
        if let Some(interp) =  {
            // Send the FD to the client
        } else {
            // No interpreter ready
            stream.write_all(b"ERROR")?;
        }
        Ok(())
    }

    fn handle_exitcode_request(&mut self, id_str: &str, stream: &mut UnixStream) -> Result<()> {
        Ok(())
    }


    fn run(&mut self) -> Result<()> {
        self.spawn_interpreter()?;

        loop {
            // Accept a connection
            let (mut stream, _addr) = match self.listener.accept() {
                Ok(pair) => pair,
                Err(e) => {
                    // If accept fails, just log and continue
                    eprintln!("Accept failed: {}", e);
                    continue;
                }
            };
            debug_assert!(has_cloexec(stream.as_raw_fd()), "O_CLOEXEC not set on stream");

            // Handle requests in a separate block so we can use the ? operator more easily
            // and catch errors to send back to client.
            if let Err(err) = self.handle(&mut stream) {
                // If we get here, an error occurred
                eprintln!("Error handling request: {:?}", err);

                // Attempt to send an error message back to the client
                // Note: The client might have already closed the connection or
                // we might fail again, but we'll try gracefully.
                let err_msg = format!("ERROR: {}\n", err);
                let _ = stream.write_all(err_msg.as_bytes());

                // Continue with next iteration of the loop (keep listening)
                continue;
            }
        }
    }

    fn handle(&mut self, mut stream: &mut UnixStream) -> Result<()> {
        let mut buf = [0u8; 1024];
        let n = stream.read(&mut buf).context("Failed to read request")?;
        if n == 0 {
            // Client closed connection; just continue
            return Ok(());
        }

        let req = String::from_utf8_lossy(&buf[..n]).trim().to_string();

        if req.starts_with("INIT ") {
            // Restart the current interpreter
            let prelude = req.strip_prefix("INIT ").unwrap();
            if let Some(interp) = &self.current_interpreter {
                graceful_kill(interp.id.get_pid())?;
            }
            self.prelude_code = Some(prelude.to_string());
            self.spawn_interpreter()?;
            stream.write_all(b"OK").context("Failed to write response")?;
        } else if req == "TAKE" {
            // Take the interpreter and return it
            let interp = self.current_interpreter.take().context("no interpreter")?;
            let response = format!("OK {}", interp.id);
            let iov = [IoSlice::new(response.as_bytes())];
            let fds = [interp.pty_master_fd.as_raw_fd()];
            let cmsg = [ControlMessage::ScmRights(&fds)];
            sendmsg::<()>(stream.as_raw_fd(), &iov, &cmsg, MsgFlags::empty(), None).context("Failed to sendmsg")?;

            // Spawn a new interpreter for next request
            self.spawn_interpreter()?;
        } else if req.starts_with("EXITCODE ") {
            // Get exit code from supervisor
            let id_str = req.strip_prefix("EXITCODE ").unwrap();
            let child_id = ChildId::from_str(id_str.trim()).with_context(|| format!("child_id='{}'", id_str))?;
            let exit_code = self.supervisor.get_exit_code(child_id)?;
            stream.write_all(format!("OK {}", exit_code).as_bytes())
                .context("Failed to write exit code response")?;
        } else {
            bail!("Unknown command '{}'", req)
        }

        Ok(())
    }
}

fn main() -> Result<()> {
    let mut server = ServerState::new()?;
    server.run()?;
    Ok(())
}


fn graceful_kill(pid: Pid) -> Result<WaitStatus> {
    // Send SIGTERM to request graceful termination
    let _ = nix::sys::signal::kill(pid, nix::sys::signal::SIGTERM);

    // Check if the process ends gracefully within a timeout
    // We'll do a simple retry loop with sleeps. In a real application,
    // you might use more precise timing or asynchronous I/O.
    use std::time::{Duration, Instant};
    let start = Instant::now();
    let timeout = Duration::from_secs(2);

    let mut status = WaitStatus::StillAlive;
    while start.elapsed() < timeout {
        match waitpid(pid, Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::StillAlive) => {
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

    if status != WaitStatus::StillAlive {
        // Still running after graceful timeout, send SIGKILL
        let _ = nix::sys::signal::kill(pid, nix::sys::signal::SIGKILL);
        // Now wait without WNOHANG since SIGKILL should terminate it quickly
        status = waitpid(pid, None).unwrap_or(status);
    }
    Ok(status)
}

fn has_cloexec(read_fd: RawFd) -> bool {
    let read_flags = fcntl(read_fd, FcntlArg::F_GETFD).expect("Failed to get flags");
    FdFlag::from_bits_truncate(read_flags).contains(FdFlag::FD_CLOEXEC)
}
