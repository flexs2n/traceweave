//! `ccc-storage` — SQLite persistence, artifact store, and store traits.

pub mod artifacts;
pub mod sqlite;
#[doc(hidden)]
pub mod test_support;
pub mod traits;

pub use artifacts::FsArtifactStore;
pub use sqlite::SqliteStore;
pub use traits::*;
