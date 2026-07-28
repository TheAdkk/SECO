# Building and installing plugins

Prerequisites: stable Rust ≥ 1.85 (`rustup update stable`).

CLAP hosts search these locations (`reference/clap/include/clap/entry.h:12-32`),
plus any directories in the `CLAP_PATH` environment variable:

| OS | User install | System install |
|---|---|---|
| macOS | `~/Library/Audio/Plug-Ins/CLAP` | `/Library/Audio/Plug-Ins/CLAP` |
| Linux | `~/.clap` | `/usr/lib/clap` |
| Windows | `%LOCALAPPDATA%\Programs\Common\CLAP` | `%COMMONPROGRAMFILES%\CLAP` |

## Building

One command per artifact, on every platform:

```sh
cargo xtask bundle zape --release            # -> target/zape.clap
cargo xtask bundle zape --release --install  # ... and copy it where hosts look
```

Options: `--release` (default is debug), `--features <list>` (e.g. `gui`),
`--vst3`, `--install`. `cargo xtask help` prints them.

On macOS a `.clap` is a **bundle** (a directory with `Contents/MacOS/` and an
`Info.plist`), not a renamed dylib, and Apple Silicon refuses unsigned code —
xtask assembles the directory and ad-hoc-signs it (`codesign -s -`). On Linux
and Windows the artifact is the shared object under a `.clap` name.

The `Info.plist` is not written by hand: xtask loads the freshly built
binary the way a host does (`clap_entry` -> factory -> descriptor) and fills
`CFBundleIdentifier`, `CFBundleName` and the version from what the plugin
reports. `impl Plugin` stays the only place those strings are written.

Installing by hand instead of `--install` is a copy into the table above:

```sh
cp -R target/zape.clap ~/Library/Audio/Plug-Ins/CLAP/     # macOS
cp target/zape.clap ~/.clap/                              # Linux
copy target\zape.clap "%LOCALAPPDATA%\Programs\Common\CLAP\zape.clap"   :: Windows
```

## VST3 (for hosts without CLAP support, e.g. Ableton Live)

The `vst3` cargo feature (off by default) wraps the CLAP plugin as a VST3
via free-audio's clap-wrapper — C++ inside, embedded MIT-licensed VST 3 SDK;
see the README's "Formats, honestly" section. Neither seco-core nor
seco-clap participate: the wrapper re-hosts the `clap_entry` the plugin
already exports.

```sh
cargo xtask bundle zape --release --vst3 --install   # → target/zape.vst3
```

The flag implies the `vst3` cargo feature: a `.vst3` bundle around a binary
built without it is a bundle no host can load.

Validate headless with [pluginval](https://github.com/Tracktion/pluginval):
`pluginval --strictness-level 10 --validate target/zape.vst3`.

## Packaging it for someone else

```sh
cargo xtask dist zape
```

| host | archive | installer |
|---|---|---|
| macOS | `Zape-0.5.0-macOS.zip` | `Install.command` |
| Linux | `Zape-0.5.0-linux.tar.gz` | `install.sh` |
| Windows | `Zape-0.5.0-windows.zip` | `Install.bat` |

Builds both faces at release with the editor on and packages them with an
installer and a Read Me, for the platform it runs on — nothing here
cross-compiles. The name and version come from the plugin's own descriptor,
like everything else the bundle says.

### One layout per platform, and one of them was wrong

CLAP is a bundle directory on macOS and a plain shared object named `.clap`
everywhere else. VST3 is a bundle on **all three** — including Linux and
Windows, where the binary lives at `Contents/<arch>-linux/name.so` and
`Contents/<arch>-win/name.vst3`. The non-macOS path here used to write a
renamed `.so` for both, which is not a VST3 anywhere.

The target is a `Platform` value rather than a `cfg!`, so all three trees
can be assembled and checked from one machine — which is the only reason
any of the Linux or Windows work is tested at all. What that does not do is
run them: **the Linux and Windows installers have never executed on the
machines they are for.** The layouts, the folders they install into, the
line endings and the Read Me text are covered by tests; the scripts
themselves are unproven until someone runs them.

Two cargo builds rather than one, on purpose: the `vst3` feature links
clap-wrapper's C++ into the binary, and the CLAP that ships should be the
pure-Rust one this project claims it is. Verified on the packaged
artifacts — the `.clap` exports `clap_entry` and nothing else, the `.vst3`
exports `clap_entry` and `GetPluginFactory`.

### What the installer is actually for

Not copying files. macOS tags anything downloaded with
`com.apple.quarantine`, and a quarantined bundle that is not *notarized* is
refused by Gatekeeper — with the ad-hoc signature we do have, the failure
is exact and reproducible:

```
dlopen(.../zape.clap/Contents/MacOS/zape): code signature not valid for use
in process: library load disallowed by system policy
```

Notarization needs a paid Apple Developer ID. Without one, the installer's
job is to remove that flag from the copies it makes, which the user is
allowed to do on their own machine. The Read Me says exactly that rather
than calling it a formality: someone installing unsigned code deserves to
know that is what they are doing, and it points them at building from
source as the alternative.

The whole flow is verified by actually doing it — quarantine the zip the way
a browser would, extract it, confirm the extracted bundle refuses to load,
run the installer, confirm the installed copy loads and passes
clap-validator.

## Verifying without a DAW

[clap-validator](https://github.com/free-audio/clap-validator) (by the CLAP
authors) loads the plugin, drives the full lifecycle, and fuzzes `process()`:

```sh
cargo install --git https://github.com/free-audio/clap-validator.git --locked
clap-validator validate target/zape.clap
```

## Verifying in a DAW

The development host is **REAPER** (native CLAP support since v7). It scans
the standard macOS location above (`~/Library/Audio/Plug-Ins/CLAP`); after
installing a new build: Options → Preferences → Plug-ins → Re-scan. Insert
**Zape** on a track: the output should drop by 6 dB, nothing else.

Ableton Live does not support CLAP (as of 2026), which is why it is not the
development host. The planned route into Live is the VST3 wrapper phase; the
CLAP binary stays the single source.

Bitwig (demo) serves as a second host when cross-checking behavior
(Settings → Locations → rescan).
