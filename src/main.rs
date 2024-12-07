use nix::pty::{openpty, Winsize};
use nix::unistd::{read, write};
use nix::sys::socket::{
    AddressFamily, SockAddr, UnixAddr, socket, connect, sendmsg, recvmsg,
    ControlMessage, MsgFlags, SocketType,
};
use nix::sys::uio::IoVec;
use nix::fcntl::{fcntl, FcntlArg, OFlag};
use std::os::unix::io::RawFd;
use std::process::exit;

fn main() {
    let server_path = "/tmp/pyforked-server.sock";

    // Ensure pyforked-server is running externally.

    // Allocate pty locally
    let pty = openpty(None, None).expect("openpty failed");
    // pty.master and pty.slave are our FDs
    // We'll send pty.slave to the server

    let fd = socket(AddressFamily::Unix, SocketType::Stream, 0).unwrap();
    let addr = UnixAddr::new(server_path).unwrap();
    connect(fd, &SockAddr::Unix(addr)).unwrap();

    // send pty.slave via SCM_RIGHTS
    {
        let iov = [IoVec::from_slice(b"RUN")];
        let cmsg = [ControlMessage::ScmRights(&[pty.slave])];
        sendmsg(fd, &iov, &cmsg, MsgFlags::empty(), None).unwrap();
    }

    // Receive response from server
    let mut buf = [0u8; 1024];
    let msg = recvmsg(fd, &mut [IoVec::from_mut_slice(&mut buf)], None, MsgFlags::empty()).unwrap();
    let response = &buf[..msg.bytes];
    if response != b"OK" {
        eprintln!("Server did not respond with OK");
        exit(1);
    }

    // We no longer need pty.slave fd
    nix::unistd::close(pty.slave).unwrap();

    // Now proxy data between our stdin/stdout and pty.master
    set_nonblocking(0);
    set_nonblocking(1);
    set_nonblocking(pty.master);

    let stdin_fd = 0;
    let stdout_fd = 1;
    let master_fd = pty.master;

    let mut buf_in = [0u8; 1024];
    let mut buf_out = [0u8; 1024];

    loop {
        let mut fds = nix::sys::select::FdSet::new();
        fds.insert(stdin_fd);
        fds.insert(master_fd);
        let nfds = std::cmp::max(stdin_fd, master_fd) + 1;
        let res = nix::sys::select::select(
            nfds,
            Some(&mut fds),
            None,
            None,
            None
        );
        if res.is_err() {
            break;
        }

        if fds.contains(stdin_fd) {
            match read(stdin_fd, &mut buf_in) {
                Ok(n) if n > 0 => {
                    let _ = write(master_fd, &buf_in[..n]);
                }
                _ => {}
            }
        }

        if fds.contains(master_fd) {
            match read(master_fd, &mut buf_out) {
                Ok(n) if n > 0 => {
                    let _ = write(stdout_fd, &buf_out[..n]);
                }
                _ => {}
            }
        }
    }
}

fn set_nonblocking(fd: RawFd) {
    let flags = fcntl(fd, FcntlArg::F_GETFL).unwrap();
    let new_flags = OFlag::from_bits_truncate(flags) | OFlag::O_NONBLOCK;
    fcntl(fd, FcntlArg::F_SETFL(new_flags)).unwrap();
}
