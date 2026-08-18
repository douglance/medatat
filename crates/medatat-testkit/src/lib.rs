//! Synthetic fixtures, doubles, and corpora for the medatat test and benchmark suites.
//!
//! **Synthetic data only, always.** Nothing in this crate reads a clinical source, and
//! every string it emits comes from the fixed word lists in [`words`]. This system does not
//! handle PHI today; the checklist for changing that is `docs/12-PHI-READINESS.md`.
//!
//! Everything is seeded. A form, a case, and a corpus are all pure functions of their seed,
//! so a benchmark that regresses on CI reproduces exactly on a laptop — which is the point,
//! because Benches 1–3 are an M1 gate (`docs/07-TESTING.md`).
//!
//! ```
//! use medatat_testkit::{synthetic_case, synthetic_form};
//!
//! let form = synthetic_form(500);
//! let values = synthetic_case(&form, 42);
//! assert_eq!(values, synthetic_case(&form, 42));
//! ```

pub mod cases;
pub mod clock;
pub mod corpus;
pub mod forms;
pub mod mock;
pub mod rng;
pub mod words;

pub use cases::{
    synthetic_actor, synthetic_case, synthetic_case_id, synthetic_mrn, synthetic_name,
    synthetic_value_rows,
};
pub use clock::{Clock, FakeClock, SystemClock};
pub use corpus::{CorpusCase, CorpusStats, read_corpus_cases, read_corpus_form, seed_corpus};
pub use forms::{DEFAULT_SEED, synthetic_form, synthetic_form_seeded};
pub use mock::{MockError, MockTransport};
pub use rng::Rng;
