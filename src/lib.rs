#![forbid(unsafe_code)]
#![cfg_attr(feature = "strict", deny(warnings))]
#![feature(int_roundings)]
// Clippy philosophy. The goal is to have zero warnings from `cargo clippy` on the main branch.
// Thus silencing all warning that I don't want to fix now. These decisions could be revised.
#![allow(
    clippy::collapsible_else_if,
    clippy::large_enum_variant,
    clippy::manual_is_ascii_check,
    clippy::option_map_unit_fn,
    clippy::too_many_arguments,
    clippy::type_complexity
)]

pub mod algebraic;
pub mod altered_game;
pub mod board;
pub mod chalk;
pub mod client;
pub mod clock;
pub mod coord;
pub mod display;
pub mod event;
pub mod fen;
pub mod force;
pub mod game;
pub mod grid;
pub mod janitor;
pub mod lobby;
pub mod meter;
pub mod pgn;
pub mod piece;
pub mod ping_pong;
pub mod player;
pub mod rules;
pub mod scores;
pub mod server;
pub mod server_helpers;
pub mod server_hooks;
pub mod session;
pub mod session_store;
pub mod starter;
pub mod test_util;
pub mod util;
