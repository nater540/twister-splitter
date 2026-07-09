//! Confirms the "lopsided" leftover sheet (and every sheet) is CORRECT at the
//! GUI's default settings: with a sheet margin enforced, all placed parts stay
//! inside the usable [margin, size-margin] area and no two parts on a sheet
//! overlap. The lopsidedness itself is expected sparrow strip-peel behavior; this
//! guards the invariants that actually matter (no overlap, within margin).

use std::collections::HashMap;
use std::time::Duration;

use dxf::Drawing;

use twister_splitter::emit::Placed;
use twister_splitter::extract::{Sources, extract};
use twister_splitter::nest::{self, NestItem};
use twister_splitter::optimize::nest_sheets;

const SHEET: f64 = 400.0;
const SPACING: f64 = 2.0;
const MARGIN: f64 = 6.0; // GUI default sheet margin
const SEED: u64 = 0x9E37_79B9_7F4A_7C15;

fn point_in_ring(pt: [f64; 2], ring: &[[f64; 2]]) -> bool {
  let mut inside = false;
  let mut j = ring.len() - 1;
  for i in 0..ring.len() {
    let (a, b) = (ring[i], ring[j]);
    if (a[1] > pt[1]) != (b[1] > pt[1]) {
      let x = a[0] + (pt[1] - a[1]) / (b[1] - a[1]) * (b[0] - a[0]);
      if pt[0] < x {
        inside = !inside;
      }
    }
    j = i;
  }
  inside
}

fn bbox(ring: &[[f64; 2]]) -> (f64, f64, f64, f64) {
  let (mut nx, mut ny, mut xx, mut xy) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
  for &[x, y] in ring {
    nx = nx.min(x);
    ny = ny.min(y);
    xx = xx.max(x);
    xy = xy.max(y);
  }
  (nx, ny, xx, xy)
}

/// Overlap by interior-grid sampling (as in tests/pin_renest.rs). With the kerf
/// separation, disjoint reserved polygons never share a sample point; a real
/// overlap always produces one.
fn overlap(a: &[[f64; 2]], b: &[[f64; 2]]) -> bool {
  let (anx, any, axx, axy) = bbox(a);
  let (bnx, bny, bxx, bxy) = bbox(b);
  if axx < bnx - 1e-9 || bxx < anx - 1e-9 || axy < bny - 1e-9 || bxy < any - 1e-9 {
    return false;
  }
  let sample = |src: &[[f64; 2]], dst: &[[f64; 2]]| {
    let (nx, ny, xx, xy) = bbox(src);
    let step = ((xx - nx).max(xy - ny) / 12.0).max(1.0);
    let mut y = ny;
    while y <= xy {
      let mut x = nx;
      while x <= xx {
        if point_in_ring([x, y], src) && point_in_ring([x, y], dst) {
          return true;
        }
        x += step;
      }
      y += step;
    }
    false
  };
  sample(a, b) || sample(b, a)
}

/// A piece's reserved nesting polygon, transformed into place. Overlap of these
/// (the shapes the collision engine separates) is the packing correctness check.
fn placed_ring(items: &[NestItem], by_piece: &HashMap<usize, usize>, pl: &Placed) -> Vec<[f64; 2]> {
  let it = &items[by_piece[&pl.piece_index]];
  it.polygon.iter().map(|&[x, y]| { let (a, b) = pl.transform.apply(x, y); [a, b] }).collect()
}

#[test]
fn every_sheet_is_overlap_free_and_within_margin() {
  let path = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/gengar-stacked.dxf");
  let drawing = Drawing::load_file(path).expect("load fixture");
  let (pieces, _d) = extract(&drawing, Sources::Both);
  let items = nest::build_items(&drawing, &pieces);
  let by_piece: HashMap<usize, usize> = items.iter().enumerate().map(|(i, it)| (it.piece_index, i)).collect();
  let bboxes: Vec<_> = pieces.iter().map(|p| p.bbox).collect();

  let outcome = nest_sheets(
    &items, &bboxes, SHEET, SHEET, SPACING, MARGIN, SEED,
    Duration::from_millis(700), Duration::from_millis(200), None, |_| {},
  );

  let mut by_sheet: HashMap<usize, Vec<&Placed>> = HashMap::new();
  for p in &outcome.placed {
    by_sheet.entry(p.sheet).or_default().push(p);
  }

  for group in by_sheet.values() {
    // Within the usable margin-inset area (skip oversized, which own a sheet).
    for p in group {
      if p.oversized {
        continue;
      }
      let ring = placed_ring(&items, &by_piece, p);
      let (nx, ny, xx, xy) = bbox(&ring);
      assert!(
        nx >= MARGIN - 0.5 && ny >= MARGIN - 0.5 && xx <= SHEET - MARGIN + 0.5 && xy <= SHEET - MARGIN + 0.5,
        "piece {} sits in the margin: x[{nx:.1},{xx:.1}] y[{ny:.1},{xy:.1}] (usable {MARGIN}..{})",
        p.piece_index,
        SHEET - MARGIN
      );
    }
    // No two parts on the sheet overlap.
    for i in 0..group.len() {
      if group[i].oversized {
        continue;
      }
      let ri = placed_ring(&items, &by_piece, group[i]);
      for j in (i + 1)..group.len() {
        if group[j].oversized {
          continue;
        }
        let rj = placed_ring(&items, &by_piece, group[j]);
        assert!(!overlap(&ri, &rj), "pieces {} and {} overlap", group[i].piece_index, group[j].piece_index);
      }
    }
  }

  // Every piece placed exactly once (nothing dropped).
  let mut seen: Vec<usize> = outcome.placed.iter().map(|p| p.piece_index).collect();
  seen.sort_unstable();
  seen.dedup();
  assert_eq!(seen.len(), pieces.len(), "every piece placed exactly once");
}
