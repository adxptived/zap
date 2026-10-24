pub mod batch;
pub mod cli;
pub mod context_menu;
pub mod delete;
pub mod filter;
pub mod journal;
pub mod parallelism;
pub mod path_utils;
pub mod protect;
#[cfg(windows)]
pub mod recycle;
pub mod scan;
pub mod settings;
pub mod shred;
pub mod size;
pub mod stop;
#[cfg(windows)]
mod tree_delete;
pub mod treemap;
#[cfg(windows)]
pub mod winapi;
