//! # management-app
//!
//! A simple application that implements management operations,
//! such as firmware upgrade.
//!
//! It directly implements the APDU and CTAPHID dispatch App interfaces.
#![no_std]

#[macro_use]
extern crate delog;
generate_macros!();

mod admin;
pub use admin::{App, Reboot};
