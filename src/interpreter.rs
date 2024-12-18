use crate::sendfd::PtyMaster;
use anyhow::Result;
use nix::libc;
use nix::unistd::Pid;
use std::fmt;
use std::io::Write;
use std::net::Shutdown;
use std::os::fd::{AsRawFd, RawFd};
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::str::FromStr;

// Pair of child_id and Pid
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ChildId {
    pub id: u32,
    pub pid: Pid,
}

impl fmt::Display for ChildId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "({},{})", self.id, self.pid.as_raw())
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct ParseChildIdError(String);

impl std::fmt::Display for ParseChildIdError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid ChildId: {:?}", self.0)
    }
}

impl std::error::Error for ParseChildIdError {}

impl FromStr for ChildId {
    type Err = ParseChildIdError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (x, y) = s
            .strip_prefix('(')
            .and_then(|s| s.strip_suffix(')'))
            .and_then(|s| s.split_once(','))
            .ok_or_else(|| ParseChildIdError(s.to_string()))?;

        let id = x
            .parse::<u32>()
            .map_err(|_| ParseChildIdError(s.to_string()))?;
        let pid = y
            .parse::<libc::pid_t>()
            .map_err(|_| ParseChildIdError(s.to_string()))?;

        Ok(ChildId {
            id,
            pid: Pid::from_raw(pid),
        })
    }
}

impl ChildId {
    pub fn new(id: u32, pid: Pid) -> Self {
        ChildId { id, pid }
    }
}

pub struct Interpreter {
    id: ChildId,
    control_fd: UnixStream,
    pty_master: Option<PtyMaster>,
    supervised: bool,
}

impl Interpreter {
    pub fn new(id: ChildId, control_fd: UnixStream, pty_master: PtyMaster) -> Self {
        Interpreter {
            id,
            control_fd,
            pty_master: Some(pty_master),
            supervised: true,
        }
    }

    pub fn id(&self) -> &ChildId {
        &self.id
    }

    pub fn take_pty_master(&mut self) -> Option<PtyMaster> {
        self.pty_master.take()
    }

    pub fn unsupervise(&mut self) -> Result<()> {
        let stop_supervision = "supervised = False";
        if let Err(err) = self
            .control_fd
            .write_all(format!("{:?}\n", stop_supervision).as_ref())
        {
            // This can happen if interpreter has died
            eprintln!("interpreter unsupervise err writing to control_fd: {}", err);
        }
        self.supervised = false;
        Ok(())
    }

    pub fn run_instructions(&mut self, instructions: &str) -> Result<()> {
        assert!(!self.supervised, "still supervised");
        if let Err(err) = self
            .control_fd
            .write_all(format!("{:?}\n", instructions).as_ref())
        {
            eprintln!("interpreter run_instructions send failed: {}", err);
        }
        let _ = self.control_fd.shutdown(Shutdown::Both); // ignore error
        Ok(())
    }

    pub unsafe fn from_raw(msg: &[u8], fds: &[RawFd]) -> Result<Self> {
        Ok(Interpreter {
            id: ChildId::from_str(&String::from_utf8_lossy(msg))?,
            control_fd: UnixStream::from_raw_fd(fds[0]),
            pty_master: if fds.len() > 1 {
                Some(OwnedFd::from_raw_fd(fds[1]).into())
            } else {
                None
            },
            supervised: false,
        })
    }

    pub fn to_raw(&self) -> (Vec<u8>, Vec<RawFd>) {
        assert!(!self.supervised, "cannot send supervised interpreter");
        let msg = self.id.to_string().into_bytes();
        let mut fds = vec![self.control_fd.as_raw_fd()];
        if let Some(fd) = &self.pty_master {
            fds.push(fd.as_raw_fd());
        }
        (msg, fds)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_child_id_display_parse() {
        let id = 2;
        let pid = Pid::from_raw(81581);
        let child_id = ChildId::new(id, pid);
        assert_eq!(child_id.id, id);
        assert_eq!(child_id.pid, pid);
        let displayed = format!("{}", child_id);
        assert_eq!(displayed, "(2,81581)");
        let child_id2 = ChildId::from_str(&displayed).unwrap();
        assert_eq!(child_id2, child_id);
    }

    #[test]
    fn test_child_id_from_str_missing_parentheses() {
        let inputs = ["123,456", "(123,456", "123,456)", ""];

        for input in inputs {
            assert!(
                ChildId::from_str(input).is_err(),
                "Should fail for input: {}",
                input
            );
        }
    }

    #[test]
    fn test_child_id_from_str_missing_comma() {
        let inputs = ["(123 456)", "(123)", "(,456)", "(123,)"];

        for input in inputs {
            assert!(
                ChildId::from_str(input).is_err(),
                "Should fail for input: {}",
                input
            );
        }
    }

    #[test]
    fn test_child_id_from_str_invalid_numbers() {
        let inputs = ["(abc,456)", "(123,xyz)", "(abc,xyz)"];

        for input in inputs {
            assert!(
                ChildId::from_str(input).is_err(),
                "Should fail for input: {}",
                input
            );
        }
    }
}
