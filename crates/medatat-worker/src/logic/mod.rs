//! Request logic as pure functions over narrow traits.
//!
//! Nothing in here touches workerd, D1, KV, or a Durable Object, so all of it is exercised
//! by `cargo test -p medatat-worker` on the host. `routes/` and `store/` are the thin
//! adapters that supply the IO. See `docs/adr/0004-workers-rs.md`.

pub mod auth;
pub mod case_store;
pub mod cases;
pub mod config;
pub mod config_rows;
pub mod values;
