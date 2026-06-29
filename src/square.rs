//! Per-square stack representation as a 4-bit "nibble code".
//!
//! A square holds a stack of up to [`MAX_STACK`] pieces. We encode the whole
//! stack as a single small integer (0..=15) so a [`crate::position::Position`]
//! is a plain `[u8; NCELLS]` array — `Copy`, fixed size, allocation free.
//!
//! Encoding: a sentinel `1` bit marks the top of the value; below it are the
//! piece colors, bottom-of-stack in the most significant color bit and
//! top-of-stack in bit 0 (White = 0, Black = 1).
//!
//! ```text
//! code  binary   stack (bottom -> top)
//! 0     0        (empty)
//! 2     0b10     [W]
//! 3     0b11     [B]
//! 4     0b100    [W, W]
//! 5     0b101    [W, B]
//! 6     0b110    [B, W]
//! 7     0b111    [B, B]
//! 8..15 0b1xxx   height 3
//! ```
//!
//! With this scheme `height(code) = floor(log2(code))` and the top color is the
//! low bit, so push/pop are branch-light bit shifts.

use crate::color::Color;

/// Board columns.
pub const COLS: usize = 5;
/// Board rows.
pub const ROWS: usize = 6;
/// Number of squares on the board.
pub const NCELLS: usize = COLS * ROWS; // 30
/// Maximum stack height.
pub const MAX_STACK: usize = 3;

/// Number of pieces in the stack on this square (0..=[`MAX_STACK`]).
#[inline]
pub fn height(code: u8) -> usize {
    if code == 0 {
        0
    } else {
        (31 - (code as u32).leading_zeros()) as usize
    }
}

/// Whether the square is empty.
#[inline]
pub fn is_empty(code: u8) -> bool {
    code == 0
}

/// Color of the top piece, or `None` if the square is empty.
#[inline]
pub fn top_color(code: u8) -> Option<Color> {
    if code == 0 {
        None
    } else {
        Some(Color::from_bit(code & 1))
    }
}

/// Place `color` on top of the stack. Caller must ensure `height < MAX_STACK`.
#[inline]
pub fn push(code: u8, color: Color) -> u8 {
    debug_assert!(height(code) < MAX_STACK, "stack overflow");
    if code == 0 {
        0b10 | color.bit()
    } else {
        (code << 1) | color.bit()
    }
}

/// Remove the top piece. Caller must ensure the square is non-empty.
#[inline]
pub fn pop(code: u8) -> u8 {
    debug_assert!(code != 0, "pop on empty square");
    if height(code) == 1 {
        0
    } else {
        code >> 1
    }
}

/// Swap the color of every piece in the stack (White <-> Black), keeping the
/// height and bottom-to-top order. Used by the side-to-move reorientation.
/// Involutive: `swap_colors(swap_colors(c)) == c`.
#[inline]
pub fn swap_colors(code: u8) -> u8 {
    if code == 0 {
        0
    } else {
        // Flip the low `height` color bits; leave the sentinel bit untouched.
        let h = height(code);
        code ^ ((1u8 << h) - 1)
    }
}

/// Build a code from an explicit bottom-to-top list of colors (for tests and
/// for constructing positions). Panics if `stack.len() > MAX_STACK`.
pub fn make(stack: &[Color]) -> u8 {
    assert!(stack.len() <= MAX_STACK, "stack too tall");
    let mut code = 0u8;
    for &c in stack {
        code = push(code, c);
    }
    code
}

/// Decode the stack into a bottom-to-top list of colors (for display).
pub fn colors(code: u8) -> Vec<Color> {
    let h = height(code);
    let mut v = Vec::with_capacity(h);
    for d in 0..h {
        let bit = ((code >> (h - 1 - d)) & 1) as u8;
        v.push(Color::from_bit(bit));
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::color::Color::{Black, White};

    #[test]
    fn empty_square() {
        assert_eq!(height(0), 0);
        assert!(is_empty(0));
        assert_eq!(top_color(0), None);
        assert!(colors(0).is_empty());
    }

    #[test]
    fn push_pop_roundtrip_all_stacks() {
        let cases: &[&[Color]] = &[
            &[White],
            &[Black],
            &[White, Black],
            &[Black, White],
            &[White, Black, White],
            &[Black, Black, Black],
        ];
        for &stack in cases {
            let code = make(stack);
            assert_eq!(height(code), stack.len());
            assert_eq!(colors(code), stack.to_vec());
            assert_eq!(top_color(code), stack.last().copied());
            // pushing then popping returns the original code
            if stack.len() < MAX_STACK {
                assert_eq!(pop(push(code, White)), code);
                assert_eq!(pop(push(code, Black)), code);
            }
            // popping the top yields the same stack minus its last element
            assert_eq!(colors(pop(code)), stack[..stack.len() - 1].to_vec());
        }
    }

    #[test]
    fn codes_match_documented_table() {
        assert_eq!(make(&[White]), 2);
        assert_eq!(make(&[Black]), 3);
        assert_eq!(make(&[White, Black]), 5);
        assert_eq!(make(&[Black, White]), 6);
    }

    #[test]
    fn swap_colors_is_correct_and_involutive() {
        assert_eq!(swap_colors(0), 0);
        assert_eq!(swap_colors(make(&[White])), make(&[Black]));
        assert_eq!(swap_colors(make(&[White, Black])), make(&[Black, White]));
        assert_eq!(
            swap_colors(make(&[White, Black, White])),
            make(&[Black, White, Black])
        );
        for code in [0u8, 2, 3, 5, 6, 10, 13, 15] {
            assert_eq!(swap_colors(swap_colors(code)), code);
            assert_eq!(height(swap_colors(code)), height(code));
        }
    }
}
