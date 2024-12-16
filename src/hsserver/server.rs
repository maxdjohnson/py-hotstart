use crate::hsserver::supervisor::{ChildId, Supervisor};
use anyhow::{bail, Context, Result};
use nix::libc;
use nix::pty::PtyMaster;
use nix::sys::select::{select, FdSet};
use nix::sys::socket::{sendmsg, ControlMessage, MsgFlags};
use signal_hook::consts::{SIGCHLD, SIGINT, SIGTERM};
use signal_hook::low_level::pipe;
use std::fs;
use std::io::{IoSlice, Read, Write};
use std::os::fd::{AsFd, AsRawFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::str::FromStr;

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
        let (sigchld_fd, sigterm_fd) = {
            let (sigchld_r, sigchld_w) = UnixStream::pair()?;
            let (sigterm_r, sigterm_w) = UnixStream::pair()?;
            let sigint_w = sigterm_w.try_clone()?;
            for socket in &[&sigchld_r, &sigchld_w, &sigterm_r, &sigterm_w, &sigint_w] {
                let _ = socket
                    .set_nonblocking(true)
                    .context("Failed to set socket to non-blocking")?;
            }
            pipe::register(SIGCHLD, sigchld_w)?;
            pipe::register(SIGTERM, sigterm_w)?;
            pipe::register(SIGINT, sigint_w)?;
            (sigchld_r, sigterm_r)
        };

        if Path::new(SOCKET_PATH).exists() {
            fs::remove_file(SOCKET_PATH).ok();
        }

        let listener =
            UnixListener::bind(SOCKET_PATH).context("Failed to bind Unix domain socket")?;

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
            let (id, fd) = self
                .supervisor
                .spawn_interpreter(self.prelude_code.as_deref())?;
            self.current_interpreter = Some(InterpreterState {
                id,
                pty_master_fd: fd,
            });
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
        // Wait for input or signal
        let (listener_ready, sigchld_ready, sigterm_ready) = {
            let listener_fd = self.listener.as_fd();
            let sigchld_fd = self.sigchld_fd.as_fd();
            let sigterm_fd = self.sigterm_fd.as_fd();
            let mut readfds = FdSet::new();
            readfds.insert(listener_fd);
            readfds.insert(sigchld_fd);
            readfds.insert(sigterm_fd);

            let ready = select(None, &mut readfds, None, None, None)?;
            if ready == 0 {
                (false, false, false)
            } else {
                (
                    readfds.contains(listener_fd),
                    readfds.contains(sigchld_fd),
                    readfds.contains(sigterm_fd),
                )
            }
        };

        if sigchld_ready {
            let mut buf = [0u8; 64];
            while let Ok(n) = self.sigchld_fd.read(&mut buf) {
                if n == 0 {
                    break;
                }
            }
            self.supervisor.handle_sigchld()?;
        }

        if sigterm_ready {
            let mut buf = [0u8; 64];
            while let Ok(n) = self.sigterm_fd.read(&mut buf) {
                if n == 0 {
                    break;
                }
            }
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

        let req = String::from_utf8_lossy(&buf[..n]).trim().to_string();

        if req.starts_with("INIT ") {
            // Update prelude
            let prelude = req.strip_prefix("INIT ").unwrap();
            self.prelude_code = Some(prelude.to_string());

            // Kill current interpreter (if present)
            if let Some(interp) = &self.current_interpreter {
                self.supervisor.kill(interp.id)?;
            }

            // Start new interpreter
            self.ensure_interpreter()?;
            stream
                .write_all(b"OK")
                .context("Failed to write response")?;
        } else if req == "TAKE" {
            // Take the interpreter and return it
            let interp = self.current_interpreter.take().context("no interpreter")?;
            let response = format!("OK {}", interp.id);
            let iov = [IoSlice::new(response.as_bytes())];
            let fds = [interp.pty_master_fd.as_raw_fd()];
            let cmsg = [ControlMessage::ScmRights(&fds)];
            sendmsg::<()>(stream.as_raw_fd(), &iov, &cmsg, MsgFlags::empty(), None)
                .context("Failed to sendmsg")?;

            // Spawn a new interpreter for next request
            self.ensure_interpreter()?;
        } else if req.starts_with("EXITCODE ") {
            // Return exit code from supervisor
            let id_str = req.strip_prefix("EXITCODE ").unwrap();
            let child_id = ChildId::from_str(id_str.trim())
                .with_context(|| format!("child_id='{}'", id_str))?;
            let exit_code = self.supervisor.get_exit_code(child_id)?;
            stream
                .write_all(format!("OK {}", exit_code).as_bytes())
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
