use anyhow::{anyhow, Context, Result};
use nix::pty::openpty;
use nix::sys::socket::{
    connect, socket, AddressFamily, ControlMessage, MsgFlags, SockFlag, SockType, UnixAddr,
};
use nix::sys::termios::tcgetattr;
use nix::unistd::dup;
use std::fs;
use std::io::Read;
use std::io::{IoSlice, IoSliceMut};
use std::os::fd::AsRawFd;
use std::os::unix::io::FromRawFd;
use std::process::Command;

const SCRIPT: &str = include_str!("./pyhotstart.py");


fn run() {

    fs::write(
        "/tmp/pyhotstart.py",
        format!("{}\n{}", prelude, SCRIPT),
    )
    .context("Failed to write server script")?;

    let termios = tcgetattr(std::io::stdin()).ok();
    let (master, slave) = openpty(None, &termios)
        .map(|p| (p.master, p.slave))
        .context("Unable to allocate pty")?;

    let stdin_fd = dup(slave.as_raw_fd()).context("Failed to dup stdin_fd")?;
    let stdout_fd = dup(slave.as_raw_fd()).context("Failed to dup stdout_fd")?;
    let stderr_fd = dup(slave.as_raw_fd()).context("Failed to dup stderr_fd")?;

    let mut child = Command::new("python3")
        .arg("/tmp/pyhotstart.py")
        .stdin(unsafe { std::process::Stdio::from_raw_fd(stdin_fd) })
        .stdout(unsafe { std::process::Stdio::from_raw_fd(stdout_fd) })
        .stderr(unsafe { std::process::Stdio::from_raw_fd(stderr_fd) })
        .spawn()
        .context("Failed to spawn process")?;

    drop(slave);

    let mut output = String::new();
    let mut master_file: std::fs::File = master.into();
    master_file
        .read_to_string(&mut output)
        .context("Failed to read from master")?;

    let status = child.wait().context("Failed to wait for child process")?;
    if !status.success() {
        return Err(anyhow!("Server failed to start: {}\n{}", status, output));
    }
}
