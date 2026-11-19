//! Everything a source can fail with.
//!
//! Provenance of a [`SourceError`] is deliberate: [`SourceError::Load`] and
//! [`SourceError::Generation`] wrap external-generator failures, while
//! [`SourceError::Internal`] means a map-core invariant broke. Callers (tile
//! schedulers, the CLI, the UI) should show the first two to the user and log
//! the last as a bug.

/// A failure while loading a world, generating terrain or serving a query.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SourceError {
    /// The world/generator/asset load failed (path, missing file, bad JSON).
    #[error("load failed: {0}")]
    Load(String),
    /// Chunk or column generation failed.
    #[error("generation failed: {0}")]
    Generation(String),
    /// The coordinate lies outside the generatable area.
    #[error("({x}, {z}) is outside the generatable area")]
    OutOfBounds { x: i32, z: i32 },
    /// The operation was cancelled by the caller (tile scheduler).
    #[error("cancelled")]
    Cancelled,
    /// This source does not implement the query (e.g. slices before T09).
    #[error("unsupported: {0}")]
    Unsupported(&'static str),
    /// An internal invariant broke; a bug report, not a user error.
    #[error("internal: {0}")]
    Internal(String),
}

/// The crate convention: every fallible source operation returns this alias.
pub type SourceResult<T> = Result<T, SourceError>;

/// Extracts a readable message from a panic payload.
///
/// The shared conversion for every boundary that must *survive* a panic
/// in code it calls: the scheduler's renderer boundary (T15) and the
/// worldgen loader/query boundaries (T08/T09). Those call sites wrap the
/// call in `catch_unwind` — a documented style-guide deviation, argued
/// per site — and map the payload onto [`SourceError::Generation`] or
/// [`SourceError::Load`] with this message.
pub(crate) fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<String>() {
        return message.clone();
    }
    if let Some(message) = payload.downcast_ref::<&'static str>() {
        return (*message).to_string();
    }
    "<non-string panic>".to_string()
}
