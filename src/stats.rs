//! Per-sheet packing statistics, owned by the library so the CLI and GUI don't
//! each recompute them from geometry.
//!
//! A sheet's *yield* is the true cut area of its parts (outer contour minus
//! holes) over the sheet area. Cut length and pierce count come from the parts'
//! ring geometry; both are invariant under the rigid place transform, so they
//! can be read from each piece's local rings. An optional [`CutProfile`] turns
//! those into an estimated machine run time.

use dxf::Drawing;

use crate::emit::Placed;
use crate::extract::Piece;

/// A laser/router feed model used to estimate run time from cut geometry.
#[derive(Clone, Copy, Debug)]
pub struct CutProfile {
  /// Cutting feed rate, mm per second.
  pub feed_mm_s: f64,
  /// Time added per pierce (per closed ring), in seconds.
  pub pierce_s: f64,
}

impl Default for CutProfile {
  fn default() -> Self {
    // Conservative plywood-ish defaults; the UI's material picker can override.
    CutProfile { feed_mm_s: 20.0, pierce_s: 0.5 }
  }
}

/// Job material metadata (S-8). Carried alongside the job so the stats/kerf
/// features have a home; `cut` drives run-time estimation in [`sheet_stats`].
#[derive(Clone, Debug)]
pub struct Material {
  pub name: String,
  pub thickness_mm: f64,
  pub cut: CutProfile,
}

impl Default for Material {
  fn default() -> Self {
    Material { name: "Plywood".into(), thickness_mm: 3.0, cut: CutProfile::default() }
  }
}

/// Packing quality for one sheet.
#[derive(Clone, Debug)]
pub struct SheetStats {
  pub sheet: usize,
  pub piece_count: usize,
  /// True cut area of the sheet's parts (outer minus holes), DXF units².
  pub used_area: f64,
  pub sheet_area: f64,
  /// `used_area / sheet_area`, clamped to `>= 0`.
  pub utilization: f32,
  /// Total cut path length on the sheet (sum of every ring perimeter), mm.
  pub cut_len_mm: f64,
  /// Number of pierces (one per closed ring).
  pub pierces: usize,
  /// Estimated machine run time from the supplied [`CutProfile`], seconds.
  pub est_run_secs: f64,
}

fn ring_perimeter(ring: &[[f64; 2]]) -> f64 {
  if ring.len() < 2 {
    return 0.0;
  }
  let mut per = 0.0;
  for i in 0..ring.len() {
    let a = ring[i];
    let b = ring[(i + 1) % ring.len()];
    per += (b[0] - a[0]).hypot(b[1] - a[1]);
  }
  per
}

/// Compute [`SheetStats`] for one sheet from the placements on it.
pub fn sheet_stats(
  pieces: &[Piece],
  drawing: &Drawing,
  placed: &[Placed],
  sheet: usize,
  sheet_w: f64,
  sheet_h: f64,
  profile: CutProfile,
) -> SheetStats {
  let mut piece_count = 0;
  let mut used_area = 0.0;
  let mut cut_len_mm = 0.0;
  let mut pierces = 0;
  for pl in placed.iter().filter(|p| p.sheet == sheet) {
    piece_count += 1;
    used_area += pieces[pl.piece_index].area;
    for ring in &pieces[pl.piece_index].rings(drawing) {
      pierces += 1;
      cut_len_mm += ring_perimeter(ring);
    }
  }
  let sheet_area = (sheet_w * sheet_h).max(1.0);
  let used_area = used_area.max(0.0);
  SheetStats {
    sheet,
    piece_count,
    used_area,
    sheet_area,
    utilization: (used_area / sheet_area) as f32,
    cut_len_mm,
    pierces,
    est_run_secs: cut_len_mm / profile.feed_mm_s.max(1e-6) + pierces as f64 * profile.pierce_s,
  }
}

/// Compute [`SheetStats`] for every sheet `0..sheets`.
pub fn all_sheet_stats(
  pieces: &[Piece],
  drawing: &Drawing,
  placed: &[Placed],
  sheets: usize,
  sheet_w: f64,
  sheet_h: f64,
  profile: CutProfile,
) -> Vec<SheetStats> {
  (0..sheets)
    .map(|s| sheet_stats(pieces, drawing, placed, s, sheet_w, sheet_h, profile))
    .collect()
}
