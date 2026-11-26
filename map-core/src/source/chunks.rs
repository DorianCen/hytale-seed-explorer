//! Compact per-chunk summaries (T09) — the "expensive half" of
//! [`WorldSource`](crate::source::WorldSource): surface blocks, fluid depth
//! and horizontal slices, without retaining `GeneratedChunk` buffers.
//!
//! A generated chunk costs ~6.6 MiB resident (five 327,680-entry arrays plus
//! fluids — measured in the T01 audit §1); a 256×256-block tile would retain
//! ~420 MiB if chunks were kept whole. A [`ChunkSummary`] is ~22 KB and
//! serves every resolution step (summaries are keyed by chunk, never by
//! step). Caches are namespaced by [`SummaryFidelity`](crate::source::SummaryFidelity)
//! since T29: a surface-only summary (caves/prefabs skipped) can never
//! answer a full-fidelity request. Slices are extracted on demand into a
//! separate small cache keyed by `(chunk, y)` and are always full-fidelity
//! (T28's cave slices need real caves).
//!
//! Extraction definitions (documented so T17/T20/T21 can rely on them):
//!
//! - `height`: the y of the **topmost non-air block** of the column (the
//!   game-visible surface, *not* the generator's noise height — prefabs,
//!   covers and plants sit on top of it). `-1` for an empty column.
//! - `block`: that topmost block's id (plants included; the renderer
//!   filters by opacity through the catalog if it wants solid-only).
//! - `fluid`: the topmost fluid id in the column (`0` = none).
//! - `fluid_depth`: the contiguous fluid run starting at the topmost fluid
//!   cell (a block or air inside the run ends it).
//! - `cave_cells`: air cells strictly below the surface block (a coarse
//!   cave-volume proxy for T28's density map).

use std::sync::Arc;

use worldgen_model::chunk::GeneratedChunk;

use crate::source::SummaryFidelity;
use crate::types::{BlockId, FluidId, SliceCell};

/// Chunk edge in blocks (the generator's `ChunkUtil.SIZE`).
pub const CHUNK_SIZE: i32 = 32;
/// World height in blocks (the generator's `ChunkUtil.HEIGHT`).
pub const CHUNK_HEIGHT: i32 = 320;
const COLUMNS: usize = (CHUNK_SIZE * CHUNK_SIZE) as usize;

/// The chunk containing a world block coordinate (floor division; arithmetic
/// shift is exact for negatives).
#[must_use]
pub fn chunk_of(x: i32) -> i32 {
    x >> 5
}

/// The chunk-local coordinate of a world block coordinate (`0..32`, correct
/// for negative world coordinates).
#[must_use]
pub fn local_of(x: i32) -> i32 {
    x & 31
}

/// Cache key: identity + seed + chunk coordinates. A seed change or a
/// generator-version bump must never hit a stale summary.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ChunkKey {
    /// Stable hash of the `GeneratorIdentity` (T07).
    pub identity: u64,
    /// The generator-visible seed (low 32 bits, T02).
    pub generator_seed: i32,
    pub cx: i32,
    pub cz: i32,
}

/// Cache key for an extracted horizontal slice.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SliceKey {
    pub chunk: ChunkKey,
    pub y: i32,
}

/// Cache key for a chunk summary: the chunk plus the fidelity it was
/// generated at (T29).
///
/// The fidelity is part of the key on purpose: a [`SummaryFidelity::SurfaceOnly`]
/// summary skips caves and prefabs, so it must never satisfy a
/// [`SummaryFidelity::Full`] request. Same chunk, two fidelities → two
/// entries. [`ChunkKey`] keeps its identity-only meaning for the slice
/// cache; slices are always extracted from a full generation (T28's cave
/// slices need real caves).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SummaryKey {
    pub chunk: ChunkKey,
    pub fidelity: SummaryFidelity,
}

/// Compact per-chunk surface data; the column layout is row-major
/// `(z << 5) | x`, matching the generator's `ChunkUtil.indexColumn`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkSummary {
    pub cx: i32,
    pub cz: i32,
    /// Topmost non-air block y per column; `-1` = empty column.
    pub height: [i16; COLUMNS],
    /// Topmost non-air block id per column; `0` = empty.
    pub block: [BlockId; COLUMNS],
    /// Topmost fluid id per column; `0` = none.
    pub fluid: [FluidId; COLUMNS],
    /// Contiguous fluid cells from the topmost fluid cell downward.
    pub fluid_depth: [u16; COLUMNS],
    /// Per-column tint id.
    pub tint: [i32; COLUMNS],
    /// Per-column environment id.
    pub environment: [i32; COLUMNS],
    /// Air cells strictly below the surface block (cave-volume proxy).
    pub cave_cells: [u16; COLUMNS],
}

impl ChunkSummary {
    /// Resident bytes of this summary (the cache weigher's unit).
    #[must_use]
    pub fn byte_size() -> u64 {
        (COLUMNS * (2 + 4 + 4 + 2 + 4 + 4 + 2)) as u64
    }

    /// The column at chunk-local `(lx, lz)`; `None` outside `0..32`.
    #[must_use]
    pub fn column(&self, lx: i32, lz: i32) -> Option<SummaryColumn> {
        if !(0..CHUNK_SIZE).contains(&lx) || !(0..CHUNK_SIZE).contains(&lz) {
            return None;
        }
        let index = column_index(lx, lz);
        Some(SummaryColumn {
            height: self.height[index],
            block: self.block[index],
            fluid: self.fluid[index],
            fluid_depth: self.fluid_depth[index],
            tint: self.tint[index],
            environment: self.environment[index],
            cave_cells: self.cave_cells[index],
        })
    }
}

/// One column of a [`ChunkSummary`] (a copy; summaries stay behind `Arc`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SummaryColumn {
    pub height: i16,
    pub block: BlockId,
    pub fluid: FluidId,
    pub fluid_depth: u16,
    pub tint: i32,
    pub environment: i32,
    pub cave_cells: u16,
}

/// Row-major column index: `(z << 5) | x` (the generator's convention).
#[must_use]
pub fn column_index(lx: i32, lz: i32) -> usize {
    ((lz << 5) | lx) as usize
}

/// One extracted horizontal slice: a `SliceCell` per column, row-major.
pub type ChunkSlice = Arc<[SliceCell]>;

/// Extracts a summary from a generated chunk (pure; shared by pool workers
/// and tests). The chunk is dropped by the caller afterwards.
#[must_use]
pub fn extract_summary(chunk: &GeneratedChunk, cx: i32, cz: i32) -> ChunkSummary {
    let mut summary = ChunkSummary {
        cx,
        cz,
        height: [-1; COLUMNS],
        block: [BlockId(0); COLUMNS],
        fluid: [FluidId(0); COLUMNS],
        fluid_depth: [0; COLUMNS],
        tint: [0; COLUMNS],
        environment: [0; COLUMNS],
        cave_cells: [0; COLUMNS],
    };
    let environment_columns = chunk.environment_column();
    for lz in 0..CHUNK_SIZE {
        for lx in 0..CHUNK_SIZE {
            let index = column_index(lx, lz);
            let mut top_block: Option<(i32, i32)> = None;
            let mut top_fluid: Option<(i32, i16)> = None;
            let mut cave_cells: u16 = 0;
            let mut fluid_depth: u16 = 0;
            let mut counting_depth = false;
            for y in (0..CHUNK_HEIGHT).rev() {
                let block_id = chunk.get_block(lx, y, lz);
                let fluid_id = chunk.get_fluid_id(lx, y, lz);
                if top_block.is_none() && block_id != 0 {
                    top_block = Some((block_id, y));
                }
                if top_fluid.is_none() && fluid_id != 0 {
                    top_fluid = Some((fluid_id, y as i16));
                    counting_depth = true;
                }
                if counting_depth {
                    if fluid_id != 0 {
                        fluid_depth += 1;
                    } else {
                        counting_depth = false;
                    }
                }
                if top_block.is_some() && block_id == 0 && fluid_id == 0 {
                    cave_cells += 1;
                }
            }
            if let Some((block_id, y)) = top_block {
                summary.height[index] = y as i16;
                summary.block[index] = BlockId(block_id);
            }
            if let Some((fluid_id, _)) = top_fluid {
                summary.fluid[index] = FluidId(fluid_id);
            }
            summary.fluid_depth[index] = fluid_depth;
            summary.cave_cells[index] = cave_cells;
            summary.tint[index] = chunk.get_tint(lx, lz);
            summary.environment[index] = environment_columns[index];
        }
    }
    summary
}

/// The T28 cave-density fraction of one column: [`SummaryColumn::cave_cells`]
/// divided by the column's topmost non-air block Y, the number of cells
/// strictly below that block. An empty or surface-at-Y-0 column has no
/// below-surface volume and reports `0.0`; the result is clamped to
/// `0.0..=1.0`.
///
/// The numerator counts air only — fluid cells below the surface are not
/// caves — and it includes every air pocket below the top block (caves,
/// overhangs and prefab voids). It is a cave-volume proxy, not a cave-type
/// classifier; this definition is the metric version in
/// [`CAVE_DENSITY_MODE`](crate::render::caves::CAVE_DENSITY_MODE).
#[must_use]
pub fn cave_fraction(column: &SummaryColumn) -> f32 {
    let height = i32::from(column.height);
    if height <= 0 {
        return 0.0;
    }
    (f32::from(column.cave_cells) / height as f32).clamp(0.0, 1.0)
}

/// Extracts the horizontal slice at world `y` (pure; `y` outside
/// `0..CHUNK_HEIGHT` yields `Air` everywhere, matching the chunk's
/// out-of-height behavior).
#[must_use]
pub fn extract_slice(chunk: &GeneratedChunk, y: i32) -> ChunkSlice {
    let mut cells = Vec::with_capacity(COLUMNS);
    for lz in 0..CHUNK_SIZE {
        for lx in 0..CHUNK_SIZE {
            let block_id = chunk.get_block(lx, y, lz);
            let fluid_id = chunk.get_fluid_id(lx, y, lz);
            cells.push(if block_id != 0 {
                SliceCell::Solid(BlockId(block_id))
            } else if fluid_id != 0 {
                SliceCell::Fluid(FluidId(fluid_id))
            } else {
                SliceCell::Air
            });
        }
    }
    Arc::from(cells.into_boxed_slice())
}

/// The summary cache: byte-weighed (capacity is a bytes budget).
pub type SummaryCache = moka::sync::Cache<SummaryKey, Arc<ChunkSummary>>;

/// The slice cache: entry-weighed (a small "last few Y values" cache).
pub type SliceCache = moka::sync::Cache<SliceKey, ChunkSlice>;

/// Default byte budget for summaries (plan §9: 256 MB separate from the
/// tile cache budget).
pub const DEFAULT_SUMMARY_BUDGET_BYTES: u64 = 256 * 1024 * 1024;

/// How many (chunk, y) slices to keep before eviction.
pub const DEFAULT_SLICE_CACHE_ENTRIES: u64 = 64;

#[must_use]
pub fn summary_cache(budget_bytes: u64) -> SummaryCache {
    moka::sync::Cache::builder()
        .weigher(|_key, _value: &Arc<ChunkSummary>| {
            u32::try_from(ChunkSummary::byte_size()).expect("22 KB fits u32")
        })
        .max_capacity(budget_bytes)
        .build()
}

#[must_use]
pub fn slice_cache(entries: u64) -> SliceCache {
    moka::sync::Cache::builder().max_capacity(entries).build()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The extraction contract on a hand-built chunk: top block, fluid
    /// depth run, cave counting and the row-major index.
    #[test]
    fn extraction_definitions_hold_on_a_built_chunk() {
        let mut chunk = GeneratedChunk::with_coordinates(0, 1, -1);
        // World (x, z) = (1, -1) is chunk (0, -1) local (1, 31): a column
        // with a stone floor at y=64, water at y=70..74, air in between,
        // and a cave hole at y=40.
        let (lx, lz) = (1, 31);
        let index = column_index(lx, lz);
        chunk.set_block(lx, 64, lz, 7, 0, 0);
        for y in 70..75 {
            chunk.set_fluid(lx, y, lz, 11, 8);
        }
        chunk.set_block(lx, 40, lz, 7, 0, 0);
        chunk.set_block(lx, 39, lz, 7, 0, 0);

        let summary = extract_summary(&chunk, 0, -1);
        assert_eq!(summary.height[index], 64);
        assert_eq!(summary.block[index], BlockId(7));
        assert_eq!(summary.fluid[index], FluidId(11));
        assert_eq!(summary.fluid_depth[index], 5);
        // Air strictly below the top block (y < 64): 64 cells minus the two
        // floor blocks at 39/40 = 62 cave-counted cells.
        assert_eq!(summary.cave_cells[index], 62);
        let column = summary.column(lx, lz).expect("in-chunk");
        assert_eq!(column.height, 64);
        assert_eq!(
            cave_fraction(&column),
            62.0 / 64.0,
            "the density denominator is the top block's Y (cells below it)"
        );
        // Out-of-chunk access is typed None, never a panic.
        assert!(summary.column(32, 0).is_none());
        assert!(summary.column(-1, 0).is_none());

        // An empty column has no below-surface volume; a surface at Y=0
        // has none either, so both report zero rather than dividing by it.
        let empty = SummaryColumn {
            height: -1,
            block: BlockId(0),
            fluid: FluidId(0),
            fluid_depth: 0,
            tint: 0,
            environment: 0,
            cave_cells: 0,
        };
        assert_eq!(cave_fraction(&empty), 0.0);
        let surface_at_zero = SummaryColumn { height: 0, ..empty };
        assert_eq!(cave_fraction(&surface_at_zero), 0.0);
    }

    #[test]
    fn chunk_math_handles_negative_coordinates() {
        assert_eq!(chunk_of(-1), -1);
        assert_eq!(local_of(-1), 31);
        assert_eq!(chunk_of(-382), -12);
        assert_eq!(local_of(-382), 2);
        assert_eq!(chunk_of(963), 30);
        assert_eq!(local_of(963), 3);
    }

    fn empty_summary(cx: i32, cz: i32) -> ChunkSummary {
        ChunkSummary {
            cx,
            cz,
            height: [-1; COLUMNS],
            block: [BlockId(0); COLUMNS],
            fluid: [FluidId(0); COLUMNS],
            fluid_depth: [0; COLUMNS],
            tint: [0; COLUMNS],
            environment: [0; COLUMNS],
            cave_cells: [0; COLUMNS],
        }
    }

    /// T29's correctness core: the summary cache namespaces by fidelity, so
    /// a surface-only entry can never satisfy a full request on the same
    /// chunk. Same chunk, two fidelities → two entries.
    #[test]
    fn fidelity_surface_only_summary_never_serves_a_full_request() {
        let cache = summary_cache(8 * 1024 * 1024);
        let chunk = ChunkKey {
            identity: 0xDEAD_BEEF,
            generator_seed: 7,
            cx: 3,
            cz: -2,
        };
        let surface = SummaryKey {
            chunk: chunk.clone(),
            fidelity: SummaryFidelity::SurfaceOnly,
        };
        let full = SummaryKey {
            chunk: chunk.clone(),
            fidelity: SummaryFidelity::Full,
        };

        cache.insert(surface.clone(), Arc::new(empty_summary(3, -2)));
        assert!(cache.get(&surface).is_some(), "the surface-only entry hits");
        assert!(
            cache.get(&full).is_none(),
            "a surface-only summary must never satisfy a full request"
        );

        cache.insert(full.clone(), Arc::new(empty_summary(3, -2)));
        cache.run_pending_tasks();
        assert!(cache.get(&full).is_some(), "the full entry hits on its own");
        assert!(
            cache.get(&surface).is_some(),
            "the two fidelities coexist as separate entries"
        );
        assert_eq!(cache.entry_count(), 2, "one entry per fidelity");
    }
}
