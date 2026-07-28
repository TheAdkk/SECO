//! Packages a plugin for someone else's machine.
//!
//! # What this has to work around
//!
//! Notarization needs a paid Apple Developer ID, and this project does not
//! have one. That is not the obstacle people assume it is, but it is not
//! nothing either, and the difference is worth stating precisely because it
//! decides what the installer does.
//!
//! The bundles are ad-hoc signed (`codesign -s -`), which is what Apple
//! Silicon requires before a host may load them at all. What ad-hoc signing
//! does *not* do is satisfy Gatekeeper for code that arrives from the
//! internet: macOS tags every downloaded file with `com.apple.quarantine`,
//! and a quarantined bundle that is not notarized is refused — the host
//! reports a code signature error, or simply does not list the plugin.
//!
//! So the installer's real job is not copying files. It is removing that
//! flag from the copies it makes, which the user is allowed to do to their
//! own machine and Apple has never taken away. The Read Me says so in as
//! many words rather than pretending the step is a formality: someone
//! installing unsigned code deserves to know that is what they are doing.
//!
//! macOS only, for now. Linux and Windows have no equivalent problem and no
//! equivalent script; when they get built here they will be a different and
//! much shorter function.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::bundle;
use crate::descriptor::Descriptor;

/// Assembles the folder, writes the installer and zips the lot. Returns the
/// archive.
pub fn package(
    package_name: &str,
    clap_bundle: &Path,
    vst3_bundle: &Path,
    descriptor: &Descriptor,
    dist_dir: &Path,
) -> Result<PathBuf, String> {
    let folder_name = format!("{} {}", descriptor.name, descriptor.version);
    let staging = dist_dir.join(&folder_name);
    bundle::remove_existing(&staging)?;
    std::fs::create_dir_all(&staging)
        .map_err(|e| format!("could not create {}: {e}", staging.display()))?;

    for artifact in [clap_bundle, vst3_bundle] {
        let name = artifact.file_name().ok_or("bundle has no file name")?;
        bundle::copy_dir(artifact, &staging.join(name))?;
    }

    let clap_name = file_name(clap_bundle)?;
    let vst3_name = file_name(vst3_bundle)?;
    write_script(&staging.join("Install.command"), descriptor, &clap_name, &vst3_name)?;
    std::fs::write(
        staging.join("Read Me.txt"),
        read_me(package_name, descriptor, &clap_name, &vst3_name),
    )
    .map_err(|e| format!("could not write the read me: {e}"))?;

    let archive = dist_dir.join(format!(
        "{}-{}-macOS.zip",
        descriptor.name.replace(' ', "-"),
        descriptor.version
    ));
    bundle::remove_existing(&archive)?;
    // ditto rather than zip: it keeps the bundle's symlinks and extended
    // attributes, and a code signature that survives the round trip is the
    // whole point of shipping the thing.
    let status = Command::new("ditto")
        .args(["-c", "-k", "--sequesterRsrc", "--keepParent"])
        .arg(&staging)
        .arg(&archive)
        .status()
        .map_err(|e| format!("could not run ditto: {e}"))?;
    if !status.success() {
        return Err("ditto failed to build the archive".into());
    }
    Ok(archive)
}

fn file_name(path: &Path) -> Result<String, String> {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(str::to_owned)
        .ok_or_else(|| format!("unusable bundle name: {}", path.display()))
}

/// The double-clickable installer.
fn write_script(
    path: &Path,
    descriptor: &Descriptor,
    clap_name: &str,
    vst3_name: &str,
) -> Result<(), String> {
    let name = &descriptor.name;
    let script = format!(
        r#"#!/bin/bash
# {name} installer.
#
# Copies the plug-ins into your own plug-in folders — no administrator
# password, nothing outside your home directory — and clears the quarantine
# flag macOS puts on anything that arrives from the internet. Without that
# last step the plug-ins are installed and every host refuses to load them.
set -euo pipefail
cd "$(dirname "$0")"

CLAP_DIR="$HOME/Library/Audio/Plug-Ins/CLAP"
VST3_DIR="$HOME/Library/Audio/Plug-Ins/VST3"

install_bundle() {{
  local bundle="$1" dest="$2"
  if [ ! -d "$bundle" ]; then
    return 0
  fi
  mkdir -p "$dest"
  rm -rf "${{dest:?}}/$bundle"
  ditto "$bundle" "$dest/$bundle"
  xattr -dr com.apple.quarantine "$dest/$bundle" 2>/dev/null || true
  echo "  $dest/$bundle"
}}

echo "Installing {name}..."
install_bundle "{clap_name}" "$CLAP_DIR"
install_bundle "{vst3_name}" "$VST3_DIR"
echo
echo "Done. Rescan plug-ins in your DAW:"
echo "  Ableton Live: Preferences -> Plug-Ins -> Rescan"
echo "  REAPER:       Preferences -> Plug-ins -> Re-scan"
echo
# `|| true`: `read` fails at end-of-file, and under `set -e` that would turn
# a finished install into a script that reports failure — which is exactly
# what happens when this is run from a terminal rather than double-clicked.
read -n 1 -s -r -p "Press any key to close." || true
echo
"#
    );
    std::fs::write(path, script).map_err(|e| format!("could not write the installer: {e}"))?;
    make_executable(path)
}

fn make_executable(path: &Path) -> Result<(), String> {
    let status = Command::new("chmod")
        .arg("+x")
        .arg(path)
        .status()
        .map_err(|e| format!("could not run chmod: {e}"))?;
    if status.success() { Ok(()) } else { Err("chmod failed on the installer".into()) }
}

fn read_me(
    package_name: &str,
    descriptor: &Descriptor,
    clap_name: &str,
    vst3_name: &str,
) -> String {
    let Descriptor { name, version, id } = descriptor;
    format!(
        "\
{name} {version}
by SECO

WHAT IS IN HERE
  {clap_name}          the CLAP version — REAPER, Bitwig, Studio One
  {vst3_name}          the VST3 version — Ableton Live and anything without CLAP
  Install.command   double-click this

INSTALLING
  Double-click Install.command.

  The first time, macOS will refuse and say it is from an unidentified
  developer. Right-click it instead, choose Open, then Open again. That is
  a one-time thing for this file.

  It installs into your own folders and never asks for a password:
    ~/Library/Audio/Plug-Ins/CLAP/{clap_name}
    ~/Library/Audio/Plug-Ins/VST3/{vst3_name}

  Then rescan plug-ins in your DAW.

WHY THE WARNING
  Apple charges 99 USD a year for the certificate that makes that warning
  go away. This plugin is free and does not have one, so macOS treats it
  the way it treats anything it cannot trace to a paying developer.

  The plug-ins are signed, just not by an identity Apple sold. What the
  installer does about it is one line: it removes the `com.apple.quarantine`
  flag from the copies it just made, which is the flag macOS puts on
  anything downloaded. Without that, the files install fine and every host
  refuses to load them.

  You are installing unsigned code from the internet. That is a real thing
  to be careful about, and you should be — the source is public, and you
  can build it yourself instead:
      cargo xtask bundle {package_name} --release --features gui --install

DOING IT BY HAND
  If you would rather not run a script, copy the two bundles into the
  folders listed above and then run:
      xattr -dr com.apple.quarantine ~/Library/Audio/Plug-Ins/CLAP/{clap_name}
      xattr -dr com.apple.quarantine ~/Library/Audio/Plug-Ins/VST3/{vst3_name}

UNINSTALLING
  Delete those two folders. {name} keeps its saved curves and preferences in
      ~/Library/Application Support/SECO/{name}
  which you can delete too, or leave for next time.

  Nothing is installed anywhere else. No background process, no login item,
  no receipts.

PLUGIN IDENTIFIER
  {id}
"
    )
}
