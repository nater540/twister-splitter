//! 2D nesting via the MaxRects algorithm (best-short-side-fit), across as many
//! fixed-size sheets as needed, with optional 90° rotation and a kerf gap.

use crate::emit::Placed;
use crate::geom::{Affine, Bbox};

/// Where a piece ended up after packing.
pub struct Placement {
  /// Index into the input `pieces` slice.
  pub piece_index: usize,
  /// Which sheet (0-based) the piece landed on.
  pub sheet: usize,
  /// Min corner of the piece's *unpadded* bbox on the sheet.
  pub x: f64,
  pub y: f64,
  /// Rotation applied, in radians (0 or π/2).
  pub theta: f64,
  /// Set when the piece is larger than the sheet and could not be nested.
  pub oversized: bool,
}

impl Placement {
  /// Convert to the unified [`Placed`] form emit consumes, using the piece's
  /// local bounding box to resolve the rotate-then-drop-to-corner transform.
  pub fn to_placed(&self, bbox: &Bbox) -> Placed {
    Placed {
      piece_index: self.piece_index,
      sheet: self.sheet,
      transform: Affine::place(bbox, self.theta, self.x, self.y),
      oversized: self.oversized,
      locked: false,
    }
  }
}

pub struct PackConfig {
  pub sheet_w: f64,
  pub sheet_h: f64,
  /// Spacing added between neighbouring parts (and at the far edges).
  pub kerf: f64,
  /// Allow rotating a piece 90° if it packs better (or only then fits).
  pub allow_rotation: bool,
}

/// A free rectangle available for placement within one sheet.
#[derive(Clone, Copy)]
struct FreeRect {
  x: f64,
  y: f64,
  w: f64,
  h: f64,
}

struct Sheet {
  free: Vec<FreeRect>,
}

impl Sheet {
  fn new(w: f64, h: f64) -> Self {
    Sheet {
      free: vec![FreeRect { x: 0.0, y: 0.0, w, h }],
    }
  }

  /// Best-short-side-fit score for placing a `w`×`h` rect, plus the chosen
  /// free-rect origin. Lower score is better.
  fn find_position(&self, w: f64, h: f64) -> Option<(f64, f64, f64)> {
    let mut best: Option<(f64, f64, f64)> = None; // (score, x, y)
    for fr in &self.free {
      if w <= fr.w + EPS && h <= fr.h + EPS {
        let leftover_h = (fr.w - w).abs();
        let leftover_v = (fr.h - h).abs();
        let short = leftover_h.min(leftover_v);
        if best.is_none_or(|(s, _, _)| short < s) {
          best = Some((short, fr.x, fr.y));
        }
      }
    }
    best
  }

  /// Place a `w`×`h` rect at `(x, y)` and repair the free-rect list.
  fn place(&mut self, x: f64, y: f64, w: f64, h: f64) {
    let placed = FreeRect { x, y, w, h };
    let mut next = Vec::new();
    for fr in std::mem::take(&mut self.free) {
      if !overlaps(&fr, &placed) {
        next.push(fr);
        continue;
      }
      // Split fr around placed into up to four guillotine-free stubs.
      if placed.x > fr.x + EPS {
        next.push(FreeRect { x: fr.x, y: fr.y, w: placed.x - fr.x, h: fr.h });
      }
      if placed.x + w < fr.x + fr.w - EPS {
        next.push(FreeRect {
          x: placed.x + w,
          y: fr.y,
          w: fr.x + fr.w - (placed.x + w),
          h: fr.h,
        });
      }
      if placed.y > fr.y + EPS {
        next.push(FreeRect { x: fr.x, y: fr.y, w: fr.w, h: placed.y - fr.y });
      }
      if placed.y + h < fr.y + fr.h - EPS {
        next.push(FreeRect {
          x: fr.x,
          y: placed.y + h,
          w: fr.w,
          h: fr.y + fr.h - (placed.y + h),
        });
      }
    }
    // Drop free rects fully contained in another (keeps the list small).
    prune(&mut next);
    self.free = next;
  }
}

const EPS: f64 = 1e-6;

fn overlaps(a: &FreeRect, b: &FreeRect) -> bool {
  a.x < b.x + b.w - EPS
    && b.x < a.x + a.w - EPS
    && a.y < b.y + b.h - EPS
    && b.y < a.y + a.h - EPS
}

fn contains(outer: &FreeRect, inner: &FreeRect) -> bool {
  inner.x >= outer.x - EPS
    && inner.y >= outer.y - EPS
    && inner.x + inner.w <= outer.x + outer.w + EPS
    && inner.y + inner.h <= outer.y + outer.h + EPS
}

fn prune(rects: &mut Vec<FreeRect>) {
  let mut i = 0;
  while i < rects.len() {
    let mut removed = false;
    let mut j = 0;
    while j < rects.len() {
      if i != j && contains(&rects[j], &rects[i]) {
        rects.remove(i);
        removed = true;
        break;
      }
      j += 1;
    }
    if !removed {
      i += 1;
    }
  }
}

/// Pack `pieces` (given by their bounding boxes) into sheets.
///
/// Returns one `Placement` per piece, in the same order as `bboxes`.
pub fn pack(bboxes: &[Bbox], cfg: &PackConfig) -> Vec<Placement> {
  let half_pi = std::f64::consts::FRAC_PI_2;

  // Sort by descending longer side — classic first-fit-decreasing ordering —
  // but remember original indices so the caller can map back.
  let mut order: Vec<usize> = (0..bboxes.len()).collect();
  order.sort_by(|&a, &b| {
    let la = bboxes[a].width().max(bboxes[a].height());
    let lb = bboxes[b].width().max(bboxes[b].height());
    lb.partial_cmp(&la).unwrap_or(std::cmp::Ordering::Equal)
  });

  let mut sheets: Vec<Sheet> = Vec::new();
  let mut placements: Vec<Option<Placement>> = (0..bboxes.len()).map(|_| None).collect();

  for idx in order {
    let bb = &bboxes[idx];
    // Padded dimensions include the kerf gap on the top/right.
    let pw = bb.width() + cfg.kerf;
    let ph = bb.height() + cfg.kerf;

    let fits_normal = pw <= cfg.sheet_w + EPS && ph <= cfg.sheet_h + EPS;
    let fits_rot =
      cfg.allow_rotation && ph <= cfg.sheet_w + EPS && pw <= cfg.sheet_h + EPS;

    if !fits_normal && !fits_rot {
      // Too big for any sheet: give it its own sheet, flagged. Clear the free
      // list so no other piece gets nested on top of the oversized one.
      let sheet = sheets.len();
      let mut own = Sheet::new(cfg.sheet_w, cfg.sheet_h);
      own.free.clear();
      sheets.push(own);
      placements[idx] = Some(Placement {
        piece_index: idx,
        sheet,
        x: 0.0,
        y: 0.0,
        theta: 0.0,
        oversized: true,
      });
      continue;
    }

    // Try every existing sheet, picking the best (orientation, position).
    let mut chosen: Option<(usize, f64, f64, f64)> = None; // (sheet, x, y, theta)
    let mut best_score = f64::INFINITY;
    for (si, sheet) in sheets.iter().enumerate() {
      if fits_normal
        && let Some((score, x, y)) = sheet.find_position(pw, ph)
          && score < best_score {
            best_score = score;
            chosen = Some((si, x, y, 0.0));
          }
      if fits_rot
        && let Some((score, x, y)) = sheet.find_position(ph, pw)
          && score < best_score {
            best_score = score;
            chosen = Some((si, x, y, half_pi));
          }
    }

    // No room in any open sheet — start a new one.
    if chosen.is_none() {
      let si = sheets.len();
      sheets.push(Sheet::new(cfg.sheet_w, cfg.sheet_h));
      let (w, h, theta) = if fits_normal { (pw, ph, 0.0) } else { (ph, pw, half_pi) };
      let (_score, x, y) = sheets[si].find_position(w, h).expect("fresh sheet must fit");
      chosen = Some((si, x, y, theta));
    }

    let (si, x, y, theta) = chosen.unwrap();
    let (w, h) = if theta == 0.0 { (pw, ph) } else { (ph, pw) };
    sheets[si].place(x, y, w, h);
    placements[idx] = Some(Placement {
      piece_index: idx,
      sheet: si,
      x,
      y,
      theta,
      oversized: false,
    });
  }

  placements.into_iter().map(|p| p.unwrap()).collect()
}
