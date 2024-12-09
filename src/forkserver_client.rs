use std::process::Command;
use nix::unistd::dup;
use nix::pty::openpty;
use std::os::unix::io::FromRawFd;
use nix::sys::termios::tcgetattr;
use std::io::Read;
use nix::sys::socket::{
    connect, socket, AddressFamily, ControlMessage, MsgFlags, SockFlag, SockType, UnixAddr,
};
use std::fs;
use std::io::{IoSlice, IoSliceMut};
use std::os::fd::AsRawFd;
use anyhow::{anyhow, Context, Result};

const SERVER_ADDRESS: &str = "/tmp/pyforked-server.sock";
const SCRIPT: &str = include_str!("../pyforked-server.py");

pub fn start(prelude: &str) -> Result<()> {
    if send_exit_message()? {
        // The process is alive, and we successfully sent EXIT. Wait for socket file to be removed.
        let start = std::time::Instant::now();
        while start.elapsed() < std::time::Duration::from_secs(1) {
            if !std::path::Path::new(SERVER_ADDRESS).exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        // If the socket still exists after 1s, delete it
        if std::path::Path::new(SERVER_ADDRESS).exists() {
            eprintln!("pyforked-server.py failed to clean up sock {}", SERVER_ADDRESS);
            if let Err(e) = fs::remove_file(SERVER_ADDRESS) {
                if e.kind() != std::io::ErrorKind::NotFound {
                    return Err(anyhow!("Failed to remove sock {}: {}", SERVER_ADDRESS, e));
                }
            }
        }
    }

    fs::write("/tmp/pyforked-server.py", format!("{}\n{}", prelude, SCRIPT))
        .context("Failed to write server script")?;

    let termios = tcgetattr(std::io::stdin()).ok();
    let (master, slave) = openpty(None, &termios)
        .map(|p| (p.master, p.slave))
        .context("Unable to allocate pty")?;

    let stdin_fd = dup(slave.as_raw_fd()).context("Failed to dup stdin_fd")?;
    let stdout_fd = dup(slave.as_raw_fd()).context("Failed to dup stdout_fd")?;
    let stderr_fd = dup(slave.as_raw_fd()).context("Failed to dup stderr_fd")?;

    let mut child = Command::new("python3")
        .arg("/tmp/pyforked-server.py")
        .stdin(unsafe { std::process::Stdio::from_raw_fd(stdin_fd) })
        .stdout(unsafe { std::process::Stdio::from_raw_fd(stdout_fd) })
        .stderr(unsafe { std::process::Stdio::from_raw_fd(stderr_fd) })
        .spawn()
        .context("Failed to spawn process")?;

    drop(slave);

    let mut output = String::new();
    let mut master_file: std::fs::File = master.into();
    master_file.read_to_string(&mut output)
        .context("Failed to read from master")?;

    let status = child.wait().context("Failed to wait for child process")?;
    if !status.success() {
        return Err(anyhow!("Server failed to start: {}\n{}", status, output));
    }

    // Wait for socket to be created
    let start = std::time::Instant::now();
    while start.elapsed() < std::time::Duration::from_secs(60) {
        if std::path::Path::new(SERVER_ADDRESS).exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    if !std::path::Path::new(SERVER_ADDRESS).exists() {
        return Err(anyhow!("Timed out waiting for server to start"));
    }
    Ok(())
}

pub fn send_exit_message() -> Result<bool> {
    let fd = socket(AddressFamily::Unix, SockType::Stream, SockFlag::empty(), None)
        .context("socket creation failed")?;
    let addr = match UnixAddr::new(SERVER_ADDRESS) {
        Ok(addr) => addr,
        Err(_) => return Ok(false),
    };
    if connect(fd.as_raw_fd(), &addr).is_err() {
        return Ok(false);
    }

    let message = "EXIT";
    let iov = [IoSlice::new(message.as_bytes())];
    if nix::sys::socket::sendmsg::<()>(fd.as_raw_fd(), &iov, &[], MsgFlags::empty(), None).is_err() {
        return Ok(false);
    }

    Ok(true)
}

pub fn request_fork(command: &str, fd_arr: &[i32]) -> Result<i32> {
    let fd = socket(AddressFamily::Unix, SockType::Stream, SockFlag::empty(), None)
        .context("socket creation failed")?;
    let addr = UnixAddr::new(SERVER_ADDRESS)
        .context("UnixAddr failed")?;
    connect(fd.as_raw_fd(), &addr)
        .map_err(|e| anyhow!("Unable to connect to forkserver: {}\nStart the server with pyforked -i", e))?;

    let cmsg = [ControlMessage::ScmRights(fd_arr)];
    let iov = [IoSlice::new(command.as_bytes())];

    nix::sys::socket::sendmsg::<()>(fd.as_raw_fd(), &iov, &cmsg, MsgFlags::empty(), None)
        .context("Failed to send message and fd to server")?;

    let mut buf = [0u8; 1024];
    let response_size = {
        let mut iov = [IoSliceMut::new(&mut buf)];
        let msg = nix::sys::socket::recvmsg::<()>(
            fd.as_raw_fd(),
            &mut iov,
            None,
            MsgFlags::empty(),
        ).context("Error receiving response from server")?;

        if msg.bytes == 0 {
            return Err(anyhow!("Server disconnected prematurely (no data)."));
        }
        msg.bytes
    };

    let response = &buf[..response_size];
    let response_str = std::str::from_utf8(response)
        .map_err(|_| anyhow!("Server response was not valid UTF-8"))?;

    let parts: Vec<&str> = response_str.split_whitespace().collect();
    if parts.len() != 2 || parts[0] != "OK" {
        return Err(anyhow!("Server responded with invalid message: {:?}", response_str));
    }
    let pid = parts[1].parse::<i32>()
        .map_err(|_| anyhow!("Server responded with invalid PID: {}", parts[1]))?;

    Ok(pid)
}
