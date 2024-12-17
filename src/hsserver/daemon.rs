use anyhow::{anyhow, bail, Context, Result};
use nix::errno::Errno;
use nix::fcntl::{open, OFlag};
use nix::libc;
use nix::sys::signal::kill;
use nix::sys::stat::Mode;
use nix::unistd::Pid;
use nix::unistd::{close, dup2, fork, setsid, ForkResult};
use std::env;
use std::fs::{hard_link, read_to_string, remove_file, OpenOptions};
use std::io::Write;
use std::os::fd::RawFd;
use std::path::{Path, PathBuf};
use std::process;

const LOGFILE: &str = "/tmp/py-hotstart.log";

pub fn daemonize() -> Result<ForkResult> {
    // First fork
    if let ForkResult::Parent { child } = unsafe { fork() }? {
        return Ok(ForkResult::Parent { child });
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
    match unsafe { fork() } {
        Ok(ForkResult::Parent { .. }) => process::exit(0),
        Ok(ForkResult::Child) => {}
        Err(e) => {
            eprintln!("second fork failed: {:#}", e);
            process::exit(1);
        }
    }

    // Move to root directory
    if let Err(e) = env::set_current_dir("/") {
        eprintln!("Failed to chdir to '/': {:#}", e);
        process::exit(1);
    }

    // Daemon is fully set up
    // Log a message indicating successful daemonization
    eprintln!("py-hotstart daemonized");
    Ok(ForkResult::Child)
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

pub struct PidFileGuard {
    pid_file_path: PathBuf,
}

impl PidFileGuard {
    pub fn test<P: AsRef<Path>>(path: P) -> Result<Option<Pid>> {
        if path.as_ref().exists() {
            let contents = read_to_string(path.as_ref())?;
            let pid_str = contents.trim();
            if let Ok(other_pid) = pid_str.parse::<i32>() {
                if process_is_alive(other_pid)? {
                    return Ok(Some(Pid::from_raw(other_pid)));
                }
            }
            // Otherwise, treat it as stale PID file
            std::fs::remove_file(&path)?;
        }
        Ok(None)
    }

    /// Create a PID file atomically. If a PID file already exists:
    /// - Check if that process is alive. If yes, return an error (another instance is running).
    /// - If not, remove the stale PID file and try creating again.
    pub fn new<P: AsRef<Path>>(pid: Pid, path: P) -> Result<PidFileGuard> {
        // If file exists and it points to a running process, bail
        if let Some(other_pid) = Self::test(&path)? {
            bail!("file exists for running process other_pid={}", other_pid);
        }

        // Write to a temporary file first, then rename to the final PID file for atomicity.
        let pid_file_path = path.as_ref().to_path_buf();
        let tmp_file_path = pid_file_path.with_extension(format!("pid.{}", pid));
        {
            let mut file = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&tmp_file_path)?;
            writeln!(file, "{}", pid)?;
            file.sync_all()?; // Ensure data is flushed
        }

        // Use hard_link to atomically create pidfile, and error if it alrady exists.
        let result = hard_link(&tmp_file_path, &pid_file_path);
        remove_file(&tmp_file_path)?;
        result.context("hard_link error")?;

        Ok(PidFileGuard { pid_file_path })
    }
}

impl Drop for PidFileGuard {
    fn drop(&mut self) {
        // Attempt to remove the PID file. Errors are ignored.
        let _ = std::fs::remove_file(&self.pid_file_path);
    }
}

fn process_is_alive(pid: i32) -> Result<bool> {
    match kill(Pid::from_raw(pid), None) {
        Ok(_) => Ok(true),
        Err(Errno::ESRCH) => Ok(false),
        Err(e) => Err(anyhow!("process_is_alive: kill error {}", e)),
    }
}
