# Building and installing zape

Prerequisites: stable Rust ≥ 1.85 (`rustup update stable`).

CLAP hosts search these locations (`reference/clap/include/clap/entry.h:12-32`),
plus any directories in the `CLAP_PATH` environment variable:

| OS | User install | System install |
|---|---|---|
| macOS | `~/Library/Audio/Plug-Ins/CLAP` | `/Library/Audio/Plug-Ins/CLAP` |
| Linux | `~/.clap` | `/usr/lib/clap` |
| Windows | `%LOCALAPPDATA%\Programs\Common\CLAP` | `%COMMONPROGRAMFILES%\CLAP` |

## macOS

On macOS a `.clap` is a **bundle** (a directory with `Contents/MacOS/` and an
`Info.plist`), not a renamed dylib. The script builds and assembles it:

```sh
./scripts/bundle-macos.sh            # release build → target/zape.clap
mkdir -p ~/Library/Audio/Plug-Ins/CLAP
cp -R target/zape.clap ~/Library/Audio/Plug-Ins/CLAP/
```

The script ad-hoc-signs the bundle (`codesign -s -`); Apple Silicon refuses
unsigned code.

## Linux

A `.clap` is a renamed shared object:

```sh
cargo build -p zape --release
mkdir -p ~/.clap
cp target/release/libzape.so ~/.clap/zape.clap
```

## Windows

A `.clap` is a renamed DLL:

```bat
cargo build -p zape --release
copy target\release\zape.dll "%LOCALAPPDATA%\Programs\Common\CLAP\zape.clap"
```

## VST3 (for hosts without CLAP support, e.g. Ableton Live)

The `vst3` cargo feature (off by default) wraps the CLAP plugin as a VST3
via free-audio's clap-wrapper — C++ inside, embedded MIT-licensed VST 3 SDK;
see the README's "Formats, honestly" section. Neither seco-core nor
seco-clap participate: the wrapper re-hosts the `clap_entry` the plugin
already exports.

```sh
./scripts/bundle-macos.sh vst3      # → target/zape.vst3
cp -R target/zape.vst3 ~/Library/Audio/Plug-Ins/VST3/
```

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
