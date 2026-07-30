# SECO patch set

Base: `clap-wrapper` 0.3.1, upstream revision
`5818f931f9dbb34adff7495d8b26c8e5bd5f1f44`.

## Monotonic VST3 timer

`external/clap-wrapper/src/detail/os/macos.mm` and `linux.cpp` use
`std::chrono::steady_clock` and milliseconds for `getTickInMS()`.

Version 0.3.1 used process CPU time there. VST3 idle callbacks then advanced
slowly or irregularly while a DAW waited, causing editor-meter stutter.
Windows already uses `GetTickCount64()` and remains unchanged.

The package keeps upstream `LICENSE-MIT` and `LICENSE-APACHE`; bundled third
party license files remain in place.
