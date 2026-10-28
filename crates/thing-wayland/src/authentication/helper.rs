use std::{
    ffi::{CStr, CString},
    fs, io,
    mem::MaybeUninit,
    os::{
        fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd},
        unix::fs::MetadataExt,
    },
    path::Path,
};

use pam_client::{Context, ConversationHandler, ErrorCode, Flag};
use thing_ui::SecretBytes;

use super::protocol::{self, AttemptId, Packet, Receive};

pub const HELPER_SENTINEL: &str = "__thing-authentication-helper";
const PAM_SERVICE: &str = "thing";
const PAM_POLICY: &str = "/etc/pam.d/thing";
const MAX_PROMPTS: u64 = 8;
const MAX_DISPLAY_BYTES: usize = 8 * 1024;

/// disable ordinary core dumps before a process receives authentication data.
pub fn harden_secret_process() -> io::Result<()> {
    let limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: limit points to an initialized rlimit value for this call.
    if unsafe { libc::setrlimit(libc::RLIMIT_CORE, &limit) } == -1 {
        return Err(io::Error::last_os_error());
    }
    // Linux is the supported Wayland host for this helper.
    // SAFETY: PR_SET_DUMPABLE accepts the integer value zero and no pointer arguments.
    if unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// require the fixed packager-owned policy used by the authentication helper.
pub fn preflight_policy() -> io::Result<()> {
    let path = Path::new(PAM_POLICY);
    let metadata = fs::symlink_metadata(path)?;
    if !secure_policy_metadata(
        metadata.file_type().is_file(),
        metadata.uid(),
        metadata.mode(),
    ) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "the thing PAM policy must be a root-owned regular file that is not group- or world-writable",
        ));
    }
    Ok(())
}

fn secure_policy_metadata(is_file: bool, uid: u32, mode: u32) -> bool {
    is_file && uid == 0 && mode & 0o022 == 0
}

/// run the hidden authentication helper on its inherited stdin socket.
///
/// the first supported unlock scope calls only `pam_authenticate`. it does not
/// run account, session, password-change, or credential-management stages.
/// this entry point must never be used as a test backend; tests exercise the
/// private protocol with fake peers instead.
pub fn run() -> io::Result<()> {
    harden_secret_process()?;
    preflight_policy()?;
    // SAFETY: the hidden entry point runs once and takes ownership of its open stdin.
    let socket = unsafe { OwnedFd::from_raw_fd(libc::STDIN_FILENO) };
    validate_socket(socket.as_fd())?;
    let id = receive_begin(socket.as_fd())?;
    let username = real_username()?;
    let conversation = PamConversation::new(socket.as_fd(), id);
    let mut context = Context::new(PAM_SERVICE, Some(&username), conversation)
        .map_err(|_| io::Error::other("cannot initialize PAM"))?;
    protocol::send(socket.as_fd(), &Packet::Ready { id })?;
    let initial = receive_secret(socket.as_fd(), id, 0)?;
    context.conversation_mut().initial = Some(initial);

    let result = context.authenticate(Flag::DISALLOW_NULL_AUTHTOK);
    let conversation_failed = context.conversation().failed;
    let code = result
        .as_ref()
        .err()
        .map_or(ErrorCode::CONV_ERR as u32, |error| error.code() as u32);
    let success = result.is_ok() && !conversation_failed;
    // pam_end and every conversation-owned buffer complete before the result.
    drop(context);
    let packet = if success {
        Packet::Success { id }
    } else {
        Packet::Failure { id, code }
    };
    protocol::send(socket.as_fd(), &packet)
}

struct PamConversation<'a> {
    socket: BorrowedFd<'a>,
    id: AttemptId,
    initial: Option<SecretBytes>,
    prompts: u64,
    display_bytes: usize,
    failed: bool,
}

impl<'a> PamConversation<'a> {
    fn new(socket: BorrowedFd<'a>, id: AttemptId) -> Self {
        Self {
            socket,
            id,
            initial: None,
            prompts: 0,
            display_bytes: 0,
            failed: false,
        }
    }

    fn send_text(&mut self, text: &CStr, error: bool) {
        let text = protocol::sanitize_text(text.to_bytes());
        let Some(total) = self.display_bytes.checked_add(text.len()) else {
            self.failed = true;
            return;
        };
        if total > MAX_DISPLAY_BYTES {
            self.failed = true;
            return;
        }
        self.display_bytes = total;
        let packet = if error {
            Packet::Error {
                id: self.id,
                prompt: self.prompts.saturating_sub(1),
                text,
            }
        } else {
            Packet::Info {
                id: self.id,
                prompt: self.prompts.saturating_sub(1),
                text,
            }
        };
        if protocol::send(self.socket, &packet).is_err() {
            self.failed = true;
        }
    }
}

impl ConversationHandler for PamConversation<'_> {
    fn prompt_echo_on(&mut self, _prompt: &CStr) -> Result<CString, ErrorCode> {
        self.failed = true;
        Err(ErrorCode::CONV_ERR)
    }

    fn prompt_echo_off(&mut self, prompt: &CStr) -> Result<CString, ErrorCode> {
        self.prompts = self.prompts.checked_add(1).ok_or(ErrorCode::CONV_ERR)?;
        if self.prompts > MAX_PROMPTS {
            self.failed = true;
            return Err(ErrorCode::CONV_ERR);
        }
        let generation = self.prompts - 1;
        let secret = if generation == 0 {
            self.initial.take().ok_or(ErrorCode::CONV_ERR)?
        } else {
            let text = protocol::sanitize_text(prompt.to_bytes());
            let packet = Packet::Prompt {
                id: self.id,
                prompt: generation,
                text,
            };
            protocol::send(self.socket, &packet).map_err(|_| ErrorCode::CONV_ERR)?;
            receive_secret(self.socket, self.id, generation).map_err(|_| ErrorCode::CONV_ERR)?
        };
        let response = secret
            .expose(|bytes| CString::new(bytes))
            .map_err(|_| ErrorCode::CONV_ERR)?;
        // the protected source is cleared here; PAM and pam-client may retain
        // their own copies until this helper exits.
        drop(secret);
        Ok(response)
    }

    fn text_info(&mut self, message: &CStr) {
        self.send_text(message, false);
    }

    fn error_msg(&mut self, message: &CStr) {
        self.send_text(message, true);
    }

    fn radio_prompt(&mut self, _prompt: &CStr) -> Result<bool, ErrorCode> {
        self.failed = true;
        Err(ErrorCode::CONV_ERR)
    }

    fn binary_prompt(&mut self, _kind: u8, _data: &[u8]) -> Result<(u8, Vec<u8>), ErrorCode> {
        self.failed = true;
        Err(ErrorCode::CONV_ERR)
    }
}

fn receive_begin(socket: BorrowedFd<'_>) -> io::Result<AttemptId> {
    loop {
        match protocol::receive_blocking(socket)? {
            Receive::Packet(Packet::Begin { id }) => return Ok(id),
            Receive::Empty => continue,
            Receive::Closed => return Err(io::Error::from(io::ErrorKind::UnexpectedEof)),
            Receive::Packet(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "authentication helper expected begin",
                ));
            }
        }
    }
}

fn receive_secret(socket: BorrowedFd<'_>, id: AttemptId, prompt: u64) -> io::Result<SecretBytes> {
    loop {
        match protocol::receive_blocking(socket)? {
            Receive::Packet(Packet::Secret {
                id: received,
                prompt: received_prompt,
                secret,
            }) if received == id && received_prompt == prompt => return Ok(secret),
            Receive::Empty => continue,
            Receive::Closed => return Err(io::Error::from(io::ErrorKind::UnexpectedEof)),
            Receive::Packet(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "authentication helper received an out-of-order response",
                ));
            }
        }
    }
}

fn validate_socket(socket: BorrowedFd<'_>) -> io::Result<()> {
    let mut kind = 0;
    let mut length = std::mem::size_of_val(&kind) as libc::socklen_t;
    // SAFETY: kind and length point to initialized writable storage for this call.
    if unsafe {
        libc::getsockopt(
            socket.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_TYPE,
            (&mut kind as *mut libc::c_int).cast(),
            &mut length,
        )
    } == -1
    {
        return Err(io::Error::last_os_error());
    }
    if kind != libc::SOCK_SEQPACKET {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "authentication helper stdin is not a sequence-packet socket",
        ));
    }
    Ok(())
}

fn real_username() -> io::Result<String> {
    // SAFETY: getuid has no preconditions.
    let uid = unsafe { libc::getuid() };
    let mut record = MaybeUninit::<libc::passwd>::uninit();
    let mut result = std::ptr::null_mut();
    let mut buffer = [0_u8; 16 * 1024];
    // SAFETY: all output pointers refer to live writable storage for this call.
    let status = unsafe {
        libc::getpwuid_r(
            uid,
            record.as_mut_ptr(),
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            &mut result,
        )
    };
    if status != 0 {
        return Err(io::Error::from_raw_os_error(status));
    }
    if result.is_null() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "the real uid has no account",
        ));
    }
    // SAFETY: a successful getpwuid_r with a non-null result initialized record.
    let record = unsafe { record.assume_init() };
    if record.pw_name.is_null() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "the real uid has no account name",
        ));
    }
    // SAFETY: getpwuid_r returned a non-null, NUL-terminated name in its live buffer.
    let username = unsafe { CStr::from_ptr(record.pw_name) }
        .to_str()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "account name is not UTF-8"))?;
    if username.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "account name is empty",
        ));
    }
    Ok(username.to_owned())
}

#[cfg(test)]
mod tests {
    use std::{env, os::fd::AsFd, process::Command};

    use socket2::{Domain, Socket, Type};

    use super::*;

    const HARDENING_PROBE: &str = "THING_AUTH_HARDENING_PROBE";

    #[test]
    fn helper_hardening_disables_process_dumps() {
        let status = Command::new(env::current_exe().unwrap())
            .args([
                "--exact",
                "authentication::helper::tests::helper_hardening_probe",
            ])
            .env(HARDENING_PROBE, "1")
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[test]
    fn helper_hardening_probe() {
        if env::var_os(HARDENING_PROBE).is_none() {
            return;
        }
        harden_secret_process().unwrap();
        let mut limit = MaybeUninit::<libc::rlimit>::uninit();
        // SAFETY: limit points to writable storage and RLIMIT_CORE is valid.
        let status = unsafe { libc::getrlimit(libc::RLIMIT_CORE, limit.as_mut_ptr()) };
        assert_eq!(status, 0);
        // SAFETY: successful getrlimit initialized limit.
        let limit = unsafe { limit.assume_init() };
        assert_eq!(limit.rlim_cur, 0);
        assert_eq!(limit.rlim_max, 0);
        // SAFETY: PR_GET_DUMPABLE takes no additional arguments.
        assert_eq!(unsafe { libc::prctl(libc::PR_GET_DUMPABLE) }, 0);
    }

    #[test]
    fn unexpected_echo_on_radio_and_binary_prompts_fail_closed() {
        let (left, _right) = Socket::pair(Domain::UNIX, Type::SEQPACKET, None).unwrap();
        let mut conversation = PamConversation::new(
            left.as_fd(),
            AttemptId {
                lock: 1,
                attempt: 1,
            },
        );
        let prompt = c"unexpected";

        assert_eq!(
            conversation.prompt_echo_on(prompt),
            Err(ErrorCode::CONV_ERR)
        );
        assert_eq!(conversation.radio_prompt(prompt), Err(ErrorCode::CONV_ERR));
        assert_eq!(
            conversation.binary_prompt(1, b"data"),
            Err(ErrorCode::CONV_ERR)
        );
        assert!(conversation.failed);
    }

    #[test]
    fn service_policy_name_and_location_are_fixed() {
        assert_eq!(PAM_SERVICE, "thing");
        assert_eq!(PAM_POLICY, "/etc/pam.d/thing");
        assert!(secure_policy_metadata(true, 0, 0o100644));
        assert!(!secure_policy_metadata(false, 0, 0o100644));
        assert!(!secure_policy_metadata(true, 1000, 0o100644));
        assert!(!secure_policy_metadata(true, 0, 0o100664));
        assert!(!secure_policy_metadata(true, 0, 0o100646));
    }
}
