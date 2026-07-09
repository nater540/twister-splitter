//! Shape-aware nesting via `jagua-rs`: pack irregular piece polygons onto
//! fixed-size sheets with free rotation and a kerf gap, using a greedy
//! bottom-left-fill placement loop over jagua's collision-detection engine.
//!
//! jagua provides the collision engine and the bin-packing problem model but no
//! packing heuristic, so the placement strategy here is ours. Density is well
//! above bounding-box packing but below jagua's research-grade optimizer
//! (`sparrow`); a metaheuristic could be swapped in later behind this seam.

use std::collections::{HashMap, HashSet};
use std::f32::consts::PI;
use std::sync::atomic::{AtomicBool, Ordering};

use jagua_rs::collision_detection::CDEConfig;
use jagua_rs::collision_detection::hazards::filter::NoFilter;
use jagua_rs::entities::{Instance, Item, Layout};
use jagua_rs::geometry::fail_fast::SPSurrogateConfig;
use jagua_rs::geometry::geo_enums::RotationRange;
use jagua_rs::geometry::geo_traits::TransformableFrom;
use jagua_rs::geometry::primitives::{Point, SPolygon};
use jagua_rs::geometry::{DTransformation, Transformation};
use jagua_rs::io::export::int_to_ext_transformation;
use jagua_rs::io::ext_repr::{ExtContainer, ExtItem as BaseExtItem, ExtSPolygon, ExtShape};
use jagua_rs::io::import::{Importer, ext_to_int_transformation};
use jagua_rs::probs::bpp::entities::{BPLayoutType, BPPlacement, BPProblem, LayKey};
use jagua_rs::probs::bpp::io::ext_repr::{ExtBPInstance, ExtBin, ExtItem as BpItem};
use jagua_rs::probs::bpp::io::import_instance;

use dxf::Drawing;

use crate::emit::Placed;
use crate::extract::{Piece, PieceKind};
use crate::flatten;
use crate::geom::{Affine, Bbox};

/// A piece's nesting polygon, tagged with the piece it belongs to.
#[derive(Clone)]
pub struct NestItem {
  pub piece_index: usize,
  /// Outline in the piece's own coordinate frame (same frame as its entities).
  pub polygon: Vec<[f64; 2]>,
  /// True when `polygon` is non-simple (self-intersecting after flattening), so
  /// the nester reserves its convex hull instead. Such a piece reserves *more*
  /// than its real outline; the UI can badge it so the slot looks under-filled
  /// for a reason.
  pub hull_fallback: bool,
}

/// Flatten every piece to a single nesting polygon that contains all of its
/// geometry (see [`build_items_with`]; kerf compensation off).
pub fn build_items(drawing: &Drawing, pieces: &[Piece]) -> Vec<NestItem> {
  build_items_with(drawing, pieces, 0.0)
}

/// Flatten every piece to a single nesting polygon that contains all of its
/// geometry. A piece with no substantial outline falls back to its bounding-box
/// rectangle so it still nests rather than being treated as unplaceable.
///
/// When `kerf_comp > 0`, the emitted cut outline is offset outward by
/// `kerf_comp / 2` (kerf compensation). The reservation is then built from the
/// *compensated* rings, so `piece_polygon`'s containment guarantee covers the
/// enlarged outline — otherwise the compensated (larger) cut could spill past
/// its slot into a neighbour.
pub fn build_items_with(drawing: &Drawing, pieces: &[Piece], kerf_comp: f64) -> Vec<NestItem> {
  let half = (kerf_comp * 0.5).max(0.0);
  pieces
    .iter()
    .enumerate()
    .map(|(i, piece)| {
      let mut rings = Vec::new();
      match &piece.kind {
        PieceKind::Loose(entities) => {
          for e in entities {
            flatten::entity_rings(e, &mut rings);
          }
        }
        PieceKind::Insert { block_name, .. } => {
          if let Some(b) = drawing.blocks().find(|b| &b.name == block_name) {
            for e in &b.entities {
              flatten::entity_rings(e, &mut rings);
            }
          }
        }
      }
      // Reserve from the compensated rings so the enlarged outline is contained.
      if half > 0.0 {
        rings = flatten::compensate_rings(&rings, half);
      }
      let polygon = flatten::piece_polygon(&rings, 4.0, 0.4).unwrap_or_else(|| {
        // No usable outline: reserve a rectangle. With compensation on, emit
        // outputs the compensated rings as polylines, so reserve THEIR bbox (the
        // piece bbox is the pre-compensation extent and could be too small). With
        // compensation off, emit outputs the original entities, so the piece bbox
        // (a control-point over-estimate) is the safe reservation.
        let mut bb = Bbox::empty();
        if half > 0.0 {
          for r in &rings {
            for &[x, y] in r {
              bb.add_point(x, y);
            }
          }
        }
        if bb.is_empty() {
          bb = piece.bbox;
        }
        vec![
          [bb.min_x, bb.min_y],
          [bb.max_x, bb.min_y],
          [bb.max_x, bb.max_y],
          [bb.min_x, bb.max_y],
        ]
      });
      // Record up-front whether jagua will reject this ring as non-simple (so it
      // is nested by its hull). Same test `valid_ring_or_hull` applies, but done
      // here it can be reported per-piece rather than as an aggregate count.
      let pts: Vec<Point> = polygon.iter().map(|&[x, y]| Point(x as f32, y as f32)).collect();
      let hull_fallback = SPolygon::new(pts).is_err();
      NestItem { piece_index: i, polygon, hull_fallback }
    })
    .collect()
}

/// Outcome of a nesting run.
#[derive(Clone)]
pub struct NestResult {
  /// Placed pieces, one per fitted item, transform in original coordinates.
  pub placed: Vec<Placed>,
  /// Number of sheets the fitted pieces occupy (sheet indices `0..sheets`).
  pub sheets: usize,
  /// Piece indices that did not fit on any sheet (too large even rotated).
  pub oversized: Vec<usize>,
  /// Piece indices left unplaced because the run was cancelled before reaching
  /// them (empty on a run that finished normally).
  pub unplaced: Vec<usize>,
}

/// Return `polygon` as an `(f32,f32)` ring if jagua accepts it as a simple
/// polygon, otherwise its convex hull. Increments `fallbacks` on hull use.
pub(crate) fn valid_ring_or_hull(polygon: &[[f64; 2]], fallbacks: &mut usize) -> Vec<(f32, f32)> {
  let pts: Vec<Point> = polygon.iter().map(|&[x, y]| Point(x as f32, y as f32)).collect();
  if SPolygon::new(pts).is_ok() {
    polygon.iter().map(|&[x, y]| (x as f32, y as f32)).collect()
  } else {
    *fallbacks += 1;
    crate::flatten::convex_hull(polygon.to_vec())
      .iter()
      .map(|&[x, y]| (x as f32, y as f32))
      .collect()
  }
}

/// CDE / surrogate configuration (values taken from sparrow's defaults).
fn cde_config() -> CDEConfig {
  CDEConfig {
    quadtree_depth: 4,
    cd_threshold: 16,
    item_surrogate_config: SPSurrogateConfig {
      n_pole_limits: [(64, 0.0), (16, 0.8), (8, 0.9)],
      n_ff_poles: 1,
      n_ff_piers: 0,
    },
  }
}

const N_ROT: usize = 24; // rotation samples for continuous rotation
const GRID: usize = 48; // translation grid resolution per axis

/// Build a jagua bin-packing instance from `items` on `sheet_w`×`sheet_h` sheets
/// with a `kerf` separation and a `margin` edge inset (shared by `nest` and
/// `nest_pinned`). Every polygon is gated through jagua's `SPolygon` validator;
/// a rejected (self-intersecting) ring falls back to its convex hull, so import
/// never fails on a bad ring. Hull fallbacks are surfaced per-piece via
/// `NestItem::hull_fallback`, so nothing is printed here.
fn build_bpp_instance(
  items: &[NestItem],
  sheet_w: f64,
  sheet_h: f64,
  kerf: f64,
  margin: f64,
) -> Result<jagua_rs::probs::bpp::entities::BPInstance, String> {
  let mut hull_fallbacks = 0usize;
  let ext_items: Vec<BpItem> = items
    .iter()
    .enumerate()
    .map(|(i, it)| {
      let ring = valid_ring_or_hull(&it.polygon, &mut hull_fallbacks);
      BpItem {
        base: BaseExtItem {
          id: i as u64,
          allowed_orientations: None, // None => continuous (free) rotation
          shape: ExtShape::SimplePolygon(ExtSPolygon(ring)),
          min_quality: None,
        },
        demand: 1,
      }
    })
    .collect();

  // Inset the container by the edge margin so nothing is placed in the margin.
  let margin = margin.max(0.0) as f32;
  let bins = vec![ExtBin {
    base: ExtContainer {
      id: 0,
      shape: ExtShape::Rectangle {
        x_min: margin,
        y_min: margin,
        width: (sheet_w as f32 - 2.0 * margin).max(0.0),
        height: (sheet_h as f32 - 2.0 * margin).max(0.0),
      },
      zones: vec![],
    },
    stock: items.len(), // generous: never run out of sheets
    cost: 1,
  }];

  let separation = if kerf > 0.0 { Some(kerf as f32) } else { None };
  let importer = Importer::new(cde_config(), None, separation, None);
  import_instance(&importer, &ExtBPInstance { name: "twister".into(), items: ext_items, bins })
    .map_err(|e| format!("jagua instance import failed: {e}"))
}

/// Nest `items` onto `sheet_w` x `sheet_h` sheets with a `kerf` gap.
///
/// `progress` is invoked as each piece is placed with `(done, total)`, so a
/// caller can drive a progress bar without the library depending on any UI.
/// `cancel`, when set true, stops the loop at the next piece boundary and
/// returns a partial result (remaining pieces go in `NestResult::unplaced`).
/// `margin` reserves a usable inset (mm) on every sheet edge by nesting into an
/// inset container, so no part sits in the margin band.
#[allow(clippy::too_many_arguments)]
pub fn nest(
  items: &[NestItem],
  sheet_w: f64,
  sheet_h: f64,
  kerf: f64,
  margin: f64,
  cancel: Option<&AtomicBool>,
  mut progress: impl FnMut(usize, usize),
) -> Result<NestResult, String> {
  if items.is_empty() {
    return Ok(NestResult { placed: vec![], sheets: 0, oversized: vec![], unplaced: vec![] });
  }

  // --- greedy placement, largest piece first ------------------------------
  let instance = build_bpp_instance(items, sheet_w, sheet_h, kerf, margin)?;
  let mut prob = BPProblem::new(instance);
  let mut order: Vec<usize> = (0..items.len()).collect();
  order.sort_by(|&a, &b| {
    let da = prob.instance.item(a).shape_cd.diameter;
    let db = prob.instance.item(b).shape_cd.diameter;
    db.partial_cmp(&da).unwrap_or(std::cmp::Ordering::Equal)
  });

  let total = order.len();
  let mut oversized = Vec::new();
  let mut unplaced = Vec::new();
  let mut canceled_at = None;
  for (done, id) in order.iter().copied().enumerate() {
    // Cooperative cancellation: stop at the next piece boundary.
    if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
      canceled_at = Some(done);
      break;
    }
    if !place_greedy(&mut prob, id, GRID) {
      oversized.push(items[id].piece_index);
    }
    progress(done + 1, total);
  }
  if let Some(from) = canceled_at {
    unplaced.extend(order[from..].iter().map(|&id| items[id].piece_index));
  }


  // --- read back placements in original coordinates -----------------------
  let mut placed = Vec::new();
  let mut sheets = 0usize;
  for (sheet, (_lkey, layout)) in prob.layouts.iter().enumerate() {
    sheets = sheets.max(sheet + 1);
    for (_pik, pi) in &layout.placed_items {
      let item = prob.instance.item(pi.item_id);
      let ext = int_to_ext_transformation(&pi.d_transf, &item.shape_orig.pre_transform);
      let theta = ext.rotation() as f64;
      let (tx, ty) = ext.translation();
      let (s, c) = theta.sin_cos();
      placed.push(Placed {
        piece_index: items[pi.item_id].piece_index,
        sheet,
        transform: Affine {
          m00: c,
          m01: -s,
          m10: s,
          m11: c,
          tx: tx as f64,
          ty: ty as f64,
        },
        oversized: false,
        locked: false,
      });
    }
  }

  Ok(NestResult { placed, sheets, oversized, unplaced })
}

/// Re-nest around pinned placements (P2-9). `fixed` are placements the user has
/// locked: they are inserted into the jagua bin-packing model as immovable
/// obstacles, and the remaining (free) pieces are packed around them. This is
/// the jagua path rather than sparrow, because sparrow strip-packing cannot take
/// pre-placed obstacles.
///
/// Guarantees (see `tests/pin_renest.rs`):
/// * **Locked pieces are byte-identical.** Their `Placed` is copied straight
///   through to the output; jagua is used only to make them collision hazards,
///   never to recompute their transform or sheet.
/// * **No overlap.** Free pieces are placed through jagua's collision engine,
///   which rejects any position overlapping a locked hazard, another free piece,
///   or the (margin-inset) sheet boundary.
///
/// `items` covers *all* pieces (locked and free); the free set is `items` whose
/// `piece_index` is not among `fixed`. A free piece too large to fit is returned
/// in `NestResult::oversized`; on cancellation the unreached free pieces go to
/// `unplaced`.
#[allow(clippy::too_many_arguments)]
pub fn nest_pinned(
  items: &[NestItem],
  fixed: &[Placed],
  sheet_w: f64,
  sheet_h: f64,
  kerf: f64,
  margin: f64,
  cancel: Option<&AtomicBool>,
  mut progress: impl FnMut(usize, usize),
) -> Result<NestResult, String> {
  if items.is_empty() {
    return Ok(NestResult { placed: vec![], sheets: 0, oversized: vec![], unplaced: vec![] });
  }

  let instance = build_bpp_instance(items, sheet_w, sheet_h, kerf, margin)?;
  let mut prob = BPProblem::new(instance);

  // piece_index -> jagua item id (items are imported in `items` order).
  let piece_to_item: HashMap<usize, usize> =
    items.iter().enumerate().map(|(i, it)| (it.piece_index, i)).collect();
  let locked_pieces: HashSet<usize> = fixed.iter().map(|p| p.piece_index).collect();
  // Every sheet a locked piece sits on is reserved: no free piece may be numbered
  // onto it unless it is physically placed into that same locked layout.
  let locked_sheets: HashSet<usize> = fixed.iter().map(|p| p.sheet).collect();

  // Insert each non-oversized locked piece as a forced obstacle, grouping all
  // locked pieces of one sheet into a single jagua layout. Oversized locked
  // pieces are NOT inserted (they own an isolated sheet); their `Placed` still
  // flows through unchanged and their sheet stays reserved.
  let mut sheet_layout: HashMap<usize, LayKey> = HashMap::new();
  let mut locked_item_sheet: HashMap<usize, usize> = HashMap::new();
  let mut fixed_sorted: Vec<&Placed> = fixed.iter().filter(|p| !p.oversized).collect();
  fixed_sorted.sort_by_key(|p| p.sheet);
  for f in fixed_sorted {
    let Some(&item_id) = piece_to_item.get(&f.piece_index) else { continue };
    // Convert the locked placement (an external, absolute-sheet transform) into
    // jagua's internal frame via the item's pre-transform — the exact inverse of
    // the `int_to_ext_transformation` used to read placements back out.
    let pre = prob.instance.item(item_id).shape_orig.pre_transform;
    let ext_dt = DTransformation::new(
      f.transform.rotation() as f32,
      (f.transform.tx as f32, f.transform.ty as f32),
    );
    let int_dt = ext_to_int_transformation(&ext_dt, &pre);
    let layout_id = match sheet_layout.get(&f.sheet) {
      Some(&lk) => BPLayoutType::Open(lk),
      None => BPLayoutType::Closed { bin_id: 0 },
    };
    let (lk, _) = prob.place_item(BPPlacement { layout_id, item_id, d_transf: int_dt });
    sheet_layout.entry(f.sheet).or_insert(lk);
    locked_item_sheet.insert(item_id, f.sheet);
  }

  // Greedily place the free pieces (largest first) around the locked obstacles.
  let mut order: Vec<usize> = (0..items.len())
    .filter(|&i| !locked_pieces.contains(&items[i].piece_index))
    .collect();
  order.sort_by(|&a, &b| {
    let da = prob.instance.item(a).shape_cd.diameter;
    let db = prob.instance.item(b).shape_cd.diameter;
    db.partial_cmp(&da).unwrap_or(std::cmp::Ordering::Equal)
  });

  let total = order.len();
  let mut oversized = Vec::new();
  let mut unplaced = Vec::new();
  let mut canceled_at = None;
  for (done, id) in order.iter().copied().enumerate() {
    if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
      canceled_at = Some(done);
      break;
    }
    if !place_greedy(&mut prob, id, GRID) {
      oversized.push(items[id].piece_index);
    }
    progress(done + 1, total);
  }
  if let Some(from) = canceled_at {
    unplaced.extend(order[from..].iter().map(|&id| items[id].piece_index));
  }

  // --- read back ----------------------------------------------------------
  // Locked pieces flow through byte-identical; only free pieces are read from
  // jagua. Each layout's output sheet index is the reserved sheet of the locked
  // piece it holds, or the lowest unused index for a fresh (free-only) layout.
  let mut layout_sheet: HashMap<LayKey, usize> = HashMap::new();
  let mut fresh: Vec<(f64, f64, LayKey)> = Vec::new();
  for (lk, layout) in prob.layouts.iter() {
    let locked = layout
      .placed_items
      .values()
      .find_map(|pi| locked_item_sheet.get(&pi.item_id).copied());
    match locked {
      Some(s) => {
        layout_sheet.insert(lk, s);
      }
      None => {
        // Deterministic sheet numbering: order fresh layouts by min corner.
        let (mut mnx, mut mny) = (f64::MAX, f64::MAX);
        for pi in layout.placed_items.values() {
          let ext = int_to_ext_transformation(&pi.d_transf, &prob.instance.item(pi.item_id).shape_orig.pre_transform);
          let (tx, ty) = ext.translation();
          mnx = mnx.min(tx as f64);
          mny = mny.min(ty as f64);
        }
        fresh.push((mnx, mny, lk));
      }
    }
  }
  fresh.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
  let mut next = 0usize;
  for (_, _, lk) in &fresh {
    while locked_sheets.contains(&next) {
      next += 1;
    }
    layout_sheet.insert(*lk, next);
    next += 1;
  }

  // Locked placements pass through unchanged (byte-identical).
  let mut placed: Vec<Placed> = fixed.to_vec();
  for (lk, layout) in prob.layouts.iter() {
    let sheet = layout_sheet[&lk];
    for pi in layout.placed_items.values() {
      let piece_index = items[pi.item_id].piece_index;
      if locked_pieces.contains(&piece_index) {
        continue; // locked: already emitted from `fixed`
      }
      let ext = int_to_ext_transformation(&pi.d_transf, &prob.instance.item(pi.item_id).shape_orig.pre_transform);
      let theta = ext.rotation() as f64;
      let (tx, ty) = ext.translation();
      let (s, c) = theta.sin_cos();
      placed.push(Placed {
        piece_index,
        sheet,
        transform: Affine { m00: c, m01: -s, m10: s, m11: c, tx: tx as f64, ty: ty as f64 },
        oversized: false,
        locked: false,
      });
    }
  }
  let sheets = placed.iter().map(|p| p.sheet).max().map_or(0, |m| m + 1);
  Ok(NestResult { placed, sheets, oversized, unplaced })
}

/// Bottom-left objective: heavily weight leftward, then downward, so pieces
/// pack toward one corner (following sparrow's 10:1 weighting).
fn objective(bb: &jagua_rs::geometry::primitives::Rect) -> f32 {
  10.0 * bb.x_min + bb.y_min
}

/// Try to place item `id` into any open sheet, else open a new one.
/// Returns false if it fits nowhere (larger than a sheet even rotated).
fn place_greedy(prob: &mut BPProblem, id: usize, grid: usize) -> bool {
  if place_into_existing(prob, id, grid) {
    return true;
  }
  // Spill onto a fresh bin (bin type 0 has effectively unlimited stock here).
  if prob.bin_stock_qtys[0] > 0 {
    let scratch = Layout::new(prob.instance.bins[0].container.clone());
    if let Some(dt) = best_placement(&scratch, prob.instance.item(id), grid) {
      prob.place_item(BPPlacement {
        layout_id: BPLayoutType::Closed { bin_id: 0 },
        item_id: id,
        d_transf: dt,
      });
      return true;
    }
  }
  false
}

/// Place item `id` into the first open sheet it fits, without opening a new one.
fn place_into_existing(prob: &mut BPProblem, id: usize, grid: usize) -> bool {
  let open: Vec<_> = prob.layouts.keys().collect();
  for lkey in open {
    if let Some(dt) = best_placement(&prob.layouts[lkey], prob.instance.item(id), grid) {
      prob.place_item(BPPlacement {
        layout_id: BPLayoutType::Open(lkey),
        item_id: id,
        d_transf: dt,
      });
      return true;
    }
  }
  false
}

fn rotation_set(item: &Item) -> Vec<f32> {
  match &item.allowed_rotation {
    RotationRange::None => vec![0.0],
    RotationRange::Discrete(v) => v.to_vec(),
    RotationRange::Continuous => (0..N_ROT).map(|i| i as f32 * (2.0 * PI) / N_ROT as f32).collect(),
  }
}

/// Return the lowest-objective feasible placement of `item` in `layout`, refined
/// by local search, or None if it does not fit. `grid` controls the coarse scan
/// resolution (higher = more thorough, e.g. for squeezing into leftover gaps).
fn best_placement(layout: &Layout, item: &Item, grid: usize) -> Option<DTransformation> {
  let cont = layout.container.outer_cd.bbox;
  let mut buf = item.shape_cd.as_ref().clone();
  let mut rbuf = item.shape_cd.as_ref().clone();

  let mut best: Option<(f32, DTransformation)> = None;
  for r in rotation_set(item) {
    // Valid translation window keeps the rotated item fully inside the sheet.
    let r_bbox = rbuf
      .transform_from(&item.shape_cd, &Transformation::from_rotation(r))
      .bbox;
    let (x0, x1) = (cont.x_min - r_bbox.x_min, cont.x_max - r_bbox.x_max);
    let (y0, y1) = (cont.y_min - r_bbox.y_min, cont.y_max - r_bbox.y_max);
    if x1 <= x0 || y1 <= y0 {
      continue; // doesn't fit at this rotation
    }
    for iy in 0..=grid {
      let ty = y0 + (y1 - y0) * (iy as f32 / grid as f32);
      for ix in 0..=grid {
        let tx = x0 + (x1 - x0) * (ix as f32 / grid as f32);
        let dt = DTransformation::new(r, (tx, ty));
        if feasible(layout, item, &dt, &mut buf) {
          let obj = objective(&buf.bbox);
          if best.as_ref().is_none_or(|(o, _)| obj < *o) {
            best = Some((obj, dt));
          }
          break; // left-most feasible x in this row; move up
        }
      }
    }
  }

  // Local-search refinement: slide the best placement toward the corner in
  // shrinking steps. This tightens packing and lets pieces settle into gaps and
  // concavities a coarse grid alone would miss.
  best.map(|(mut cur, mut dt)| {
    let span = (cont.x_max - cont.x_min).max(cont.y_max - cont.y_min);
    let mut step = span / grid as f32;
    let r = dt.rotation();
    let (mut x, mut y) = dt.translation();
    while step > 0.05 {
      let mut improved = false;
      for (dx, dy) in [(-step, 0.0), (0.0, -step), (-step, -step)] {
        let cand = DTransformation::new(r, (x + dx, y + dy));
        if feasible(layout, item, &cand, &mut buf) {
          let obj = objective(&buf.bbox);
          if obj < cur {
            x += dx;
            y += dy;
            cur = obj;
            dt = cand;
            improved = true;
            break;
          }
        }
      }
      if !improved {
        step /= 2.0;
      }
    }
    dt
  })
}

/// True if placing `item` at `dt` in `layout` is collision-free and inside the
/// sheet. Leaves `buf` holding the transformed shape (its bbox is then valid).
fn feasible(layout: &Layout, item: &Item, dt: &DTransformation, buf: &mut SPolygon) -> bool {
  let cde = layout.cde();
  let t = dt.compose();
  // Cheap inscribed-surrogate reject.
  if cde.detect_surrogate_collision(item.shape_cd.surrogate(), &t, &NoFilter) {
    return false;
  }
  // Exact test; also rejects anything poking outside the container boundary.
  buf.transform_from(&item.shape_cd, &t);
  !cde.detect_poly_collision(buf, &NoFilter)
}
