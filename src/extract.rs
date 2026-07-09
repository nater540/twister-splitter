//! Turn a loaded DXF drawing into a flat list of packable pieces.
//!
//! A "piece" is one thing that gets nested onto a sheet. We recognise two
//! sources (both are kept, per the project spec):
//!
//! * **Loose entities grouped by layer** — every top-level spline/polyline is
//!   bucketed by its DXF layer; each layer becomes one piece.
//! * **Block references (INSERTs)** — each INSERT becomes its own piece that
//!   carries the entity plus a handle to its block definition.
//!
//! Solid HATCH fills are intentionally dropped (they duplicate the spline
//! outlines we already cut), and the `dxf` crate does not surface them anyway.

use std::collections::BTreeMap;
use std::hash::{Hash, Hasher};

use crate::flatten;

use dxf::Drawing;
use dxf::entities::{Entity, EntityType};

use crate::diag::Diagnostic;
use crate::geom::Bbox;

/// What a piece is made of.
#[derive(Clone)]
pub enum PieceKind {
  /// A group of loose entities that shared a DXF layer, in world coordinates.
  Loose(Vec<Entity>),
  /// A single block reference plus the name of the block it instantiates.
  /// Boxed to keep the enum small (an `Entity` is far larger than a `Vec`).
  Insert { insert: Box<Entity>, block_name: String },
}

/// Where a piece came from: a connected part of loose outline geometry, or a
/// block reference (INSERT). Lets the UI group/filter Part vs Block.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PieceSource {
  Part,
  Block,
}

#[derive(Clone)]
pub struct Piece {
  /// Human-readable label, e.g. `part:0` or `block:Block_0`.
  pub label: String,
  pub kind: PieceKind,
  /// Local bounding box (in the coordinates the geometry currently lives in).
  pub bbox: Bbox,
  /// True outline area (outer contour minus its holes), in DXF units².
  pub area: f64,
  /// Whether this piece is a loose part or a block reference.
  pub source: PieceSource,
  /// Content-derived identity, stable across re-extraction of the same input
  /// (unlike `label`/index, which shift when the source filter changes). Two
  /// geometrically-identical pieces get distinct ids via an occurrence counter.
  pub id: u64,
  /// How many copies of this piece to nest (R3-1). Extraction sets it to 1; a
  /// caller (the GUI's per-part quantity knob) may raise it. `nest::build_items`
  /// then reserves one nesting item per copy, so the from-scratch nesters place
  /// and `emit` renders that many. Reset to 1 on re-extraction.
  pub quantity: usize,
}

impl Piece {
  /// The piece's outline rings in its own (local) coordinate frame. Loose parts
  /// return their entities' rings; an INSERT resolves its block's entities. The
  /// place transform is rigid, so applying it to these rings yields the sheet
  /// geometry.
  pub fn rings(&self, drawing: &Drawing) -> Vec<Vec<[f64; 2]>> {
    let mut rings = Vec::new();
    match &self.kind {
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
    rings
  }
}

/// True outline area of a set of rings: the largest ring (outer contour) minus
/// every other ring (treated as holes). Matches how the piece is cut.
fn outline_area(rings: &[Vec<[f64; 2]>]) -> f64 {
  let mut areas: Vec<f64> = rings
    .iter()
    .map(|r| if r.len() >= 3 { flatten::area(r) } else { 0.0 })
    .collect();
  let Some((outer_i, &outer)) = areas
    .iter()
    .enumerate()
    .max_by(|a, b| a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal))
  else {
    return 0.0;
  };
  areas[outer_i] = 0.0;
  (outer - areas.iter().sum::<f64>()).max(0.0)
}

/// A stable content hash for a piece: block name or the rounded outer geometry,
/// mixed with an occurrence counter so identical duplicates stay distinct.
fn piece_id(kind: &PieceKind, bbox: &Bbox, area: f64, occurrence: u64) -> u64 {
  let mut h = std::collections::hash_map::DefaultHasher::new();
  match kind {
    PieceKind::Loose(_) => 0u8.hash(&mut h),
    PieceKind::Insert { block_name, .. } => {
      1u8.hash(&mut h);
      block_name.hash(&mut h);
    }
  }
  // Quantise geometry so floating-point jitter across loads doesn't shift ids.
  for v in [bbox.min_x, bbox.min_y, bbox.max_x, bbox.max_y, area] {
    ((v * 100.0).round() as i64).hash(&mut h);
  }
  occurrence.hash(&mut h);
  h.finish()
}

/// Accumulate a single entity's cut geometry into `bbox`.
///
/// Only outline geometry contributes (splines, polylines). For splines the
/// control-point hull bounds the curve, so this is a safe (slightly generous)
/// box — exactly what we want for nesting with a kerf gap.
fn accumulate_entity_bbox(entity: &Entity, bbox: &mut Bbox) {
  match &entity.specific {
    EntityType::Spline(s) => {
      for p in &s.control_points {
        bbox.add_point(p.x, p.y);
      }
      for p in &s.fit_points {
        bbox.add_point(p.x, p.y);
      }
    }
    EntityType::LwPolyline(p) => {
      for v in &p.vertices {
        bbox.add_point(v.x, v.y);
      }
    }
    EntityType::Polyline(p) => {
      for v in p.vertices() {
        bbox.add_point(v.location.x, v.location.y);
      }
    }
    EntityType::Line(l) => {
      bbox.add_point(l.p1.x, l.p1.y);
      bbox.add_point(l.p2.x, l.p2.y);
    }
    // Circles and arcs: use the full-circle box. It over-estimates an arc's
    // true extent, which is the safe direction for nesting with a kerf gap.
    EntityType::Circle(c) => {
      bbox.add_point(c.center.x - c.radius, c.center.y - c.radius);
      bbox.add_point(c.center.x + c.radius, c.center.y + c.radius);
    }
    EntityType::Arc(a) => {
      bbox.add_point(a.center.x - a.radius, a.center.y - a.radius);
      bbox.add_point(a.center.x + a.radius, a.center.y + a.radius);
    }
    // Ellipse: bound by the major radius on both axes (a safe over-estimate
    // regardless of the ellipse's rotation).
    EntityType::Ellipse(e) => {
      let a = (e.major_axis.x * e.major_axis.x + e.major_axis.y * e.major_axis.y).sqrt();
      bbox.add_point(e.center.x - a, e.center.y - a);
      bbox.add_point(e.center.x + a, e.center.y + a);
    }
    _ => {}
  }
}

/// True for entity types we keep as cuttable outlines.
fn is_outline(entity: &Entity) -> bool {
  matches!(
    entity.specific,
    EntityType::Spline(_)
      | EntityType::LwPolyline(_)
      | EntityType::Polyline(_)
      | EntityType::Line(_)
      | EntityType::Circle(_)
      | EntityType::Arc(_)
      | EntityType::Ellipse(_)
  )
}

/// Which piece sources to emit.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Sources {
  Layer,
  Block,
  Both,
}

/// Extract all pieces from a drawing according to `sources`.
///
/// Returns the pieces plus any [`Diagnostic`]s (skipped inserts, non-unit
/// scales, dropped degenerate parts) for the caller to surface — the library
/// never prints.
pub fn extract(drawing: &Drawing, sources: Sources) -> (Vec<Piece>, Vec<Diagnostic>) {
  let mut pieces = Vec::new();
  let mut diags = Vec::new();
  // Occurrence counters keyed by (content) so identical duplicate pieces get
  // distinct stable ids.
  let mut occ: BTreeMap<u64, u64> = BTreeMap::new();

  // Bounding boxes of every block definition, keyed by block name, so INSERT
  // pieces can be sized from the geometry they reference.
  let mut block_bbox: BTreeMap<String, Bbox> = BTreeMap::new();
  for block in drawing.blocks() {
    let mut bbox = Bbox::empty();
    for e in &block.entities {
      accumulate_entity_bbox(e, &mut bbox);
    }
    block_bbox.insert(block.name.clone(), bbox);
  }

  // INSERTs become pieces immediately; loose outline entities are collected and
  // then grouped into connected parts below.
  let mut loose: Vec<Entity> = Vec::new();

  for entity in drawing.entities() {
    if let EntityType::Insert(insert) = &entity.specific {
      if sources == Sources::Layer {
        continue;
      }
      let block_name = insert.name.clone();
      let bbox = block_bbox.get(&block_name).copied().unwrap_or_else(Bbox::empty);
      if bbox.is_empty() {
        diags.push(
          Diagnostic::warning(format!(
            "INSERT of block '{block_name}' has no cut geometry; skipping"
          ))
          .for_piece(format!("block:{block_name}")),
        );
        continue;
      }
      // The piece bbox is measured from the block's unscaled geometry, so a
      // non-unit insert scale would render larger/smaller than the reserved
      // footprint and overlap its neighbours. Warn rather than mis-nest.
      let non_unit_scale = (insert.x_scale_factor - 1.0).abs() > 1e-9
        || (insert.y_scale_factor - 1.0).abs() > 1e-9;
      if non_unit_scale {
        diags.push(
          Diagnostic::warning(format!(
            "INSERT of block '{block_name}' has a non-unit scale \
             ({:.3}x{:.3}); nesting assumes unit scale and its footprint may be wrong",
            insert.x_scale_factor, insert.y_scale_factor
          ))
          .for_piece(format!("block:{block_name}")),
        );
      }
      // True cut area from the block's own rings (outer minus holes).
      let mut rings = Vec::new();
      if let Some(b) = drawing.blocks().find(|b| b.name == block_name) {
        for e in &b.entities {
          flatten::entity_rings(e, &mut rings);
        }
      }
      let area = outline_area(&rings);
      let kind = PieceKind::Insert { insert: Box::new(entity.clone()), block_name: block_name.clone() };
      let content = piece_id(&kind, &bbox, area, 0);
      let n = occ.entry(content).or_insert(0);
      let id = piece_id(&kind, &bbox, area, *n);
      *n += 1;
      pieces.push(Piece {
        label: format!("block:{block_name}"),
        kind,
        bbox,
        area,
        source: PieceSource::Block,
        id,
        quantity: 1,
      });
    } else if is_outline(entity) {
      if sources == Sources::Block {
        continue;
      }
      loose.push(entity.clone());
    }
    // Everything else (hatches, text, etc.) is dropped.
  }

  // Group loose entities into connected parts (an outer contour plus the rings
  // nested inside it) by ring containment. This splits a layer that holds
  // several separate shapes into individually-nestable pieces, while keeping a
  // contour's holes with it — so nesting sees the true concave cut outlines.
  let mut dropped = 0usize;
  for (n, entities) in group_parts(loose).into_iter().enumerate() {
    let mut bbox = Bbox::empty();
    for e in &entities {
      accumulate_entity_bbox(e, &mut bbox);
    }
    // Drop degenerate parts (near-points / zero-width slivers): these are export
    // artifacts with no cuttable area, and the nester rejects zero-area shapes.
    if bbox.is_empty() || bbox.width() < 0.1 || bbox.height() < 0.1 {
      dropped += 1;
      continue;
    }
    let mut rings = Vec::new();
    for e in &entities {
      flatten::entity_rings(e, &mut rings);
    }
    let area = outline_area(&rings);
    let kind = PieceKind::Loose(entities);
    let content = piece_id(&kind, &bbox, area, 0);
    let occn = occ.entry(content).or_insert(0);
    let id = piece_id(&kind, &bbox, area, *occn);
    *occn += 1;
    pieces.push(Piece {
      label: format!("part:{n}"),
      kind,
      bbox,
      area,
      source: PieceSource::Part,
      id,
      quantity: 1,
    });
  }
  if dropped > 0 {
    diags.push(Diagnostic::info(format!(
      "dropped {dropped} degenerate part(s) with no cuttable area"
    )));
  }

  (pieces, diags)
}

/// Group outline entities into connected parts by ring containment.
///
/// Each entity contributes one closed ring. Using an even/odd containment-depth
/// rule, a solid contour (even depth: 0, 2, …) starts a part and absorbs the
/// rings immediately inside it (its holes, odd depth); a solid island sitting
/// inside a hole starts its own part. The result is one group of entities per
/// physically-cuttable part.
fn group_parts(entities: Vec<Entity>) -> Vec<Vec<Entity>> {
  let rings: Vec<Vec<[f64; 2]>> = entities
    .iter()
    .map(|e| {
      let mut r = Vec::new();
      crate::flatten::entity_rings(e, &mut r);
      r.into_iter().next().unwrap_or_default()
    })
    .collect();
  let areas: Vec<f64> = rings
    .iter()
    .map(|r| if r.len() >= 3 { flatten::area(r) } else { 0.0 })
    .collect();
  let n = entities.len();

  // `j` strictly contains `i` if it is larger and holds all of `i`'s vertices.
  let contains = |j: usize, i: usize| -> bool {
    if j == i || rings[j].len() < 3 || rings[i].len() < 3 || areas[j] <= areas[i] {
      return false;
    }
    rings[i].iter().all(|&p| flatten::point_in_ring(p, &rings[j]))
  };

  // For each ring: its containment depth and its immediate (smallest) container.
  let mut parent = vec![None; n];
  let mut depth = vec![0usize; n];
  for i in 0..n {
    let mut smallest: Option<usize> = None;
    for j in 0..n {
      if contains(j, i) {
        depth[i] += 1;
        if smallest.is_none_or(|b| areas[j] < areas[b]) {
          smallest = Some(j);
        }
      }
    }
    parent[i] = smallest;
  }

  // Root of each ring's part: itself when solid (even depth), else its container.
  let mut groups: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
  for i in 0..n {
    let root = if depth[i].is_multiple_of(2) { i } else { parent[i].unwrap_or(i) };
    groups.entry(root).or_default().push(i);
  }

  let mut ents: Vec<Option<Entity>> = entities.into_iter().map(Some).collect();
  groups
    .into_values()
    .map(|idxs| idxs.into_iter().map(|i| ents[i].take().unwrap()).collect())
    .collect()
}
