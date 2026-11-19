//! The explorer's stable vocabulary.
//!
//! These types are the shared contract between the world sources
//! ([`crate::source`]), the tile pipeline ([`crate::tiles`]), the renderers
//! ([`crate::render`]) and everything built on top of them. They are
//! deliberately plain data — serializable, cheap where small, and free of
//! generator internals, so the rest of the explorer never has to touch the
//! worldgen workspace directly.
//!
//! Seed semantics (numeric-only input, low-32-bit generator truncation) are
//! fixed by `docs/seed-semantics.md`; [`Seed`] and [`SeedError`] implement
//! exactly those rules.

use std::collections::hash_map::DefaultHasher;

use serde::{Deserialize, Serialize};
use thiserror::Error;

// ---------------------------------------------------------------------------
// Seed
// ---------------------------------------------------------------------------

/// A Hytale world seed, parsed from the text a user typed.
///
/// Semantics are fixed by `docs/seed-semantics.md`:
///
/// - Only **numeric** seeds exist. The input must parse as a signed decimal
///   `i64` with Java `Long.parseLong` parity: an optional single leading
///   `+`/`-`, then one or more ASCII digits, the whole string, and **no
///   whitespace trimming**. Hytale has no string-seed path; non-numeric input
///   is rejected with a typed error, never hashed.
/// - World generation sees only the low 32 bits: [`Seed::generator`] is
///   `world as i32`. This truncation is **documented, not an error** — seeds
///   `4294967297` and `1` generate identical terrain, and callers are expected
///   to display both values when they differ.
/// - A [`Seed`] is a pure function of its input text: the same string always
///   yields the same `(input, world, generator)` triple.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Seed {
    input: String,
    world: i64,
    generator: i32,
}

impl Seed {
    /// Parses seed text into a [`Seed`].
    ///
    /// The rules are Java `Long.parseLong` parity: an optional single leading
    /// `+` or `-`, then one or more ASCII digits, forming the entire string.
    /// Leading/trailing whitespace is *not* trimmed, and values outside the
    /// `i64` range are errors. `generator` is always `world as i32` — the
    /// documented low-32-bit truncation, never a parse failure.
    ///
    /// # Examples
    ///
    /// ```
    /// use map_core::types::{Seed, SeedError};
    ///
    /// let seed = Seed::parse("4294967297").expect("a valid numeric seed");
    /// assert_eq!(seed.world(), 4294967297);
    /// assert_eq!(seed.generator(), 1); // documented low-32-bit truncation
    ///
    /// assert_eq!(Seed::parse("abc"), Err(SeedError::NotANumericI64));
    /// ```
    pub fn parse(input: &str) -> Result<Seed, SeedError> {
        if input.is_empty() {
            return Err(SeedError::Empty);
        }

        let bytes = input.as_bytes();
        let (sign, digits) = match bytes[0] {
            b'+' => (1_i128, &bytes[1..]),
            b'-' => (-1_i128, &bytes[1..]),
            _ => (1_i128, bytes),
        };
        if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
            return Err(SeedError::NotANumericI64);
        }

        // Accumulate in `i128` so every value up to and including
        // `i64::MIN` (one more than `i64::MAX` in magnitude) fits; longer
        // digit runs fail the range check below.
        let mut magnitude: i128 = 0;
        for &digit in digits {
            let Some(next) = magnitude
                .checked_mul(10)
                .and_then(|m| m.checked_add(i128::from(digit - b'0')))
            else {
                return Err(SeedError::NotANumericI64);
            };
            magnitude = next;
        }
        let world = i64::try_from(sign * magnitude).map_err(|_| SeedError::NotANumericI64)?;

        Ok(Seed {
            input: input.to_string(),
            world,
            generator: world as i32,
        })
    }

    /// Convenience constructor: converts `input` to text and parses it.
    pub fn new(input: impl Into<String>) -> Result<Self, SeedError> {
        let input = input.into();
        Seed::parse(&input)
    }

    /// The exact text the seed was parsed from, kept verbatim for the UI,
    /// history and share strings.
    pub fn input(&self) -> &str {
        &self.input
    }

    /// The persisted Hytale world seed (full `i64`).
    pub fn world(&self) -> i64 {
        self.world
    }

    /// The generator-visible value: `world as i32`, the low 32 bits.
    ///
    /// This is the only seed value terrain, biomes, features, caves and spawn
    /// points depend on.
    pub fn generator(&self) -> i32 {
        self.generator
    }
}

/// Why a seed string could not be parsed into a [`Seed`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum SeedError {
    /// The input was empty (zero characters); there is nothing to parse.
    #[error("the seed is empty")]
    Empty,
    /// The input was not exactly a signed decimal integer in `i64` range.
    /// Hytale seeds are numeric only; the input is never hashed as a fallback.
    #[error("seeds are numeric (i64); Hytale has no string-seed path")]
    NotANumericI64,
}

// ---------------------------------------------------------------------------
// Ids
// ---------------------------------------------------------------------------

/// The generator/registry id of a biome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BiomeId(pub i32);

/// The generator/registry id of a block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BlockId(pub i32);

/// The generator/registry id of a fluid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FluidId(pub i32);

/// The generator/registry id of a zone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ZoneId(pub i32);

// ---------------------------------------------------------------------------
// Rectangles
// ---------------------------------------------------------------------------

/// An axis-aligned integer rectangle in world XZ space.
///
/// The `max_*` bounds are **inclusive**: a rect from 0 to 5 spans six columns.
/// Every method below respects that convention.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Rect {
    pub min_x: i32,
    pub min_z: i32,
    pub max_x: i32,
    pub max_z: i32,
}

impl Rect {
    /// Creates a rect, asserting that the inclusive max bounds are not
    /// smaller than the min bounds.
    pub fn new(min_x: i32, min_z: i32, max_x: i32, max_z: i32) -> Rect {
        assert!(
            max_x >= min_x && max_z >= min_z,
            "Rect max bounds must not be smaller than min bounds (max is inclusive): \
             min=({min_x}, {min_z}), max=({max_x}, {max_z})",
        );
        Rect {
            min_x,
            min_z,
            max_x,
            max_z,
        }
    }

    /// Creates a rect from a min corner and a size in blocks; the max bounds
    /// are `min + size - 1` (inclusive), so a 1×1 rect has `max == min`.
    pub fn from_dims(min_x: i32, min_z: i32, width: u32, height: u32) -> Rect {
        Rect {
            min_x,
            min_z,
            max_x: min_x + width as i32 - 1,
            max_z: min_z + height as i32 - 1,
        }
    }

    /// The width in blocks: `max_x - min_x + 1` (inclusive max).
    pub fn width(&self) -> u32 {
        (self.max_x - self.min_x + 1) as u32
    }

    /// The height in blocks: `max_z - min_z + 1` (inclusive max).
    pub fn height(&self) -> u32 {
        (self.max_z - self.min_z + 1) as u32
    }

    /// True when the rect covers no block: either bound pair is inverted
    /// (only possible via [`Rect::clamp`] on disjoint input) or the size is
    /// zero on an axis.
    pub fn is_empty(&self) -> bool {
        self.max_x < self.min_x
            || self.max_z < self.min_z
            || self.width() == 0
            || self.height() == 0
    }

    /// True when the block column (`x`, `z`) lies inside the rect,
    /// inclusive of both max bounds.
    pub fn contains(&self, x: i32, z: i32) -> bool {
        x >= self.min_x && x <= self.max_x && z >= self.min_z && z <= self.max_z
    }

    /// True when the two rects share at least one block column. Rects that
    /// merely touch along an edge (one ends at 5, the other starts at 6) do
    /// **not** intersect, because the max bound is inclusive.
    pub fn intersects(&self, other: &Rect) -> bool {
        self.min_x <= other.max_x
            && other.min_x <= self.max_x
            && self.min_z <= other.max_z
            && other.min_z <= self.max_z
    }

    /// Grows the rect by `by` blocks on all four bounds. A negative `by`
    /// shrinks it (and can invert it).
    #[must_use]
    pub fn expand(&self, by: i32) -> Rect {
        Rect {
            min_x: self.min_x - by,
            min_z: self.min_z - by,
            max_x: self.max_x + by,
            max_z: self.max_z + by,
        }
    }

    /// Clamps this rect into `other`: each bound is limited to `other`'s
    /// range. When the rects are disjoint on an axis, the result is
    /// *inverted* on that axis (`min_* > max_*`) rather than panicking — an
    /// empty rect whose `width`/`height` are not meaningful. Call
    /// [`Rect::intersects`] first if you need a well-formed intersection.
    #[must_use]
    pub fn clamp(&self, other: &Rect) -> Rect {
        Rect {
            min_x: self.min_x.max(other.min_x),
            min_z: self.min_z.max(other.min_z),
            max_x: self.max_x.min(other.max_x),
            max_z: self.max_z.min(other.max_z),
        }
    }

    /// The number of sample columns and rows when sampling every `step`
    /// blocks, rounded up so the last sample stays at or before the max
    /// bounds: `(ceil(width / step), ceil(height / step))`.
    #[must_use]
    pub fn sample_dims(&self, step: u32) -> (u32, u32) {
        assert!(
            step >= 1,
            "sample step must be at least 1 block per sample, got {step}",
        );
        (self.width().div_ceil(step), self.height().div_ceil(step))
    }

    /// Iterates the sample coordinates of the rect, every `step` blocks,
    /// row-major (`z` outer, `x` inner): `(min_x + i * step, min_z + j * step)`.
    /// Works for negative origins; samples are not guaranteed to land on the
    /// max bounds unless the size divides evenly.
    pub fn iter_blocks(&self, step: u32) -> impl Iterator<Item = (i32, i32)> {
        let (samples_x, samples_z) = self.sample_dims(step);
        let min_x = self.min_x;
        let min_z = self.min_z;
        let step_i = step as i32;
        (0..samples_z).flat_map(move |j| {
            let z = min_z + j as i32 * step_i;
            (0..samples_x).map(move |i| (min_x + i as i32 * step_i, z))
        })
    }
}

/// An axis-aligned integer box in world XYZ space (max bounds inclusive).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Rect3 {
    pub min_x: i32,
    pub min_y: i32,
    pub min_z: i32,
    pub max_x: i32,
    pub max_y: i32,
    pub max_z: i32,
}

impl Rect3 {
    /// Creates a box, asserting that the inclusive max bounds are not smaller
    /// than the min bounds on any axis.
    pub fn new(min_x: i32, min_y: i32, min_z: i32, max_x: i32, max_y: i32, max_z: i32) -> Rect3 {
        assert!(
            max_x >= min_x && max_y >= min_y && max_z >= min_z,
            "Rect3 max bounds must not be smaller than min bounds (max is inclusive): \
             min=({min_x}, {min_y}, {min_z}), max=({max_x}, {max_y}, {max_z})",
        );
        Rect3 {
            min_x,
            min_y,
            min_z,
            max_x,
            max_y,
            max_z,
        }
    }

    /// Size along X in blocks (inclusive max).
    pub fn size_x(&self) -> u32 {
        (self.max_x - self.min_x + 1) as u32
    }

    /// Size along Y in blocks (inclusive max).
    pub fn size_y(&self) -> u32 {
        (self.max_y - self.min_y + 1) as u32
    }

    /// Size along Z in blocks (inclusive max).
    pub fn size_z(&self) -> u32 {
        (self.max_z - self.min_z + 1) as u32
    }
}

// ---------------------------------------------------------------------------
// Sampled grids
// ---------------------------------------------------------------------------

/// Panics unless `len` matches a `width * height` row-major grid and the step
/// is positive. Shared by every grid constructor.
fn assert_grid_layout(step: u32, width: u32, height: u32, len: usize) {
    assert!(
        step >= 1,
        "grid step must be at least 1 block per sample, got {step}",
    );
    assert_eq!(
        len,
        width as usize * height as usize,
        "grid values must number exactly width * height (row-major): \
         got {len} values for a {width} x {height} grid",
    );
}

/// Emits the shared sampling methods (world coordinates → row-major index
/// into the `Vec` held by `$field`) for a grid type; invoke it *inside* the
/// type's `impl` block.
macro_rules! impl_grid_sampling {
    ($field:ident, $elem:ty) => {
        /// Maps world coordinates to the row-major index
        /// `z_index * width + x_index` of the sample covering them, or
        /// [`None`] when the point falls outside the covered rect.
        ///
        /// `step` is blocks per sample.
        pub fn index_of(&self, world_x: i32, world_z: i32) -> Option<usize> {
            let rel_x = world_x.checked_sub(self.origin_x)?;
            let rel_z = world_z.checked_sub(self.origin_z)?;
            if rel_x < 0 || rel_z < 0 {
                return None;
            }
            let x_index = (rel_x as u32) / self.step;
            let z_index = (rel_z as u32) / self.step;
            if x_index >= self.width || z_index >= self.height {
                return None;
            }
            Some(z_index as usize * self.width as usize + x_index as usize)
        }

        /// Borrows the sample covering the world point, if covered.
        pub fn sample(&self, world_x: i32, world_z: i32) -> Option<&$elem> {
            let index = self.index_of(world_x, world_z)?;
            Some(&self.$field[index])
        }

        /// Mutably borrows the sample covering the world point, if covered.
        pub fn sample_mut(&mut self, world_x: i32, world_z: i32) -> Option<&mut $elem> {
            let index = self.index_of(world_x, world_z)?;
            Some(&mut self.$field[index])
        }
    };
}

/// Implements the shared constructor plus the sampling API for a grid type.
macro_rules! impl_grid {
    ($name:ident, $field:ident, $elem:ty) => {
        impl $name {
            /// Creates a grid whose samples start at (`origin_x`, `origin_z`)
            /// and advance `step` blocks per sample — `width` samples along X,
            /// `height` along Z. `values` are row-major (`z * width + x`) and
            /// must number exactly `width * height`.
            pub fn new(
                origin_x: i32,
                origin_z: i32,
                step: u32,
                width: u32,
                height: u32,
                values: Vec<$elem>,
            ) -> Self {
                assert_grid_layout(step, width, height, values.len());
                Self {
                    origin_x,
                    origin_z,
                    step,
                    width,
                    height,
                    $field: values,
                }
            }

            impl_grid_sampling!($field, $elem);
        }
    };
}

/// Biome ids sampled on a regular grid: `step` blocks per sample, row-major
/// (`z * width + x`), anchored at (`origin_x`, `origin_z`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BiomeGrid {
    pub origin_x: i32,
    pub origin_z: i32,
    pub step: u32,
    pub width: u32,
    pub height: u32,
    pub biomes: Vec<BiomeId>,
}

impl_grid!(BiomeGrid, biomes, BiomeId);

/// Zone ids sampled on a regular grid: `step` blocks per sample, row-major
/// (`z * width + x`), anchored at (`origin_x`, `origin_z`). A sample is
/// [`None`] where the generator reports no zone for the column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZoneGrid {
    pub origin_x: i32,
    pub origin_z: i32,
    pub step: u32,
    pub width: u32,
    pub height: u32,
    pub zones: Vec<Option<ZoneId>>,
}

impl_grid!(ZoneGrid, zones, Option<ZoneId>);

/// Surface heights (in blocks) sampled on a regular grid: `step` blocks per
/// sample, row-major (`z * width + x`), anchored at (`origin_x`, `origin_z`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeightGrid {
    pub origin_x: i32,
    pub origin_z: i32,
    pub step: u32,
    pub width: u32,
    pub height: u32,
    pub heights: Vec<i32>,
}

impl_grid!(HeightGrid, heights, i32);

/// Full surface samples on a regular grid: `step` blocks per sample,
/// row-major (`z * width + x`), anchored at (`origin_x`, `origin_z`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SurfaceGrid {
    pub origin_x: i32,
    pub origin_z: i32,
    pub step: u32,
    pub width: u32,
    pub height: u32,
    pub samples: Vec<SurfaceSample>,
}

impl_grid!(SurfaceGrid, samples, SurfaceSample);

/// One sampled surface column: what the generator reports for the top of the
/// world at (`x`, `z`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SurfaceSample {
    /// World X of the sampled column.
    pub x: i32,
    /// World Z of the sampled column.
    pub z: i32,
    /// Biome at the surface.
    pub biome: BiomeId,
    /// Surface height in blocks.
    pub height: i32,
    /// Block at the surface.
    pub block: BlockId,
    /// Fluid covering the column, if any.
    pub fluid: Option<FluidId>,
    /// Fluid depth in blocks (0 when [`SurfaceSample::fluid`] is [`None`]).
    pub fluid_depth: u16,
    /// Generator-provided packed color tint for the surface block.
    pub tint: i32,
}

/// A horizontal slice of blocks at the world height `y`: `step` blocks per
/// sample, row-major (`z * width + x`), anchored at (`origin_x`, `origin_z`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SliceGrid {
    pub origin_x: i32,
    pub origin_z: i32,
    pub step: u32,
    pub width: u32,
    pub height: u32,
    /// World Y of the slice; every cell lies in this horizontal plane.
    pub y: i32,
    pub cells: Vec<SliceCell>,
}

impl SliceGrid {
    /// Creates a slice grid — like the other grid constructors, plus the
    /// world height `y` of the slice. `values` are row-major
    /// (`z * width + x`) and must number exactly `width * height`.
    pub fn new(
        origin_x: i32,
        origin_z: i32,
        step: u32,
        width: u32,
        height: u32,
        y: i32,
        values: Vec<SliceCell>,
    ) -> Self {
        assert_grid_layout(step, width, height, values.len());
        Self {
            origin_x,
            origin_z,
            step,
            width,
            height,
            y,
            cells: values,
        }
    }

    impl_grid_sampling!(cells, SliceCell);
}

/// One cell of a [`SliceGrid`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SliceCell {
    /// The cell is filled with the given block.
    Solid(BlockId),
    /// The cell is filled with the given fluid.
    Fluid(FluidId),
    /// The cell is empty space.
    Air,
    /// The generator did not report this cell (ungenerated or unsampled).
    Unknown,
}

// ---------------------------------------------------------------------------
// Features
// ---------------------------------------------------------------------------

/// The kind of a point of interest on the map.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum FeatureKind {
    /// A one-of-a-kind prefab.
    UniquePrefab,
    /// A prefab placed by biome decoration.
    BiomePrefab,
    /// A cave entrance.
    CaveEntrance,
    /// The world spawn point.
    Spawn,
    /// A named point of interest.
    Poi,
}

/// A bitmask over [`FeatureKind`], one bit per kind.
///
/// Serializes as its raw `u16` so it can be embedded in serialized filters
/// and saved state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FeatureKindMask(pub u16);

impl FeatureKindMask {
    /// Every kind: the low five bits, one per [`FeatureKind`] variant.
    pub const ALL: Self = Self(0b1_1111);

    /// The bit position of a kind, fixed by an explicit match (not by
    /// discriminant order, so reordering variants cannot silently corrupt
    /// persisted masks).
    fn bit_index(kind: FeatureKind) -> u16 {
        match kind {
            FeatureKind::UniquePrefab => 0,
            FeatureKind::BiomePrefab => 1,
            FeatureKind::CaveEntrance => 2,
            FeatureKind::Spawn => 3,
            FeatureKind::Poi => 4,
        }
    }

    /// A mask selecting exactly one kind.
    #[must_use]
    pub fn from_kind(kind: FeatureKind) -> Self {
        Self(1 << Self::bit_index(kind))
    }

    /// True when the mask selects `kind`.
    pub fn contains(&self, kind: FeatureKind) -> bool {
        self.0 & (1 << Self::bit_index(kind)) != 0
    }

    /// The kinds selected by either mask.
    #[must_use]
    pub fn union(&self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

/// Optional extra information the generator reports about a feature.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeatureMetadata {
    /// Generator-specific variant name, when the feature has one.
    pub variant: Option<String>,
    /// Reported footprint size, when the generator provides one.
    pub size: Option<i32>,
    /// Source asset the feature comes from, when known.
    pub source: Option<String>,
}

/// A filter over the feature catalog.
///
/// Every set criterion must pass (`AND` semantics); unset criteria
/// ([`None`] / the full [`FeatureKindMask::ALL`]) match everything.
///
/// The `min_size` rule: a feature's *effective size* is
/// `metadata.size` when set, otherwise the largest dimension of `bounds`
/// when present; a feature with neither a reported size nor bounds never
/// matches a size-filtered query.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeatureFilter {
    pub kinds: FeatureKindMask,
    pub categories: Option<Vec<String>>,
    pub name_contains: Option<String>,
    pub min_size: Option<i32>,
}

impl FeatureFilter {
    /// True when `feature` passes every criterion of this filter.
    pub fn matches(&self, feature: &Feature) -> bool {
        if !self.kinds.contains(feature.kind) {
            return false;
        }
        if let Some(categories) = &self.categories
            && !categories
                .iter()
                .any(|category| category == &feature.category)
        {
            return false;
        }
        if let Some(needle) = &self.name_contains {
            // An empty needle would match everything, which is never what a
            // user typing a search box means; treat it as no match.
            if needle.is_empty() {
                return false;
            }
            if !feature.name.to_lowercase().contains(&needle.to_lowercase()) {
                return false;
            }
        }
        if let Some(min_size) = self.min_size {
            let Some(effective_size) = Self::effective_size(feature) else {
                return false;
            };
            if effective_size < min_size {
                return false;
            }
        }
        true
    }

    /// The feature's effective size per the `min_size` rule documented on
    /// [`FeatureFilter`]: reported metadata size first, then the largest
    /// bounds dimension, otherwise nothing.
    fn effective_size(feature: &Feature) -> Option<i32> {
        if let Some(size) = feature.metadata.size {
            return Some(size);
        }
        feature
            .bounds
            .as_ref()
            .map(|bounds| bounds.size_x().max(bounds.size_y()).max(bounds.size_z()) as i32)
    }
}

/// A point of interest on the map: a prefab, cave entrance, spawn or POI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Feature {
    pub kind: FeatureKind,
    pub name: String,
    pub category: String,
    /// World position as `(x, y, z)`.
    pub position: (i32, i32, i32),
    /// The feature's world-space footprint, when known.
    pub bounds: Option<Rect3>,
    pub metadata: FeatureMetadata,
}

// ---------------------------------------------------------------------------
// Generator identity
// ---------------------------------------------------------------------------

/// What pinpoints the exact generator output semantics a cache entry or
/// export was produced with.
///
/// Tile keys hash this identity together with the generator seed `i32`; a
/// change to any field invalidates every tile, summary and export keyed to
/// the previous identity (see `docs/seed-semantics.md` §6).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct GeneratorIdentity {
    /// `worldgen_hytale::GENERATOR_VERSION` — bumped when output semantics change.
    pub generator_version: u32,
    /// World folder name + provider version (e.g. `"Default/0.0.0"`).
    pub world_assets: String,
    /// [`palette::PALETTE_VERSION`](crate::palette::PALETTE_VERSION) — bumped
    /// when the committed fallback palette's data changes.
    pub palette_version: u32,
}

impl GeneratorIdentity {
    /// Hashes the three identity fields with
    /// [`std::collections::hash_map::DefaultHasher`]; this is the value tile
    /// keys (T14) hash in.
    ///
    /// The hash is deterministic within a single build but is **not** a
    /// cross-version contract (`DefaultHasher` makes no stability promise):
    /// persist the fields, not this value, when identity must survive an
    /// upgrade.
    #[must_use]
    pub fn stable_hash(&self) -> u64 {
        use std::hash::{Hash, Hasher};

        let mut hasher = DefaultHasher::new();
        self.generator_version.hash(&mut hasher);
        self.world_assets.hash(&mut hasher);
        self.palette_version.hash(&mut hasher);
        hasher.finish()
    }
}

// ---------------------------------------------------------------------------
// Spawn
// ---------------------------------------------------------------------------

/// A player spawn position: world coordinates plus facing yaw.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SpawnPoint {
    pub x: f64,
    pub y: f64,
    pub z: f64,
    pub yaw: f32,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- Rect ---------------------------------------------------------------

    #[test]
    fn rect_contains_respects_inclusive_max_and_negative_origins() {
        let rect = Rect::new(-10, -10, -1, -1);
        assert!(rect.contains(-10, -10), "min corner is inside");
        assert!(rect.contains(-1, -1), "inclusive max corner is inside");
        assert!(rect.contains(-5, -7), "interior is inside");
        assert!(!rect.contains(0, -1), "one past max_x is outside");
        assert!(!rect.contains(-1, 0), "one past max_z is outside");
        assert!(!rect.contains(-11, -10), "one before min_x is outside");
        assert!(!rect.contains(-10, -11), "one before min_z is outside");
    }

    #[test]
    fn rect_intersects_distinguishes_touching_from_overlapping() {
        let base = Rect::new(0, 0, 5, 5);
        // Touching along an edge (base ends at 5, other starts at 6): the
        // inclusive max means these share no block column.
        assert!(!base.intersects(&Rect::new(6, 0, 10, 5)));
        assert!(!base.intersects(&Rect::new(0, 6, 5, 10)));
        // Overlapping at exactly the shared boundary column/row x=5 / z=5.
        assert!(base.intersects(&Rect::new(5, 0, 10, 5)));
        assert!(base.intersects(&Rect::new(0, 5, 5, 10)));
        // Same at a negative origin.
        let negative = Rect::new(-10, -10, -5, -5);
        assert!(!negative.intersects(&Rect::new(-4, -10, 0, -5)));
        assert!(negative.intersects(&Rect::new(-5, -10, 0, -5)));
    }

    #[test]
    fn rect_from_dims_computes_inclusive_max() {
        assert_eq!(Rect::from_dims(2, 3, 5, 4), Rect::new(2, 3, 6, 6));
        assert_eq!(Rect::from_dims(0, 0, 1, 1), Rect::new(0, 0, 0, 0));
        assert_eq!(Rect::from_dims(-4, -2, 3, 2).width(), 3);
        assert_eq!(Rect::from_dims(-4, -2, 3, 2).height(), 2);
    }

    #[test]
    fn rect_expand_grows_all_four_bounds() {
        assert_eq!(Rect::new(0, 0, 4, 4).expand(2), Rect::new(-2, -2, 6, 6));
        assert_eq!(Rect::new(-3, -3, 3, 3).expand(1), Rect::new(-4, -4, 4, 4));
        // A negative `by` shrinks.
        assert_eq!(Rect::new(0, 0, 4, 4).expand(-1), Rect::new(1, 1, 3, 3));
    }

    #[test]
    fn rect_sample_dims_rounds_up() {
        // 5 x 4 blocks, step 2 → 3 x 2 samples (ceil rounding).
        assert_eq!(Rect::new(0, 0, 4, 3).sample_dims(2), (3, 2));
        assert_eq!(Rect::new(0, 0, 4, 3).sample_dims(1), (5, 4));
        // A step larger than the rect still yields one sample per axis.
        assert_eq!(Rect::new(0, 0, 4, 3).sample_dims(8), (1, 1));
        // Non-square with negative origin.
        assert_eq!(Rect::new(-7, -2, -3, 2).sample_dims(2), (3, 3));
    }

    #[test]
    fn rect_iter_blocks_negative_origin_step_two() {
        // 6 x 4 blocks starting at (-5, -3), step 2 → 3 x 2 = 6 samples,
        // row-major (z outer): xs = -5, -3, -1 and zs = -3, -1.
        let rect = Rect::new(-5, -3, 0, 0);
        let sampled: Vec<(i32, i32)> = rect.iter_blocks(2).collect();
        assert_eq!(
            sampled,
            vec![(-5, -3), (-3, -3), (-1, -3), (-5, -1), (-3, -1), (-1, -1),],
        );
        assert_eq!(sampled.first(), Some(&(-5, -3)));
        assert_eq!(sampled.last(), Some(&(-1, -1)));
    }

    // -- Grids --------------------------------------------------------------

    #[test]
    fn height_grid_sample_and_index_of_at_edges() {
        // 3 x 2 samples, step 2, negative origin: covers x in [-5, 0],
        // z in [-3, 0].
        let grid = HeightGrid::new(-5, -3, 2, 3, 2, vec![0, 1, 2, 3, 4, 5]);

        // Exact edges, inclusive on both bounds.
        assert_eq!(grid.sample(-5, -3), Some(&0));
        assert_eq!(grid.sample(0, -3), Some(&2));
        assert_eq!(grid.sample(-5, 0), Some(&3));
        assert_eq!(grid.sample(0, 0), Some(&5));
        assert_eq!(grid.sample(-3, -1), Some(&4));
        assert_eq!(grid.index_of(-3, -1), Some(4));
        assert_eq!(grid.index_of(0, 0), Some(5));

        // One past each edge.
        assert_eq!(grid.sample(-6, -3), None);
        assert_eq!(grid.sample(1, -3), None);
        assert_eq!(grid.sample(-5, -4), None);
        assert_eq!(grid.sample(-5, 1), None);
        assert_eq!(grid.index_of(1, 0), None);
        assert_eq!(grid.index_of(-5, 1), None);
    }

    #[test]
    fn height_grid_sample_mut_updates_in_place() {
        let mut grid = HeightGrid::new(-5, -3, 2, 3, 2, vec![0, 1, 2, 3, 4, 5]);
        *grid.sample_mut(0, 0).expect("covered point") = 99;
        assert_eq!(grid.sample(0, 0), Some(&99));
        assert_eq!(grid.sample_mut(1, 0), None);
    }

    #[test]
    #[should_panic(expected = "grid values must number exactly width * height")]
    fn grid_new_asserts_on_wrong_value_count() {
        let _ = HeightGrid::new(-5, -3, 2, 3, 2, vec![0; 5]);
    }

    // -- Seed ---------------------------------------------------------------

    #[test]
    fn seed_parse_i32_boundaries_map_to_themselves() {
        for input in ["0", "1", "-1", "2147483647", "-2147483648"] {
            let seed = Seed::parse(input).expect("an i32-sized seed always parses");
            assert_eq!(seed.input(), input);
            let world: i64 = input.parse().expect("test input is a valid i64");
            assert_eq!(seed.world(), world);
            assert_eq!(seed.generator(), world as i32, "no truncation in i32 range");
        }
        // Full i64 range is accepted too.
        let min = Seed::parse("-9223372036854775808").expect("i64::MIN parses");
        assert_eq!(min.world(), i64::MIN);
    }

    #[test]
    fn seed_parse_truncates_low_32_bits() {
        let seed = Seed::parse("4294967297").expect("i64-range seed");
        assert_eq!(seed.world(), 4294967297);
        assert_eq!(seed.generator(), 1, "documented low-32-bit truncation");
        assert_eq!(seed.input(), "4294967297");
    }

    #[test]
    fn seed_parse_i64_max_wraps_generator_to_minus_one() {
        let seed = Seed::parse("9223372036854775807").expect("i64::MAX parses");
        assert_eq!(seed.world(), i64::MAX);
        assert_eq!(seed.generator(), -1, "low 32 bits of i64::MAX are all ones");
    }

    #[test]
    fn seed_parse_rejects_non_numeric_input() {
        assert_eq!(Seed::parse(""), Err(SeedError::Empty));
        // No whitespace trimming: these are numeric-format errors, not empty.
        assert_eq!(Seed::parse(" "), Err(SeedError::NotANumericI64));
        assert_eq!(Seed::parse("abc"), Err(SeedError::NotANumericI64));
        assert_eq!(Seed::parse("12x"), Err(SeedError::NotANumericI64));
        // One past i64::MAX.
        assert_eq!(
            Seed::parse("9223372036854775808"),
            Err(SeedError::NotANumericI64),
        );
        // Signs without digits, and embedded signs, are numeric errors too.
        assert_eq!(Seed::parse("-"), Err(SeedError::NotANumericI64));
        assert_eq!(Seed::parse("1-2"), Err(SeedError::NotANumericI64));
    }

    #[test]
    fn seed_parse_accepts_leading_plus() {
        let seed = Seed::parse("+5").expect("Java Long.parseLong accepts a leading +");
        assert_eq!(seed.world(), 5);
        assert_eq!(seed.generator(), 5);
        assert_eq!(seed.input(), "+5");
    }

    #[test]
    fn seed_is_a_pure_function_of_its_input() {
        let a = Seed::parse("4294967297").expect("parses");
        let b = Seed::parse("4294967297").expect("parses");
        assert_eq!(a, b);
        let via_new = Seed::new(String::from("4294967297")).expect("parses");
        assert_eq!(a, via_new);
    }

    // -- Features -----------------------------------------------------------

    fn feature(kind: FeatureKind, name: &str, category: &str) -> Feature {
        Feature {
            kind,
            name: name.to_string(),
            category: category.to_string(),
            position: (0, 0, 0),
            bounds: None,
            metadata: FeatureMetadata {
                variant: None,
                size: None,
                source: None,
            },
        }
    }

    fn unrestricted_filter() -> FeatureFilter {
        FeatureFilter {
            kinds: FeatureKindMask::ALL,
            categories: None,
            name_contains: None,
            min_size: None,
        }
    }

    #[test]
    fn feature_kind_mask_bits_and_union() {
        assert_eq!(FeatureKindMask::ALL.0, 0b1_1111);
        for kind in [
            FeatureKind::UniquePrefab,
            FeatureKind::BiomePrefab,
            FeatureKind::CaveEntrance,
            FeatureKind::Spawn,
            FeatureKind::Poi,
        ] {
            assert!(FeatureKindMask::ALL.contains(kind));
            let single = FeatureKindMask::from_kind(kind);
            assert!(single.contains(kind), "{kind:?} bit set in its own mask");
            assert!(!single.contains(FeatureKind::Spawn) || kind == FeatureKind::Spawn);
        }
        let prefabs = FeatureKindMask::from_kind(FeatureKind::UniquePrefab)
            .union(FeatureKindMask::from_kind(FeatureKind::BiomePrefab));
        assert!(prefabs.contains(FeatureKind::UniquePrefab));
        assert!(prefabs.contains(FeatureKind::BiomePrefab));
        assert!(!prefabs.contains(FeatureKind::CaveEntrance));
    }

    #[test]
    fn feature_filter_matches_kind_mask() {
        let feature = feature(FeatureKind::CaveEntrance, "cave", "caves");
        let mut filter = unrestricted_filter();
        assert!(filter.matches(&feature));

        filter.kinds = FeatureKindMask::from_kind(FeatureKind::CaveEntrance);
        assert!(filter.matches(&feature));
        filter.kinds = FeatureKindMask::from_kind(FeatureKind::Spawn);
        assert!(!filter.matches(&feature));
    }

    #[test]
    fn feature_filter_category_is_exact_match() {
        let feature = feature(FeatureKind::Poi, "ruin", "structures");
        let mut filter = unrestricted_filter();
        filter.categories = Some(vec![String::from("dungeons"), String::from("structures")]);
        assert!(filter.matches(&feature));
        filter.categories = Some(vec![String::from("dungeons")]);
        assert!(!filter.matches(&feature));
    }

    #[test]
    fn feature_filter_name_contains_is_case_insensitive() {
        let feature = feature(FeatureKind::Poi, "Sunken RUIN", "structures");
        let mut filter = unrestricted_filter();
        filter.name_contains = Some(String::from("ruin"));
        assert!(filter.matches(&feature));
        filter.name_contains = Some(String::from("SUNKEN"));
        assert!(filter.matches(&feature));
        filter.name_contains = Some(String::from("keep"));
        assert!(!filter.matches(&feature));
        // An empty needle never matches.
        filter.name_contains = Some(String::new());
        assert!(!filter.matches(&feature));
    }

    #[test]
    fn feature_filter_min_size_uses_metadata_then_bounds() {
        let mut feature = feature(FeatureKind::BiomePrefab, "hut", "structures");

        // No size and no bounds: never matches a size-filtered query.
        let mut filter = unrestricted_filter();
        filter.min_size = Some(5);
        assert!(!filter.matches(&feature));

        // Metadata size wins when set.
        feature.metadata.size = Some(10);
        assert!(filter.matches(&feature));
        filter.min_size = Some(11);
        assert!(!filter.matches(&feature));

        // Without metadata size, the largest bounds dimension is used
        // (largest dimension of the 4 x 10 x 6 box is 10).
        feature.metadata.size = None;
        feature.bounds = Some(Rect3::new(0, 0, 0, 3, 9, 5));
        filter = unrestricted_filter();
        filter.min_size = Some(10);
        assert!(filter.matches(&feature));
        filter.min_size = Some(11);
        assert!(!filter.matches(&feature));
    }

    // -- GeneratorIdentity --------------------------------------------------

    #[test]
    fn generator_identity_stable_hash_is_deterministic() {
        let identity = GeneratorIdentity {
            generator_version: 1,
            world_assets: String::from("Default/0.0.0"),
            palette_version: 0,
        };
        assert_eq!(identity.stable_hash(), identity.stable_hash());
        let other = GeneratorIdentity {
            world_assets: String::from("Default/0.0.1"),
            ..identity.clone()
        };
        assert_ne!(identity.stable_hash(), other.stable_hash());
    }
}
