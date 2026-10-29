use std::{
    io,
    os::fd::{AsRawFd, BorrowedFd},
    sync::atomic::{Ordering, compiler_fence},
};

use thing_ui::{MAX_SECRET_BYTES, SecretBytes};

const MAGIC: [u8; 4] = *b"THAU";
const VERSION: u8 = 1;
const HEADER_BYTES: usize = 36;
const MAX_TEXT_BYTES: usize = 1024;
const MAX_PACKET_BYTES: usize = HEADER_BYTES + MAX_SECRET_BYTES;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AttemptId {
    pub lock: u64,
    pub attempt: u64,
}

pub enum Packet {
    Begin {
        id: AttemptId,
    },
    Ready {
        id: AttemptId,
    },
    Secret {
        id: AttemptId,
        prompt: u64,
        secret: SecretBytes,
    },
    Prompt {
        id: AttemptId,
        prompt: u64,
        text: String,
    },
    Info {
        id: AttemptId,
        prompt: u64,
        text: String,
    },
    Error {
        id: AttemptId,
        prompt: u64,
        text: String,
    },
    Success {
        id: AttemptId,
    },
    Failure {
        id: AttemptId,
        code: u32,
    },
}

impl Packet {
    pub fn id(&self) -> AttemptId {
        match self {
            Self::Begin { id }
            | Self::Ready { id }
            | Self::Secret { id, .. }
            | Self::Prompt { id, .. }
            | Self::Info { id, .. }
            | Self::Error { id, .. }
            | Self::Success { id }
            | Self::Failure { id, .. } => *id,
        }
    }
}

pub enum Receive {
    Empty,
    Closed,
    Packet(Packet),
}

pub fn send(fd: BorrowedFd<'_>, packet: &Packet) -> io::Result<()> {
    let encoded = encode(packet)?;
    // SOCK_SEQPACKET preserves this frame as one atomic record.
    // SAFETY: fd is live and encoded provides a readable buffer of encoded.len bytes.
    let sent = unsafe {
        libc::send(
            fd.as_raw_fd(),
            encoded.bytes.as_ptr().cast(),
            encoded.len,
            libc::MSG_NOSIGNAL | libc::MSG_DONTWAIT,
        )
    };
    if sent < 0 {
        return Err(io::Error::last_os_error());
    }
    if sent as usize != encoded.len {
        return Err(io::Error::new(
            io::ErrorKind::WriteZero,
            "authentication packet was not sent atomically",
        ));
    }
    Ok(())
}

pub fn receive(fd: BorrowedFd<'_>) -> io::Result<Receive> {
    receive_with_flags(fd, libc::MSG_DONTWAIT | libc::MSG_CMSG_CLOEXEC)
}

pub fn receive_blocking(fd: BorrowedFd<'_>) -> io::Result<Receive> {
    receive_with_flags(fd, libc::MSG_CMSG_CLOEXEC)
}

fn receive_with_flags(fd: BorrowedFd<'_>, flags: libc::c_int) -> io::Result<Receive> {
    let mut bytes = SensitiveBytes::new();
    let mut iovec = libc::iovec {
        iov_base: bytes.bytes.as_mut_ptr().cast(),
        iov_len: bytes.bytes.len(),
    };
    let mut message = libc::msghdr {
        msg_name: std::ptr::null_mut(),
        msg_namelen: 0,
        msg_iov: &mut iovec,
        msg_iovlen: 1,
        msg_control: std::ptr::null_mut(),
        msg_controllen: 0,
        msg_flags: 0,
    };
    // SAFETY: every pointer in message refers to the live fixed buffer and iovec above.
    let received = unsafe { libc::recvmsg(fd.as_raw_fd(), &mut message, flags) };
    if received < 0 {
        let error = io::Error::last_os_error();
        if matches!(
            error.kind(),
            io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
        ) {
            return Ok(Receive::Empty);
        }
        return Err(error);
    }
    if received == 0 {
        return Ok(Receive::Closed);
    }
    if message.msg_flags & (libc::MSG_TRUNC | libc::MSG_CTRUNC) != 0 {
        return Err(invalid("authentication packet was truncated"));
    }
    let packet = decode(&bytes.bytes[..received as usize])?;
    Ok(Receive::Packet(packet))
}

fn encode(packet: &Packet) -> io::Result<EncodedPacket> {
    let (kind, id, prompt, payload): (u8, AttemptId, u64, &[u8]) = match packet {
        Packet::Begin { id } => (0, *id, 0, &[]),
        Packet::Ready { id } => (1, *id, 0, &[]),
        Packet::Secret { id, prompt, secret } => {
            return secret.expose(|bytes| encode_parts(2, *id, *prompt, bytes));
        }
        Packet::Prompt { id, prompt, text } => (3, *id, *prompt, text.as_bytes()),
        Packet::Info { id, prompt, text } => (4, *id, *prompt, text.as_bytes()),
        Packet::Error { id, prompt, text } => (5, *id, *prompt, text.as_bytes()),
        Packet::Success { id } => (6, *id, 0, &[]),
        Packet::Failure { id, code } => {
            return encode_parts(7, *id, 0, &code.to_be_bytes());
        }
    };
    encode_parts(kind, id, prompt, payload)
}

fn encode_parts(kind: u8, id: AttemptId, prompt: u64, payload: &[u8]) -> io::Result<EncodedPacket> {
    let limit = if kind == 2 {
        MAX_SECRET_BYTES
    } else {
        MAX_TEXT_BYTES
    };
    if payload.len() > limit || payload.len() > u32::MAX as usize {
        return Err(invalid("authentication packet payload is too large"));
    }
    if matches!(kind, 2..=5) && payload.contains(&0) {
        return Err(invalid("authentication packet contains NUL"));
    }
    let mut encoded = EncodedPacket::new();
    encoded.bytes[..4].copy_from_slice(&MAGIC);
    encoded.bytes[4] = VERSION;
    encoded.bytes[5] = kind;
    encoded.bytes[6..8].fill(0);
    encoded.bytes[8..12].copy_from_slice(&(payload.len() as u32).to_be_bytes());
    encoded.bytes[12..20].copy_from_slice(&id.lock.to_be_bytes());
    encoded.bytes[20..28].copy_from_slice(&id.attempt.to_be_bytes());
    encoded.bytes[28..36].copy_from_slice(&prompt.to_be_bytes());
    encoded.bytes[HEADER_BYTES..HEADER_BYTES + payload.len()].copy_from_slice(payload);
    encoded.len = HEADER_BYTES + payload.len();
    Ok(encoded)
}

fn decode(bytes: &[u8]) -> io::Result<Packet> {
    if bytes.len() < HEADER_BYTES || bytes[..4] != MAGIC || bytes[4] != VERSION {
        return Err(invalid("invalid authentication packet header"));
    }
    if bytes[6..8] != [0, 0] {
        return Err(invalid("authentication packet has unsupported flags"));
    }
    let payload_len = u32::from_be_bytes(bytes[8..12].try_into().unwrap()) as usize;
    if payload_len > MAX_SECRET_BYTES || bytes.len() != HEADER_BYTES + payload_len {
        return Err(invalid("invalid authentication packet length"));
    }
    let id = AttemptId {
        lock: u64::from_be_bytes(bytes[12..20].try_into().unwrap()),
        attempt: u64::from_be_bytes(bytes[20..28].try_into().unwrap()),
    };
    let prompt = u64::from_be_bytes(bytes[28..36].try_into().unwrap());
    let payload = &bytes[HEADER_BYTES..];
    if matches!(bytes[5], 2..=5) && payload.contains(&0) {
        return Err(invalid("authentication packet contains NUL"));
    }
    match bytes[5] {
        0 if prompt == 0 && payload.is_empty() => Ok(Packet::Begin { id }),
        1 if prompt == 0 && payload.is_empty() => Ok(Packet::Ready { id }),
        2 => SecretBytes::copy_from(payload)
            .map(|secret| Packet::Secret { id, prompt, secret })
            .ok_or_else(|| invalid("invalid authentication secret")),
        3 => text(payload).map(|text| Packet::Prompt { id, prompt, text }),
        4 => text(payload).map(|text| Packet::Info { id, prompt, text }),
        5 => text(payload).map(|text| Packet::Error { id, prompt, text }),
        6 if prompt == 0 && payload.is_empty() => Ok(Packet::Success { id }),
        7 if prompt == 0 && payload.len() == 4 => Ok(Packet::Failure {
            id,
            code: u32::from_be_bytes(payload.try_into().unwrap()),
        }),
        _ => Err(invalid("unknown or malformed authentication packet")),
    }
}

pub fn sanitize_text(value: &[u8]) -> String {
    let mut sanitized = String::with_capacity(value.len().min(MAX_TEXT_BYTES));
    for character in String::from_utf8_lossy(value).chars() {
        let character = if character == '\0'
            || character.is_control()
            || matches!(character, '\u{2028}' | '\u{2029}')
        {
            ' '
        } else {
            character
        };
        if sanitized.len() + character.len_utf8() > MAX_TEXT_BYTES {
            break;
        }
        sanitized.push(character);
    }
    sanitized
}

fn text(payload: &[u8]) -> io::Result<String> {
    if payload.len() > MAX_TEXT_BYTES {
        return Err(invalid("authentication text is too large"));
    }
    std::str::from_utf8(payload)
        .map(str::to_owned)
        .map_err(|_| invalid("authentication text is not UTF-8"))
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

struct EncodedPacket {
    bytes: [u8; MAX_PACKET_BYTES],
    len: usize,
}

impl EncodedPacket {
    fn new() -> Self {
        Self {
            bytes: [0; MAX_PACKET_BYTES],
            len: 0,
        }
    }
}

impl Drop for EncodedPacket {
    fn drop(&mut self) {
        secure_zero(&mut self.bytes);
    }
}

struct SensitiveBytes {
    bytes: [u8; MAX_PACKET_BYTES],
}

impl SensitiveBytes {
    fn new() -> Self {
        Self {
            bytes: [0; MAX_PACKET_BYTES],
        }
    }
}

impl Drop for SensitiveBytes {
    fn drop(&mut self) {
        secure_zero(&mut self.bytes);
    }
}

fn secure_zero(bytes: &mut [u8]) {
    for byte in bytes {
        // volatile writes prevent the compiler from eliding secret packet cleanup.
        // SAFETY: byte is a valid unique reference for this volatile write.
        unsafe { std::ptr::write_volatile(byte, 0) };
    }
    compiler_fence(Ordering::SeqCst);
}

#[cfg(test)]
mod tests {
    use std::os::fd::AsFd;

    use socket2::{Domain, Socket, Type};

    use super::*;

    fn pair() -> (Socket, Socket) {
        Socket::pair(Domain::UNIX, Type::SEQPACKET, None).unwrap()
    }

    fn receive_error_kind(socket: &Socket) -> io::ErrorKind {
        let Err(error) = receive(socket.as_fd()) else {
            panic!("expected an invalid packet");
        };
        error.kind()
    }

    #[test]
    fn secret_packet_round_trips_without_debuggable_payload_types() {
        let (left, right) = pair();
        let id = AttemptId {
            lock: 7,
            attempt: 11,
        };
        let packet = Packet::Secret {
            id,
            prompt: 3,
            secret: SecretBytes::copy_from(b"private").unwrap(),
        };
        send(left.as_fd(), &packet).unwrap();

        let Receive::Packet(Packet::Secret {
            id: received,
            prompt,
            secret,
        }) = receive(right.as_fd()).unwrap()
        else {
            panic!("expected one secret packet");
        };
        assert_eq!(received, id);
        assert_eq!(prompt, 3);
        secret.expose(|bytes| assert_eq!(bytes, b"private"));
    }

    #[test]
    fn rejects_truncated_oversized_and_malformed_packets() {
        let (left, right) = pair();
        let oversized = vec![b'x'; MAX_PACKET_BYTES + 1];
        left.send(&oversized).unwrap();
        assert_eq!(receive_error_kind(&right), io::ErrorKind::InvalidData);

        left.send(b"short").unwrap();
        assert_eq!(receive_error_kind(&right), io::ErrorKind::InvalidData);

        let id = AttemptId {
            lock: 1,
            attempt: 2,
        };
        let mut encoded = encode(&Packet::Ready { id }).unwrap();
        encoded.bytes[5] = 200;
        left.send(&encoded.bytes[..encoded.len]).unwrap();
        assert_eq!(receive_error_kind(&right), io::ErrorKind::InvalidData);
    }

    #[test]
    fn closed_and_empty_transport_states_are_distinct() {
        let (left, right) = pair();
        assert!(matches!(receive(right.as_fd()).unwrap(), Receive::Empty));
        // another test can fork before exec closes its inherited descriptors.
        let inherited_peer = left.try_clone().unwrap();
        drop(left);
        assert!(matches!(receive(right.as_fd()).unwrap(), Receive::Empty));
        inherited_peer.shutdown(std::net::Shutdown::Write).unwrap();
        assert!(matches!(receive(right.as_fd()).unwrap(), Receive::Closed));
    }

    #[test]
    fn sanitizes_untrusted_pam_display_text() {
        assert_eq!(sanitize_text(b"one\ntwo\0three"), "one two three");
        assert_eq!(sanitize_text(&[b'a', 0xff, b'b']), "a�b");
        assert!(sanitize_text(&[0xff; MAX_TEXT_BYTES]).len() <= MAX_TEXT_BYTES);
    }

    #[test]
    fn failure_codes_are_binary_payloads() {
        let (left, right) = pair();
        let id = AttemptId {
            lock: 3,
            attempt: 5,
        };
        send(left.as_fd(), &Packet::Failure { id, code: 7 }).unwrap();

        let Receive::Packet(Packet::Failure { id: received, code }) =
            receive(right.as_fd()).unwrap()
        else {
            panic!("expected one failure packet");
        };
        assert_eq!(received, id);
        assert_eq!(code, 7);
    }
}
