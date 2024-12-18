use crate::hsserver::server::{ensure, SOCKET_PATH};
use crate::interpreter::{ChildId, Interpreter};
use crate::sendfd::RecvWithFd;
use anyhow::{bail, Context, Result};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;

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

pub fn take_interpreter() -> Result<Interpreter> {
    let stream = send_request("TAKE")?;
    let mut bytes = [0u8; 1024];
    let mut fds = [0; 2];
    let (n_bytes, n_fds) = stream.recv_with_fd(&mut bytes, &mut fds)?;
    let msg = &bytes[..n_bytes];
    if n_fds != 2 {
        bail!("error: {}", String::from_utf8_lossy(msg));
    }
    unsafe { Interpreter::from_raw(msg, &fds[..n_fds]) }
}

pub fn get_exit_code(id: &ChildId) -> Result<i32> {
    let req = format!("EXITCODE {}", id);
    let mut stream = send_request(&req)?;
    let mut buf = [0u8; 1024];
    let n = stream.read(&mut buf)?;
    let resp = String::from_utf8_lossy(&buf[..n]).trim().to_string();
    let exit_code = resp
        .strip_prefix("OK ")
        .with_context(|| format!("unexpected exit code response {}", resp))?;
    exit_code
        .parse::<i32>()
        .context("Failed to parse exit code from server")
}
