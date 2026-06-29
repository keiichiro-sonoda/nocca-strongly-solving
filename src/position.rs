//! Board position, stored in the **side-to-move's perspective**.
//!
//! To fold the first-player/second-player symmetry into the representation, a
//! position is always viewed so that the player about to move is `Black`
//! ([`US`]) and their goal is the far row ([`US_GOAL_ROW`] = `ROWS - 1`). The
//! opponent is `White` ([`THEM`]) with goal row 0. Because of this there is **no
//! `turn` field**: the mover is always `Black`.
//!
//! After a move, [`Position::apply_move`] re-orients the board to the next
//! player's perspective via [`Position::reorient`] (flip rows + swap colors), so
//! the invariant "mover is Black, goal is row 5" always holds. The whole game
//! rule set (8-direction step, stacking, height-3 cap) is color-symmetric, so
//! this transform preserves legality.
//!
//! Display / move-record code must convert back to absolute White/Black using
//! the ply parity (see `main.rs`); never show this normalized board directly.

use crate::color::Color;
use crate::moves::{Move, MoveList};
use crate::square::{self, COLS, MAX_STACK, NCELLS, ROWS};

/// The side to move, always represented as `Black`.
pub const US: Color = Color::Black;
/// The opponent, always represented as `White`.
pub const THEM: Color = Color::White;
/// The row the side to move is trying to reach (the far row).
pub const US_GOAL_ROW: usize = ROWS - 1;
/// The row the opponent is trying to reach.
pub const THEM_GOAL_ROW: usize = 0;

/// A complete game position from the side-to-move's perspective.
///
/// `cells[row * COLS + col]` is the [stack code](crate::square) of that square.
/// `Copy`, fixed size, no `turn` field.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub struct Position {
    cells: [u8; NCELLS],
}

/// The 8 king-style step directions as `(d_row, d_col)`.
const DIRS: [(i32, i32); 8] = [
    (-1, -1),
    (-1, 0),
    (-1, 1),
    (0, -1),
    (0, 1),
    (1, -1),
    (1, 0),
    (1, 1),
];

/// Flatten `(row, col)` into a cell index.
#[inline]
pub const fn idx(row: usize, col: usize) -> usize {
    row * COLS + col
}

/// Row of a cell index.
#[inline]
pub fn row_of(i: usize) -> usize {
    i / COLS
}

/// Column of a cell index.
#[inline]
pub fn col_of(i: usize) -> usize {
    i % COLS
}

impl Position {
    /// Standard opening in the normalized frame: the side to move (`Black`/[`US`])
    /// occupies row 0, the opponent (`White`/[`THEM`]) the far row.
    ///
    /// This is the first player's position before their first move, color-swapped
    /// so the mover reads as `Black` (the first player's goal already is the far
    /// row, so no row flip is needed here).
    pub fn initial() -> Self {
        let mut cells = [0u8; NCELLS];
        for c in 0..COLS {
            cells[idx(0, c)] = square::make(&[US]);
            cells[idx(ROWS - 1, c)] = square::make(&[THEM]);
        }
        Position { cells }
    }

    /// Build a position from raw cell codes (for tests and analysis tooling).
    /// The codes are interpreted in the normalized frame (mover = `Black`).
    pub fn from_cells(cells: [u8; NCELLS]) -> Self {
        Position { cells }
    }

    /// Stack code on a single square.
    #[inline]
    pub fn cell(&self, i: usize) -> u8 {
        self.cells[i]
    }

    /// Re-orient to the other player's perspective: flip rows (`r -> ROWS-1-r`)
    /// and swap every piece color. Involutive (`reorient∘reorient == id`) and
    /// commutes with [`Position::mirror`].
    pub fn reorient(&self) -> Position {
        let mut out = [0u8; NCELLS];
        for r in 0..ROWS {
            for c in 0..COLS {
                out[idx(ROWS - 1 - r, c)] = square::swap_colors(self.cells[idx(r, c)]);
            }
        }
        Position { cells: out }
    }

    /// Color-swap every square in place (no row flip). Used to recover the
    /// absolute board for display on even plies.
    pub fn swapped(&self) -> Position {
        let mut cells = self.cells;
        for code in cells.iter_mut() {
            *code = square::swap_colors(*code);
        }
        Position { cells }
    }

    /// Flip rows (`r -> ROWS-1-r`) with no color change. Used to recover the
    /// absolute board for display on odd plies.
    pub fn flipped(&self) -> Position {
        let mut cells = [0u8; NCELLS];
        for r in 0..ROWS {
            for c in 0..COLS {
                cells[idx(ROWS - 1 - r, c)] = self.cells[idx(r, c)];
            }
        }
        Position { cells }
    }

    /// Append all legal moves for the side to move (`Black`/[`US`]) into `out`
    /// (cleared first). A move is legal iff the source's top piece is `US`, the
    /// destination is on the board, and its stack is not full ([`MAX_STACK`]).
    pub fn generate_legal_moves(&self, out: &mut MoveList) {
        out.clear();
        for i in 0..NCELLS {
            if square::top_color(self.cells[i]) != Some(US) {
                continue;
            }
            let r = row_of(i) as i32;
            let c = col_of(i) as i32;
            for &(dr, dc) in DIRS.iter() {
                let nr = r + dr;
                let nc = c + dc;
                if nr < 0 || nr >= ROWS as i32 || nc < 0 || nc >= COLS as i32 {
                    continue;
                }
                let ni = idx(nr as usize, nc as usize);
                if square::height(self.cells[ni]) >= MAX_STACK {
                    continue;
                }
                out.push(Move::new(i, ni));
            }
        }
    }

    /// Apply a move and re-orient to the next player's perspective. The mover is
    /// always `US`, so the moved piece is pushed as `Black`; the returned
    /// position again has the (new) side to move as `Black`.
    pub fn apply_move(&self, m: Move) -> Position {
        let mut next = *self;
        let from = m.from as usize;
        let to = m.to as usize;
        next.cells[from] = square::pop(next.cells[from]);
        next.cells[to] = square::push(next.cells[to], US);
        next.reorient()
    }

    /// True if the opponent (`White`/[`THEM`]) already has a piece on top of a
    /// square in their goal row ([`THEM_GOAL_ROW`]). This means the opponent won
    /// on the previous move, i.e. the side to move has **lost**. (There is no
    /// symmetric "side to move has already won" terminal: a win is realized by
    /// making a move whose resulting child is a loss for that child's mover.)
    pub fn opponent_reached_goal(&self) -> bool {
        (0..COLS).any(|c| square::top_color(self.cells[idx(THEM_GOAL_ROW, c)]) == Some(THEM))
    }

    /// Try-type win for the side to move: at the start of its turn, `US` has an
    /// *uncovered* (top-of-stack) piece on its goal row ([`US_GOAL_ROW`], the
    /// opponent's back row). Such a piece could step off the board to the goal
    /// zone next and nothing is better, so the win is claimed now (the off-board
    /// move is not modeled). Placement-only ⇒ path-independent. This is the
    /// correct NOCCA win condition; the opponent gets one prior turn to block by
    /// stacking onto the piece (乗っかり).
    pub fn try_win(&self) -> bool {
        (0..COLS).any(|c| square::top_color(self.cells[idx(US_GOAL_ROW, c)]) == Some(US))
    }

    /// Compact 120-bit encoding packed into a `u128`: each of the 30 squares is
    /// a 4-bit nibble. No turn bit (the mover is implicit). Suitable as a hash /
    /// transposition key.
    pub fn key(&self) -> u128 {
        let mut k = 0u128;
        for i in 0..NCELLS {
            k |= (self.cells[i] as u128) << (i * 4);
        }
        k
    }

    /// Inverse of [`Position::key`].
    pub fn from_key(k: u128) -> Position {
        let mut cells = [0u8; NCELLS];
        for i in 0..NCELLS {
            cells[i] = ((k >> (i * 4)) & 0xF) as u8;
        }
        Position { cells }
    }

    /// Mirror the board left-to-right (`c -> COLS-1-c`); colors unchanged.
    pub fn mirror(&self) -> Position {
        let mut cells = [0u8; NCELLS];
        for r in 0..ROWS {
            for c in 0..COLS {
                cells[idx(r, c)] = self.cells[idx(r, COLS - 1 - c)];
            }
        }
        Position { cells }
    }

    /// Canonical key under the only value-preserving board symmetry: left-right
    /// mirror. (The first/second-player symmetry is already folded into the
    /// representation, so [`Position::reorient`] is deliberately **not** part of
    /// canonicalization — it changes whose move it is and hence the value.)
    pub fn canonical_key(&self) -> u128 {
        let a = self.key();
        let b = self.mirror().key();
        if a <= b {
            a
        } else {
            b
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::color::Color::{Black, White};

    #[test]
    fn initial_position_basics() {
        let p = Position::initial();
        for c in 0..COLS {
            assert_eq!(square::top_color(p.cell(idx(0, c))), Some(US)); // Black
            assert_eq!(square::top_color(p.cell(idx(ROWS - 1, c))), Some(THEM)); // White
        }
        // White is on row 5, not row 0, so the opponent has not reached row 0.
        assert!(!p.opponent_reached_goal());
    }

    #[test]
    fn initial_legal_move_count_is_21() {
        let mut ml = MoveList::new();
        Position::initial().generate_legal_moves(&mut ml);
        assert_eq!(ml.len(), 21);
    }

    #[test]
    fn key_roundtrip() {
        assert_eq!(Position::from_key(Position::initial().key()), Position::initial());

        let mut cells = [0u8; NCELLS];
        cells[idx(2, 1)] = square::make(&[Black, White]);
        cells[idx(3, 4)] = square::make(&[White, Black, Black]);
        cells[idx(0, 0)] = square::make(&[White]);
        let q = Position::from_cells(cells);
        assert_eq!(Position::from_key(q.key()), q);
    }

    #[test]
    fn mirror_reorient_and_swap_are_involutions() {
        let mut cells = [0u8; NCELLS];
        cells[idx(2, 0)] = square::make(&[Black]);
        cells[idx(3, 1)] = square::make(&[White, Black]);
        cells[idx(1, 4)] = square::make(&[Black, White, Black]);
        let q = Position::from_cells(cells);

        assert_eq!(q.mirror().mirror(), q);
        assert_eq!(q.reorient().reorient(), q);
        assert_eq!(q.swapped().swapped(), q);
        assert_eq!(q.flipped().flipped(), q);
        // mirror and reorient commute.
        assert_eq!(q.mirror().reorient(), q.reorient().mirror());
        // reorient == flip then color-swap.
        assert_eq!(q.reorient(), q.flipped().swapped());
        // mirror flips columns.
        assert_eq!(q.mirror().cell(idx(2, COLS - 1)), q.cell(idx(2, 0)));
    }

    #[test]
    fn canonical_key_is_idempotent_and_mirror_invariant() {
        let mut cells = [0u8; NCELLS];
        cells[idx(2, 0)] = square::make(&[Black]);
        cells[idx(4, 3)] = square::make(&[White, Black]);
        let q = Position::from_cells(cells);

        let ck = q.canonical_key();
        assert_eq!(q.mirror().canonical_key(), ck);
        assert_eq!(Position::from_key(ck).canonical_key(), ck);
    }

    #[test]
    fn move_onto_goal_makes_child_a_loss_for_its_mover() {
        // US (Black) one step from the goal row.
        let mut cells = [0u8; NCELLS];
        cells[idx(ROWS - 2, 2)] = square::make(&[US]);
        let p = Position::from_cells(cells);

        let mut ml = MoveList::new();
        p.generate_legal_moves(&mut ml);
        let winning = ml
            .as_slice()
            .iter()
            .find(|m| m.to as usize == idx(US_GOAL_ROW, 2))
            .copied()
            .expect("a step onto the goal row must exist");

        let child = p.apply_move(winning);
        // After the move + reorient, the opponent (new side to move) sees that we
        // already reached the goal => it is a loss for them, i.e. we won.
        assert!(child.opponent_reached_goal());
    }
}
