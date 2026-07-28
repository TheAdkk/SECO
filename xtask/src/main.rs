//! SECO's build tasks. Run through the alias in `.cargo/config.toml`:
//!
//! ```text
//! cargo xtask bundle zape [--release] [--features gui] [--vst3] [--install]
//! cargo xtask dist zape
//! ```
//!
//! It builds the plugin crate, then assembles the artifact each platform's
//! hosts actually scan for: a bundle directory on macOS, a renamed shared
//! object elsewhere. Everything the bundle says about the plugin — bundle
//! identifier, display name, version — is read back out of the built binary
//! (`descriptor`), so no build script restates what `impl Plugin` already
//! declares.

mod bundle;
mod descriptor;
mod dist;

use std::path::PathBuf;
use std::process::{Command, ExitCode};

use bundle::Format;

const USAGE: &str = "\
usage: cargo xtask bundle <package> [options]
       cargo xtask dist <package>

bundle options:
  --release            optimized build (default: debug)
  --features <list>    comma- or space-separated cargo features
  --vst3               emit a .vst3 bundle (implies the vst3 feature)
  --install            copy the result into the user plug-in folder

dist builds the release CLAP and VST3 with the editor, and zips them with
an installer for someone else's machine. macOS only so far.
";

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("xtask: {message}");
            ExitCode::FAILURE
        }
    }
}

struct Args {
    package: String,
    release: bool,
    features: Vec<String>,
    format: Format,
    install: bool,
}

fn run() -> Result<(), String> {
    let mut argv = std::env::args().skip(1);
    let command = argv.next();
    match command.as_deref() {
        Some("dist") => {
            let package = argv.next().ok_or_else(|| format!("missing package\n\n{USAGE}"))?;
            if argv.next().is_some() {
                return Err(format!("dist takes only a package name\n\n{USAGE}"));
            }
            return dist_command(&package);
        }
        Some("bundle") => {}
        Some("help" | "-h" | "--help") => {
            println!("{USAGE}");
            return Ok(());
        }
        Some(other) => return Err(format!("unknown command `{other}`\n\n{USAGE}")),
        None => return Err(format!("missing command\n\n{USAGE}")),
    }
    let args = parse_bundle_args(argv)?;
    bundle_command(&args)
}

fn parse_bundle_args(argv: impl Iterator<Item = String>) -> Result<Args, String> {
    let mut package = None;
    let mut release = false;
    let mut features = Vec::new();
    let mut format = Format::Clap;
    let mut install = false;

    let mut argv = argv.peekable();
    while let Some(arg) = argv.next() {
        match arg.as_str() {
            "--release" => release = true,
            "--vst3" => format = Format::Vst3,
            "--install" => install = true,
            "--features" => {
                let list = argv.next().ok_or("--features needs a value")?;
                features.extend(
                    list.split([',', ' ']).filter(|f| !f.is_empty()).map(str::to_owned),
                );
            }
            other if other.starts_with('-') => {
                return Err(format!("unknown option `{other}`\n\n{USAGE}"));
            }
            other => {
                if package.replace(other.to_owned()).is_some() {
                    return Err(format!("more than one package given\n\n{USAGE}"));
                }
            }
        }
    }
    // The vst3 face comes from the clap-wrapper feature; asking for the
    // bundle and forgetting the feature produced a .vst3 no host could load,
    // so the flag implies it.
    if format == Format::Vst3 && !features.iter().any(|f| f == "vst3") {
        features.push("vst3".to_owned());
    }
    Ok(Args {
        package: package.ok_or_else(|| format!("missing package\n\n{USAGE}"))?,
        release,
        features,
        format,
        install,
    })
}

/// Builds a plugin and assembles one artifact from it.
fn build_artifact(args: &Args) -> Result<(PathBuf, descriptor::Descriptor), String> {
    cargo_build(args)?;

    let profile = if args.release { "release" } else { "debug" };
    let library = target_dir()?.join(profile).join(library_file_name(&args.package));
    if !library.exists() {
        return Err(format!("built library not found: {}", library.display()));
    }

    let descriptor = descriptor::read(&library)?;
    let artifact = bundle::assemble(&library, &args.package, &descriptor, args.format)?;
    Ok((artifact, descriptor))
}

fn bundle_command(args: &Args) -> Result<(), String> {
    let (artifact, _) = build_artifact(args)?;
    println!("built {}", artifact.display());

    if args.install {
        let installed = bundle::install(&artifact, args.format)?;
        println!("installed {}", installed.display());
    } else {
        println!("install with: cargo xtask bundle {} ... --install", args.package);
    }
    Ok(())
}

/// Builds both faces at release with the editor on, then packages them.
///
/// Two cargo builds rather than one, deliberately: the vst3 feature links
/// clap-wrapper's C++ into the binary, and the CLAP that ships should be the
/// pure-Rust one the README claims it is. The same dylib would have worked
/// for both faces and would have made that sentence false.
fn dist_command(package: &str) -> Result<(), String> {
    if !cfg!(target_os = "macos") {
        return Err("dist only builds macOS packages so far".into());
    }
    let editor_only = Args {
        package: package.to_owned(),
        release: true,
        features: vec!["gui".to_owned()],
        format: Format::Clap,
        install: false,
    };
    let (clap_bundle, descriptor) = build_artifact(&editor_only)?;

    let with_wrapper = Args {
        features: vec!["gui".to_owned(), "vst3".to_owned()],
        format: Format::Vst3,
        ..editor_only
    };
    let (vst3_bundle, _) = build_artifact(&with_wrapper)?;

    let dist_dir = target_dir()?.join("dist");
    std::fs::create_dir_all(&dist_dir)
        .map_err(|e| format!("could not create {}: {e}", dist_dir.display()))?;
    let archive =
        dist::package(package, &clap_bundle, &vst3_bundle, &descriptor, &dist_dir)?;
    println!("packaged {}", archive.display());
    Ok(())
}

fn cargo_build(args: &Args) -> Result<(), String> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
    let mut command = Command::new(cargo);
    command.args(["build", "-p", &args.package]);
    if args.release {
        command.arg("--release");
    }
    if !args.features.is_empty() {
        command.args(["--features", &args.features.join(",")]);
    }
    let status = command.status().map_err(|e| format!("could not run cargo: {e}"))?;
    if status.success() { Ok(()) } else { Err("cargo build failed".into()) }
}

/// The workspace's target directory. `CARGO_TARGET_DIR` wins, as it does for
/// cargo itself.
fn target_dir() -> Result<PathBuf, String> {
    if let Ok(dir) = std::env::var("CARGO_TARGET_DIR") {
        return Ok(PathBuf::from(dir));
    }
    let manifest = std::env::var("CARGO_MANIFEST_DIR")
        .map_err(|_| "CARGO_MANIFEST_DIR unset — run through `cargo xtask`")?;
    let workspace = PathBuf::from(manifest)
        .parent()
        .ok_or("xtask manifest has no parent directory")?
        .to_path_buf();
    Ok(workspace.join("target"))
}

/// What cargo names the cdylib for `package`. The crate name is the package
/// name with dashes turned into underscores.
fn library_file_name(package: &str) -> String {
    let crate_name = package.replace('-', "_");
    if cfg!(target_os = "windows") {
        format!("{crate_name}.dll")
    } else if cfg!(target_os = "macos") {
        format!("lib{crate_name}.dylib")
    } else {
        format!("lib{crate_name}.so")
    }
}
