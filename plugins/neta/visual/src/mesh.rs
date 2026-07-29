//! Safe, bounded import helpers for Neta's visual objects.
//!
//! OBJ geometry is fully decoded for the initial native object path. GLB is
//! parsed as a validated container now; semantic glTF accessors are left to
//! the next adapter so malformed files never reach a GPU API unchecked.

use std::fmt;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Mesh {
    pub positions: Vec<[f32; 3]>,
    pub indices: Vec<u32>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ObjError {
    InvalidVertex { line: usize },
    InvalidFace { line: usize },
    IndexOutOfRange { line: usize },
    TooManyVertices,
}

impl fmt::Display for ObjError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidVertex { line } => write!(formatter, "invalid OBJ vertex at line {line}"),
            Self::InvalidFace { line } => write!(formatter, "invalid OBJ face at line {line}"),
            Self::IndexOutOfRange { line } => {
                write!(formatter, "OBJ face index out of range at line {line}")
            }
            Self::TooManyVertices => {
                formatter.write_str("OBJ has more vertices than u32 indices can address")
            }
        }
    }
}

impl std::error::Error for ObjError {}

/// Parses positions and triangular/polygonal `f` records from Wavefront OBJ.
/// Texture and normal slots (`f v/vt/vn`) are accepted then ignored because
/// Neta's first object pass colours procedurally from analysis data.
pub fn load_obj(source: &str) -> Result<Mesh, ObjError> {
    let mut mesh = Mesh::default();
    for (zero_line, raw_line) in source.lines().enumerate() {
        let line = zero_line + 1;
        let text = raw_line.trim();
        if text.is_empty() || text.starts_with('#') {
            continue;
        }
        let mut parts = text.split_whitespace();
        let Some(kind) = parts.next() else {
            continue;
        };
        match kind {
            "v" => {
                let coordinates = [
                    next_float(&mut parts, line)?,
                    next_float(&mut parts, line)?,
                    next_float(&mut parts, line)?,
                ];
                if coordinates.iter().any(|coordinate| !coordinate.is_finite()) {
                    return Err(ObjError::InvalidVertex { line });
                }
                mesh.positions.push(coordinates);
                if mesh.positions.len() > u32::MAX as usize {
                    return Err(ObjError::TooManyVertices);
                }
            }
            "f" => {
                let vertices: Result<Vec<usize>, ObjError> = parts
                    .map(|token| face_index(token, mesh.positions.len(), line))
                    .collect();
                let vertices = vertices?;
                if vertices.len() < 3 {
                    return Err(ObjError::InvalidFace { line });
                }
                for triangle in vertices[1..].windows(2) {
                    mesh.indices.extend([
                        vertices[0] as u32,
                        triangle[0] as u32,
                        triangle[1] as u32,
                    ]);
                }
            }
            _ => {}
        }
    }
    Ok(mesh)
}

fn next_float<'a>(parts: &mut impl Iterator<Item = &'a str>, line: usize) -> Result<f32, ObjError> {
    parts
        .next()
        .and_then(|part| part.parse::<f32>().ok())
        .ok_or(ObjError::InvalidVertex { line })
}

fn face_index(token: &str, vertex_count: usize, line: usize) -> Result<usize, ObjError> {
    let raw = token
        .split('/')
        .next()
        .unwrap_or_default()
        .parse::<i64>()
        .map_err(|_| ObjError::InvalidFace { line })?;
    let index = if raw > 0 {
        raw - 1
    } else if raw < 0 {
        i64::try_from(vertex_count).unwrap_or(i64::MAX) + raw
    } else {
        return Err(ObjError::InvalidFace { line });
    };
    usize::try_from(index)
        .ok()
        .filter(|index| *index < vertex_count)
        .ok_or(ObjError::IndexOutOfRange { line })
}

impl Mesh {
    /// The mesh as a point cloud that fits in the unit sphere, no larger
    /// than `budget` points.
    ///
    /// Two jobs, both of which a renderer would otherwise have to guess at.
    /// It recentres on the bounding-box centre, because an exported model
    /// sits wherever its author left it — this Lamborghini's origin is five
    /// metres behind the car — and rescales so one camera works for every
    /// model. Then it samples at a fixed stride, which keeps each part of
    /// the model in proportion to its own vertex count instead of taking
    /// the first `budget` vertices and dropping everything after the bonnet.
    ///
    /// A cloud rather than triangles because that is what the audio can
    /// move: points scatter, a surface can only spin.
    pub fn unit_point_cloud(&self, budget: usize) -> Vec<[f32; 3]> {
        if budget == 0 {
            return Vec::new();
        }
        // Non-finite points are dropped rather than clamped, and dropped
        // here rather than trusted to the loader: `Mesh` has public fields,
        // and `f32::min`/`max` return the *other* operand for a NaN, so
        // bounds taken over a NaN look perfectly finite while the NaN sails
        // through into the vertex buffer.
        let finite = |position: &&[f32; 3]| position.iter().all(|axis| axis.is_finite());
        let count = self.positions.iter().filter(finite).count();
        if count == 0 {
            return Vec::new();
        }
        let mut minimum = [f32::INFINITY; 3];
        let mut maximum = [f32::NEG_INFINITY; 3];
        for position in self.positions.iter().filter(finite) {
            for axis in 0..3 {
                minimum[axis] = minimum[axis].min(position[axis]);
                maximum[axis] = maximum[axis].max(position[axis]);
            }
        }
        let centre = [
            (minimum[0] + maximum[0]) * 0.5,
            (minimum[1] + maximum[1]) * 0.5,
            (minimum[2] + maximum[2]) * 0.5,
        ];
        let extent = (0..3)
            .map(|axis| maximum[axis] - minimum[axis])
            .fold(0.0_f32, f32::max);
        // A model with no extent is one point repeated; scaling it by
        // anything is arbitrary, so leave it at the origin.
        let scale = if extent > 0.0 { 2.0 / extent } else { 0.0 };

        let stride = count.div_ceil(budget).max(1);
        self.positions
            .iter()
            .filter(finite)
            .step_by(stride)
            .map(|position| {
                [
                    (position[0] - centre[0]) * scale,
                    (position[1] - centre[1]) * scale,
                    (position[2] - centre[2]) * scale,
                ]
            })
            .collect()
    }
}

pub const GLB_MAGIC: u32 = 0x4654_6C67;
pub const GLB_JSON_CHUNK: u32 = 0x4E4F_534A;
pub const GLB_BIN_CHUNK: u32 = 0x004E_4942;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Glb<'a> {
    pub json: &'a [u8],
    pub binary: Option<&'a [u8]>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GlbError {
    TooShort,
    BadMagic,
    UnsupportedVersion(u32),
    LengthMismatch,
    TruncatedChunk,
    MissingJson,
    DuplicateJson,
    DuplicateBinary,
    UnknownChunk(u32),
}

impl fmt::Display for GlbError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooShort => formatter.write_str("GLB is shorter than its header"),
            Self::BadMagic => formatter.write_str("GLB magic is invalid"),
            Self::UnsupportedVersion(version) => {
                write!(formatter, "unsupported GLB version {version}")
            }
            Self::LengthMismatch => formatter.write_str("GLB declared length does not match input"),
            Self::TruncatedChunk => formatter.write_str("GLB chunk exceeds input"),
            Self::MissingJson => formatter.write_str("GLB has no JSON chunk"),
            Self::DuplicateJson => formatter.write_str("GLB has more than one JSON chunk"),
            Self::DuplicateBinary => formatter.write_str("GLB has more than one binary chunk"),
            Self::UnknownChunk(kind) => write!(formatter, "unknown GLB chunk type {kind:#x}"),
        }
    }
}

impl std::error::Error for GlbError {}

/// Validates GLB v2 framing without unchecked casts, pointer arithmetic, or
/// allocation. The returned slices borrow the caller's input.
pub fn parse_glb(bytes: &[u8]) -> Result<Glb<'_>, GlbError> {
    if bytes.len() < 12 {
        return Err(GlbError::TooShort);
    }
    if le_u32(&bytes[0..4]) != GLB_MAGIC {
        return Err(GlbError::BadMagic);
    }
    let version = le_u32(&bytes[4..8]);
    if version != 2 {
        return Err(GlbError::UnsupportedVersion(version));
    }
    if usize::try_from(le_u32(&bytes[8..12])).ok() != Some(bytes.len()) {
        return Err(GlbError::LengthMismatch);
    }
    let mut offset = 12_usize;
    let mut json = None;
    let mut binary = None;
    while offset < bytes.len() {
        let header_end = offset.checked_add(8).ok_or(GlbError::TruncatedChunk)?;
        let header = bytes
            .get(offset..header_end)
            .ok_or(GlbError::TruncatedChunk)?;
        let length =
            usize::try_from(le_u32(&header[0..4])).map_err(|_| GlbError::TruncatedChunk)?;
        let kind = le_u32(&header[4..8]);
        let data_start = header_end;
        let data_end = data_start
            .checked_add(length)
            .ok_or(GlbError::TruncatedChunk)?;
        let data = bytes
            .get(data_start..data_end)
            .ok_or(GlbError::TruncatedChunk)?;
        match kind {
            GLB_JSON_CHUNK if json.replace(data).is_some() => return Err(GlbError::DuplicateJson),
            GLB_BIN_CHUNK if binary.replace(data).is_some() => {
                return Err(GlbError::DuplicateBinary);
            }
            GLB_JSON_CHUNK | GLB_BIN_CHUNK => {}
            _ => return Err(GlbError::UnknownChunk(kind)),
        }
        offset = data_end;
    }
    Ok(Glb {
        json: json.ok_or(GlbError::MissingJson)?,
        binary,
    })
}

fn le_u32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn obj_triangulates_quad_and_supports_negative_indices() {
        let mesh = load_obj("v 0 0 0\nv 1 0 0\nv 1 1 0\nv 0 1 0\nf -4 -3 -2 -1\n").unwrap();
        assert_eq!(mesh.positions.len(), 4);
        assert_eq!(mesh.indices, vec![0, 1, 2, 0, 2, 3]);
    }

    #[test]
    fn obj_rejects_forward_face_index() {
        assert_eq!(
            load_obj("f 1 2 3\n"),
            Err(ObjError::IndexOutOfRange { line: 1 })
        );
    }

    #[test]
    fn glb_parses_json_and_binary() {
        let json = br#"{}  "#;
        let binary = [1_u8, 2, 3, 4];
        let total = 12 + 8 + json.len() + 8 + binary.len();
        let mut bytes = Vec::new();
        bytes.extend(GLB_MAGIC.to_le_bytes());
        bytes.extend(2_u32.to_le_bytes());
        bytes.extend((total as u32).to_le_bytes());
        bytes.extend((json.len() as u32).to_le_bytes());
        bytes.extend(GLB_JSON_CHUNK.to_le_bytes());
        bytes.extend(json);
        bytes.extend((binary.len() as u32).to_le_bytes());
        bytes.extend(GLB_BIN_CHUNK.to_le_bytes());
        bytes.extend(binary);
        let parsed = parse_glb(&bytes).unwrap();
        assert_eq!(parsed.json, json);
        assert_eq!(parsed.binary, Some(binary.as_slice()));
    }

    #[test]
    fn glb_rejects_mismatched_length() {
        let mut bytes = [0_u8; 12];
        bytes[0..4].copy_from_slice(&GLB_MAGIC.to_le_bytes());
        bytes[4..8].copy_from_slice(&2_u32.to_le_bytes());
        bytes[8..12].copy_from_slice(&999_u32.to_le_bytes());
        assert_eq!(parse_glb(&bytes), Err(GlbError::LengthMismatch));
    }
}

#[cfg(test)]
mod cloud_tests {
    use super::*;

    fn mesh_of(positions: &[[f32; 3]]) -> Mesh {
        Mesh {
            positions: positions.to_vec(),
            indices: Vec::new(),
        }
    }

    #[test]
    fn the_cloud_is_recentred_and_fits_the_unit_sphere() {
        // Offset from the origin and 10 units across, like an exported model.
        let mesh = mesh_of(&[[10.0, 0.0, 0.0], [20.0, 0.0, 0.0], [15.0, 1.0, -1.0]]);
        let cloud = mesh.unit_point_cloud(16);
        assert_eq!(cloud.len(), 3);
        // x spans 10..20 so the extent is 10 and the scale 0.2; y and z are
        // recentred on their own midpoints too, which is why they are not 0.
        assert_eq!(cloud[0], [-1.0, -0.1, 0.1]);
        assert_eq!(cloud[1], [1.0, -0.1, 0.1]);
        for point in &cloud {
            assert!(
                point.iter().all(|axis| axis.abs() <= 1.001),
                "{point:?} left the unit box"
            );
        }
    }

    /// The budget is a rendering limit, so exceeding it is a bug the renderer
    /// cannot detect — it would just get slow.
    #[test]
    fn the_cloud_never_exceeds_its_budget_and_spans_the_whole_model() {
        let positions: Vec<[f32; 3]> = (0..1_000)
            .map(|index| [index as f32, 0.0, 0.0])
            .collect();
        let cloud = mesh_of(&positions).unit_point_cloud(64);
        assert!(cloud.len() <= 64, "budget exceeded: {}", cloud.len());
        // Sampled across the model, not truncated to its first 64 vertices.
        assert!(cloud.first().unwrap()[0] < -0.9);
        assert!(cloud.last().unwrap()[0] > 0.9);
    }

    #[test]
    fn degenerate_models_produce_nothing_rather_than_infinities() {
        assert!(mesh_of(&[]).unit_point_cloud(16).is_empty());
        assert!(mesh_of(&[[1.0, 1.0, 1.0]]).unit_point_cloud(0).is_empty());
        let single = mesh_of(&[[5.0, 5.0, 5.0]]).unit_point_cloud(16);
        assert_eq!(single, vec![[0.0, 0.0, 0.0]]);
        // A NaN vertex is dropped, and the finite ones still arrive.
        let broken = mesh_of(&[[f32::NAN, 0.0, 0.0], [1.0, 0.0, 0.0], [3.0, 0.0, 0.0]]);
        let cloud = broken.unit_point_cloud(16);
        assert_eq!(cloud, vec![[-1.0, 0.0, 0.0], [1.0, 0.0, 0.0]]);
        assert!(mesh_of(&[[f32::NAN; 3]]).unit_point_cloud(16).is_empty());
    }
}
