use crate::{TreeError, VectorSegment};

const MAX_SEGMENTS: usize = 4096;

pub(crate) fn normalize_path(data: &str) -> Result<Vec<VectorSegment>, TreeError> {
    let mut state = PathState::default();
    let mut output = Vec::new();
    let mut source_segments = 0_usize;
    for segment in svgtypes::PathParser::from(data) {
        let segment = segment.map_err(|error| TreeError(format!("invalid path data: {error}")))?;
        source_segments += 1;
        if source_segments > MAX_SEGMENTS {
            return Err(TreeError("path data exceeds 4,096 source segments".into()));
        }
        validate_source_segment(segment)?;
        state.apply(segment, &mut output)?;
    }
    if source_segments == 0 {
        return Err(TreeError(
            "path data must contain at least one command".into(),
        ));
    }
    Ok(output)
}

#[derive(Default)]
struct PathState {
    x: f64,
    y: f64,
    start_x: f64,
    start_y: f64,
    cubic_control: Option<(f64, f64)>,
    quadratic_control: Option<(f64, f64)>,
    after_close: bool,
}

impl PathState {
    fn apply(
        &mut self,
        segment: svgtypes::PathSegment,
        output: &mut Vec<VectorSegment>,
    ) -> Result<(), TreeError> {
        use svgtypes::{PathSegment, SimplePathSegment};

        if self.after_close
            && !matches!(
                segment,
                PathSegment::MoveTo { .. } | PathSegment::ClosePath { .. }
            )
        {
            push_segment(
                output,
                SimplePathSegment::MoveTo {
                    x: self.x,
                    y: self.y,
                },
            )?;
            self.after_close = false;
        }

        match segment {
            PathSegment::MoveTo { abs, x, y } => {
                let (x, y) = self.resolve(abs, x, y);
                push_segment(output, SimplePathSegment::MoveTo { x, y })?;
                self.x = x;
                self.y = y;
                self.start_x = x;
                self.start_y = y;
                self.after_close = false;
                self.reset_controls();
            }
            PathSegment::LineTo { abs, x, y } => {
                let (x, y) = self.resolve(abs, x, y);
                push_segment(output, SimplePathSegment::LineTo { x, y })?;
                self.finish_at(x, y);
            }
            PathSegment::HorizontalLineTo { abs, x } => {
                let x = if abs { x } else { self.x + x };
                push_segment(output, SimplePathSegment::LineTo { x, y: self.y })?;
                self.finish_at(x, self.y);
            }
            PathSegment::VerticalLineTo { abs, y } => {
                let y = if abs { y } else { self.y + y };
                push_segment(output, SimplePathSegment::LineTo { x: self.x, y })?;
                self.finish_at(self.x, y);
            }
            PathSegment::CurveTo {
                abs,
                x1,
                y1,
                x2,
                y2,
                x,
                y,
            } => {
                let (x1, y1) = self.resolve(abs, x1, y1);
                let (x2, y2) = self.resolve(abs, x2, y2);
                let (x, y) = self.resolve(abs, x, y);
                push_segment(
                    output,
                    SimplePathSegment::CurveTo {
                        x1,
                        y1,
                        x2,
                        y2,
                        x,
                        y,
                    },
                )?;
                self.x = x;
                self.y = y;
                self.cubic_control = Some((x2, y2));
                self.quadratic_control = None;
            }
            PathSegment::SmoothCurveTo { abs, x2, y2, x, y } => {
                let (x1, y1) = self
                    .cubic_control
                    .map(|(x, y)| (2.0 * self.x - x, 2.0 * self.y - y))
                    .unwrap_or((self.x, self.y));
                let (x2, y2) = self.resolve(abs, x2, y2);
                let (x, y) = self.resolve(abs, x, y);
                push_segment(
                    output,
                    SimplePathSegment::CurveTo {
                        x1,
                        y1,
                        x2,
                        y2,
                        x,
                        y,
                    },
                )?;
                self.x = x;
                self.y = y;
                self.cubic_control = Some((x2, y2));
                self.quadratic_control = None;
            }
            PathSegment::Quadratic { abs, x1, y1, x, y } => {
                let (x1, y1) = self.resolve(abs, x1, y1);
                let (x, y) = self.resolve(abs, x, y);
                push_segment(output, SimplePathSegment::Quadratic { x1, y1, x, y })?;
                self.x = x;
                self.y = y;
                self.cubic_control = None;
                self.quadratic_control = Some((x1, y1));
            }
            PathSegment::SmoothQuadratic { abs, x, y } => {
                let (x1, y1) = self
                    .quadratic_control
                    .map(|(x, y)| (2.0 * self.x - x, 2.0 * self.y - y))
                    .unwrap_or((self.x, self.y));
                let (x, y) = self.resolve(abs, x, y);
                push_segment(output, SimplePathSegment::Quadratic { x1, y1, x, y })?;
                self.x = x;
                self.y = y;
                self.cubic_control = None;
                self.quadratic_control = Some((x1, y1));
            }
            PathSegment::EllipticalArc {
                abs,
                rx,
                ry,
                x_axis_rotation,
                large_arc,
                sweep,
                x,
                y,
            } => {
                let (x, y) = self.resolve(abs, x, y);
                append_arc(
                    output,
                    (self.x, self.y),
                    ArcCommand {
                        to: (x, y),
                        radii: (rx, ry),
                        rotation_degrees: x_axis_rotation,
                        large_arc,
                        sweep,
                    },
                )?;
                self.finish_at(x, y);
            }
            PathSegment::ClosePath { .. } => {
                if !self.after_close {
                    push_segment(output, SimplePathSegment::ClosePath)?;
                }
                self.x = self.start_x;
                self.y = self.start_y;
                self.after_close = true;
                self.reset_controls();
            }
        }
        Ok(())
    }

    fn resolve(&self, absolute: bool, x: f64, y: f64) -> (f64, f64) {
        if absolute {
            (x, y)
        } else {
            (self.x + x, self.y + y)
        }
    }

    fn finish_at(&mut self, x: f64, y: f64) {
        self.x = x;
        self.y = y;
        self.reset_controls();
    }

    fn reset_controls(&mut self) {
        self.cubic_control = None;
        self.quadratic_control = None;
    }
}

fn push_segment(
    output: &mut Vec<VectorSegment>,
    segment: svgtypes::SimplePathSegment,
) -> Result<(), TreeError> {
    if output.len() >= MAX_SEGMENTS {
        return Err(TreeError(
            "path data exceeds 4,096 normalized segments".into(),
        ));
    }
    output.push(normalize_segment(segment)?);
    Ok(())
}

struct ArcCommand {
    to: (f64, f64),
    radii: (f64, f64),
    rotation_degrees: f64,
    large_arc: bool,
    sweep: bool,
}

fn append_arc(
    output: &mut Vec<VectorSegment>,
    from: (f64, f64),
    command: ArcCommand,
) -> Result<(), TreeError> {
    use svgtypes::SimplePathSegment;

    let ArcCommand {
        to,
        radii,
        rotation_degrees,
        large_arc,
        sweep,
    } = command;
    if from == to {
        return Ok(());
    }
    if radii.0 == 0.0 || radii.1 == 0.0 {
        return push_segment(output, SimplePathSegment::LineTo { x: to.0, y: to.1 });
    }

    let radii = corrected_arc_radii(from, to, radii, rotation_degrees)?;
    let factor = 1.0 / radii.0.min(radii.1);
    let values = [
        (to.0 - from.0) * factor,
        (to.1 - from.1) * factor,
        radii.0 * factor,
        radii.1 * factor,
    ];
    if !factor.is_finite()
        || values
            .into_iter()
            .any(|value| !value.is_finite() || value.abs() > 1.0e150)
    {
        return Err(TreeError("path arc exceeds numeric limits".into()));
    }
    let rotation = rotation_degrees.to_radians();
    let (sin, cos) = (rotation % std::f64::consts::TAU).sin_cos();
    let half_x = values[0] * 0.5;
    let half_y = values[1] * 0.5;
    let projected_x = cos * half_x + sin * half_y;
    let projected_y = -sin * half_x + cos * half_y;
    let weighted_x = values[2] * projected_y;
    let weighted_y = values[3] * projected_x;
    let chord = weighted_x * weighted_x + weighted_y * weighted_y;
    if !chord.is_finite() {
        return Err(TreeError("path arc exceeds numeric limits".into()));
    }
    // kurbo requires this weighted chord to be nonzero. if it underflows,
    // the endpoints are already identical at the path's f32 precision.
    if chord == 0.0 {
        return Ok(());
    }

    let arc = kurbo::Arc::from_svg_arc(&kurbo::SvgArc {
        from: kurbo::Point::ZERO,
        to: kurbo::Point::new(values[0], values[1]),
        radii: kurbo::Vec2::new(values[2], values[3]),
        x_rotation: rotation,
        large_arc,
        sweep,
    })
    .ok_or_else(|| TreeError("path arc could not be normalized".into()))?;
    let tolerance = values[2].max(values[3]) * 0.0001;
    let remaining = MAX_SEGMENTS.saturating_sub(output.len());
    let mut curves = Vec::new();
    let mut exceeded_limit = false;
    arc.to_cubic_beziers(tolerance, |p1, p2, point| {
        if curves.len() < remaining {
            curves.push((p1, p2, point));
        } else {
            exceeded_limit = true;
        }
    });
    if exceeded_limit {
        return Err(TreeError(
            "path data exceeds 4,096 normalized segments".into(),
        ));
    }
    for (p1, p2, point) in curves {
        push_segment(
            output,
            SimplePathSegment::CurveTo {
                x1: p1.x / factor + from.0,
                y1: p1.y / factor + from.1,
                x2: p2.x / factor + from.0,
                y2: p2.y / factor + from.1,
                x: point.x / factor + from.0,
                y: point.y / factor + from.1,
            },
        )?;
    }
    Ok(())
}

fn corrected_arc_radii(
    from: (f64, f64),
    to: (f64, f64),
    radii: (f64, f64),
    rotation_degrees: f64,
) -> Result<(f64, f64), TreeError> {
    let angle = rotation_degrees.to_radians();
    let (sin, cos) = angle.sin_cos();
    let half_x = (from.0 - to.0) / 2.0;
    let half_y = (from.1 - to.1) / 2.0;
    let x = cos * half_x + sin * half_y;
    let y = -sin * half_x + cos * half_y;
    let log_x = if x == 0.0 {
        f64::NEG_INFINITY
    } else {
        x.abs().ln() - radii.0.ln()
    };
    let log_y = if y == 0.0 {
        f64::NEG_INFINITY
    } else {
        y.abs().ln() - radii.1.ln()
    };
    // svg expands radii that cannot reach both endpoints. keeping this in the
    // logarithmic domain avoids overflow before the corrected values are checked.
    let max = log_x.max(log_y);
    let log_scale = max + 0.5 * ((2.0 * (log_x - max)).exp() + (2.0 * (log_y - max)).exp()).ln();
    let scale = log_scale.max(0.0);
    let radii = ((radii.0.ln() + scale).exp(), (radii.1.ln() + scale).exp());
    if radii.0.is_finite() && radii.1.is_finite() && radii.0 > 0.0 && radii.1 > 0.0 {
        Ok(radii)
    } else {
        Err(TreeError("path arc exceeds numeric limits".into()))
    }
}

fn validate_source_segment(segment: svgtypes::PathSegment) -> Result<(), TreeError> {
    use svgtypes::PathSegment;

    let mut values = [0.0; 7];
    let count = match segment {
        PathSegment::MoveTo { x, y, .. } | PathSegment::LineTo { x, y, .. } => {
            values[..2].copy_from_slice(&[x, y]);
            2
        }
        PathSegment::HorizontalLineTo { x, .. } => {
            values[0] = x;
            1
        }
        PathSegment::VerticalLineTo { y, .. } => {
            values[0] = y;
            1
        }
        PathSegment::CurveTo {
            x1,
            y1,
            x2,
            y2,
            x,
            y,
            ..
        } => {
            values[..6].copy_from_slice(&[x1, y1, x2, y2, x, y]);
            6
        }
        PathSegment::SmoothCurveTo { x2, y2, x, y, .. }
        | PathSegment::Quadratic {
            x1: x2,
            y1: y2,
            x,
            y,
            ..
        } => {
            values[..4].copy_from_slice(&[x2, y2, x, y]);
            4
        }
        PathSegment::SmoothQuadratic { x, y, .. } => {
            values[..2].copy_from_slice(&[x, y]);
            2
        }
        PathSegment::EllipticalArc {
            rx,
            ry,
            x_axis_rotation,
            x,
            y,
            ..
        } => {
            if rx < 0.0
                || ry < 0.0
                || !rx.is_finite()
                || !ry.is_finite()
                || rx > 100_000.0
                || ry > 100_000.0
            {
                return Err(TreeError(
                    "path arc radii must be finite and between 0 and 100,000".into(),
                ));
            }
            values[..5].copy_from_slice(&[rx, ry, x_axis_rotation, x, y]);
            5
        }
        PathSegment::ClosePath { .. } => 0,
    };
    if values[..count].iter().any(|value| !value.is_finite()) {
        return Err(TreeError("path values must be finite".into()));
    }
    Ok(())
}

fn normalize_segment(segment: svgtypes::SimplePathSegment) -> Result<VectorSegment, TreeError> {
    use svgtypes::SimplePathSegment;

    let normalized = match segment {
        SimplePathSegment::MoveTo { x, y } => {
            validate_coordinates(&[x, y])?;
            VectorSegment::MoveTo {
                x: x as f32,
                y: y as f32,
            }
        }
        SimplePathSegment::LineTo { x, y } => {
            validate_coordinates(&[x, y])?;
            VectorSegment::LineTo {
                x: x as f32,
                y: y as f32,
            }
        }
        SimplePathSegment::CurveTo {
            x1,
            y1,
            x2,
            y2,
            x,
            y,
        } => {
            validate_coordinates(&[x1, y1, x2, y2, x, y])?;
            VectorSegment::CurveTo {
                x1: x1 as f32,
                y1: y1 as f32,
                x2: x2 as f32,
                y2: y2 as f32,
                x: x as f32,
                y: y as f32,
            }
        }
        SimplePathSegment::Quadratic { x1, y1, x, y } => {
            validate_coordinates(&[x1, y1, x, y])?;
            VectorSegment::Quadratic {
                x1: x1 as f32,
                y1: y1 as f32,
                x: x as f32,
                y: y as f32,
            }
        }
        SimplePathSegment::ClosePath => VectorSegment::Close,
    };
    Ok(normalized)
}

fn validate_coordinates(values: &[f64]) -> Result<(), TreeError> {
    if values
        .iter()
        .any(|value| !value.is_finite() || !(-100_000.0..=100_000.0).contains(value))
    {
        return Err(TreeError(
            "path coordinates must be finite and between -100,000 and 100,000".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smooth_commands_reflect_the_previous_control_point() {
        let path = normalize_path("M 10 10 c 1 0 2 1 3 1 s 2 1 3 0 q 1 1 2 0 t 2 0").unwrap();
        assert_eq!(
            path[2],
            VectorSegment::CurveTo {
                x1: 14.0,
                y1: 11.0,
                x2: 15.0,
                y2: 12.0,
                x: 16.0,
                y: 11.0,
            }
        );
        assert_eq!(
            path[4],
            VectorSegment::Quadratic {
                x1: 19.0,
                y1: 10.0,
                x: 20.0,
                y: 11.0,
            }
        );
    }

    #[test]
    fn a_command_after_close_starts_at_the_subpath_origin() {
        let path = normalize_path("M 1 1 L 2 1 Z l 0 1").unwrap();
        assert_eq!(path[2], VectorSegment::Close);
        assert_eq!(path[3], VectorSegment::MoveTo { x: 1.0, y: 1.0 });
        assert_eq!(path[4], VectorSegment::LineTo { x: 1.0, y: 2.0 });
    }

    #[test]
    fn microscopic_arcs_remain_curves() {
        let path = normalize_path("M 0 0 A 0.000001 0.000001 0 0 1 0.000001 0.000001").unwrap();
        assert!(
            path[1..]
                .iter()
                .all(|segment| matches!(segment, VectorSegment::CurveTo { .. }))
        );
        assert!(path.len() > 1);
        assert!(matches!(
            path.last(),
            Some(VectorSegment::CurveTo { x, y, .. })
                if (*x - 0.000001).abs() < 1.0e-10 && (*y - 0.000001).abs() < 1.0e-10
        ));
    }

    #[test]
    fn degenerate_arcs_follow_svg_endpoint_rules() {
        let line = normalize_path("M 1 1 A 0 2 0 0 1 4 5").unwrap();
        assert_eq!(line[1], VectorSegment::LineTo { x: 4.0, y: 5.0 });

        let omitted = normalize_path("M 1 1 A 2 2 0 1 1 1 1").unwrap();
        assert_eq!(omitted, vec![VectorSegment::MoveTo { x: 1.0, y: 1.0 }]);
    }
}
