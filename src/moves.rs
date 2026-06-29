//! Move representation and a fixed-capacity move buffer.

/// A single move: the top piece of square `from` steps to adjacent square `to`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Move {
    pub from: u8,
    pub to: u8,
}

impl Move {
    #[inline]
    pub fn new(from: usize, to: usize) -> Move {
        Move {
            from: from as u8,
            to: to as u8,
        }
    }
}

/// A turn has at most 5 movable pieces × 8 directions = 40 moves; 64 is a safe
/// fixed cap so move generation never allocates (important for later search).
pub const MAX_MOVES: usize = 64;

/// Fixed-capacity, stack-allocated list of legal moves.
#[derive(Clone)]
pub struct MoveList {
    buf: [Move; MAX_MOVES],
    len: usize,
}

impl MoveList {
    #[inline]
    pub fn new() -> Self {
        MoveList {
            buf: [Move::default(); MAX_MOVES],
            len: 0,
        }
    }

    #[inline]
    pub fn clear(&mut self) {
        self.len = 0;
    }

    #[inline]
    pub fn push(&mut self, m: Move) {
        debug_assert!(self.len < MAX_MOVES, "move list overflow");
        self.buf[self.len] = m;
        self.len += 1;
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[inline]
    pub fn get(&self, i: usize) -> Move {
        self.buf[i]
    }

    #[inline]
    pub fn as_slice(&self) -> &[Move] {
        &self.buf[..self.len]
    }
}

impl Default for MoveList {
    fn default() -> Self {
        MoveList::new()
    }
}
