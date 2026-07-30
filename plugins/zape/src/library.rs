//! The curve library on disk: one small file per saved curve.
//!
//! The philosophy is Serum's — draw a shape once, reach for it in any
//! project — with one rule that comes first: **the curve in use lives in the
//! session, not here.** `clap.state` already carries it, so a project sounds
//! the same on a machine that has never seen this directory. The library is
//! a drawer to pull shapes out of, never the source of truth for how a track
//! sounds. A plugin whose presets live only on disk is a plugin that sounds
//! different when you send the project to someone else.
//!
//! One file per curve, holding exactly what the editor sends: it makes a
//! save an atomic rename, a delete a single unlink, and a curve something
//! you can mail to a friend.
//!
//! All of this runs on the main thread, from `Plugin::editor_message` —
//! never anywhere near the audio thread.

use std::path::{Path, PathBuf};

use crate::custom::CustomCurve;

/// Longest curve file worth reading. A curve is about a hundred bytes;
/// anything larger is not one of ours.
const MAX_FILE_BYTES: u64 = 4096;

/// How many curves the editor will list. A cap so a directory someone
/// dumped a thousand files into cannot stall the UI.
const MAX_ENTRIES: usize = 128;

/// File extension, distinctive enough to be recognizable in a folder.
const EXTENSION: &str = "zapecurve";

/// Where Zape keeps everything it owns on disk: the curve library and the
/// chosen skin.
///
/// `SECO_ZAPE_DIR` overrides it — that is what the tests use, and it gives
/// anyone a way to point the whole lot at a synced folder. One root rather
/// than one variable per file: pointing at the curve folder alone left the
/// skin file being written to *its parent*, which during tests was the
/// system temp directory.
pub(crate) fn data_directory() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("SECO_ZAPE_DIR") {
        return Some(PathBuf::from(path));
    }
    if cfg!(target_os = "macos") {
        let home = std::env::var_os("HOME")?;
        Some(PathBuf::from(home).join("Library/Application Support/SECO/Zape"))
    } else if cfg!(target_os = "windows") {
        let appdata = std::env::var_os("APPDATA")?;
        Some(PathBuf::from(appdata).join("SECO/Zape"))
    } else {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
        Some(base.join("seco/zape"))
    }
}

/// Where curves live: one file each, inside the data directory.
pub(crate) fn directory() -> Option<PathBuf> {
    Some(data_directory()?.join("curves"))
}

/// Reads a small preference file from the data directory.
///
/// Preferences are one file, one value: a skin name, a flag. They live
/// beside the library rather than in the session because they follow the
/// person, not the project — the same set on another machine should not
/// drag someone else's taste in with it.
///
/// `key` comes from the caller's own fixed list, never from the page.
pub(crate) fn read_setting(key: &str) -> Option<String> {
    let path = data_directory()?.join(key);
    let metadata = std::fs::metadata(&path).ok()?;
    if !metadata.is_file() || metadata.len() > 64 {
        return None;
    }
    let value = std::fs::read_to_string(&path).ok()?.trim().to_owned();
    (!value.is_empty()).then_some(value)
}

/// Remembers a preference. The caller has already checked both the key and
/// the value against what it ships.
pub(crate) fn write_setting(key: &str, value: &str) -> bool {
    let Some(dir) = data_directory() else {
        return false;
    };
    if std::fs::create_dir_all(&dir).is_err() {
        return false;
    }
    let temp = dir.join(format!("{key}.tmp"));
    if std::fs::write(&temp, value.as_bytes()).is_err() {
        return false;
    }
    std::fs::rename(&temp, dir.join(key)).is_ok()
}

/// A saved curve.
pub(crate) struct Entry {
    pub(crate) name: String,
    pub(crate) curve: CustomCurve,
}

/// Every readable curve in the library, sorted by name.
///
/// Unreadable files are skipped rather than reported: a stray file in the
/// folder is not an error the user needs a dialog about.
pub(crate) fn list() -> Vec<Entry> {
    let Some(dir) = directory() else {
        return Vec::new();
    };
    let Ok(read) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut entries = Vec::new();
    for item in read.flatten() {
        if entries.len() == MAX_ENTRIES {
            break;
        }
        let path = item.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some(EXTENSION) {
            continue;
        }
        let Some(name) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        let Some(curve) = read_curve(&path) else {
            continue;
        };
        entries.push(Entry {
            name: name.to_owned(),
            curve,
        });
    }
    entries.sort_by_key(|entry| entry.name.to_lowercase());
    entries
}

fn read_curve(path: &Path) -> Option<CustomCurve> {
    let metadata = std::fs::metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > MAX_FILE_BYTES {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    let curve = CustomCurve::parse(&bytes);
    // `parse` is total — junk reads as the default curve — so a file that
    // does not round-trip is not a curve file, and listing it as one would
    // put a lie in the browser.
    (CustomCurve::parse(curve.to_wire().as_bytes()) == curve && curve == CustomCurve::parse(&bytes))
        .then_some(curve)
        .filter(|_| !bytes.is_empty())
}

/// Writes a curve, replacing one of the same name. Returns false if the
/// name is unusable or the write fails.
///
/// Written to a temporary file and renamed: a rename is atomic on every
/// platform we target, so a crash or a full disk mid-write leaves the
/// previous curve intact instead of half of a new one.
pub(crate) fn save(name: &str, curve: CustomCurve) -> bool {
    let Some(name) = sanitize(name) else {
        return false;
    };
    let Some(dir) = directory() else {
        return false;
    };
    if std::fs::create_dir_all(&dir).is_err() {
        return false;
    }
    let final_path = dir.join(format!("{name}.{EXTENSION}"));
    let temp_path = dir.join(format!("{name}.{EXTENSION}.tmp"));
    if std::fs::write(&temp_path, curve.to_wire().as_bytes()).is_err() {
        return false;
    }
    if std::fs::rename(&temp_path, &final_path).is_err() {
        let _ = std::fs::remove_file(&temp_path);
        return false;
    }
    true
}

/// Deletes a saved curve. Missing is success: the user wanted it gone.
pub(crate) fn delete(name: &str) -> bool {
    let Some(name) = sanitize(name) else {
        return false;
    };
    let Some(dir) = directory() else {
        return false;
    };
    match std::fs::remove_file(dir.join(format!("{name}.{EXTENSION}"))) {
        Ok(()) => true,
        Err(error) => error.kind() == std::io::ErrorKind::NotFound,
    }
}

/// Turns a name typed in a webview into a file name that cannot escape the
/// library directory.
///
/// This is the only place a plugin writes to a user's disk, and the name
/// comes from a text field, so it is an allowlist rather than a blocklist:
/// letters, digits, space, dash and underscore survive, everything else
/// becomes a dash. That takes `../../.bashrc`, a NUL, a Windows reserved
/// name and a 300-character title out of play in one rule.
fn sanitize(name: &str) -> Option<String> {
    let cleaned: String = name
        .trim()
        .chars()
        .take(48)
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, ' ' | '-' | '_') {
                character
            } else {
                '-'
            }
        })
        .collect();
    let cleaned = cleaned.trim().to_owned();
    // All-punctuation names collapse to dashes; a name of nothing but
    // separators is not a name.
    (!cleaned.is_empty() && cleaned.chars().any(|c| c.is_ascii_alphanumeric())).then_some(cleaned)
}

/// A throwaway library directory for tests, installed through the env
/// override above. Everything in this crate that touches the library shares
/// one process, so the guard serializes those tests as well as isolating
/// their files.
#[cfg(test)]
pub(crate) struct TempLibrary {
    path: PathBuf,
    _guard: std::sync::MutexGuard<'static, ()>,
}

#[cfg(test)]
static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
impl TempLibrary {
    pub(crate) fn new(tag: &str) -> Self {
        let guard = LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let path = std::env::temp_dir().join(format!("seco-zape-curves-{tag}"));
        let _ = std::fs::remove_dir_all(&path);
        // SAFETY: the guard above makes this the only thread touching the
        // environment for the duration.
        unsafe { std::env::set_var("SECO_ZAPE_DIR", &path) };
        Self {
            path,
            _guard: guard,
        }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(test)]
impl Drop for TempLibrary {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
        // SAFETY: as in `new`.
        unsafe { std::env::remove_var("SECO_ZAPE_DIR") };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drawn() -> CustomCurve {
        CustomCurve::parse(b"0,0.9,0.2,0.4,0.6,0.8,1,1,1,0.5,0.5,0.5,1,1,1,1")
    }

    #[test]
    fn a_curve_survives_save_and_list() {
        let _library = TempLibrary::new("round-trip");
        assert!(save("Kick 4x4", drawn()));
        let entries = list();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "Kick 4x4");
        assert_eq!(entries[0].curve, drawn());
        assert!(delete("Kick 4x4"));
        assert!(list().is_empty());
        // Deleting what is not there is what the user asked for anyway.
        assert!(delete("Kick 4x4"));
    }

    #[test]
    fn saving_the_same_name_replaces_it() {
        let _library = TempLibrary::new("replace");
        assert!(save("one", CustomCurve::default()));
        assert!(save("one", drawn()));
        let entries = list();
        assert_eq!(entries.len(), 1, "a second save must not leave two files");
        assert_eq!(entries[0].curve, drawn());
    }

    /// The name comes from a text field in a webview and becomes a path.
    #[test]
    fn names_cannot_escape_the_library() {
        let library = TempLibrary::new("escape");
        assert!(save("../../escaped", drawn()));
        assert!(save("with/slash", drawn()));
        assert!(save("nul\0byte", drawn()));

        // Everything landed inside the directory, with the separators gone.
        let names: Vec<String> = list().into_iter().map(|entry| entry.name).collect();
        assert_eq!(names.len(), 3, "{names:?}");
        assert!(
            names
                .iter()
                .all(|name| !name.contains('/') && !name.contains("..")),
            "{names:?}"
        );
        let files = std::fs::read_dir(directory().unwrap()).unwrap().count();
        assert_eq!(files, 3, "files escaped the library directory");

        // A name with nothing to keep is refused rather than turned into
        // some default file.
        assert!(!save("///", drawn()));
        assert!(!save("   ", drawn()));

        // A traversing delete only ever reaches inside the library: it
        // reports success because the sanitized name is simply not there,
        // and the file it was aiming at is untouched.
        let outside = library.path().join("seco-zape-bystander");
        std::fs::write(&outside, b"do not delete me").unwrap();
        delete("../../seco-zape-bystander");
        assert!(outside.exists(), "delete escaped the library directory");
        std::fs::remove_file(&outside).unwrap();
    }

    #[test]
    fn preferences_are_remembered_one_file_each() {
        let _library = TempLibrary::new("settings");
        assert_eq!(
            read_setting("skin"),
            None,
            "no file yet means no preference"
        );
        assert!(write_setting("skin", "tianguis"));
        assert_eq!(read_setting("skin").as_deref(), Some("tianguis"));
        assert!(write_setting("skin", "jukebox"));
        assert_eq!(read_setting("skin").as_deref(), Some("jukebox"));
        // A second preference does not disturb the first.
        assert!(write_setting("fx3d", "0"));
        assert_eq!(read_setting("fx3d").as_deref(), Some("0"));
        assert_eq!(read_setting("skin").as_deref(), Some("jukebox"));
        // And the curve library is oblivious to both.
        assert!(list().is_empty());
    }

    #[test]
    fn strays_in_the_folder_are_ignored() {
        let _library = TempLibrary::new("strays");
        std::fs::create_dir_all(directory().unwrap()).unwrap();
        std::fs::write(directory().unwrap().join("notes.txt"), b"not a curve").unwrap();
        std::fs::write(directory().unwrap().join("empty.zapecurve"), b"").unwrap();
        std::fs::write(directory().unwrap().join("junk.zapecurve"), b"hello there").unwrap();
        std::fs::write(
            directory().unwrap().join("huge.zapecurve"),
            vec![b'0'; 5000],
        )
        .unwrap();
        std::fs::create_dir_all(directory().unwrap().join("subdir.zapecurve")).unwrap();
        assert!(save("real", drawn()));

        let names: Vec<String> = list().into_iter().map(|entry| entry.name).collect();
        assert_eq!(
            names,
            vec!["real".to_string()],
            "a stray file was listed as a curve"
        );
    }
}
