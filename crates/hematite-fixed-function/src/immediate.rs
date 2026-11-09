//! Bounded immediate-mode vertex capture and smooth-shaded topology conversion.
//!
//! Current attributes persist across begin/end, including empty batches. Positions are
//! retained in object space for a future GPU vertex shader. Flat-shading provoking-vertex
//! rules and polygon edge flags are not implemented by this module.

use crate::Error;

/// Legacy GL primitive modes accepted by `glBegin`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Primitive {
    /// Independent points.
    Points,
    /// Independent pairs of vertices.
    Lines,
    /// Adjacent vertices joined by lines.
    LineStrip,
    /// Line strip closed back to its first vertex.
    LineLoop,
    /// Independent triples of vertices.
    Triangles,
    /// Triangles with alternating winding order.
    TriangleStrip,
    /// Triangles sharing the first vertex.
    TriangleFan,
    /// Independent groups of four vertices.
    Quads,
    /// Quad pairs sharing the previous pair.
    QuadStrip,
    /// One convex polygon; concave/nonplanar polygons are undefined in legacy GL.
    Polygon,
}

impl Primitive {
    /// Decode GL enums 0x0000 through 0x0009. Adjacency/patch enums are not legal here.
    pub fn from_gl(value: u32) -> Result<Self, Error> {
        Ok(match value {
            0 => Self::Points,
            1 => Self::Lines,
            2 => Self::LineLoop,
            3 => Self::LineStrip,
            4 => Self::Triangles,
            5 => Self::TriangleStrip,
            6 => Self::TriangleFan,
            7 => Self::Quads,
            8 => Self::QuadStrip,
            9 => Self::Polygon,
            _ => return Err(Error::InvalidEnum),
        })
    }
}

/// Primitive list consumed by a backend after legacy conversion.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListTopology {
    /// One index per point.
    Points,
    /// Two indices per line.
    Lines,
    /// Three indices per triangle.
    Triangles,
}

/// Current GL vertex attributes captured when a vertex is submitted.
/// Eight texture coordinate sets cover the intended legacy Minecraft path.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Attributes {
    /// Primary RGBA colour, initially white.
    pub color: [f32; 4],
    /// Object-space normal, initially +Z.
    pub normal: [f32; 3],
    /// Homogeneous texture coordinates per unit, initially (0,0,0,1).
    pub tex_coords: [[f32; 4]; 8],
    /// Explicit fog coordinate, initially zero.
    pub fog_coord: f32,
}

impl Default for Attributes {
    fn default() -> Self {
        Self {
            color: [1.0; 4],
            normal: [0.0, 0.0, 1.0],
            tex_coords: [[0.0, 0.0, 0.0, 1.0]; 8],
            fog_coord: 0.0,
        }
    }
}

/// A captured vertex. Position defaults (z=0, w=1) must be supplied by the frontend's
/// `glVertex2*` / `glVertex3*` adapters; this module accepts explicit homogeneous positions.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Vertex {
    /// Object-space homogeneous position.
    pub position: [f64; 4],
    /// Snapshot of current attributes at submission time.
    pub attributes: Attributes,
}

/// Borrowed output, valid until the assembler is next mutated. No per-batch allocation.
#[derive(Debug)]
pub struct Batch<'a> {
    /// Backend primitive topology.
    pub topology: ListTopology,
    /// Captured vertices; incomplete trailing primitives are retained but not indexed.
    pub vertices: &'a [Vertex],
    /// Expanded primitive-list indices.
    pub indices: &'a [u32],
}

/// A context-owned, preallocated begin/end recorder.
/// No call silently grows the buffers. On capacity exhaustion `vertex` reports an error
/// and preserves all previously captured vertices; callers must surface this failure.
#[derive(Debug)]
pub struct Immediate {
    mode: Option<Primitive>,
    current: Attributes,
    vertices: Vec<Vertex>,
    indices: Vec<u32>,
    limit: usize,
}

impl Immediate {
    /// Reserve all memory up front. The index bound (6 per vertex) includes every mode.
    pub fn new(max_vertices: usize) -> Result<Self, Error> {
        if max_vertices == 0 || max_vertices > u32::MAX as usize {
            return Err(Error::InvalidValue);
        }
        let max_indices = max_vertices.checked_mul(6).ok_or(Error::Capacity)?;
        let mut vertices = Vec::new();
        let mut indices = Vec::new();
        vertices.try_reserve_exact(max_vertices).map_err(|_| Error::Capacity)?;
        indices.try_reserve_exact(max_indices).map_err(|_| Error::Capacity)?;
        Ok(Self {
            mode: None,
            current: Attributes::default(),
            vertices,
            indices,
            limit: max_vertices,
        })
    }

    /// Current attributes, writable both inside and outside begin/end.
    pub fn attributes_mut(&mut self) -> &mut Attributes {
        &mut self.current
    }

    /// Whether a begin/end block is open.
    pub fn is_active(&self) -> bool {
        self.mode.is_some()
    }

    /// Start a batch. Nested begin is rejected without clearing the existing batch.
    pub fn begin(&mut self, mode: Primitive) -> Result<(), Error> {
        if self.is_active() {
            return Err(Error::InvalidOperation);
        }
        self.vertices.clear();
        self.indices.clear();
        self.mode = Some(mode);
        Ok(())
    }

    /// Capture one vertex and its current attributes. Only legal inside begin/end.
    pub fn vertex(&mut self, position: [f64; 4]) -> Result<(), Error> {
        if !self.is_active() {
            return Err(Error::InvalidOperation);
        }
        if self.vertices.len() == self.limit {
            return Err(Error::Capacity);
        }
        self.vertices.push(Vertex { position, attributes: self.current });
        Ok(())
    }

    /// Close the batch, discarding incomplete primitives from its index stream.
    pub fn end(&mut self) -> Result<Batch<'_>, Error> {
        let mode = self.mode.take().ok_or(Error::InvalidOperation)?;
        let n = self.vertices.len() as u32;
        let topology = match mode {
            Primitive::Points => {
                self.indices.extend(0..n);
                ListTopology::Points
            }
            Primitive::Lines => {
                self.indices.extend(0..(n / 2 * 2));
                ListTopology::Lines
            }
            Primitive::LineStrip | Primitive::LineLoop => {
                for i in 1..n {
                    self.indices.extend_from_slice(&[i - 1, i]);
                }
                if mode == Primitive::LineLoop && n >= 2 {
                    self.indices.extend_from_slice(&[n - 1, 0]);
                }
                ListTopology::Lines
            }
            Primitive::Triangles => {
                self.indices.extend(0..(n / 3 * 3));
                ListTopology::Triangles
            }
            Primitive::TriangleStrip => {
                for i in 2..n {
                    let pair = if i % 2 == 0 { [i - 2, i - 1] } else { [i - 1, i - 2] };
                    self.indices.extend_from_slice(&[pair[0], pair[1], i]);
                }
                ListTopology::Triangles
            }
            Primitive::TriangleFan | Primitive::Polygon => {
                for i in 2..n {
                    self.indices.extend_from_slice(&[0, i - 1, i]);
                }
                ListTopology::Triangles
            }
            Primitive::Quads => {
                for quad in 0..(n / 4) {
                    let i = quad * 4;
                    self.indices.extend_from_slice(&[i, i + 1, i + 2, i, i + 2, i + 3]);
                }
                ListTopology::Triangles
            }
            Primitive::QuadStrip => {
                for pair in 1..(n / 2) {
                    let i = (pair - 1) * 2;
                    self.indices.extend_from_slice(&[i, i + 1, i + 3, i, i + 3, i + 2]);
                }
                ListTopology::Triangles
            }
        };
        Ok(Batch { topology, vertices: &self.vertices, indices: &self.indices })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn indices(mode: Primitive, n: usize) -> Vec<u32> {
        let mut immediate = Immediate::new(n.max(1)).unwrap();
        immediate.begin(mode).unwrap();
        for i in 0..n {
            immediate.vertex([i as f64, 0.0, 0.0, 1.0]).unwrap();
        }
        immediate.end().unwrap().indices.to_vec()
    }

    #[test]
    fn decode_primitive_enums() {
        for value in 0..10 {
            assert!(Primitive::from_gl(value).is_ok());
        }
        assert_eq!(Primitive::from_gl(10), Err(Error::InvalidEnum));
        assert_eq!(Primitive::from_gl(u32::MAX), Err(Error::InvalidEnum));
    }

    #[test]
    fn independent_primitives_drop_incomplete_tails() {
        assert_eq!(indices(Primitive::Points, 3), [0, 1, 2]);
        assert_eq!(indices(Primitive::Lines, 5), [0, 1, 2, 3]);
        assert_eq!(indices(Primitive::Triangles, 5), [0, 1, 2]);
        assert!(indices(Primitive::Quads, 3).is_empty());
    }

    #[test]
    fn line_loop_closes_and_strip_does_not() {
        assert_eq!(indices(Primitive::LineStrip, 3), [0, 1, 1, 2]);
        assert_eq!(indices(Primitive::LineLoop, 3), [0, 1, 1, 2, 2, 0]);
        assert_eq!(indices(Primitive::LineLoop, 2), [0, 1, 1, 0]);
        assert!(indices(Primitive::LineLoop, 1).is_empty());
    }

    #[test]
    fn triangle_strip_alternates_winding() {
        assert_eq!(indices(Primitive::TriangleStrip, 5), [0, 1, 2, 2, 1, 3, 2, 3, 4]);
    }

    #[test]
    fn fan_and_convex_polygon() {
        for mode in [Primitive::TriangleFan, Primitive::Polygon] {
            assert_eq!(indices(mode, 5), [0, 1, 2, 0, 2, 3, 0, 3, 4]);
        }
    }

    #[test]
    fn quad_expansion() {
        assert_eq!(indices(Primitive::Quads, 9), [0, 1, 2, 0, 2, 3, 4, 5, 6, 4, 6, 7]);
        assert_eq!(indices(Primitive::QuadStrip, 7), [0, 1, 3, 0, 3, 2, 2, 3, 5, 2, 5, 4]);
    }

    #[test]
    fn empty_batches_are_legal() {
        for value in 0..10 {
            assert!(indices(Primitive::from_gl(value).unwrap(), 0).is_empty());
        }
    }

    #[test]
    fn current_attributes_are_snapshotted_and_persist() {
        let mut im = Immediate::new(3).unwrap();
        im.attributes_mut().color = [0.0, 1.0, 0.0, 1.0];
        im.begin(Primitive::Points).unwrap();
        im.vertex([0.0; 4]).unwrap();
        im.attributes_mut().color = [1.0, 0.0, 0.0, 1.0];
        im.attributes_mut().tex_coords[1] = [0.5, 0.25, 0.0, 1.0];
        im.vertex([1.0; 4]).unwrap();
        let batch = im.end().unwrap();
        assert_eq!(batch.vertices[0].attributes.color, [0.0, 1.0, 0.0, 1.0]);
        assert_eq!(batch.vertices[1].attributes.color, [1.0, 0.0, 0.0, 1.0]);
        assert_eq!(batch.vertices[1].attributes.tex_coords[1], [0.5, 0.25, 0.0, 1.0]);
        im.begin(Primitive::Points).unwrap();
        im.vertex([2.0; 4]).unwrap();
        assert_eq!(im.end().unwrap().vertices[0].attributes.color, [1.0, 0.0, 0.0, 1.0]);
    }

    #[test]
    fn begin_end_errors_do_not_destroy_batch() {
        let mut im = Immediate::new(2).unwrap();
        assert!(matches!(im.end(), Err(Error::InvalidOperation)));
        assert_eq!(im.vertex([0.0; 4]), Err(Error::InvalidOperation));
        im.begin(Primitive::Lines).unwrap();
        im.vertex([0.0; 4]).unwrap();
        assert_eq!(im.begin(Primitive::Triangles), Err(Error::InvalidOperation));
        im.vertex([1.0; 4]).unwrap();
        assert_eq!(im.end().unwrap().indices, [0, 1]);
        assert!(!im.is_active());
    }

    #[test]
    fn bounded_capacity_and_reuse() {
        assert!(matches!(Immediate::new(0), Err(Error::InvalidValue)));
        let mut im = Immediate::new(1).unwrap();
        for _ in 0..3 {
            im.begin(Primitive::Points).unwrap();
            im.vertex([0.0; 4]).unwrap();
            assert_eq!(im.vertex([1.0; 4]), Err(Error::Capacity));
            assert_eq!(im.end().unwrap().vertices.len(), 1);
        }
    }

    #[test]
    fn storage_does_not_grow_after_creation() {
        let mut im = Immediate::new(64).unwrap();
        let vertex_storage = im.vertices.as_ptr();
        let index_storage = im.indices.as_ptr();
        let capacities = (im.vertices.capacity(), im.indices.capacity());
        for value in 0..10 {
            im.begin(Primitive::from_gl(value).unwrap()).unwrap();
            for _ in 0..64 {
                im.vertex([0.0; 4]).unwrap();
            }
            im.end().unwrap();
            assert_eq!(im.vertices.as_ptr(), vertex_storage);
            assert_eq!(im.indices.as_ptr(), index_storage);
            assert_eq!((im.vertices.capacity(), im.indices.capacity()), capacities);
        }
    }

    #[test]
    fn all_indices_in_range_for_all_small_counts() {
        for value in 0..10 {
            for n in 0..65 {
                let out = indices(Primitive::from_gl(value).unwrap(), n);
                assert!(out.iter().all(|i| (*i as usize) < n));
                assert!(out.len() <= 6 * n);
            }
        }
    }
}
