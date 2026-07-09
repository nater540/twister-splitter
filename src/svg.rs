//! SVG export of packed sheets.
//!
//! The outlines are already flattened to polygons by [`flatten`], so a sheet is
//! rendered by transforming each piece's rings into place and emitting them as
//! `<polyline>`s. DXF is y-up and SVG is y-down, so the y axis is flipped within
//! each sheet's box. Coordinates are in millimetres (the `viewBox` and the
//! `width`/`height` are set in `mm`), matching the DXF units.
//!
//! [`flatten`]: crate::flatten

use std::io::Write;
use std::path::{Path, PathBuf};

use dxf::Drawing;

use crate::emit::Placed;
use crate::extract::Piece;

/// Gap (mm) between stacked sheets in a combined document.
const COMBINE_GAP: f64 = 20.0;

/// Outline rings of one sheet, transformed into sheet coordinates (DXF y-up).
fn sheet_rings(pieces: &[Piece], drawing: &Drawing, placed: &[Placed], sheet: usize) -> Vec<Vec<[f64; 2]>> {
  let mut out = Vec::new();
  for pl in placed.iter().filter(|p| p.sheet == sheet) {
    for ring in pieces[pl.piece_index].rings(drawing) {
      if ring.len() < 2 {
        continue;
      }
      out.push(
        ring
          .iter()
          .map(|&[x, y]| {
            let (tx, ty) = pl.transform.apply(x, y);
            [tx, ty]
          })
          .collect(),
      );
    }
  }
  out
}

/// Append one sheet's border + rings to `buf`, flipping y within the sheet box
/// and offsetting the whole sheet down by `y_off` (for stacking in a combined
/// document). `h` is the sheet height used for the y flip.
fn push_sheet(buf: &mut String, rings: &[Vec<[f64; 2]>], w: f64, h: f64, y_off: f64) {
  buf.push_str(&format!(
    "  <rect x=\"0\" y=\"{:.3}\" width=\"{:.3}\" height=\"{:.3}\" fill=\"none\" stroke=\"#888\" stroke-width=\"0.3\"/>\n",
    y_off, w, h
  ));
  for ring in rings {
    buf.push_str("  <polyline points=\"");
    for (i, &[x, y]) in ring.iter().enumerate() {
      if i > 0 {
        buf.push(' ');
      }
      // y-up (DXF) -> y-down (SVG) within this sheet's box, then offset.
      buf.push_str(&format!("{:.3},{:.3}", x, y_off + (h - y)));
    }
    // Close the ring back to its first point.
    if let Some(&[x0, y0]) = ring.first() {
      buf.push_str(&format!(" {:.3},{:.3}", x0, y_off + (h - y0)));
    }
    buf.push_str("\" fill=\"none\" stroke=\"#000\" stroke-width=\"0.2\"/>\n");
  }
}

fn svg_doc(width: f64, height: f64, body: &str) -> String {
  format!(
    "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
     <svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{width:.3}mm\" height=\"{height:.3}mm\" \
     viewBox=\"0 0 {width:.3} {height:.3}\">\n{body}</svg>\n"
  )
}

/// Number of sheets referenced by `placed`.
fn sheet_count(placed: &[Placed]) -> usize {
  placed.iter().map(|p| p.sheet).max().map_or(0, |m| m + 1)
}

/// Render each sheet as its own SVG string, in sheet order (no filesystem I/O).
pub fn sheet_svgs(source: &Drawing, pieces: &[Piece], placed: &[Placed], sheet_w: f64, sheet_h: f64) -> Vec<String> {
  (0..sheet_count(placed))
    .map(|s| {
      let rings = sheet_rings(pieces, source, placed, s);
      let mut body = String::new();
      push_sheet(&mut body, &rings, sheet_w, sheet_h, 0.0);
      svg_doc(sheet_w, sheet_h, &body)
    })
    .collect()
}

/// Render all sheets stacked vertically into a single SVG string.
pub fn combined_svg(source: &Drawing, pieces: &[Piece], placed: &[Placed], sheet_w: f64, sheet_h: f64) -> String {
  let n = sheet_count(placed);
  let mut body = String::new();
  for s in 0..n {
    let rings = sheet_rings(pieces, source, placed, s);
    push_sheet(&mut body, &rings, sheet_w, sheet_h, s as f64 * (sheet_h + COMBINE_GAP));
  }
  let total_h = if n == 0 { sheet_h } else { n as f64 * sheet_h + (n.saturating_sub(1)) as f64 * COMBINE_GAP };
  svg_doc(sheet_w, total_h, &body)
}

/// Write packed sheets as SVG into `out_dir`. When `combined`, writes a single
/// `{stem}.svg` with all sheets stacked; otherwise one `{stem}_sheet_{NN}.svg`
/// per sheet. Returns the paths written.
#[allow(clippy::too_many_arguments)]
pub fn write_svg(
  source: &Drawing,
  pieces: &[Piece],
  placed: &[Placed],
  out_dir: &Path,
  stem: &str,
  sheet_w: f64,
  sheet_h: f64,
  combined: bool,
) -> std::io::Result<Vec<PathBuf>> {
  std::fs::create_dir_all(out_dir)?;
  let mut files = Vec::new();
  if combined {
    let path = out_dir.join(format!("{stem}.svg"));
    let mut f = std::fs::File::create(&path)?;
    f.write_all(combined_svg(source, pieces, placed, sheet_w, sheet_h).as_bytes())?;
    files.push(path);
  } else {
    for (sheet, doc) in sheet_svgs(source, pieces, placed, sheet_w, sheet_h).into_iter().enumerate() {
      let path = out_dir.join(format!("{stem}_sheet_{sheet:02}.svg"));
      let mut f = std::fs::File::create(&path)?;
      f.write_all(doc.as_bytes())?;
      files.push(path);
    }
  }
  Ok(files)
}
