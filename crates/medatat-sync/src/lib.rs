//! Delta sync between the local store and the Cloudflare backend.
//!
//! Deliberately not a CRDT. Cases are effectively single-writer in practice, and because
//! sync is off the UI critical path (`docs/adr/0002`), convergence machinery would be
//! complexity with no payoff. See `docs/04-SYNC.md`.

pub mod backoff;
pub mod engine;
pub mod transport;

pub use backoff::delay_for;
pub use engine::{CaseloadStats, DrainReport, SyncEngine, SyncError, SyncState, SyncStatus};
pub use transport::{Transport, TransportError};
