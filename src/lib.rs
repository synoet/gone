//! `gone`: run a registered teardown command when a watched directory is
//! deleted. See the README for the why; `model` for the state machine;
//! `daemon` for detection.

pub mod cli;
pub mod commands;
pub mod core;
pub mod daemon;
pub mod events;
pub mod exec;
pub mod migrate;
pub mod model;
pub mod presence;
pub mod store;
pub mod time;
