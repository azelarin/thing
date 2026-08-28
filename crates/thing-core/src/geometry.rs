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
}
