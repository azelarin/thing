use std::{fmt, io, os::fd::BorrowedFd, time::Instant};

use thing_ui::SecretBytes;

use super::{
    model::{Action, Attempt, Completion, FAILURE_COOLDOWN, Phase},
    process::{OsProcess, ProcessContext, ProcessControl},
    protocol::{AttemptId, Receive},
};

const MAX_PACKETS_PER_POLL: usize = 16;

#[derive(Clone, Eq, PartialEq)]
pub enum Event {
    Prompt {
        id: AttemptId,
        generation: u64,
        text: String,
    },
    Info {
        id: AttemptId,
        text: String,
    },
    Error {
        id: AttemptId,
        text: String,
    },
    Authenticated {
        id: AttemptId,
    },
    Rejected {
        id: AttemptId,
        code: u32,
    },
    Unavailable {
        id: AttemptId,
    },
}

impl fmt::Debug for Event {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Prompt { id, generation, .. } => formatter
                .debug_struct("Prompt")
                .field("id", id)
                .field("generation", generation)
                .field("text", &"<redacted>")
                .finish(),
            Self::Info { id, .. } => formatter
                .debug_struct("Info")
                .field("id", id)
                .field("text", &"<redacted>")
                .finish(),
            Self::Error { id, .. } => formatter
                .debug_struct("Error")
                .field("id", id)
                .field("text", &"<redacted>")
                .finish(),
            Self::Authenticated { id } => formatter
                .debug_struct("Authenticated")
                .field("id", id)
                .finish(),
            Self::Rejected { id, code } => formatter
                .debug_struct("Rejected")
                .field("id", id)
                .field("code", code)
                .finish(),
            Self::Unavailable { id } => formatter
                .debug_struct("Unavailable")
                .field("id", id)
                .finish(),
        }
    }
}

pub struct StartError {
    error: io::Error,
    secret: Option<SecretBytes>,
}

impl StartError {
    pub fn error(&self) -> &io::Error {
        &self.error
    }

    pub fn into_secret(self) -> Option<SecretBytes> {
        self.secret
    }
}

pub struct AuthenticationBroker {
    context: ProcessContext,
    inner: Broker<OsProcess>,
}

impl AuthenticationBroker {
    pub fn new(context: ProcessContext, lock_generation: u64) -> Self {
        Self {
            context,
            inner: Broker::new(lock_generation),
        }
    }

    pub fn can_start(&self, now: Instant) -> bool {
        self.inner.can_start(now)
    }

    pub fn start(&mut self, secret: SecretBytes, now: Instant) -> Result<AttemptId, StartError> {
        if !self.inner.can_start(now) {
            return Err(StartError {
                error: io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "authentication is already running or cooling down",
                ),
                secret: Some(secret),
            });
        }
        let process = match self.context.spawn() {
            Ok(process) => process,
            Err(error) => {
                return Err(StartError {
                    error,
                    secret: Some(secret),
                });
            }
        };
        self.inner
            .start(process, secret, now)
            .map_err(|error| StartError {
                error,
                secret: None,
            })
    }

    pub fn respond(
        &mut self,
        id: AttemptId,
        prompt: u64,
        secret: SecretBytes,
        now: Instant,
    ) -> Result<(), SecretBytes> {
        self.inner.respond(id, prompt, secret, now)
    }

    pub fn poll(&mut self, now: Instant) -> Vec<Event> {
        self.inner.poll(now)
    }

    pub fn cancel(&mut self, now: Instant) {
        self.inner.cancel(now);
    }

    pub fn set_lock_generation(&mut self, generation: u64, now: Instant) {
        self.inner.set_lock_generation(generation, now);
    }

    pub fn pidfd(&self) -> Option<BorrowedFd<'_>> {
        self.inner
            .active
            .as_ref()
            .and_then(|active| active.process.pidfd())
    }
}

struct Broker<P> {
    active: Option<Active<P>>,
    lock_generation: u64,
    next_attempt: u64,
    current: Option<AttemptId>,
    cooldown_until: Option<Instant>,
}

struct Active<P> {
    process: P,
    attempt: Attempt,
    termination: Termination,
    abnormal: bool,
    exit: Option<bool>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum Termination {
    #[default]
    None,
    TermSent,
    KillSent,
}

impl<P: ProcessControl> Broker<P> {
    fn new(lock_generation: u64) -> Self {
        Self {
            active: None,
            lock_generation,
            next_attempt: 1,
            current: None,
            cooldown_until: None,
        }
    }

    fn can_start(&self, now: Instant) -> bool {
        self.active.is_none() && self.cooldown_until.is_none_or(|until| now >= until)
    }

    fn start(&mut self, process: P, secret: SecretBytes, now: Instant) -> io::Result<AttemptId> {
        if !self.can_start(now) {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "authentication is already running or cooling down",
            ));
        }
        self.cooldown_until = None;
        let id = AttemptId {
            lock: self.lock_generation,
            attempt: self.next_attempt,
        };
        self.next_attempt = self.next_attempt.wrapping_add(1).max(1);
        let attempt = Attempt::new(id, secret, now);
        self.current = Some(id);
        self.active = Some(Active {
            process,
            attempt,
            termination: Termination::None,
            abnormal: false,
            exit: None,
        });
        let begin = self
            .active
            .as_ref()
            .expect("the attempt was just installed")
            .attempt
            .begin_packet();
        if let Err(error) = self
            .active
            .as_mut()
            .expect("the attempt was just installed")
            .process
            .send(&begin)
        {
            self.start_cancellation(now, true);
            return Err(error);
        }
        Ok(id)
    }

    fn respond(
        &mut self,
        id: AttemptId,
        prompt: u64,
        secret: SecretBytes,
        now: Instant,
    ) -> Result<(), SecretBytes> {
        let Some(active) = self.active.as_mut() else {
            return Err(secret);
        };
        if self.current != Some(id) || active.attempt.id() != id {
            return Err(secret);
        }
        let action = active.attempt.respond(prompt, secret, now)?;
        if apply_action(active, action, &mut Vec::new()).is_err() {
            self.start_cancellation(now, true);
        }
        Ok(())
    }

    fn poll(&mut self, now: Instant) -> Vec<Event> {
        let mut events = Vec::new();
        let (mut cancel, mut transport_drained) =
            self.active.as_mut().map_or((false, false), |active| {
                drain_packets(active, now, &mut events)
            });
        if self
            .active
            .as_mut()
            .is_some_and(|active| active.attempt.timeout(now).is_some())
        {
            cancel = true;
        }
        if cancel {
            self.start_cancellation(now, true);
            transport_drained = true;
        }

        if let Some(active) = self.active.as_mut()
            && active.attempt.phase() == Phase::Cancelling
            && active.termination == Termination::TermSent
            && now >= active.attempt.deadline()
        {
            let _ = active.process.kill_group();
            active.termination = Termination::KillSent;
        }

        let mut newly_exited = false;
        let mut wait_failed = false;
        if let Some(active) = self.active.as_mut()
            && active.exit.is_none()
        {
            match active.process.try_wait() {
                Ok(Some(clean)) => {
                    active.exit = Some(clean);
                    newly_exited = true;
                }
                Ok(None) => {}
                Err(_) => wait_failed = true,
            }
        }
        if wait_failed {
            self.start_cancellation(now, true);
            transport_drained = true;
        }

        // Exit readiness can race ahead of the final queued result packet.
        // Drain once more after observing exit, but keep each pass bounded.
        if newly_exited && let Some(active) = self.active.as_mut() {
            let (second_cancel, second_drained) = drain_packets(active, now, &mut events);
            transport_drained = second_drained;
            if second_cancel {
                self.start_cancellation(now, true);
                transport_drained = true;
            }
        }

        let finished = self
            .active
            .as_ref()
            .is_some_and(|active| active.exit.is_some() && transport_drained);
        if finished {
            let active = self.active.take().expect("the exited attempt exists");
            let id = active.attempt.id();
            let completion = active.attempt.complete(
                active.exit.expect("the exit status was checked"),
                self.current,
            );
            self.current = None;
            match completion {
                Completion::Authenticated => events.push(Event::Authenticated { id }),
                Completion::Rejected(code) => events.push(Event::Rejected { id, code }),
                Completion::Cancelled if active.abnormal => {
                    self.cooldown_until = Some(now + FAILURE_COOLDOWN);
                    events.push(Event::Unavailable { id });
                }
                Completion::Cancelled => {}
                Completion::Unavailable => {
                    self.cooldown_until = Some(now + FAILURE_COOLDOWN);
                    events.push(Event::Unavailable { id });
                }
            }
        }
        events
    }

    fn cancel(&mut self, now: Instant) {
        self.start_cancellation(now, false);
    }

    fn set_lock_generation(&mut self, generation: u64, now: Instant) {
        if self.lock_generation != generation {
            self.lock_generation = generation;
            self.start_cancellation(now, false);
        }
    }

    fn start_cancellation(&mut self, now: Instant, abnormal: bool) {
        self.current = None;
        let Some(active) = self.active.as_mut() else {
            return;
        };
        active.abnormal |= abnormal;
        let action = active.attempt.cancel(now);
        if action.is_none() && active.termination != Termination::None {
            return;
        }
        active.process.close_socket();
        let _ = active.process.terminate_group();
        active.termination = Termination::TermSent;
    }
}

fn drain_packets<P: ProcessControl>(
    active: &mut Active<P>,
    now: Instant,
    events: &mut Vec<Event>,
) -> (bool, bool) {
    for _ in 0..MAX_PACKETS_PER_POLL {
        match active.process.receive() {
            Ok(Receive::Packet(packet)) => match active.attempt.on_packet(packet, now) {
                Ok(actions) => {
                    for action in actions {
                        if apply_action(active, action, events).is_err() {
                            return (true, false);
                        }
                    }
                }
                Err(_) => return (true, false),
            },
            Ok(Receive::Empty | Receive::Closed) => return (false, true),
            Err(_) => return (true, false),
        }
    }
    (false, false)
}

fn apply_action<P: ProcessControl>(
    active: &mut Active<P>,
    action: Action,
    events: &mut Vec<Event>,
) -> io::Result<()> {
    let id = active.attempt.id();
    match action {
        Action::Send(packet) => active.process.send(&packet),
        Action::Prompt { generation, text } => {
            events.push(Event::Prompt {
                id,
                generation,
                text,
            });
            Ok(())
        }
        Action::Info(text) => {
            events.push(Event::Info { id, text });
            Ok(())
        }
        Action::Error(text) => {
            events.push(Event::Error { id, text });
            Ok(())
        }
        Action::Terminate => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, collections::VecDeque, rc::Rc, time::Duration};

    use super::*;
    use crate::authentication::protocol::Packet;

    #[derive(Default)]
    struct FakeState {
        incoming: VecDeque<Packet>,
        sent: Vec<&'static str>,
        closed: bool,
        term: usize,
        kill: usize,
        exit: Option<bool>,
    }

    struct FakeProcess(Rc<RefCell<FakeState>>);

    impl ProcessControl for FakeProcess {
        fn send(&mut self, packet: &Packet) -> io::Result<()> {
            let name = match packet {
                Packet::Begin { .. } => "begin",
                Packet::Secret { .. } => "secret",
                _ => "unexpected",
            };
            self.0.borrow_mut().sent.push(name);
            Ok(())
        }

        fn receive(&mut self) -> io::Result<Receive> {
            Ok(self
                .0
                .borrow_mut()
                .incoming
                .pop_front()
                .map_or(Receive::Empty, Receive::Packet))
        }

        fn close_socket(&mut self) {
            self.0.borrow_mut().closed = true;
        }

        fn terminate_group(&mut self) -> io::Result<()> {
            self.0.borrow_mut().term += 1;
            Ok(())
        }

        fn kill_group(&mut self) -> io::Result<()> {
            self.0.borrow_mut().kill += 1;
            Ok(())
        }

        fn try_wait(&mut self) -> io::Result<Option<bool>> {
            Ok(self.0.borrow_mut().exit.take())
        }
    }

    fn secret() -> SecretBytes {
        SecretBytes::copy_from(b"private").unwrap()
    }

    fn broker(now: Instant) -> (Broker<FakeProcess>, Rc<RefCell<FakeState>>, AttemptId) {
        let state = Rc::new(RefCell::new(FakeState::default()));
        let process = FakeProcess(state.clone());
        let mut broker = Broker::new(9);
        let id = broker.start(process, secret(), now).unwrap();
        (broker, state, id)
    }

    #[test]
    fn secret_is_not_sent_until_the_execed_helper_is_ready() {
        let now = Instant::now();
        let (mut broker, state, id) = broker(now);
        assert_eq!(state.borrow().sent, ["begin"]);
        state.borrow_mut().incoming.push_back(Packet::Ready { id });

        assert!(broker.poll(now).is_empty());
        assert_eq!(state.borrow().sent, ["begin", "secret"]);
    }

    #[test]
    fn queued_success_is_drained_before_clean_exit_is_decided() {
        let now = Instant::now();
        let (mut broker, state, id) = broker(now);
        state
            .borrow_mut()
            .incoming
            .extend([Packet::Ready { id }, Packet::Success { id }]);
        state.borrow_mut().exit = Some(true);

        assert_eq!(broker.poll(now), [Event::Authenticated { id }]);
        assert!(broker.active.is_none());
    }

    #[test]
    fn an_exited_helper_is_retained_until_all_bounded_packet_batches_are_drained() {
        let now = Instant::now();
        let (mut broker, state, id) = broker(now);
        let mut incoming = VecDeque::from([Packet::Ready { id }]);
        incoming.extend((0..40).map(|_| Packet::Info {
            id,
            prompt: 0,
            text: String::new(),
        }));
        incoming.push_back(Packet::Success { id });
        state.borrow_mut().incoming = incoming;
        state.borrow_mut().exit = Some(true);

        assert!(
            broker
                .poll(now)
                .iter()
                .all(|event| !matches!(event, Event::Authenticated { .. }))
        );
        assert!(broker.active.is_some());
        assert!(broker.poll(now).contains(&Event::Authenticated { id }));
        assert!(broker.active.is_none());
    }

    #[test]
    fn stale_success_during_cancellation_cannot_authenticate() {
        let now = Instant::now();
        let (mut broker, state, id) = broker(now);
        state.borrow_mut().incoming.push_back(Packet::Ready { id });
        broker.poll(now);
        broker.cancel(now);
        state
            .borrow_mut()
            .incoming
            .push_back(Packet::Success { id });
        state.borrow_mut().exit = Some(true);

        assert!(broker.poll(now).is_empty());
        assert!(state.borrow().closed);
        assert_eq!(state.borrow().term, 1);
    }

    #[test]
    fn timeout_kills_once_and_never_replaces_an_unreaped_child() {
        let now = Instant::now();
        let (mut broker, state, _) = broker(now);
        broker.poll(now + super::super::model::STARTUP_TIMEOUT);
        assert_eq!(state.borrow().term, 1);
        broker.poll(now + super::super::model::STARTUP_TIMEOUT + Duration::from_millis(100));
        broker.poll(now + super::super::model::STARTUP_TIMEOUT + Duration::from_secs(2));
        assert_eq!(state.borrow().kill, 1);
        assert!(!broker.can_start(now + Duration::from_secs(10)));

        state.borrow_mut().exit = Some(false);
        assert!(matches!(
            broker.poll(now + Duration::from_secs(10)).as_slice(),
            [Event::Unavailable { .. }]
        ));
        assert!(!broker.can_start(now + Duration::from_secs(10)));
        assert!(broker.can_start(now + Duration::from_secs(11)));
    }

    #[test]
    fn changing_lock_generation_cancels_the_old_attempt() {
        let now = Instant::now();
        let (mut broker, state, _) = broker(now);
        broker.set_lock_generation(10, now);

        assert!(state.borrow().closed);
        assert_eq!(state.borrow().term, 1);
        assert!(broker.current.is_none());
    }

    #[test]
    fn debug_output_redacts_untrusted_pam_text() {
        let id = AttemptId {
            lock: 9,
            attempt: 1,
        };
        for event in [
            Event::Prompt {
                id,
                generation: 2,
                text: "private prompt".into(),
            },
            Event::Info {
                id,
                text: "private info".into(),
            },
            Event::Error {
                id,
                text: "private error".into(),
            },
        ] {
            let output = format!("{event:?}");
            assert!(output.contains("<redacted>"));
            assert!(!output.contains("private"));
        }
    }
}
