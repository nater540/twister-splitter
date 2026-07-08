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
use sparrow::util::terminator::BasicTerminator;

use crate::emit::Placed;
use crate::geom::Affine;
use crate::nest::{NestItem, NestResult, valid_ring_or_hull};

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
#[allow(clippy::too_many_arguments)]
pub fn nest_sparrow(
  items: &[NestItem],
  sheet_w: f64,
  sheet_h: f64,
  kerf: f64,
  seed: u64,
  explore: Duration,
  compress: Duration,
  mut progress: impl FnMut(usize),
) -> NestResult {
  // Split off pieces that cannot fit a sheet at any rotation.
  let mut oversized = Vec::new();
  let mut remaining: Vec<&NestItem> = Vec::new();
  for it in items {
    if fits_sheet(&it.polygon, sheet_w, sheet_h) {
      remaining.push(it);
    } else {
      oversized.push(it.piece_index);
    }
  }
  if remaining.is_empty() {
    return NestResult { placed: Vec::new(), sheets: 0, oversized };
  }

  let mut cfg = DEFAULT_SPARROW_CONFIG;
  cfg.expl_cfg.time_limit = explore;
  cfg.cmpr_cfg.time_limit = compress;

  // Peel sheets off the strip: strip-pack the remaining pieces, keep those that
  // land fully within the first sheet width, re-pack the rest onto the next sheet.
  let mut rem = remaining;
  let mut placed = Vec::new();
  let mut sheet = 0usize;
  while !rem.is_empty() {
    let mut round = strip_pack(&rem, sheet_h, kerf, &cfg, seed.wrapping_add(sheet as u64));
    round.sort_by(|a, b| a.max_x.partial_cmp(&b.max_x).unwrap_or(std::cmp::Ordering::Equal));

    let mut next: Vec<&NestItem> = Vec::new();
    let mut assigned_any = false;
    for p in &round {
      if p.max_x <= sheet_w + 1e-3 {
        placed.push(Placed { piece_index: rem[p.jid].piece_index, sheet, transform: p.xf, oversized: false });
        assigned_any = true;
      } else {
        next.push(rem[p.jid]);
      }
    }
    if !assigned_any {
      placed.push(Placed { piece_index: rem[round[0].jid].piece_index, sheet, transform: round[0].xf, oversized: false });
      next = round[1..].iter().map(|p| rem[p.jid]).collect();
    }

    rem = next;
    sheet += 1;
    progress(sheet);
  }

  NestResult { placed, sheets: sheet, oversized }
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
fn strip_pack(
  polys: &[&NestItem],
  sheet_h: f64,
  kerf: f64,
  cfg: &sparrow::config::SparrowConfig,
  seed: u64,
) -> Vec<Pl> {
  let (instance, _) = build_instance(polys, sheet_h, kerf);
  let rng = Xoshiro256PlusPlus::seed_from_u64(seed);
  let solution = optimize(
    instance.clone(),
    rng,
    &mut DummySolListener,
    &mut BasicTerminator::new(),
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
