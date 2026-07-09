//! Write one output DXF per packed sheet, transforming each piece into place.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use dxf::entities::{Entity, EntityType, LwPolyline, Text};
use dxf::enums::{HorizontalTextJustification, VerticalTextJustification};
use dxf::tables::Layer;
use dxf::{Color, Drawing, LwPolylineVertex, Point};

use crate::diag::Diagnostic;
use crate::extract::{Piece, PieceKind, PieceSource};
use crate::flatten;
use crate::geom::{Affine, Bbox};

/// Emit-time options. Grown as a struct (rather than positional args) because the
/// output pipeline keeps gaining knobs — kerf compensation, engraved numbers, and
/// more to come. `Default` is "faithful cut geometry, nothing extra".
#[derive(Clone, Copy, Debug, Default)]
pub struct EmitOptions {
  /// Kerf compensation offset (DXF units, 0 = off): each cut outline is offset
  /// outward by `kerf_comp / 2` and emitted as closed polylines (curved outlines
  /// approximated) so finished parts are dimensionally correct.
  pub kerf_comp: f64,
  /// Engrave each piece's assembly number as a TEXT entity on the `Engrave`
  /// layer (R3-2), so a stacked/layered model can be re-stacked in order.
  pub engrave_numbers: bool,
  /// Micro-tab (holding-bridge) length in DXF units (0 = off). When > 0 with
  /// `tab_count > 0`, each cut outline is broken into open polyline segments
  /// separated by `tab_count` uncut gaps of this length, so fully-cut parts stay
  /// attached to the sheet (R3-3). Forces the polyline-approximation emit path
  /// (curved outlines are flattened), reported via [`EmitReport::diagnostics`].
  pub tab_width: f64,
  /// Number of micro-tabs distributed evenly around each closed outline ring.
  pub tab_count: usize,
  /// Split each loose part's outer contour onto the `Cut-Outer` layer and its
  /// holes onto `Cut-Inner` (R1-1b), so the two can carry different laser
  /// operations/speeds. Loose parts only — block (INSERT) pieces stay on `Cut`.
  pub split_cut_layers: bool,
}

/// Conventional layer name for through-cut geometry (R1-1a). Laser software
/// (LightBurn, etc.) keys its cut operation off the layer/colour, so putting
/// every emitted entity on one `Cut` layer with a distinct ACI colour makes the
/// output import as a single cut op with no manual layer assignment.
const CUT_LAYER: &str = "Cut";
/// ACI 1 (red): the near-universal convention for a through-cut.
const CUT_COLOR: u8 = 1;
/// Layer for the outer contour when the outer/inner split is on (R1-1b). Shares
/// the cut colour with `Cut`; only the layer name distinguishes it.
const CUT_OUTER_LAYER: &str = "Cut-Outer";
/// Layer for interior contours (holes) when the split is on (R1-1b).
const CUT_INNER_LAYER: &str = "Cut-Inner";
/// ACI 3 (green): a distinct colour so inner cuts read differently from outer.
const CUT_INNER_COLOR: u8 = 3;
/// Layer for engraved assembly numbers (R3-2), kept off the cut layer so laser
/// software runs it as a separate (engrave/score) operation.
const ENGRAVE_LAYER: &str = "Engrave";
/// ACI 5 (blue): a conventional non-cut / engrave colour.
const ENGRAVE_COLOR: u8 = 5;

/// Put an entity on a given layer/colour and add it to `out`.
fn add_on_layer(out: &mut Drawing, mut e: Entity, layer: &str, color: u8) {
  e.common.layer = layer.to_string();
  e.common.color = Color::from_index(color);
  out.add_entity(e);
}

/// Put an entity on the shared cut layer/colour and add it to `out`. Used where
/// the outer/inner split does not apply (INSERT geometry, or split off).
fn add_cut(out: &mut Drawing, e: Entity) {
  add_on_layer(out, e, CUT_LAYER, CUT_COLOR);
}

/// The (layer, colour) a cut entity belongs on. With the outer/inner split on
/// (R1-1b) and a splittable *loose part*, the outer contour goes to `Cut-Outer`
/// and interior contours (holes) to `Cut-Inner`; otherwise everything is `Cut`.
/// Blocks are never split (a single INSERT can't be per-ring layered).
fn cut_role(opts: EmitOptions, is_part: bool, is_outer: bool) -> (&'static str, u8) {
  if opts.split_cut_layers && is_part {
    if is_outer {
      (CUT_OUTER_LAYER, CUT_COLOR)
    } else {
      (CUT_INNER_LAYER, CUT_INNER_COLOR)
    }
  } else {
    (CUT_LAYER, CUT_COLOR)
  }
}

/// Register the `Cut` layer (with its ACI colour) in `out`'s layer table, so a
/// consumer that reads cut settings off the *layer* — not just the entity colour
/// — also sees red. `add_entity` auto-creates a layer by name but with the
/// default colour, so we add it explicitly first.
fn register_cut_layer(out: &mut Drawing) {
  out.add_layer(Layer {
    name: CUT_LAYER.to_string(),
    color: Color::from_index(CUT_COLOR),
    ..Default::default()
  });
}

/// Register the `Cut-Outer` / `Cut-Inner` layers for the outer/inner split (R1-1b).
fn register_split_layers(out: &mut Drawing) {
  out.add_layer(Layer {
    name: CUT_OUTER_LAYER.to_string(),
    color: Color::from_index(CUT_COLOR),
    ..Default::default()
  });
  out.add_layer(Layer {
    name: CUT_INNER_LAYER.to_string(),
    color: Color::from_index(CUT_INNER_COLOR),
    ..Default::default()
  });
}

/// Register the `Engrave` layer (ACI 5) for assembly-number labels (R3-2).
fn register_engrave_layer(out: &mut Drawing) {
  out.add_layer(Layer {
    name: ENGRAVE_LAYER.to_string(),
    color: Color::from_index(ENGRAVE_COLOR),
    ..Default::default()
  });
}

/// A centred TEXT label of `number` at the piece's footprint centre, on the
/// engrave layer (R3-2). `bbox` is the piece's local bbox; `xf` maps it onto the
/// sheet. Height scales with the smaller footprint dimension, clamped to a
/// legible-but-not-huge range.
fn engrave_text(number: usize, bbox: &Bbox, xf: &Affine) -> Entity {
  let (cx, cy) = xf.apply((bbox.min_x + bbox.max_x) * 0.5, (bbox.min_y + bbox.max_y) * 0.5);
  let height = (bbox.width().min(bbox.height()) * 0.25).clamp(2.0, 10.0);
  let text = Text {
    value: number.to_string(),
    text_height: height,
    location: Point::new(cx, cy, 0.0),
    // With Center/Middle justification the anchor is the second alignment point.
    second_alignment_point: Point::new(cx, cy, 0.0),
    horizontal_text_justification: HorizontalTextJustification::Center,
    vertical_text_justification: VerticalTextJustification::Middle,
    ..Default::default()
  };
  let mut e = Entity::new(EntityType::Text(text));
  e.common.layer = ENGRAVE_LAYER.to_string();
  e.common.color = Color::from_index(ENGRAVE_COLOR);
  e
}

/// The largest outline-ring area of an entity, used to order cuts within a piece
/// (R3-4): sorting ascending puts small interior rings (holes/detail) before the
/// large outer contour, so a part stays anchored until its final outer cut. An
/// entity with no rings (a stray line) sorts first, which is harmless.
fn entity_area(e: &Entity) -> f64 {
  let mut rings = Vec::new();
  flatten::entity_rings(e, &mut rings);
  rings.iter().map(|r| flatten::area(r)).fold(0.0, f64::max)
}

/// Emit a ring of local-frame points as one polyline, mapped onto the sheet by
/// `xf` and put on `layer`/`color`. `closed` sets the polyline's closed flag.
fn emit_polyline(out: &mut Drawing, ring: &[[f64; 2]], xf: &Affine, closed: bool, layer: &str, color: u8) {
  let mut lw = LwPolyline::default();
  for p in ring {
    let (x, y) = xf.apply(p[0], p[1]);
    lw.vertices.push(LwPolylineVertex { x, y, ..Default::default() });
  }
  lw.set_is_closed(closed);
  add_on_layer(out, Entity::new(EntityType::LwPolyline(lw)), layer, color);
}

/// Break a closed `ring` into open cut segments separated by `tab_count` uncut
/// gaps ("micro-tabs") of length `tab_width`, distributed evenly by arc length
/// (R3-3). Returns each cut segment as an open point list. If the tabs would
/// consume the whole perimeter (or the ring is degenerate), returns the ring
/// whole so the part is still fully outlined rather than uncut.
fn tab_ring(ring: &[[f64; 2]], tab_width: f64, tab_count: usize) -> Vec<Vec<[f64; 2]>> {
  let n = ring.len();
  if n < 2 || tab_count == 0 {
    return vec![ring.to_vec()];
  }
  // Cumulative arc length at each vertex (cum[0]=0 .. cum[n]=perimeter).
  let mut cum = Vec::with_capacity(n + 1);
  cum.push(0.0);
  for i in 0..n {
    let a = ring[i];
    let b = ring[(i + 1) % n];
    cum.push(cum[i] + (b[0] - a[0]).hypot(b[1] - a[1]));
  }
  let perim = cum[n];
  let half = tab_width * 0.5;
  if perim <= 1e-9 || tab_width * tab_count as f64 >= perim {
    return vec![ring.to_vec()];
  }
  // Point at arc-length `s` along the ring (linear scan; rings are small).
  let point_at = |s: f64| -> [f64; 2] {
    let mut i = 0;
    while i + 1 < cum.len() && cum[i + 1] < s {
      i += 1;
    }
    let seg = (cum[i + 1] - cum[i]).max(1e-12);
    let t = ((s - cum[i]) / seg).clamp(0.0, 1.0);
    let a = ring[i % n];
    let b = ring[(i + 1) % n];
    [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t]
  };
  // Gaps are centred at k·perim/tab_count; keep the arc between consecutive
  // gaps as one cut segment: [c_k + half, c_{k+1} - half].
  let mut segments = Vec::new();
  for k in 0..tab_count {
    let a = k as f64 * perim / tab_count as f64 + half;
    let b = (k + 1) as f64 * perim / tab_count as f64 - half;
    if b <= a {
      continue;
    }
    let mut pts = vec![point_at(a)];
    for i in 1..n {
      if cum[i] > a && cum[i] < b {
        pts.push(ring[i]);
      }
    }
    pts.push(point_at(b));
    if pts.len() >= 2 {
      segments.push(pts);
    }
  }
  if segments.is_empty() { vec![ring.to_vec()] } else { segments }
}

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
/// When `opts.kerf_comp > 0`, each piece's outline is kerf-compensated (outer
/// grown / holes shrunk by `kerf_comp / 2`) and emitted as closed polylines
/// instead of its original entities — a deliberate, opt-in fidelity trade
/// (curved outlines become polylines) reported via [`EmitReport::diagnostics`].
///
/// Within each piece, cuts are ordered smallest-ring-first (R3-4) so interior
/// cuts precede the outer contour and a part stays anchored until its last cut.
/// When `opts.engrave_numbers`, each piece also gets a TEXT label of its assembly
/// number on the `Engrave` layer (R3-2).
fn build_one_sheet(source: &Drawing, pieces: &[Piece], placed: &[Placed], sheet: usize, opts: EmitOptions) -> Drawing {
  let mut out = Drawing::new();
  // Inherit the source's ACAD version. `Drawing::new()` defaults to R12, whose
  // writer silently drops SPLINE (an R13+ entity) — so without this every spline
  // outline would vanish from the output.
  out.header.version = source.header.version;
  register_cut_layer(&mut out);
  if opts.split_cut_layers {
    register_split_layers(&mut out);
  }
  if opts.engrave_numbers {
    register_engrave_layer(&mut out);
  }
  let mut added_blocks: HashSet<String> = HashSet::new();

  for placement in placed.iter().filter(|p| p.sheet == sheet) {
    let piece = &pieces[placement.piece_index];
    let xf = &placement.transform;

    // Polyline-approximation mode: kerf compensation and/or micro-tabs both
    // replace the piece's faithful entities with flattened outline rings. Rings
    // are ordered holes-first (ascending area) so interior cuts precede the
    // outer contour (R3-4).
    let tabbing = opts.tab_width > 0.0 && opts.tab_count > 0;
    if opts.kerf_comp > 0.0 || tabbing {
      let mut rings = if opts.kerf_comp > 0.0 {
        flatten::compensate_rings(&piece.rings(source), opts.kerf_comp * 0.5)
      } else {
        piece.rings(source)
      };
      rings.sort_by(|a, b| flatten::area(a).partial_cmp(&flatten::area(b)).unwrap_or(std::cmp::Ordering::Equal));
      // Largest-area ring is the outer contour; the rest are holes (R1-1b).
      let is_part = piece.source == PieceSource::Part;
      let last = rings.len().saturating_sub(1);
      for (i, ring) in rings.iter().enumerate() {
        if ring.len() < 2 {
          continue;
        }
        let (layer, color) = cut_role(opts, is_part, i == last);
        if tabbing {
          // Break the ring into open cut segments with uncut holding tabs (R3-3).
          for seg in tab_ring(ring, opts.tab_width, opts.tab_count) {
            emit_polyline(&mut out, &seg, xf, false, layer, color);
          }
        } else {
          emit_polyline(&mut out, ring, xf, true, layer, color);
        }
      }
      if opts.engrave_numbers {
        out.add_entity(engrave_text(placement.piece_index + 1, &piece.bbox, xf));
      }
      continue;
    }

    match &piece.kind {
      PieceKind::Loose(entities) => {
        // Emit interior cuts before the outer contour (R3-4): sort by outline
        // area ascending so the large outer ring is cut last. The largest is the
        // outer contour; the rest are holes (R1-1b outer/inner split).
        let mut order: Vec<&Entity> = entities.iter().collect();
        order.sort_by(|a, b| entity_area(a).partial_cmp(&entity_area(b)).unwrap_or(std::cmp::Ordering::Equal));
        let last = order.len().saturating_sub(1);
        for (i, entity) in order.into_iter().enumerate() {
          let mut e = entity.clone();
          transform_entity(&mut e, xf);
          let (layer, color) = cut_role(opts, true, i == last);
          add_on_layer(&mut out, e, layer, color);
        }
      }
      PieceKind::Insert { insert, block_name } => {
        // Locate the referenced block once, then reuse it below.
        let block = source.blocks().find(|b| &b.name == block_name);
        // Copy the block definition into this sheet on first use, moving its
        // sub-entities onto the cut layer/colour so the block's geometry cuts
        // even when the DXF consumer doesn't inherit the INSERT's layer, and
        // ordering them holes-first (R3-4) so the block cuts interior-out.
        if added_blocks.insert(block_name.clone())
          && let Some(b) = block {
            let mut b = b.clone();
            b.entities.sort_by(|a, c| entity_area(a).partial_cmp(&entity_area(c)).unwrap_or(std::cmp::Ordering::Equal));
            for e in &mut b.entities {
              e.common.layer = CUT_LAYER.to_string();
              e.common.color = Color::from_index(CUT_COLOR);
            }
            out.add_block(b);
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
        add_cut(&mut out, e);
      }
    }

    // Assembly number engraved at the piece's footprint centre (R3-2).
    if opts.engrave_numbers {
      out.add_entity(engrave_text(placement.piece_index + 1, &piece.bbox, xf));
    }
  }

  out.normalize();
  out
}

/// Build one output `Drawing` per sheet, in sheet order, without touching the
/// filesystem. Callers that want files use [`emit`]/[`emit_opts`]; callers that
/// want the drawings in memory (single-file export, preview, SVG conversion) use
/// this. `opts` controls kerf compensation, cut ordering, and engraving — see
/// [`build_one_sheet`].
pub fn build_sheet_drawings(source: &Drawing, pieces: &[Piece], placed: &[Placed], opts: EmitOptions) -> Vec<Drawing> {
  let sheet_count = placed.iter().map(|p| p.sheet).max().map_or(0, |m| m + 1);
  (0..sheet_count)
    .map(|sheet| build_one_sheet(source, pieces, placed, sheet, opts))
    .collect()
}

/// Write every sheet described by `placed` into `out_dir` as
/// `{file_stem}_sheet_{NN}.dxf`, with kerf compensation only (`kerf_comp` = 0 to
/// disable). A thin `f64` wrapper over [`emit_opts`] for callers that want just
/// the compensation knob; use [`emit_opts`] for engraving and other options.
///
/// `source` is the original drawing, used to copy block definitions that INSERT
/// pieces depend on.
pub fn emit(
  source: &Drawing,
  pieces: &[Piece],
  placed: &[Placed],
  out_dir: &Path,
  file_stem: &str,
  kerf_comp: f64,
) -> std::io::Result<EmitReport> {
  emit_opts(source, pieces, placed, out_dir, file_stem, EmitOptions { kerf_comp, ..Default::default() })
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
  emit(source, pieces, placed, out_dir, file_stem, kerf_comp)
}

/// Write every sheet described by `placed` into `out_dir` as
/// `{file_stem}_sheet_{NN}.dxf`, with full [`EmitOptions`] control (kerf
/// compensation, cut ordering, engraved assembly numbers).
pub fn emit_opts(
  source: &Drawing,
  pieces: &[Piece],
  placed: &[Placed],
  out_dir: &Path,
  file_stem: &str,
  opts: EmitOptions,
) -> std::io::Result<EmitReport> {
  std::fs::create_dir_all(out_dir)?;
  let mut files = Vec::new();
  for (sheet, drawing) in build_sheet_drawings(source, pieces, placed, opts).into_iter().enumerate() {
    let path = out_dir.join(format!("{file_stem}_sheet_{sheet:02}.dxf"));
    drawing
      .save_file(&path)
      .map_err(|e| std::io::Error::other(e.to_string()))?;
    files.push(path);
  }
  let mut diagnostics = Vec::new();
  if opts.kerf_comp > 0.0 {
    let n = placed.len();
    diagnostics.push(Diagnostic::info(format!(
      "kerf compensation on ({} units): {n} piece outline(s) offset and emitted as polylines (curved outlines approximated)",
      opts.kerf_comp
    )));
  }
  if opts.engrave_numbers {
    diagnostics.push(Diagnostic::info(
      "engraving on: each piece's assembly number added as TEXT on the Engrave layer".to_string(),
    ));
  }
  if opts.tab_width > 0.0 && opts.tab_count > 0 {
    diagnostics.push(Diagnostic::info(format!(
      "micro-tabs on: {} tab(s) of {} units per outline; cuts emitted as open polylines (curved outlines approximated)",
      opts.tab_count, opts.tab_width
    )));
  }
  if opts.split_cut_layers {
    diagnostics.push(Diagnostic::info(
      "outer/inner split on: loose part contours on Cut-Outer, holes on Cut-Inner".to_string(),
    ));
    let blocks = placed.iter().filter(|p| pieces[p.piece_index].source == PieceSource::Block).count();
    if blocks > 0 {
      diagnostics.push(Diagnostic::warning(format!(
        "{blocks} block piece(s) kept on the Cut layer: a block is one INSERT and can't be per-ring split"
      )));
    }
  }
  Ok(EmitReport { files, diagnostics })
}
