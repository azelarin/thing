use std::{error::Error, fmt, str::FromStr};

/// an unpremultiplied srgb color. alpha is linear and stored separately.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Color(pub [u8; 4]);

impl Color {
    pub const TRANSPARENT: Self = Self([0, 0, 0, 0]);
    pub const WHITE: Self = Self([255; 4]);

    /// components in the range 0–1, suitable for a renderer's color api.
    pub fn normalized(self) -> [f32; 4] {
        self.0.map(|component| f32::from(component) / 255.0)
    }
}

/// a color must be hexadecimal rgb or rgba, prefixed with `#`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParseColorError;

impl fmt::Display for ParseColorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("expected #RRGGBB or #RRGGBBAA")
    }
}

impl Error for ParseColorError {}

impl FromStr for Color {
    type Err = ParseColorError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let hex = value.strip_prefix('#').ok_or(ParseColorError)?;
        if !matches!(hex.len(), 6 | 8) || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(ParseColorError);
        }
        let mut rgba = [255; 4];
        for (index, component) in rgba.iter_mut().take(hex.len() / 2).enumerate() {
            *component = u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16)
                .map_err(|_| ParseColorError)?;
        }
        Ok(Self(rgba))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_alpha_without_premultiplying_rgb() {
        assert_eq!("#ff804020".parse::<Color>(), Ok(Color([255, 128, 64, 32])));
        assert_eq!("#abcdef".parse::<Color>(), Ok(Color([171, 205, 239, 255])));
    }

    #[test]
    fn rejects_invalid_and_multibyte_input_without_panicking() {
        for value in ["", "red", "ffffff", "#abc", "#gggggg", "#ééé", "#fffffffff"] {
            assert!(value.parse::<Color>().is_err(), "accepted {value:?}");
        }
    }
}
