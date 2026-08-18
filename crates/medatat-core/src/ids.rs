//! Identifier newtypes.
//!
//! `FieldId` is global, stable, and never reused — values attach to it, never to a
//! placement. See `docs/02-DATA-MODEL.md`.

use serde::{Deserialize, Serialize};
use std::fmt;
use uuid::Uuid;

macro_rules! uuid_id {
    ($(#[$m:meta])* $name:ident) => {
        $(#[$m])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub Uuid);

        impl Default for $name {
            fn default() -> Self { Self::nil() }
        }
        impl $name {
            /// A fresh random identity. `Default` is the nil UUID, deliberately distinct.
            #[allow(clippy::new_without_default)]
            pub fn new() -> Self { Self(Uuid::new_v4()) }
            pub fn nil() -> Self { Self(Uuid::nil()) }
            pub fn parse(s: &str) -> Result<Self, uuid::Error> { Ok(Self(Uuid::parse_str(s)?)) }
        }
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { self.0.fmt(f) }
        }
        impl From<Uuid> for $name { fn from(u: Uuid) -> Self { Self(u) } }
    };
}

uuid_id!(
    /// Globally stable field identity. Never reused, never re-typed in place.
    FieldId
);
uuid_id!(CaseId);
uuid_id!(FormId);
uuid_id!(SectionId);

/// Dense index of a field within one `FormDef`. Used on hot paths so lookups are O(1)
/// array indexing rather than hashing a UUID.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct FieldIdx(pub u32);

impl FieldIdx {
    #[inline]
    pub fn as_usize(self) -> usize {
        self.0 as usize
    }
}

/// Per-case revision counter. Assigned by the CaseDO, which is single-threaded, so it is
/// race-free without locking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CaseRev(pub i64);

impl CaseRev {
    pub const ZERO: CaseRev = CaseRev(0);
    #[inline]
    pub fn next(self) -> Self {
        CaseRev(self.0 + 1)
    }
}

impl fmt::Display for CaseRev {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Config version, bumped on every form/section/field mutation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ConfigRev(pub i64);

/// The stored code of a radio/select option. This is what lands in `field_value`; the
/// label is display-only and may change without touching data.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct OptionCode(pub String);

impl OptionCode {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for OptionCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Identifies a user for `updated_by`. Always resolved server-side from the session token,
/// never accepted from a client.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ActorId(pub String);

impl ActorId {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
