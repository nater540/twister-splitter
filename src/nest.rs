//! Shape-aware nesting via `jagua-rs`: pack irregular piece polygons onto
//! fixed-size sheets with free rotation and a kerf gap, using a greedy
//! bottom-left-fill placement loop over jagua's collision-detection engine.
//!
//! jagua provides the collision engine and the bin-packing problem model but no
//! packing heuristic, so the placement strategy here is ours. Density is well
//! above bounding-box packing but below jagua's research-grade optimizer
//! (`sparrow`); a metaheuristic could be swapped in later behind this seam.

use std::f32::consts::PI;

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
use jagua_rs::io::import::Importer;
use jagua_rs::probs::bpp::entities::{BPLayoutType, BPPlacement, BPProblem};
use jagua_rs::probs::bpp::io::ext_repr::{ExtBPInstance, ExtBin, ExtItem as BpItem};
use jagua_rs::probs::bpp::io::import_instance;

use dxf::Drawing;

use crate::emit::Placed;
use crate::extract::{Piece, PieceKind};
use crate::flatten;
use crate::geom::Affine;

/// A piece's nesting polygon, tagged with the piece it belongs to.
pub struct NestItem {
  pub piece_index: usize,
  /// Outline in the piece's own coordinate frame (same frame as its entities).
  pub polygon: Vec<[f64; 2]>,
}

/// Flatten every piece to a single nesting polygon that contains all of its
/// geometry. A piece with no substantial outline falls back to its bounding-box
/// rectangle so it still nests rather than being treated as unplaceable.
pub fn build_items(drawing: &Drawing, pieces: &[Piece]) -> Vec<NestItem> {
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
      let polygon = flatten::piece_polygon(&rings, 4.0, 0.4).unwrap_or_else(|| {
        let b = &piece.bbox;
        vec![
          [b.min_x, b.min_y],
          [b.max_x, b.min_y],
          [b.max_x, b.max_y],
          [b.min_x, b.max_y],
        ]
      });
      NestItem { piece_index: i, polygon }
    })
    .collect()
}

/// Outcome of a nesting run.
pub struct NestResult {
  /// Placed pieces, one per fitted item, transform in original coordinates.
  pub placed: Vec<Placed>,
  /// Number of sheets the fitted pieces occupy (sheet indices `0..sheets`).
  pub sheets: usize,
  /// Piece indices that did not fit on any sheet (too large even rotated).
  pub oversized: Vec<usize>,
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

/// Nest `items` onto `sheet_w` x `sheet_h` sheets with a `kerf` gap.
///
/// `progress` is invoked as each piece is placed with `(done, total)`, so a
/// caller can drive a progress bar without the library depending on any UI.
pub fn nest(
  items: &[NestItem],
  sheet_w: f64,
  sheet_h: f64,
  kerf: f64,
  mut progress: impl FnMut(usize, usize),
) -> Result<NestResult, String> {
  if items.is_empty() {
    return Ok(NestResult { placed: vec![], sheets: 0, oversized: vec![] });
  }

  // --- build the jagua instance -------------------------------------------
  // Gate every polygon through jagua's own SPolygon validator; if it rejects a
  // ring (self-intersecting after flattening/simplification), fall back to the
  // convex hull, which is always a valid simple polygon. This guarantees the
  // instance import never fails on a bad ring.
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
  if hull_fallbacks > 0 {
    eprintln!(
      "note: {hull_fallbacks} piece(s) had a non-simple outline and were nested by their convex hull"
    );
  }

  let bins = vec![ExtBin {
    base: ExtContainer {
      id: 0,
      shape: ExtShape::Rectangle {
        x_min: 0.0,
        y_min: 0.0,
        width: sheet_w as f32,
        height: sheet_h as f32,
      },
      zones: vec![],
    },
    stock: items.len(), // generous: never run out of sheets
    cost: 1,
  }];

  let separation = if kerf > 0.0 { Some(kerf as f32) } else { None };
  let importer = Importer::new(cde_config(), None, separation, None);
  let instance = import_instance(
    &importer,
    &ExtBPInstance { name: "twister".into(), items: ext_items, bins },
  )
  .map_err(|e| format!("jagua instance import failed: {e}"))?;

  // --- greedy placement, largest piece first ------------------------------
  let mut prob = BPProblem::new(instance);
  let mut order: Vec<usize> = (0..items.len()).collect();
  order.sort_by(|&a, &b| {
    let da = prob.instance.item(a).shape_cd.diameter;
    let db = prob.instance.item(b).shape_cd.diameter;
    db.partial_cmp(&da).unwrap_or(std::cmp::Ordering::Equal)
  });

  let total = order.len();
  let mut oversized = Vec::new();
  for (done, id) in order.into_iter().enumerate() {
    if !place_greedy(&mut prob, id, GRID) {
      oversized.push(items[id].piece_index);
    }
    progress(done + 1, total);
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
      });
    }
  }

  Ok(NestResult { placed, sheets, oversized })
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
