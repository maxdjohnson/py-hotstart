use anyhow::{Context, Result};
use nix::fcntl::{open, OFlag};
use nix::libc;
use nix::sys::stat::Mode;
use nix::unistd::{close, dup2, fork, setsid, ForkResult};
use std::env;
use std::os::fd::RawFd;
use std::process;

const LOGFILE: &str = "/tmp/py-hotstart.log";

fn daemonize() -> Result<()> {
    // First fork. After this
    if let ForkResult::Parent { .. } = unsafe { fork() }? {
        process::exit(0);
    }

    // At this point, we're the child of the original process, but not yet fully daemonized.
    // Setup stdout/stderr to go to the logfile now, so if setsid fails, we can log it.
    if let Err(err) = setup_stdio(LOGFILE) {
        eprintln!("Failed to setup stdio: {:#}", err);
        process::exit(1);
    }

    // Now perform setsid and log any errors
    if let Err(e) = setsid() {
        eprintln!("setsid failed: {:#}", e);
        process::exit(1);
    }

    // Second fork to ensure we can't re-acquire a controlling terminal
    if let ForkResult::Parent { .. } = unsafe { fork() }? {
        process::exit(0);
    }

    // Move to root directory
    if let Err(e) = env::set_current_dir("/") {
        eprintln!("Failed to chdir to '/': {:#}", e);
        process::exit(1);
    }

    // Daemon is fully set up
    // Log a message indicating successful daemonization
    eprintln!("py-hotstart daemonized");
    Ok(())
}

/// Redirect stdin to /dev/null and stdout/stderr to the specified logfile.
fn setup_stdio(logfile: &str) -> Result<()> {
    // Redirect stdin to /dev/null
    let null_fd = open("/dev/null", OFlag::O_RDONLY, Mode::empty())
        .context("Opening /dev/null for reading failed")?;
    dup2(null_fd, libc::STDIN_FILENO).context("dup2 for stdin failed")?;
    close(null_fd).ok();

    // Open logfile for stdout/stderr
    let log_fd = open(
        logfile,
        OFlag::O_WRONLY | OFlag::O_CREAT | OFlag::O_APPEND,
        Mode::from_bits(0o644).unwrap(),
    )
    .with_context(|| format!("Opening {} for writing failed", logfile))?;

    redirect_fd(log_fd, libc::STDOUT_FILENO)?;
    redirect_fd(log_fd, libc::STDERR_FILENO)?;
    if log_fd > 2 {
        close(log_fd)?;
    }
    Ok(())
}

/// Duplicate `fd` into `target_fd`.
fn redirect_fd(fd: RawFd, target_fd: RawFd) -> Result<()> {
    dup2(fd, target_fd).with_context(|| format!("dup2 failed for fd {} to {}", fd, target_fd))?;
    Ok(())
}
