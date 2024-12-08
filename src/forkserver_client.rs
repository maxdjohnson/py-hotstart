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
use nix::unistd::{isatty, read, write, pipe, Pid};
use std::env;
use std::fs;
use std::io::{IoSlice, IoSliceMut};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::process::exit;

const PIDFILE: &str = "/tmp/pyforked-server.pid";
const SERVER_ADDRESS: &str = "/tmp/pyforked-server.sock";

// Make a request to the forkserver, returning the pid of the new process.
pub fn request_fork(command: &str, fd_arr: &[i32]) -> Result<i32, String> {
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

/// Ensure that the given PID corresponds to a currently running process.
pub fn ensure_alive() -> Result<(), String> {
    let pid = read_pid_file(PIDFILE)?;
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
