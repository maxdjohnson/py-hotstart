use anyhow::{Context, Result};
use nix::errno::Errno;
use nix::libc;
use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
use nix::sys::termios::{cfmakeraw, tcgetattr, tcsetattr, SetArg, Termios};
use nix::unistd::{close, read, write};
use signal_hook::consts::SIGWINCH;
use signal_hook::low_level::pipe;
use std::io::{Read, Write};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::os::unix::net::UnixStream;

// Create wrappers for TIOCGWINSZ and TIOCSWINSZ
nix::ioctl_read_bad!(tiocgwinsz, libc::TIOCGWINSZ, libc::winsize);
nix::ioctl_write_ptr_bad!(tiocswinsz, libc::TIOCSWINSZ, libc::winsize);

/// RAII guard for terminal mode restoration.
struct TerminalModeGuard {
    fd: BorrowedFd<'static>,
    original: Termios,
}

impl TerminalModeGuard {
    fn new(fd: BorrowedFd<'_>) -> Result<TerminalModeGuard> {
        let termios = tcgetattr(fd).context("Failed to get terminal attributes")?;
        let original = termios.clone();
        let mut raw = termios;
        cfmakeraw(&mut raw);
        tcsetattr(fd, SetArg::TCSANOW, &raw).context("Failed to set terminal to raw mode")?;

        // Extend lifetime of fd borrow by making it 'static.
        let fd_static: BorrowedFd<'static> = unsafe { std::mem::transmute(fd) };
        Ok(TerminalModeGuard {
            fd: fd_static,
            original,
        })
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

/// Write all bytes from `buf` to `fd` until exhausted or error.
fn write_all<Fd: AsFd>(fd: Fd, mut buf: &[u8]) -> Result<(), nix::Error> {
    while !buf.is_empty() {
        match write(fd.as_fd(), buf) {
            Ok(0) => return Err(nix::Error::from(Errno::EIO)),
            Ok(n) => {
                buf = &buf[n..];
            }
            Err(nix::Error::EINTR) => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Setup the terminal in raw mode and return a guard that restores it on drop.
fn setup_terminal_mode(stdin_fd: BorrowedFd) -> Result<TerminalModeGuard> {
    TerminalModeGuard::new(stdin_fd)
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

/// Drain the signal stream after a POLLIN event.
fn drain_all(sigwinch_r: &mut UnixStream) -> Result<()> {
    let mut sigbuf = [0u8; 128];
    while let Ok(n) = sigwinch_r.read(&mut sigbuf) {
        if n == 0 {
            break;
        }
    }
    Ok(())
}

/// Main polling loop that proxies data between stdin/stdout and the PTY, and handles SIGWINCH.
fn proxy_loop(
    pty_fd: BorrowedFd,
    stdin_fd: BorrowedFd,
    stdout_fd: BorrowedFd,
    mut sigwinch_r: UnixStream,
) -> Result<()> {
    let mut buf = [0u8; 1024];
    let mut stdin_eof = false;

    loop {
        let mut fds = Vec::with_capacity(3);
        fds.push(PollFd::new(sigwinch_r.as_fd(), PollFlags::POLLIN));
        fds.push(PollFd::new(pty_fd, PollFlags::POLLIN));
        if !stdin_eof {
            fds.push(PollFd::new(stdin_fd, PollFlags::POLLIN));
        }

        poll(&mut fds, PollTimeout::NONE).context("Failed to poll file descriptors")?;

        let sigwinch_revents = fds[0].revents();
        let pty_revents = fds.get(1).and_then(|f| f.revents());
        let stdin_revents = if !stdin_eof {
            fds.get(2).and_then(|f| f.revents())
        } else {
            None
        };

        // Handle SIGWINCH events
        if let Some(revents) = sigwinch_revents {
            if revents.contains(PollFlags::POLLIN) {
                drain_all(&mut sigwinch_r).context("Failed to drain SIGWINCH stream")?;
                if let Err(e) = sync_winsize(stdout_fd, pty_fd) {
                    eprintln!("Failed to sync window size: {}", e);
                }
            }
        }

        // Check PTY for output
        if let Some(revents) = pty_revents {
            if revents.contains(PollFlags::POLLIN) {
                let n = read(pty_fd.as_raw_fd(), &mut buf).unwrap_or(0);
                if n == 0 {
                    // Interpreter exited
                    break;
                }
                {
                    let mut stdout_locked = std::io::stdout().lock();
                    stdout_locked.write_all(&buf[..n])?;
                    stdout_locked.flush()?;
                }
            }
        }

        // Check STDIN for user input
        if !stdin_eof {
            if let Some(revents) = stdin_revents {
                if revents.contains(PollFlags::POLLIN) {
                    let n = std::io::stdin().lock().read(&mut buf).unwrap_or(0);
                    if n == 0 {
                        // EOF on stdin - close write side to PTY
                        let _ = close(pty_fd.as_raw_fd());
                        stdin_eof = true;
                    } else {
                        let _ = write_all(pty_fd, &buf[..n]);
                    }
                }
            }
        }
    }

    Ok(())
}

/// Entrypoint for setting up and running the terminal proxy.
pub fn do_proxy(pty_fd: BorrowedFd, instructions: &str) -> Result<()> {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let stdin_fd = stdin.as_fd();
    let stdout_fd = stdout.as_fd();

    // Set raw mode on the user’s terminal with guard
    let _mode_guard = setup_terminal_mode(stdin_fd)?;

    // Set up signal handling for SIGWINCH
    let sigwinch_r = setup_sigwinch_stream()?;

    // Sync window size initially
    if let Err(e) = sync_winsize(stdout_fd, pty_fd) {
        eprintln!("Failed to sync window size: {}", e);
    }

    // Write instructions to interpreter
    let instructions_literal = format!("{:?}\n", instructions);
    write_all(pty_fd, instructions_literal.as_bytes())
        .context("Failed to write instructions to interpreter")?;

    // Run the polling loop
    proxy_loop(pty_fd, stdin_fd, stdout_fd, sigwinch_r)?;

    // Terminal mode will be restored by _mode_guard on drop.
    Ok(())
}
