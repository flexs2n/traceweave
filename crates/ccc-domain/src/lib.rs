//! `ccc-domain` — core types for the change-aware context compiler.
//!
//! This crate holds the shared vocabulary of the system: opaque identifier
//! newtypes, the enumeration values fixed by TIS §6.2/§6.3/§7.3/§15.1, the
//! error taxonomy of TIS §13.2, and the canonical entity records.
//!
//! Per SDD §27.1 the domain must not depend on API frameworks; the only
//! dependencies here are serialization and identifier utilities.

pub mod enums;
pub mod error;
pub mod id;
pub mod records;
pub mod util;

pub use enums::*;
pub use error::{CccError, ErrorCode};
pub use id::*;
pub use records::*;
pub use util::{blake3_hex, canonical_json, now_rfc3339};
