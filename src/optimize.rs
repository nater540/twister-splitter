//! Shape nesting via the real `sparrow` optimizer (a state-of-the-art 2D
//! irregular strip-packing heuristic, MIT, built on the same `jagua-rs` we use —
//! so it shares our jagua types and needs no data marshalling).
//!
//! sparrow solves *strip* packing (one open dimension). We want fixed-size
//! sheets, so we **peel** sheets off the strip: strip-pack the remaining pieces
//! (strip height = sheet height), keep every piece that lands fully within the
//! first sheet-width onto the current sheet, then re-pack the rest onto the next
//! sheet. Re-packing each round realigns the layout to x=0, so no piece is ever
//! split across a sheet boundary.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use jagua_rs::entities::Instance;
use jagua_rs::io::export::int_to_ext_transformation;
use jagua_rs::io::ext_repr::{ExtItem as BaseExtItem, ExtSPolygon, ExtShape};
use jagua_rs::io::import::Importer;
use jagua_rs::probs::spp::io::ext_repr::{ExtItem as SpItem, ExtSPInstance};
use jagua_rs::probs::spp::io::import_instance;
use rand::SeedableRng;
use rand::rngs::Xoshiro256PlusPlus;
use sparrow::config::DEFAULT_SPARROW_CONFIG;
use sparrow::optimizer::optimize;
use sparrow::util::listener::DummySolListener;
use sparrow::util::terminator::{BasicTerminator, Terminator};

use crate::emit::Placed;
use crate::geom::{Affine, Bbox};
use crate::nest::{NestItem, NestResult, valid_ring_or_hull};

/// Progress/streaming events emitted during a nest run. Keeping this a callback
/// (rather than a channel) preserves the library's UI-free property — the caller
/// forwards these however it likes.
pub enum NestEvent<'a> {
  /// A sheet was finalized; these placements will not change.
  SheetCompleted { sheet: usize, placed: &'a [Placed] },
  /// Coarse progress for a determinate bar, in `0.0..=1.0` (best-effort).
  Progress { fraction: f32 },
}

/// A sparrow terminator that also stops when an external cancel flag is set, so
/// a caller (e.g. a GUI "Cancel" button) can end an in-flight optimize promptly.
/// It delegates timeout handling to the wrapped [`BasicTerminator`].
struct CancelTerminator<'a> {
  inner: BasicTerminator,
  cancel: Option<&'a AtomicBool>,
}

impl Terminator for CancelTerminator<'_> {
  fn kill(&self) -> bool {
    self.inner.kill() || self.cancel.is_some_and(|c| c.load(Ordering::Relaxed))
  }
  fn new_timeout(&mut self, timeout: Duration) {
    self.inner.new_timeout(timeout);
  }
  fn timeout_at(&self) -> Option<jagua_rs::Instant> {
    self.inner.timeout_at()
  }
}

/// Does `polygon` fit a `sheet_w` × `sheet_h` sheet at *some* rotation?
fn fits_sheet(polygon: &[[f64; 2]], sheet_w: f64, sheet_h: f64) -> bool {
  // Sample orientations over a half-turn (axis-aligned bbox has period π).
  for i in 0..90 {
    let theta = (i as f64) * std::f64::consts::PI / 90.0;
    let (s, c) = theta.sin_cos();
    let (mut minx, mut miny, mut maxx, mut maxy) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
    for p in polygon {
      let x = c * p[0] - s * p[1];
      let y = s * p[0] + c * p[1];
      minx = minx.min(x);
      miny = miny.min(y);
      maxx = maxx.max(x);
      maxy = maxy.max(y);
    }
    if (maxx - minx) <= sheet_w + 1e-6 && (maxy - miny) <= sheet_h + 1e-6 {
      return true;
    }
  }
  false
}

/// Build a jagua strip-packing instance from `polys` (validated / hull-repaired),
/// returning it plus a map from jagua item id → caller's piece index.
fn build_instance(
  polys: &[&NestItem],
  sheet_h: f64,
  kerf: f64,
) -> (jagua_rs::probs::spp::entities::SPInstance, Vec<usize>) {
  let cfg = DEFAULT_SPARROW_CONFIG;
  let mut fallbacks = 0;
  let items: Vec<SpItem> = polys
    .iter()
    .enumerate()
    .map(|(i, it)| SpItem {
      base: BaseExtItem {
        id: i as u64,
        allowed_orientations: None, // continuous rotation
        shape: ExtShape::SimplePolygon(ExtSPolygon(valid_ring_or_hull(&it.polygon, &mut fallbacks))),
        min_quality: None,
      },
      demand: 1,
    })
    .collect();
  let id_map: Vec<usize> = polys.iter().map(|it| it.piece_index).collect();

  let separation = if kerf > 0.0 { Some(kerf as f32) } else { None };
  let importer = Importer::new(
    cfg.cde_config,
    cfg.poly_simpl_tolerance,
    separation,
    cfg.narrow_concavity_cutoff_ratio,
  );
  let ext = ExtSPInstance {
    name: "twister".into(),
    items,
    strip_height: sheet_h as f32,
  };
  let instance = import_instance(&importer, &ext).expect("valid strip instance");
  (instance, id_map)
}

fn affine_from(rotation: f32, tx: f32, ty: f32) -> Affine {
  let (s, c) = (rotation as f64).sin_cos();
  Affine { m00: c, m01: -s, m10: s, m11: c, tx: tx as f64, ty: ty as f64 }
}

/// Nest `items` onto `sheet_w` × `sheet_h` sheets using sparrow, peeling one
/// sheet at a time (re-packing each round realigns to x=0 so no piece crosses a
/// sheet edge). `explore`/`compress` are the per-sheet time budgets; pieces too
/// large for a sheet at any rotation are returned as `oversized`.
///
/// `cancel`, when set true, stops the run as promptly as sparrow allows and
/// returns a partial result: everything placed so far, with the pieces not yet
/// reached in `NestResult::unplaced`. `on_event` streams a [`NestEvent`] as each
/// sheet is finalized (its placements are then immutable) and after each sheet
/// for coarse progress.
///
/// `margin` is a usable inset (mm) kept clear on every sheet edge: pieces are
/// nested into the `(sheet_w-2·margin) × (sheet_h-2·margin)` interior and then
/// shifted in by `margin`, so nothing sits in the margin band.
#[allow(clippy::too_many_arguments)]
pub fn nest_sparrow(
  items: &[NestItem],
  sheet_w: f64,
  sheet_h: f64,
  kerf: f64,
  margin: f64,
  seed: u64,
  explore: Duration,
  compress: Duration,
  cancel: Option<Arc<AtomicBool>>,
  mut on_event: impl FnMut(NestEvent),
) -> NestResult {
  // The nestable interior after reserving the edge margin.
  let margin = margin.max(0.0);
  let inner_w = (sheet_w - 2.0 * margin).max(0.0);
  let inner_h = (sheet_h - 2.0 * margin).max(0.0);

  // Split off pieces that cannot fit the interior at any rotation.
  let mut oversized = Vec::new();
  let mut remaining: Vec<&NestItem> = Vec::new();
  for it in items {
    if inner_w > 0.0 && inner_h > 0.0 && fits_sheet(&it.polygon, inner_w, inner_h) {
      remaining.push(it);
    } else {
      oversized.push(it.piece_index);
    }
  }
  if remaining.is_empty() {
    return NestResult { placed: Vec::new(), sheets: 0, oversized, unplaced: Vec::new() };
  }

  let mut cfg = DEFAULT_SPARROW_CONFIG;
  cfg.expl_cfg.time_limit = explore;
  cfg.cmpr_cfg.time_limit = compress;

  let cancel_flag = cancel.as_deref();
  let is_canceled = || cancel_flag.is_some_and(|c| c.load(Ordering::Relaxed));

  // Peel sheets off the strip: strip-pack the remaining pieces, keep those that
  // land fully within the first sheet width, re-pack the rest onto the next sheet.
  let total = remaining.len();
  let mut rem = remaining;
  let mut placed = Vec::new();
  let mut sheet = 0usize;
  while !rem.is_empty() {
    // Stop before starting a new sheet if cancellation was requested; pieces
    // still in `rem` become `unplaced` in the partial result below.
    if is_canceled() {
      break;
    }
    let mut round = strip_pack(&rem, inner_h, kerf, &cfg, seed.wrapping_add(sheet as u64), cancel_flag);
    round.sort_by(|a, b| a.max_x.partial_cmp(&b.max_x).unwrap_or(std::cmp::Ordering::Equal));

    // Shift a recentered placement in by the edge margin so nothing sits in it.
    let inset = |mut xf: Affine| {
      xf.tx += margin;
      xf.ty += margin;
      xf
    };

    let mut next: Vec<&NestItem> = Vec::new();
    let mut this_sheet: Vec<Placed> = Vec::new();
    for p in &round {
      if p.max_x <= inner_w + 1e-3 {
        this_sheet.push(Placed { piece_index: rem[p.jid].piece_index, sheet, transform: inset(p.xf), oversized: false, locked: false });
      } else {
        next.push(rem[p.jid]);
      }
    }
    if this_sheet.is_empty() {
      // Guarantee forward progress: a piece too wide for the remaining strip
      // width still takes this sheet, so the loop can't spin forever.
      this_sheet.push(Placed { piece_index: rem[round[0].jid].piece_index, sheet, transform: inset(round[0].xf), oversized: false, locked: false });
      next = round[1..].iter().map(|p| rem[p.jid]).collect();
    }

    on_event(NestEvent::SheetCompleted { sheet, placed: &this_sheet });
    placed.extend(this_sheet);
    rem = next;
    sheet += 1;
    on_event(NestEvent::Progress { fraction: (total - rem.len()) as f32 / total as f32 });
  }

  let unplaced: Vec<usize> = rem.iter().map(|it| it.piece_index).collect();
  NestResult { placed, sheets: sheet, oversized, unplaced }
}

/// One placed piece from a strip pack: index into the packed slice, its
/// transform in original coordinates, and its world bbox extents (recentered so
/// the whole pack's bottom-left corner is at the origin).
struct Pl {
  jid: usize,
  xf: Affine,
  min_x: f64,
  max_x: f64,
  min_y: f64,
}

/// Strip-pack `polys` once with sparrow and return recentered placements.
/// `cancel`, when set, ends the optimize early via [`CancelTerminator`]; sparrow
/// still returns a feasible (if less-optimized) layout, so callers get valid,
/// non-overlapping placements even on cancellation.
fn strip_pack(
  polys: &[&NestItem],
  sheet_h: f64,
  kerf: f64,
  cfg: &sparrow::config::SparrowConfig,
  seed: u64,
  cancel: Option<&AtomicBool>,
) -> Vec<Pl> {
  let (instance, _) = build_instance(polys, sheet_h, kerf);
  let rng = Xoshiro256PlusPlus::seed_from_u64(seed);
  let mut terminator = CancelTerminator { inner: BasicTerminator::new(), cancel };
  let solution = optimize(
    instance.clone(),
    rng,
    &mut DummySolListener,
    &mut terminator,
    &cfg.expl_cfg,
    &cfg.cmpr_cfg,
    None,
  );

  let mut pls: Vec<Pl> = solution
    .layout_snapshot
    .placed_items
    .values()
    .map(|pi| {
      let ext =
        int_to_ext_transformation(&pi.d_transf, &instance.item(pi.item_id).shape_orig.pre_transform);
      let (tx, ty) = ext.translation();
      let xf = affine_from(ext.rotation(), tx, ty);
      let (mut a, mut b, mut c) = (f64::MAX, f64::MIN, f64::MAX);
      for p in &polys[pi.item_id].polygon {
        let (x, y) = xf.apply(p[0], p[1]);
        a = a.min(x);
        b = b.max(x);
        c = c.min(y);
      }
      Pl { jid: pi.item_id, xf, min_x: a, max_x: b, min_y: c }
    })
    .collect();

  // Recenter so the pack's bottom-left corner sits at (0,0).
  let gmin_x = pls.iter().map(|p| p.min_x).fold(f64::MAX, f64::min);
  let gmin_y = pls.iter().map(|p| p.min_y).fold(f64::MAX, f64::min);
  for p in &mut pls {
    p.xf.tx -= gmin_x;
    p.xf.ty -= gmin_y;
    p.min_x -= gmin_x;
    p.max_x -= gmin_x;
  }
  pls
}

/// The full result of nesting onto fixed sheets: fitted placements plus oversized
/// pieces each on their own sheet. This is what a caller emits or previews.
#[derive(Clone)]
pub struct NestOutcome {
  /// Every placement, including oversized pieces on their own sheets.
  pub placed: Vec<Placed>,
  /// Piece indices that exceeded a sheet at any rotation (each on its own sheet).
  pub oversized: Vec<usize>,
  /// Piece indices left unplaced because the run was cancelled early.
  pub unplaced: Vec<usize>,
  /// Total number of sheets used (fitted sheets + one per oversized piece).
  pub sheets: usize,
  /// Whether the run ended because cancellation was requested.
  pub canceled: bool,
}

/// Full pipeline over sparrow: nest onto fixed `sheet_w` × `sheet_h` sheets, then
/// place each oversized piece on its own sheet (recentred to the sheet origin,
/// matching the rectangle packer's oversized path). This is the single entry
/// point the CLI and GUI share, so the oversized post-processing lives in one
/// place instead of being duplicated at each call site.
#[allow(clippy::too_many_arguments)]
pub fn nest_sheets(
  items: &[NestItem],
  piece_bboxes: &[Bbox],
  sheet_w: f64,
  sheet_h: f64,
  kerf: f64,
  margin: f64,
  seed: u64,
  explore: Duration,
  compress: Duration,
  cancel: Option<Arc<AtomicBool>>,
  on_event: impl FnMut(NestEvent),
) -> NestOutcome {
  let cancel_probe = cancel.clone();
  let result = nest_sparrow(items, sheet_w, sheet_h, kerf, margin, seed, explore, compress, cancel, on_event);

  let mut placed = result.placed;
  // Oversized pieces each get their own sheet, recentred to the sheet origin.
  for (k, &pi) in result.oversized.iter().enumerate() {
    placed.push(Placed {
      piece_index: pi,
      sheet: result.sheets + k,
      transform: Affine::place(&piece_bboxes[pi], 0.0, 0.0, 0.0),
      oversized: true,
      locked: false,
    });
  }
  let sheets = result.sheets + result.oversized.len();
  let canceled = cancel_probe.is_some_and(|c| c.load(Ordering::Relaxed));

  NestOutcome { placed, oversized: result.oversized, unplaced: result.unplaced, sheets, canceled }
}

/// Pin-and-re-nest (P2-9): keep the `fixed` (user-locked) placements exactly
/// where they are and re-pack every other piece around them, then place oversized
/// pieces on their own sheets. Routes through the jagua bin-packing path
/// (`nest::nest_pinned`) because sparrow strip-packing cannot take pre-placed
/// obstacles; locked pieces come back byte-identical (see `nest::nest_pinned`).
///
/// `items` covers all pieces; `fixed` are the locked placements to preserve.
/// `on_event` streams `Progress` during packing and a `SheetCompleted` per sheet
/// once the layout is final.
#[allow(clippy::too_many_arguments)]
pub fn nest_sheets_pinned(
  items: &[NestItem],
  piece_bboxes: &[Bbox],
  fixed: &[Placed],
  sheet_w: f64,
  sheet_h: f64,
  kerf: f64,
  margin: f64,
  cancel: Option<Arc<AtomicBool>>,
  mut on_event: impl FnMut(NestEvent),
) -> NestOutcome {
  let cancel_probe = cancel.clone();
  let result = crate::nest::nest_pinned(
    items,
    fixed,
    sheet_w,
    sheet_h,
    kerf,
    margin,
    cancel.as_deref(),
    |done, total| {
      if total > 0 {
        on_event(NestEvent::Progress { fraction: done as f32 / total as f32 });
      }
    },
  )
  .expect("valid pinned bin-packing instance");

  let mut placed = result.placed;
  // Oversized free pieces each get their own sheet, recentred to the origin.
  for (k, &pi) in result.oversized.iter().enumerate() {
    placed.push(Placed {
      piece_index: pi,
      sheet: result.sheets + k,
      transform: Affine::place(&piece_bboxes[pi], 0.0, 0.0, 0.0),
      oversized: true,
      locked: false,
    });
  }
  let sheets = result.sheets + result.oversized.len();

  // Stream each finalized sheet (placements are immutable now).
  for s in 0..sheets {
    let sub: Vec<Placed> = placed.iter().filter(|p| p.sheet == s).cloned().collect();
    on_event(NestEvent::SheetCompleted { sheet: s, placed: &sub });
  }

  let canceled = cancel_probe.is_some_and(|c| c.load(Ordering::Relaxed));
  NestOutcome { placed, oversized: result.oversized, unplaced: result.unplaced, sheets, canceled }
}
