//! 2D geometry helpers: bounding boxes and affine transforms.

/// An axis-aligned bounding box in DXF units.
#[derive(Clone, Copy, Debug)]
pub struct Bbox {
  pub min_x: f64,
  pub min_y: f64,
  pub max_x: f64,
  pub max_y: f64,
}

impl Bbox {
  /// An empty box that swallows points as they are added.
  pub fn empty() -> Self {
    Bbox {
      min_x: f64::INFINITY,
      min_y: f64::INFINITY,
      max_x: f64::NEG_INFINITY,
      max_y: f64::NEG_INFINITY,
    }
  }

  pub fn add_point(&mut self, x: f64, y: f64) {
    if x < self.min_x {
      self.min_x = x;
    }
    if y < self.min_y {
      self.min_y = y;
    }
    if x > self.max_x {
      self.max_x = x;
    }
    if y > self.max_y {
      self.max_y = y;
    }
  }

  pub fn is_empty(&self) -> bool {
    self.min_x > self.max_x || self.min_y > self.max_y
  }

  pub fn width(&self) -> f64 {
    self.max_x - self.min_x
  }

  pub fn height(&self) -> f64 {
    self.max_y - self.min_y
  }
}

/// A 2D affine transform `x' = m00*x + m01*y + tx`, `y' = m10*x + m11*y + ty`.
///
/// Only rotation about Z and translation are used here (no scale/shear), which
/// keeps splines, polylines and block references geometrically faithful.
#[derive(Clone, Copy, Debug)]
pub struct Affine {
  pub m00: f64,
  pub m01: f64,
  pub m10: f64,
  pub m11: f64,
  pub tx: f64,
  pub ty: f64,
}

impl Affine {
  /// Build the transform that places a piece whose local bounding box is
  /// `bbox`, rotated by `theta` radians, so that the rotated box's min corner
  /// lands at `(target_x, target_y)`.
  pub fn place(bbox: &Bbox, theta: f64, target_x: f64, target_y: f64) -> Affine {
    let (s, c) = theta.sin_cos();
    // Rotate the four corners to find the rotated box's min corner.
    let corners = [
      (bbox.min_x, bbox.min_y),
      (bbox.max_x, bbox.min_y),
      (bbox.min_x, bbox.max_y),
      (bbox.max_x, bbox.max_y),
    ];
    let mut rmin_x = f64::INFINITY;
    let mut rmin_y = f64::INFINITY;
    for (x, y) in corners {
      let rx = c * x - s * y;
      let ry = s * x + c * y;
      if rx < rmin_x {
        rmin_x = rx;
      }
      if ry < rmin_y {
        rmin_y = ry;
      }
    }
    // world = Rot * p + t, with t chosen so the rotated min corner hits target.
    Affine {
      m00: c,
      m01: -s,
      m10: s,
      m11: c,
      tx: target_x - rmin_x,
      ty: target_y - rmin_y,
    }
  }

  /// Apply the full transform (rotation + translation) to a point.
  pub fn apply(&self, x: f64, y: f64) -> (f64, f64) {
    (
      self.m00 * x + self.m01 * y + self.tx,
      self.m10 * x + self.m11 * y + self.ty,
    )
  }

  /// Apply only the linear part (rotation) to a direction vector.
  pub fn apply_vec(&self, x: f64, y: f64) -> (f64, f64) {
    (self.m00 * x + self.m01 * y, self.m10 * x + self.m11 * y)
  }

  /// The rotation component, in radians (the transform has no scale/shear).
  pub fn rotation(&self) -> f64 {
    self.m10.atan2(self.m00)
  }

  /// Determinant of the linear part. `+1` for a pure rotation, `-1` for a
  /// reflection (mirror). Used by `emit` to correct sweep/scale for mirrored
  /// placements.
  pub fn determinant(&self) -> f64 {
    self.m00 * self.m11 - self.m01 * self.m10
  }

  /// Reflection of a piece across the vertical line `x = cx` (a horizontal
  /// flip), or the horizontal line `y = cy` (a vertical flip). These compose
  /// onto an existing placement to mirror it in place; the result has
  /// determinant `-1`.
  pub fn reflect_x(cx: f64) -> Affine {
    // (x, y) -> (2·cx - x, y)
    Affine { m00: -1.0, m01: 0.0, m10: 0.0, m11: 1.0, tx: 2.0 * cx, ty: 0.0 }
  }

  pub fn reflect_y(cy: f64) -> Affine {
    // (x, y) -> (x, 2·cy - y)
    Affine { m00: 1.0, m01: 0.0, m10: 0.0, m11: -1.0, tx: 0.0, ty: 2.0 * cy }
  }

  /// Rotation by `theta` radians about the point `(cx, cy)` — a pure
  /// rotation+translation (determinant +1), so it stays emit-faithful.
  pub fn rotation_about(cx: f64, cy: f64, theta: f64) -> Affine {
    let (s, c) = theta.sin_cos();
    // p' = R*(p - c) + c  =>  translation = c - R*c
    Affine {
      m00: c,
      m01: -s,
      m10: s,
      m11: c,
      tx: cx - (c * cx - s * cy),
      ty: cy - (s * cx + c * cy),
    }
  }

  /// Compose two transforms: the result applies `inner` first, then `self`
  /// (`self(inner(p))`). Used to layer an edit (e.g. a rotate) onto an existing
  /// placement transform.
  pub fn compose(&self, inner: &Affine) -> Affine {
    Affine {
      m00: self.m00 * inner.m00 + self.m01 * inner.m10,
      m01: self.m00 * inner.m01 + self.m01 * inner.m11,
      m10: self.m10 * inner.m00 + self.m11 * inner.m10,
      m11: self.m10 * inner.m01 + self.m11 * inner.m11,
      tx: self.m00 * inner.tx + self.m01 * inner.ty + self.tx,
      ty: self.m10 * inner.tx + self.m11 * inner.ty + self.ty,
    }
  }
}
