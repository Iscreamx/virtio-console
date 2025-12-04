#![no_std]

//! This module is designed for an environment where the standard library is not available (`no_std`).

extern crate alloc;
#[macro_use]
extern crate log;
mod config;
mod queue;
mod console;

pub use crate::console::VirtioConsole;