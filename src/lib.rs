#![warn(clippy::all, clippy::nursery, clippy::pedantic)]
#![allow(
    clippy::implicit_hasher,
    clippy::missing_errors_doc,
    clippy::must_use_candidate,
    clippy::return_self_not_must_use
)]

pub mod app;
pub mod cli;
pub mod cluster;
pub mod column;
pub mod columns;
pub mod commandstats;
pub mod config;
pub mod credentials;
pub mod discovery;
pub mod hotkeys;
pub mod model;
pub mod overview;
pub mod parse;
pub mod poller;
mod redis_connection;
pub mod registry;
pub mod target_addr;
mod text;
pub mod topology;
pub mod tui;
