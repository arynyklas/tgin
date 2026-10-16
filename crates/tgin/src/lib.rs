//! The tgin binary: command line, process assembly, the data and admin
//! planes, and hot reload.

pub mod admin;
pub mod app;
pub mod cli;
mod data_server;
pub mod exit;
mod reload;
mod sources;
