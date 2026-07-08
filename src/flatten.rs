//! Flatten DXF outline entities into polygons for the nesting engine.
//!
//! Splines are evaluated to points on the actual curve (De Boor), not their
//! control hull — the control polygon can self-intersect, which the nester
//! rejects. Each piece is reduced to a single simple polygon: the concave outer
//! ring when its other rings are interior holes, or the convex hull of all
//! rings when the piece has genuinely disjoint parts (so nothing is left
//! unreserved and pieces never overlap).

use dxf::entities::{Entity, EntityType};

/// A closed ring of points.
type Ring = Vec<[f64; 2]>;

const TAU: f64 = std::f64::consts::TAU;

/// Signed-area magnitude (shoelace) of a ring.
pub(crate) fn area(ring: &[[f64; 2]]) -> f64 {
  let mut a = 0.0;
  for i in 0..ring.len() {
    let j = (i + 1) % ring.len();
    a += ring[i][0] * ring[j][1] - ring[j][0] * ring[i][1];
  }
  a.abs() / 2.0
}

/// Evaluate a clamped B-spline of degree `p` at parameter `u` via De Boor.
/// `ctrl` are homogeneous points `[w*x, w*y, w]`, so this handles rational
/// (NURBS) splines too; for non-rational splines `w == 1`.
fn de_boor(p: usize, knots: &[f64], ctrl: &[[f64; 3]], u: f64) -> [f64; 2] {
  let n = ctrl.len() - 1;
  // Find the knot span index containing u (The NURBS Book, A2.1).
  let span = if u >= knots[n + 1] {
    n
  } else if u <= knots[p] {
    p
  } else {
    let (mut lo, mut hi) = (p, n + 1);
    let mut mid = (lo + hi) / 2;
    while u < knots[mid] || u >= knots[mid + 1] {
      if u < knots[mid] {
        hi = mid;
      } else {
        lo = mid;
      }
      mid = (lo + hi) / 2;
    }
    mid
  };

  let mut d: Vec<[f64; 3]> = (0..=p).map(|j| ctrl[span - p + j]).collect();
  for r in 1..=p {
    for j in (r..=p).rev() {
      let i = span - p + j;
      let denom = knots[i + p - r + 1] - knots[i];
      let a = if denom.abs() < 1e-12 {
        0.0
      } else {
        (u - knots[i]) / denom
      };
      let (lo, hi) = (d[j - 1], d[j]);
      d[j] = [
        (1.0 - a) * lo[0] + a * hi[0],
        (1.0 - a) * lo[1] + a * hi[1],
        (1.0 - a) * lo[2] + a * hi[2],
      ];
    }
  }
  let w = if d[p][2].abs() < 1e-12 { 1.0 } else { d[p][2] };
  [d[p][0] / w, d[p][1] / w]
}

/// Sample a spline entity into a closed ring.
fn spline_ring(s: &dxf::entities::Spline) -> Option<Ring> {
  let p = s.degree_of_curve.max(1) as usize;
  if s.control_points.len() < p + 1 || s.knot_values.len() != s.control_points.len() + p + 1 {
    return None;
  }
  let weights = &s.weight_values;
  let ctrl: Vec<[f64; 3]> = s
    .control_points
    .iter()
    .enumerate()
    .map(|(i, pt)| {
      let w = weights.get(i).copied().filter(|w| *w > 0.0).unwrap_or(1.0);
      [pt.x * w, pt.y * w, w]
    })
    .collect();
  let knots = &s.knot_values;
  let n = ctrl.len() - 1;
  let (u0, u1) = (knots[p], knots[n + 1]);
  if u1 <= u0 {
    return None;
  }
  // ~3 samples per control point, bounded, then simplified by the caller.
  let samples = ((n - p) * 3).clamp(32, 3000);
  let mut ring = Vec::with_capacity(samples + 1);
  for i in 0..=samples {
    let u = u0 + (u1 - u0) * (i as f64 / samples as f64);
    ring.push(de_boor(p, knots, &ctrl, u.min(u1)));
  }
  Some(ring)
}

/// Append the ring(s) contributed by one entity to `rings`.
pub fn entity_rings(entity: &Entity, rings: &mut Vec<Ring>) {
  match &entity.specific {
    EntityType::Spline(s) => {
      if let Some(r) = spline_ring(s) {
        rings.push(r);
      }
    }
    EntityType::LwPolyline(p) => {
      // Bulges (arcs) are approximated by their chords — adequate for a
      // packing footprint; the exact arc still travels with the piece.
      let ring: Ring = p.vertices.iter().map(|v| [v.x, v.y]).collect();
      if ring.len() >= 2 {
        rings.push(ring);
      }
    }
    EntityType::Polyline(p) => {
      let ring: Ring = p.vertices().map(|v| [v.location.x, v.location.y]).collect();
      if ring.len() >= 2 {
        rings.push(ring);
      }
    }
    EntityType::Line(l) => {
      rings.push(vec![[l.p1.x, l.p1.y], [l.p2.x, l.p2.y]]);
    }
    EntityType::Circle(c) => {
      rings.push(arc_ring(c.center.x, c.center.y, c.radius, 0.0, TAU));
    }
    EntityType::Arc(a) => {
      let (s, e) = (a.start_angle.to_radians(), a.end_angle.to_radians());
      let sweep = if e > s { e } else { e + TAU };
      rings.push(arc_ring(a.center.x, a.center.y, a.radius, s, sweep));
    }
    EntityType::Ellipse(e) => {
      rings.push(ellipse_ring(e));
    }
    _ => {}
  }
}

fn arc_ring(cx: f64, cy: f64, r: f64, start: f64, end: f64) -> Ring {
  let n = 48.max(((end - start).abs() / TAU * 64.0) as usize);
  (0..=n)
    .map(|i| {
      let t = start + (end - start) * (i as f64 / n as f64);
      [cx + r * t.cos(), cy + r * t.sin()]
    })
    .collect()
}

fn ellipse_ring(e: &dxf::entities::Ellipse) -> Ring {
  let (ax, ay) = (e.major_axis.x, e.major_axis.y);
  let a = (ax * ax + ay * ay).sqrt();
  let b = a * e.minor_axis_ratio;
  let rot = ay.atan2(ax);
  let (s, esw) = (e.start_parameter, e.end_parameter);
  let end = if esw > s { esw } else { esw + TAU };
  let n = 64;
  (0..=n)
    .map(|i| {
      let t = s + (end - s) * (i as f64 / n as f64);
      // point on axis-aligned ellipse, then rotate by the major-axis angle
      let (px, py) = (a * t.cos(), b * t.sin());
      [
        e.center.x + px * rot.cos() - py * rot.sin(),
        e.center.y + px * rot.sin() + py * rot.cos(),
      ]
    })
    .collect()
}

/// Ray-cast point-in-polygon test.
pub(crate) fn point_in_ring(pt: [f64; 2], ring: &[[f64; 2]]) -> bool {
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

/// Andrew's monotone-chain convex hull. Always yields a simple polygon, so it
/// is the safe fallback when a concave outline can't be used directly.
pub fn convex_hull(mut pts: Vec<[f64; 2]>) -> Ring {
  pts.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
  pts.dedup();
  if pts.len() < 3 {
    return pts;
  }
  let cross = |o: [f64; 2], a: [f64; 2], b: [f64; 2]| {
    (a[0] - o[0]) * (b[1] - o[1]) - (a[1] - o[1]) * (b[0] - o[0])
  };
  let mut lower = Vec::new();
  for &p in &pts {
    while lower.len() >= 2 && cross(lower[lower.len() - 2], lower[lower.len() - 1], p) <= 0.0 {
      lower.pop();
    }
    lower.push(p);
  }
  let mut upper = Vec::new();
  for &p in pts.iter().rev() {
    while upper.len() >= 2 && cross(upper[upper.len() - 2], upper[upper.len() - 1], p) <= 0.0 {
      upper.pop();
    }
    upper.push(p);
  }
  lower.pop();
  upper.pop();
  lower.extend(upper);
  lower
}

/// Douglas-Peucker simplification of an open polyline.
fn rdp(pts: &[[f64; 2]], eps: f64) -> Ring {
  if pts.len() < 3 {
    return pts.to_vec();
  }
  let (first, last) = (pts[0], pts[pts.len() - 1]);
  let (dx, dy) = (last[0] - first[0], last[1] - first[1]);
  let len = (dx * dx + dy * dy).sqrt().max(1e-9);
  let mut dmax = 0.0;
  let mut idx = 0;
  for (i, p) in pts.iter().enumerate().take(pts.len() - 1).skip(1) {
    let d = (dy * p[0] - dx * p[1] + last[0] * first[1] - last[1] * first[0]).abs() / len;
    if d > dmax {
      dmax = d;
      idx = i;
    }
  }
  if dmax > eps {
    let mut left = rdp(&pts[..=idx], eps);
    let right = rdp(&pts[idx..], eps);
    left.pop();
    left.extend(right);
    left
  } else {
    vec![first, last]
  }
}

/// Reduce a set of rings (all of one piece) to a single simple polygon that
/// **contains all of the piece's geometry**, in the same coordinate frame as
/// the rings. This containment is a hard requirement: emit transforms every
/// entity rigidly, so any geometry outside the reserved polygon would spill off
/// its sheet slot and overlap neighbours. Returns `None` if the piece has no
/// substantial ring (the caller then reserves its bounding box).
pub fn piece_polygon(rings: &[Ring], min_area: f64, simplify_eps: f64) -> Option<Ring> {
  let usable: Vec<&Ring> = rings.iter().filter(|r| r.len() >= 3).collect();
  if usable.is_empty() {
    return None;
  }
  // The outer candidate is the largest-area ring; require a real one so we don't
  // build a footprint out of degenerate slivers.
  let outer = *usable
    .iter()
    .max_by(|a, b| area(a).partial_cmp(&area(b)).unwrap_or(std::cmp::Ordering::Equal))
    .unwrap();
  if area(outer) < min_area {
    return None;
  }

  // Keep the outer ring's concavity ONLY if it already contains every vertex of
  // every other ring (interior holes). Otherwise some geometry — disjoint parts,
  // or degenerate artifacts sitting outside the silhouette — falls outside it, so
  // reserve the convex hull of ALL vertices, which contains everything.
  let contained = usable
    .iter()
    .all(|r| std::ptr::eq(*r, outer) || r.iter().all(|&p| point_in_ring(p, outer)));
  let mut ring = if contained {
    outer.clone()
  } else {
    let all_pts: Vec<[f64; 2]> = usable.iter().flat_map(|r| r.iter().copied()).collect();
    convex_hull(all_pts)
  };

  // Drop a duplicated closing vertex so Douglas-Peucker (which anchors the two
  // endpoints) doesn't degenerate on a zero-length base line. Returns a ring of
  // distinct vertices, not explicitly closed.
  if ring.len() > 1 {
    let (a, b) = (ring[0], ring[ring.len() - 1]);
    if (a[0] - b[0]).hypot(a[1] - b[1]) < 1e-9 {
      ring.pop();
    }
  }
  let simplified = rdp(&ring, simplify_eps);
  if simplified.len() < 3 {
    None
  } else {
    Some(simplified)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use dxf::entities::Spline;
  use dxf::Point;

  /// A clamped cubic B-spline that traces a square should evaluate to points
  /// on/near that square, closed, with a sensible bbox.
  #[test]
  fn spline_evaluates_on_curve() {
    // Clamped cubic through a diamond of control points.
    let cps = [
      (0.0, 0.0),
      (10.0, 0.0),
      (10.0, 10.0),
      (0.0, 10.0),
      (0.0, 0.0),
    ];
    let s = Spline {
      degree_of_curve: 3,
      control_points: cps.iter().map(|&(x, y)| Point::new(x, y, 0.0)).collect(),
      // clamped knot vector: len = n_ctrl + degree + 1 = 5 + 4 = 9
      knot_values: vec![0.0, 0.0, 0.0, 0.0, 1.0, 2.0, 2.0, 2.0, 2.0],
      ..Default::default()
    };
    let ring = spline_ring(&s).expect("ring");
    assert!(ring.len() > 10);
    // curve stays within the control hull bbox [0,10]x[0,10]
    for p in &ring {
      assert!(p[0] >= -0.01 && p[0] <= 10.01 && p[1] >= -0.01 && p[1] <= 10.01);
    }
    // endpoints coincide (clamped, first==last control point)
    let (a, b) = (ring[0], ring[ring.len() - 1]);
    assert!((a[0] - b[0]).abs() < 1e-6 && (a[1] - b[1]).abs() < 1e-6);
  }

  #[test]
  fn concave_outer_kept_holes_ignored() {
    // Big outer square + a small interior square (hole).
    let outer = vec![[0.0, 0.0], [100.0, 0.0], [100.0, 100.0], [0.0, 100.0], [0.0, 0.0]];
    let hole = vec![[40.0, 40.0], [60.0, 40.0], [60.0, 60.0], [40.0, 60.0], [40.0, 40.0]];
    let poly = piece_polygon(&[outer, hole], 1.0, 0.1).unwrap();
    // Should be ~the outer square (4-5 verts), not a hull merge.
    let (mut minx, mut maxx) = (f64::MAX, f64::MIN);
    for p in &poly {
      minx = minx.min(p[0]);
      maxx = maxx.max(p[0]);
    }
    assert!((minx - 0.0).abs() < 1e-6 && (maxx - 100.0).abs() < 1e-6);
    assert!(poly.len() <= 6);
  }

  #[test]
  fn disjoint_parts_use_hull() {
    // Two separated squares -> convex hull spans both.
    let a = vec![[0.0, 0.0], [10.0, 0.0], [10.0, 10.0], [0.0, 10.0], [0.0, 0.0]];
    let b = vec![[90.0, 0.0], [100.0, 0.0], [100.0, 10.0], [90.0, 10.0], [90.0, 0.0]];
    let poly = piece_polygon(&[a, b], 1.0, 0.1).unwrap();
    let maxx = poly.iter().map(|p| p[0]).fold(f64::MIN, f64::max);
    assert!((maxx - 100.0).abs() < 1e-6, "hull must span both parts");
  }
}
