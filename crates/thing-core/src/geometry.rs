/// a point in logical pixels, before output scaling.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Point {
    pub x: f32,
    pub y: f32,
}

/// a half-open rectangle in logical pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl Rect {
    pub fn contains(self, point: Point) -> bool {
        point.x >= self.x
            && point.y >= self.y
            && point.x < self.x + self.width
            && point.y < self.y + self.height
    }

    pub fn intersection(self, other: Self) -> Option<Self> {
        let x = self.x.max(other.x);
        let y = self.y.max(other.y);
        let width = (self.x + self.width).min(other.x + other.width) - x;
        let height = (self.y + self.height).min(other.y + other.height) - y;
        (width > 0.0 && height > 0.0).then_some(Self {
            x,
            y,
            width,
            height,
        })
    }
}

/// the horizontal span used to paint and operate a range input.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HorizontalRange {
    start: f32,
    length: f32,
}

impl HorizontalRange {
    /// keep an inset inside the widget while leaving a usable input span.
    pub fn inset(bounds: Rect, inset: f32) -> Self {
        let inset = inset.min(((bounds.width - 1.0) / 2.0).max(0.0));
        Self {
            start: bounds.x + inset,
            length: (bounds.width - 2.0 * inset).max(0.0),
        }
    }

    /// apply an inset without changing the behavior of narrow legacy controls.
    pub fn unclamped_inset(bounds: Rect, inset: f32) -> Self {
        Self {
            start: bounds.x + inset,
            length: (bounds.width - 2.0 * inset).max(0.0),
        }
    }

    pub fn start(self) -> f32 {
        self.start
    }

    pub fn length(self) -> f32 {
        self.length
    }

    pub fn fraction_at(self, x: f32) -> f32 {
        let fraction = (x - self.start) / self.length.max(1.0);
        if fraction.is_finite() {
            fraction.clamp(0.0, 1.0)
        } else {
            fraction
        }
    }

    pub fn position(self, fraction: f32) -> f32 {
        self.start + self.length * fraction.clamp(0.0, 1.0)
    }
}

/// a validated output scale, expressed in wayland's 120ths of a logical pixel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Scale(u32);

impl Default for Scale {
    fn default() -> Self {
        Self(120)
    }
}

impl Scale {
    pub fn from_120ths(value: u32) -> Option<Self> {
        (value > 0 && value <= 1920).then_some(Self(value))
    }

    pub fn factor(self) -> f32 {
        self.0 as f32 / 120.0
    }

    /// round up so fractional scaling never leaves part of the surface uncovered.
    pub fn physical_size(self, logical: u32) -> Option<u32> {
        let size = (u64::from(logical) * u64::from(self.0)).div_ceil(120);
        u32::try_from(size).ok().filter(|&size| size > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adjacent_widgets_do_not_both_receive_an_edge_click() {
        let a = Rect {
            width: 10.0,
            height: 10.0,
            ..Rect::default()
        };
        let b = Rect { x: 10.0, ..a };
        let point = Point { x: 10.0, y: 5.0 };
        assert!(!a.contains(point));
        assert!(b.contains(point));
    }

    #[test]
    fn clips_disjoint_and_touching_rectangles() {
        let a = Rect {
            width: 10.0,
            height: 10.0,
            ..Rect::default()
        };
        assert_eq!(a.intersection(Rect { x: 10.0, ..a }), None);
        assert_eq!(
            a.intersection(Rect { x: 5.0, ..a }).map(|r| r.width),
            Some(5.0)
        );
    }

    #[test]
    fn fractional_scaling_rounds_up_and_checks_overflow() {
        let scale = Scale::from_120ths(150).expect("valid scale");
        assert_eq!(scale.physical_size(101), Some(127));
        assert_eq!(scale.physical_size(u32::MAX), None);
        assert_eq!(scale.physical_size(0), None);
        assert!(Scale::from_120ths(0).is_none());
    }

    #[test]
    fn range_insets_keep_custom_controls_usable_without_changing_legacy_geometry() {
        let ordinary = HorizontalRange::unclamped_inset(
            Rect {
                x: 20.0,
                width: 100.0,
                height: 20.0,
                ..Rect::default()
            },
            8.0,
        );
        assert_eq!(ordinary.start(), 28.0);
        assert_eq!(ordinary.length(), 84.0);
        assert_eq!(ordinary.fraction_at(70.0), 0.5);

        let bounds = Rect {
            x: 20.0,
            width: 10.0,
            height: 20.0,
            ..Rect::default()
        };
        let custom = HorizontalRange::inset(bounds, 8.0);
        assert_eq!(custom.start(), 24.5);
        assert_eq!(custom.length(), 1.0);
        assert_eq!(custom.fraction_at(24.5), 0.0);
        assert_eq!(custom.fraction_at(25.5), 1.0);

        let legacy = HorizontalRange::unclamped_inset(bounds, 8.0);
        assert_eq!(legacy.start(), 28.0);
        assert_eq!(legacy.length(), 0.0);
        assert_eq!(legacy.fraction_at(28.5), 0.5);
        assert_eq!(legacy.position(1.0), 28.0);
    }
}
