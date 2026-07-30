//! Turns a built shared library into the artifact hosts scan for.
//!
//! The three platforms disagree about what a plugin *is*, and the
//! disagreement is not symmetric:
//!
//! - **CLAP** is a bundle directory on macOS (`entry.h:22-24`) and a plain
//!   shared object named `.clap` everywhere else.
//! - **VST3** is a bundle directory on *all three* — including Linux and
//!   Windows, where the binary lives under `Contents/<arch>-<os>/`. A
//!   renamed `.so` is not a VST3 on Linux, which is what this used to
//!   produce.
//!
//! The target is a value rather than a `cfg!`, so the layouts can be
//! assembled and checked on a machine that is not the one they are for.
//! That is the only way any of the Linux and Windows work here gets tested
//! at all.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::descriptor::Descriptor;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
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
}

/// Which set of conventions to build for. Always the host today — nothing
/// here cross-compiles — but naming it makes the layouts testable.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Platform {
    MacOs,
    Linux,
    Windows,
}

impl Platform {
    pub fn host() -> Self {
        if cfg!(target_os = "macos") {
            Platform::MacOs
        } else if cfg!(target_os = "windows") {
            Platform::Windows
        } else {
            Platform::Linux
        }
    }

    /// What cargo names the cdylib for `package`. The crate name is the
    /// package name with dashes turned into underscores.
    pub fn library_file_name(self, package: &str) -> String {
        let crate_name = package.replace('-', "_");
        match self {
            Platform::Windows => format!("{crate_name}.dll"),
            Platform::MacOs => format!("lib{crate_name}.dylib"),
            Platform::Linux => format!("lib{crate_name}.so"),
        }
    }

    /// Where the binary sits inside a bundle: `Contents/MacOS/name` on
    /// macOS, and an architecture-named directory elsewhere (VST 3 SDK's
    /// "Plug-in format": `Contents/x86_64-linux/name.so`,
    /// `Contents/x86_64-win/name.vst3`).
    ///
    /// The architecture is the host's, because that is what was just built.
    fn bundle_binary_path(self, name: &str) -> PathBuf {
        let arch = std::env::consts::ARCH;
        match self {
            Platform::MacOs => PathBuf::from("Contents/MacOS").join(name),
            Platform::Linux => {
                PathBuf::from(format!("Contents/{arch}-linux")).join(format!("{name}.so"))
            }
            Platform::Windows => {
                // Steinberg spells the 64-bit ARM slice "arm64-win", not
                // "aarch64-win".
                let arch = if arch == "aarch64" { "arm64" } else { arch };
                PathBuf::from(format!("Contents/{arch}-win")).join(format!("{name}.vst3"))
            }
        }
    }

    /// The per-user plug-in folder hosts scan (`entry.h:12-32` for CLAP, the
    /// VST 3 SDK's locations for VST3).
    fn user_dir(self, format: Format) -> Result<PathBuf, String> {
        Ok(match self {
            Platform::MacOs => {
                let home = PathBuf::from(std::env::var("HOME").map_err(|_| "HOME unset")?);
                match format {
                    Format::Clap => home.join("Library/Audio/Plug-Ins/CLAP"),
                    Format::Vst3 => home.join("Library/Audio/Plug-Ins/VST3"),
                }
            }
            Platform::Linux => {
                let home = PathBuf::from(std::env::var("HOME").map_err(|_| "HOME unset")?);
                match format {
                    Format::Clap => home.join(".clap"),
                    Format::Vst3 => home.join(".vst3"),
                }
            }
            Platform::Windows => {
                let local = std::env::var("LOCALAPPDATA").map_err(|_| "LOCALAPPDATA unset")?;
                let base = PathBuf::from(local).join("Programs/Common");
                match format {
                    Format::Clap => base.join("CLAP"),
                    Format::Vst3 => base.join("VST3"),
                }
            }
        })
    }
}

/// Assembles the artifact next to the built library and returns its path.
pub fn assemble(
    library: &Path,
    package: &str,
    descriptor: &Descriptor,
    format: Format,
    platform: Platform,
) -> Result<PathBuf, String> {
    let out_dir = library.parent().and_then(Path::parent).ok_or("unexpected target layout")?;
    let artifact = out_dir.join(format!("{package}.{}", format.extension()));
    assemble_at(library, &artifact, package, descriptor, format, platform)
}

/// The layout itself, with the destination given rather than derived — the
/// tests build all three trees this way.
pub fn assemble_at(
    library: &Path,
    artifact: &Path,
    package: &str,
    descriptor: &Descriptor,
    format: Format,
    platform: Platform,
) -> Result<PathBuf, String> {
    remove_existing(artifact)?;
    let bundled = platform == Platform::MacOs || format == Format::Vst3;
    if !bundled {
        // CLAP off macOS is the shared object under a different name.
        copy(library, artifact)?;
        return Ok(artifact.to_path_buf());
    }

    let binary = artifact.join(platform.bundle_binary_path(package));
    let parent = binary.parent().ok_or("bundle binary has no parent")?;
    std::fs::create_dir_all(parent)
        .map_err(|e| format!("could not create {}: {e}", parent.display()))?;
    copy(library, &binary)?;

    // The plist and the signature are macOS's alone: Linux and Windows read
    // the bundle's shape, not a manifest, and neither refuses unsigned code.
    if platform == Platform::MacOs {
        let plist = artifact.join("Contents/Info.plist");
        std::fs::write(&plist, info_plist(package, descriptor))
            .map_err(|e| format!("could not write {}: {e}", plist.display()))?;
        sign(artifact)?;
    }
    Ok(artifact.to_path_buf())
}

/// Ad-hoc signature: Apple Silicon refuses unsigned code. Nothing to do on
/// the other two.
///
/// Gated on the *host*, not on the target platform. `codesign` is a macOS tool,
/// so a Linux or Windows machine assembling a macOS layout cannot sign it and
/// must not fail trying — which is what `vst3_is_a_bundle_everywhere` and
/// `only_macos_gets_a_plist` do from any host, and what CI does on ubuntu. The
/// Mac that ships a bundle is the one that signs it, and `package.yml` builds
/// each platform on its own runner.
fn sign(artifact: &Path) -> Result<(), String> {
    if !cfg!(target_os = "macos") {
        return Ok(());
    }
    let status = Command::new("codesign")
        .args(["--force", "--sign", "-"])
        .arg(artifact)
        .status()
        .map_err(|e| format!("could not run codesign: {e}"))?;
    if status.success() { Ok(()) } else { Err("codesign failed".into()) }
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
pub fn install(artifact: &Path, format: Format, platform: Platform) -> Result<PathBuf, String> {
    let dir = platform.user_dir(format)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn descriptor() -> Descriptor {
        Descriptor {
            id: "dev.seco.zape".into(),
            name: "Zape".into(),
            version: "0.5.0".into(),
        }
    }

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!("seco-xtask-{tag}"));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        /// A stand-in for the built cdylib.
        fn library(&self) -> PathBuf {
            let path = self.0.join("libzape.so");
            std::fs::write(&path, b"not really a shared object").unwrap();
            path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Off macOS, a CLAP is the shared object under another name — not a
    /// directory. Hosts scan for the file itself.
    #[test]
    fn clap_off_macos_is_a_single_file() {
        for platform in [Platform::Linux, Platform::Windows] {
            let temp = TempDir::new(&format!("clap-{platform:?}"));
            let artifact = temp.0.join("zape.clap");
            assemble_at(
                &temp.library(),
                &artifact,
                "zape",
                &descriptor(),
                Format::Clap,
                platform,
            )
            .unwrap();
            assert!(artifact.is_file(), "{platform:?}: a CLAP here is a file");
        }
    }

    /// A VST3 is a bundle on every platform, and the binary sits in an
    /// architecture-named directory off macOS. This is the layout the old
    /// code got wrong: it wrote a renamed `.so`, which no Linux host loads.
    #[test]
    fn vst3_is_a_bundle_everywhere() {
        let arch = std::env::consts::ARCH;
        let windows_arch = if arch == "aarch64" { "arm64" } else { arch };
        let cases = [
            (Platform::MacOs, PathBuf::from("Contents/MacOS/zape")),
            (
                Platform::Linux,
                PathBuf::from(format!("Contents/{arch}-linux/zape.so")),
            ),
            (
                Platform::Windows,
                PathBuf::from(format!("Contents/{windows_arch}-win/zape.vst3")),
            ),
        ];
        for (platform, expected) in cases {
            let temp = TempDir::new(&format!("vst3-{platform:?}"));
            let artifact = temp.0.join("zape.vst3");
            assemble_at(
                &temp.library(),
                &artifact,
                "zape",
                &descriptor(),
                Format::Vst3,
                platform,
            )
            .unwrap();
            assert!(artifact.is_dir(), "{platform:?}: a VST3 is a bundle");
            assert!(
                artifact.join(&expected).is_file(),
                "{platform:?}: expected the binary at {}",
                expected.display()
            );
        }
    }

    /// Only macOS gets a plist, and it carries what the plugin itself
    /// reports rather than anything restated here.
    #[test]
    fn only_macos_gets_a_plist() {
        let temp = TempDir::new("plist");
        let artifact = temp.0.join("zape.clap");
        assemble_at(&temp.library(), &artifact, "zape", &descriptor(), Format::Clap, Platform::MacOs)
            .unwrap();
        let plist = std::fs::read_to_string(artifact.join("Contents/Info.plist")).unwrap();
        assert!(plist.contains("dev.seco.zape"));
        assert!(plist.contains("0.5.0"));

        let temp = TempDir::new("plist-linux");
        let artifact = temp.0.join("zape.vst3");
        assemble_at(&temp.library(), &artifact, "zape", &descriptor(), Format::Vst3, Platform::Linux)
            .unwrap();
        assert!(!artifact.join("Contents/Info.plist").exists());
    }

    /// The name cargo gives the built library, which xtask has to find
    /// before it can bundle anything.
    #[test]
    fn library_names_match_cargo() {
        assert_eq!(Platform::MacOs.library_file_name("zape"), "libzape.dylib");
        assert_eq!(Platform::Linux.library_file_name("zape"), "libzape.so");
        assert_eq!(Platform::Windows.library_file_name("zape"), "zape.dll");
        // Cargo turns dashes into underscores for the crate name.
        assert_eq!(Platform::Linux.library_file_name("my-plugin"), "libmy_plugin.so");
    }

    /// A `&` in a plugin name would otherwise produce a plist macOS refuses
    /// to parse — and plugin names are free-form Rust strings.
    #[test]
    fn plist_escapes_plugin_names() {
        let descriptor = Descriptor {
            id: "dev.seco.a&b".into(),
            name: "A & B <test>".into(),
            version: "1.0".into(),
        };
        let plist = info_plist("zape", &descriptor);
        assert!(plist.contains("A &amp; B &lt;test&gt;"));
        assert!(!plist.contains("A & B"));
    }
}
