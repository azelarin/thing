use std::{
    cell::RefCell,
    rc::{Rc, Weak},
    time::{Duration, Instant},
};

use mlua::{Error, Function, Lua, Result, Table, UserData, UserDataMethods};

const MAX_TIMERS: usize = 128;
const MAX_CONTROLS: usize = 256;
const MAX_INTERVAL_MS: f64 = 31_536_000_000.0;

/// one pending timer callback, invalidated by cancellation or rescheduling.
#[derive(Clone, Debug)]
pub struct TimerToken {
    id: usize,
    deadline: Instant,
    schedule: Rc<()>,
}

impl TimerToken {
    pub fn deadline(&self) -> Instant {
        self.deadline
    }
}

struct Schedule {
    deadline: Instant,
    identity: Rc<()>,
}

impl Schedule {
    fn new(deadline: Instant) -> Self {
        Self {
            deadline,
            identity: Rc::new(()),
        }
    }
}

struct Timer {
    interval: Duration,
    callback: Function,
    repeating: bool,
    autostart: bool,
    schedule: Option<Schedule>,
}

#[derive(Clone, Copy)]
enum Control {
    Start,
    Stop,
    Restart,
}

#[derive(Default)]
pub(crate) struct Timers {
    entries: Vec<Timer>,
    controls: Vec<(usize, Control)>,
    frozen: bool,
    activated: bool,
    allowed: bool,
}

impl Timers {
    pub(crate) fn install(owner: &Rc<RefCell<Self>>, lua: &Lua, ui: &Table) -> Result<()> {
        let owner = Rc::downgrade(owner);
        ui.set(
            "timer",
            lua.create_function(move |_, options: Table| {
                let owner = owner
                    .upgrade()
                    .ok_or_else(|| Error::runtime("timer runtime has ended"))?;
                let mut timers = owner.borrow_mut();
                if timers.frozen {
                    return Err(Error::runtime(
                        "declare timers while loading the configuration",
                    ));
                }
                if timers.entries.len() >= MAX_TIMERS {
                    return Err(Error::runtime("configuration exceeds 128 timers"));
                }
                for field in options.clone().pairs::<String, mlua::Value>() {
                    let (field, _) = field?;
                    if !matches!(
                        field.as_str(),
                        "interval" | "on_timeout" | "repeating" | "autostart"
                    ) {
                        return Err(Error::runtime(format!("unknown timer property {field:?}")));
                    }
                }
                let interval = match options.raw_get::<mlua::Value>("interval")? {
                    mlua::Value::Integer(value) => value as f64,
                    mlua::Value::Number(value) => value,
                    _ => return Err(Error::runtime("timer interval must be a number")),
                };
                if !interval.is_finite()
                    || interval.fract() != 0.0
                    || !(1.0..=MAX_INTERVAL_MS).contains(&interval)
                {
                    return Err(Error::runtime(
                        "timer interval must be whole milliseconds from 1 through 31,536,000,000",
                    ));
                }
                let callback = options.raw_get("on_timeout")?;
                let repeating = boolean_option(&options, "repeating")?;
                let autostart = boolean_option(&options, "autostart")?;
                let id = timers.entries.len();
                timers.entries.push(Timer {
                    interval: Duration::from_millis(interval as u64),
                    callback,
                    repeating,
                    autostart,
                    schedule: None,
                });
                Ok(Handle {
                    id,
                    owner: Rc::downgrade(&owner),
                })
            })?,
        )
    }

    pub(crate) fn is_activated(&self) -> bool {
        self.activated
    }

    pub(crate) fn freeze(&mut self) {
        self.frozen = true;
    }

    pub(crate) fn activate(&mut self, now: Instant) {
        if self.activated {
            return;
        }
        self.activated = true;
        for timer in &mut self.entries {
            if timer.autostart {
                timer.schedule = Some(Schedule::new(now + timer.interval));
            }
        }
    }

    pub(crate) fn next_deadline(&self) -> Option<Instant> {
        self.entries
            .iter()
            .filter_map(|timer| timer.schedule.as_ref().map(|schedule| schedule.deadline))
            .min()
    }

    pub(crate) fn due(&self, now: Instant) -> Vec<TimerToken> {
        let mut due = self
            .entries
            .iter()
            .enumerate()
            .filter_map(|(id, timer)| {
                let schedule = timer.schedule.as_ref()?;
                (schedule.deadline <= now).then(|| TimerToken {
                    id,
                    deadline: schedule.deadline,
                    schedule: schedule.identity.clone(),
                })
            })
            .collect::<Vec<_>>();
        due.sort_by_key(|token| (token.deadline, token.id));
        due
    }

    pub(crate) fn claim(&mut self, token: &TimerToken, now: Instant) -> Option<Function> {
        let timer = self.entries.get_mut(token.id)?;
        let schedule = timer.schedule.as_ref()?;
        if schedule.deadline > now || !Rc::ptr_eq(&schedule.identity, &token.schedule) {
            return None;
        }
        timer.schedule = None;
        Some(timer.callback.clone())
    }

    pub(crate) fn handle(owner: &Rc<RefCell<Self>>, token: &TimerToken) -> Handle {
        Handle {
            id: token.id,
            owner: Rc::downgrade(owner),
        }
    }

    pub(crate) fn repeat_after_rejection(&mut self, token: &TimerToken, now: Instant) {
        let timer = &mut self.entries[token.id];
        if timer.repeating {
            timer.schedule = Some(Schedule::new(now + timer.interval));
        }
    }

    pub(crate) fn repeat(&mut self, token: &TimerToken) {
        if self.entries[token.id].repeating && !self.controls.iter().any(|(id, _)| *id == token.id)
        {
            // automatic repetition does not consume the callback's control budget.
            self.controls.push((token.id, Control::Start));
        }
    }

    pub(crate) fn checkpoint(&self) -> usize {
        self.controls.len()
    }
    pub(crate) fn truncate(&mut self, checkpoint: usize) {
        self.controls.truncate(checkpoint);
    }
    pub(crate) fn allow_controls(&mut self, allowed: bool) {
        self.allowed = allowed;
    }

    pub(crate) fn commit(&mut self, now: Instant) {
        for (id, control) in self.controls.drain(..) {
            let timer = &mut self.entries[id];
            match control {
                Control::Start if timer.schedule.is_some() => {}
                Control::Start | Control::Restart => {
                    timer.schedule = Some(Schedule::new(now + timer.interval))
                }
                Control::Stop => timer.schedule = None,
            }
        }
    }

    fn queue(&mut self, id: usize, control: Control) -> Result<()> {
        if !self.allowed {
            return Err(Error::runtime(
                "timer controls are available only inside event handlers",
            ));
        }
        if !self.activated {
            return Err(Error::runtime("timer runtime has not been activated"));
        }
        if self.controls.len() >= MAX_CONTROLS {
            return Err(Error::runtime("too many timer controls in one event"));
        }
        self.controls.push((id, control));
        Ok(())
    }
}

pub(crate) struct Handle {
    id: usize,
    owner: Weak<RefCell<Timers>>,
}

impl Handle {
    fn queue(&self, control: Control) -> Result<()> {
        self.owner
            .upgrade()
            .ok_or_else(|| Error::runtime("timer runtime has ended"))?
            .borrow_mut()
            .queue(self.id, control)
    }
}

impl UserData for Handle {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("start", |_, this, ()| this.queue(Control::Start));
        methods.add_method("stop", |_, this, ()| this.queue(Control::Stop));
        methods.add_method("restart", |_, this, ()| this.queue(Control::Restart));
    }
}

fn boolean_option(options: &Table, field: &str) -> Result<bool> {
    match options.raw_get::<mlua::Value>(field)? {
        mlua::Value::Nil => Ok(false),
        mlua::Value::Boolean(value) => Ok(value),
        _ => Err(Error::runtime(format!("timer {field} must be a boolean"))),
    }
}
