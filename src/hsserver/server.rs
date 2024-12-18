use crate::hsserver::daemon::{daemonize, PidFileGuard};
use crate::hsserver::supervisor::Supervisor;
use crate::interpreter::{ChildId, Interpreter};
use crate::sendfd::SendWithFd;
use anyhow::{bail, Context, Result};
use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
use nix::unistd::{ForkResult, Pid};
use signal_hook::consts::{SIGCHLD, SIGINT, SIGTERM};
use signal_hook::low_level::pipe;
use std::fs;
use std::io::{Read, Write};
use std::os::fd::AsFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process;
use std::str::FromStr;
use std::time::Duration;

use super::daemon::kill_with_timeout;

pub const SOCKET_PATH: &str = "/tmp/py_hotstart.sock";
const PIDFILE_PATH: &str = "/tmp/py_hotstart.pid";

struct ServerState {
    listener: UnixListener,
    current_interpreter: Option<Interpreter>,
    prelude_code: Option<String>,
    supervisor: Supervisor,
    sigchld_fd: UnixStream,
    sigterm_fd: UnixStream,
}

impl ServerState {
    fn new() -> Result<ServerState> {
        if Path::new(SOCKET_PATH).exists() {
            fs::remove_file(SOCKET_PATH).ok();
        }

        let listener =
            UnixListener::bind(SOCKET_PATH).context("Failed to bind Unix domain socket")?;

        eprintln!("Listening on {}", SOCKET_PATH);

        let (sigchld_fd, sigterm_fd) = {
            let (sigchld_r, sigchld_w) = UnixStream::pair()?;
            let (sigterm_r, sigterm_w) = UnixStream::pair()?;
            let sigint_w = sigterm_w.try_clone()?;
            for socket in &[&sigchld_r, &sigchld_w, &sigterm_r, &sigterm_w, &sigint_w] {
                socket
                    .set_nonblocking(true)
                    .context("Failed to set socket to non-blocking")?;
            }
            pipe::register(SIGCHLD, sigchld_w)?;
            pipe::register(SIGTERM, sigterm_w)?;
            pipe::register(SIGINT, sigint_w)?;
            (sigchld_r, sigterm_r)
        };

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

    fn ensure_interpreter(&mut self) -> Result<()> {
        if self.current_interpreter.is_none() {
            self.current_interpreter = Some(
                self.supervisor
                    .spawn_interpreter(self.prelude_code.as_deref())?,
            );
        }
        Ok(())
    }

    fn run(&mut self) -> Result<()> {
        self.ensure_interpreter()?;

        loop {
            match self.run_one() {
                Ok(false) => break,
                Ok(true) => {}
                Err(e) => {
                    eprintln!("error occurred during serve loop: {}", e);
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
            }
        }
        Ok(())
    }

    fn run_one(&mut self) -> Result<bool> {
        let listener_fd = self.listener.as_fd();
        let sigchld_fd = self.sigchld_fd.as_fd();
        let sigterm_fd = self.sigterm_fd.as_fd();

        let mut fds = [
            PollFd::new(listener_fd, PollFlags::POLLIN),
            PollFd::new(sigchld_fd, PollFlags::POLLIN),
            PollFd::new(sigterm_fd, PollFlags::POLLIN),
        ];

        // Wait for input or signal
        loop {
            match poll(&mut fds, PollTimeout::NONE) {
                Ok(0) => continue,
                Ok(_) => break,
                Err(nix::Error::EINTR) => continue,
                Err(e) => bail!("poll failed: {}", e),
            };
        }

        let listener_ready = fds[0]
            .revents()
            .map_or(false, |r| r.contains(PollFlags::POLLIN));
        let sigchld_ready = fds[1]
            .revents()
            .map_or(false, |r| r.contains(PollFlags::POLLIN));
        let sigterm_ready = fds[2]
            .revents()
            .map_or(false, |r| r.contains(PollFlags::POLLIN));

        if sigchld_ready {
            let mut buf = [0u8; 1];
            self.sigchld_fd
                .read_exact(&mut buf)
                .context("sigchld_fd.read_exact error")?;
            self.supervisor.handle_sigchld()?;
        }

        if sigterm_ready {
            let mut buf = [0u8; 1];
            self.sigterm_fd
                .read_exact(&mut buf)
                .context("sigterm_fd.read_exact error")?;
            eprintln!("Received SIGTERM or SIGINT, shutting down gracefully.");
            return Ok(false);
        }

        if listener_ready {
            let (mut stream, _addr) = self.listener.accept().context("accept failed")?;
            if let Err(err) = self.handle(&mut stream) {
                eprintln!("Error handling request: {:?}", err);
                let err_msg = format!("ERROR: {}\n", err);
                let _ = stream.write_all(err_msg.as_bytes());
            }
        }
        Ok(true)
    }

    fn handle(&mut self, stream: &mut UnixStream) -> Result<()> {
        let mut buf = [0u8; 1024];
        let n = stream.read(&mut buf).context("Failed to read request")?;
        if n == 0 {
            // Client closed connection; just continue
            return Ok(());
        }
        let req = String::from_utf8_lossy(&buf[..n]);
        eprintln!("Received request: {:?}", req);

        if req.starts_with("INIT ") {
            // Update prelude
            let prelude = req.strip_prefix("INIT ").unwrap();
            self.prelude_code = Some(prelude.to_string());

            // Kill current interpreter (if present)
            if let Some(interp) = &self.current_interpreter.take() {
                self.supervisor.kill(interp.id())?;
            }

            // Start new interpreter
            self.ensure_interpreter()?;
            let response = "OK";
            eprintln!("Responding: {:?}", response);
            stream
                .write_all(response.as_bytes())
                .context("Failed to write response")?;
        } else if req == "TAKE" {
            // Take the interpreter and return it
            let interp = self
                .current_interpreter
                .as_mut()
                .context("no interpreter")?;
            interp.unsupervise()?;
            let (msg, fds) = interp.to_raw();
            stream
                .send_with_fd(&msg, &fds)
                .context("take send_with_fds failed")?;
            // Purposefully keep the reference until _after_ it's successfully sent to cli
            self.current_interpreter = None;

            // Spawn a new interpreter for next request
            self.ensure_interpreter()?;
        } else if req.starts_with("EXITCODE ") {
            // Return exit code from supervisor
            let id_str = req.strip_prefix("EXITCODE ").unwrap();
            let child_id = ChildId::from_str(id_str.trim())?;
            let exit_code = self.supervisor.get_exit_code(child_id)?;
            let response = format!("OK {}", exit_code);
            eprintln!("Responding: {:?}", response);
            stream
                .write_all(response.as_bytes())
                .context("Failed to write exit code response")?;
        } else {
            bail!("Unknown command '{}'", req)
        }

        Ok(())
    }
}

pub fn restart() -> Result<()> {
    if let Some(pid) = PidFileGuard::test(PIDFILE_PATH)? {
        kill_with_timeout(pid, Duration::from_secs(2))?;
        // Attempt to remove the PID file just in case. Errors are ignored.
        let _ = std::fs::remove_file(PIDFILE_PATH);
    }
    ensure()
}

pub fn ensure() -> Result<()> {
    if PidFileGuard::test(PIDFILE_PATH)?.is_some() {
        return Ok(());
    }
    // Spawn daemon process and return
    if let ForkResult::Child = daemonize()? {
        if let Err(e) = serve() {
            eprintln!("Server error {e}");
            process::exit(1);
        }
        process::exit(0);
    }
    Ok(())
}

fn serve() -> Result<()> {
    let pid = Pid::this();
    let pidfile: PathBuf = PIDFILE_PATH.into();
    let _pidfile_guard = PidFileGuard::new(pid, &pidfile)
        .with_context(move || format!("pid={} file={}", pid, pidfile.to_string_lossy()))?;
    let mut server = ServerState::new()?;
    server.run()?;
    Ok(())
}
