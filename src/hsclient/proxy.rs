use crate::sendfd::PtyMaster;
use anyhow::{Context, Result};
use nix::libc;
use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
use nix::sys::termios::{cfmakeraw, tcgetattr, tcsetattr, SetArg, Termios};
use signal_hook::consts::SIGWINCH;
use signal_hook::low_level::pipe;
use std::io::{Read, Stdin, Stdout, Write};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::os::unix::net::UnixStream;

// Create wrappers for TIOCGWINSZ and TIOCSWINSZ
nix::ioctl_read_bad!(tiocgwinsz, libc::TIOCGWINSZ, libc::winsize);
nix::ioctl_write_ptr_bad!(tiocswinsz, libc::TIOCSWINSZ, libc::winsize);

pub struct TerminalModeGuard {
    fd: BorrowedFd<'static>,
    original: Termios,
}

impl TerminalModeGuard {
    pub fn new(fd: BorrowedFd<'_>) -> Result<TerminalModeGuard> {
        let termios = tcgetattr(fd).context("Failed to get terminal attributes")?;
        let original = termios.clone();
        let mut raw = termios;
        cfmakeraw(&mut raw);
        tcsetattr(fd, SetArg::TCSANOW, &raw).context("Failed to set terminal to raw mode")?;

        let fd_static: BorrowedFd<'static> = unsafe { std::mem::transmute(fd) };
        Ok(TerminalModeGuard {
            fd: fd_static,
            original,
        })
    }

    pub fn get_original(&self) -> &Termios {
        &self.original
    }
}

impl Drop for TerminalModeGuard {
    fn drop(&mut self) {
        let _ = tcsetattr(self.fd, SetArg::TCSANOW, &self.original);
    }
}

/// Sync the terminal window size from `from_fd` to `to_fd`.
fn sync_winsize(from_fd: BorrowedFd, to_fd: BorrowedFd) -> Result<()> {
    let mut ws: libc::winsize = libc::winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };

    let res = unsafe { tiocgwinsz(from_fd.as_raw_fd(), &mut ws) };
    if res.is_err() {
        eprintln!("Failed to get terminal size: {:?}", res);
        // If we can't get the terminal size, use a default.
        ws = libc::winsize {
            ws_row: 30,
            ws_col: 80,
            ws_xpixel: 640,
            ws_ypixel: 480,
        };
    }

    unsafe { tiocswinsz(to_fd.as_raw_fd(), &ws) }.context("failed to set winsize")?;

    Ok(())
}

/// Set up SIGWINCH signal handling via a UnixStream pair and register with signal_hook.
fn setup_sigwinch_stream() -> Result<UnixStream> {
    let (sigwinch_r, sigwinch_w) =
        UnixStream::pair().context("Failed to create UnixStream pair for signals")?;
    sigwinch_r
        .set_nonblocking(true)
        .context("Failed to set sigwinch_r to non-blocking")?;
    sigwinch_w
        .set_nonblocking(true)
        .context("Failed to set sigwinch_w to non-blocking")?;
    pipe::register(SIGWINCH, sigwinch_w).context("Failed to register SIGWINCH with pipe")?;
    Ok(sigwinch_r)
}

/// Main polling loop using high-level I/O on pty_file.
fn proxy_loop(
    mut pty: Option<PtyMaster>,
    mut stdin: Option<Stdin>,
    mut stdout: Stdout,
    mut sigwinch_r: UnixStream,
) -> Result<()> {
    let mut buf = [0u8; 1024];

    loop {
        let mut fds = Vec::with_capacity(3);
        fds.push(PollFd::new(sigwinch_r.as_fd(), PollFlags::POLLIN));
        if let Some(pty_fd) = &pty {
            fds.push(PollFd::new(pty_fd.as_fd(), PollFlags::POLLIN));
        }
        if let Some(stdin_fd) = &stdin {
            fds.push(PollFd::new(stdin_fd.as_fd(), PollFlags::POLLIN));
        }

        poll(&mut fds, PollTimeout::NONE).context("Failed to poll file descriptors")?;

        let sigwinch_revents = fds[0].revents();
        let pty_revents = fds.get(1).and_then(|f| f.revents());
        let stdin_revents = fds.get(2).and_then(|f| f.revents());

        // Handle SIGWINCH events
        if let Some(revents) = sigwinch_revents {
            if revents.contains(PollFlags::POLLIN) {
                let mut sbuf = [0u8; 1];
                sigwinch_r
                    .read_exact(&mut sbuf)
                    .context("sigwinch_r.read_exact error")?;
                if let Some(pty_fd) = &mut pty {
                    if let Err(e) = sync_winsize(stdout.as_fd(), pty_fd.as_fd()) {
                        eprintln!("Failed to sync window size: {}", e);
                    }
                }
            }
        }

        // Check PTY for output
        if let (Some(pty_fd), Some(revents)) = (&mut pty, pty_revents) {
            if revents.contains(PollFlags::POLLIN) {
                // Read from pty_file
                let n = pty_fd.read(&mut buf)?;
                if n == 0 {
                    // Interpreter exited
                    break;
                }
                stdout.write_all(&buf[..n])?;
                stdout.flush()?;
            }
        }

        // Check STDIN for user input
        if let (Some(stdin_fd), Some(revents)) = (&mut stdin, stdin_revents) {
            if revents.contains(PollFlags::POLLIN) {
                let n = stdin_fd.read(&mut buf)?;
                if n == 0 {
                    // EOF on stdin - close write side of PTY
                    if let Some(pty_fd) = pty.take() {
                        drop(pty_fd);
                    }
                    stdin = None;
                } else if let Some(pty_fd) = &mut pty {
                    pty_fd
                        .write_all(&buf[..n])
                        .context("proxy write to pty error")?
                }
            }
        }
    }

    Ok(())
}

/// Updated `do_proxy` to accept a reference to a `std::fs::File` and use high-level I/O.
pub fn do_proxy(_guard: &TerminalModeGuard, pty: PtyMaster) -> Result<()> {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();

    // Set up signal handling for SIGWINCH
    let sigwinch_r = setup_sigwinch_stream()?;

    // Sync window size initially
    if let Err(e) = sync_winsize(stdout.as_fd(), pty.as_fd()) {
        eprintln!("Failed to sync window size: {}", e);
    }

    // Run the polling loop using high-level operations
    proxy_loop(Some(pty), Some(stdin), stdout, sigwinch_r)?;

    Ok(())
}
