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
                    let slave_fd = open(
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


fn main() -> Result<()> {
    let mut state = ServerState::new()?;

    let mut poll = Poll::new()?;
    let mut events = Events::with_capacity(128);

    // Register the listener
    poll.registry().register(&mut state.listener, LISTENER, Interest::READABLE)?;

    // Register the signal FDs
    // Reading from these will tell us when a signal has occurred
    poll.registry().register(&mut state.sigchld_fd, SIGCHLD_TOKEN, Interest::READABLE)?;
    poll.registry().register(&mut state.sigterm_fd, SIGTERM_TOKEN, Interest::READABLE)?;

    // Store active clients
    let mut clients: HashMap<Token, MioUnixStream> = HashMap::new();
    let mut next_token_id = 3; // Start after 2 since we used 0,1,2 already

    loop {
        poll.poll(&mut events, None)?;

        for event in events.iter() {
            match event.token() {
                LISTENER => {
                    // Accept new connections
                    loop {
                        match state.listener.accept() {
                            Ok((mut stream, addr)) => {
                                let token = Token(next_token_id);
                                next_token_id += 1;

                                // Register the new client
                                poll.registry().register(&mut stream, token, Interest::READABLE | Interest::WRITABLE)?;
                                clients.insert(token, stream);

                                eprintln!("New connection from {:?}", addr);
                            }
                            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                                // No more clients to accept
                                break;
                            }
                            Err(e) => {
                                eprintln!("Accept error: {}", e);
                                break;
                            }
                        }
                    }
                }

                SIGCHLD_TOKEN => {
                    // SIGCHLD occurred. Read from the pipe to clear it.
                    handle_signal_fd(&mut state.sigchld_fd, "SIGCHLD")?;
                    // TODO: handle child process logic here (e.g., waitpid)
                }

                SIGTERM_TOKEN => {
                    // SIGTERM or SIGINT occurred. Read from the pipe to clear it.
                    handle_signal_fd(&mut state.sigterm_fd, "SIGTERM/SIGINT")?;
                    // TODO: handle graceful shutdown logic here
                    eprintln!("Received SIGTERM/SIGINT, shutting down...");
                    return Ok(());
                }

                token => {
                    // Data available on a client connection or it’s writable
                    if let Some(stream) = clients.get_mut(&token) {
                        if event.is_readable() {
                            let mut buf = [0u8; 1024];
                            match stream.read(&mut buf) {
                                Ok(0) => {
                                    // Client disconnected
                                    eprintln!("Client {:?} disconnected", token);
                                    clients.remove(&token);
                                }
                                Ok(n) => {
                                    eprintln!("Read {} bytes from {:?}", n, token);
                                    // Echo back for demonstration
                                    if event.is_writable() {
                                        let _ = stream.write_all(&buf[..n]);
                                    }
                                }
                                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                                    // No more data to read now
                                }
                                Err(e) => {
                                    eprintln!("Error reading from client {:?}: {}", token, e);
                                    clients.remove(&token);
                                }
                            }
                        }
                        // event.is_writable() can also be checked here if you had pending writes
                    }
                }
            }
        }
    }
}

fn handle_signal_fd(stream: &mut UnixStream, signal_name: &str) -> Result<()> {
    let mut buf = [0u8; 64];
    // Just read whatever is there to clear the event
    match stream.read(&mut buf) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
            // Nothing to read; ignore
        }
        Err(e) => {
            eprintln!("Error reading {} signal pipe: {}", signal_name, e);
        }
    }
    Ok(())
}
