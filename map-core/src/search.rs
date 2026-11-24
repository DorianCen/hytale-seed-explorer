//! The search engines: ring-based streaming queries over a source.
//!
//! - [`feature`] — feature search (T24): expanding rings of regions around a
//!   center, streaming nearest-first results with cancellation.
//! - Biome search (T25) will join as a sibling module; both share the
//!   event/handle conventions established here (streaming events, a bounded
//!   channel, a cancel token owned by the caller).
//!
//! Everything here is headless and GUI-free: the UI (T26) owns the text
//! boxes, result list and recv loops; this module only produces data.

pub mod feature;
