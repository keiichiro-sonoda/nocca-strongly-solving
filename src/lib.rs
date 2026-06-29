//! NOCCA×NOCCA — size-parametric **strong solving** by retrograde analysis (the
//! value + distance-to-mate of *every* position), with an independent reproduction
//! of the full 6×5 result (Yamamoto–Hoki, GPW 2022).
//!
//! Core pieces:
//! - a correct size-parametric engine ([`varboard`], [`position`], [`moves`],
//!   [`color`], [`square`]) — the "try-type" win rule;
//! - a dense mirror-canonical ranking ([`mirror_rank`], [`rank`]) over the full
//!   combinatorial space;
//! - the **strong solve**: a bounded all-in-memory 2-bit retrograde
//!   ([`inmem_retro`]) that solves every 6×5 representative, with two cross-
//!   validating retrograde oracles — an in-RAM one ([`retro`]) and a disk-based
//!   external-sort one ([`disk_retro`], [`extsort`]); [`inmem_retro`] also holds a
//!   forward reachability BFS + projection used to compare with paper B's
//!   reachable-set tables (it does not restrict the solution);
//! - [`solver`]: a forward df-pn *weak*-solver — an earlier attempt at the initial
//!   position that diverged on draw-prone positions (motivating the retrograde);
//!   kept for the methodological contrast and the shared `Value` type.
//!
//! See the README for the reproduction story and how to run it.

pub mod color;
pub mod square;
pub mod moves;
pub mod position;
pub mod solver;
pub mod varboard;
pub mod retro;
pub mod extsort;
pub mod disk_retro;
pub mod rank;
pub mod mirror_rank;
pub mod inmem_retro;

pub use color::Color;
pub use moves::{Move, MoveList};
pub use position::Position;
pub use solver::{
    solve_dfpn, solve_dfpn_verbose, solve_root, Dfpn, Goal, Progress, Proven, Solver, Tt, Value,
};
