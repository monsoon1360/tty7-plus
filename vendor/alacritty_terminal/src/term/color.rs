use std::ops::{Index, IndexMut};

use crate::vte::ansi::{NamedColor, Rgb};

/// Number of terminal colors.
pub const COUNT: usize = 269;

/// Array of indexed colors.
///
/// | Indices  | Description       |
/// | -------- | ----------------- |
/// | 0..16    | Named ANSI colors |
/// | 16..232  | Color cube        |
/// | 233..256 | Grayscale ramp    |
/// | 256      | Foreground        |
/// | 257      | Background        |
/// | 258      | Cursor            |
/// | 259..267 | Dim colors        |
/// | 267      | Bright foreground |
/// | 268      | Dim background    |
#[derive(Copy, Clone)]
pub struct Colors([Option<Rgb>; COUNT]);

impl Default for Colors {
    fn default() -> Self {
        Self([None; COUNT])
    }
}

#[cfg(feature = "serde")]
impl serde::Serialize for Colors {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serde::Serialize::serialize(self.0.as_slice(), serializer)
    }
}

#[cfg(feature = "serde")]
impl<'de> serde::Deserialize<'de> for Colors {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let values = <Vec<Option<Rgb>> as serde::Deserialize>::deserialize(deserializer)?;
        let array = values.try_into().map_err(|_| serde::de::Error::custom("invalid palette size"))?;
        Ok(Self(array))
    }
}

impl Index<usize> for Colors {
    type Output = Option<Rgb>;

    #[inline]
    fn index(&self, index: usize) -> &Self::Output {
        &self.0[index]
    }
}

impl IndexMut<usize> for Colors {
    #[inline]
    fn index_mut(&mut self, index: usize) -> &mut Self::Output {
        &mut self.0[index]
    }
}

impl Index<NamedColor> for Colors {
    type Output = Option<Rgb>;

    #[inline]
    fn index(&self, index: NamedColor) -> &Self::Output {
        &self.0[index as usize]
    }
}

impl IndexMut<NamedColor> for Colors {
    #[inline]
    fn index_mut(&mut self, index: NamedColor) -> &mut Self::Output {
        &mut self.0[index as usize]
    }
}
