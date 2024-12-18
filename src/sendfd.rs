use nix::libc;
use nix::sys::socket::{recvmsg, ControlMessageOwned, MsgFlags};
use nix::sys::socket::{sendmsg, ControlMessage};
use std::io;
use std::os::unix::io::{AsFd, AsRawFd, BorrowedFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
use std::os::unix::net;

/// An extension trait that enables sending associated file descriptors along with the data.
pub trait SendWithFd {
    /// Send the bytes and the file descriptors.
    fn send_with_fd(&self, bytes: &[u8], fds: &[RawFd]) -> nix::Result<usize>;
}

/// An extension trait that enables receiving associated file descriptors along with the data.
pub trait RecvWithFd {
    /// Receive the bytes and the file descriptors.
    ///
    /// The bytes and the file descriptors are received into the corresponding buffers.
    fn recv_with_fd(&self, bytes: &mut [u8], fds: &mut [RawFd]) -> nix::Result<(usize, usize)>;
}

fn send_with_fd(socket: RawFd, bs: &[u8], fds: &[RawFd]) -> nix::Result<usize> {
    let iov = [io::IoSlice::new(bs)];
    let cmsg = [ControlMessage::ScmRights(fds)];
    sendmsg::<()>(socket, &iov, &cmsg, MsgFlags::empty(), None)
}

fn recv_with_fd(socket: RawFd, bs: &mut [u8], fds: &mut [RawFd]) -> nix::Result<(usize, usize)> {
    let mut iov = [io::IoSliceMut::new(bs)];

    // construct cmsgspace manually based on size of fds, not supported by nix::cmsg_space!
    let fds_len = std::mem::size_of_val(fds);
    let cmsg_buffer_len = unsafe { libc::CMSG_SPACE(fds_len as u32) as usize };
    let mut cmsgspace = Vec::<u8>::with_capacity(cmsg_buffer_len);

    let msg = recvmsg::<()>(socket, &mut iov, Some(&mut cmsgspace), MsgFlags::empty())?;
    let mut descriptor_count = 0;
    for cmsg in msg.cmsgs()? {
        if let ControlMessageOwned::ScmRights(cmsg_fds) = cmsg {
            for fd in cmsg_fds {
                fds[descriptor_count] = fd;
                descriptor_count += 1;
            }
        }
    }
    Ok((msg.bytes, descriptor_count))
}

impl SendWithFd for net::UnixStream {
    /// Send the bytes and the file descriptors as a stream.
    ///
    /// Neither is guaranteed to be received by the other end in a single chunk and
    /// may arrive entirely independently.
    fn send_with_fd(&self, bytes: &[u8], fds: &[RawFd]) -> nix::Result<usize> {
        send_with_fd(self.as_raw_fd(), bytes, fds)
    }
}

impl RecvWithFd for net::UnixStream {
    /// Receive the bytes and the file descriptors from the stream.
    ///
    /// It is not guaranteed that the received information will form a single coherent packet of
    /// data. In other words, it is not required that this receives the bytes and file descriptors
    /// that were sent with a single `send_with_fd` call by somebody else.
    fn recv_with_fd(&self, bytes: &mut [u8], fds: &mut [RawFd]) -> nix::Result<(usize, usize)> {
        recv_with_fd(self.as_raw_fd(), bytes, fds)
    }
}

/// Representation of the Master device in a master/slave pty pair. Copied from nix to add
/// From<OwnedFd> trait impl.
#[derive(Debug)]
pub struct PtyMaster(OwnedFd);

impl From<OwnedFd> for PtyMaster {
    fn from(fd: OwnedFd) -> Self {
        PtyMaster(fd)
    }
}

impl From<nix::pty::PtyMaster> for PtyMaster {
    fn from(fd: nix::pty::PtyMaster) -> Self {
        unsafe { OwnedFd::from_raw_fd(fd.into_raw_fd()) }.into()
    }
}

impl AsRawFd for PtyMaster {
    fn as_raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}

impl AsFd for PtyMaster {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

impl IntoRawFd for PtyMaster {
    fn into_raw_fd(self) -> RawFd {
        let fd = self.0;
        fd.into_raw_fd()
    }
}

impl io::Read for PtyMaster {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        nix::unistd::read(self.0.as_raw_fd(), buf).map_err(io::Error::from)
    }
}

impl io::Write for PtyMaster {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        nix::unistd::write(&self.0, buf).map_err(io::Error::from)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl io::Read for &PtyMaster {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        nix::unistd::read(self.0.as_raw_fd(), buf).map_err(io::Error::from)
    }
}

impl io::Write for &PtyMaster {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        nix::unistd::write(&self.0, buf).map_err(io::Error::from)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{RecvWithFd, SendWithFd};
    use std::os::unix::io::{AsRawFd, FromRawFd};
    use std::os::unix::net;

    #[test]
    fn stream_works() {
        let (l, r) = net::UnixStream::pair().expect("create UnixStream pair");
        let sent_bytes = b"hello world!";
        let sent_fds = [l.as_raw_fd(), r.as_raw_fd()];
        assert_eq!(
            l.send_with_fd(&sent_bytes[..], &sent_fds[..])
                .expect("send should be successful"),
            sent_bytes.len()
        );
        let mut recv_bytes = [0; 128];
        let mut recv_fds = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        assert_eq!(
            r.recv_with_fd(&mut recv_bytes, &mut recv_fds)
                .expect("recv should be successful"),
            (sent_bytes.len(), sent_fds.len())
        );
        assert_eq!(recv_bytes[..sent_bytes.len()], sent_bytes[..]);
        for (&sent, &recvd) in sent_fds.iter().zip(&recv_fds[..]) {
            // Modify the sent resource and check if the received resource has been modified the
            // same way.
            let expected_value = Some(std::time::Duration::from_secs(42));
            unsafe {
                let s = net::UnixStream::from_raw_fd(sent);
                s.set_read_timeout(expected_value)
                    .expect("set read timeout");
                std::mem::forget(s);
                assert_eq!(
                    net::UnixStream::from_raw_fd(recvd)
                        .read_timeout()
                        .expect("get read timeout"),
                    expected_value
                );
            }
        }
    }

    #[test]
    fn sending_junk_fails() {
        let (l, _) = net::UnixStream::pair().expect("create UnixStream pair");
        let sent_bytes = b"hello world!";
        if l.send_with_fd(&sent_bytes[..], &[i32::MAX][..]).is_ok() {
            panic!("expected an error when sending a junk file descriptor");
        }
        if l.send_with_fd(&sent_bytes[..], &[0xffi32][..]).is_ok() {
            panic!("expected an error when sending a junk file descriptor");
        }
    }
}
