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
//! Linux and Windows have no equivalent problem — no Gatekeeper, no
//! quarantine on a plug-in — so their installers only copy, and their Read
//! Me only says where. The asymmetry is the point: the macOS script is long
//! because macOS made it long.
//!
//! Everything here is generated from a `Platform` value rather than a
//! `cfg!`, so the Linux and Windows scripts can be produced and inspected
//! from a Mac. What that does *not* do is run them: the two of them have
//! never executed on the machines they are for, and this file should stop
//! saying so only once they have.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::bundle::{self, Platform};
use crate::descriptor::Descriptor;

/// Assembles the folder, writes the installer and zips the lot. Returns the
/// archive.
pub fn package(
    package_name: &str,
    clap_bundle: &Path,
    vst3_bundle: &Path,
    descriptor: &Descriptor,
    dist_dir: &Path,
    platform: Platform,
) -> Result<PathBuf, String> {
    let folder_name = format!("{} {}", descriptor.name, descriptor.version);
    let staging = dist_dir.join(&folder_name);
    bundle::remove_existing(&staging)?;
    std::fs::create_dir_all(&staging)
        .map_err(|e| format!("could not create {}: {e}", staging.display()))?;

    for artifact in [clap_bundle, vst3_bundle] {
        let name = artifact.file_name().ok_or("bundle has no file name")?;
        let target = staging.join(name);
        // A CLAP off macOS is a single file; everything else is a bundle.
        if artifact.is_dir() {
            bundle::copy_dir(artifact, &target)?;
        } else {
            std::fs::copy(artifact, &target)
                .map(|_| ())
                .map_err(|e| format!("could not copy {}: {e}", artifact.display()))?;
        }
    }

    let names = Names { clap: file_name(clap_bundle)?, vst3: file_name(vst3_bundle)? };
    let script_name = match platform {
        Platform::MacOs => "Install.command",
        Platform::Linux => "install.sh",
        Platform::Windows => "Install.bat",
    };
    write_script(&staging.join(script_name), descriptor, &names, platform)?;
    std::fs::write(
        staging.join("Read Me.txt"),
        read_me(package_name, descriptor, &names, platform),
    )
    .map_err(|e| format!("could not write the read me: {e}"))?;

    let stem = format!("{}-{}", descriptor.name.replace(' ', "-"), descriptor.version);
    let archive = dist_dir.join(match platform {
        Platform::MacOs => format!("{stem}-macOS.zip"),
        Platform::Linux => format!("{stem}-linux.tar.gz"),
        Platform::Windows => format!("{stem}-windows.zip"),
    });
    bundle::remove_existing(&archive)?;
    compress(&staging, &archive, platform)?;
    Ok(archive)
}

/// Bundle file names, which differ per platform and are needed in three
/// places each.
struct Names {
    clap: String,
    vst3: String,
}

/// Archives the staging folder.
///
/// One tool per platform, each chosen for a reason rather than for being
/// available: `ditto` keeps macOS's extended attributes and the code
/// signature that rides in them, `tar` keeps the executable bit that a
/// shell script needs, and `Compress-Archive` is the one thing guaranteed
/// to exist on a Windows box without installing anything.
fn compress(staging: &Path, archive: &Path, platform: Platform) -> Result<(), String> {
    let parent = staging.parent().ok_or("staging folder has no parent")?;
    let folder = staging.file_name().ok_or("staging folder has no name")?;
    let mut command = match platform {
        Platform::MacOs => {
            let mut c = Command::new("ditto");
            c.args(["-c", "-k", "--sequesterRsrc", "--keepParent"]).arg(staging).arg(archive);
            c
        }
        Platform::Linux => {
            let mut c = Command::new("tar");
            c.arg("-czf").arg(archive).arg("-C").arg(parent).arg(folder);
            c
        }
        Platform::Windows => {
            let mut c = Command::new("powershell");
            c.args(["-NoProfile", "-Command", "Compress-Archive", "-Path"])
                .arg(staging)
                .args(["-DestinationPath"])
                .arg(archive);
            c
        }
    };
    let status = command.status().map_err(|e| format!("could not run the archiver: {e}"))?;
    if status.success() { Ok(()) } else { Err("the archiver failed".into()) }
}

fn file_name(path: &Path) -> Result<String, String> {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(str::to_owned)
        .ok_or_else(|| format!("unusable bundle name: {}", path.display()))
}

/// The installer the archive ships with.
fn write_script(
    path: &Path,
    descriptor: &Descriptor,
    names: &Names,
    platform: Platform,
) -> Result<(), String> {
    let script = match platform {
        Platform::MacOs => macos_script(descriptor, names),
        Platform::Linux => linux_script(descriptor, names),
        Platform::Windows => windows_script(descriptor, names),
    };
    std::fs::write(path, script).map_err(|e| format!("could not write the installer: {e}"))?;
    if platform == Platform::Windows {
        // A .bat needs no execute bit, and chmod is not there to give it one.
        return Ok(());
    }
    make_executable(path)
}

/// macOS: copy, then clear the quarantine flag. Without that last step the
/// plug-ins install fine and every host refuses to load them.
fn macos_script(descriptor: &Descriptor, names: &Names) -> String {
    let name = &descriptor.name;
    let (clap_name, vst3_name) = (&names.clap, &names.vst3);
    format!(
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
  if [ ! -e "$bundle" ]; then
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
    )
}

/// Linux: copy, and that is the whole job. Nothing here refuses to load
/// code because of where it came from.
fn linux_script(descriptor: &Descriptor, names: &Names) -> String {
    let name = &descriptor.name;
    let (clap_name, vst3_name) = (&names.clap, &names.vst3);
    format!(
        r#"#!/bin/sh
# {name} installer. Copies the plug-ins into your own plug-in folders.
set -eu
cd "$(dirname "$0")"

CLAP_DIR="$HOME/.clap"
VST3_DIR="$HOME/.vst3"

install_plugin() {{
  bundle="$1"
  dest="$2"
  [ -e "$bundle" ] || return 0
  mkdir -p "$dest"
  rm -rf "$dest/$bundle"
  cp -R "$bundle" "$dest/$bundle"
  echo "  $dest/$bundle"
}}

echo "Installing {name}..."
install_plugin "{clap_name}" "$CLAP_DIR"
install_plugin "{vst3_name}" "$VST3_DIR"
echo
echo "Done. Rescan plug-ins in your DAW."
"#
    )
}

/// Windows: copy into the per-user plug-in folders, which need no
/// administrator and no elevation prompt.
fn windows_script(descriptor: &Descriptor, names: &Names) -> String {
    let name = &descriptor.name;
    let (clap_name, vst3_name) = (&names.clap, &names.vst3);
    // CRLF: a .bat with bare newlines runs, until the day it does not.
    let script = format!(
        r#"@echo off
rem {name} installer. Copies the plug-ins into your own plug-in folders.
setlocal
cd /d "%~dp0"

set "CLAP_DIR=%LOCALAPPDATA%\Programs\Common\CLAP"
set "VST3_DIR=%LOCALAPPDATA%\Programs\Common\VST3"

echo Installing {name}...
if not exist "%CLAP_DIR%" mkdir "%CLAP_DIR%"
if not exist "%VST3_DIR%" mkdir "%VST3_DIR%"

rem The CLAP is a single file here; the VST3 is a folder.
if exist "{clap_name}" copy /y "{clap_name}" "%CLAP_DIR%\{clap_name}" >nul
if exist "{vst3_name}" (
  if exist "%VST3_DIR%\{vst3_name}" rmdir /s /q "%VST3_DIR%\{vst3_name}"
  xcopy /e /i /q /y "{vst3_name}" "%VST3_DIR%\{vst3_name}" >nul
)

echo   %CLAP_DIR%\{clap_name}
echo   %VST3_DIR%\{vst3_name}
echo.
echo Done. Rescan plug-ins in your DAW.
pause
"#
    );
    script.replace('\n', "\r\n")
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
    names: &Names,
    platform: Platform,
) -> String {
    let Descriptor { name, version, id } = descriptor;
    let (clap_name, vst3_name) = (&names.clap, &names.vst3);
    let installing = match platform {
        Platform::MacOs => format!(
            "\
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
  If you would rather not run a script, copy the two plug-ins into the
  folders listed above and then run:
      xattr -dr com.apple.quarantine ~/Library/Audio/Plug-Ins/CLAP/{clap_name}
      xattr -dr com.apple.quarantine ~/Library/Audio/Plug-Ins/VST3/{vst3_name}
"
        ),
        Platform::Linux => format!(
            "\
INSTALLING
  Run ./install.sh

  It installs into your own folders and never asks for a password:
    ~/.clap/{clap_name}
    ~/.vst3/{vst3_name}

  Then rescan plug-ins in your DAW.

DOING IT BY HAND
  Copy the two plug-ins into the folders listed above. That is the whole
  install — nothing here refuses to load code because of where it came
  from, so there is no flag to clear and no warning to click through.

  Or build it yourself:
      cargo xtask bundle {package_name} --release --features gui --install
"
        ),
        Platform::Windows => format!(
            "\
INSTALLING
  Double-click Install.bat.

  Windows may show a blue \"Windows protected your PC\" box, because this
  is not signed with a certificate bought from a certificate authority.
  Click More info, then Run anyway.

  It installs into your own folders and never asks for administrator
  rights:
    %LOCALAPPDATA%\\Programs\\Common\\CLAP\\{clap_name}
    %LOCALAPPDATA%\\Programs\\Common\\VST3\\{vst3_name}

  Then rescan plug-ins in your DAW.

DOING IT BY HAND
  Copy the two plug-ins into the folders listed above. If Windows marked
  the download, right-click the zip before extracting, choose Properties,
  and tick Unblock.

  Or build it yourself:
      cargo xtask bundle {package_name} --release --features gui --install
"
        ),
    };

    let uninstalling = match platform {
        Platform::MacOs => format!(
            "  ~/Library/Audio/Plug-Ins/CLAP/{clap_name}\n               ~/Library/Audio/Plug-Ins/VST3/{vst3_name}\n\n               {name} keeps its saved curves and preferences in\n                   ~/Library/Application Support/SECO/{name}"
        ),
        Platform::Linux => format!(
            "  ~/.clap/{clap_name}\n  ~/.vst3/{vst3_name}\n\n               {name} keeps its saved curves and preferences in\n                   ~/.config/seco/{name_lower}",
            name_lower = name.to_lowercase()
        ),
        Platform::Windows => format!(
            "  %LOCALAPPDATA%\\Programs\\Common\\CLAP\\{clap_name}\n               %LOCALAPPDATA%\\Programs\\Common\\VST3\\{vst3_name}\n\n               {name} keeps its saved curves and preferences in\n                   %APPDATA%\\SECO\\{name}"
        ),
    };

    format!(
        "\
{name} {version}
by SECO

WHAT IS IN HERE
  {clap_name}
      the CLAP version — REAPER, Bitwig, Studio One
  {vst3_name}
      the VST3 version — Ableton Live and anything without CLAP

{installing}
UNINSTALLING
  Delete these:
{uninstalling}

  which you can delete too, or leave for next time.

  Nothing is installed anywhere else. No background process, no login item,
  no receipts.

PLUGIN IDENTIFIER
  {id}
"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn descriptor() -> Descriptor {
        Descriptor { id: "dev.seco.zape".into(), name: "Zape".into(), version: "0.5.0".into() }
    }

    fn names() -> Names {
        Names { clap: "zape.clap".into(), vst3: "zape.vst3".into() }
    }

    /// Each installer has to name the folders its own platform actually
    /// scans. Getting this wrong installs successfully into nowhere.
    #[test]
    fn each_installer_targets_its_own_plugin_folders() {
        let macos = macos_script(&descriptor(), &names());
        assert!(macos.contains("Library/Audio/Plug-Ins/CLAP"));
        assert!(macos.contains("Library/Audio/Plug-Ins/VST3"));

        let linux = linux_script(&descriptor(), &names());
        assert!(linux.contains("$HOME/.clap"));
        assert!(linux.contains("$HOME/.vst3"));

        let windows = windows_script(&descriptor(), &names());
        assert!(windows.contains(r"%LOCALAPPDATA%\Programs\Common\CLAP"));
        assert!(windows.contains(r"%LOCALAPPDATA%\Programs\Common\VST3"));
    }

    /// The quarantine strip is the entire reason the macOS installer exists,
    /// and it is meaningless on the other two.
    #[test]
    fn only_macos_clears_quarantine() {
        assert!(macos_script(&descriptor(), &names())
            .contains("xattr -dr com.apple.quarantine"));
        assert!(!linux_script(&descriptor(), &names()).contains("xattr"));
        assert!(!windows_script(&descriptor(), &names()).contains("xattr"));
    }

    /// A .bat with bare newlines runs, until the day it does not.
    #[test]
    fn the_batch_file_has_windows_line_endings() {
        let windows = windows_script(&descriptor(), &names());
        assert!(windows.contains("\r\n"));
        assert!(!windows.replace("\r\n", "").contains('\n'), "a bare newline survived");

        // And the shell scripts must not: `#!/bin/sh\r` is a shell that
        // does not exist.
        assert!(!macos_script(&descriptor(), &names()).contains('\r'));
        assert!(!linux_script(&descriptor(), &names()).contains('\r'));
    }

    /// Both shells stop on the first failure. Copying half a plugin and
    /// reporting success is worse than failing.
    #[test]
    fn the_shell_installers_stop_on_error() {
        assert!(macos_script(&descriptor(), &names()).contains("set -euo pipefail"));
        assert!(linux_script(&descriptor(), &names()).contains("set -eu"));
    }

    /// The whole packaging path for a platform that is not this one. Only
    /// possible because the layouts are a value: this runs on a Mac and
    /// builds the Linux archive, which is the only kind of proof available
    /// here short of a second machine.
    #[test]
    fn a_linux_package_can_be_built_from_here() {
        let temp = std::env::temp_dir().join("seco-xtask-linux-package");
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).unwrap();

        // A CLAP is a file on Linux; a VST3 is a bundle.
        let clap = temp.join("zape.clap");
        std::fs::write(&clap, b"shared object").unwrap();
        let vst3 = temp.join("zape.vst3");
        std::fs::create_dir_all(vst3.join("Contents/x86_64-linux")).unwrap();
        std::fs::write(vst3.join("Contents/x86_64-linux/zape.so"), b"shared object").unwrap();

        let dist = temp.join("dist");
        std::fs::create_dir_all(&dist).unwrap();
        let archive =
            package("zape", &clap, &vst3, &descriptor(), &dist, Platform::Linux).unwrap();

        assert!(archive.exists());
        assert_eq!(archive.file_name().unwrap(), "Zape-0.5.0-linux.tar.gz");
        let listing = std::process::Command::new("tar")
            .arg("-tzf")
            .arg(&archive)
            .output()
            .unwrap();
        let listing = String::from_utf8_lossy(&listing.stdout);
        for expected in [
            "Zape 0.5.0/install.sh",
            "Zape 0.5.0/Read Me.txt",
            "Zape 0.5.0/zape.clap",
            "Zape 0.5.0/zape.vst3/Contents/x86_64-linux/zape.so",
        ] {
            assert!(listing.contains(expected), "missing {expected} in\n{listing}");
        }
        let _ = std::fs::remove_dir_all(&temp);
    }

    /// The Read Me tells the truth about the platform it ships on: only
    /// macOS has the Gatekeeper story, and every platform names its own
    /// preferences folder for uninstalling.
    #[test]
    fn each_read_me_describes_its_own_platform() {
        let macos = read_me("zape", &descriptor(), &names(), Platform::MacOs);
        assert!(macos.contains("99 USD"), "the macOS read me owes an explanation");
        assert!(macos.contains("Library/Application Support/SECO/Zape"));

        let linux = read_me("zape", &descriptor(), &names(), Platform::Linux);
        assert!(!linux.contains("99 USD"), "Linux has no such problem to explain");
        assert!(linux.contains(".config/seco/zape"));

        let windows =
            read_me("zape", &descriptor(), &names(), Platform::Windows);
        assert!(windows.contains("Windows protected your PC"));
        assert!(windows.contains(r"%APPDATA%\SECO\Zape"));
    }
}
