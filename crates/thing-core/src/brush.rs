use std::str::FromStr;

use crate::{Color, ParseColorError, TreeError};

/// a solid color or a gradient relative to its paint box.
#[derive(Clone, Debug, PartialEq)]
pub enum Brush {
    Solid(Color),
    Linear(LinearGradient),
}

/// a position relative to the paint box, where (0, 0) and (1, 1) are its corners.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GradientPoint {
    pub x: f64,
    pub y: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct LinearGradient {
    pub from: GradientPoint,
    pub to: GradientPoint,
    pub stops: Vec<GradientStop>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GradientStop {
    pub position: f64,
    pub color: Color,
}

impl Default for Brush {
    fn default() -> Self {
        Self::Solid(Color::TRANSPARENT)
    }
}

impl From<Color> for Brush {
    fn from(color: Color) -> Self {
        Self::Solid(color)
    }
}

impl FromStr for Brush {
    type Err = ParseColorError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        value.parse().map(Self::Solid)
    }
}

impl Brush {
    pub fn validate(&self) -> Result<(), TreeError> {
        match self {
            Self::Solid(_) => Ok(()),
            Self::Linear(gradient) => gradient.validate(),
        }
    }

    pub fn is_transparent(&self) -> bool {
        match self {
            Self::Solid(color) => color.0[3] == 0,
            Self::Linear(gradient) => gradient.stops.iter().all(|stop| stop.color.0[3] == 0),
        }
    }
}

impl LinearGradient {
    pub fn validate(&self) -> Result<(), TreeError> {
        if ![self.from.x, self.from.y, self.to.x, self.to.y]
            .into_iter()
            .all(f64::is_finite)
            || self.from == self.to
        {
            return Err(TreeError(
                "gradient endpoints must be finite and different".into(),
            ));
        }
        if !(2..=16).contains(&self.stops.len()) {
            return Err(TreeError("a gradient needs 2 through 16 stops".into()));
        }
        if self
            .stops
            .iter()
            .any(|stop| !stop.position.is_finite() || !(0.0..=1.0).contains(&stop.position))
            || self
                .stops
                .windows(2)
                .any(|stops| stops[0].position > stops[1].position)
        {
            return Err(TreeError(
                "gradient stop positions must be finite, ordered, and between 0 and 1".into(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gradient() -> LinearGradient {
        LinearGradient {
            from: GradientPoint { x: -2.0, y: 0.0 },
            to: GradientPoint { x: 3.0, y: 1.0 },
            stops: vec![
                GradientStop {
                    position: 0.0,
                    color: Color::TRANSPARENT,
                },
                GradientStop {
                    position: 1.0,
                    color: Color::WHITE,
                },
            ],
        }
    }

    #[test]
    fn validates_stop_count_order_and_endpoints() {
        let valid = gradient();
        assert!(valid.validate().is_ok());
        let mut invalid = valid.clone();
        invalid.to = invalid.from;
        assert!(invalid.validate().is_err());
        invalid = valid.clone();
        invalid.from.x = f64::INFINITY;
        assert!(invalid.validate().is_err());
        for positions in [
            vec![0.0],
            vec![0.0; 17],
            vec![0.5, 0.25],
            vec![-0.1, 1.0],
            vec![0.0, f64::NAN],
        ] {
            let mut invalid = valid.clone();
            invalid.stops = positions
                .into_iter()
                .map(|position| GradientStop {
                    position,
                    color: Color::WHITE,
                })
                .collect();
            assert!(invalid.validate().is_err());
        }
        let mut hard_stop = valid;
        hard_stop.stops = vec![
            GradientStop {
                position: 0.5,
                color: Color::WHITE
            };
            4
        ];
        assert!(hard_stop.validate().is_ok());
    }

    #[test]
    fn transparency_includes_every_stop() {
        let mut gradient = gradient();
        assert!(!Brush::Linear(gradient.clone()).is_transparent());
        gradient.stops[1].color = Color::TRANSPARENT;
        assert!(Brush::Linear(gradient).is_transparent());
    }
}
