//! P2-9 pin-and-re-nest correctness, on the real fixture.
//!
//! The invariants here ARE the feature: locked pieces must come back
//! byte-identical, and re-packed free pieces must never overlap a locked piece
//! (nor each other) and must stay inside the usable, margin-inset area. Overlap
//! is checked on the *rendered* nesting polygons (each piece's reserved outline
//! transformed by its placement) — so if the locked-obstacle conversion
//! (ext<->int transformation) were wrong, a free piece would land on the locked
//! piece's real footprint and this test would catch it.

use std::collections::HashMap;
use std::time::Duration;

use dxf::Drawing;

use twister_splitter::emit::Placed;
use twister_splitter::extract::{Sources, extract};
use twister_splitter::nest::{self, NestItem};
use twister_splitter::optimize::{nest_sheets, nest_sheets_pinned};

const SHEET: f64 = 400.0;
const KERF: f64 = 3.0;
const MARGIN: f64 = 10.0;
const SEED: u64 = 0x9E37_79B9_7F4A_7C15;

fn ring_of(item: &NestItem, xf: &twister_splitter::geom::Affine) -> Vec<[f64; 2]> {
  item.polygon.iter().map(|&[x, y]| { let (a, b) = xf.apply(x, y); [a, b] }).collect()
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

/// True if the two rings overlap: sample each ring's interior on a grid and test
/// for a point landing inside the other. With a >0 kerf separation, disjoint
/// reserved polygons never produce a hit; a gross overlap (e.g. a mis-placed
/// locked obstacle) always does.
fn rings_overlap(a: &[[f64; 2]], b: &[[f64; 2]]) -> bool {
  let (anx, any, axx, axy) = bbox(a);
  let (bnx, bny, bxx, bxy) = bbox(b);
  // Fast reject on bounding boxes (with a hair of slack).
  if axx < bnx - 1e-9 || bxx < anx - 1e-9 || axy < bny - 1e-9 || bxy < any - 1e-9 {
    return false;
  }
  let sample_into = |src: &[[f64; 2]], dst: &[[f64; 2]]| -> bool {
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
  sample_into(a, b) || sample_into(b, a)
}

#[test]
fn overlap_detector_is_not_vacuous() {
  let a = vec![[0.0, 0.0], [40.0, 0.0], [40.0, 40.0], [0.0, 40.0]];
  let overlapping = vec![[20.0, 20.0], [60.0, 20.0], [60.0, 60.0], [20.0, 60.0]];
  let disjoint = vec![[100.0, 100.0], [140.0, 100.0], [140.0, 140.0], [100.0, 140.0]];
  assert!(rings_overlap(&a, &overlapping), "must detect a real overlap");
  assert!(!rings_overlap(&a, &disjoint), "must not flag disjoint rings");
}

#[test]
fn pinned_renest_preserves_locks_and_avoids_overlap() {
  let path = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/gengar-stacked.dxf");
  let drawing = Drawing::load_file(path).expect("load fixture");
  let (pieces, _d) = extract(&drawing, Sources::Both);
  let items = nest::build_items(&drawing, &pieces);
  // build_items numbers piece_index == item index.
  for (i, it) in items.iter().enumerate() {
    assert_eq!(it.piece_index, i);
  }
  let bboxes: Vec<_> = pieces.iter().map(|p| p.bbox).collect();

  // Baseline nest to get a real placement to lock some pieces from.
  let base = nest_sheets(
    &items, &bboxes, SHEET, SHEET, KERF, MARGIN, SEED,
    Duration::from_millis(500), Duration::from_millis(120), None, |_| {},
  );

  // Lock the first few non-oversized placements.
  let fixed: Vec<Placed> = base
    .placed
    .iter()
    .filter(|p| !p.oversized)
    .take(4)
    .cloned()
    .map(|mut p| { p.locked = true; p })
    .collect();
  assert!(fixed.len() >= 2, "need a couple of pieces to lock");
  let locked_pieces: HashMap<usize, Placed> = fixed.iter().map(|p| (p.piece_index, p.clone())).collect();

  // Re-nest around the pins.
  let out = nest_sheets_pinned(
    &items, &bboxes, &fixed, SHEET, SHEET, KERF, MARGIN, None, |_| {},
  );

  // (1) Every piece placed exactly once.
  let mut seen: Vec<usize> = out.placed.iter().map(|p| p.piece_index).collect();
  seen.sort_unstable();
  seen.dedup();
  assert_eq!(seen.len(), pieces.len(), "every piece placed exactly once");

  // (2) Locked pieces are byte-identical (transform, sheet, flags).
  for locked in &fixed {
    let got = out.placed.iter().find(|p| p.piece_index == locked.piece_index).expect("locked present");
    let t = &got.transform;
    let l = &locked.transform;
    assert_eq!(got.sheet, locked.sheet, "locked piece changed sheet");
    assert!(got.locked, "locked flag lost");
    assert_eq!((t.m00, t.m01, t.m10, t.m11, t.tx, t.ty), (l.m00, l.m01, l.m10, l.m11, l.tx, l.ty), "locked transform changed");
  }

  // (3) No two pieces on a sheet overlap; free pieces stay inside the margin.
  let mut by_sheet: HashMap<usize, Vec<&Placed>> = HashMap::new();
  for p in &out.placed {
    by_sheet.entry(p.sheet).or_default().push(p);
  }
  for group in by_sheet.values() {
    // Bounds: every free (non-oversized) piece within [margin, size-margin].
    for p in group {
      if p.oversized || locked_pieces.contains_key(&p.piece_index) {
        continue;
      }
      let ring = ring_of(&items[p.piece_index], &p.transform);
      let (nx, ny, xx, xy) = bbox(&ring);
      assert!(
        nx >= MARGIN - 0.5 && ny >= MARGIN - 0.5 && xx <= SHEET - MARGIN + 0.5 && xy <= SHEET - MARGIN + 0.5,
        "free piece {} left the usable area: x[{nx:.1},{xx:.1}] y[{ny:.1},{xy:.1}]",
        p.piece_index
      );
    }
    // Overlap: all pairs on the sheet (skip oversized, which own isolated sheets).
    for i in 0..group.len() {
      if group[i].oversized {
        continue;
      }
      let ri = ring_of(&items[group[i].piece_index], &group[i].transform);
      for j in (i + 1)..group.len() {
        if group[j].oversized {
          continue;
        }
        let rj = ring_of(&items[group[j].piece_index], &group[j].transform);
        assert!(
          !rings_overlap(&ri, &rj),
          "pieces {} and {} overlap on their sheet",
          group[i].piece_index, group[j].piece_index
        );
      }
    }
  }
}
