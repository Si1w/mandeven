//! Mandeven library crate.
//!
//! Modules are added here as they come online. The binary entry point
//! lives in `src/main.rs` and pulls from this crate.

pub mod agent;
pub mod bus;
pub mod channels;
pub mod cli;
pub mod command;
pub mod config;
pub mod cron;
pub mod gateway;
pub mod heartbeat;
pub mod hook;
pub mod llm;
pub mod memory;
pub mod prompt;
pub mod security;
pub mod session;
pub mod skill;
pub mod task;
pub mod tools;
pub mod utils;
