use crate::TreeError;

pub const MAX_IMAGE_BLUR: f32 = 64.0;
pub const MAX_IMAGE_SATURATION: f32 = 2.0;

/// painting options for an image's pixels. they are not part of the image's
/// source identity, so changing them never loads or decodes the image again.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ImageEffects {
    /// a blur radius in local logical pixels; the gaussian sigma is half of it.
    pub blur: f32,
    /// 0 is grayscale, 1 keeps the colors, and 2 doubles their saturation.
    pub saturation: f32,
}

impl Default for ImageEffects {
    fn default() -> Self {
        Self {
            blur: 0.0,
            saturation: 1.0,
        }
    }
}

impl ImageEffects {
    /// whether painting can skip every effect and keep the plain image draw.
    pub fn is_neutral(self) -> bool {
        // halving a positive subnormal radius rounds the sigma to zero.
        self.blur / 2.0 == 0.0 && self.saturation == 1.0
    }

    pub fn validate(self) -> Result<(), TreeError> {
        if !self.blur.is_finite() || !(0.0..=MAX_IMAGE_BLUR).contains(&self.blur) {
            return Err(TreeError(
                "image blur must be between 0 and 64 logical pixels".into(),
            ));
        }
        if !self.saturation.is_finite() || !(0.0..=MAX_IMAGE_SATURATION).contains(&self.saturation)
        {
            return Err(TreeError("image saturation must be between 0 and 2".into()));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounds_accept_the_documented_range_only() {
        assert!(ImageEffects::default().is_neutral());
        for (blur, saturation) in [(0.0, 0.0), (64.0, 2.0), (0.5, 1.0)] {
            assert!(ImageEffects { blur, saturation }.validate().is_ok());
        }
        for (blur, saturation) in [
            (-0.1, 1.0),
            (64.1, 1.0),
            (f32::NAN, 1.0),
            (0.0, -0.01),
            (0.0, 2.01),
            (0.0, f32::INFINITY),
        ] {
            assert!(ImageEffects { blur, saturation }.validate().is_err());
        }
        // a subnormal radius cannot produce a sigma and keeps the plain path.
        assert!(
            ImageEffects {
                blur: f32::from_bits(1),
                saturation: 1.0
            }
            .is_neutral()
        );
    }
}
