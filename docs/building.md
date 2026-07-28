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
