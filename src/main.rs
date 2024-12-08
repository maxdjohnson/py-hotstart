use nix::fcntl::{fcntl, FcntlArg, OFlag};
use nix::ioctl_read;
use nix::libc::{winsize, TIOCGWINSZ};
use nix::pty::openpty;
use nix::sys::select::{select, FdSet};
use nix::sys::signal;
use nix::sys::socket::{
    connect, socket, AddressFamily, ControlMessage, MsgFlags, SockFlag, SockType, UnixAddr,
};
use nix::sys::termios::{tcgetattr, LocalFlags};
use nix::sys::wait::{waitpid, WaitStatus};
use nix::unistd::{isatty, read, write, Pid};
use std::env;
use std::fs;
use std::io::{IoSlice, IoSliceMut};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::process::exit;

const PIDFILE: &str = "/tmp/pyforked-server.pid";
const SERVER_ADDRESS: &str = "/tmp/pyforked-server.sock";

ioctl_read!(tiocgwinsz, TIOCGWINSZ, 0, winsize);

fn main() {
    if let Err(e) = run() {
        eprintln!("Error: {}", e);
        exit(1);
    }
}

fn run() -> Result<(), String> {
    // Parse arguments to obtain either `code_snippet` or `module_name`.
    let (code_snippet, module_name) = parse_arguments(env::args().skip(1))?;

    // If a module name is provided, ignore the code snippet and run the module.
    let code_snippet = if !module_name.is_empty() {
        format!(
            "import runpy; runpy.run_module('{}', run_name='__main__')",
            module_name
        )
    } else if code_snippet.is_empty() {
        // Default snippet: run a Python REPL
        "import code; code.interact(local={})".to_string()
    } else {
        code_snippet
    };

    // Check forkserver status from PIDFILE
    let pid = read_pid_file(PIDFILE)?;
    ensure_process_alive(pid)?;

    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let stderr = std::io::stderr();

    // Check if stdin is a TTY
    if !isatty(stdin.as_raw_fd()).unwrap_or(false) {
        run_notty(stdin.as_fd(), stdout.as_fd(), stderr.as_fd(), &code_snippet)
    } else {
        run_pty(stdin.as_fd(), stdout.as_fd(), &code_snippet)
    }
}

/// Runs code in non-TTY mode by passing file descriptors directly to the forkserver
fn run_notty(stdin_fd: BorrowedFd<'_>, stdout_fd: BorrowedFd<'_>, stderr_fd: BorrowedFd<'_>, code_snippet: &str) -> Result<(), String> {
    // Construct the request and send
    let message = format!("RUN {}", code_snippet);
    let fd_arr = [stdin_fd.as_raw_fd(), stdout_fd.as_raw_fd(), stderr_fd.as_raw_fd()];
    let child_pid = forkserver_req(&message, &fd_arr)
        .map_err(|e| format!("Failed to communicate with forkserver: {}", e))?;
    // Wait for the child process to finish
    let status = waitpid(Pid::from_raw(child_pid), None)
        .map_err(|e| format!("Failed to wait for child process: {}", e))?;

    match status {
        WaitStatus::Exited(_, code) => {
            if code != 0 {
                return Err(format!("Child process exited with code {}", code));
            }
        }
        WaitStatus::Signaled(_, signal, _) => {
            return Err(format!("Child process terminated by signal {}", signal));
        }
        _ => return Err("Unexpected child process status".to_string()),
    }

    Ok(())
}

/// Runs code inside a PTY and forwards input/output
fn run_pty(stdin_fd: BorrowedFd<'_>, stdout_fd: BorrowedFd<'_>, code_snippet: &str) -> Result<(), String> {
    // Get current terminal settings
    let mut termios = tcgetattr(stdin_fd)
        .map_err(|e| format!("Failed to get terminal attributes: {}", e))?;
    termios.local_flags |= LocalFlags::ICANON;

    // Get current window size
    let ws = get_winsize(stdin_fd.as_fd());

    // Open a pty
    let pty = openpty(&ws, Some(&termios)).map_err(|e| format!("openpty failed: {}", e))?;
    let master_fd = pty.master;
    let slave_fd = pty.slave;

    // Construct the request and send
    let message = format!("RUN_PTY {}", code_snippet);
    let fd_arr = [slave_fd.as_raw_fd()];
    forkserver_req(&message, &fd_arr)
        .map_err(|e| format!("Failed to communicate with forkserver: {}", e))?;

    // Close slave fd locally as we no longer need it
    drop(slave_fd);

    // Set stdin, stdout, and master_fd to non-blocking mode
    set_nonblocking(stdin_fd.as_raw_fd())?;
    set_nonblocking(stdout_fd.as_raw_fd())?;
    set_nonblocking(master_fd.as_raw_fd())?;

    let mut buf_in = [0u8; 1024];
    let mut buf_out = [0u8; 1024];

    loop {
        let mut fds = FdSet::new();
        fds.insert(master_fd.as_fd());
        fds.insert(stdin_fd);

        let nfds = std::cmp::max(stdin_fd.as_raw_fd(), master_fd.as_raw_fd()) + 1;
        select(nfds, Some(&mut fds), None, None, None)
            .map_err(|e| format!("select error: {}", e))?;

        // If there's input available on stdin
        if fds.contains(stdin_fd) {
            match read(stdin_fd.as_raw_fd(), &mut buf_in) {
                Ok(n) if n > 0 => {
                    if !write_all(&master_fd.as_fd(), &buf_in[..n]) {
                        eprintln!("Error writing to master fd.");
                        break;
                    }
                }
                Ok(0) => {
                    // EOF on stdin, end the session
                    break;
                }
                Err(_) => {
                    // EAGAIN or non-fatal error, just ignore and continue
                }
                _ => {}
            }
        }

        // If there's data available from the child process through master_fd
        if fds.contains(master_fd.as_fd()) {
            match read(master_fd.as_raw_fd(), &mut buf_out) {
                Ok(n) if n > 0 => {
                    if !write_all(&stdout_fd, &buf_out[..n]) {
                        eprintln!("Error writing to stdout.");
                        break;
                    }
                }
                Ok(0) => {
                    // Child process exited or master closed
                    eprintln!("Child exited or master closed. Ending session.");
                    break;
                }
                Err(_) => {
                    // EAGAIN or non-fatal error
                }
                _ => {}
            }
        }
    }

    eprintln!("CLI shutting down cleanly.");
    Ok(())
}

fn forkserver_req(command: &str, fd_arr: &[i32]) -> Result<i32, String> {
    // Connect to the forkserver
    let fd = socket(AddressFamily::Unix, SockType::Stream, SockFlag::empty(), None)
        .map_err(|e| format!("socket creation failed: {}", e))?;
    let addr = UnixAddr::new(SERVER_ADDRESS).map_err(|e| format!("UnixAddr failed: {}", e))?;
    connect(fd.as_raw_fd(), &addr).map_err(|e| format!("Unable to connect to forkserver: {}", e))?;

    // Send the slave_fd along with the message
    let cmsg = [ControlMessage::ScmRights(fd_arr)];
    let iov = [IoSlice::new(command.as_bytes())];

    nix::sys::socket::sendmsg::<()>(fd.as_raw_fd(), &iov, &cmsg, MsgFlags::empty(), None)
        .map_err(|e| format!("Failed to send message and fd to server: {}", e))?;

    // Receive response from server
    let mut buf = [0u8; 1024];
    let response_size = {
        let mut iov = [IoSliceMut::new(&mut buf)];
        let msg = nix::sys::socket::recvmsg::<()>(
            fd.as_raw_fd(),
            &mut iov,
            None,
            MsgFlags::empty(),
        )
        .map_err(|e| format!("Error receiving response from server: {}", e))?;

        if msg.bytes == 0 {
            return Err("Server disconnected prematurely (no data).".into());
        }
        msg.bytes
    };

    // Get response string
    let response = &buf[..response_size];
    let response_str = std::str::from_utf8(response)
        .map_err(|_| "Server response was not valid UTF-8".to_string())?;

    // Parse PID
    let parts: Vec<&str> = response_str.split_whitespace().collect();
    if parts.len() != 2 || parts[0] != "OK" {
        return Err(format!("Server responded with invalid message: {:?}", response_str));
    }
    let pid = parts[1].parse::<i32>()
        .map_err(|_| format!("Server responded with invalid PID: {}", parts[1]))?;

    Ok(pid)
}

/// Parse command-line arguments, extracting either `-c code_snippet` or `-m module_name`.
fn parse_arguments<I: Iterator<Item = String>>(mut args: I) -> Result<(String, String), String> {
    let mut code_snippet = String::new();
    let mut module_name = String::new();

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
                return Err(format!("Unknown argument: {}", other));
            }
        }
    }

    Ok((code_snippet, module_name))
}

/// Read the PID from the given pidfile and return it.
fn read_pid_file(path: &str) -> Result<i32, String> {
    let pid_str = fs::read_to_string(path).map_err(|_| {
        "Forkserver not running (no pidfile). Please start it first.".to_string()
    })?;
    pid_str
        .trim()
        .parse::<i32>()
        .map_err(|_| "Invalid PID in pidfile".to_string())
}

/// Ensure that the given PID corresponds to a currently running process.
fn ensure_process_alive(pid: i32) -> Result<(), String> {
    if let Err(err) = signal::kill(Pid::from_raw(pid), None) {
        if err == nix::errno::Errno::ESRCH {
            return Err(format!(
                "No process with pid {} is alive. The forkserver might have crashed. Please restart it.",
                pid
            ));
        } else {
            return Err(format!("Failed to check process status: {}", err));
        }
    }
    Ok(())
}

/// Get the current window size using the `tiocgwinsz` ioctl.
fn get_winsize(fd: impl AsFd) -> Option<winsize> {
    let mut ws = winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let res = unsafe { tiocgwinsz(fd.as_fd().as_raw_fd(), &mut ws) };
    if res.is_ok() {
        Some(ws)
    } else {
        None
    }
}

/// Write all data to the given file descriptor.
fn write_all(fd: &impl AsFd, mut data: &[u8]) -> bool {
    while !data.is_empty() {
        match write(fd, data) {
            Ok(n) if n > 0 => data = &data[n..],
            Ok(0) => return false, // Unexpected EOF or no progress
            Err(_) => return false, // Handle write error
            _ => unreachable!(),
        }
    }
    true
}

/// Set a file descriptor to non-blocking mode.
fn set_nonblocking(fd: i32) -> Result<(), String> {
    let flags = fcntl(fd, FcntlArg::F_GETFL)
        .map_err(|e| format!("F_GETFL failed: {}", e))?;
    let new_flags = OFlag::from_bits_truncate(flags) | OFlag::O_NONBLOCK;
    fcntl(fd, FcntlArg::F_SETFL(new_flags))
        .map_err(|e| format!("F_SETFL failed: {}", e))?;
    Ok(())
}
