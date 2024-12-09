mod forkserver_client;

use anyhow::{anyhow, Context, Result};
use nix::errno::Errno;
use nix::libc;
use nix::pty::openpty;
use nix::sys::select::{pselect, FdSet};
use nix::sys::signal::{SigSet, Signal};
use nix::sys::termios::{cfmakeraw, tcgetattr, tcsetattr, SetArg, Termios};
use nix::unistd::{isatty, pipe, read, write};
use signal_hook::flag;
use std::env;
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

fn setup_terminal_raw() -> Result<Termios> {
    let mut termios =
        tcgetattr(std::io::stdin().as_fd()).context("Failed to get terminal attributes")?;
    let saved = termios.clone();
    cfmakeraw(&mut termios);
    tcsetattr(std::io::stdin().as_fd(), SetArg::TCSANOW, &termios)
        .context("Failed to set terminal attributes to raw mode")?;
    Ok(saved)
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

        pselect(None, &mut readfds, None, None, None, &sigmask_empty).context("pselect failed")?;

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

fn run() -> Result<()> {
    let (code_snippet, module_name, file_name, start_with_imports) =
        parse_arguments(env::args().skip(1))?;
    if !start_with_imports.is_empty() {
        return forkserver_client::start(&start_with_imports);
    }

    let code_snippet = if !file_name.is_empty() {
        format!(
            "import runpy; runpy.run_path('{}', run_name='__main__')",
            file_name
        )
    } else if !module_name.is_empty() {
        format!(
            "import runpy; runpy.run_module('{}', run_name='__main__')",
            module_name
        )
    } else if code_snippet.is_empty() {
        "import code; code.interact(local={}, exitmsg='')".to_string()
    } else {
        code_snippet
    };

    if !isatty(std::io::stdin().as_raw_fd()).unwrap_or(false) {
        run_notty(&code_snippet)
    } else {
        run_pty(&code_snippet)
    }
}

fn run_notty(code_snippet: &str) -> Result<()> {
    let (read_fd, write_fd) = pipe().context("Failed to create pipe")?;

    {
        let stdin = std::io::stdin();
        let stdout = std::io::stdout();
        let stderr = std::io::stderr();
        let message = format!("RUN {}", code_snippet);
        let fd_arr = [
            stdin.as_raw_fd(),
            stdout.as_raw_fd(),
            stderr.as_raw_fd(),
            write_fd.as_raw_fd(),
        ];
        forkserver_client::request_fork(&message, &fd_arr)?;
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
    let original_termios = setup_terminal_raw()
        .map(|t| TermiosGuard { original: t })
        .context("Unable to set terminal attributes")?;

    let (master, slave) = openpty(&original_winsize, Some(&original_termios.original))
        .map(|p| (p.master, p.slave))
        .context("Unable to allocate pty")?;

    let message = format!("RUN_PTY {}", code_snippet);
    let fd_arr = [slave.as_raw_fd()];
    forkserver_client::request_fork(&message, &fd_arr)?;
    drop(slave);

    if let Err(e) = do_proxy(&master) {
        eprintln!("Error in do_proxy: {}", e);
    }
    Ok(())
}

fn parse_arguments<I: Iterator<Item = String>>(
    mut args: I,
) -> Result<(String, String, String, String)> {
    let mut code_snippet = String::new();
    let mut module_name = String::new();
    let mut file_name = String::new();
    let mut start_with_imports = String::new();

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-c" => {
                code_snippet = args
                    .next()
                    .ok_or_else(|| anyhow!("No code snippet provided after -c"))?;
            }
            "-m" => {
                module_name = args
                    .next()
                    .ok_or_else(|| anyhow!("No module name provided after -m"))?;
            }
            "-i" => {
                start_with_imports = args
                    .next()
                    .ok_or_else(|| anyhow!("No import value provided after -i"))?;
            }
            other => {
                file_name = other.to_string();
            }
        }
    }

    Ok((code_snippet, module_name, file_name, start_with_imports))
}

fn main() {
    if let Err(e) = run() {
        eprintln!("Error: {}", e);
        std::process::exit(1);
    }
}
