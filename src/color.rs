//! Piece / player color.

/// The color of a piece.
///
/// In the *absolute* match frame, `White` is the first player (先手) and `Black`
/// the second (後手). Inside a [`crate::position::Position`], however, the board
/// is stored from the side-to-move's perspective where the mover is always
/// `Black` ([`crate::position::US`]) and the opponent `White`
/// ([`crate::position::THEM`]); see `position.rs` for that normalization.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Color {
    White,
    Black,
}

impl Color {
    /// The other player.
    #[inline]
    pub fn opponent(self) -> Color {
        match self {
            Color::White => Color::Black,
            Color::Black => Color::White,
        }
    }

    /// Single-bit encoding used inside square stack codes (White = 0, Black = 1).
    #[inline]
    pub fn bit(self) -> u8 {
        match self {
            Color::White => 0,
            Color::Black => 1,
        }
    }

    /// Decode the low bit back into a color.
    #[inline]
    pub fn from_bit(b: u8) -> Color {
        if b & 1 == 0 {
            Color::White
        } else {
            Color::Black
        }
    }

    /// Display character.
    #[inline]
    pub fn to_char(self) -> char {
        match self {
            Color::White => 'W',
            Color::Black => 'B',
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Color;

    #[test]
    fn bit_roundtrip() {
        for c in [Color::White, Color::Black] {
            assert_eq!(Color::from_bit(c.bit()), c);
        }
    }
}
