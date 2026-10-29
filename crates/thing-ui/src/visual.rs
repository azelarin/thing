use thing_core::{Point, Rect, Style};

const RENDER_LIMIT: f64 = f32::MAX as f64;

/// a finite affine transform from widget-local logical pixels to surface space.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Affine {
    pub xx: f64,
    pub yx: f64,
    pub xy: f64,
    pub yy: f64,
    pub tx: f64,
    pub ty: f64,
}

impl Default for Affine {
    fn default() -> Self {
        Self::IDENTITY
    }
}

impl Affine {
    pub const IDENTITY: Self = Self {
        xx: 1.0,
        yx: 0.0,
        xy: 0.0,
        yy: 1.0,
        tx: 0.0,
        ty: 0.0,
    };

    pub fn translation(x: f64, y: f64) -> Self {
        Self {
            tx: x,
            ty: y,
            ..Self::IDENTITY
        }
    }

    fn scale(x: f64, y: f64) -> Self {
        Self {
            xx: x,
            yy: y,
            ..Self::IDENTITY
        }
    }

    fn rotation(degrees: f64) -> Self {
        let radians = degrees.to_radians();
        let (sin, cos) = radians.sin_cos();
        Self {
            xx: cos,
            yx: sin,
            xy: -sin,
            yy: cos,
            tx: 0.0,
            ty: 0.0,
        }
    }

    /// compose `self * next`, applying `next` first.
    pub fn compose(self, next: Self) -> Result<Self, String> {
        let result = Self {
            xx: self.xx * next.xx + self.xy * next.yx,
            yx: self.yx * next.xx + self.yy * next.yx,
            xy: self.xx * next.xy + self.xy * next.yy,
            yy: self.yx * next.xy + self.yy * next.yy,
            tx: self.xx * next.tx + self.xy * next.ty + self.tx,
            ty: self.yx * next.tx + self.yy * next.ty + self.ty,
        };
        result.validate()?;
        Ok(result)
    }

    pub fn widget(style: &Style, width: f32, height: f32) -> Result<Self, String> {
        let origin = style.transform_origin;
        let pivot_x = f64::from(width) * f64::from(origin.x) + f64::from(origin.offset_x);
        let pivot_y = f64::from(height) * f64::from(origin.y) + f64::from(origin.offset_y);
        let mut result =
            Self::translation(f64::from(style.translate_x), f64::from(style.translate_y));
        result = result.compose(Self::translation(pivot_x, pivot_y))?;
        result = result.compose(Self::rotation(f64::from(style.rotation)))?;
        result = result.compose(Self::scale(
            f64::from(style.scale_x),
            f64::from(style.scale_y),
        ))?;
        result.compose(Self::translation(-pivot_x, -pivot_y))
    }

    pub fn inverse(self) -> Result<Option<Self>, String> {
        let determinant = self.xx * self.yy - self.xy * self.yx;
        if determinant == 0.0 {
            return Ok(None);
        }
        let inverse = Self {
            xx: self.yy / determinant,
            yx: -self.yx / determinant,
            xy: -self.xy / determinant,
            yy: self.xx / determinant,
            tx: (self.xy * self.ty - self.yy * self.tx) / determinant,
            ty: (self.yx * self.tx - self.xx * self.ty) / determinant,
        };
        inverse.validate()?;
        Ok(Some(inverse))
    }

    pub fn point(self, point: Point) -> Option<Point> {
        self.point_f64(f64::from(point.x), f64::from(point.y))
    }

    pub fn point_f64(self, x: f64, y: f64) -> Option<Point> {
        let surface_x = self.xx * x + self.xy * y + self.tx;
        let surface_y = self.yx * x + self.yy * y + self.ty;
        representable_f32(surface_x)
            .zip(representable_f32(surface_y))
            .map(|(x, y)| Point { x, y })
    }

    pub fn rect_aabb(self, rect: Rect) -> Result<Rect, String> {
        let x1 = f64::from(rect.x);
        let y1 = f64::from(rect.y);
        let x2 = x1 + f64::from(rect.width);
        let y2 = y1 + f64::from(rect.height);
        let points = [
            self.point_f64(x1, y1),
            self.point_f64(x2, y1),
            self.point_f64(x1, y2),
            self.point_f64(x2, y2),
        ];
        let [Some(a), Some(b), Some(c), Some(d)] = points else {
            return Err("visual transform exceeds the renderer coordinate range".into());
        };
        let left = a.x.min(b.x).min(c.x).min(d.x);
        let top = a.y.min(b.y).min(c.y).min(d.y);
        let right = a.x.max(b.x).max(c.x).max(d.x);
        let bottom = a.y.max(b.y).max(c.y).max(d.y);
        let result = Rect {
            x: left,
            y: top,
            width: right - left,
            height: bottom - top,
        };
        if [result.x, result.y, result.width, result.height]
            .into_iter()
            .all(f32::is_finite)
        {
            Ok(result)
        } else {
            Err("visual transform produces an invalid surface-space bound".into())
        }
    }

    /// the largest singular value of the linear part.
    pub fn magnification(self) -> f64 {
        let first = (self.xx + self.yy).hypot(self.yx - self.xy);
        let second = (self.xx - self.yy).hypot(self.yx + self.xy);
        (first + second) / 2.0
    }

    pub(crate) fn renderer_canonical(self) -> Result<Self, String> {
        let [xx, xy, tx, yx, yy, ty] = self.skia_values()?;
        Ok(Self {
            xx: f64::from(xx),
            yx: f64::from(yx),
            xy: f64::from(xy),
            yy: f64::from(yy),
            tx: f64::from(tx),
            ty: f64::from(ty),
        })
    }

    pub(crate) fn skia_values(self) -> Result<[f32; 6], String> {
        let values = [self.xx, self.xy, self.tx, self.yx, self.yy, self.ty];
        let [Some(xx), Some(xy), Some(tx), Some(yx), Some(yy), Some(ty)] =
            values.map(representable_f32)
        else {
            return Err("visual transform cannot be represented by the renderer".into());
        };
        Ok([xx, xy, tx, yx, yy, ty])
    }

    fn validate(self) -> Result<(), String> {
        self.skia_values().map(|_| ())
    }
}

fn representable_f32(value: f64) -> Option<f32> {
    if !value.is_finite() || value.abs() > RENDER_LIMIT {
        return None;
    }
    let result = value as f32;
    (result.is_finite() && (value == 0.0 || result != 0.0)).then_some(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(actual: f32, expected: f32) {
        assert!((actual - expected).abs() < 0.001, "{actual} != {expected}");
    }

    #[test]
    fn signed_scale_and_clockwise_rotation_share_one_inverse() {
        let mut style = Style {
            scale_x: -2.0,
            scale_y: 0.5,
            rotation: 90.0,
            translate_x: 7.0,
            translate_y: -3.0,
            ..Style::default()
        };
        style.transform_origin.x = 0.0;
        style.transform_origin.y = 0.0;
        let transform = Affine::widget(&style, 20.0, 10.0).unwrap();
        let surface = transform.point(Point { x: 4.0, y: 6.0 }).unwrap();
        close(surface.x, 4.0);
        close(surface.y, -11.0);
        let local = transform
            .inverse()
            .unwrap()
            .unwrap()
            .point(surface)
            .unwrap();
        close(local.x, 4.0);
        close(local.y, 6.0);
    }

    #[test]
    fn singular_transforms_have_bounds_but_no_inverse() {
        let style = Style {
            scale_x: 0.0,
            ..Style::default()
        };
        let transform = Affine::widget(&style, 20.0, 10.0).unwrap();
        assert_eq!(transform.inverse().unwrap(), None);
        assert_eq!(
            transform
                .rect_aabb(Rect {
                    width: 20.0,
                    height: 10.0,
                    ..Rect::default()
                })
                .unwrap()
                .width,
            0.0
        );
    }

    #[test]
    fn nested_nonuniform_transforms_use_the_operator_norm() {
        let parent = Affine::rotation(45.0)
            .compose(Affine::scale(4.0, 1.0))
            .unwrap();
        let child = parent.compose(Affine::rotation(-45.0)).unwrap();
        assert!(child.magnification() > 3.9);
    }
}
