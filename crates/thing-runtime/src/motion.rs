use std::{
    cell::RefCell,
    collections::BTreeSet,
    rc::{Rc, Weak},
    time::Instant,
};

use mlua::{Error, Lua, Result, Table, UserData, UserDataMethods, Value};

use crate::{
    state::{Scalar, Tracker},
    transition::{Resolved, Spec, State},
};

const MAX_MOTIONS: usize = 128;
const MAX_CONTROLS: usize = 256;

struct Entry {
    state: State,
    sources: [usize; 3],
    pending_sample: bool,
}

#[derive(Clone, Copy)]
enum Control {
    Target(f32),
    Jump(f32),
    Stop,
}

pub(crate) struct Motions {
    entries: Vec<Entry>,
    controls: Vec<(usize, Control)>,
    tracker: Rc<RefCell<Tracker>>,
    frozen: bool,
    allowed: bool,
}

pub(crate) struct Frame {
    states: Vec<(usize, State)>,
    previous: Vec<(usize, Scalar)>,
}

impl Motions {
    pub(crate) fn new(tracker: Rc<RefCell<Tracker>>) -> Self {
        Self {
            entries: Vec::new(),
            controls: Vec::new(),
            tracker,
            frozen: false,
            allowed: false,
        }
    }

    pub(crate) fn install(owner: &Rc<RefCell<Self>>, lua: &Lua, ui: &Table) -> Result<()> {
        let owner = Rc::downgrade(owner);
        ui.set(
            "motion",
            lua.create_function(move |lua, (initial, options): (Value, Table)| {
                let owner = owner
                    .upgrade()
                    .ok_or_else(|| Error::runtime("motion runtime has ended"))?;
                let mut motions = owner.borrow_mut();
                if motions.frozen {
                    return Err(Error::runtime(
                        "declare motions while loading the configuration",
                    ));
                }
                if motions.entries.len() >= MAX_MOTIONS {
                    return Err(Error::runtime("configuration exceeds 128 motions"));
                }
                let initial = number(initial)?;
                let spec = Spec::motion(lua, options)?;
                let sources = {
                    let mut tracker = motions.tracker.borrow_mut();
                    // reserve all signals before changing the tracker.
                    if tracker.remaining_values() < 3 {
                        return Err(Error::runtime("configuration exceeds 10,000 state values"));
                    }
                    [
                        tracker.insert(Scalar::Number(f64::from(initial)))?,
                        tracker.insert(Scalar::Number(0.0))?,
                        tracker.insert(Scalar::Boolean(false))?,
                    ]
                };
                let id = motions.entries.len();
                motions.entries.push(Entry {
                    state: State::new(spec, Resolved::Number(initial)),
                    sources,
                    pending_sample: false,
                });
                Ok(Handle {
                    id,
                    owner: Rc::downgrade(&owner),
                })
            })?,
        )
    }

    pub(crate) fn freeze(&mut self) {
        self.frozen = true;
    }

    pub(crate) fn allow_controls(&mut self, allowed: bool) {
        self.allowed = allowed;
    }

    pub(crate) fn checkpoint(&self) -> usize {
        self.controls.len()
    }

    pub(crate) fn truncate(&mut self, checkpoint: usize) {
        self.controls.truncate(checkpoint);
    }

    pub(crate) fn has_controls(&self) -> bool {
        !self.controls.is_empty()
    }

    pub(crate) fn commit_controls(&mut self, now: Instant) {
        for (id, control) in self.controls.drain(..) {
            let entry = &mut self.entries[id];
            match control {
                Control::Target(target) => entry.state.retarget(Resolved::Number(target), now),
                Control::Jump(value) => entry.state.reset(Resolved::Number(value)),
                Control::Stop => {
                    entry.state.advance(now);
                    entry.state.reset(entry.state.current());
                }
            }
            entry.pending_sample = true;
        }
    }

    pub(crate) fn active_bindings(&self) -> BTreeSet<usize> {
        let tracker = self.tracker.borrow();
        self.entries
            .iter()
            .filter(|entry| entry.state.is_active() || entry.pending_sample)
            .flat_map(|entry| entry.sources)
            .flat_map(|source| tracker.subscribers(source))
            .collect()
    }

    pub(crate) fn stage(&self, now: Instant) -> Result<Frame> {
        let mut frame = Frame {
            states: Vec::new(),
            previous: Vec::new(),
        };
        let mut values = Vec::new();
        let mut tracker = self.tracker.borrow_mut();
        for (id, entry) in self.entries.iter().enumerate() {
            if !entry.state.is_active() && !entry.pending_sample {
                continue;
            }
            let mut state = entry.state.clone();
            state.advance(now);
            let Resolved::Number(position) = state.current() else {
                unreachable!("shared motions have numeric values");
            };
            for (source, value) in entry.sources.into_iter().zip([
                Scalar::Number(f64::from(position)),
                Scalar::Number(state.velocity(now)),
                Scalar::Boolean(state.is_active()),
            ]) {
                frame.previous.push((source, tracker.peek(source)));
                values.push((source, value));
            }
            frame.states.push((id, state));
        }
        tracker.write_batch(values)?;
        Ok(frame)
    }

    pub(crate) fn commit_frame(&mut self, frame: Frame) {
        for (id, state) in frame.states {
            self.entries[id].state = state;
            self.entries[id].pending_sample = false;
        }
    }

    pub(crate) fn reject_frame(&mut self, frame: Frame) -> Result<()> {
        self.tracker.borrow_mut().write_batch(frame.previous)?;
        for (id, _) in frame.states {
            let entry = &mut self.entries[id];
            let Scalar::Number(value) = self.tracker.borrow().peek(entry.sources[0]) else {
                unreachable!("motion signals are numeric");
            };
            entry.state.reset(Resolved::Number(value as f32));
            entry.pending_sample = false;
        }
        Ok(())
    }
}

struct Handle {
    id: usize,
    owner: Weak<RefCell<Motions>>,
}

impl Handle {
    fn owner(&self) -> Result<Rc<RefCell<Motions>>> {
        self.owner
            .upgrade()
            .ok_or_else(|| Error::runtime("motion runtime has ended"))
    }

    fn read(&self, component: usize) -> Result<Scalar> {
        let owner = self.owner()?;
        let motions = owner.borrow();
        let source = motions.entries[self.id].sources[component];
        let value = motions.tracker.borrow_mut().read(source);
        Ok(value)
    }

    fn sample(&self, component: usize) -> Result<f64> {
        let Scalar::Number(value) = self.read(component)? else {
            unreachable!("motion signals are numeric");
        };
        Ok(value)
    }

    fn queue(&self, control: Control) -> Result<()> {
        let owner = self.owner()?;
        let mut motions = owner.borrow_mut();
        if !motions.allowed {
            return Err(Error::runtime(
                "motion controls are available only inside event handlers",
            ));
        }
        if motions.controls.len() >= MAX_CONTROLS {
            return Err(Error::runtime("too many motion controls in one event"));
        }
        motions.controls.push((self.id, control));
        Ok(())
    }
}

impl UserData for Handle {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("get", |_, this, ()| this.sample(0));
        methods.add_method("velocity", |_, this, ()| this.sample(1));
        methods.add_method("running", |_, this, ()| {
            let Scalar::Boolean(value) = this.read(2)? else {
                unreachable!("motion activity is a boolean");
            };
            Ok(value)
        });
        methods.add_method("to", |_, this, target: Value| {
            this.queue(Control::Target(number(target)?))
        });
        methods.add_method("jump", |_, this, value: Value| {
            this.queue(Control::Jump(number(value)?))
        });
        methods.add_method("stop", |_, this, ()| this.queue(Control::Stop));
    }
}

fn number(value: Value) -> Result<f32> {
    let value = match value {
        Value::Integer(value) => value as f64,
        Value::Number(value) => value,
        _ => return Err(Error::runtime("motion value must be a finite number")),
    } as f32;
    if !value.is_finite() {
        return Err(Error::runtime(
            "motion value must fit in a finite 32-bit number",
        ));
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insufficient_capacity_does_not_reserve_partial_signals() {
        let tracker = Rc::new(RefCell::new(Tracker::default()));
        for _ in 0..9998 {
            tracker.borrow_mut().insert(Scalar::Nil).unwrap();
        }
        let motions = Rc::new(RefCell::new(Motions::new(tracker.clone())));
        let lua = Lua::new();
        let ui = lua.create_table().unwrap();
        Motions::install(&motions, &lua, &ui).unwrap();
        lua.globals().set("ui", ui).unwrap();
        assert!(
            lua.load("return ui.motion(0, {kind='transition',duration=1})")
                .exec()
                .is_err()
        );
        assert_eq!(tracker.borrow().remaining_values(), 2);
        assert!(motions.borrow().entries.is_empty());
    }
}
