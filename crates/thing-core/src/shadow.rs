use crate::{Color, Point, TreeError};

pub const MAX_SHADOW_BLUR: f32 = 32.0;
pub const MAX_SHADOW_SPREAD: f32 = 32.0;
pub const MAX_SHADOW_OFFSET: f32 = 100_000.0;

/// an outer shadow of a border box or a filled path, in local logical pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Shadow {
    pub color: Color,
    pub offset: Point,
    pub blur: f32,
    pub spread: f32,
}

impl Shadow {
    pub fn validate(self) -> Result<(), TreeError> {
        if !self.blur.is_finite() || !(0.0..=MAX_SHADOW_BLUR).contains(&self.blur) {
            return Err(TreeError(
                "shadow blur must be between 0 and 32 logical pixels".into(),
            ));
        }
        if !self.spread.is_finite() || self.spread.abs() > MAX_SHADOW_SPREAD {
            return Err(TreeError(
                "shadow spread must be between -32 and 32 logical pixels".into(),
            ));
        }
        if [self.offset.x, self.offset.y]
            .into_iter()
            .any(|value| !value.is_finite() || value.abs() > MAX_SHADOW_OFFSET)
        {
            return Err(TreeError(
                "shadow offsets must be between -100000 and 100000 logical pixels".into(),
            ));
        }
        Ok(())
    }
}
