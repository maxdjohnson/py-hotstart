use anyhow::{Context, Result, bail};
use nix::sys::socket::{recvmsg, MsgFlags, ControlMessageOwned};
use std::os::fd::{OwnedFd, FromRawFd};
use std::os::unix::net::UnixStream;
use std::io::IoSliceMut;
use std::os::unix::io::{AsRawFd, RawFd};
use std::io::{Read, Write};
use nix::cmsg_space;

const SOCKET_PATH: &str = "/tmp/py_hotstart.sock";

struct ClientInterpreter {
    pub id: String,
    pub pty_master_fd: OwnedFd
}

fn send_request(req: &str) -> Result<UnixStream> {
    let mut stream = UnixStream::connect(SOCKET_PATH)
        .context("Failed to connect to server")?;
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
    ).context("Failed to recvmsg")?;

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

    let id = resp_str.strip_prefix("OK ").with_context(|| format!("invalid response {}", resp_str))?;
    let pty_fd_value = pty_fd.context("missing fd")?;
    Ok(ClientInterpreter { id: id.to_string(), pty_master_fd: pty_fd_value })
}

pub fn get_exit_code(interpreter: &ClientInterpreter) -> Result<i32> {
    let req = format!("EXITCODE {}", interpreter.id);
    let mut stream = send_request(&req)?;
    let mut buf = [0u8; 1024];
    let n = stream.read(&mut buf)?;
    let resp = String::from_utf8_lossy(&buf[..n]).trim().to_string();
    resp.parse::<i32>().context("Failed to parse exit code from server")
}
