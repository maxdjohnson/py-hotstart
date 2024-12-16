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
use mio::{Events, Interest, Poll, Token};
use std::os::unix::net;
use mio::net::{UnixListener, UnixStream};
use std::path::Path;
use signal_hook::low_level::pipe;
use signal_hook::consts::{SIGCHLD, SIGTERM, SIGINT};


use std::fmt;
use nix::unistd::Pid;
use std::str::FromStr;
use nix::pty::PtyMaster;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ChildId { }

impl fmt::Display for ChildId {
    // implementation omitted
}

impl FromStr for ChildId {
    // implementation omitted
}

impl Supervisor {
    pub fn spawn_interpreter(&mut self, prelude_code: Option<&str>) -> Result<(ChildId, PtyMaster), Box<dyn std::error::Error>> {
        unimplemented!()
    }

    pub fn get_exit_code(&mut self, child_id: ChildId) -> Result<i32, Box<dyn std::error::Error>> {
        unimplemented!()
    }

    pub fn handle_sigchld(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        unimplemented!()
    }
}

// For TIOCSCTTY
nix::ioctl_write_int_bad!(ioctl_set_ctty, libc::TIOCSCTTY);

const SOCKET_PATH: &str = "/tmp/py_hotstart.sock";

// Tokens for event sources
const LISTENER: Token = Token(0);
const SIGCHLD_TOKEN: Token = Token(1);
const SIGTERM_TOKEN: Token = Token(2);

struct InterpreterState {
    pid: Pid,
    pty_master_fd: PtyMaster,
}

struct ServerState {
    listener: UnixListener,
    current_interpreter: Option<InterpreterState>,
    prelude_code: Option<String>,
    sigchld_fd: UnixStream,
    sigterm_fd: UnixStream,
}

impl ServerState {
    fn new() -> Result<ServerState> {
        let (sigchld_fd, sigterm_fd)= {
            let (sigchld_r, sigchld_w) = net::UnixStream::pair()?;
            let (sigterm_r, sigterm_w) = net::UnixStream::pair()?;
            let sigint_w = sigterm_w.try_clone()?;
            for socket in &[&sigchld_r, &sigchld_w, &sigterm_r, &sigterm_w, &sigint_w] {
                let _ = socket.set_nonblocking(true).context("Failed to set socket to non-blocking")?;
            }
            pipe::register(SIGCHLD, sigchld_w)?;
            pipe::register(SIGTERM, sigterm_w)?;
            pipe::register(SIGINT, sigint_w)?;
            (UnixStream::from_std(sigchld_r), UnixStream::from_std(sigterm_r))
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
            sigchld_fd,
            sigterm_fd,
        })
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
            let response = format!("OK {}", interp.pid);
            let iov = [IoSlice::new(response.as_bytes())];
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

    fn handle_exitcode_request(&mut self, pid_str: &str, stream: &mut UnixStream) -> Result<()> {
        // Parse the PID from the input
        let pid = nix::unistd::Pid::from_raw(i32::from_str(pid_str.trim())?);
        // TODO wait for pid
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
            let prelude = req.strip_prefix("INIT ").unwrap();
            self.handle_initialize(prelude)
                .context("Failed to initialize with new prelude")?;
            stream.write_all(b"OK").context("Failed to write response")?;
        } else if req == "RUN" {
            self.handle_run_request(&mut stream).context("Failed to handle RUN request")?;
        } else if req.starts_with("EXITCODE ") {
            let pid_str = req.strip_prefix("EXITCODE ").unwrap();
            self.handle_exitcode_request(pid_str, &mut stream).context("Failed to handle EXITCODE request")?;
        } else {
            stream.write_all(b"UNKNOWN").context("Failed to write UNKNOWN response")?;
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
