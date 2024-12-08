use nix::fcntl::{fcntl, FcntlArg, OFlag};
use nix::pty::openpty;
use nix::sys::select::{select, FdSet};
use nix::sys::socket::{
    AddressFamily, SockaddrLike, UnixAddr, SockType, SockFlag, socket, connect,
    sendmsg, recvmsg, ControlMessage, MsgFlags,
};
use nix::unistd::{close, read, write};
use std::os::fd::{AsFd, BorrowedFd, RawFd};
use std::process::exit;
use std::io::{IoSlice, IoSliceMut};

fn main() {
    let server_path = "/tmp/pyforked-server.sock";

    // Allocate pty locally
    let pty = openpty(None, None).expect("openpty failed");
    let master_fd = pty.master;
    let slave_fd = pty.slave;

    // Create a Unix socket and connect
    let fd = socket(AddressFamily::Unix, SockType::Stream, SockFlag::empty(), None)
        .expect("socket failed");
    let addr = UnixAddr::new(server_path).expect("UnixAddr failed");
    connect(fd, &addr).expect("connect failed");

    // send pty.slave via SCM_RIGHTS
    {
        let iov = [IoSlice::new(b"RUN")];
        let cmsg = [ControlMessage::ScmRights(&[slave_fd])];
        sendmsg(BorrowedFd::borrow_raw(fd), &iov, &cmsg, MsgFlags::empty(), None)
            .expect("sendmsg failed");
    }

    // Receive response from server
    let mut buf = [0u8; 1024];
    let mut iov = [IoSliceMut::new(&mut buf)];
    let msg = recvmsg(BorrowedFd::borrow_raw(fd), &mut iov, None, MsgFlags::empty())
        .expect("recvmsg failed");
    let response = &buf[..msg.bytes];
    if response != b"OK" {
        eprintln!("Server did not respond with OK");
        exit(1);
    }

    // Close slave fd locally
    close(slave_fd).expect("close slave failed");

    // Now proxy data between stdin/stdout and pty.master
    set_nonblocking(0);
    set_nonblocking(1);
    set_nonblocking(master_fd);

    let stdin_fd = 0;
    let stdout_fd = 1;

    let mut buf_in = [0u8; 1024];
    let mut buf_out = [0u8; 1024];

    loop {
        let mut fds = FdSet::new();
        fds.insert(BorrowedFd::borrow_raw(stdin_fd));
        fds.insert(BorrowedFd::borrow_raw(master_fd));
        let nfds = std::cmp::max(stdin_fd, master_fd) + 1;
        let res = select(
            nfds,
            Some(&mut fds),
            None,
            None,
            None
        );
        if let Err(_) = res {
            break;
        }

        if fds.contains(BorrowedFd::borrow_raw(stdin_fd)) {
            match read(BorrowedFd::borrow_raw(stdin_fd), &mut buf_in) {
                Ok(n) if n > 0 => {
                    let _ = write(BorrowedFd::borrow_raw(master_fd), &buf_in[..n]);
                }
                _ => {}
            }
        }

        if fds.contains(BorrowedFd::borrow_raw(master_fd)) {
            match read(BorrowedFd::borrow_raw(master_fd), &mut buf_out) {
                Ok(n) if n > 0 => {
                    let _ = write(BorrowedFd::borrow_raw(stdout_fd), &buf_out[..n]);
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
