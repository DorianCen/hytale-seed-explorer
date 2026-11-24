//! Shared ring-walk math for the search engines (extracted from T24's
//! feature search; reused by T25's biome search).
//!
//! A "ring walk" expands square rings of grid cells around the cell
//! containing a search center: ring `r` is every cell at Chebyshev index
//! exactly `r`. The grid is whatever the caller's step defines — 512-block
//! query regions for the feature search, 16-block sampling cells for the
//! biome search — the math is identical, so it lives here once.
//!
//! Two facts the engines rely on:
//!
//! - [`ring_min_distance`] is the exact minimum distance from the center to
//!   any *point* of ring `r`'s cells (O(1), verified against brute force in
//!   the unit tests). It is a lower bound for anything not yet queried in a
//!   later ring — the safe-emission threshold both engines stream by.
//! - [`ring_regions`] walks a ring's cells in a deterministic perimeter
//!   order, and the cells of different rings never overlap, so a cell is
//!   queried at most once per search.

use crate::types::Rect;

/// The ring walk's fixed frame: the clamped center and its cell.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RingGeometry {
    /// The grid step (blocks per cell edge).
    pub(crate) size: i64,
    /// The center's cell index on each axis.
    pub(crate) rx0: i64,
    pub(crate) rz0: i64,
    /// The center, clamped into the i32 block world.
    pub(crate) cx: f64,
    pub(crate) cz: f64,
}

impl RingGeometry {
    /// Clamps the center into the i32 block world (world data lives at i32
    /// positions; a center beyond the world saturates to its edge) and finds
    /// the cell containing it: `floor(center / size)`.
    pub(crate) fn new(center: (f64, f64), size: u32) -> Self {
        let cx = center.0.clamp(f64::from(i32::MIN), f64::from(i32::MAX));
        let cz = center.1.clamp(f64::from(i32::MIN), f64::from(i32::MAX));
        let size = i64::from(size);
        let rx0 = (cx / size as f64).floor() as i64;
        let rz0 = (cz / size as f64).floor() as i64;
        Self {
            size,
            rx0,
            rz0,
            cx,
            cz,
        }
    }
}

/// The on-world part of cell (`rx`, `rz`) — an inclusive block rect of edge
/// `size` — or [`None`] when the cell lies entirely outside the i32 world.
///
/// A cell straddling the world edge clamps to it: the part that exists is
/// still worth one query. The off-world check happens on the raw i64 bounds
/// *before* clamping; clamping first would collapse an off-world cell into
/// a degenerate one-block rect.
pub(crate) fn cell_rect(rx: i64, rz: i64, size: i64) -> Option<Rect> {
    let min_x = rx.checked_mul(size)?;
    let max_x = min_x.checked_add(size - 1)?;
    let min_z = rz.checked_mul(size)?;
    let max_z = min_z.checked_add(size - 1)?;
    let world_min = i64::from(i32::MIN);
    let world_max = i64::from(i32::MAX);
    if max_x < world_min || min_x > world_max || max_z < world_min || min_z > world_max {
        return None;
    }
    Some(Rect::new(
        clamp_to_i32(min_x),
        clamp_to_i32(min_z),
        clamp_to_i32(max_x),
        clamp_to_i32(max_z),
    ))
}

/// `value` clamped into the i32 range; exact for values already in range.
pub(crate) fn clamp_to_i32(value: i64) -> i32 {
    value.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
}

/// The closest any point of ring `ring`'s cells can be to the center.
///
/// O(1). Ring 0 is the center's own cell — a fractional center can sit
/// between blocks, *outside* its cell's inclusive bounds, so its minimum is
/// the plain point-to-rect distance into that cell. Ring `r ≥ 1` is covered
/// by four strips (the cell columns `±r` steps east/west and the rows `±r`
/// steps south/north); a strip's minimum is the hypotenuse of its axis
/// clearance and the center's gap to the nearest row-rect of the orthogonal
/// axis — a fractional center parked between blocks is that gap away from
/// both adjacent rows, and the corner cells make the diagonal the true
/// minimum, not the bare axis clearance.
pub(crate) fn ring_min_distance(geometry: &RingGeometry, ring: u32) -> f64 {
    let size = geometry.size;
    if ring == 0 {
        let dx = axis_distance_into_rect(geometry.cx, geometry.rx0, size);
        let dz = axis_distance_into_rect(geometry.cz, geometry.rz0, size);
        return dx.hypot(dz);
    }
    let r = i64::from(ring);
    let size = size as f64;
    let x_gap = axis_gap_to_adjacent_rows(geometry.cx, geometry.rx0, geometry.size);
    let z_gap = axis_gap_to_adjacent_rows(geometry.cz, geometry.rz0, geometry.size);
    let east = (geometry.rx0 + r) as f64 * size - geometry.cx;
    let west = geometry.cx - ((geometry.rx0 - r) as f64 * size + size - 1.0);
    let south = (geometry.rz0 + r) as f64 * size - geometry.cz;
    let north = geometry.cz - ((geometry.rz0 - r) as f64 * size + size - 1.0);
    east.hypot(z_gap)
        .min(west.hypot(z_gap))
        .min(south.hypot(x_gap))
        .min(north.hypot(x_gap))
        .max(0.0)
}

/// Distance from `v` to the rect of cell `index` along one axis: zero
/// inside, else the distance to the nearer edge (ring 0's bound).
fn axis_distance_into_rect(v: f64, index: i64, size: i64) -> f64 {
    let row_min = index as f64 * size as f64;
    let row_max = row_min + (size - 1) as f64;
    (row_min - v).max(0.0).max(v - row_max)
}

/// Distance from `v` to the nearest point of the row-rects of `index` and
/// `index + 1` along one axis. Zero inside a rect; a fractional `v` parked
/// in the crack between the top block of `index` and the bottom block of
/// `index + 1` is the smaller clearance to either side. Both rows are part
/// of every ring-`r ≥ 1` strip, so this is the strip's orthogonal gap.
fn axis_gap_to_adjacent_rows(v: f64, index: i64, size: i64) -> f64 {
    let own_gap = axis_distance_into_rect(v, index, size);
    if own_gap == 0.0 {
        return 0.0;
    }
    let next_row_gap = axis_distance_into_rect(v, index + 1, size);
    own_gap.min(next_row_gap)
}

/// Whether every cell of ring `ring` lies outside the i32 world.
///
/// O(1): each ring edge's midpoint cell shares the center cell's range on
/// the other axis (always on-world), so a midpoint being off-world means
/// the whole edge is beyond that world bound — and the four edges are the
/// ring. Ring 0 contains the center and is never off-world.
pub(crate) fn ring_entirely_off_world(geometry: &RingGeometry, ring: u32) -> bool {
    if ring == 0 {
        return false;
    }
    let r = i64::from(ring);
    cell_rect(geometry.rx0 - r, geometry.rz0, geometry.size).is_none()
        && cell_rect(geometry.rx0 + r, geometry.rz0, geometry.size).is_none()
        && cell_rect(geometry.rx0, geometry.rz0 - r, geometry.size).is_none()
        && cell_rect(geometry.rx0, geometry.rz0 + r, geometry.size).is_none()
}

/// The cells of ring `ring`: Chebyshev index exactly `ring` from the
/// center's cell, in a deterministic perimeter walk (top edge left to
/// right, bottom edge, then the two side interiors).
pub(crate) fn ring_regions(rx0: i64, rz0: i64, ring: u32) -> impl Iterator<Item = (i64, i64)> {
    let r = i64::from(ring);
    let top = (-r..=r).map(move |dx| (rx0 + dx, rz0 - r));
    // Ring 0 is the center cell alone; the other three edges would
    // duplicate it.
    let bottom = if r == 0 {
        None
    } else {
        Some((-r..=r).map(move |dx| (rx0 + dx, rz0 + r)))
    };
    let left = (-r + 1..=r - 1).map(move |dz| (rx0 - r, rz0 + dz));
    let right = if r == 0 {
        None
    } else {
        Some((-r + 1..=r - 1).map(move |dz| (rx0 + r, rz0 + dz)))
    };
    top.chain(bottom.into_iter().flatten())
        .chain(left)
        .chain(right.into_iter().flatten())
}

/// 2-D Euclidean distance in the XZ plane.
pub(crate) fn distance_2d(cx: f64, cz: f64, x: i32, z: i32) -> f64 {
    let dx = cx - f64::from(x);
    let dz = cz - f64::from(z);
    dx.hypot(dz)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// Point-to-rect XZ distance, the brute force `ring_min_distance` is
    /// compared against.
    fn point_rect_distance(cx: f64, cz: f64, rect: &Rect) -> f64 {
        let dx = (f64::from(rect.min_x) - cx)
            .max(0.0)
            .max(cx - f64::from(rect.max_x));
        let dz = (f64::from(rect.min_z) - cz)
            .max(0.0)
            .max(cz - f64::from(rect.max_z));
        dx.hypot(dz)
    }

    #[test]
    fn rings_iteration_counts_and_uniqueness() {
        let expected_counts = [(0u32, 1usize), (1, 8), (2, 16), (3, 24), (7, 56)];
        for (ring, count) in expected_counts {
            assert_eq!(
                ring_regions(10, -4, ring).count(),
                count,
                "ring {ring} must hold 8r cells (1 for the center)",
            );
        }
        for ring in [1u32, 2, 5] {
            let cells: Vec<(i64, i64)> = ring_regions(0, 0, ring).collect();
            let unique: HashSet<(i64, i64)> = cells.iter().copied().collect();
            assert_eq!(
                cells.len(),
                unique.len(),
                "ring {ring} must not repeat a cell"
            );
            for &(rx, rz) in &cells {
                assert_eq!(
                    rx.unsigned_abs().max(rz.unsigned_abs()),
                    u64::from(ring),
                    "every cell of ring {ring} is at Chebyshev index {ring}",
                );
            }
        }
        // Ring 0 is exactly the center cell.
        assert_eq!(ring_regions(3, 7, 0).collect::<Vec<_>>(), vec![(3, 7)]);
    }

    #[test]
    fn rings_min_distance_matches_brute_force() {
        // Centers inside a cell, exactly on a boundary, near an edge, and
        // negative — the four-strip shortcut must equal the true minimum
        // point-to-cell distance for every early ring.
        let centers: [(f64, f64); 4] = [(100.5, 200.25), (0.0, 0.0), (511.9, -0.1), (-0.5, -512.0)];
        for center in centers {
            let geometry = RingGeometry::new(center, 512);
            for ring in 0u32..8 {
                let brute = ring_regions(geometry.rx0, geometry.rz0, ring)
                    .filter_map(|(rx, rz)| cell_rect(rx, rz, geometry.size))
                    .map(|rect| point_rect_distance(geometry.cx, geometry.cz, &rect))
                    .fold(f64::INFINITY, f64::min);
                let fast = ring_min_distance(&geometry, ring);
                assert!(
                    (fast - brute).abs() < 1e-9,
                    "ring_min({ring}) from {center:?}: {fast} must match the brute-force {brute}",
                );
            }
        }
    }

    #[test]
    fn rings_cell_rect_alignment_and_edges() {
        // Inclusive max: a 512 cell is 512 blocks wide.
        assert_eq!(
            cell_rect(0, 0, 512),
            Some(Rect::new(0, 0, 511, 511)),
            "cell (0, 0) covers 0..=511",
        );
        assert_eq!(
            cell_rect(-1, 0, 512),
            Some(Rect::new(-512, 0, -1, 511)),
            "negative cells floor toward minus infinity",
        );
        // A center exactly on a cell boundary belongs to the next cell.
        assert_eq!(RingGeometry::new((512.0, 0.0), 512).rx0, 1);
        assert_eq!(RingGeometry::new((511.9, 0.0), 512).rx0, 0);
        // Centers beyond the i32 world saturate to its edge.
        let saturated = RingGeometry::new((1e12, -1e12), 512);
        assert_eq!(saturated.cx, f64::from(i32::MAX));
        assert_eq!(saturated.cz, f64::from(i32::MIN));
        // Cells fully outside the world are None; straddling ones clamp.
        assert!(
            cell_rect(10_000_000, 0, 512).is_none(),
            "a cell fully east of the i32 world has nothing to query",
        );
        assert_eq!(
            cell_rect(4_194_303, 0, 512),
            Some(Rect::new(i32::MAX - 511, 0, i32::MAX, 511)),
            "the cell containing i32::MAX clamps to it",
        );
    }

    #[test]
    fn rings_off_world_detection_matches_per_cell_rects() {
        let geometry = RingGeometry::new((0.0, 0.0), 512);
        // Ring 2 is fully on-world; a ring past every world edge is not.
        assert!(!ring_entirely_off_world(&geometry, 2));
        let far_ring = u32::try_from(9_000_000).expect("fits u32");
        assert!(
            ring_entirely_off_world(&geometry, far_ring),
            "a ring beyond the i32 world on all four sides is off-world",
        );
        // The O(1) check must agree with the per-cell truth on a ring that
        // only partially leaves the world: every sampled cell rect of a
        // ring the shortcut calls off-world is really None. Sampled (every
        // 1,000th cell) because this ring holds ~33M cells.
        let mut sampled = 0usize;
        for (index, (rx, rz)) in ring_regions(geometry.rx0, geometry.rz0, far_ring).enumerate() {
            if index % 1_000 != 0 {
                continue;
            }
            sampled += 1;
            assert!(
                cell_rect(rx, rz, geometry.size).is_none(),
                "shortcut claimed off-world but ({rx}, {rz}) has a rect",
            );
        }
        assert!(sampled > 1_000, "the sample must cover the ring's edges");
    }
}
