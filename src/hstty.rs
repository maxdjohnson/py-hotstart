use anyhow::{anyhow, Context, Result};
use nix::errno::Errno;
use nix::libc;
use nix::pty::openpty;
use nix::sys::select::{pselect, FdSet};
use nix::sys::signal::{SigSet, Signal};
use nix::sys::termios::{cfmakeraw, tcgetattr, tcsetattr, SetArg, Termios};
use nix::unistd::{isatty, pipe, read, write};
use signal_hook::flag;
use std::os::fd::{AsFd, AsRawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

// Create wrappers for TIOCGWINSZ and TIOCSWINSZ
nix::ioctl_read_bad!(tiocgwinsz, libc::TIOCGWINSZ, libc::winsize);
nix::ioctl_write_ptr_bad!(tiocswinsz, libc::TIOCSWINSZ, libc::winsize);

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

fn get_winsize(fd: impl AsRawFd) -> Option<libc::winsize> {
    let mut ws = libc::winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let res = unsafe { tiocgwinsz(fd.as_raw_fd(), &mut ws) };
    if res.is_ok() {
        Some(ws)
    } else {
        None
    }
}

fn resize_pty<Fd: AsRawFd>(pty_fd: Fd) -> Result<()> {
    let ws = get_winsize(std::io::stdin()).unwrap_or(libc::winsize {
        ws_row: 30,
        ws_col: 80,
        ws_xpixel: 640,
        ws_ypixel: 480,
    });

    let pty_raw = pty_fd.as_raw_fd();
    unsafe {
        tiocswinsz(pty_raw, &ws).context("Failed to set pty window size")?;
    }
    Ok(())
}

fn do_proxy<Fd: AsFd>(pty_fd: Fd) -> Result<()> {
    let winch_happened: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
    let pty_raw = pty_fd.as_fd().as_raw_fd();
    let stdin = std::io::stdin();
    let stdin_fd = stdin.as_fd();

    flag::register(signal_hook::consts::SIGWINCH, Arc::clone(&winch_happened))
        .context("failed to register SIGWINCH")?;

    resize_pty(pty_fd.as_fd())?;

    let mut buf = [0u8; 4096];

    let mut sigmask = SigSet::empty();
    sigmask.add(Signal::SIGWINCH);
    sigmask.thread_block().context("Failed to block SIGWINCH")?;
    let sigmask_empty = SigSet::empty();

    loop {
        if winch_happened.swap(false, Ordering::SeqCst) {
            resize_pty(pty_fd.as_fd())?;
        }

        let mut readfds = FdSet::new();
        readfds.insert(stdin_fd);
        readfds.insert(pty_fd.as_fd());

        match pselect(None, &mut readfds, None, None, None, &sigmask_empty) {
            Ok(_) => (),
            Err(nix::Error::EINTR) => continue,
            Err(e) => return Err(anyhow!("pselect failed: {}", e)),
        }

        if readfds.contains(stdin_fd) {
            match read(stdin_fd.as_raw_fd(), &mut buf) {
                Ok(0) => return Ok(()),
                Ok(n) => write_all(&pty_fd, &buf[..n])?,
                Err(nix::Error::EINTR) => continue,
                Err(e) => return Err(anyhow!("read from stdin failed: {}", e)),
            }
        }

        if readfds.contains(pty_fd.as_fd()) {
            match read(pty_raw, &mut buf) {
                Ok(0) => return Ok(()),
                Ok(n) => write_all(std::io::stdout().as_fd(), &buf[..n])?,
                Err(nix::Error::EINTR) => continue,
                Err(e) => return Err(anyhow!("read from pty failed: {}", e)),
            }
        }
    }
}

fn run_module(module_name: String, args: Vec<String>) -> Result<()> {
    let code_snippet = format!(
        "import sys; sys.argv[1:] = {}; import runpy; runpy.run_module('{}', run_name='__main__')",
        json::stringify(args),
        module_name
    );
    run_code(&code_snippet)
}

fn run_file(file_name: String, args: Vec<String>) -> Result<()> {
    let code_snippet = format!(
        "import sys; sys.argv[1:] = {}; import runpy; runpy.run_path('{}', run_name='__main__')",
        json::stringify(args),
        file_name
    );
    run_code(&code_snippet)
}

fn run_code(python_code: &str) -> Result<()> {
    if !isatty(std::io::stdin().as_raw_fd()).unwrap_or(false) {
        run_notty(python_code)
    } else {
        run_pty(python_code)
    }
}

fn run_repl() -> Result<()> {
    run_code("import code; code.interact(local={}, exitmsg='')")
}

fn run_notty(code_snippet: &str) -> Result<()> {
    let (read_fd, write_fd) = pipe().context("Failed to create pipe")?;

    {
        let stdin = std::io::stdin();
        let stdout = std::io::stdout();
        let stderr = std::io::stderr();

        // Send the code snippet directly as the command
        let fd_arr = [
            stdin.as_raw_fd(),
            stdout.as_raw_fd(),
            stderr.as_raw_fd(),
            write_fd.as_raw_fd(),
        ];

        hsclient::request_run(false, code_snippet, &fd_arr)?;
    }
    drop(write_fd);

    let mut buf = [0u8; 1];
    while read(read_fd.as_raw_fd(), &mut buf)
        .map_err(|e| anyhow!("Failed to read from pipe: {}", e))?
        > 0
    {}
    Ok(())
}

struct TermiosGuard {
    original: Termios,
}

impl Drop for TermiosGuard {
    fn drop(&mut self) {
        loop {
            match tcsetattr(std::io::stdin().as_fd(), SetArg::TCSANOW, &self.original) {
                Ok(_) => break,
                Err(nix::Error::EINTR) => continue,
                Err(e) => {
                    eprintln!("Failed to restore terminal attributes: {}", e);
                    break;
                }
            }
        }
    }
}

fn run_pty(code_snippet: &str) -> Result<()> {
    let original_winsize = get_winsize(std::io::stdin());

    // Temporarily configure the terminal to raw. TermiosGuard will restore the original settings at exit.
    let mut termios =
        tcgetattr(std::io::stdin().as_fd()).context("Failed to get terminal attributes")?;
    let original_termios = TermiosGuard {
        original: termios.clone(),
    };
    cfmakeraw(&mut termios);
    tcsetattr(std::io::stdin().as_fd(), SetArg::TCSANOW, &termios)
        .context("Failed to set terminal attributes to raw mode")?;

    let (master, slave) = openpty(&original_winsize, Some(&original_termios.original))
        .map(|p| (p.master, p.slave))
        .context("Unable to allocate pty")?;

    let fd_arr = [slave.as_raw_fd()];
    hsclient::request_run(true, code_snippet, &fd_arr)?;
    drop(slave);

    if let Err(e) = do_proxy(&master) {
        eprintln!("Error in do_proxy: {}", e);
    }
    Ok(())
}
