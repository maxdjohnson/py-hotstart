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

const SERVER_ADDRESS: &str = "/tmp/pyforked-server.sock";
const SCRIPT: &str = include_str!("../pyforked-server.py");


pub fn start(prelude: &str) -> Result<(), String> {
    // Read PID file if it exists
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
                    return Err(format!("Failed to remove sock {}: {}", SERVER_ADDRESS, e));
                }
            }
        }
    }
    // Write the server script to a temporary file
    fs::write("/tmp/pyforked-server.py", format!("{}\n{}", prelude, SCRIPT))
        .map_err(|e| format!("Failed to write server script: {}", e))?;

    // Use openpty to obtain master/slave fds
    let termios = tcgetattr(std::io::stdin()).ok();
    let (master, slave) = openpty(None, &termios)
        .map(|p| (p.master, p.slave))
        .map_err(|e| format!("Unable to allocate pty: {}", e))?;

    let stdin_fd = dup(slave.as_raw_fd()).map_err(|e| format!("Failed to dup stdin_fd: {}", e))?;
    let stdout_fd = dup(slave.as_raw_fd()).map_err(|e| format!("Failed to dup stdout_fd: {}", e))?;
    let stderr_fd = dup(slave.as_raw_fd()).map_err(|e| format!("Failed to dup stderr_fd: {}", e))?;

    // Run forkserver with pty
    let mut child = Command::new("python3")
        .arg("/tmp/pyforked-server.py")
        .stdin(unsafe { std::process::Stdio::from_raw_fd(stdin_fd)})
        .stdout(unsafe { std::process::Stdio::from_raw_fd(stdout_fd)})
        .stderr(unsafe { std::process::Stdio::from_raw_fd(stderr_fd)})
        .spawn()
        .map_err(|e| format!("Failed to spawn process: {}", e))?;

    // Drop the slave side of the pty
    drop(slave);

    // Read master side of pty until EOF (child exits)
    let mut output = String::new();
    let mut master_file: std::fs::File = master.into();
    master_file.read_to_string(&mut output).map_err(|e| format!("Failed to read from master: {}", e))?;

    // Wait for the child to finish if needed
    let status = child.wait().map_err(|e| format!("Failed to wait for child process: {}", e))?;
    if !status.success() {
        return Err(format!(
            "Server failed to start: {}\n{}",
            status,
            output,
        ));
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
        return Err("Timed out waiting for server to start".into());
    }
    Ok(())
}


pub fn send_exit_message() -> Result<bool, String> {
    // Try to connect to the forkserver
    let fd = socket(AddressFamily::Unix, SockType::Stream, SockFlag::empty(), None)
        .map_err(|e| format!("socket creation failed: {}", e))?;
    let addr = match UnixAddr::new(SERVER_ADDRESS) {
        Ok(addr) => addr,
        Err(_) => return Ok(false),
    };
    if let Err(_) = connect(fd.as_raw_fd(), &addr) {
        return Ok(false);
    }

    // Send the "EXIT" message
    let message = "EXIT";
    let iov = [IoSlice::new(message.as_bytes())];
    if nix::sys::socket::sendmsg::<()>(fd.as_raw_fd(), &iov, &[], MsgFlags::empty(), None).is_err() {
        return Ok(false);
    }

    Ok(true)
}


// Make a request to the forkserver, returning the pid of the new process.
pub fn request_fork(command: &str, fd_arr: &[i32]) -> Result<i32, String> {
    // Connect to the forkserver
    let fd = socket(AddressFamily::Unix, SockType::Stream, SockFlag::empty(), None)
        .map_err(|e| format!("socket creation failed: {}", e))?;
    let addr = UnixAddr::new(SERVER_ADDRESS).map_err(|e| format!("UnixAddr failed: {}", e))?;
    connect(fd.as_raw_fd(), &addr).map_err(|e| format!("Unable to connect to forkserver: {}\nStart the server with pyforked -i", e))?;

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
