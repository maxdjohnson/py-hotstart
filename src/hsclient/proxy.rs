use anyhow::{Context, Result};
use nix::errno::Errno;
use std::io::{Read, Write};
use nix::libc;
use nix::sys::termios::{tcgetattr, tcsetattr, Termios, SetArg, cfmakeraw};
use nix::unistd::{read, write, close};
use std::os::fd::{BorrowedFd, AsFd, AsRawFd};
use std::{env, fs};
use std::os::unix::net::UnixStream;
use signal_hook::low_level::pipe;
use signal_hook::consts::SIGWINCH;
use nix::poll::{poll, PollFd, PollFlags, PollTimeout};

// Create wrappers for TIOCGWINSZ and TIOCSWINSZ
nix::ioctl_read_bad!(tiocgwinsz, libc::TIOCGWINSZ, libc::winsize);
nix::ioctl_write_ptr_bad!(tiocswinsz, libc::TIOCSWINSZ, libc::winsize);

struct TerminalModeGuard {
    fd: BorrowedFd<'static>,
    original: Termios,
}

impl TerminalModeGuard {
    fn new(fd: BorrowedFd<'_>) -> Result<TerminalModeGuard> {
        let termios = tcgetattr(fd)?;
        let original = termios.clone();
        let mut raw = termios;
        cfmakeraw(&mut raw);
        tcsetattr(fd, SetArg::TCSANOW, &raw)?;
        // Extend lifetime of fd borrow by making a 'static reference.
        // Safe here because we're not actually extending fd lifetime beyond main function scope;
        // Just be careful that fd outlives this guard.
        // An alternative is to store the raw_fd and re-borrow it as needed.
        let fd_static: BorrowedFd<'static> = unsafe { std::mem::transmute(fd) };
        Ok(TerminalModeGuard { fd: fd_static, original })
    }
}

impl Drop for TerminalModeGuard {
    fn drop(&mut self) {
        let _ = tcsetattr(self.fd, SetArg::TCSANOW, &self.original);
    }
}

fn sync_winsize(from_fd: BorrowedFd, to_fd: BorrowedFd) -> Result<()> {
    let mut ws: libc::winsize = libc::winsize { ws_row: 0, ws_col: 0, ws_xpixel: 0, ws_ypixel: 0 };
    let res = unsafe { tiocgwinsz(from_fd.as_raw_fd(), &mut ws) };
    if let Err(_) = res {
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

pub fn do_proxy(pty_fd: BorrowedFd, final_code: &str) -> Result<()> {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let stdin_fd = stdin.as_fd();
    let stdout_fd = stdout.as_fd();

    // Set raw mode on user’s terminal with guard
    let _mode_guard = TerminalModeGuard::new(stdin_fd)?;

    // Register pipe-based handler for SIGWINCH
    let mut sigwinch_r = {
        let (sigwinch_r, sigwinch_w) = UnixStream::pair().context("Failed to create UnixStream pair for signals")?;
        sigwinch_r.set_nonblocking(true).context("Failed to set socket sigwinch_r to non-blocking")?;
        sigwinch_w.set_nonblocking(true).context("Failed to set socket sigwinch_w to non-blocking")?;

        // Register SIGWINCH with the write end of the pipe
        pipe::register(SIGWINCH, sigwinch_w).context("Failed to register SIGWINCH with pipe")?;
        sigwinch_r
    };

    // Sync window size initially
    if let Err(e) = sync_winsize(stdout_fd, pty_fd) {
        eprintln!("Failed to sync window size: {}", e);
    }

    // Write code to interpreter
    write_all(pty_fd, final_code.as_bytes()).context("Failed to write final code to interpreter")?;

    let mut buf = [0u8; 1024];
    let mut stdin_eof = false;

    loop {
        let mut fds = Vec::with_capacity(3);
        fds.push(PollFd::new(sigwinch_r.as_fd(), PollFlags::POLLIN));
        fds.push(PollFd::new(pty_fd, PollFlags::POLLIN));
        if !stdin_eof {
            fds.push(PollFd::new(stdin_fd, PollFlags::POLLIN));
        }

        poll(&mut fds, PollTimeout::NONE)?;

        let sigwinch_revents = fds[0].revents();
        let pty_revents = fds.get(1).and_then(|f| f.revents());
        let stdin_revents = if !stdin_eof {
            fds.get(2).and_then(|f| f.revents())
        } else {
            None
        };

        drop(fds); // Drop to release borrow on sigwinch_r

        // Now we can safely read from sigwinch_r
        if let Some(revents) = sigwinch_revents {
            if revents.contains(PollFlags::POLLIN) {
                let mut sigbuf = [0u8; 128];
                // Read until no more data
                while let Ok(n) = sigwinch_r.read(&mut sigbuf) {
                    if n == 0 {
                        break;
                    }
                }
                if let Err(e) = sync_winsize(stdout_fd, pty_fd) {
                    eprintln!("Failed to sync window size: {}", e);
                }
            }
        }

        // Check PTY for output
        if let Some(revents) = pty_revents {
            if revents.contains(PollFlags::POLLIN) {
                let n = match read(pty_fd.as_raw_fd(), &mut buf) {
                    Ok(n) => n,
                    Err(_) => 0,
                };
                if n == 0 {
                    // Interpreter exited
                    break;
                }
                // Write to stdout
                // For simplicity, no retry logic here, just a single write
                // Typically fine for a TTY
                let _ = stdout.lock().write_all(&buf[..n]);
                let _ = stdout.lock().flush();
            }
        }

        // Check STDIN for user input
        if !stdin_eof {
            if let Some(revents) = stdin_revents {
                if revents.contains(PollFlags::POLLIN) {
                    let n = match stdin.lock().read(&mut buf) {
                        Ok(n) => n,
                        Err(_) => 0,
                    };
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

    // Terminal mode will be restored by _mode_guard on drop.
    Ok(())
}
