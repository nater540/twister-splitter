//! Write one output DXF per packed sheet, transforming each piece into place.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use dxf::entities::{Entity, EntityType, LwPolyline};
use dxf::{Drawing, LwPolylineVertex};

use crate::diag::Diagnostic;
use crate::extract::{Piece, PieceKind};
use crate::flatten;
use crate::geom::Affine;

/// Apply a rotation+translation to an entity's geometry, in place.
fn transform_entity(entity: &mut Entity, xf: &Affine) {
  match &mut entity.specific {
    EntityType::Spline(s) => {
      for p in &mut s.control_points {
        let (x, y) = xf.apply(p.x, p.y);
        p.x = x;
        p.y = y;
      }
      for p in &mut s.fit_points {
        let (x, y) = xf.apply(p.x, p.y);
        p.x = x;
        p.y = y;
      }
      // Tangents are directions, not positions: rotate only.
      let (tx, ty) = xf.apply_vec(s.start_tangent.x, s.start_tangent.y);
      s.start_tangent.x = tx;
      s.start_tangent.y = ty;
      let (ex, ey) = xf.apply_vec(s.end_tangent.x, s.end_tangent.y);
      s.end_tangent.x = ex;
      s.end_tangent.y = ey;
    }
    EntityType::LwPolyline(p) => {
      for v in &mut p.vertices {
        let (x, y) = xf.apply(v.x, v.y);
        v.x = x;
        v.y = y;
      }
    }
    EntityType::Polyline(p) => {
      for v in p.vertices_mut() {
        let (x, y) = xf.apply(v.location.x, v.location.y);
        v.location.x = x;
        v.location.y = y;
      }
    }
    EntityType::Line(l) => {
      let (x1, y1) = xf.apply(l.p1.x, l.p1.y);
      l.p1.x = x1;
      l.p1.y = y1;
      let (x2, y2) = xf.apply(l.p2.x, l.p2.y);
      l.p2.x = x2;
      l.p2.y = y2;
    }
    EntityType::Circle(c) => {
      let (x, y) = xf.apply(c.center.x, c.center.y);
      c.center.x = x;
      c.center.y = y;
      // radius is rotation/translation-invariant.
    }
    EntityType::Arc(a) => {
      let (x, y) = xf.apply(a.center.x, a.center.y);
      a.center.x = x;
      a.center.y = y;
      // Sweep angles are measured CCW from +X. Under a pure rotation they shift
      // by the rotation angle. Under a reflection (det < 0) the CCW sweep
      // reverses: with `2β = atan2(m10, m00) = xf.rotation()` (the mirror axis
      // angle, doubled), the arc [s, e] maps to the CCW arc [2β - e, 2β - s].
      let deg = xf.rotation().to_degrees();
      if xf.determinant() < 0.0 {
        let (s, e) = (a.start_angle, a.end_angle);
        a.start_angle = deg - e;
        a.end_angle = deg - s;
      } else {
        a.start_angle += deg;
        a.end_angle += deg;
      }
    }
    EntityType::Ellipse(e) => {
      let (cx, cy) = xf.apply(e.center.x, e.center.y);
      e.center.x = cx;
      e.center.y = cy;
      // major_axis is a direction relative to the centre: transform it by the
      // linear part (rotation or reflection).
      let (mx, my) = xf.apply_vec(e.major_axis.x, e.major_axis.y);
      e.major_axis.x = mx;
      e.major_axis.y = my;
      // The DXF minor axis is a 90°-CCW rotation of the major axis scaled by the
      // ratio, so a reflection flips the parametric orientation: parameter `t`
      // maps to `-t`, turning the CCW arc [s, e] into [-e, -s]. (A pure rotation
      // leaves the parameters unchanged — they are relative to the major axis.)
      if xf.determinant() < 0.0 {
        let (s, en) = (e.start_parameter, e.end_parameter);
        e.start_parameter = -en;
        e.end_parameter = -s;
      }
    }
    _ => {}
  }
}

/// A piece assigned to a sheet with the transform that maps its geometry from
/// its local frame onto that sheet. Both packers (rectangle and nesting) emit
/// this common form, so `emit` is agnostic to how placement was computed.
#[derive(Clone, Debug)]
pub struct Placed {
  pub piece_index: usize,
  pub sheet: usize,
  pub transform: Affine,
  pub oversized: bool,
  /// User-pinned: a future pin-and-re-nest path treats locked placements as
  /// fixed. Set/read by the UI; the automatic nesters ignore it today.
  pub locked: bool,
}

impl Placed {
  /// Move this placement to a different sheet (S-5 "Move to Sheet ▸").
  pub fn move_to_sheet(&mut self, sheet: usize) {
    self.sheet = sheet;
  }

  /// Rotate the piece in place by `quarter_turns` × 90° about the centre of its
  /// current footprint (S-5 "Rotate 90° CW/CCW"). `bbox` is the piece's local
  /// bounding box. Stays a pure rotation+translation, so `emit` renders it
  /// faithfully. Note: a manual rotate is not re-checked for sheet bounds or
  /// overlap — that is the caller's responsibility (or re-nest).
  pub fn rotate(&mut self, bbox: &crate::geom::Bbox, quarter_turns: i32) {
    let theta = quarter_turns as f64 * std::f64::consts::FRAC_PI_2;
    // Centre of the piece's current footprint, in sheet coordinates.
    let (bx, by) = ((bbox.min_x + bbox.max_x) * 0.5, (bbox.min_y + bbox.max_y) * 0.5);
    let (cx, cy) = self.transform.apply(bx, by);
    let rot = Affine::rotation_about(cx, cy, theta);
    self.transform = rot.compose(&self.transform);
  }

  /// Centre of the piece's current footprint, in sheet coordinates.
  fn footprint_center(&self, bbox: &crate::geom::Bbox) -> (f64, f64) {
    let (bx, by) = ((bbox.min_x + bbox.max_x) * 0.5, (bbox.min_y + bbox.max_y) * 0.5);
    self.transform.apply(bx, by)
  }

  /// Mirror the piece horizontally (left↔right) about its footprint centre
  /// (S-5 "Flip Horizontal"). Introduces a reflection (determinant `-1`), which
  /// `emit` renders faithfully (arc sweeps and INSERT scale are handled).
  pub fn flip_h(&mut self, bbox: &crate::geom::Bbox) {
    let (cx, _cy) = self.footprint_center(bbox);
    self.transform = Affine::reflect_x(cx).compose(&self.transform);
  }

  /// Mirror the piece vertically (top↔bottom) about its footprint centre
  /// (S-5 "Flip Vertical").
  pub fn flip_v(&mut self, bbox: &crate::geom::Bbox) {
    let (_cx, cy) = self.footprint_center(bbox);
    self.transform = Affine::reflect_y(cy).compose(&self.transform);
  }
}

/// Result of writing all sheets.
pub struct EmitReport {
  pub files: Vec<PathBuf>,
  /// Notices produced while writing (e.g. the kerf-compensation polyline
  /// approximation), for the caller to surface.
  pub diagnostics: Vec<Diagnostic>,
}

/// Build the output `Drawing` for a single sheet: every piece assigned to
/// `sheet` transformed into place, block definitions copied in as needed.
///
/// When `kerf_comp > 0`, each piece's outline is kerf-compensated (outer grown /
/// holes shrunk by `kerf_comp / 2`) and emitted as closed polylines instead of
/// its original entities — a deliberate, opt-in fidelity trade (curved outlines
/// become polylines) reported via [`EmitReport::diagnostics`].
fn build_one_sheet(source: &Drawing, pieces: &[Piece], placed: &[Placed], sheet: usize, kerf_comp: f64) -> Drawing {
  let mut out = Drawing::new();
  // Inherit the source's ACAD version. `Drawing::new()` defaults to R12, whose
  // writer silently drops SPLINE (an R13+ entity) — so without this every spline
  // outline would vanish from the output.
  out.header.version = source.header.version;
  let mut added_blocks: HashSet<String> = HashSet::new();

  for placement in placed.iter().filter(|p| p.sheet == sheet) {
    let piece = &pieces[placement.piece_index];
    let xf = &placement.transform;

    // Kerf compensation: replace the piece's outline with offset closed
    // polylines (outer +kerf/2, holes -kerf/2), transformed into place.
    if kerf_comp > 0.0 {
      for ring in flatten::compensate_rings(&piece.rings(source), kerf_comp * 0.5) {
        if ring.len() < 2 {
          continue;
        }
        let mut lw = LwPolyline::default();
        for p in &ring {
          let (x, y) = xf.apply(p[0], p[1]);
          lw.vertices.push(LwPolylineVertex { x, y, ..Default::default() });
        }
        lw.set_is_closed(true);
        out.add_entity(Entity::new(EntityType::LwPolyline(lw)));
      }
      continue;
    }

    match &piece.kind {
      PieceKind::Loose(entities) => {
        for entity in entities {
          let mut e = entity.clone();
          transform_entity(&mut e, xf);
          out.add_entity(e);
        }
      }
      PieceKind::Insert { insert, block_name } => {
        // Locate the referenced block once, then reuse it below.
        let block = source.blocks().find(|b| &b.name == block_name);
        // Copy the block definition into this sheet on first use.
        if added_blocks.insert(block_name.clone())
          && let Some(b) = block {
            out.add_block(b.clone());
          }
        let mut e = (**insert).clone();
        if let EntityType::Insert(ins) = &mut e.specific {
          // Position the block reference so a block vertex `v` renders at
          // `xf(v)`. DXF maps a block point as `v -> L + Rot(rot)·Scale·(v -
          // base)`, so with `L = xf(base)` we need `Rot(rot)·Scale = xf`'s linear
          // part. Pure rotation: `rot = xf.rotation()`, `Scale = (1, 1)`. A
          // reflection (det < 0) factors as `Rot(xf.rotation())·diag(1, -1)`, so
          // we mirror the block via `y_scale_factor = -1` — the DXF consumer then
          // reflects the whole block (arcs included) correctly.
          let base = block
            .map(|b| (b.base_point.x, b.base_point.y))
            .unwrap_or((0.0, 0.0));
          let (lx, ly) = xf.apply(base.0, base.1);
          ins.location.x = lx;
          ins.location.y = ly;
          ins.rotation = xf.rotation().to_degrees();
          if xf.determinant() < 0.0 {
            ins.x_scale_factor = 1.0;
            ins.y_scale_factor = -1.0;
          }
        }
        out.add_entity(e);
      }
    }
  }

  out.normalize();
  out
}

/// Build one output `Drawing` per sheet, in sheet order, without touching the
/// filesystem. Callers that want files use [`emit`]; callers that want the
/// drawings in memory (single-file export, preview, SVG conversion) use this.
/// `kerf_comp` (0 = off) enables kerf compensation — see [`build_one_sheet`].
pub fn build_sheet_drawings(source: &Drawing, pieces: &[Piece], placed: &[Placed], kerf_comp: f64) -> Vec<Drawing> {
  let sheet_count = placed.iter().map(|p| p.sheet).max().map_or(0, |m| m + 1);
  (0..sheet_count)
    .map(|sheet| build_one_sheet(source, pieces, placed, sheet, kerf_comp))
    .collect()
}

/// Write every sheet described by `placed` into `out_dir` as
/// `{file_stem}_sheet_{NN}.dxf`.
///
/// `source` is the original drawing, used to copy block definitions that INSERT
/// pieces depend on. `kerf_comp` (0 = off, in DXF units) offsets each cut
/// outline outward by half its value so finished parts are dimensionally
/// correct; when on, outlines are emitted as polylines and a diagnostic is
/// returned. A thin wrapper over [`build_sheet_drawings`].
pub fn emit(
  source: &Drawing,
  pieces: &[Piece],
  placed: &[Placed],
  out_dir: &Path,
  file_stem: &str,
  kerf_comp: f64,
) -> std::io::Result<EmitReport> {
  emit_with(source, pieces, placed, out_dir, file_stem, kerf_comp)
}

/// Alias of [`emit`] (identical signature). Kept so callers that spell it
/// `emit_with` also resolve.
pub fn emit_with(
  source: &Drawing,
  pieces: &[Piece],
  placed: &[Placed],
  out_dir: &Path,
  file_stem: &str,
  kerf_comp: f64,
) -> std::io::Result<EmitReport> {
  std::fs::create_dir_all(out_dir)?;
  let mut files = Vec::new();
  for (sheet, drawing) in build_sheet_drawings(source, pieces, placed, kerf_comp).into_iter().enumerate() {
    let path = out_dir.join(format!("{file_stem}_sheet_{sheet:02}.dxf"));
    drawing
      .save_file(&path)
      .map_err(|e| std::io::Error::other(e.to_string()))?;
    files.push(path);
  }
  let mut diagnostics = Vec::new();
  if kerf_comp > 0.0 {
    let n = placed.len();
    diagnostics.push(Diagnostic::info(format!(
      "kerf compensation on ({kerf_comp} units): {n} piece outline(s) offset and emitted as polylines (curved outlines approximated)"
    )));
  }
  Ok(EmitReport { files, diagnostics })
}
