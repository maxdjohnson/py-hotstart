use nix::fcntl::{fcntl, FcntlArg, OFlag};
use nix::pty::openpty;
use nix::ioctl_read;
use nix::libc::{winsize, TIOCGWINSZ};
use nix::unistd::isatty;
use nix::unistd::Pid;
use nix::sys::signal;
use nix::sys::select::{select, FdSet};
use nix::sys::socket::{
    AddressFamily, UnixAddr, SockType, SockFlag, socket, connect,
    sendmsg, recvmsg, ControlMessage, MsgFlags,
};
use nix::sys::termios::{tcgetattr, LocalFlags};
use nix::unistd::{read, write};
use std::os::fd::{AsFd, AsRawFd};
use std::io::{IoSlice, IoSliceMut};
use std::process::exit;
use std::fs;
use std::env;

const PIDFILE: &str = "/tmp/pyforked-server.pid";
const SERVER_ADDRESS: &str = "/tmp/pyforked-server.sock";
const CTRL_D: u8 = 0x04;

// Define the tiocgwinsz ioctl operation
ioctl_read!(tiocgwinsz, TIOCGWINSZ, 0, winsize);

fn main() {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let stdin_borrowed = stdin.as_fd();
    let stdout_borrowed = stdout.as_fd();

    // Check if stdin is a TTY
    if !isatty(stdin.as_raw_fd()).unwrap_or(false) {
        eprintln!("stdin is not a tty, cannot inherit terminfo");
        std::process::exit(1);
    }
    // Parse arguments
    // Supported:
    // -c code_snippet
    // -m module_name
    // If neither is given, default snippet runs REPL
    let args: Vec<String> = env::args().collect();
    let mut code_snippet = String::new();
    let mut module_name = String::new();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "-c" => {
                i += 1;
                if i < args.len() {
                    code_snippet = args[i].clone();
                } else {
                    eprintln!("No code snippet provided after -c");
                    exit(1);
                }
            }
            "-m" => {
                i += 1;
                if i < args.len() {
                    module_name = args[i].clone();
                } else {
                    eprintln!("No module name provided after -m");
                    exit(1);
                }
            }
            _ => {
                eprintln!("Unknown argument: {}", args[i]);
                exit(1);
            }
        }
        i += 1;
    }

    // If module_name is given, ignore code_snippet and generate snippet for module.
    if !module_name.is_empty() {
        code_snippet = format!("import runpy; runpy.run_module('{}', run_name='__main__')", module_name);
    } else if code_snippet.is_empty() {
        // Default snippet: run a Python REPL
        code_snippet = "import code; code.interact(local={})".to_string();
    }

    // Check if forkserver is running by checking pidfile
    let pid = match fs::read_to_string(PIDFILE) {
        Ok(s) => s.trim().parse::<i32>().ok(),
        Err(_) => None,
    };
    if pid.is_none() {
        eprintln!("Forkserver not running (no pidfile). Please start it first.");
        exit(1);
    }
    let pid = pid.unwrap();

    // Check if process with that pid is alive
    if let Err(err) = signal::kill(Pid::from_raw(pid), None) {
        if err == nix::errno::Errno::ESRCH {
            eprintln!(
                "No process with pid {} is alive. The forkserver might have crashed. Please restart it.",
                pid
            );
            exit(1);
        } else {
            eprintln!("Failed to check process status: {}", err);
            exit(1);
        }
    }

    // Construct the message: "RUN <code_snippet>"
    let mut message = b"RUN".to_vec();
    if !code_snippet.is_empty() {
        message.push(b' ');
        message.extend_from_slice(code_snippet.as_bytes());
    }

    // Get current terminal attributes from CLI
    let mut termios = match tcgetattr(&stdin) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("Failed to get terminal attributes: {}", e);
            std::process::exit(1);
        }
    };
    // Ensure canonical mode:
    termios.local_flags |= LocalFlags::ICANON;

    // Get current window size
    let ws = get_winsize(&stdin);

    // Allocate pty locally
    let pty = openpty(&ws, Some(&termios)).expect("openpty failed");
    let master_fd = pty.master;
    let slave_fd = pty.slave;


    // Connect to forkserver
    let fd = socket(AddressFamily::Unix, SockType::Stream, SockFlag::empty(), None)
        .expect("socket failed");
    let addr = UnixAddr::new(SERVER_ADDRESS).expect("UnixAddr failed");

    if let Err(e) = connect(fd.as_raw_fd(), &addr) {
        eprintln!("Unable to connect to forkserver: {}", e);
        exit(1);
    }

    // Send slave_fd via SCM_RIGHTS along with the message
    let fd_arr = [slave_fd.as_raw_fd()];
    let cmsg = [ControlMessage::ScmRights(&fd_arr)];
    let iov = [IoSlice::new(&message)];

    if let Err(e) = sendmsg::<()>(fd.as_raw_fd(), &iov, &cmsg, MsgFlags::empty(), None) {
        eprintln!("Failed to send message and fd to server: {}", e);
        exit(1);
    }

    // Receive response from server
    let mut buf = [0u8; 1024];
    let msg_bytes = {
        let mut iov = [IoSliceMut::new(&mut buf)];
        match recvmsg::<()>(fd.as_raw_fd(), &mut iov, None, MsgFlags::empty()) {
            Ok(msg) => {
                if msg.bytes == 0 {
                    eprintln!("Server disconnected prematurely (no data).");
                    exit(1);
                }
                msg.bytes
            },
            Err(e) => {
                eprintln!("Error receiving response from server: {}", e);
                exit(1);
            }
        }
    };

    let response = &buf[..msg_bytes];
    if response != b"OK" {
        eprintln!("Server responded with invalid message: {:?}", response);
        exit(1);
    }

    // Close slave fd locally
    drop(slave_fd);

    // Set nonblocking
    set_nonblocking(stdin_borrowed.as_raw_fd());
    set_nonblocking(stdout_borrowed.as_raw_fd());
    set_nonblocking(master_fd.as_raw_fd());

    let mut buf_in = [0u8; 1024];
    let mut buf_out = [0u8; 1024];

    let mut stdin_eof = false;

    loop {
        let mut fds = FdSet::new();
        // Only listen on stdin if we haven't hit EOF yet
        if !stdin_eof {
            fds.insert(stdin_borrowed);
        }
        fds.insert(master_fd.as_fd());

        let nfds = std::cmp::max(stdin_borrowed.as_raw_fd(), master_fd.as_raw_fd()) + 1;
        let res = select(nfds, Some(&mut fds), None, None, None);
        if let Err(e) = res {
            eprintln!("select error: {}", e);
            break;
        }

        // If something happened on stdin and not EOF yet
        if !stdin_eof && fds.contains(stdin_borrowed) {
            match read(stdin_borrowed.as_raw_fd(), &mut buf_in) {
                Ok(n) if n > 0 => {
                    if !write_all(&master_fd.as_fd(), &buf_in[..n]) {
                        eprintln!("Error writing to master fd.");
                        break;
                    }
                }
                Ok(0) => {
                    // EOF on stdin, user closed input. Stop the session.
                    break;
                }
                Err(_) => {
                    // Non-fatal error or EAGAIN
                }
                _ => {}
            }
        }

        if fds.contains(master_fd.as_fd()) {
            match read(master_fd.as_raw_fd(), &mut buf_out) {
                Ok(n) if n > 0 => {
                    if !write_all(&stdout_borrowed, &buf_out[..n]) {
                        eprintln!("Error writing to stdout.");
                        break;
                    }
                }
                Ok(0) => {
                    // Child process exited or master closed
                    eprintln!("Child exited or master closed. Ending session.");
                    break;
                }
                Err(_) => {
                    // Non-fatal error or EAGAIN
                }
                _ => {}
            }
        }
    }

    eprintln!("CLI shutting down cleanly.");
}

fn get_winsize(fd: impl AsFd) -> Option<winsize> {
    let mut ws = winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };

    // Use the generated ioctl function
    let res = unsafe { tiocgwinsz(fd.as_fd().as_raw_fd(), &mut ws) };
    if res.is_ok() {
        Some(ws)
    } else {
        None
    }
}

fn write_all(fd: &impl AsFd, mut data: &[u8]) -> bool {
    while !data.is_empty() {
        match write(fd, data) {
            Ok(n) if n > 0 => {
                data = &data[n..];
            }
            Ok(0) => {
                // Handle unexpected EOF or write returning 0
                return false;
            }
            Err(_) => {
                // Handle write error
                return false;
            }
            _ => unreachable!(),
        }
    }
    true
}

fn set_nonblocking(fd: i32) {
    let flags = fcntl(fd, FcntlArg::F_GETFL).unwrap();
    let new_flags = OFlag::from_bits_truncate(flags) | OFlag::O_NONBLOCK;
    fcntl(fd, FcntlArg::F_SETFL(new_flags)).unwrap();
}
