use nix::sys::signal;
use std::process::Command;
use nix::sys::socket::{
    connect, socket, AddressFamily, ControlMessage, MsgFlags, SockFlag, SockType, UnixAddr,
};
use std::fs;
use std::io::{IoSlice, IoSliceMut};
use std::os::fd::AsRawFd;
use nix::unistd::Pid;

const PIDFILE: &str = "/tmp/pyforked-server.pid";
const SERVER_ADDRESS: &str = "/tmp/pyforked-server.sock";
const SCRIPT: &str = include_str!("../pyforked-server.py");


pub fn start(prelude: &str) -> Result<(), String> {
    // Read PID file if it exists
    if let Some(pid) = read_pid_file(PIDFILE)? {
        if let Err(err) = signal::kill(Pid::from_raw(pid), signal::Signal::SIGTERM) {
            if err != nix::errno::Errno::ESRCH {
                return Err(format!("Failed to kill process: {}", err));
            }
        } else {
            // The process is alive, and we successfully sent SIGKILL. Wait for pidfile to be removed
            let start = std::time::Instant::now();
            while start.elapsed() < std::time::Duration::from_secs(1) {
                if !std::path::Path::new(PIDFILE).exists() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }

            // If the pidfile still exists after 1s, force kill the process
            if let Err(err) = signal::kill(Pid::from_raw(pid), signal::Signal::SIGKILL) {
                if err != nix::errno::Errno::ESRCH {
                    return Err(format!("Failed to kill process: {}", err));
                }
            }
        }
        // Rm the pidfile
        if let Err(e) = fs::remove_file(PIDFILE) {
            if e.kind() != std::io::ErrorKind::NotFound {
                return Err(format!("Failed to remove pidfile: {}", e));
            }
        }
    }
    // Write the server script to a temporary file
    fs::write("/tmp/pyforked-server.py", format!("{}\n{}", prelude, SCRIPT))
        .map_err(|e| format!("Failed to write server script: {}", e))?;

    // Launch the process and wait for it to exit
    let output = Command::new("python3")
        .arg("/tmp/pyforked-server.py")
        .output()
        .map_err(|e| format!("Failed to launch process: {}", e))?;

    if !output.status.success() {
        return Err(format!(
            "Server failed to start: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    // Wait for pidfile and socket to be created
    let start = std::time::Instant::now();
    while start.elapsed() < std::time::Duration::from_secs(60) {
        if std::path::Path::new(PIDFILE).exists() && std::path::Path::new(SERVER_ADDRESS).exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    if !std::path::Path::new(PIDFILE).exists() {
        return Err("Timed out waiting for server to start".into());
    }
    Ok(())
}

pub fn is_alive() -> Result<bool, String> {
    if let Some(pid) = read_pid_file(PIDFILE)? {
        if pid_is_alive(pid)? {
            return Ok(true)
        }
    }
    Ok(false)
}

/// Ensure that the given PID corresponds to a currently running process.
fn pid_is_alive(pid: i32) -> Result<bool, String> {
    if let Err(err) = signal::kill(Pid::from_raw(pid), None) {
        if err == nix::errno::Errno::ESRCH {
            return Ok(false);
        } else {
            return Err(format!("Failed to check process status: {}", err));
        }
    }
    Ok(true)
}

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

/// Read the PID from the given pidfile and return it.
fn read_pid_file(path: &str) -> Result<Option<i32>, String> {
    let pid_str = match fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err("Error reading pidfile".to_string()),
    };
    pid_str
        .trim()
        .parse::<i32>()
        .map_err(|_| "Invalid PID in pidfile".to_string())
        .map(Some)
}
