use std::time::{Duration, Instant};

use thing_ui::SecretBytes;

use super::protocol::{AttemptId, Packet};

pub const STARTUP_TIMEOUT: Duration = Duration::from_secs(5);
pub const STEP_TIMEOUT: Duration = Duration::from_secs(60);
pub const TOTAL_TIMEOUT: Duration = Duration::from_secs(5 * 60);
pub const TERMINATE_GRACE: Duration = Duration::from_millis(100);
pub const FAILURE_COOLDOWN: Duration = Duration::from_secs(1);
const MAX_PROMPTS: u64 = 8;
const MAX_DISPLAY_BYTES: usize = 8 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Phase {
    Starting,
    Authenticating,
    WaitingForSecret,
    Finishing,
    Cancelling,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ResultFrame {
    Success,
    Failure(u32),
}

pub enum Action {
    Send(Packet),
    Prompt { generation: u64, text: String },
    Info(String),
    Error(String),
    Terminate,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Completion {
    Authenticated,
    Rejected(u32),
    Cancelled,
    Unavailable,
}

pub struct Attempt {
    id: AttemptId,
    phase: Phase,
    initial_secret: Option<SecretBytes>,
    prompt: u64,
    result: Option<ResultFrame>,
    display_bytes: usize,
    deadline: Instant,
    total_deadline: Instant,
}

impl Attempt {
    pub fn new(id: AttemptId, secret: SecretBytes, now: Instant) -> Self {
        Self {
            id,
            phase: Phase::Starting,
            initial_secret: Some(secret),
            prompt: 0,
            result: None,
            display_bytes: 0,
            deadline: now + STARTUP_TIMEOUT,
            total_deadline: now + TOTAL_TIMEOUT,
        }
    }

    pub fn id(&self) -> AttemptId {
        self.id
    }

    pub fn phase(&self) -> Phase {
        self.phase
    }

    pub fn deadline(&self) -> Instant {
        self.deadline
    }

    pub fn begin_packet(&self) -> Packet {
        Packet::Begin { id: self.id }
    }

    pub fn on_packet(&mut self, packet: Packet, now: Instant) -> Result<Vec<Action>, &'static str> {
        if packet.id() != self.id {
            return Err("authentication packet belongs to another attempt");
        }
        if self.phase == Phase::Cancelling {
            return Ok(Vec::new());
        }
        match packet {
            Packet::Ready { .. } if self.phase == Phase::Starting => {
                let secret = self
                    .initial_secret
                    .take()
                    .ok_or("authentication helper sent duplicate ready")?;
                self.phase = Phase::Authenticating;
                self.reset_step(now);
                Ok(vec![Action::Send(Packet::Secret {
                    id: self.id,
                    prompt: 0,
                    secret,
                })])
            }
            Packet::Prompt { prompt, text, .. }
                if self.phase == Phase::Authenticating
                    && self.result.is_none()
                    && prompt == self.prompt + 1
                    && prompt < MAX_PROMPTS =>
            {
                self.add_display(&text)?;
                self.prompt = prompt;
                self.phase = Phase::WaitingForSecret;
                self.reset_step(now);
                Ok(vec![Action::Prompt {
                    generation: prompt,
                    text,
                }])
            }
            Packet::Info { prompt, text, .. }
                if matches!(self.phase, Phase::Authenticating | Phase::WaitingForSecret)
                    && self.result.is_none()
                    && prompt == self.prompt =>
            {
                self.add_display(&text)?;
                Ok(vec![Action::Info(text)])
            }
            Packet::Error { prompt, text, .. }
                if matches!(self.phase, Phase::Authenticating | Phase::WaitingForSecret)
                    && self.result.is_none()
                    && prompt == self.prompt =>
            {
                self.add_display(&text)?;
                Ok(vec![Action::Error(text)])
            }
            Packet::Success { .. } if self.phase == Phase::Authenticating => {
                if self.result.replace(ResultFrame::Success).is_some() {
                    return Err("authentication helper sent more than one result");
                }
                self.phase = Phase::Finishing;
                self.reset_step(now);
                Ok(Vec::new())
            }
            Packet::Failure { code, .. } if self.phase == Phase::Authenticating => {
                if self.result.replace(ResultFrame::Failure(code)).is_some() {
                    return Err("authentication helper sent more than one result");
                }
                self.phase = Phase::Finishing;
                self.reset_step(now);
                Ok(Vec::new())
            }
            Packet::Begin { .. } | Packet::Secret { .. } => {
                Err("authentication helper sent a parent-only packet")
            }
            _ => Err("authentication packet arrived out of order"),
        }
    }

    pub fn respond(
        &mut self,
        prompt: u64,
        secret: SecretBytes,
        now: Instant,
    ) -> Result<Action, SecretBytes> {
        if self.phase != Phase::WaitingForSecret || prompt != self.prompt {
            return Err(secret);
        }
        self.phase = Phase::Authenticating;
        self.reset_step(now);
        Ok(Action::Send(Packet::Secret {
            id: self.id,
            prompt,
            secret,
        }))
    }

    pub fn cancel(&mut self, now: Instant) -> Option<Action> {
        if self.phase == Phase::Cancelling {
            return None;
        }
        self.initial_secret = None;
        self.phase = Phase::Cancelling;
        self.deadline = now + TERMINATE_GRACE;
        Some(Action::Terminate)
    }

    pub fn timeout(&mut self, now: Instant) -> Option<Action> {
        (now >= self.deadline).then(|| self.cancel(now)).flatten()
    }

    pub fn complete(self, clean_exit: bool, current: Option<AttemptId>) -> Completion {
        if self.phase == Phase::Cancelling {
            return Completion::Cancelled;
        }
        if !clean_exit || current != Some(self.id) {
            return Completion::Unavailable;
        }
        match self.result {
            Some(ResultFrame::Success) => Completion::Authenticated,
            Some(ResultFrame::Failure(code)) => Completion::Rejected(code),
            None => Completion::Unavailable,
        }
    }

    fn add_display(&mut self, text: &str) -> Result<(), &'static str> {
        let Some(total) = self.display_bytes.checked_add(text.len()) else {
            return Err("authentication messages exceed their limit");
        };
        if total > MAX_DISPLAY_BYTES {
            return Err("authentication messages exceed their limit");
        }
        self.display_bytes = total;
        Ok(())
    }

    fn reset_step(&mut self, now: Instant) {
        self.deadline = (now + STEP_TIMEOUT).min(self.total_deadline);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(attempt: u64) -> AttemptId {
        AttemptId { lock: 4, attempt }
    }

    fn secret(value: &[u8]) -> SecretBytes {
        SecretBytes::copy_from(value).unwrap()
    }

    #[test]
    fn waits_for_ready_before_releasing_the_initial_secret() {
        let now = Instant::now();
        let mut attempt = Attempt::new(id(1), secret(b"private"), now);
        assert_eq!(attempt.phase(), Phase::Starting);

        let actions = attempt.on_packet(Packet::Ready { id: id(1) }, now).unwrap();
        let [Action::Send(Packet::Secret { prompt, secret, .. })] = actions.as_slice() else {
            panic!("ready did not release exactly one secret");
        };
        assert_eq!(*prompt, 0);
        secret.expose(|bytes| assert_eq!(bytes, b"private"));
        assert_eq!(attempt.phase(), Phase::Authenticating);
    }

    #[test]
    fn success_requires_one_result_and_a_clean_reap() {
        let now = Instant::now();
        let mut attempt = Attempt::new(id(1), secret(b"private"), now);
        attempt.on_packet(Packet::Ready { id: id(1) }, now).unwrap();
        attempt
            .on_packet(Packet::Success { id: id(1) }, now)
            .unwrap();
        assert_eq!(attempt.phase(), Phase::Finishing);
        assert_eq!(
            attempt.complete(true, Some(id(1))),
            Completion::Authenticated
        );

        let mut missing = Attempt::new(id(2), secret(b"private"), now);
        missing.on_packet(Packet::Ready { id: id(2) }, now).unwrap();
        assert_eq!(missing.complete(true, Some(id(2))), Completion::Unavailable);

        let mut crashed = Attempt::new(id(3), secret(b"private"), now);
        crashed.on_packet(Packet::Ready { id: id(3) }, now).unwrap();
        crashed
            .on_packet(Packet::Success { id: id(3) }, now)
            .unwrap();
        assert_eq!(
            crashed.complete(false, Some(id(3))),
            Completion::Unavailable
        );
    }

    #[test]
    fn cancellation_invalidates_late_success() {
        let now = Instant::now();
        let mut attempt = Attempt::new(id(1), secret(b"private"), now);
        attempt.on_packet(Packet::Ready { id: id(1) }, now).unwrap();
        assert!(matches!(attempt.cancel(now), Some(Action::Terminate)));
        assert!(
            attempt
                .on_packet(Packet::Success { id: id(1) }, now)
                .unwrap()
                .is_empty()
        );
        assert_eq!(attempt.complete(true, Some(id(1))), Completion::Cancelled);
    }

    #[test]
    fn prompt_generation_and_limits_are_strict() {
        let now = Instant::now();
        let mut attempt = Attempt::new(id(1), secret(b"first"), now);
        attempt.on_packet(Packet::Ready { id: id(1) }, now).unwrap();
        let actions = attempt
            .on_packet(
                Packet::Prompt {
                    id: id(1),
                    prompt: 1,
                    text: "token".into(),
                },
                now,
            )
            .unwrap();
        assert!(matches!(
            actions.as_slice(),
            [Action::Prompt { generation: 1, .. }]
        ));
        assert!(attempt.respond(2, secret(b"stale"), now).is_err());
        assert!(matches!(
            attempt.respond(1, secret(b"second"), now),
            Ok(Action::Send(Packet::Secret { prompt: 1, .. }))
        ));

        assert!(
            attempt
                .on_packet(
                    Packet::Info {
                        id: id(1),
                        prompt: 0,
                        text: "stale".into(),
                    },
                    now,
                )
                .is_err()
        );

        assert!(
            attempt
                .on_packet(
                    Packet::Prompt {
                        id: id(1),
                        prompt: MAX_PROMPTS,
                        text: "too many".into(),
                    },
                    now,
                )
                .is_err()
        );
    }

    #[test]
    fn prompt_limit_counts_the_initial_secret() {
        let now = Instant::now();
        let mut attempt = Attempt::new(id(1), secret(b"first"), now);
        attempt.on_packet(Packet::Ready { id: id(1) }, now).unwrap();
        for prompt in 1..MAX_PROMPTS {
            attempt
                .on_packet(
                    Packet::Prompt {
                        id: id(1),
                        prompt,
                        text: "next factor".into(),
                    },
                    now,
                )
                .unwrap();
            assert!(attempt.respond(prompt, secret(b"response"), now).is_ok());
        }
        assert!(
            attempt
                .on_packet(
                    Packet::Prompt {
                        id: id(1),
                        prompt: MAX_PROMPTS,
                        text: "one too many".into(),
                    },
                    now,
                )
                .is_err()
        );
    }

    #[test]
    fn timeout_enters_bounded_cancellation_once() {
        let now = Instant::now();
        let mut attempt = Attempt::new(id(1), secret(b"private"), now);
        assert!(attempt.timeout(now).is_none());
        assert!(matches!(
            attempt.timeout(now + STARTUP_TIMEOUT),
            Some(Action::Terminate)
        ));
        assert!(
            attempt
                .timeout(now + STARTUP_TIMEOUT + TERMINATE_GRACE)
                .is_none()
        );
        assert_eq!(attempt.phase(), Phase::Cancelling);
    }
}
