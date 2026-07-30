//! Turning a file on disk into points the editor can draw.
//!
//! This runs on the main thread, from `Plugin::editor_message`, which is the
//! only place in the plugin allowed to touch a filesystem. Nothing here is
//! reachable from `process()`.
//!
//! The model is reduced to a point cloud rather than kept as geometry. Two
//! reasons: the file the user reaches for is whatever they downloaded — the
//! Lamborghini this was built against is 26 MB and 210,000 triangles, which
//! no plugin editor is going to draw at the refresh rate — and points are
//! what the audio can actually move.

use neta_visual::mesh;

/// Refuse anything larger before reading it. Generous enough for a detailed
/// car, small enough that a wrong pick cannot stall the host's main thread
/// while it reads a DVD image off a network drive.
const MAX_BYTES: u64 = 96 * 1_024 * 1_024;

/// Points sent to the page. Chosen against the compatibility editor's canvas:
/// a few thousand points read as a solid object at this size, and the whole
/// payload stays a single evaluate-JavaScript call.
const BUDGET: usize = 9_000;

/// Loads `path` and answers with the JavaScript that hands the page either a
/// cloud or a reason it has none.
pub(crate) fn load(path: &str) -> String {
    match read_cloud(path) {
        Ok((name, cloud)) => {
            let mut points = String::with_capacity(cloud.len() * 18);
            for point in &cloud {
                if !points.is_empty() {
                    points.push(',');
                }
                // Three decimals on a model normalised to ±1 is finer than
                // a pixel at any size this panel will ever be.
                points.push_str(&format!("{:.3},{:.3},{:.3}", point[0], point[1], point[2]));
            }
            format!(
                "window.__neta_model&&window.__neta_model({},[{points}]);",
                quote(&name)
            )
        }
        Err(problem) => format!(
            "window.__neta_model_failed&&window.__neta_model_failed({});",
            quote(&problem)
        ),
    }
}

fn read_cloud(path: &str) -> Result<(String, Vec<[f32; 3]>), String> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return Err("No file chosen".to_owned());
    }
    let file = std::path::Path::new(trimmed);
    let extension = file
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if extension != "obj" {
        return Err(format!("Neta reads .obj files, not .{extension}"));
    }
    // Check the size before reading, not after: the point is to avoid
    // pulling the bytes in at all.
    let size = std::fs::metadata(file)
        .map_err(|error| format!("Cannot open that file: {error}"))?
        .len();
    if size > MAX_BYTES {
        return Err(format!(
            "That file is {} MB. Neta loads models up to {} MB.",
            size / (1_024 * 1_024),
            MAX_BYTES / (1_024 * 1_024)
        ));
    }
    let source =
        std::fs::read_to_string(file).map_err(|error| format!("Cannot read it: {error}"))?;
    let mesh =
        mesh::load_obj(&source).map_err(|error| format!("That OBJ is malformed: {error}"))?;
    let cloud = mesh.unit_point_cloud(BUDGET);
    if cloud.is_empty() {
        return Err("That file has no usable geometry".to_owned());
    }
    let name = file
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("model")
        .to_owned();
    Ok((name, cloud))
}

/// A JavaScript string literal for arbitrary text.
///
/// Every path here comes from the user's own disk, but it still reaches the
/// page as source that gets evaluated, so a quote or a backslash in a
/// filename must not be able to end the literal.
pub(crate) fn quote(text: &str) -> String {
    let mut quoted = String::with_capacity(text.len() + 2);
    quoted.push('"');
    for character in text.chars() {
        match character {
            '"' => quoted.push_str("\\\""),
            '\\' => quoted.push_str("\\\\"),
            '\n' => quoted.push_str("\\n"),
            '\r' => quoted.push_str("\\r"),
            '\u{2028}' => quoted.push_str("\\u2028"),
            '\u{2029}' => quoted.push_str("\\u2029"),
            // Anything else in the control range would also terminate or
            // corrupt the literal; drop it rather than guess an escape.
            character if (character as u32) < 0x20 => {}
            character => quoted.push(character),
        }
    }
    quoted.push('"');
    quoted
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting_survives_a_hostile_filename() {
        assert_eq!(quote(r#"a"b"#), r#""a\"b""#);
        assert_eq!(quote(r"a\b"), r#""a\\b""#);
        assert_eq!(quote("a\nb"), r#""a\nb""#);
        assert_eq!(quote("a\u{2028}b"), r#""a\u2028b""#);
        assert_eq!(quote("a\u{7}b"), r#""ab""#);
    }

    #[test]
    fn a_missing_or_wrong_file_explains_itself_rather_than_failing_silently() {
        assert!(load("").contains("__neta_model_failed"));
        assert!(load("/nonexistent/model.obj").contains("Cannot open"));
        let answer = load("/nonexistent/model.txt");
        assert!(answer.contains("not .txt"), "{answer}");
    }

    #[test]
    fn a_real_obj_becomes_a_bounded_cloud() {
        let directory = std::env::temp_dir().join("neta-model-test");
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("box.obj");
        let mut source = String::new();
        for index in 0..50_000 {
            source.push_str(&format!("v {index} 0 0\n"));
        }
        std::fs::write(&path, &source).unwrap();

        let answer = load(path.to_str().unwrap());
        assert!(
            answer.starts_with("window.__neta_model&&"),
            "{}",
            &answer[..60]
        );
        assert!(answer.contains("\"box\""));
        // Three coordinates per point, comma separated, within budget.
        let body = answer.split('[').nth(1).unwrap();
        let coordinates = body.split(',').count();
        assert!(
            coordinates <= BUDGET * 3,
            "budget exceeded: {coordinates} coordinates"
        );
        std::fs::remove_dir_all(&directory).ok();
    }
}
