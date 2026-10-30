use std::time::{Duration, Instant};

use mlua::{Error, Function, Lua, Result, Table, UserData, Value as LuaValue};
use thing_core::Color;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Easing {
    Linear,
    InCubic,
    OutCubic,
    InOutCubic,
}

impl Easing {
    fn parse(value: LuaValue) -> Result<Self> {
        let LuaValue::String(value) = value else {
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

    fn slope(self, progress: f64) -> f64 {
        match self {
            Self::Linear => 1.0,
            Self::InCubic => 3.0 * progress * progress,
            Self::OutCubic => 3.0 * (1.0 - progress).powi(2),
            Self::InOutCubic if progress < 0.5 => 12.0 * progress * progress,
            Self::InOutCubic => 3.0 * (-2.0 * progress + 2.0).powi(2),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TransitionSpec {
    duration: Duration,
    easing: Easing,
}

impl TransitionSpec {
    fn parse(options: Table) -> Result<Self> {
        let mut duration = None;
        let mut easing = Easing::OutCubic;
        for pair in options.pairs::<String, LuaValue>() {
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

#[derive(Clone, Copy, Debug, PartialEq)]
struct SpringSpec {
    damping: f64,
    epsilon: f64,
    angular_frequency: f64,
    velocity_epsilon: f64,
}

impl SpringSpec {
    fn parse(options: Table) -> Result<Self> {
        let mut stiffness = None;
        let mut damping = None;
        let mut epsilon = 0.01;
        for pair in options.pairs::<String, LuaValue>() {
            let (name, value) = pair?;
            match name.as_str() {
                "stiffness" => stiffness = Some(positive(value, "spring stiffness")?),
                "damping" => damping = Some(positive(value, "spring damping")?),
                "epsilon" => epsilon = positive(value, "spring epsilon")?,
                _ => return Err(Error::runtime(format!("unknown spring option {name:?}"))),
            }
        }
        let stiffness =
            stiffness.ok_or_else(|| Error::runtime("spring options must include stiffness"))?;
        let damping =
            damping.ok_or_else(|| Error::runtime("spring options must include damping"))?;
        let angular_frequency = stiffness.sqrt();
        let decay = damping * angular_frequency;
        let velocity_epsilon = epsilon * angular_frequency;
        let fast_mode = if damping > 1.0 {
            let root = (damping - 1.0).sqrt() * (damping + 1.0).sqrt();
            angular_frequency * (damping + root)
        } else {
            angular_frequency
        };
        if !decay.is_finite()
            || decay <= 0.0
            || !velocity_epsilon.is_finite()
            || velocity_epsilon <= 0.0
            || !fast_mode.is_finite()
        {
            return Err(Error::runtime("spring options are too small or too large"));
        }
        Ok(Self {
            damping,
            epsilon,
            angular_frequency,
            velocity_epsilon,
        })
    }

    fn settled(self, position: f64, velocity: f64, target: f64) -> bool {
        (position - target).abs() <= self.epsilon && velocity.abs() <= self.velocity_epsilon
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum SpecKind {
    Transition(TransitionSpec),
    Spring(SpringSpec),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Spec(SpecKind);

impl Spec {
    pub(crate) fn motion(lua: &Lua, options: Table) -> Result<Self> {
        let kind = options.raw_get::<String>("kind")?;
        let parameters = lua.create_table()?;
        for pair in options.pairs::<String, LuaValue>() {
            let (name, value) = pair?;
            if name != "kind" {
                parameters.raw_set(name, value)?;
            }
        }
        match kind.as_str() {
            "transition" => Self::transition(parameters),
            "spring" => Self::spring(parameters),
            _ => Err(Error::runtime(
                "motion kind must be 'transition' or 'spring'",
            )),
        }
    }

    fn transition(options: Table) -> Result<Self> {
        Ok(Self(SpecKind::Transition(TransitionSpec::parse(options)?)))
    }

    fn spring(options: Table) -> Result<Self> {
        Ok(Self(SpecKind::Spring(SpringSpec::parse(options)?)))
    }

    pub(crate) fn supports(self, property: &str) -> bool {
        match self.0 {
            SpecKind::Transition(_) => supports(property),
            SpecKind::Spring(_) => numeric_property(property),
        }
    }

    pub(crate) fn name(self) -> &'static str {
        match self.0 {
            SpecKind::Transition(_) => "transitions",
            SpecKind::Spring(_) => "springs",
        }
    }
}

pub(crate) struct Descriptor {
    function: Function,
    spec: Spec,
}

impl Descriptor {
    pub(crate) fn transition(function: Function, options: Table) -> Result<Self> {
        Ok(Self {
            function,
            spec: Spec::transition(options)?,
        })
    }

    pub(crate) fn spring(function: Function, options: Table) -> Result<Self> {
        Ok(Self {
            function,
            spec: Spec::spring(options)?,
        })
    }

    pub(crate) fn parts(&self) -> (Function, Spec) {
        (self.function.clone(), self.spec)
    }
}

impl UserData for Descriptor {}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Resolved {
    Number(f32),
    Color(Color),
}

impl Resolved {
    fn interpolate(self, target: Self, progress: f32) -> Self {
        if progress <= 0.0 {
            return self;
        }
        if progress >= 1.0 {
            return target;
        }
        match (self, target) {
            (Self::Number(from), Self::Number(to)) => Self::Number(
                (f64::from(from) + (f64::from(to) - f64::from(from)) * f64::from(progress)) as f32,
            ),
            (Self::Color(from), Self::Color(to)) => {
                Self::Color(interpolate_color(from, to, progress))
            }
            _ => unreachable!("a transition property keeps one value type"),
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Segment {
    from: Resolved,
    started_at: Instant,
}

#[derive(Clone, Copy, Debug)]
struct SpringMotion {
    position: f64,
    velocity: f64,
    advanced_at: Option<Instant>,
    active: bool,
}

#[derive(Clone, Copy, Debug)]
enum Motion {
    Transition(Option<Segment>),
    Spring(SpringMotion),
}

#[derive(Clone, Debug)]
pub(crate) struct State {
    spec: Spec,
    current: Resolved,
    target: Resolved,
    motion: Motion,
}

impl State {
    pub(crate) fn new(spec: Spec, target: Resolved) -> Self {
        let motion = match spec.0 {
            SpecKind::Transition(_) => Motion::Transition(None),
            SpecKind::Spring(_) => {
                let Resolved::Number(value) = target else {
                    unreachable!("spring descriptors are accepted only for numeric properties")
                };
                Motion::Spring(SpringMotion {
                    position: f64::from(value),
                    velocity: 0.0,
                    advanced_at: None,
                    active: false,
                })
            }
        };
        Self {
            spec,
            current: target,
            target,
            motion,
        }
    }

    pub(crate) fn inherit(previous: &Self, initial: &Self, now: Instant) -> Self {
        let mut previous = previous.clone();
        previous.advance(now);
        if previous.spec == initial.spec {
            previous.retarget(initial.target, now);
            previous
        } else {
            let current = previous.current;
            let mut next = Self::new(initial.spec, current);
            next.retarget(initial.target, now);
            next
        }
    }

    pub(crate) fn retarget(&mut self, target: Resolved, now: Instant) {
        self.advance(now);
        if self.target == target {
            return;
        }
        self.target = target;
        match (self.spec.0, &mut self.motion) {
            (SpecKind::Transition(spec), Motion::Transition(segment)) => {
                if self.current == target || spec.duration.is_zero() {
                    self.current = target;
                    *segment = None;
                } else {
                    *segment = Some(Segment {
                        from: self.current,
                        started_at: now,
                    });
                }
            }
            (SpecKind::Spring(spec), Motion::Spring(motion)) => {
                let Resolved::Number(target) = target else {
                    unreachable!("a spring target remains numeric")
                };
                motion.advanced_at = Some(
                    motion
                        .advanced_at
                        .map_or(now, |advanced_at| advanced_at.max(now)),
                );
                motion.active = !spec.settled(motion.position, motion.velocity, f64::from(target));
                if !motion.active {
                    motion.position = f64::from(target);
                    motion.velocity = 0.0;
                    self.current = target.into();
                }
            }
            _ => unreachable!("the animation state matches its specification"),
        }
    }

    pub(crate) fn advance(&mut self, now: Instant) {
        match (self.spec.0, &mut self.motion) {
            (SpecKind::Transition(spec), Motion::Transition(segment)) => {
                let Some(active) = *segment else {
                    return;
                };
                let elapsed = now.saturating_duration_since(active.started_at);
                if elapsed >= spec.duration {
                    self.current = self.target;
                    *segment = None;
                    return;
                }
                let progress = elapsed.as_secs_f32() / spec.duration.as_secs_f32();
                let eased = spec.easing.sample(progress.clamp(0.0, 1.0));
                self.current = active.from.interpolate(self.target, eased);
            }
            (SpecKind::Spring(spec), Motion::Spring(motion)) => {
                advance_spring(spec, motion, self.target, &mut self.current, now);
            }
            _ => unreachable!("the animation state matches its specification"),
        }
    }

    pub(crate) fn current(&self) -> Resolved {
        self.current
    }

    pub(crate) fn target(&self) -> Resolved {
        self.target
    }

    pub(crate) fn is_active(&self) -> bool {
        match self.motion {
            Motion::Transition(segment) => segment.is_some(),
            Motion::Spring(motion) => motion.active,
        }
    }

    pub(crate) fn reset(&mut self, value: Resolved) {
        *self = Self::new(self.spec, value);
    }

    pub(crate) fn velocity(&self, now: Instant) -> f64 {
        match (self.spec.0, self.motion) {
            (_, Motion::Spring(motion)) => motion.velocity,
            (SpecKind::Transition(spec), Motion::Transition(Some(segment))) => {
                let (Resolved::Number(from), Resolved::Number(to)) = (segment.from, self.target)
                else {
                    return 0.0;
                };
                let progress = now
                    .saturating_duration_since(segment.started_at)
                    .as_secs_f64()
                    / spec.duration.as_secs_f64();
                if progress >= 1.0 {
                    0.0
                } else {
                    (f64::from(to) - f64::from(from)) * spec.easing.slope(progress)
                        / spec.duration.as_secs_f64()
                }
            }
            _ => 0.0,
        }
    }
}

impl From<f32> for Resolved {
    fn from(value: f32) -> Self {
        Self::Number(value)
    }
}

fn advance_spring(
    spec: SpringSpec,
    motion: &mut SpringMotion,
    target: Resolved,
    current: &mut Resolved,
    now: Instant,
) {
    if !motion.active {
        return;
    }
    let Resolved::Number(target) = target else {
        unreachable!("a spring target remains numeric")
    };
    let advanced_at = motion.advanced_at.unwrap_or(now);
    let Some(elapsed) = now.checked_duration_since(advanced_at) else {
        return;
    };
    if elapsed.is_zero() {
        return;
    }
    let target = f64::from(target);
    let (position, velocity) = spring_step(
        spec,
        motion.position,
        motion.velocity,
        target,
        elapsed.as_secs_f64(),
    );
    motion.advanced_at = Some(now);
    if !position.is_finite() || position.abs() > f64::from(f32::MAX) || !velocity.is_finite() {
        motion.position = target;
        motion.velocity = 0.0;
        motion.active = false;
        *current = Resolved::Number(target as f32);
        return;
    }
    motion.position = position;
    motion.velocity = velocity;
    if spec.settled(position, velocity, target) {
        motion.position = target;
        motion.velocity = 0.0;
        motion.active = false;
    }
    *current = Resolved::Number(motion.position as f32);
}

fn spring_step(
    spec: SpringSpec,
    position: f64,
    velocity: f64,
    target: f64,
    elapsed: f64,
) -> (f64, f64) {
    let displacement = position - target;
    let frequency = spec.angular_frequency;
    let damping = spec.damping;
    let decay = damping * frequency;

    if damping < 1.0 {
        let damped = frequency * (1.0 - damping * damping).sqrt();
        let phase = damped * elapsed;
        let envelope = (-decay * elapsed).exp();
        if envelope == 0.0 {
            return (target, 0.0);
        }
        let normalized_sine = sinc(phase);
        let damped_sine = envelope * frequency * elapsed * normalized_sine;
        let damped_time_sine = envelope * elapsed * normalized_sine;
        let next_displacement = envelope * displacement * phase.cos()
            + velocity * damped_time_sine
            + damping * displacement * damped_sine;
        let next_velocity = envelope * velocity * phase.cos()
            - frequency * displacement * damped_sine
            - damping * velocity * damped_sine;
        (target + next_displacement, next_velocity)
    } else if damping == 1.0 {
        let phase = frequency * elapsed;
        let envelope = (-phase).exp();
        if envelope == 0.0 {
            return (target, 0.0);
        }
        let damped_phase = envelope * phase;
        let next_displacement =
            displacement * (envelope + damped_phase) + velocity * envelope * elapsed;
        let next_velocity =
            velocity * (envelope - damped_phase) - frequency * displacement * damped_phase;
        (target + next_displacement, next_velocity)
    } else {
        let root = (damping - 1.0).sqrt() * (damping + 1.0).sqrt();
        let grown = frequency * root;
        let phase = grown * elapsed;
        // the hyperbolic form avoids ill-conditioned coefficients while the
        // modes remain close over this step. once their phase separates,
        // explicit modes avoid cancellation between a large cosh and sinh.
        if phase <= 1.0 {
            let envelope = (-decay * elapsed).exp();
            if envelope == 0.0 {
                return (target, 0.0);
            }
            let normalized_sine = sinhc(phase);
            let damped_frequency_sine = envelope * frequency * elapsed * normalized_sine;
            let damped_decay_sine = envelope * decay * elapsed * normalized_sine;
            let next_displacement = envelope * displacement * phase.cosh()
                + velocity * envelope * elapsed * normalized_sine
                + displacement * damped_decay_sine;
            let next_velocity = envelope * velocity * phase.cosh()
                - frequency * displacement * damped_frequency_sine
                - velocity * damped_decay_sine;
            return (target + next_displacement, next_velocity);
        }
        // `damping - root` loses the slow mode entirely for large damping.
        // the reciprocal form is the same root, without subtracting two
        // nearly equal values.
        let slow = -frequency / (damping + root);
        let fast = -frequency * (damping + root);
        let mode_ratio = slow / fast;
        let velocity_ratio = velocity / fast;
        let denominator = mode_ratio - 1.0;
        let slow_coefficient = (velocity_ratio - displacement) / denominator;
        let fast_coefficient = (mode_ratio * displacement - velocity_ratio) / denominator;
        let slow_term = slow_coefficient * (slow * elapsed).exp();
        let fast_term = fast_coefficient * (fast * elapsed).exp();
        (
            target + slow_term + fast_term,
            slow * slow_term + fast * fast_term,
        )
    }
}

fn sinc(value: f64) -> f64 {
    if value.abs() < 1.0e-4 {
        let squared = value * value;
        1.0 - squared / 6.0 + squared * squared / 120.0
    } else {
        value.sin() / value
    }
}

fn sinhc(value: f64) -> f64 {
    if value.abs() < 1.0e-4 {
        let squared = value * value;
        1.0 + squared / 6.0 + squared * squared / 120.0
    } else {
        value.sinh() / value
    }
}

pub(crate) fn supports(property: &str) -> bool {
    numeric_property(property) || color_property(property)
}

pub(crate) fn target(value: LuaValue, property: &str) -> Result<Resolved> {
    if color_property(property) {
        let LuaValue::String(value) = value else {
            return Err(Error::runtime(format!(
                "animation target for {property} must be a color string"
            )));
        };
        return value.to_str()?.parse().map(Resolved::Color).map_err(|_| {
            Error::runtime(format!(
                "animation target for {property} must be #RRGGBB or #RRGGBBAA"
            ))
        });
    }
    let number = match value {
        LuaValue::Integer(value) => value as f64,
        LuaValue::Number(value) => value,
        _ => {
            return Err(Error::runtime(format!(
                "animation target for {property} must be a number"
            )));
        }
    };
    let number = number as f32;
    if !number.is_finite() {
        return Err(Error::runtime(format!(
            "animation target for {property} must be finite"
        )));
    }
    Ok(Resolved::Number(number))
}

fn numeric_property(property: &str) -> bool {
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
            | "style.translate_x"
            | "style.translate_y"
            | "style.scale_x"
            | "style.scale_y"
            | "style.rotation"
    )
}

fn color_property(property: &str) -> bool {
    matches!(
        property,
        "style.background"
            | "style.foreground"
            | "style.border_color"
            | "style.hover_overlay"
            | "style.pressed_overlay"
            | "style.focus_ring_color"
    )
}

fn interpolate_color(from: Color, target: Color, progress: f32) -> Color {
    let from = linear_premultiplied(from);
    let target = linear_premultiplied(target);
    let mixed: [f32; 4] =
        std::array::from_fn(|index| from[index] + (target[index] - from[index]) * progress);
    let alpha = mixed[3].clamp(0.0, 1.0);
    let mut rgba = [0; 4];
    if alpha > 0.0 {
        for index in 0..3 {
            rgba[index] = linear_to_srgb((mixed[index] / alpha).clamp(0.0, 1.0));
        }
    }
    rgba[3] = (alpha * 255.0).round().clamp(0.0, 255.0) as u8;
    Color(rgba)
}

fn linear_premultiplied(color: Color) -> [f32; 4] {
    let alpha = f32::from(color.0[3]) / 255.0;
    [
        srgb_to_linear(color.0[0]) * alpha,
        srgb_to_linear(color.0[1]) * alpha,
        srgb_to_linear(color.0[2]) * alpha,
        alpha,
    ]
}

fn srgb_to_linear(component: u8) -> f32 {
    let value = f32::from(component) / 255.0;
    if value <= 0.040_45 {
        value / 12.92
    } else {
        ((value + 0.055) / 1.055).powf(2.4)
    }
}

fn linear_to_srgb(value: f32) -> u8 {
    let value = if value <= 0.003_130_8 {
        value * 12.92
    } else {
        1.055 * value.powf(1.0 / 2.4) - 0.055
    };
    (value * 255.0).round().clamp(0.0, 255.0) as u8
}

fn milliseconds(value: LuaValue) -> Result<Duration> {
    let number = match value {
        LuaValue::Integer(value) => value as f64,
        LuaValue::Number(value) => value,
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

fn positive(value: LuaValue, name: &str) -> Result<f64> {
    let number = match value {
        LuaValue::Integer(value) => value as f64,
        LuaValue::Number(value) => value,
        _ => return Err(Error::runtime(format!("{name} must be a positive number"))),
    };
    if !number.is_finite() || number <= 0.0 {
        return Err(Error::runtime(format!(
            "{name} must be a positive finite number"
        )));
    }
    Ok(number)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(left: f32, right: f32) {
        assert!((left - right).abs() < 0.001, "{left} != {right}");
    }

    fn number(value: Resolved) -> f32 {
        let Resolved::Number(value) = value else {
            panic!("expected a numeric transition value");
        };
        value
    }

    fn transition_spec(duration: Duration, easing: Easing) -> Spec {
        Spec(SpecKind::Transition(TransitionSpec { duration, easing }))
    }

    fn spring_spec(stiffness: f64, damping: f64, epsilon: f64) -> SpringSpec {
        let angular_frequency = stiffness.sqrt();
        SpringSpec {
            damping,
            epsilon,
            angular_frequency,
            velocity_epsilon: epsilon * angular_frequency,
        }
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
        let spec = transition_spec(Duration::from_millis(100), Easing::Linear);
        let mut state = State::new(spec, Resolved::Number(0.0));
        state.retarget(Resolved::Number(10.0), start);
        state.advance(start + Duration::from_millis(40));
        close(number(state.current()), 4.0);

        state.retarget(Resolved::Number(0.0), start + Duration::from_millis(40));
        close(number(state.current()), 4.0);
        state.advance(start + Duration::from_millis(90));
        close(number(state.current()), 2.0);
        state.advance(start + Duration::from_millis(140));
        close(number(state.current()), 0.0);
        assert!(!state.is_active());
    }

    #[test]
    fn an_unchanged_target_does_not_restart_a_segment() {
        let start = Instant::now();
        let spec = transition_spec(Duration::from_millis(100), Easing::Linear);
        let mut state = State::new(spec, Resolved::Number(0.0));
        state.retarget(Resolved::Number(10.0), start);
        state.retarget(Resolved::Number(10.0), start + Duration::from_millis(40));
        state.advance(start + Duration::from_millis(50));
        close(number(state.current()), 5.0);
    }

    #[test]
    fn zero_duration_snaps_to_the_target() {
        let start = Instant::now();
        let mut state = State::new(
            transition_spec(Duration::ZERO, Easing::OutCubic),
            Resolved::Number(0.0),
        );
        state.retarget(Resolved::Number(10.0), start);
        assert_eq!(state.current(), Resolved::Number(10.0));
        assert!(!state.is_active());
    }

    #[test]
    fn analytic_springs_do_not_depend_on_frame_partitioning() {
        for damping in [0.5, 0.999_999_999, 1.0, 1.000_000_001, 2.0] {
            let spec = spring_spec(170.0, damping, 0.001);
            let direct = spring_step(spec, 0.0, 0.0, 100.0, 0.75);
            let mut partitioned = (0.0, 0.0);
            for _ in 0..15 {
                partitioned = spring_step(spec, partitioned.0, partitioned.1, 100.0, 0.05);
            }
            assert!((direct.0 - partitioned.0).abs() < 1.0e-9);
            assert!((direct.1 - partitioned.1).abs() < 1.0e-8);
        }
    }

    #[test]
    fn overdamped_forms_agree_across_the_phase_boundary() {
        let spec = spring_spec(170.0, 2.0, 0.001);
        let direct = spring_step(spec, 0.0, 0.0, 100.0, 0.06);
        let mut partitioned = (0.0, 0.0);
        for _ in 0..3 {
            partitioned = spring_step(spec, partitioned.0, partitioned.1, 100.0, 0.02);
        }

        assert!((direct.0 - partitioned.0).abs() < 1.0e-10);
        assert!((direct.1 - partitioned.1).abs() < 1.0e-9);
    }

    #[test]
    fn spring_retargeting_preserves_momentum() {
        let start = Instant::now();
        let mut state = State::new(
            Spec(SpecKind::Spring(spring_spec(100.0, 0.5, 0.001))),
            Resolved::Number(0.0),
        );
        state.retarget(Resolved::Number(10.0), start);
        state.advance(start + Duration::from_millis(100));
        let before = number(state.current());

        state.retarget(Resolved::Number(before), start + Duration::from_millis(100));
        assert!(state.is_active());
        state.advance(start + Duration::from_millis(110));
        assert!(number(state.current()) > before);
    }

    #[test]
    fn stale_spring_timestamps_do_not_rewind_motion() {
        let start = Instant::now();
        let mut state = State::new(
            Spec(SpecKind::Spring(spring_spec(100.0, 0.8, 0.001))),
            Resolved::Number(0.0),
        );
        state.retarget(Resolved::Number(100.0), start);
        state.advance(start + Duration::from_millis(100));
        let position = state.current();

        state.advance(start + Duration::from_millis(50));
        assert_eq!(state.current(), position);
        state.advance(start + Duration::from_millis(110));
        assert_ne!(state.current(), position);
    }

    #[test]
    fn a_long_frame_settles_a_damped_spring_exactly() {
        let start = Instant::now();
        let mut state = State::new(
            Spec(SpecKind::Spring(spring_spec(170.0, 0.82, 0.01))),
            Resolved::Number(0.0),
        );
        state.retarget(Resolved::Number(100.0), start);
        state.advance(start + Duration::from_secs(60));

        assert_eq!(state.current(), Resolved::Number(100.0));
        assert!(!state.is_active());
    }

    #[test]
    fn heavily_overdamped_springs_keep_their_slow_mode() {
        let spec = spring_spec(1.0, 1.0e16, 0.001);
        let (position, velocity) = spring_step(spec, 0.0, 0.0, 100.0, 1.0e16);

        assert!((position - 39.346_934).abs() < 0.000_1, "{position}");
        assert!(velocity.is_finite());
    }

    #[test]
    fn an_overdamped_fast_mode_does_not_gain_a_slow_tail() {
        let spec = spring_spec(1.0, 10.0, 1.0e-20);
        let root = 99.0_f64.sqrt();
        let fast = -(10.0 + root);
        let elapsed = 300.0 / root;
        let expected = (fast * elapsed).exp();

        let (position, velocity) = spring_step(spec, 1.0, fast, 0.0, elapsed);
        assert!((position / expected - 1.0).abs() < 1.0e-12);
        assert!((velocity / (fast * expected) - 1.0).abs() < 1.0e-12);
    }

    #[test]
    fn a_very_stiff_underdamped_spring_stays_finite() {
        let spec = spring_spec(1.0e308, 1.0e-154, 0.01);

        for elapsed in [1.0e-9, 0.001] {
            let (position, velocity) = spring_step(spec, 0.0, 0.0, 100.0, elapsed);
            assert!(position.is_finite(), "position at {elapsed}s: {position}");
            assert!(velocity.is_finite(), "velocity at {elapsed}s: {velocity}");
            assert!((0.0..=200.0).contains(&position), "{position}");
        }
    }

    #[test]
    fn an_extremely_overdamped_step_keeps_its_slow_mode() {
        let spec = spring_spec(1.0, 5.0e306, 0.01);
        let (position, velocity) = spring_step(spec, 0.0, 0.0, 100.0, 0.001);

        assert!(position.abs() < 1.0e-12, "{position}");
        assert!(velocity.is_finite(), "{velocity}");
    }

    #[test]
    fn colors_interpolate_in_premultiplied_linear_srgb() {
        let black = Resolved::Color(Color([0, 0, 0, 255]));
        let white = Resolved::Color(Color([255, 255, 255, 255]));
        assert_eq!(black.interpolate(white, 0.0), black);
        assert_eq!(black.interpolate(white, 1.0), white);
        assert_eq!(
            black.interpolate(white, 0.5),
            Resolved::Color(Color([188, 188, 188, 255]))
        );

        let transparent_red = Resolved::Color(Color([255, 0, 0, 0]));
        let opaque_blue = Resolved::Color(Color([0, 0, 255, 255]));
        assert_eq!(
            transparent_red.interpolate(opaque_blue, 0.5),
            Resolved::Color(Color([0, 0, 255, 128]))
        );
    }
}
