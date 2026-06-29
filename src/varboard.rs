//! Size-parametric NOCCA×NOCCA board for the reduced-version validation phase
//! (and the eventual disk-based retrograde over the full 6×5).
//!
//! The production [`crate::position::Position`] is fixed at 6×5; this type takes
//! `rows`/`cols` at runtime so we can solve the reduced versions whose values are
//! published by Matsumoto & Kuroda (IPSJ 2022) and use them as an external
//! oracle. Per-cell encoding reuses [`crate::square`] (4-bit stack codes), so the
//! game rules are identical — only the board dimensions differ.
//!
//! Rule (matching the prior research): **pieces per player = board width =
//! `cols`**, placed on each player's back row in the initial position. The side
//! to move is always `US` (Black); [`VarBoard::apply_move`] re-orients, exactly
//! like the fixed board.

use crate::color::Color;
use crate::position::{THEM, US};
use crate::square;

/// Largest board we support: 6×5 = 30 cells ≤ 32, and 30×4 = 120 bits ≤ 128.
pub const MAX_CELLS: usize = 32;

/// A board of `rows × cols` with up to [`MAX_CELLS`] cells. `Copy`, no allocation.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct VarBoard {
    rows: u8,
    cols: u8,
    cells: [u8; MAX_CELLS],
}

impl VarBoard {
    #[inline]
    pub fn rows(&self) -> usize {
        self.rows as usize
    }
    #[inline]
    pub fn cols(&self) -> usize {
        self.cols as usize
    }
    #[inline]
    pub fn ncells(&self) -> usize {
        self.rows as usize * self.cols as usize
    }
    #[inline]
    fn idx(&self, r: usize, c: usize) -> usize {
        r * self.cols as usize + c
    }

    /// Initial position: `US` fills row 0, `THEM` fills the far row; one piece per
    /// column (so pieces per player = `cols`). `US` is to move.
    pub fn initial(rows: usize, cols: usize) -> VarBoard {
        assert!(rows >= 1 && cols >= 1 && rows * cols <= MAX_CELLS);
        let mut b = VarBoard {
            rows: rows as u8,
            cols: cols as u8,
            cells: [0u8; MAX_CELLS],
        };
        for c in 0..cols {
            b.cells[b.idx(0, c)] = square::make(&[US]);
            b.cells[b.idx(rows - 1, c)] = square::make(&[THEM]);
        }
        b
    }

    /// Re-orient to the next player's perspective: flip rows and swap colors.
    /// Involutive (`reorient(reorient(x)) == x`), so it also undoes itself when
    /// walking edges backwards (predecessor generation).
    pub fn reorient(&self) -> VarBoard {
        let mut out = VarBoard {
            rows: self.rows,
            cols: self.cols,
            cells: [0u8; MAX_CELLS],
        };
        let (rows, cols) = (self.rows(), self.cols());
        for r in 0..rows {
            for c in 0..cols {
                out.cells[out.idx(rows - 1 - r, c)] =
                    square::swap_colors(self.cells[self.idx(r, c)]);
            }
        }
        out
    }

    /// True if `THEM` has a piece on top in their goal row (row 0): the opponent
    /// won on the previous move, i.e. the side to move has lost.
    pub fn opponent_reached_goal(&self) -> bool {
        (0..self.cols()).any(|c| square::top_color(self.cells[self.idx(0, c)]) == Some(THEM))
    }

    /// Append all legal moves as `(from, to)` cell-index pairs. A move slides the
    /// top `US` piece one step onto an adjacent in-board cell with height < max.
    /// `diag` selects the direction set: `true` = 8-dir king (the production
    /// rule), `false` = 4-dir orthogonal.
    ///
    /// No off-board moves: the win is handled by [`VarBoard::try_win`] (try-type),
    /// not by an explicit exit move.
    pub fn moves(&self, out: &mut Vec<(usize, usize)>, diag: bool) {
        out.clear();
        let (rows, cols) = (self.rows() as i32, self.cols() as i32);
        for r in 0..self.rows() {
            for c in 0..self.cols() {
                let from = self.idx(r, c);
                if square::top_color(self.cells[from]) != Some(US) {
                    continue;
                }
                for dr in -1..=1i32 {
                    for dc in -1..=1i32 {
                        if dr == 0 && dc == 0 {
                            continue;
                        }
                        if !diag && dr != 0 && dc != 0 {
                            continue; // skip diagonals in orthogonal mode
                        }
                        let nr = r as i32 + dr;
                        let nc = c as i32 + dc;
                        if nr < 0 || nr >= rows || nc < 0 || nc >= cols {
                            continue;
                        }
                        let to = self.idx(nr as usize, nc as usize);
                        if square::height(self.cells[to]) >= square::MAX_STACK {
                            continue;
                        }
                        out.push((from, to));
                    }
                }
            }
        }
    }

    /// Try-type win: at the start of the side-to-move's turn, the side to move
    /// (`US`) wins iff it has an *uncovered* (top-of-stack) piece on the far row
    /// (`rows-1`, the opponent's back row). Rationale: such a piece could step
    /// off the board to the goal zone next, and nothing is better, so the win is
    /// claimed now without simulating the off-board move. Placement-only ⇒
    /// path-independent ⇒ usable directly as a retrograde terminal.
    pub fn try_win(&self) -> bool {
        let far = self.rows() - 1;
        (0..self.cols()).any(|c| square::top_color(self.cells[self.idx(far, c)]) == Some(US))
    }

    /// Apply a `(from, to)` move and re-orient. The mover is always `US`.
    pub fn apply_move(&self, from: usize, to: usize) -> VarBoard {
        self.apply_raw(from, to).reorient()
    }

    /// Move the top `US` piece from `from` to `to` WITHOUT re-orienting. Used by
    /// predecessor generation, which re-orients first and then un-moves.
    pub fn apply_raw(&self, from: usize, to: usize) -> VarBoard {
        let mut next = *self;
        next.cells[from] = square::pop(next.cells[from]);
        next.cells[to] = square::push(next.cells[to], US);
        next
    }

    /// Compact `ncells × 4`-bit packing of the board into a `u128` (no dims).
    pub fn key(&self) -> u128 {
        let mut k = 0u128;
        for i in 0..self.ncells() {
            k |= (self.cells[i] as u128) << (i * 4);
        }
        k
    }

    /// Predecessor generation: all reachable-shape parents `P` such that a
    /// forward move from `P` yields this state (`self`, a canonical key holder).
    ///
    /// Method (hypothesis: reuse the forward move generator): for each mirror
    /// representative `rep` of this canonical board, take `Q = reorient(rep)` and
    /// run the *forward* move generator on `Q`; each `(src,dst)` un-moved via
    /// [`apply_raw`](Self::apply_raw) (no post-reorient) is a parent candidate.
    /// `filter_terminal` drops candidates that are themselves try-win terminals
    /// (terminals never move, so they are not real parents) — the terminal-
    /// boundary exception. Output is canonical keys, sorted+deduped.
    pub fn predecessors(&self, diag: bool, filter_terminal: bool, out: &mut Vec<u128>) {
        out.clear();
        let mut mv: Vec<(usize, usize)> = Vec::new();
        for rep in [*self, self.mirror()] {
            let q = rep.reorient();
            q.moves(&mut mv, diag);
            for &(src, dst) in &mv {
                let p = q.apply_raw(src, dst);
                if filter_terminal && p.try_win() {
                    continue;
                }
                out.push(p.canonical_key());
            }
        }
        out.sort_unstable();
        out.dedup();
    }

    /// Left-right mirror (`c -> cols-1-c`); colors unchanged.
    pub fn mirror(&self) -> VarBoard {
        let mut out = VarBoard {
            rows: self.rows,
            cols: self.cols,
            cells: [0u8; MAX_CELLS],
        };
        let (rows, cols) = (self.rows(), self.cols());
        for r in 0..rows {
            for c in 0..cols {
                out.cells[out.idx(r, cols - 1 - c)] = self.cells[self.idx(r, c)];
            }
        }
        out
    }

    /// Canonical key: min of the board and its mirror. Side-to-move is already
    /// normalized to `US`, so this is the same node identity the solver uses.
    pub fn canonical_key(&self) -> u128 {
        self.key().min(self.mirror().key())
    }

    /// Rebuild a board of the given dimensions from a packed key.
    pub fn from_key(rows: usize, cols: usize, k: u128) -> VarBoard {
        let mut b = VarBoard {
            rows: rows as u8,
            cols: cols as u8,
            cells: [0u8; MAX_CELLS],
        };
        for i in 0..rows * cols {
            b.cells[i] = ((k >> (i * 4)) & 0xF) as u8;
        }
        b
    }

    /// Bijective dense rank of this board (see [`crate::rank`]). Pieces are
    /// conserved (`2·cols` total), so placement+coloring give a bijection. Apply
    /// to a canonical board so mirror images share a rank.
    pub fn rank(&self) -> u64 {
        let ncells = self.ncells();
        let total = 2 * self.cols();
        let (comp, binom) = crate::rank::tables(ncells, total);

        let mut heights = [0u8; MAX_CELLS];
        let mut blacks = [0usize; MAX_CELLS];
        let mut nblack = 0usize;
        let mut slot = 0usize;
        for i in 0..ncells {
            let code = self.cells[i];
            let h = square::height(code);
            heights[i] = h as u8;
            for b in (0..h).rev() {
                if (code >> b) & 1 == 1 {
                    blacks[nblack] = slot;
                    nblack += 1;
                }
                slot += 1;
            }
        }
        let p = crate::rank::rank_placement(&heights[..ncells], total, &comp);
        let c = crate::rank::rank_comb(&blacks[..nblack], &binom);
        p * binom[total][self.cols()] + c
    }

    /// Placement half of [`rank`](Self::rank) only: returns `(placement_rank,
    /// nblack)` and fills `blacks[..nblack]` with the black slot indices, WITHOUT
    /// computing the color rank. Lets `mirror_rank_fast` skip the losing side's
    /// `rank_comb`. Same cell scan as `rank`, so identical placement/slots.
    pub fn place_rank_into(&self, blacks: &mut [usize]) -> (u64, usize) {
        let ncells = self.ncells();
        let total = 2 * self.cols();
        let (comp, _binom) = crate::rank::tables(ncells, total);
        let mut heights = [0u8; MAX_CELLS];
        let mut nblack = 0usize;
        let mut slot = 0usize;
        for i in 0..ncells {
            let code = self.cells[i];
            let h = square::height(code);
            heights[i] = h as u8;
            for b in (0..h).rev() {
                if (code >> b) & 1 == 1 {
                    blacks[nblack] = slot;
                    nblack += 1;
                }
                slot += 1;
            }
        }
        let p = crate::rank::rank_placement(&heights[..ncells], total, &comp);
        (p, nblack)
    }

    /// Just the black slot indices (fills `blacks[..nblack]`, returns `nblack`),
    /// WITHOUT placement or color rank — for `mirror_rank_fast3`'s losing side,
    /// which only needs the mirror's color rank (its placement comes from a table).
    pub fn blacks_into(&self, blacks: &mut [usize]) -> usize {
        let ncells = self.ncells();
        let mut nblack = 0usize;
        let mut slot = 0usize;
        for i in 0..ncells {
            let code = self.cells[i];
            let h = square::height(code);
            for b in (0..h).rev() {
                if (code >> b) & 1 == 1 {
                    blacks[nblack] = slot;
                    nblack += 1;
                }
                slot += 1;
            }
        }
        nblack
    }

    /// Rank broken into parts for inspection: `(placement_rank, color_rank,
    /// heights[ncells], black_slot_indices)`. `rank = P · C(2cols,cols) + C`.
    pub fn rank_parts(&self) -> (u64, u64, Vec<u8>, Vec<usize>) {
        let ncells = self.ncells();
        let total = 2 * self.cols();
        let (comp, binom) = crate::rank::tables(ncells, total);

        let mut heights = vec![0u8; ncells];
        let mut blacks: Vec<usize> = Vec::with_capacity(total);
        let mut slot = 0usize;
        for i in 0..ncells {
            let code = self.cells[i];
            let h = square::height(code);
            heights[i] = h as u8;
            // colors bottom-to-top: bits (h-1) down to 0; bit==1 ⇒ Black.
            for b in (0..h).rev() {
                if (code >> b) & 1 == 1 {
                    blacks.push(slot);
                }
                slot += 1;
            }
        }
        let p = crate::rank::rank_placement(&heights, total, &comp);
        let c = crate::rank::rank_comb(&blacks, &binom);
        (p, c, heights, blacks)
    }

    /// Inverse of [`rank`](Self::rank): reconstruct the board from its rank.
    pub fn unrank(rows: usize, cols: usize, r: u64) -> VarBoard {
        let ncells = rows * cols;
        let total = 2 * cols;
        let (comp, binom) = crate::rank::tables(ncells, total);
        let cbin = binom[total][cols];
        let rc = r % cbin;
        let rp = r / cbin;
        let mut heights = [0u8; MAX_CELLS];
        let mut blacks = [0usize; MAX_CELLS];
        crate::rank::unrank_placement_into(rp, ncells, total, &comp, &mut heights);
        crate::rank::unrank_comb_into(rc, total, cols, &binom, &mut blacks); // ascending slots

        let mut cells = [0u8; MAX_CELLS];
        let mut bi = 0usize;
        let mut slot = 0usize;
        for i in 0..ncells {
            let mut code = 0u8;
            for _ in 0..heights[i] {
                let is_black = bi < cols && blacks[bi] == slot;
                let color = if is_black {
                    bi += 1;
                    Color::Black
                } else {
                    Color::White
                };
                code = square::push(code, color); // pushes bottom-to-top
                slot += 1;
            }
            cells[i] = code;
        }
        VarBoard {
            rows: rows as u8,
            cols: cols as u8,
            cells,
        }
    }
}

impl std::fmt::Debug for VarBoard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "VarBoard {}x{}:", self.rows, self.cols)?;
        for r in 0..self.rows() {
            for c in 0..self.cols() {
                let code = self.cells[self.idx(r, c)];
                let ch = match square::top_color(code) {
                    None => '.',
                    Some(Color::Black) => 'B',
                    Some(Color::White) => 'W',
                };
                write!(f, "{}{} ", ch, square::height(code))?;
            }
            writeln!(f)?;
        }
        Ok(())
    }
}
