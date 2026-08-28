use std::{
    io::{self, Read, Write},
    os::{fd::AsRawFd, unix::net::UnixStream},
    path::Path,
    time::{Duration, Instant},
};

use socket2::{Domain, SockAddr, Socket, Type};

const MAX_LINE: usize = 1024 * 1024;
pub(super) const REQUEST_TIMEOUT: Duration = Duration::from_secs(3);

/// nonblocking json transport with a deadline for each request and reply.
pub(super) struct Connection {
    stream: UnixStream,
    input: Vec<u8>,
    output: Vec<u8>,
    written: usize,
    pub deadline: Option<Instant>,
}

impl Connection {
    pub fn connect(path: &Path) -> io::Result<Self> {
        let socket = Socket::new(Domain::UNIX, Type::STREAM, None)?;
        socket.set_nonblocking(true)?;
        if let Err(error) = socket.connect(&SockAddr::unix(path)?)
            && error.raw_os_error() != Some(libc::EINPROGRESS)
        {
            return Err(error);
        }
        Ok(Self {
            stream: std::os::fd::OwnedFd::from(socket).into(),
            input: Vec::new(),
            output: Vec::new(),
            written: 0,
            deadline: None,
        })
    }

    pub fn send(&mut self, request: &serde_json::Value) -> io::Result<()> {
        self.output = serde_json::to_vec(request)?;
        self.output.push(b'\n');
        self.written = 0;
        self.deadline = Some(Instant::now() + REQUEST_TIMEOUT);
        Ok(())
    }

    pub fn descriptor(&self) -> libc::pollfd {
        libc::pollfd {
            fd: self.stream.as_raw_fd(),
            events: libc::POLLIN
                | if self.written < self.output.len() {
                    libc::POLLOUT
                } else {
                    0
                },
            revents: 0,
        }
    }

    pub fn ready(&mut self, events: i16) -> io::Result<Vec<Vec<u8>>> {
        if events & (libc::POLLERR | libc::POLLNVAL) != 0 {
            return Err(self
                .stream
                .take_error()?
                .unwrap_or_else(|| io::Error::other("niri socket error")));
        }
        if events & libc::POLLOUT != 0 {
            match self.stream.write(&self.output[self.written..]) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(count) => self.written += count,
                Err(e)
                    if e.kind() == io::ErrorKind::WouldBlock
                        || e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        let mut lines = Vec::new();
        if events & (libc::POLLIN | libc::POLLHUP) != 0 {
            // bound each dispatch so a busy event stream cannot starve commands.
            let mut buffer = [0_u8; 65536];
            match self.stream.read(&mut buffer) {
                Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
                Ok(count) => {
                    self.input.extend_from_slice(&buffer[..count]);
                    let mut start = 0;
                    for (index, byte) in self.input.iter().enumerate() {
                        if *byte == b'\n' {
                            if index - start > MAX_LINE {
                                return Err(io::Error::other("niri event exceeds 1 MiB"));
                            }
                            lines.push(self.input[start..index].to_vec());
                            start = index + 1;
                        }
                    }
                    self.input.drain(..start);
                    if self.input.len() > MAX_LINE {
                        return Err(io::Error::other("niri event exceeds 1 MiB"));
                    }
                }
                Err(e)
                    if e.kind() == io::ErrorKind::WouldBlock
                        || e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(lines)
    }
}

pub(super) fn poll(descriptors: &mut [libc::pollfd], timeout: i32) -> io::Result<()> {
    // SAFETY: the slice describes initialized, writable pollfd entries and stays
    // alive for the call. poll borrows descriptors; it does not take ownership.
    let result = unsafe {
        libc::poll(
            descriptors.as_mut_ptr(),
            descriptors.len() as libc::nfds_t,
            timeout,
        )
    };
    if result < 0 {
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
    Ok(())
}
