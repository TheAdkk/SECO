//! User-object import for Neta's native visual layer.
//!
//! Import runs before the app event loop, never in an audio callback. OBJ is
//! parsed by Neta's small safe parser; glTF/GLB accessor decoding uses the
//! safe `gltf` crate. Imported vertices become a bounded reactive point cloud
//! rendered by the same WGPU point path as Dalia.

use std::path::Path;

use neta_visual::mesh::load_obj;

const MAX_OBJECT_POINTS: usize = 8_192;
const MAX_OBJECT_BYTES: u64 = 16 * 1024 * 1024;
type Matrix4 = [[f32; 4]; 4];

pub struct ImportedObject {
    label: String,
    positions: Vec<f32>,
    colors: Vec<f32>,
}

impl ImportedObject {
    pub fn load(path: &Path) -> Result<Self, String> {
        ensure_file_size(path)?;
        let extension = path
            .extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        let raw_positions = match extension.as_str() {
            "obj" => {
                let text = std::fs::read_to_string(path).map_err(|error| error.to_string())?;
                load_obj(&text)
                    .map_err(|error| error.to_string())?
                    .positions
                    .into_iter()
                    .take(MAX_OBJECT_POINTS)
                    .collect()
            }
            "gltf" | "glb" => load_gltf(path)?,
            _ => return Err("supported object formats: .obj, .gltf, .glb".to_owned()),
        };
        Self::from_positions(path.display().to_string(), raw_positions)
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn positions(&self) -> &[f32] {
        &self.positions
    }

    pub fn colors(&self) -> &[f32] {
        &self.colors
    }

    fn from_positions(label: String, raw_positions: Vec<[f32; 3]>) -> Result<Self, String> {
        if raw_positions.is_empty() {
            return Err("object contains no POSITION vertices".to_owned());
        }
        let mut minimum = [f32::INFINITY; 3];
        let mut maximum = [f32::NEG_INFINITY; 3];
        for point in &raw_positions {
            for axis in 0..3 {
                let value = if point[axis].is_finite() {
                    point[axis]
                } else {
                    0.0
                };
                minimum[axis] = minimum[axis].min(value);
                maximum[axis] = maximum[axis].max(value);
            }
        }
        let center: [f32; 3] = std::array::from_fn(|axis| (minimum[axis] + maximum[axis]) * 0.5);
        let largest = (0..3)
            .map(|axis| (maximum[axis] - minimum[axis]).abs() * 0.5)
            .fold(0.0_f32, f32::max)
            .max(0.000_1);
        let mut positions = Vec::with_capacity(raw_positions.len() * 3);
        let mut colors = Vec::with_capacity(raw_positions.len() * 3);
        for (index, point) in raw_positions.into_iter().enumerate() {
            let point = point.map(|value| if value.is_finite() { value } else { 0.0 });
            let normalized: [f32; 3] =
                std::array::from_fn(|axis| (point[axis] - center[axis]) / largest);
            positions.extend(normalized);
            let pulse = index as f32 / MAX_OBJECT_POINTS as f32;
            colors.extend([
                0.23 + pulse * 0.35,
                0.36 + pulse * 0.16,
                0.96 - pulse * 0.31,
            ]);
        }
        Ok(Self {
            label,
            positions,
            colors,
        })
    }
}

fn load_gltf(path: &Path) -> Result<Vec<[f32; 3]>, String> {
    let gltf = gltf::Gltf::open(path).map_err(|error| error.to_string())?;
    let gltf::Gltf { document, blob } = gltf;
    let declared_bytes = document.buffers().try_fold(0_u64, |total, buffer| {
        total
            .checked_add(buffer.length() as u64)
            .ok_or_else(|| "glTF buffer size overflows Neta import limit".to_owned())
    })?;
    if declared_bytes > MAX_OBJECT_BYTES {
        return Err(format!(
            "glTF declares {declared_bytes} bytes; Neta object limit is {MAX_OBJECT_BYTES}"
        ));
    }
    let buffers =
        gltf::import_buffers(&document, path.parent(), blob).map_err(|error| error.to_string())?;
    let mut positions = Vec::new();
    if let Some(scene) = document
        .default_scene()
        .or_else(|| document.scenes().next())
    {
        for node in scene.nodes() {
            append_node_positions(node, identity_matrix(), &buffers, &mut positions);
            if positions.len() == MAX_OBJECT_POINTS {
                break;
            }
        }
    } else {
        // Some generated glTF files omit scenes. Keep them usable, with an
        // explicit identity transform rather than silently importing nothing.
        for mesh in document.meshes() {
            append_mesh_positions(mesh, identity_matrix(), &buffers, &mut positions);
            if positions.len() == MAX_OBJECT_POINTS {
                break;
            }
        }
    }
    Ok(positions)
}

fn ensure_file_size(path: &Path) -> Result<(), String> {
    let bytes = std::fs::metadata(path)
        .map_err(|error| error.to_string())?
        .len();
    if bytes > MAX_OBJECT_BYTES {
        Err(format!(
            "object is {bytes} bytes; Neta object limit is {MAX_OBJECT_BYTES}"
        ))
    } else {
        Ok(())
    }
}

fn append_node_positions(
    node: gltf::Node<'_>,
    parent: Matrix4,
    buffers: &[gltf::buffer::Data],
    positions: &mut Vec<[f32; 3]>,
) {
    if positions.len() == MAX_OBJECT_POINTS {
        return;
    }
    let transform = multiply_matrix(parent, node.transform().matrix());
    if let Some(mesh) = node.mesh() {
        append_mesh_positions(mesh, transform, buffers, positions);
    }
    for child in node.children() {
        append_node_positions(child, transform, buffers, positions);
        if positions.len() == MAX_OBJECT_POINTS {
            return;
        }
    }
}

fn append_mesh_positions(
    mesh: gltf::Mesh<'_>,
    transform: Matrix4,
    buffers: &[gltf::buffer::Data],
    positions: &mut Vec<[f32; 3]>,
) {
    for primitive in mesh.primitives() {
        let reader =
            primitive.reader(|buffer| buffers.get(buffer.index()).map(|data| data.0.as_slice()));
        if let Some(vertices) = reader.read_positions() {
            for position in vertices {
                if positions.len() == MAX_OBJECT_POINTS {
                    return;
                }
                if let Some(position) = transform_point(transform, position) {
                    positions.push(position);
                }
            }
        }
    }
}

const fn identity_matrix() -> Matrix4 {
    [
        [1.0, 0.0, 0.0, 0.0],
        [0.0, 1.0, 0.0, 0.0],
        [0.0, 0.0, 1.0, 0.0],
        [0.0, 0.0, 0.0, 1.0],
    ]
}

fn multiply_matrix(left: Matrix4, right: Matrix4) -> Matrix4 {
    std::array::from_fn(|column| {
        std::array::from_fn(|row| {
            (0..4)
                .map(|index| left[index][row] * right[column][index])
                .sum()
        })
    })
}

fn transform_point(matrix: Matrix4, point: [f32; 3]) -> Option<[f32; 3]> {
    let x =
        matrix[0][0] * point[0] + matrix[1][0] * point[1] + matrix[2][0] * point[2] + matrix[3][0];
    let y =
        matrix[0][1] * point[0] + matrix[1][1] * point[1] + matrix[2][1] * point[2] + matrix[3][1];
    let z =
        matrix[0][2] * point[0] + matrix[1][2] * point[1] + matrix[2][2] * point[2] + matrix[3][2];
    let w =
        matrix[0][3] * point[0] + matrix[1][3] * point[1] + matrix[2][3] * point[2] + matrix[3][3];
    (x.is_finite() && y.is_finite() && z.is_finite() && w.is_finite() && w.abs() > f32::EPSILON)
        .then_some([x / w, y / w, z / w])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_points_are_normalized_and_coloured() {
        let object = ImportedObject::from_positions(
            "fixture".to_owned(),
            vec![[2.0, -1.0, 0.5], [-2.0, 1.0, -0.5]],
        )
        .unwrap();
        assert_eq!(object.positions.len(), 6);
        assert_eq!(object.colors.len(), 6);
        assert_eq!(object.positions[0], 1.0);
        assert_eq!(object.positions[3], -1.0);
    }

    #[test]
    fn object_points_are_centered_before_normalization() {
        let object = ImportedObject::from_positions(
            "offset fixture".to_owned(),
            vec![[10.0, 20.0, 30.0], [14.0, 20.0, 30.0]],
        )
        .unwrap();
        assert_eq!(&object.positions[..3], &[-1.0, 0.0, 0.0]);
        assert_eq!(&object.positions[3..], &[1.0, 0.0, 0.0]);
    }

    #[test]
    fn node_transform_is_applied_in_column_major_order() {
        let mut transform = identity_matrix();
        transform[3] = [3.0, -2.0, 1.0, 1.0];
        assert_eq!(
            transform_point(transform, [1.0, 2.0, 3.0]),
            Some([4.0, 0.0, 4.0])
        );
    }

    #[test]
    fn empty_object_is_rejected() {
        assert!(ImportedObject::from_positions("fixture".to_owned(), vec![]).is_err());
    }

    #[test]
    fn gltf_fixture_imports_positions_without_unsafe_slices() {
        let path = Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/triangle.gltf"
        ));
        let object = ImportedObject::load(path).unwrap();
        assert_eq!(object.positions.len(), 9);
        assert_eq!(object.colors.len(), 9);
    }
}
