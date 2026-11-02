pub mod adapter;
pub mod app;
pub mod domain;
mod fs_safety;
mod legacy_migration;
pub mod router;
pub mod runtime;
pub mod state_paths;
pub mod store;
pub mod tui;

mod managed_update;
#[cfg(windows)]
mod update;
