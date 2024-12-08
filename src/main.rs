mod forkserver_client;

use nix::Error;
use nix::errno::Errno;
use nix::libc;
use nix::pty::openpty;
use nix::sys::select::{pselect, FdSet};
use nix::sys::signal::{SigSet, Signal};
use nix::sys::termios::{cfmakeraw, tcgetattr, tcsetattr, SetArg, Termios};
use nix::unistd::{isatty, read, write, pipe};
use signal_hook::flag;
use std::env;
use std::os::fd::{AsFd, AsRawFd};
use std::process::exit;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};


// Create wrappers for TIOCGWINSZ and TIOCSWINSZ
nix::ioctl_read_bad!(tiocgwinsz, libc::TIOCGWINSZ, libc::winsize);
nix::ioctl_write_ptr_bad!(tiocswinsz, libc::TIOCSWINSZ, libc::winsize);

fn die(msg: &str) -> ! {
    eprintln!("[!] {}", msg);
    std::process::exit(1);
}


fn write_all<Fd: AsFd>(fd: Fd, mut buf: &[u8]) -> Result<(), Error> {
    while !buf.is_empty() {
        match write(fd.as_fd(), buf) {
            Ok(0) => return Err(Error::from(Errno::EIO)),
            Ok(n) => {
                buf = &buf[n..];
            }
            Err(Error::EINTR) => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}


fn setup_terminal_raw() -> nix::Result<Termios> {
    let mut termios = tcgetattr(std::io::stdin().as_fd())?;
    let saved = termios.clone();
    cfmakeraw(&mut termios);
    tcsetattr(std::io::stdin().as_fd(), SetArg::TCSANOW, &termios)?;
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

fn resize_pty<Fd: AsRawFd>(pty_fd: Fd) -> nix::Result<()> {
    // Get stdin winsize or fallback to a default
    let ws = get_winsize(std::io::stdin()).unwrap_or(libc::winsize {
        ws_row: 30,
        ws_col: 80,
        ws_xpixel: 640,
        ws_ypixel: 480,
    });

    let pty_raw = pty_fd.as_raw_fd();
    unsafe { tiocswinsz(pty_raw, &ws)?; }
    Ok(())
}

fn do_proxy<Fd: AsFd>(pty_fd: Fd) -> nix::Result<()> {
    let winch_happened: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
    let pty_raw = pty_fd.as_fd().as_raw_fd();
    let stdin = std::io::stdin();
    let stdin_fd = stdin.as_fd();

    // Handle SIGWINCH by setting flag so it can be delivered to child.
    flag::register(signal_hook::consts::SIGWINCH, Arc::clone(&winch_happened)).expect("failed to register SIGWINCH");

    resize_pty(pty_fd.as_fd())?;

    let mut buf = [0u8; 4096];

    // Blocks SIGWINCH except during the pselect() call to avoid race conditions.
    let mut sigmask = SigSet::empty();
    sigmask.add(Signal::SIGWINCH);
    sigmask.thread_block()?;

    let sigmask_empty = SigSet::empty();
    loop {
        if winch_happened.swap(false, Ordering::SeqCst) {
            resize_pty(pty_fd.as_fd())?;
        }

        let mut readfds = FdSet::new();
        readfds.insert(stdin_fd);
        readfds.insert(pty_fd.as_fd());

        pselect(None, &mut readfds, None, None, None, &sigmask_empty)?;

        if readfds.contains(stdin_fd) {
            match read(stdin_fd.as_raw_fd(), &mut buf) {
                Ok(0) => return Ok(()), // EOF
                Ok(n) => {
                    write_all(&pty_fd, &buf[..n])?;
                }
                Err(Error::EINTR) => continue,
                Err(e) => return Err(e),
            }
        }

        if readfds.contains(pty_fd.as_fd()) {
            match read(pty_raw, &mut buf) {
                Ok(0) => return Ok(()), // EOF
                Ok(n) => {
                    write_all(std::io::stdout().as_fd(), &buf[..n])?;
                }
                Err(Error::EINTR) => continue,
                Err(e) => return Err(e),
            }
        }
    }
}

fn run() -> Result<(), String> {
    // Parse arguments to obtain either `code_snippet` or `module_name`.
    let (code_snippet, module_name, file_name) = parse_arguments(env::args().skip(1))?;

    // If a module name is provided, ignore the code snippet and run the module.
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
        // Default snippet: run a Python REPL
        "import code; code.interact(local={}, exitmsg='')".to_string()
    } else {
        code_snippet
    };

    // Check forkserver status from PIDFILE
    forkserver_client::ensure_alive()?;

    // Check if stdin is a TTY
    if !isatty(std::io::stdin().as_raw_fd()).unwrap_or(false) {
        run_notty(&code_snippet)
    } else {
        run_pty(&code_snippet)
    }
}

/// Runs code in non-TTY mode by passing file descriptors directly to the forkserver
fn run_notty(code_snippet: &str) -> Result<(), String> {
    // Create a pipe for the child to indicate when it's done (waitpid doens't work on non-children)
    let (read_fd, write_fd) = pipe().map_err(|e| format!("Failed to create pipe: {}", e))?;

    // Construct the request and send
    {
        let stdin = std::io::stdin();
        let stdout = std::io::stdout();
        let stderr = std::io::stderr();
        let message = format!("RUN {}", code_snippet);
        let fd_arr = [stdin.as_raw_fd(), stdout.as_raw_fd(), stderr.as_raw_fd(), write_fd.as_raw_fd()];
        forkserver_client::request_fork(&message, &fd_arr)?;
    }
    drop(write_fd);

    // Wait for EOF on read_fd indicating child closed
    let mut buf = [0u8; 1];
    while read(read_fd.as_raw_fd(), &mut buf).map_err(|e| format!("Failed to read from pipe: {}", e))? > 0 {}
    Ok(())
}

/// Runs code inside a PTY and forwards input/output
fn run_pty(code_snippet: &str) -> Result<(), String> {
    let original_winsize = get_winsize(std::io::stdin());
    let original_termios = match setup_terminal_raw() {
        Ok(t) => t,
        Err(e) => die(&format!("Unable to set terminal attributes: {}", e)),
    };

    // Use openpty to obtain master/slave fds
    let (master, slave) = match openpty(&original_winsize, Some(&original_termios)) {
        Ok(p) => (p.master, p.slave),
        Err(e) => die(&format!("Unable to allocate pty: {}", e)),
    };

    // Spawn child and attach to slave pty
    let message = format!("RUN_PTY {}", code_snippet);
    let fd_arr = [slave.as_raw_fd()];
    if let Err(e) = forkserver_client::request_fork(&message, &fd_arr) {
        eprintln!("Unable to request fork: {}", e);
        std::process::exit(1);
    }
    drop(slave);

    if let Err(e) = do_proxy(&master) {
        eprintln!("Error in do_proxy: {}", e);
    }

    // Restore terminal attributes
    loop {
        match tcsetattr(std::io::stdin().as_fd(), SetArg::TCSANOW, &original_termios) {
            Ok(_) => break,
            Err(Error::EINTR) => continue,
            Err(e) => die(&format!("Unable to tcsetattr: {}", e)),
        }
    }
    Ok(())
}

/// Parse command-line arguments, extracting either `-c code_snippet` or `-m module_name`.
fn parse_arguments<I: Iterator<Item = String>>(mut args: I) -> Result<(String, String, String), String> {
    let mut code_snippet = String::new();
    let mut module_name = String::new();
    let mut file_name = String::new();

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-c" => {
                code_snippet = args
                    .next()
                    .ok_or("No code snippet provided after -c".to_string())?;
            }
            "-m" => {
                module_name = args
                    .next()
                    .ok_or("No module name provided after -m".to_string())?;
            }
            other => {
                file_name = other.to_string();
            }
        }
    }

    Ok((code_snippet, module_name, file_name))
}

fn main() {
    if let Err(e) = run() {
        eprintln!("Error: {}", e);
        exit(1);
    }
}
