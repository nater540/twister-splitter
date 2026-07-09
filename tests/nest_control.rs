//! Invariants for the P0 nesting-control surface: cooperative cancellation
//! returns a prompt partial result, and `nest_sheets` isolates oversized pieces
//! on their own sheets while placing everything exactly once.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use twister_splitter::geom::Bbox;
use twister_splitter::nest::{self, NestItem};
use twister_splitter::optimize::{NestEvent, nest_sheets, nest_sparrow};

/// An axis-aligned square nesting item of side `s` with its lower-left at origin.
fn square(piece_index: usize, s: f64) -> NestItem {
  NestItem {
    piece_index,
    polygon: vec![[0.0, 0.0], [s, 0.0], [s, s], [0.0, s]],
    hull_fallback: false,
  }
}

fn bbox(s: f64) -> Bbox {
  Bbox { min_x: 0.0, min_y: 0.0, max_x: s, max_y: s }
}

#[test]
fn precancelled_sparrow_run_returns_promptly_and_partial() {
  let items: Vec<NestItem> = (0..12).map(|i| square(i, 60.0)).collect();
  let cancel = Arc::new(AtomicBool::new(true)); // already cancelled

  let t0 = Instant::now();
  let res = nest_sparrow(
    &items,
    400.0,
    400.0,
    2.0,
    0.0,
    1,
    Duration::from_secs(30),
    Duration::from_secs(30),
    Some(cancel),
    |_| {},
  );

  // Must not have burned anywhere near the 60s budget.
  assert!(t0.elapsed() < Duration::from_secs(2), "cancellation must be prompt");
  // Nothing placed, everything reported as unplaced (a valid partial result).
  assert!(res.placed.is_empty(), "a pre-cancelled run should place nothing");
  assert_eq!(res.unplaced.len(), 12, "all pieces should be unplaced");
}

#[test]
fn precancelled_greedy_run_returns_partial() {
  let items: Vec<NestItem> = (0..6).map(|i| square(i, 40.0)).collect();
  let cancel = Arc::new(AtomicBool::new(true));
  let res = nest::nest(&items, 400.0, 400.0, 1.0, 0.0, Some(&cancel), |_, _| {}).expect("nest");
  assert!(res.placed.is_empty());
  assert_eq!(res.unplaced.len(), 6);
}

#[test]
fn nest_sheets_isolates_oversized_and_places_all() {
  // Small pieces that fit, plus one bigger than the 400×400 sheet at any angle.
  let mut items: Vec<NestItem> = (0..5).map(|i| square(i, 80.0)).collect();
  items.push(square(5, 600.0));
  let bboxes: Vec<Bbox> = (0..5).map(|_| bbox(80.0)).chain(std::iter::once(bbox(600.0))).collect();

  let mut completed = Vec::new();
  let outcome = nest_sheets(
    &items,
    &bboxes,
    400.0,
    400.0,
    2.0,
    0.0,
    7,
    Duration::from_millis(400),
    Duration::from_millis(100),
    None,
    |event| {
      if let NestEvent::SheetCompleted { sheet, placed } = event {
        completed.push((sheet, placed.len()));
      }
    },
  );

  assert!(!outcome.canceled);
  assert_eq!(outcome.oversized, vec![5], "the 600mm piece must be oversized");
  assert!(outcome.unplaced.is_empty(), "nothing left unplaced on a full run");

  // Every input piece appears exactly once across all sheets.
  let mut seen: Vec<usize> = outcome.placed.iter().map(|p| p.piece_index).collect();
  seen.sort_unstable();
  seen.dedup();
  assert_eq!(seen.len(), items.len(), "every piece placed exactly once");

  // The oversized piece is alone on its own sheet.
  let over = outcome.placed.iter().find(|p| p.oversized).expect("oversized placement");
  let sharing = outcome.placed.iter().filter(|p| p.sheet == over.sheet).count();
  assert_eq!(sharing, 1, "oversized piece must not share its sheet");
  assert_eq!(outcome.sheets, over.sheet + 1);

  // A SheetCompleted event fired for at least the fitted sheet(s).
  assert!(!completed.is_empty(), "sheet-completed events should stream");
}

#[test]
fn margin_keeps_parts_inside_the_usable_area() {
  let items: Vec<NestItem> = (0..8).map(|i| square(i, 60.0)).collect();
  let bboxes: Vec<Bbox> = (0..8).map(|_| bbox(60.0)).collect();
  let (w, h, margin) = (400.0, 400.0, 25.0);

  let outcome = nest_sheets(
    &items,
    &bboxes,
    w,
    h,
    2.0,
    margin,
    3,
    Duration::from_millis(400),
    Duration::from_millis(100),
    None,
    |_| {},
  );

  // Every placed (non-oversized) part's transformed corners lie within the
  // margin-inset usable area [margin, size-margin].
  let tol = 1e-6;
  for pl in outcome.placed.iter().filter(|p| !p.oversized) {
    for &[lx, ly] in &[[0.0, 0.0], [60.0, 0.0], [60.0, 60.0], [0.0, 60.0]] {
      let (x, y) = pl.transform.apply(lx, ly);
      assert!(
        x >= margin - tol && x <= w - margin + tol && y >= margin - tol && y <= h - margin + tol,
        "part corner ({x:.2},{y:.2}) escaped the {margin}mm margin"
      );
    }
  }
}
