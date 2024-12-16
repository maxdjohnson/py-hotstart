use anyhow::{Context, Result};
use nix::libc;
use nix::sys::termios::{tcgetattr, tcsetattr, Termios, LocalFlags, InputFlags, OutputFlags, ControlFlags, SetArg};
use nix::sys::signal::SIGWINCH;
use std::os::fd::{BorrowedFd, AsRawFd, AsFd, IntoRawFd, FromRawFd};
use std::io::{Read, Write};
use std::{env, fs};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

// Create wrappers for TIOCGWINSZ and TIOCSWINSZ
nix::ioctl_read_bad!(tiocgwinsz, libc::TIOCGWINSZ, libc::winsize);
nix::ioctl_write_ptr_bad!(tiocswinsz, libc::TIOCSWINSZ, libc::winsize);

fn set_raw_mode(fd: BorrowedFd) -> Result<Termios> {
    let mut termios = tcgetattr(fd)?;
    let original = termios.clone();

    // cfmakeraw equivalent
    termios.input_flags &= !(InputFlags::IGNBRK | InputFlags::BRKINT | InputFlags::PARMRK |
        InputFlags::ISTRIP | InputFlags::INLCR | InputFlags::IGNCR | InputFlags::ICRNL | InputFlags::IXON);
    termios.output_flags &= !OutputFlags::OPOST;
    termios.control_flags &= !(ControlFlags::CSIZE | ControlFlags::PARENB);
    termios.control_flags |= ControlFlags::CS8;
    termios.local_flags &= !(LocalFlags::ECHO | LocalFlags::ECHONL | LocalFlags::ICANON |
        LocalFlags::ISIG | LocalFlags::IEXTEN);

    tcsetattr(fd, SetArg::TCSANOW, &termios)?;
    Ok(original)
}

fn restore_mode(fd: BorrowedFd, original: &Termios) {
    let _ = tcsetattr(fd, SetArg::TCSANOW, original);
}

fn sync_winsize(from_fd: BorrowedFd, to_fd: BorrowedFd) -> Result<()> {
    let mut ws: libc::winsize = libc::winsize { ws_row: 0, ws_col: 0, ws_xpixel: 0, ws_ypixel: 0 };
    unsafe { tiocgwinsz(from_fd.as_raw_fd(), &mut ws) }.context("failed to get winsize")?;
    unsafe { tiocswinsz(to_fd.as_raw_fd(), &ws) }.context("failed to set winsize")?;
    Ok(())
}

pub fn do_proxy(pty_fd: BorrowedFd, exec_mode: &str, user_code: &str, script_args: &Vec<String>) -> Result<()> {
    let cwd = env::current_dir().context("Failed to get current directory")?;
    let cwd_str = cwd.to_str().ok_or_else(|| anyhow::anyhow!("CWD not UTF-8"))?;

    let env_vars: Vec<(String, String)> = env::vars().collect();
    let mut env_lines = String::new();
    for (k,v) in env_vars {
        let k_esc = k.replace("'", "\\'");
        let v_esc = v.replace("'", "\\'");
        env_lines.push_str(&format!("    os.environ['{k_esc}'] = '{v_esc}'\n"));
    }

    let mut argv = vec![];
    match exec_mode {
        "code" => {
            argv.push("".to_string());
        }
        "module" => {
            argv.push(user_code.to_string());
            argv.extend(script_args.iter().cloned());
        }
        "script" => {
            argv.push(user_code.to_string());
            argv.extend(script_args.iter().cloned());
        }
        _ => {}
    }

    let argv_python_list = {
        let mut s = String::from("[");
        for arg in &argv {
            let a_esc = arg.replace("'", "\\'");
            s.push_str(&format!("'{}', ", a_esc));
        }
        s.push(']');
        s
    };

    let setup_code = format!(r#"
import sys, os, runpy

os.environ.clear()
{env_lines}
os.chdir('{cwd_str}')
sys.argv = {argv_python_list}
"#, env_lines=env_lines, cwd_str=cwd_str, argv_python_list=argv_python_list);

    let final_code = match exec_mode {
        "code" => {
            format!("{setup_code}\nexec({:?}, {{'__name__':'__main__'}})", user_code)
        },
        "module" => {
            format!("{setup_code}\nrunpy.run_module({:?}, run_name='__main__')", user_code)
        },
        "script" => {
            let script_contents = fs::read_to_string(&user_code)
                .with_context(|| format!("Failed to read script '{}'", user_code))?;
            format!("{setup_code}\nexec({:?}, {{'__name__':'__main__'}})", script_contents)
        },
        _ => unreachable!()
    };

    let stdin_fd = std::io::stdin().as_fd();
    let stdout_fd = std::io::stdout().as_fd();

    // Set raw mode on user’s terminal
    let original_termios = set_raw_mode(stdin_fd)?;

    // Sync window size
    if let Err(e) = sync_winsize(stdout_fd, pty_fd) {
        eprintln!("Failed to sync window size: {}", e);
    }

    // Handle SIGWINCH to resize pty
    let resize_flag = Arc::new(AtomicBool::new(false));
    {
        let resize_flag = Arc::clone(&resize_flag);
        unsafe {
            let _ = nix::sys::signal::sigaction(SIGWINCH, &nix::sys::signal::SigAction::new(
                nix::sys::signal::SigHandler::Handler(sigwinch_handler),
                nix::sys::signal::SaFlags::empty(),
                nix::sys::signal::SigSet::empty(),
            ));
        }

        extern "C" fn sigwinch_handler(_signum: i32) {
            // Just set a global or atomic flag
            // We'll check it in the main loop
        }

        // We'll rely on checking for SIGWINCH by blocking signals or we can use signalfd.
        // For simplicity, let's do a signalfd approach.

        // Actually, let's use signalfd for SIGWINCH:
        let sigmask = nix::sys::signal::SigSet::empty();
        let signalfd = nix::sys::signalfd::signalfd(-1, &sigmask, nix::sys::signalfd::SfdFlags::SFD_NONBLOCK)
            .expect("signalfd failed");
        // We'll poll on signalfd FD for SIGWINCH
        // Actually we need to add SIGWINCH to sigmask and sigprocmask.

        let mut sigset = nix::sys::signal::SigSet::empty();
        sigset.add(SIGWINCH);
        nix::sys::signal::sigprocmask(nix::sys::signal::SigmaskHow::SIG_BLOCK, Some(&sigset), None).unwrap();

        // Now signalfd will receive SIGWINCH
        // We'll poll on signalfd's fd together with stdin and pty.
    }

    // Write code to interpreter
    {
        // Non-blocking I/O recommended, but let's just blocking write here
        let mut pty_file = unsafe { std::fs::File::from_raw_fd(pty_fd.as_raw_fd()) };
        pty_file.write_all(final_code.as_bytes())?;
        pty_file.flush()?;
        // Keep pty_file open for reading/writing
        // We'll re-wrap it later.
        let pty_fd = pty_file.into_raw_fd();
        // We'll handle I/O below
    }

    // I/O forwarding loop
    // We will use poll to handle events from stdin, pty, and signalfd
    use nix::poll::*;
    let signalfd = {
        let mut sigset = nix::sys::signal::SigSet::empty();
        sigset.add(SIGWINCH);
        nix::sys::signal::sigprocmask(nix::sys::signal::SigmaskHow::SIG_BLOCK, Some(&sigset), None).unwrap();
        nix::sys::signalfd::signalfd(-1, &sigset, nix::sys::signalfd::SfdFlags::SFD_NONBLOCK)
            .expect("signalfd failed")
    };

    let pty_file = unsafe { std::fs::File::from_raw_fd(pty_fd.as_raw_fd()) };
    let stdin_file = std::io::stdin();
    let stdout_file = std::io::stdout();
    let mut pty_fd = pty_file.as_fd();
    let mut stdin_fd = stdin_file.as_fd();
    let mut signalfd_fd = signalfd.as_fd();

    let mut buf = [0u8; 1024];

    loop {
        let mut fds = [
            PollFd::new(stdin_fd, PollFlags::POLLIN),
            PollFd::new(pty_fd, PollFlags::POLLIN),
            PollFd::new(signalfd_fd, PollFlags::POLLIN),
        ];

        nix::poll::poll(&mut fds, -1)?;

        // Check signalfd first
        if let Some(revents) = fds[2].revents() {
            if revents.contains(PollFlags::POLLIN) {
                // read signalfd
                let mut sigbuf = [0u8; 128];
                let n = signalfd.read(&mut sigbuf)?;
                if n > 0 {
                    // We got a signal, likely SIGWINCH
                    if let Err(e) = sync_winsize(stdout_fd, pty_fd) {
                        eprintln!("Failed to sync window size: {}", e);
                    }
                }
            }
        }

        // Check PTY for output
        if let Some(revents) = fds[1].revents() {
            if revents.contains(PollFlags::POLLIN) {
                let n = pty_file.read(&mut buf)?;
                if n == 0 {
                    // Interpreter exited
                    break;
                }
                stdout_file.lock().write_all(&buf[..n])?;
                stdout_file.lock().flush()?;
            }
        }

        // Check STDIN for user input
        if let Some(revents) = fds[0].revents() {
            if revents.contains(PollFlags::POLLIN) {
                let n = stdin_file.lock().read(&mut buf)?;
                if n == 0 {
                    // EOF on stdin - close write side to PTY
                    let _ = nix::unistd::close(pty_fd.as_raw_fd());
                    // TODO: no longer read from stdin
                } else {
                    // Write to PTY
                    nix::unistd::write(pty_fd, &buf[..n])?;
                }
            }
        }
    }
    restore_mode(stdin_fd, &original_termios);
    Ok(())
}
