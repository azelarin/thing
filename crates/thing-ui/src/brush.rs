use thing_core::{
    Brush, Color, GradientStop, Kind, LinearGradient, Point, RadialGradient, Rect, Style,
};

const UNREPRESENTABLE: &str = "gradient geometry cannot be represented by the renderer";
/// maximum sampled error in local pixels, before ancestor transforms.
const RADIAL_PLACEMENT_TOLERANCE: f64 = 0.25;

/// the unit shader each resolved gradient expects. a linear gradient runs
/// from (0, 0) to (1, 0); a radial gradient is a circle of radius one at the
/// origin. `local_matrix` maps either onto the paint box.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GradientShape {
    Linear,
    Radial,
}

pub(crate) struct ResolvedGradient {
    pub(crate) shape: GradientShape,
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
        resolve(&node.style.background, bounds)?;
        if node.kind == Kind::Path
            && let Some(fill) = &node.path.fill
        {
            resolve(fill, path_area(bounds, &node.style))?;
        }
        Ok(())
    };
    check().map_err(|error: String| format!("gradient on widget {}: {error}", node.id.0))
}

/// resolves a gradient brush for its paint box. a solid brush, or a box with
/// no area, needs no shader.
pub(crate) fn resolve(brush: &Brush, bounds: Rect) -> Result<Option<ResolvedGradient>, String> {
    match brush {
        Brush::Solid(_) => Ok(None),
        Brush::Linear(gradient) => resolve_linear(gradient, bounds),
        Brush::Radial(gradient) => resolve_radial(gradient, bounds),
    }
}

fn resolve_linear(
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
        return Err(UNREPRESENTABLE.into());
    }
    let unit = [dx * width / length, dy * height / length];
    let extent = unit[0].abs() * width / 2.0 + unit[1].abs() * height / 2.0;
    let from = renderer_point(center[0] - unit[0] * extent, center[1] - unit[1] * extent)?;
    let to = renderer_point(center[0] + unit[0] * extent, center[1] + unit[1] * extent)?;
    let renderer_length = (to.x - from.x).hypot(to.y - from.y);
    if !renderer_length.is_finite() || renderer_length == 0.0 {
        return Err(UNREPRESENTABLE.into());
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
        return Err(UNREPRESENTABLE.into());
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
        return Err(UNREPRESENTABLE.into());
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
        shape: GradientShape::Linear,
        local_matrix,
        colors,
        positions,
    }))
}

fn resolve_radial(
    gradient: &RadialGradient,
    bounds: Rect,
) -> Result<Option<ResolvedGradient>, String> {
    gradient.validate().map_err(|error| error.to_string())?;
    if bounds.width <= 0.0 || bounds.height <= 0.0 {
        return Ok(None);
    }
    let width = f64::from(bounds.width);
    let height = f64::from(bounds.height);
    let center = [
        f64::from(bounds.x) + gradient.center.x * width,
        f64::from(bounds.y) + gradient.center.y * height,
    ];
    let radius = [gradient.radius.x * width, gradient.radius.y * height];
    let renderer_center = renderer_point(center[0], center[1])?;
    let renderer_radius = radius.map(|radius| radius as f32);
    // a radius that rounds to zero, or to a subnormal whose reciprocal
    // overflows, would leave skia a degenerate shader.
    if !radius.iter().all(|radius| radius.is_finite())
        || !renderer_radius.iter().all(|radius| radius.is_normal())
    {
        return Err("gradient radius cannot be represented by the renderer".into());
    }
    let local_matrix = skia_safe::Matrix::new_all(
        renderer_radius[0],
        0.0,
        renderer_center.x,
        0.0,
        renderer_radius[1],
        renderer_center.y,
        0.0,
        0.0,
        1.0,
    );
    let inverse = local_matrix
        .invert()
        .filter(|matrix| matrix.is_finite())
        .ok_or(UNREPRESENTABLE)?;
    check_radial_placement(&inverse, center, radius, bounds)?;
    Ok(Some(ResolvedGradient {
        shape: GradientShape::Radial,
        local_matrix,
        colors: gradient.stops.iter().map(shader_color).collect(),
        positions: gradient
            .stops
            .iter()
            .map(|stop| stop.position as f32)
            .collect(),
    }))
}

/// skia maps each pixel through the inverse local matrix in f32. a center far
/// from its box, or a huge radius, can leave too few bits for the box's place
/// on the gradient, so the box would quietly show the wrong rings. compare the
/// matrix the shader uses with the f64 geometry at the box's corners, center,
/// and nearest point to the gradient center.
fn check_radial_placement(
    inverse: &skia_safe::Matrix,
    center: [f64; 2],
    radius: [f64; 2],
    bounds: Rect,
) -> Result<(), String> {
    let left = f64::from(bounds.x);
    let top = f64::from(bounds.y);
    let right = left + f64::from(bounds.width);
    let bottom = top + f64::from(bounds.height);
    let distance = |point: [f32; 2]| {
        ((f64::from(point[0]) - center[0]) / radius[0])
            .hypot((f64::from(point[1]) - center[1]) / radius[1])
    };
    let rendered = |point: [f32; 2]| {
        let mapped = inverse.map_point((point[0], point[1]));
        f64::from(mapped.x).hypot(f64::from(mapped.y))
    };
    let nearest = [
        center[0].clamp(left, right) as f32,
        center[1].clamp(top, bottom) as f32,
    ];
    // past the outermost ring the clamped gradient has one color, so a box
    // wholly outside it cannot show a misplaced ring.
    if distance(nearest) > 1.0 && rendered(nearest) > 1.0 {
        return Ok(());
    }
    let samples = [
        [left, top],
        [right, top],
        [left, bottom],
        [right, bottom],
        [(left + right) / 2.0, (top + bottom) / 2.0],
    ]
    .map(|point| point.map(|value| value as f32));
    let scale = radius[0].max(radius[1]);
    for point in samples.into_iter().chain([nearest]) {
        let rendered = rendered(point);
        if !rendered.is_finite()
            || (rendered - distance(point)).abs() * scale > RADIAL_PLACEMENT_TOLERANCE
        {
            return Err(
                "gradient center or radius is too far from its paint box for the renderer".into(),
            );
        }
    }
    Ok(())
}

fn shader_color(stop: &GradientStop) -> [f32; 4] {
    unpremultiplied(linear_premultiplied(stop.color))
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
        let resolved = resolve_linear(
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
        assert!(
            resolve_linear(&gradient, Rect::default())
                .unwrap()
                .is_none()
        );
        gradient.to.x = f64::MAX;
        assert!(
            resolve_linear(
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
        let resolved = resolve_linear(
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

    fn radial(center: (f64, f64), radius: (f64, f64)) -> RadialGradient {
        RadialGradient {
            center: GradientPoint {
                x: center.0,
                y: center.1,
            },
            radius: GradientPoint {
                x: radius.0,
                y: radius.1,
            },
            stops: vec![
                GradientStop {
                    position: 0.0,
                    color: Color::WHITE,
                },
                GradientStop {
                    position: 1.0,
                    color: Color::TRANSPARENT,
                },
            ],
        }
    }

    fn sized(width: f32, height: f32) -> Rect {
        Rect {
            x: 10.0,
            y: 20.0,
            width,
            height,
        }
    }

    #[test]
    fn radial_geometry_resolves_to_a_scaled_unit_circle() {
        let resolved = resolve_radial(&radial((0.5, 0.5), (0.5, 0.5)), sized(200.0, 100.0))
            .unwrap()
            .unwrap();
        assert_eq!(resolved.shape, GradientShape::Radial);
        // equal proportions on a rectangular box give a 100 by 50 ellipse.
        let matrix = resolved.local_matrix;
        assert_eq!((matrix.scale_x(), matrix.scale_y()), (100.0, 50.0));
        assert_eq!((matrix.translate_x(), matrix.translate_y()), (110.0, 70.0));
        assert_eq!(resolved.positions, [0.0, 1.0]);
        assert_eq!(resolved.colors[1], [0.0; 4]);
        assert!(
            resolve_radial(&radial((0.5, 0.5), (0.5, 0.5)), sized(0.0, 10.0))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn radial_representability_follows_the_matrix_the_shader_uses() {
        // a tiny widget and a hairline radius still have a usable matrix.
        for (radius, size) in [
            ((0.5, 0.5), (0.000_01, 0.000_01)),
            ((0.5, 1e-9), (100.0, 100.0)),
        ] {
            assert!(
                resolve_radial(&radial((0.5, 0.5), radius), sized(size.0, size.1))
                    .unwrap()
                    .is_some(),
                "{radius:?} {size:?}"
            );
        }
        // radii that round to zero or a subnormal would be degenerate.
        for radius in [(1e-50, 0.5), (0.5, 1e-42)] {
            let error = resolve_radial(&radial((0.5, 0.5), radius), sized(100.0, 100.0))
                .err()
                .unwrap();
            assert!(error.contains("radius cannot be represented"), "{error}");
        }
        // an overflowing radius or center is rejected rather than clamped.
        for (center, radius) in [((0.5, 0.5), (1e40, 0.5)), ((1e40, 0.5), (0.5, 0.5))] {
            assert!(resolve_radial(&radial(center, radius), sized(100.0, 100.0)).is_err());
        }
    }

    #[test]
    fn distant_centers_keep_their_rings_or_report_that_they_cannot() {
        // a center ten boxes away whose ring reaches the box is placed exactly.
        assert!(
            resolve_radial(&radial((-10.0, 0.5), (10.5, 10.5)), sized(100.0, 100.0))
                .unwrap()
                .is_some()
        );
        // ten million boxes away, f32 can no longer say where the ring meets
        // the box.
        let error = resolve_radial(
            &radial((-10_000_000.0, 0.5), (10_000_000.5, 10_000_000.5)),
            sized(100.0, 100.0),
        )
        .err()
        .unwrap();
        assert!(error.contains("too far from its paint box"), "{error}");
        // a box wholly past the outermost ring shows only the last color.
        assert!(
            resolve_radial(
                &radial((-10_000_000.0, 0.5), (0.5, 0.5)),
                sized(100.0, 100.0)
            )
            .unwrap()
            .is_some()
        );
    }
}
