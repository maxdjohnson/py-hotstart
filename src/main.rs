use nix::fcntl::{fcntl, FcntlArg, OFlag};
use nix::pty::openpty;
use nix::sys::select::{select, FdSet};
use nix::sys::socket::{
    AddressFamily, UnixAddr, SockType, SockFlag, socket, connect,
    sendmsg, recvmsg, ControlMessage, MsgFlags,
};
use nix::unistd::{close, read, write};
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::io::{IoSlice, IoSliceMut};
use std::process::exit;

fn main() {
    let server_path = "/tmp/pyforked-server.sock";

    // Allocate pty locally
    let pty = openpty(None, None).expect("openpty failed");
    let master_fd = pty.master;
    let slave_fd = pty.slave;

    // Convert the standard input/output to BorrowedFd
    let stdin_borrowed = std::io::stdin().as_fd();
    let stdout_borrowed = std::io::stdout().as_fd();

    // Socket to communicate with the forkserver
    let fd = socket(AddressFamily::Unix, SockType::Stream, SockFlag::empty(), None)
        .expect("socket failed");
    let addr = UnixAddr::new(server_path).expect("UnixAddr failed");
    connect(fd.as_raw_fd(), &addr).expect("connect failed");

    // Send slave_fd via SCM_RIGHTS
    {
        let iov = [IoSlice::new(b"RUN")];
        let cmsg = [ControlMessage::ScmRights(&[slave_fd.as_raw_fd()])];
        sendmsg::<()>(fd.as_raw_fd(), &iov, &cmsg, MsgFlags::empty(), None).expect("sendmsg failed");
    }

    // Receive response from server
    let mut buf = [0u8; 1024];
    let mut iov = [IoSliceMut::new(&mut buf)];
    let msg = recvmsg::<()>(fd.as_raw_fd(), &mut iov, None, MsgFlags::empty()).expect("recvmsg failed");
    let response = &buf[..msg.bytes];
    if response != b"OK" {
        eprintln!("Server did not respond with OK");
        exit(1);
    }

    // Close slave fd locally
    close(slave_fd.as_raw_fd()).expect("close slave failed");

    // Set nonblocking on stdin, stdout, and master
    set_nonblocking(stdin_borrowed.as_raw_fd());
    set_nonblocking(stdout_borrowed.as_raw_fd());
    set_nonblocking(master_fd.as_raw_fd());

    let mut buf_in = [0u8; 1024];
    let mut buf_out = [0u8; 1024];

    let master_borrowed = master_fd.as_fd();

    loop {
        let mut fds = FdSet::new();
        fds.insert(stdin_borrowed);
        fds.insert(master_borrowed);

        let nfds = std::cmp::max(stdin_borrowed.as_raw_fd(), master_borrowed.as_raw_fd()) + 1;
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

        if fds.contains(stdin_borrowed) {
            match read(stdin_borrowed.as_raw_fd(), &mut buf_in) {
                Ok(n) if n > 0 => {
                    let _ = write(master_borrowed, &buf_in[..n]);
                }
                _ => {}
            }
        }

        if fds.contains(master_borrowed) {
            match read(master_borrowed.as_raw_fd(), &mut buf_out) {
                Ok(n) if n > 0 => {
                    let _ = write(stdout_borrowed, &buf_out[..n]);
                }
                _ => {}
            }
        }
    }
}

fn set_nonblocking(fd: i32) {
    let flags = fcntl(fd, FcntlArg::F_GETFL).unwrap();
    let new_flags = OFlag::from_bits_truncate(flags) | OFlag::O_NONBLOCK;
    fcntl(fd, FcntlArg::F_SETFL(new_flags)).unwrap();
}
