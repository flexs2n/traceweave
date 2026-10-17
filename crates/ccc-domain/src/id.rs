//! Opaque identifier newtypes (TIS §4.1).
//!
//! All identifiers are opaque newtypes over UUIDv7 (time-ordered, stable).
//! They serialize as their hyphenated string form and never leak their inner
//! type through public APIs.

use serde::{Deserialize, Serialize};
use std::fmt;
use uuid::Uuid;

macro_rules! define_id {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(Uuid);

        impl $name {
            /// Generates a new identifier (UUIDv7, time-ordered).
            pub fn generate() -> Self {
                Self(Uuid::now_v7())
            }

            /// Wraps an existing UUID (used by tests and persistence layers).
            pub fn from_uuid(value: Uuid) -> Self {
                Self(value)
            }

            /// Returns the inner UUID.
            pub fn as_uuid(&self) -> Uuid {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.0)
            }
        }

        impl std::str::FromStr for $name {
            type Err = uuid::Error;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Ok(Self(Uuid::parse_str(s)?))
            }
        }
    };
}

define_id!(
    /// Identifier of a registered repository.
    RepositoryId
);
define_id!(
    /// Identifier of an immutable repository snapshot.
    SnapshotId
);
define_id!(
    /// Identifier of a file row inside a snapshot.
    FileId
);
define_id!(
    /// Identifier of a symbol extracted for a snapshot.
    SymbolId
);
define_id!(
    /// Identifier of a dependency edge.
    EdgeId
);
define_id!(
    /// Identifier of an evidence record.
    EvidenceId
);
define_id!(
    /// Identifier of a versioned engineering claim.
    ClaimId
);
define_id!(
    /// Identifier of a compiled context bundle.
    BundleId
);
define_id!(
    /// Identifier of a background or synchronous job.
    JobId
);
define_id!(
    /// Identifier of an index generation.
    GenerationId
);
define_id!(
    /// Identifier of a reference row inside a snapshot.
    ReferenceId
);
define_id!(
    /// Identifier of a change-impact report.
    ImpactId
);
define_id!(
    /// Identifier of an immutable verification record.
    VerificationId
);
define_id!(
    /// Identifier of an immutable evaluation run record.
    EvaluationRunId
);
define_id!(
    /// Identifier of a persistent engineering memory record (SDD §5.11).
    MemoryId
);
define_id!(
    /// Identifier of a memory status-transition event (history trail).
    MemoryEventId
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_round_trip_through_serde() {
        let original = SnapshotId::generate();
        let json = serde_json::to_string(&original).expect("serialize id");
        let parsed: SnapshotId = serde_json::from_str(&json).expect("deserialize id");
        assert_eq!(original, parsed);
    }

    #[test]
    fn ids_are_time_ordered() {
        let first = RepositoryId::generate();
        let second = RepositoryId::generate();
        assert!(second > first, "UUIDv7 ids must be time-ordered");
    }

    #[test]
    fn ids_display_and_parse_as_strings() {
        let id = JobId::generate();
        let text = id.to_string();
        let parsed: JobId = text.parse().expect("parse id from display form");
        assert_eq!(id, parsed);
    }

    #[test]
    fn distinct_id_types_are_not_interchangeable_by_name() {
        // The newtypes are distinct nominal types even though the inner value
        // is a UUID; this test documents the intent (no implicit conversion).
        let repo = RepositoryId::generate();
        let snapshot = SnapshotId::from_uuid(repo.as_uuid());
        assert_eq!(repo.as_uuid(), snapshot.as_uuid());
        assert_ne!(
            std::any::type_name::<RepositoryId>(),
            std::any::type_name::<SnapshotId>()
        );
    }
}
