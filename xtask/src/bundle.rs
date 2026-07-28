//! Turns a built shared library into the artifact hosts scan for.
//!
//! macOS wants a *bundle*: a directory with `Contents/MacOS/<executable>`
//! and an `Info.plist` (`reference/clap/include/clap/entry.h:22-24`); a
//! renamed dylib is never picked up. Linux and Windows want the plain shared
//! object under a `.clap` name.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::descriptor::Descriptor;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Clap,
    /// The same binary presented through clap-wrapper's VST3 entry points;
    /// only the bundle extension tells hosts which face to load.
    Vst3,
}

impl Format {
    fn extension(self) -> &'static str {
        match self {
            Format::Clap => "clap",
            Format::Vst3 => "vst3",
        }
    }

    /// The per-user plug-in folder hosts scan (`entry.h:12-32`).
    fn user_dir(self) -> Result<PathBuf, String> {
        let home = PathBuf::from(std::env::var("HOME").map_err(|_| "HOME unset")?);
        Ok(match (cfg!(target_os = "macos"), self) {
            (true, Format::Clap) => home.join("Library/Audio/Plug-Ins/CLAP"),
            (true, Format::Vst3) => home.join("Library/Audio/Plug-Ins/VST3"),
            (false, Format::Clap) => home.join(".clap"),
            (false, Format::Vst3) => home.join(".vst3"),
        })
    }
}

/// Assembles the artifact next to the built library and returns its path.
pub fn assemble(
    library: &Path,
    package: &str,
    descriptor: &Descriptor,
    format: Format,
) -> Result<PathBuf, String> {
    let out_dir = library.parent().and_then(Path::parent).ok_or("unexpected target layout")?;
    let artifact = out_dir.join(format!("{package}.{}", format.extension()));

    if cfg!(target_os = "macos") {
        assemble_macos(library, &artifact, package, descriptor)?;
    } else {
        // A renamed shared object; nothing to describe.
        remove_existing(&artifact)?;
        copy(library, &artifact)?;
    }
    Ok(artifact)
}

fn assemble_macos(
    library: &Path,
    artifact: &Path,
    package: &str,
    descriptor: &Descriptor,
) -> Result<(), String> {
    remove_existing(artifact)?;
    let macos_dir = artifact.join("Contents/MacOS");
    std::fs::create_dir_all(&macos_dir)
        .map_err(|e| format!("could not create {}: {e}", macos_dir.display()))?;
    copy(library, &macos_dir.join(package))?;

    let plist = artifact.join("Contents/Info.plist");
    std::fs::write(&plist, info_plist(package, descriptor))
        .map_err(|e| format!("could not write {}: {e}", plist.display()))?;

    // Ad-hoc signature: Apple Silicon refuses unsigned code.
    let status = Command::new("codesign")
        .args(["--force", "--sign", "-"])
        .arg(artifact)
        .status()
        .map_err(|e| format!("could not run codesign: {e}"))?;
    if !status.success() {
        return Err("codesign failed".into());
    }
    Ok(())
}

/// The plist, filled from the descriptor the binary itself reports —
/// `CFBundleIdentifier` is `Plugin::ID`, the version is `Plugin::VERSION`,
/// and neither can drift from what the plugin tells hosts.
fn info_plist(executable: &str, descriptor: &Descriptor) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleExecutable</key>
	<string>{executable}</string>
	<key>CFBundleIdentifier</key>
	<string>{id}</string>
	<key>CFBundleName</key>
	<string>{name}</string>
	<key>CFBundlePackageType</key>
	<string>BNDL</string>
	<key>CFBundleShortVersionString</key>
	<string>{version}</string>
	<key>CFBundleVersion</key>
	<string>{version}</string>
</dict>
</plist>
"#,
        executable = xml(executable),
        id = xml(&descriptor.id),
        name = xml(&descriptor.name),
        version = xml(&descriptor.version),
    )
}

/// Plugin constants are free-form Rust strings; a `&` in a vendor or plugin
/// name would otherwise produce a plist macOS refuses to parse.
fn xml(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// Copies the artifact into the user plug-in folder, replacing any previous
/// install.
pub fn install(artifact: &Path, format: Format) -> Result<PathBuf, String> {
    let dir = format.user_dir()?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("could not create {}: {e}", dir.display()))?;
    let name = artifact.file_name().ok_or("artifact has no file name")?;
    let target = dir.join(name);
    remove_existing(&target)?;
    if artifact.is_dir() {
        copy_dir(artifact, &target)?;
    } else {
        copy(artifact, &target)?;
    }
    Ok(target)
}

pub fn copy_dir(from: &Path, to: &Path) -> Result<(), String> {
    std::fs::create_dir_all(to).map_err(|e| format!("could not create {}: {e}", to.display()))?;
    let entries =
        std::fs::read_dir(from).map_err(|e| format!("could not read {}: {e}", from.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("could not read {}: {e}", from.display()))?;
        let target = to.join(entry.file_name());
        if entry.path().is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            copy(&entry.path(), &target)?;
        }
    }
    Ok(())
}

fn copy(from: &Path, to: &Path) -> Result<(), String> {
    std::fs::copy(from, to)
        .map(|_| ())
        .map_err(|e| format!("could not copy {} to {}: {e}", from.display(), to.display()))
}

/// Removes a previous artifact, file or bundle directory. Missing is fine.
pub fn remove_existing(path: &Path) -> Result<(), String> {
    let result = if path.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    };
    match result {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("could not remove {}: {e}", path.display())),
    }
}
