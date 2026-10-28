use std::time::{Duration, Instant};

use mlua::{Error, Function, Result, Table, UserData, Value};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Easing {
    Linear,
    InCubic,
    OutCubic,
    InOutCubic,
}

impl Easing {
    fn parse(value: Value) -> Result<Self> {
        let Value::String(value) = value else {
            return Err(Error::runtime("transition easing must be a string"));
        };
        match value.to_str()?.as_ref() {
            "linear" => Ok(Self::Linear),
            "in_cubic" => Ok(Self::InCubic),
            "out_cubic" => Ok(Self::OutCubic),
            "in_out_cubic" => Ok(Self::InOutCubic),
            _ => Err(Error::runtime(
                "transition easing must be 'linear', 'in_cubic', 'out_cubic', or 'in_out_cubic'",
            )),
        }
    }

    fn sample(self, progress: f32) -> f32 {
        match self {
            Self::Linear => progress,
            Self::InCubic => progress * progress * progress,
            Self::OutCubic => 1.0 - (1.0 - progress).powi(3),
            Self::InOutCubic if progress < 0.5 => 4.0 * progress * progress * progress,
            Self::InOutCubic => 1.0 - (-2.0 * progress + 2.0).powi(3) / 2.0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Spec {
    duration: Duration,
    easing: Easing,
}

impl Spec {
    fn parse(options: Table) -> Result<Self> {
        let mut duration = None;
        let mut easing = Easing::OutCubic;
        for pair in options.pairs::<String, Value>() {
            let (name, value) = pair?;
            match name.as_str() {
                "duration" => duration = Some(milliseconds(value)?),
                "easing" => easing = Easing::parse(value)?,
                _ => {
                    return Err(Error::runtime(format!(
                        "unknown transition option {name:?}"
                    )));
                }
            }
        }
        let duration = duration.ok_or_else(|| {
            Error::runtime("transition options must include a duration in milliseconds")
        })?;
        Ok(Self { duration, easing })
    }
}

pub(crate) struct Descriptor {
    function: Function,
    spec: Spec,
}

impl Descriptor {
    pub(crate) fn new(function: Function, options: Table) -> Result<Self> {
        Ok(Self {
            function,
            spec: Spec::parse(options)?,
        })
    }

    pub(crate) fn parts(&self) -> (Function, Spec) {
        (self.function.clone(), self.spec)
    }
}

impl UserData for Descriptor {}

#[derive(Clone, Copy, Debug)]
struct Segment {
    from: f32,
    started_at: Instant,
}

#[derive(Clone, Debug)]
pub(crate) struct State {
    spec: Spec,
    current: f32,
    target: f32,
    segment: Option<Segment>,
}

impl State {
    pub(crate) fn new(spec: Spec, target: f32) -> Self {
        Self {
            spec,
            current: target,
            target,
            segment: None,
        }
    }

    pub(crate) fn inherit(previous: &Self, initial: &Self, now: Instant) -> Self {
        let mut previous = previous.clone();
        previous.advance(now);
        if previous.spec == initial.spec && previous.target == initial.target {
            previous
        } else {
            let current = previous.current;
            let mut next = Self::new(initial.spec, current);
            next.retarget(initial.target, now);
            next
        }
    }

    pub(crate) fn retarget(&mut self, target: f32, now: Instant) {
        self.advance(now);
        if self.target == target {
            return;
        }
        self.target = target;
        if self.current == target || self.spec.duration.is_zero() {
            self.current = target;
            self.segment = None;
        } else {
            self.segment = Some(Segment {
                from: self.current,
                started_at: now,
            });
        }
    }

    pub(crate) fn advance(&mut self, now: Instant) {
        let Some(segment) = self.segment else {
            return;
        };
        let elapsed = now.saturating_duration_since(segment.started_at);
        if elapsed >= self.spec.duration {
            self.current = self.target;
            self.segment = None;
            return;
        }
        let progress = elapsed.as_secs_f32() / self.spec.duration.as_secs_f32();
        let eased = self.spec.easing.sample(progress.clamp(0.0, 1.0));
        self.current = segment.from + (self.target - segment.from) * eased;
    }

    pub(crate) fn current(&self) -> f32 {
        self.current
    }

    pub(crate) fn target(&self) -> f32 {
        self.target
    }

    pub(crate) fn is_active(&self) -> bool {
        self.segment.is_some()
    }
}

pub(crate) fn supports(property: &str) -> bool {
    matches!(
        property,
        "style.width"
            | "style.height"
            | "style.padding"
            | "style.padding_x"
            | "style.padding_y"
            | "style.gap"
            | "style.grow"
            | "style.radius"
            | "style.border_width"
            | "style.opacity"
    )
}

pub(crate) fn target(value: Value, property: &str) -> Result<f32> {
    let number = match value {
        Value::Integer(value) => value as f64,
        Value::Number(value) => value,
        _ => {
            return Err(Error::runtime(format!(
                "transition target for {property} must be a number"
            )));
        }
    };
    let number = number as f32;
    if !number.is_finite() {
        return Err(Error::runtime(format!(
            "transition target for {property} must be finite"
        )));
    }
    Ok(number)
}

pub(crate) fn value(value: f32) -> Value {
    Value::Number(value as f64)
}

fn milliseconds(value: Value) -> Result<Duration> {
    let number = match value {
        Value::Integer(value) => value as f64,
        Value::Number(value) => value,
        _ => {
            return Err(Error::runtime(
                "transition duration must be a whole number between 0 and 60,000",
            ));
        }
    };
    if !number.is_finite() || number.fract() != 0.0 || !(0.0..=60_000.0).contains(&number) {
        return Err(Error::runtime(
            "transition duration must be a whole number between 0 and 60,000",
        ));
    }
    Ok(Duration::from_millis(number as u64))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(left: f32, right: f32) {
        assert!((left - right).abs() < 0.001, "{left} != {right}");
    }

    #[test]
    fn easing_curves_have_exact_endpoints_and_stay_monotonic() {
        for easing in [
            Easing::Linear,
            Easing::InCubic,
            Easing::OutCubic,
            Easing::InOutCubic,
        ] {
            assert_eq!(easing.sample(0.0), 0.0);
            assert_eq!(easing.sample(1.0), 1.0);
            let samples: Vec<_> = (0..=100)
                .map(|step| easing.sample(step as f32 / 100.0))
                .collect();
            assert!(samples.windows(2).all(|pair| pair[0] <= pair[1]));
            assert!(samples.iter().all(|sample| sample.is_finite()));
        }
    }

    #[test]
    fn retargeting_starts_from_the_displayed_value() {
        let start = Instant::now();
        let spec = Spec {
            duration: Duration::from_millis(100),
            easing: Easing::Linear,
        };
        let mut state = State::new(spec, 0.0);
        state.retarget(10.0, start);
        state.advance(start + Duration::from_millis(40));
        close(state.current(), 4.0);

        state.retarget(0.0, start + Duration::from_millis(40));
        close(state.current(), 4.0);
        state.advance(start + Duration::from_millis(90));
        close(state.current(), 2.0);
        state.advance(start + Duration::from_millis(140));
        close(state.current(), 0.0);
        assert!(!state.is_active());
    }

    #[test]
    fn an_unchanged_target_does_not_restart_a_segment() {
        let start = Instant::now();
        let spec = Spec {
            duration: Duration::from_millis(100),
            easing: Easing::Linear,
        };
        let mut state = State::new(spec, 0.0);
        state.retarget(10.0, start);
        state.retarget(10.0, start + Duration::from_millis(40));
        state.advance(start + Duration::from_millis(50));
        close(state.current(), 5.0);
    }

    #[test]
    fn zero_duration_snaps_to_the_target() {
        let start = Instant::now();
        let mut state = State::new(
            Spec {
                duration: Duration::ZERO,
                easing: Easing::OutCubic,
            },
            0.0,
        );
        state.retarget(10.0, start);
        assert_eq!(state.current(), 10.0);
        assert!(!state.is_active());
    }
}
