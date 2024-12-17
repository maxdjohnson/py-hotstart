use crate::hsserver::server::{ensure, SOCKET_PATH};
use anyhow::{bail, Context, Result};
use nix::cmsg_space;
use nix::sys::socket::{recvmsg, ControlMessageOwned, MsgFlags};
use std::io::IoSliceMut;
use std::io::{Read, Write};
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::io::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;

pub struct ClientInterpreter {
    pub id: String,
    pub pty_master_fd: OwnedFd,
}

pub fn ensure_server() -> Result<()> {
    ensure()?;

    // Wait for socket to be created
    let start = std::time::Instant::now();
    while start.elapsed() < std::time::Duration::from_secs(1) {
        if std::path::Path::new(SOCKET_PATH).exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    if !std::path::Path::new(SOCKET_PATH).exists() {
        bail!("Timed out waiting for server to start");
    }
    Ok(())
}

fn send_request(req: &str) -> Result<UnixStream> {
    let mut stream = UnixStream::connect(SOCKET_PATH).context("Failed to connect to server")?;
    stream.write_all(req.as_bytes())?;
    stream.flush()?;
    Ok(stream)
}

pub fn initialize(prelude: &str) -> Result<()> {
    let mut stream = send_request(&format!("INIT {}", prelude))?;
    let mut buf = [0u8; 1024];
    let n = stream.read(&mut buf)?;
    let resp = String::from_utf8_lossy(&buf[..n]).to_string();
    if resp.trim() == "OK" {
        Ok(())
    } else {
        bail!("INIT failed: {}", resp)
    }
}

pub fn take_interpreter() -> Result<ClientInterpreter> {
    let stream = send_request("TAKE")?;
    let mut buf = [0u8; 32];
    let mut iov = [IoSliceMut::new(&mut buf)];
    let mut cmsgspace = cmsg_space!([RawFd; 1]);

    let msg = recvmsg::<()>(
        stream.as_raw_fd(),
        &mut iov,
        Some(&mut cmsgspace),
        MsgFlags::empty(),
    )
    .context("Failed to recvmsg")?;

    if msg.bytes == 0 {
        bail!("No data received from server");
    }

    let mut pty_fd: Option<OwnedFd> = None;
    for cmsg in msg.cmsgs()? {
        if let ControlMessageOwned::ScmRights(fds) = cmsg {
            pty_fd = fds.get(0).map(|fd| unsafe { OwnedFd::from_raw_fd(*fd) })
        }
    }
    let resp_str = String::from_utf8_lossy(&iov[0]);

    let id = resp_str
        .strip_prefix("OK ")
        .with_context(|| format!("invalid response {}", resp_str))?;
    let pty_fd_value = pty_fd.context("missing fd")?;
    Ok(ClientInterpreter {
        id: id.to_string(),
        pty_master_fd: pty_fd_value,
    })
}

pub fn get_exit_code(interpreter: &ClientInterpreter) -> Result<i32> {
    let req = format!("EXITCODE {}", interpreter.id);
    let mut stream = send_request(&req)?;
    let mut buf = [0u8; 1024];
    let n = stream.read(&mut buf)?;
    let resp = String::from_utf8_lossy(&buf[..n]).trim().to_string();
    resp.parse::<i32>()
        .context("Failed to parse exit code from server")
}
