//! The hover inspector (T18): debounced background surface sampling, the
//! readout format and the go-to dialog's parsing.
//!
//! # Sampling policy
//!
//! The UI thread never queries the source: `surface_at` can generate a whole
//! chunk summary, so a pointer move must not pay for it.
//!
//! - **Debounce** — the cursor has to settle for `HOVER_DEBOUNCE` before a
//!   request is issued; sweeping across the map costs nothing.
//! - **One in flight** — at most one request is alive at a time, so a slow
//!   sample never piles workers up behind the cursor.
//! - **Generations** — every request carries a fresh generation from a
//!   counter that only grows, and a reply is applied only when its
//!   generation matches the in-flight one. A stale reply therefore never
//!   overwrites a newer column (and [`InspectorState::reset`] deliberately
//!   does *not* restart the counter, so pre-reset stragglers stay stale
//!   across a source change).
//! - **LRU** — recent samples live in a small `HOVER_LRU_CAPACITY`-entry
//!   cache keyed by block column, so moving back over a visited column is
//!   instant and never re-queries.
//! - **Source lifetime** — samples belong to exactly one source (its biome
//!   ids and block names are meaningless under another); the app
//!   invalidates the inspector whenever the source changes.
//!
//! The sample itself is kept raw: id → name resolution happens at display
//! time through the loaded source's catalogs, so a late-arriving catalog
//! fix is never masked by a cached display string.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use map_core::catalog::{BiomeCatalog, BlockCatalog, FluidCatalog};
use map_core::error::{SourceError, SourceResult};
use map_core::types::{BiomeId, BlockId, FluidId, SurfaceSample};

use crate::labels;

/// How long the cursor must rest on a column before one background sample
/// is issued.
const HOVER_DEBOUNCE: Duration = Duration::from_millis(50);

/// How many recent hover samples are kept, so revisiting a column is free.
const HOVER_LRU_CAPACITY: usize = 16;

/// The go-to error when the fields do not supply two numbers at all.
const GO_TO_NEEDS_TWO: &str = "enter two numbers, like -1203 455";

/// What a hover worker sends home.
#[derive(Debug)]
pub struct HoverReply {
    pub generation: u64,
    pub x: i32,
    pub z: i32,
    pub result: SourceResult<SurfaceSample>,
}

/// The hover pipeline's knowledge about the column under the cursor.
///
/// Carries the RAW sample; id → name resolution happens at display time
/// ([`resolve_sample`]) because the catalogs live with the source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HoverKnowledge {
    /// No cursor / no source: nothing to say.
    Nothing,
    /// A sample for the hovered column is on its way (or debounce pending).
    Pending,
    /// The hovered column lies outside the generatable world.
    OutOfWorld,
    /// The sampled surface of the hovered column.
    Sample(SurfaceSample),
}

/// The hover sampling state machine.
///
/// See the module docs for the debounce / in-flight / generation policy.
pub struct InspectorState {
    /// The column the cursor is over, as last reported.
    hovered: Option<(i32, i32)>,
    /// What the readout may show for `hovered` right now.
    knowledge: HoverKnowledge,
    /// The last query failure for the hovered column (transient, never
    /// cached); a normal answer clears it.
    error: Option<String>,
    /// The single outstanding request, as `(generation, column)`.
    in_flight: Option<(u64, (i32, i32))>,
    /// When the cursor last changed — the debounce anchor.
    last_move: Instant,
    /// The request-generation counter; only ever grows, never resets.
    next_generation: u64,
    /// Recent samples by column, most recently used at the back.
    lru: SampleLru,
}

impl InspectorState {
    /// A fresh inspector: no cursor, nothing known, no request.
    #[must_use]
    pub fn new() -> Self {
        InspectorState {
            hovered: None,
            knowledge: HoverKnowledge::Nothing,
            error: None,
            in_flight: None,
            last_move: Instant::now(),
            next_generation: 0,
            lru: SampleLru::default(),
        }
    }

    /// The column under the cursor (or [`None`]).
    #[allow(dead_code)] // reserved: the feature/search panels (T23/T26) report it
    #[must_use]
    pub fn hovered(&self) -> Option<(i32, i32)> {
        self.hovered
    }

    /// The current knowledge for display.
    #[must_use]
    pub fn knowledge(&self) -> &HoverKnowledge {
        &self.knowledge
    }

    /// The last sampling failure's message, when the hovered column's query
    /// failed (transient, never cached).
    #[must_use]
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// The single in-flight request, as `(generation, column)`.
    #[must_use]
    pub fn in_flight(&self) -> Option<(u64, (i32, i32))> {
        self.in_flight
    }

    /// One frame's drive. `cursor` is the frame's hovered block column.
    ///
    /// Returns `Some((generation, x, z))` when the caller should spawn a
    /// background `surface_at` for the column NOW.
    ///
    /// A cursor move updates the knowledge immediately (LRU hit → sample,
    /// otherwise pending) and restarts the debounce; only a settled,
    /// still-pending column with no request in flight issues at all.
    pub fn poll_due(
        &mut self,
        now: Instant,
        cursor: Option<(i32, i32)>,
    ) -> Option<(u64, i32, i32)> {
        if self.hovered != cursor {
            self.hovered = cursor;
            self.last_move = now;
            // A transient failure belonged to the column that just left.
            self.error = None;
            self.knowledge = match cursor {
                None => HoverKnowledge::Nothing,
                Some(column) => match self.lru.get(column) {
                    Some(sample) => HoverKnowledge::Sample(sample),
                    None => HoverKnowledge::Pending,
                },
            };
        }

        let column = self.hovered?;
        if !matches!(self.knowledge, HoverKnowledge::Pending) {
            return None;
        }
        if self.in_flight.is_some() {
            return None;
        }
        if now.saturating_duration_since(self.last_move) < HOVER_DEBOUNCE {
            return None;
        }

        let generation = self.next_generation;
        self.next_generation += 1;
        self.in_flight = Some((generation, column));
        Some((generation, column.0, column.1))
    }

    /// Applies a worker reply; replies for any generation other than the
    /// in-flight one are stale and are ignored entirely.
    pub fn apply_reply(
        &mut self,
        generation: u64,
        x: i32,
        z: i32,
        result: SourceResult<SurfaceSample>,
    ) {
        match self.in_flight {
            Some((current, _)) if current == generation => self.in_flight = None,
            _ => return,
        }
        let hovered = self.hovered == Some((x, z));
        match result {
            Ok(sample) => {
                self.lru.insert((x, z), sample);
                if hovered {
                    self.knowledge = HoverKnowledge::Sample(sample);
                    self.error = None;
                }
            }
            Err(SourceError::OutOfBounds { .. }) => {
                // A normal answer ("there is no such column"), not a failure.
                if hovered {
                    self.knowledge = HoverKnowledge::OutOfWorld;
                    self.error = None;
                }
            }
            Err(other) => {
                if hovered {
                    self.knowledge = HoverKnowledge::Nothing;
                    self.error = Some(other.to_string());
                }
            }
        }
    }

    /// Forgets everything source-specific: cached samples, pending
    /// knowledge, in-flight, error.
    ///
    /// The generation counter deliberately keeps counting, so a straggler
    /// reply from the old source can never match a request issued after the
    /// reset.
    pub fn reset(&mut self) {
        self.hovered = None;
        self.knowledge = HoverKnowledge::Nothing;
        self.lru.clear();
        self.in_flight = None;
        self.error = None;
    }

    /// Drops the notion of an in-flight request (the app lost or cancelled
    /// the reply channel).
    pub fn cancel_in_flight(&mut self) {
        self.in_flight = None;
    }
}

impl Default for InspectorState {
    fn default() -> Self {
        InspectorState::new()
    }
}

/// Recent samples keyed by block column, least-recently-used first.
///
/// A tiny deque beats a real LRU crate here: capacity 16 and a hover rate
/// of a few columns per second make the linear scan irrelevant, and the
/// ordering is trivial to reason about.
#[derive(Debug, Default)]
struct SampleLru {
    entries: VecDeque<((i32, i32), SurfaceSample)>,
}

impl SampleLru {
    /// Borrows the sample for `column`, refreshing its recency.
    #[must_use]
    fn get(&mut self, column: (i32, i32)) -> Option<SurfaceSample> {
        let index = self.entries.iter().position(|(key, _)| *key == column)?;
        let entry = self
            .entries
            .remove(index)
            .expect("the position was just found in the same deque");
        let sample = entry.1;
        self.entries.push_back(entry);
        Some(sample)
    }

    /// Inserts (or refreshes) one column, evicting the oldest entry when the
    /// cache is full.
    fn insert(&mut self, column: (i32, i32), sample: SurfaceSample) {
        if let Some(index) = self.entries.iter().position(|(key, _)| *key == column) {
            self.entries.remove(index);
        } else if self.entries.len() == HOVER_LRU_CAPACITY {
            self.entries.pop_front();
        }
        self.entries.push_back((column, sample));
    }

    fn clear(&mut self) {
        self.entries.clear();
    }
}

/// A surface sample with every id resolved for display (names, and the ids
/// themselves — the inspector panel shows both).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedSample {
    pub biome: String,
    pub biome_id: BiomeId,
    pub height: i32,
    pub block: String,
    pub block_id: BlockId,
    pub fluid: Option<String>,
    pub fluid_id: Option<FluidId>,
    pub fluid_depth: u16,
    pub tint: i32,
}

/// Resolves a raw sample's ids through the loaded source's catalogs.
///
/// Missing catalog entries fall back to `biome#N` / `block#N` / `fluid#N`
/// rather than panicking: a modded or partially loaded world must still be
/// inspectable.
#[must_use]
pub fn resolve_sample(
    sample: &SurfaceSample,
    biomes: &BiomeCatalog,
    blocks: &BlockCatalog,
    fluids: &FluidCatalog,
) -> ResolvedSample {
    let biome = biomes
        .get(sample.biome)
        .map(|info| info.name.clone())
        .unwrap_or_else(|| format!("biome#{}", sample.biome.0));
    let block = blocks
        .get(sample.block)
        .map(str::to_string)
        .unwrap_or_else(|| format!("block#{}", sample.block.0));
    let fluid = sample.fluid.map(|id| {
        fluids
            .get(id)
            .map(str::to_string)
            .unwrap_or_else(|| format!("fluid#{}", id.0))
    });
    ResolvedSample {
        biome,
        biome_id: sample.biome,
        height: sample.height,
        block,
        block_id: sample.block,
        fluid,
        fluid_id: sample.fluid,
        fluid_depth: sample.fluid_depth,
        tint: sample.tint,
    }
}

/// What [`format_readout`] renders: fully pre-rendered, so formatting is
/// pure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Readout {
    Nothing,
    Pending,
    OutOfWorld,
    Sample(ResolvedSample),
}

/// The status bar's hover readout, in the plan's format:
/// `X:-1203 Z:455 | Biome: Forest | Height: 71 | Block: Rock_Grass | Fluid: -`
///
/// Unknown values use `?` (coordinates, biome, height, block), `-` for the
/// fluid, `...` while a sample is pending, and `out of world` when the
/// column lies outside the generatable world.
#[must_use]
pub fn format_readout(cursor: Option<(i32, i32)>, readout: &Readout) -> String {
    let coordinates = match cursor {
        Some((x, z)) => format!("X:{x} Z:{z}"),
        None => format!("X:{missing} Z:{missing}", missing = labels::STATUS_MISSING,),
    };
    let knowledge = match readout {
        Readout::Nothing => format!(
            "{biome}: {missing} | {height}: {missing} | {block}: {missing} | {fluid}: {none}",
            biome = labels::STATUS_BIOME,
            height = labels::STATUS_HEIGHT,
            block = labels::STATUS_BLOCK,
            fluid = labels::STATUS_FLUID,
            missing = labels::STATUS_MISSING,
            none = labels::STATUS_NONE,
        ),
        Readout::Pending => format!(
            "{biome}: {pending} | {height}: {pending} | {block}: {pending} | {fluid}: {none}",
            biome = labels::STATUS_BIOME,
            height = labels::STATUS_HEIGHT,
            block = labels::STATUS_BLOCK,
            fluid = labels::STATUS_FLUID,
            pending = labels::STATUS_PENDING,
            none = labels::STATUS_NONE,
        ),
        Readout::OutOfWorld => labels::STATUS_OUT_OF_WORLD.to_string(),
        Readout::Sample(sample) => format!(
            "{biome}: {} | {height}: {} | {block}: {} | {fluid}: {}",
            sample.biome,
            sample.height,
            sample.block,
            format_fluid(sample.fluid.as_deref(), sample.fluid_depth),
            biome = labels::STATUS_BIOME,
            height = labels::STATUS_HEIGHT,
            block = labels::STATUS_BLOCK,
            fluid = labels::STATUS_FLUID,
        ),
    };
    format!("{coordinates} | {knowledge}")
}

/// The fluid segment of a sample's line: `name (depth)` when there is a
/// depth, the bare name otherwise, `-` when the column is dry.
#[must_use]
fn format_fluid(fluid: Option<&str>, depth: u16) -> String {
    match fluid {
        Some(name) if depth > 0 => format!("{name} ({depth})"),
        Some(name) => name.to_string(),
        None => labels::STATUS_NONE.to_string(),
    }
}

/// The go-to dialog's editable state.
#[derive(Debug, Clone, Default)]
pub struct GoToDialog {
    /// The x field's text, exactly as typed.
    pub x_text: String,
    /// The z field's text, exactly as typed.
    pub z_text: String,
    /// The inline error of the last failed submit.
    pub error: Option<String>,
}

/// Parses the go-to fields.
///
/// - both set: each must parse as `i32` after trimming; a failure reports
///   which field and what was typed;
/// - one set: split on commas and whitespace, keep the tokens that parse as
///   `i32`, and take the FIRST and LAST of them (this accepts `"x z"`,
///   `"x,z"` and `/tp x y z` pastes);
/// - neither set, or fewer than two parseable tokens: an error asking for
///   two numbers.
///
/// Overflowing values fail through the same `i32` parse path as garbage.
#[must_use = "the caller must show the parse error inline instead of ignoring it"]
pub fn parse_go_to(x_text: &str, z_text: &str) -> Result<(i32, i32), String> {
    let x = x_text.trim();
    let z = z_text.trim();
    let parse_field = |field: &str, text: &str| {
        text.parse::<i32>()
            .map_err(|_| format!("{field} must be a whole number: {text}"))
    };

    match (x.is_empty(), z.is_empty()) {
        (false, false) => Ok((parse_field("x", x)?, parse_field("z", z)?)),
        (true, false) | (false, true) => {
            let pasted = if x.is_empty() { z } else { x };
            let numbers: Vec<i32> = pasted
                .split(|c: char| c == ',' || c.is_whitespace())
                .filter_map(|token| token.parse::<i32>().ok())
                .collect();
            if numbers.len() < 2 {
                return Err(GO_TO_NEEDS_TWO.to_string());
            }
            Ok((numbers[0], numbers[numbers.len() - 1]))
        }
        (true, true) => Err(GO_TO_NEEDS_TWO.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    use map_core::source::WorldSource;
    use map_core::source::worldgen::{WorldgenSource, WorldgenSourceConfig};
    use map_core::test_support::FakeWorldSource;
    use map_core::types::Rect;

    use super::*;
    use crate::state::AppState;

    fn sample_at(x: i32, z: i32) -> SurfaceSample {
        SurfaceSample {
            x,
            z,
            biome: BiomeId(1),
            height: 70,
            block: BlockId(2),
            fluid: None,
            fluid_depth: 0,
            tint: 0,
        }
    }

    fn resolved_sample() -> ResolvedSample {
        ResolvedSample {
            biome: String::from("Forest"),
            biome_id: BiomeId(4),
            height: 71,
            block: String::from("Rock_Grass"),
            block_id: BlockId(9),
            fluid: Some(String::from("Water")),
            fluid_id: Some(FluidId(1)),
            fluid_depth: 3,
            tint: 0x11_22_33,
        }
    }

    /// Moves a fresh inspector to `column`, waits out the debounce and
    /// returns the issued request's generation.
    fn request_for(state: &mut InspectorState, start: Instant, column: (i32, i32)) -> u64 {
        assert!(
            state.poll_due(start, Some(column)).is_none(),
            "the move itself issues nothing"
        );
        let (generation, x, z) = state
            .poll_due(start + HOVER_DEBOUNCE, Some(column))
            .expect("the settled cursor issues one request");
        assert_eq!((x, z), column);
        generation
    }

    #[test]
    fn readout_formats_all_fields() {
        let sample = resolved_sample();
        assert_eq!(
            format_readout(Some((-1203, 455)), &Readout::Sample(sample.clone())),
            "X:-1203 Z:455 | Biome: Forest | Height: 71 | Block: Rock_Grass | Fluid: Water (3)",
        );

        // A fluid at depth 0 is not spelled with a meaningless "(0)".
        let shallow = ResolvedSample {
            fluid_depth: 0,
            ..sample.clone()
        };
        assert_eq!(
            format_readout(Some((-1203, 455)), &Readout::Sample(shallow)),
            "X:-1203 Z:455 | Biome: Forest | Height: 71 | Block: Rock_Grass | Fluid: Water",
        );

        // A dry column shows the plan's "-".
        let dry = ResolvedSample {
            fluid: None,
            fluid_id: None,
            fluid_depth: 0,
            ..sample
        };
        assert_eq!(
            format_readout(Some((-1203, 455)), &Readout::Sample(dry)),
            "X:-1203 Z:455 | Biome: Forest | Height: 71 | Block: Rock_Grass | Fluid: -",
        );
    }

    #[test]
    fn readout_formats_missing_pending_and_out_of_world() {
        assert_eq!(
            format_readout(None, &Readout::Nothing),
            format!(
                "X:{q} Z:{q} | {biome}: {q} | {height}: {q} | {block}: {q} | {fluid}: {none}",
                q = labels::STATUS_MISSING,
                none = labels::STATUS_NONE,
                biome = labels::STATUS_BIOME,
                height = labels::STATUS_HEIGHT,
                block = labels::STATUS_BLOCK,
                fluid = labels::STATUS_FLUID,
            ),
            "the all-unknown line the app pins",
        );
        assert_eq!(
            format_readout(Some((-1203, 455)), &Readout::Pending),
            format!(
                "X:-1203 Z:455 | {biome}: {p} | {height}: {p} | {block}: {p} | {fluid}: {none}",
                p = labels::STATUS_PENDING,
                none = labels::STATUS_NONE,
                biome = labels::STATUS_BIOME,
                height = labels::STATUS_HEIGHT,
                block = labels::STATUS_BLOCK,
                fluid = labels::STATUS_FLUID,
            ),
        );
        assert_eq!(
            format_readout(Some((-1203, 455)), &Readout::OutOfWorld),
            format!("X:-1203 Z:455 | {}", labels::STATUS_OUT_OF_WORLD),
        );
    }

    #[test]
    fn resolve_sample_names_through_catalogs_and_falls_back() {
        let source = FakeWorldSource::new("0");
        let raw = source
            .surface_at(0, 0)
            .expect("the fake surface never fails");
        assert_eq!(raw.height, 64, "the fake's lowest gradient step");
        let resolved = resolve_sample(
            &raw,
            source.biome_catalog(),
            source.block_catalog(),
            source.fluid_catalog(),
        );
        assert_eq!(resolved.biome, "Fake_0");
        assert_eq!(resolved.biome_id, BiomeId(0));
        assert_eq!(resolved.height, 64);
        assert_eq!(resolved.block, "Fake_Block_1");
        assert_eq!(resolved.block_id, BlockId(1));
        assert_eq!(resolved.fluid.as_deref(), Some("Fake_Water"));
        assert_eq!(resolved.fluid_id, Some(FluidId(1)));
        assert_eq!(resolved.fluid_depth, 2, "the fake lake is 66 - 64 deep");

        // Ids the catalogs do not know fall back instead of panicking.
        let unknown = SurfaceSample {
            x: 5,
            z: 6,
            biome: BiomeId(999),
            height: 10,
            block: BlockId(-7),
            fluid: Some(FluidId(1234)),
            fluid_depth: 1,
            tint: 0,
        };
        let resolved = resolve_sample(
            &unknown,
            source.biome_catalog(),
            source.block_catalog(),
            source.fluid_catalog(),
        );
        assert_eq!(resolved.biome, "biome#999");
        assert_eq!(resolved.block, "block#-7");
        assert_eq!(resolved.fluid.as_deref(), Some("fluid#1234"));
    }

    #[test]
    fn hover_moves_are_debounced_into_one_request_at_a_time() {
        let mut state = InspectorState::new();
        let t0 = Instant::now();
        let column = (1, 2);

        assert!(
            state.poll_due(t0, Some(column)).is_none(),
            "a move starts the debounce, it does not issue"
        );
        assert_eq!(state.knowledge(), &HoverKnowledge::Pending);
        assert_eq!(state.hovered(), Some(column));

        assert!(
            state
                .poll_due(t0 + Duration::from_millis(40), Some(column))
                .is_none(),
            "40 ms is short of the 50 ms settle window"
        );
        assert!(state.in_flight().is_none());

        let (generation, x, z) = state
            .poll_due(t0 + HOVER_DEBOUNCE, Some(column))
            .expect("the settled cursor issues exactly one request");
        assert_eq!((x, z), column);
        assert_eq!(state.in_flight(), Some((generation, column)));

        // While it is in flight, more frames at the same cursor issue nothing.
        for extra in [0, 1, 60, 5_000] {
            assert!(
                state
                    .poll_due(
                        t0 + HOVER_DEBOUNCE + Duration::from_millis(extra),
                        Some(column),
                    )
                    .is_none(),
                "one request at a time"
            );
        }
        assert_eq!(state.in_flight(), Some((generation, column)));
    }

    #[test]
    fn stale_hover_replies_do_not_overwrite_a_newer_coordinate() {
        let mut state = InspectorState::new();
        let t0 = Instant::now();
        let a = (1, 1);
        let b = (2, 2);
        let generation_a = request_for(&mut state, t0, a);

        // The cursor moves to B while A's request is still out.
        let moved = t0 + HOVER_DEBOUNCE + Duration::from_millis(1);
        assert!(state.poll_due(moved, Some(b)).is_none());
        assert_eq!(state.knowledge(), &HoverKnowledge::Pending);

        // A's reply lands: it is inserted into the LRU and frees the slot,
        // but B stays pending (the sample is for A's column).
        state.apply_reply(generation_a, a.0, a.1, Ok(sample_at(a.0, a.1)));
        assert_eq!(state.in_flight(), None);
        assert_eq!(state.knowledge(), &HoverKnowledge::Pending);

        // B's debounce is measured from the move; now it may issue.
        let (generation_b, bx, bz) = state
            .poll_due(moved + HOVER_DEBOUNCE, Some(b))
            .expect("B issues once its debounce elapses and A freed the slot");
        assert!(generation_b > generation_a, "generations only grow");
        assert_eq!((bx, bz), b);

        // A late copy of A's old reply must be ignored entirely — it must
        // not even clear B's in-flight slot.
        state.apply_reply(generation_a, a.0, a.1, Ok(sample_at(a.0, a.1)));
        assert_eq!(state.in_flight(), Some((generation_b, b)));
        assert_eq!(state.knowledge(), &HoverKnowledge::Pending);

        state.apply_reply(generation_b, b.0, b.1, Ok(sample_at(b.0, b.1)));
        assert_eq!(
            state.knowledge(),
            &HoverKnowledge::Sample(sample_at(b.0, b.1))
        );
        assert_eq!(state.in_flight(), None);
    }

    #[test]
    fn recent_samples_are_served_from_the_lru_without_a_new_request() {
        let mut state = InspectorState::new();
        let t0 = Instant::now();
        let a = (1, 1);
        let b = (2, 2);

        let generation = request_for(&mut state, t0, a);
        state.apply_reply(generation, a.0, a.1, Ok(sample_at(a.0, a.1)));

        let at_b = t0 + HOVER_DEBOUNCE + Duration::from_millis(1);
        let generation = request_for(&mut state, at_b, b);
        state.apply_reply(generation, b.0, b.1, Ok(sample_at(b.0, b.1)));

        // Hovering back over A answers from the cache in the same frame.
        let back = at_b + HOVER_DEBOUNCE + Duration::from_millis(1);
        assert!(state.poll_due(back, Some(a)).is_none());
        assert_eq!(
            state.knowledge(),
            &HoverKnowledge::Sample(sample_at(a.0, a.1)),
            "an LRU hit is knowledge immediately, not pending"
        );
        assert!(
            state
                .poll_due(back + Duration::from_secs(1), Some(a))
                .is_none(),
            "a cached column never re-queries, however long it rests"
        );
        assert_eq!(state.in_flight(), None);
    }

    #[test]
    fn out_of_bounds_replies_report_out_of_world() {
        let mut state = InspectorState::new();
        let t0 = Instant::now();
        let column = (9, 9);
        let generation = request_for(&mut state, t0, column);

        state.apply_reply(
            generation,
            column.0,
            column.1,
            Err(SourceError::OutOfBounds {
                x: column.0,
                z: column.1,
            }),
        );
        assert_eq!(state.knowledge(), &HoverKnowledge::OutOfWorld);
        assert_eq!(state.error(), None, "out of world is a normal answer");
        assert_eq!(state.in_flight(), None);
    }

    #[test]
    fn transient_failures_are_reported_and_cleared_by_the_next_move() {
        let mut state = InspectorState::new();
        let t0 = Instant::now();
        let column = (4, 4);
        let generation = request_for(&mut state, t0, column);

        state.apply_reply(
            generation,
            column.0,
            column.1,
            Err(SourceError::Generation(String::from("boom"))),
        );
        assert_eq!(state.knowledge(), &HoverKnowledge::Nothing);
        assert_eq!(
            state.error(),
            Some("generation failed: boom"),
            "the display text comes from the error's Display impl"
        );

        // Moving away clears the transient failure with the column.
        assert!(
            state
                .poll_due(t0 + HOVER_DEBOUNCE + Duration::from_millis(1), Some((5, 5)))
                .is_none()
        );
        assert_eq!(state.error(), None);
    }

    #[test]
    fn reset_forgets_samples_and_makes_stragglers_stale() {
        let mut state = InspectorState::new();
        let t0 = Instant::now();
        let column = (3, 3);
        let generation = request_for(&mut state, t0, column);
        state.apply_reply(
            generation,
            column.0,
            column.1,
            Ok(sample_at(column.0, column.1)),
        );
        assert_eq!(
            state.knowledge(),
            &HoverKnowledge::Sample(sample_at(column.0, column.1))
        );

        state.reset();
        assert_eq!(state.hovered(), None);
        assert_eq!(state.knowledge(), &HoverKnowledge::Nothing);
        assert_eq!(state.in_flight(), None);
        assert_eq!(state.error(), None);

        // A reply from before the reset finds no in-flight slot: stale.
        state.apply_reply(
            generation,
            column.0,
            column.1,
            Ok(sample_at(column.0, column.1)),
        );
        assert_eq!(state.knowledge(), &HoverKnowledge::Nothing);
        assert_eq!(state.in_flight(), None);

        // The LRU was emptied too: the old column must query again.
        let after = t0 + Duration::from_secs(1);
        assert!(state.poll_due(after, Some(column)).is_none());
        assert_eq!(
            state.knowledge(),
            &HoverKnowledge::Pending,
            "a reset cache cannot serve the old column"
        );
    }

    #[test]
    fn go_to_parsing_accepts_both_fields_comma_and_tp_pastes() {
        for (x_text, z_text) in [
            ("-1203", "455"),
            (" -1203 ", " 455 "),
            ("-1203 455", ""),
            ("-1203,455", ""),
            ("/tp -1203 64 455", ""),
        ] {
            assert_eq!(
                parse_go_to(x_text, z_text),
                Ok((-1203, 455)),
                "inputs {x_text:?} / {z_text:?}"
            );
        }
    }

    #[test]
    fn go_to_parsing_rejects_garbage_and_overflow() {
        let error = parse_go_to("abc", "5").expect_err("the x field is garbage");
        assert!(error.contains("x must be a whole number"), "{error}");
        assert!(error.contains("abc"), "the error quotes what was typed");

        let error = parse_go_to("5", "abc").expect_err("the z field is garbage");
        assert!(error.contains("z must be a whole number"), "{error}");

        // Overflowing values fail through the i32 parse, not a wrap.
        let error = parse_go_to("9999999999", "5").expect_err("i32 overflow");
        assert!(error.contains("x must be a whole number"), "{error}");
        assert!(error.contains("9999999999"), "{error}");

        // Neither field, and a single field with fewer than two numbers.
        assert_eq!(
            parse_go_to("", ""),
            Err(GO_TO_NEEDS_TWO.to_string()),
            "nothing entered"
        );
        assert_eq!(
            parse_go_to("12", ""),
            Err(GO_TO_NEEDS_TWO.to_string()),
            "one number is not a column"
        );
    }

    #[test]
    fn go_to_submit_centers_the_view_or_reports_the_error() {
        let mut state = AppState::new("/nonexistent/world");

        // Valid coordinates center the view and close the dialog.
        state.toggle_go_to();
        {
            let dialog = state.go_to_mut().expect("the dialog just opened");
            dialog.x_text = String::from("-1203");
            dialog.z_text = String::from("455");
        }
        assert!(state.submit_go_to(None));
        assert_eq!(state.view().center().x, -1203.0);
        assert_eq!(state.view().center().z, 455.0);
        assert!(state.go_to().is_none(), "a successful submit closes it");

        // Invalid text keeps it open with the inline error.
        state.toggle_go_to();
        {
            let dialog = state.go_to_mut().expect("the dialog reopened");
            dialog.x_text = String::from("abc");
            dialog.z_text = String::from("5");
        }
        assert!(!state.submit_go_to(None));
        let error = state
            .go_to()
            .expect("a failed submit keeps the dialog open")
            .error
            .as_deref()
            .expect("the inline error is set");
        assert!(error.contains("x must be a whole number"), "{error}");

        // A comma paste inside the world is accepted...
        {
            let dialog = state.go_to_mut().expect("the dialog is still open");
            dialog.x_text = String::from("0,0");
            dialog.z_text.clear();
            dialog.error = None;
        }
        let world = Rect::new(-100, -100, 99, 99);
        assert!(state.submit_go_to(Some(world)));
        assert_eq!(state.view().center().x, 0.0);
        assert_eq!(state.view().center().z, 0.0);

        // ...but a column beyond the world range is rejected with the range.
        state.toggle_go_to();
        {
            let dialog = state.go_to_mut().expect("the dialog reopened");
            dialog.x_text = String::from("500 500");
        }
        assert!(!state.submit_go_to(Some(world)));
        let error = state
            .go_to()
            .expect("the rejected submit keeps the dialog open")
            .error
            .as_deref()
            .expect("the range error is inline");
        assert!(
            error.contains("outside the world") && error.contains("x -100..99"),
            "the error names the generatable range: {error}"
        );
    }

    // -- The fixture comparison, gated on the real generator ----------------

    /// The worldgen checkout this repository's sibling ships for tests.
    fn world_dir() -> Option<PathBuf> {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../hytale-server-rust/testdata/Default");
        // The committed world's prefab references need the user-supplied
        // asset pack; without it the generator cannot load (CI has none).
        let assets = dir.join("../../Assets/extracted");
        (dir.is_dir() && assets.is_dir()).then_some(dir)
    }

    #[test]
    fn fixture_readout_matches_the_fidelity_suite() {
        // The fixture's seed-0 (0, 0) case pins biome, height and surface
        // block; the fluid name is checked but not its depth (the fixture
        // does not pin that).
        let Some(world) = world_dir() else {
            eprintln!("skipping: the worldgen checkout is not present");
            return;
        };
        let config = WorldgenSourceConfig::new(&world, "0");
        let source = WorldgenSource::new(&config)
            .expect("the fixture world loads with the worldgen checkout and asset pack");

        let sample = source.surface_at(0, 0).expect("the fixture column samples");
        let resolved = resolve_sample(
            &sample,
            source.biome_catalog(),
            source.block_catalog(),
            source.fluid_catalog(),
        );
        assert_eq!(resolved.biome, "River_Forest_Birch");
        assert_eq!(resolved.height, 107);
        assert_eq!(resolved.block, "Soil_Gravel_Sand_White");
        assert_eq!(resolved.fluid.as_deref(), Some("Water_Source"));

        let line = format_readout(Some((0, 0)), &Readout::Sample(resolved));
        assert!(
            line.contains(
                "Biome: River_Forest_Birch | Height: 107 | Block: Soil_Gravel_Sand_White"
            ),
            "the readout reproduces the fixture: {line}"
        );
    }
}
