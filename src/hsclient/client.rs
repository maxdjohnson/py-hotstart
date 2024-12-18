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
    pub control_fd: OwnedFd,
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
    let mut cmsgspace = cmsg_space!([RawFd; 2]);

    let (n, control_fd, pty_fd) = {
        let msg = recvmsg::<()>(
            stream.as_raw_fd(),
            &mut iov,
            Some(&mut cmsgspace),
            MsgFlags::empty(),
        )
        .context("Failed to recvmsg")?;
        if msg.bytes == 0 {
            bail!("No message in response");
        }
        let mut control_fd: Option<OwnedFd> = None;
        let mut pty_fd: Option<OwnedFd> = None;
        for cmsg in msg.cmsgs()? {
            if let ControlMessageOwned::ScmRights(fds) = cmsg {
                let mut owned_fds: Vec<OwnedFd> = fds.into_iter().map(|fd| unsafe { OwnedFd::from_raw_fd(fd) }).collect();
                control_fd = Some(owned_fds.remove(0));
                pty_fd = Some(owned_fds.remove(0));
            }
        }
        (msg.bytes, control_fd.context("No control_fd in response")?, pty_fd.context("No pty_fd in response")?)
    };

    let resp_str = String::from_utf8_lossy(&iov[0][..n]);
    let id = resp_str
        .strip_prefix("OK ")
        .with_context(|| format!("invalid response {}", resp_str))?;
    Ok(ClientInterpreter {
        id: id.to_string(),
        control_fd,
        pty_master_fd: pty_fd,
    })
}

pub fn get_exit_code(id: &str) -> Result<i32> {
    let req = format!("EXITCODE {}", id);
    let mut stream = send_request(&req)?;
    let mut buf = [0u8; 1024];
    let n = stream.read(&mut buf)?;
    let resp = String::from_utf8_lossy(&buf[..n]).trim().to_string();
    let exit_code = resp.strip_prefix("OK ")
        .with_context(|| format!("unexpected exit code response {}", resp))?;
    exit_code.parse::<i32>()
        .context("Failed to parse exit code from server")
}
