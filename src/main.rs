use nix::errno::Errno;
use signal_hook::flag;
use nix::pty::openpty;
use std::sync::Arc;
use nix::Error;
use nix::libc;
use nix::sys::select::{pselect, FdSet};
use nix::sys::signal::{SigSet, Signal};
use nix::sys::termios::{cfmakeraw, tcgetattr, tcsetattr, SetArg, Termios};
use nix::unistd::{read, write, Pid};
use std::env;
use std::os::fd::{AsFd, AsRawFd};
use std::sync::atomic::{AtomicBool, Ordering};

// According to the prompt, attach_child is already implemented as:
//
// fn attach_child(pid: nix::unistd::Pid, slave_fd: impl std::os::fd::AsRawFd) -> nix::Result<()>;
extern "C" {
    fn attach_child(pid: Pid, fd: std::os::raw::c_int) -> i32;
}

// For Rust call, we wrap attach_child:
// We'll assume it returns 0 on success, else sets errno or something similar.
fn attach_child_wrapper(pid: Pid, fd: &impl AsFd) -> nix::Result<()> {
    let ret = unsafe { attach_child(pid, fd.as_fd().as_raw_fd()) };
    if ret == 0 {
        Ok(())
    } else {
        Err(nix::Error::last())
    }
}

// Create wrappers for TIOCGWINSZ and TIOCSWINSZ
nix::ioctl_read_bad!(tiocgwinsz, libc::TIOCGWINSZ, libc::winsize);
nix::ioctl_write_ptr_bad!(tiocswinsz, libc::TIOCSWINSZ, libc::winsize);

fn die(msg: &str) -> ! {
    eprintln!("[!] {}", msg);
    std::process::exit(1);
}


fn write_all<Fd: AsFd>(fd: Fd, mut buf: &[u8]) -> Result<(), Error> {
    while !buf.is_empty() {
        match write(fd.as_fd(), buf) {
            Ok(0) => return Err(Error::from(Errno::EIO)),
            Ok(n) => {
                buf = &buf[n..];
            }
            Err(Error::EINTR) => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}


fn setup_raw() -> nix::Result<Termios> {
    let mut termios = tcgetattr(std::io::stdin().as_fd())?;
    let saved = termios.clone();
    cfmakeraw(&mut termios);
    tcsetattr(std::io::stdin().as_fd(), SetArg::TCSANOW, &termios)?;
    Ok(saved)
}

fn resize_pty<Fd: AsFd>(pty_fd: Fd) -> nix::Result<()> {
    let mut ws = libc::winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };

    let stdin_fd = std::io::stdin().as_fd().as_raw_fd();
    let pty_raw = pty_fd.as_fd().as_raw_fd();

    unsafe {
        // tiocgwinsz requires a pointer to winsize. We'll do a read-modify:
        if tiocgwinsz(stdin_fd, &mut ws as *mut libc::winsize).is_err() {
            // fallback
            let default_size = libc::winsize {
                ws_row: 30,
                ws_col: 80,
                ws_xpixel: 640,
                ws_ypixel: 480,
            };
            tiocswinsz(pty_raw, &default_size as *const libc::winsize)?;
            return Ok(());
        }
        tiocswinsz(pty_raw, &ws as *const libc::winsize)?;
    }

    Ok(())
}

fn do_proxy<Fd: AsFd>(pty_fd: Fd) -> nix::Result<()> {
    let winch_happened: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
    let pty_raw = pty_fd.as_fd().as_raw_fd();
    let stdin = std::io::stdin();
    let stdin_fd = stdin.as_fd();

    // Handle SIGWINCH by setting flag so it can be delivered to child.
    flag::register(signal_hook::consts::SIGWINCH, Arc::clone(&winch_happened)).expect("failed to register SIGWINCH");

    resize_pty(&pty_fd)?;

    let mut buf = [0u8; 4096];

    // Blocks SIGWINCH except during the pselect() call to avoid race conditions.
    let mut sigmask = SigSet::empty();
    sigmask.add(Signal::SIGWINCH);
    sigmask.thread_block()?;

    let sigmask_empty = SigSet::empty();
    loop {
        if winch_happened.swap(false, Ordering::SeqCst) {
            resize_pty(&pty_fd)?;
        }

        let mut readfds = FdSet::new();
        readfds.insert(stdin_fd);
        readfds.insert(pty_fd.as_fd());

        pselect(None, &mut readfds, None, None, None, &sigmask_empty)?;

        if readfds.contains(stdin_fd) {
            match read(stdin_fd.as_raw_fd(), &mut buf) {
                Ok(0) => return Ok(()), // EOF
                Ok(n) => {
                    write_all(&pty_fd, &buf[..n])?;
                }
                Err(Error::EINTR) => continue,
                Err(e) => return Err(e),
            }
        }

        if readfds.contains(pty_fd.as_fd()) {
            match read(pty_raw, &mut buf) {
                Ok(0) => return Ok(()), // EOF
                Ok(n) => {
                    write_all(std::io::stdout().as_fd(), &buf[..n])?;
                }
                Err(Error::EINTR) => continue,
                Err(e) => return Err(e),
            }
        }
    }
}

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        die("Usage: reptyr_rust <pid>");
    }

    let pid_val = match args[1].parse::<i32>() {
        Ok(v) if v > 0 => v,
        _ => die("Invalid pid: must be a positive integer"),
    };

    let pid = Pid::from_raw(pid_val);

    // Use openpty to obtain master/slave fds
    let (master, slave) = match openpty(None, None) {
        Ok(p) => (p.master, p.slave),
        Err(e) => die(&format!("Unable to allocate pty: {}", e)),
    };

    // Attach child to slave pty
    if let Err(e) = attach_child_wrapper(pid, &slave) {
        eprintln!("Unable to attach to pid {}: {}", pid_val, e);
        std::process::exit(1);
    }
    drop(slave);

    let saved_termios = match setup_raw() {
        Ok(t) => t,
        Err(e) => die(&format!("Unable to set terminal attributes: {}", e)),
    };

    if let Err(e) = do_proxy(&master) {
        eprintln!("Error in do_proxy: {}", e);
    }

    // Restore terminal attributes
    loop {
        match tcsetattr(std::io::stdin().as_fd(), SetArg::TCSANOW, &saved_termios) {
            Ok(_) => break,
            Err(Error::EINTR) => continue,
            Err(e) => die(&format!("Unable to tcsetattr: {}", e)),
        }
    }
}
