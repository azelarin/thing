use std::str::FromStr;

use crate::{Color, ParseColorError, TreeError};

/// a solid color or a gradient relative to its paint box.
#[derive(Clone, Debug, PartialEq)]
pub enum Brush {
    Solid(Color),
    Linear(LinearGradient),
    Radial(RadialGradient),
}

/// paint-box fractions used for positions and radii. for positions, (0, 0)
/// and (1, 1) are the box's corners.
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

/// an elliptical gradient. `center` is a paint box position and `radius`
/// holds the horizontal and vertical radii as fractions of the box's width and
/// height, so equal values form an ellipse on a rectangular box.
#[derive(Clone, Debug, PartialEq)]
pub struct RadialGradient {
    pub center: GradientPoint,
    pub radius: GradientPoint,
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
            Self::Radial(gradient) => gradient.validate(),
        }
    }

    pub fn is_transparent(&self) -> bool {
        match self {
            Self::Solid(color) => color.0[3] == 0,
            Self::Linear(gradient) => transparent(&gradient.stops),
            Self::Radial(gradient) => transparent(&gradient.stops),
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
        validate_stops(&self.stops)
    }
}

impl RadialGradient {
    pub fn validate(&self) -> Result<(), TreeError> {
        if !(self.center.x.is_finite() && self.center.y.is_finite()) {
            return Err(TreeError("gradient center must be finite".into()));
        }
        if !([self.radius.x, self.radius.y]
            .into_iter()
            .all(|radius| radius.is_finite() && radius > 0.0))
        {
            return Err(TreeError(
                "gradient radii must be finite and positive".into(),
            ));
        }
        validate_stops(&self.stops)
    }
}

fn validate_stops(stops: &[GradientStop]) -> Result<(), TreeError> {
    if !(2..=16).contains(&stops.len()) {
        return Err(TreeError("a gradient needs 2 through 16 stops".into()));
    }
    if stops
        .iter()
        .any(|stop| !stop.position.is_finite() || !(0.0..=1.0).contains(&stop.position))
        || stops
            .windows(2)
            .any(|stops| stops[0].position > stops[1].position)
    {
        return Err(TreeError(
            "gradient stop positions must be finite, ordered, and between 0 and 1".into(),
        ));
    }
    Ok(())
}

fn transparent(stops: &[GradientStop]) -> bool {
    stops.iter().all(|stop| stop.color.0[3] == 0)
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
    fn radial_gradients_need_finite_positive_radii_and_valid_stops() {
        let valid = RadialGradient {
            center: GradientPoint { x: -3.0, y: 0.5 },
            radius: GradientPoint {
                x: 4.0,
                y: 0.000_001,
            },
            stops: gradient().stops,
        };
        assert!(valid.validate().is_ok());
        for (center, radius) in [
            ((f64::NAN, 0.5), (0.5, 0.5)),
            ((0.5, f64::INFINITY), (0.5, 0.5)),
            ((0.5, 0.5), (0.0, 0.5)),
            ((0.5, 0.5), (0.5, -0.5)),
            ((0.5, 0.5), (f64::INFINITY, 0.5)),
            ((0.5, 0.5), (0.5, f64::NAN)),
        ] {
            let mut invalid = valid.clone();
            invalid.center = GradientPoint {
                x: center.0,
                y: center.1,
            };
            invalid.radius = GradientPoint {
                x: radius.0,
                y: radius.1,
            };
            assert!(invalid.validate().is_err(), "{center:?} {radius:?}");
        }
        let mut invalid = valid;
        invalid.stops.reverse();
        invalid.stops[0].position = 1.0;
        invalid.stops[1].position = 0.0;
        assert!(invalid.validate().is_err());
    }

    #[test]
    fn transparency_includes_every_stop() {
        let mut gradient = gradient();
        assert!(!Brush::Linear(gradient.clone()).is_transparent());
        gradient.stops[1].color = Color::TRANSPARENT;
        assert!(Brush::Linear(gradient).is_transparent());
    }
}
