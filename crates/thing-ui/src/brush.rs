use thing_core::{Brush, Color, Kind, LinearGradient, Point, Rect, Style};

pub(crate) struct ResolvedGradient {
    pub(crate) local_matrix: skia_safe::Matrix,
    pub(crate) colors: Vec<[f32; 4]>,
    pub(crate) positions: Vec<f32>,
}

struct ProjectedStop {
    distance: f64,
    color: [f64; 4],
}

pub(crate) fn path_area(bounds: Rect, style: &Style) -> Rect {
    let x = style.padding_x.unwrap_or(style.padding);
    let y = style.padding_y.unwrap_or(style.padding);
    Rect {
        x: bounds.x + x,
        y: bounds.y + y,
        width: (bounds.width - 2.0 * x).max(0.0),
        height: (bounds.height - 2.0 * y).max(0.0),
    }
}

pub(crate) fn validate(node: &thing_core::Node, bounds: Rect) -> Result<(), String> {
    let check = || {
        if let Brush::Linear(gradient) = &node.style.background {
            resolve(gradient, bounds)?;
        }
        if node.kind == Kind::Path
            && let Some(Brush::Linear(gradient)) = &node.path.fill
        {
            resolve(gradient, path_area(bounds, &node.style))?;
        }
        Ok(())
    };
    check().map_err(|error: String| format!("gradient on widget {}: {error}", node.id.0))
}

pub(crate) fn resolve(
    gradient: &LinearGradient,
    bounds: Rect,
) -> Result<Option<ResolvedGradient>, String> {
    gradient.validate().map_err(|error| error.to_string())?;
    if bounds.width <= 0.0 || bounds.height <= 0.0 {
        return Ok(None);
    }
    let width = f64::from(bounds.width);
    let height = f64::from(bounds.height);
    let center = [
        f64::from(bounds.x) + width / 2.0,
        f64::from(bounds.y) + height / 2.0,
    ];
    let dx = gradient.to.x - gradient.from.x;
    let dy = gradient.to.y - gradient.from.y;
    let length = (dx * width).hypot(dy * height);
    if !length.is_finite() || length == 0.0 {
        return Err("gradient geometry cannot be represented by the renderer".into());
    }
    let unit = [dx * width / length, dy * height / length];
    let extent = unit[0].abs() * width / 2.0 + unit[1].abs() * height / 2.0;
    let from = renderer_point(center[0] - unit[0] * extent, center[1] - unit[1] * extent)?;
    let to = renderer_point(center[0] + unit[0] * extent, center[1] + unit[1] * extent)?;
    let renderer_length = (to.x - from.x).hypot(to.y - from.y);
    if !renderer_length.is_finite() || renderer_length == 0.0 {
        return Err("gradient geometry cannot be represented by the renderer".into());
    }
    // a unit shader avoids skia's degenerate-gradient cutoff when a tiny
    // logical widget becomes visible through an ancestor's scale.
    let local_matrix = skia_safe::Matrix::new_all(
        to.x - from.x,
        -unit[1] as f32,
        from.x,
        to.y - from.y,
        unit[0] as f32,
        from.y,
        0.0,
        0.0,
        1.0,
    );
    if !local_matrix
        .invert()
        .is_some_and(|matrix| matrix.is_finite())
    {
        return Err("gradient geometry cannot be represented by the renderer".into());
    }
    let projected = gradient
        .stops
        .iter()
        .map(|stop| {
            let position = stop.position;
            // cancel far endpoints before multiplying by the paint box dimensions.
            let x = f64::from(bounds.x) + (gradient.from.x + position * dx) * width;
            let y = f64::from(bounds.y) + (gradient.from.y + position * dy) * height;
            ProjectedStop {
                distance: (x - center[0]) * unit[0] + (y - center[1]) * unit[1],
                color: linear_premultiplied(stop.color),
            }
        })
        .collect::<Vec<_>>();
    if projected.iter().any(|stop| !stop.distance.is_finite()) {
        return Err("gradient geometry cannot be represented by the renderer".into());
    }
    let mut colors = Vec::with_capacity(projected.len() + 2);
    let mut positions = Vec::with_capacity(projected.len() + 2);
    colors.push(unpremultiplied(sample(&projected, -extent)));
    positions.push(0.0);
    for stop in &projected {
        if (-extent..=extent).contains(&stop.distance) {
            colors.push(unpremultiplied(stop.color));
            positions.push(((stop.distance + extent) / (2.0 * extent)).clamp(0.0, 1.0) as f32);
        }
    }
    colors.push(unpremultiplied(sample(&projected, extent)));
    positions.push(1.0);
    Ok(Some(ResolvedGradient {
        local_matrix,
        colors,
        positions,
    }))
}

fn renderer_point(x: f64, y: f64) -> Result<Point, String> {
    let point = Point {
        x: x as f32,
        y: y as f32,
    };
    if !x.is_finite() || !y.is_finite() || !point.x.is_finite() || !point.y.is_finite() {
        return Err("gradient paint box cannot be represented by the renderer".into());
    }
    Ok(point)
}

fn sample(stops: &[ProjectedStop], distance: f64) -> [f64; 4] {
    if distance < stops[0].distance {
        return stops[0].color;
    }
    for pair in stops.windows(2) {
        if distance < pair[1].distance {
            let progress = ((distance - pair[0].distance) / (pair[1].distance - pair[0].distance))
                .clamp(0.0, 1.0);
            return std::array::from_fn(|index| {
                pair[0].color[index] + (pair[1].color[index] - pair[0].color[index]) * progress
            });
        }
    }
    stops.last().expect("validated gradient has stops").color
}

fn linear_premultiplied(color: Color) -> [f64; 4] {
    let alpha = f64::from(color.0[3]) / 255.0;
    let linear = |value: u8| {
        let value = f64::from(value) / 255.0;
        if value <= 0.04045 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    };
    [
        linear(color.0[0]) * alpha,
        linear(color.0[1]) * alpha,
        linear(color.0[2]) * alpha,
        alpha,
    ]
}

fn unpremultiplied(color: [f64; 4]) -> [f32; 4] {
    if color[3] == 0.0 {
        return [0.0; 4];
    }
    [
        (color[0] / color[3]) as f32,
        (color[1] / color[3]) as f32,
        (color[2] / color[3]) as f32,
        color[3] as f32,
    ]
}

#[cfg(test)]
mod tests {
    use thing_core::{GradientPoint, GradientStop};

    use super::*;

    #[test]
    fn distant_endpoints_keep_a_centered_hard_stop() {
        let gradient = LinearGradient {
            from: GradientPoint {
                x: -100_000_000.0,
                y: 0.0,
            },
            to: GradientPoint {
                x: 100_000_001.0,
                y: 0.0,
            },
            stops: vec![
                GradientStop {
                    position: 0.0,
                    color: Color([0, 0, 0, 255]),
                },
                GradientStop {
                    position: 0.5,
                    color: Color([0, 0, 0, 255]),
                },
                GradientStop {
                    position: 0.5,
                    color: Color::WHITE,
                },
                GradientStop {
                    position: 1.0,
                    color: Color::WHITE,
                },
            ],
        };
        let resolved = resolve(
            &gradient,
            Rect {
                width: 100.0,
                height: 20.0,
                ..Rect::default()
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(resolved.positions, [0.0, 0.5, 0.5, 1.0]);
    }

    #[test]
    fn zero_size_skips_paint_and_unrepresentable_geometry_is_rejected() {
        let mut gradient = LinearGradient {
            from: GradientPoint { x: 0.0, y: 0.0 },
            to: GradientPoint { x: 1.0, y: 0.0 },
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
        };
        assert!(resolve(&gradient, Rect::default()).unwrap().is_none());
        gradient.to.x = f64::MAX;
        assert!(
            resolve(
                &gradient,
                Rect {
                    width: 100.0,
                    height: 20.0,
                    ..Rect::default()
                }
            )
            .is_err()
        );
    }

    #[test]
    fn distant_endpoints_preserve_a_narrow_visible_transition() {
        let gradient = LinearGradient {
            from: GradientPoint {
                x: -100_000_000.0,
                y: 0.0,
            },
            to: GradientPoint {
                x: 100_000_001.0,
                y: 0.0,
            },
            stops: vec![
                GradientStop {
                    position: 0.5 - 0.000_000_001,
                    color: Color([0, 0, 0, 255]),
                },
                GradientStop {
                    position: 0.5 + 0.000_000_001,
                    color: Color::WHITE,
                },
            ],
        };
        let resolved = resolve(
            &gradient,
            Rect {
                width: 100.0,
                height: 20.0,
                ..Rect::default()
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(resolved.positions.len(), 4);
        assert!((resolved.positions[1] - 0.3).abs() < 0.000_01);
        assert!((resolved.positions[2] - 0.7).abs() < 0.000_01);
    }
}
