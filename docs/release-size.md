# Release executable size

Measured on 24 November 2026 with Rust 1.100.0, macOS ARM64, and the same
application features and language parsers in every build. This comparison
predates the native image/video port. Sizes below are
the executable produced by Cargo, before the bundle script signs its copy.

| Build | Bytes | MiB |
| --- | ---: | ---: |
| Previous release: opt-level 3, thin LTO, unwinding, unstripped | 30,401,264 | 28.99 |
| Previous release with local symbols stripped using `strip -x` | 25,709,456 | 24.52 |
| Full LTO, abort on panic, stripped, opt-level `s` | 17,004,064 | 16.22 |
| Full LTO, abort on panic, stripped, opt-level `z` **(selected)** | 14,027,424 | 13.38 |

The selected build is 53.9% smaller than the previous release. The release
profile in the workspace `Cargo.toml` applies size optimization, full LTO,
one codegen unit, no debug information, symbol stripping, and abort-on-panic.
The native UI, editors and syntax parsers, image decoders, PTY terminal,
QuickJS extension host, and external icon-theme support remain enabled.

Build with `cargo build --release` or package it with `cargo dist`. To repeat
the other optimizer comparison, run:

```sh
CARGO_PROFILE_RELEASE_OPT_LEVEL=s cargo build -p dotdir --release
```

Run the ordinary release build again afterwards to restore the selected `z`
configuration. Full LTO increases build time, and size optimization can trade
peak throughput for smaller code; these measurements are not a performance
benchmark. No executable compression was used and no dependency forks were
introduced for this reduction.

Release Rust panics terminate the process rather than unwinding. Ordinary
application errors and JavaScript exceptions still use their existing error
handling. Development and test profiles keep unwinding.

Validation: all 46 workspace tests pass. The selected release was launched
and checked for directory navigation, Material icons, Rust source editing,
typing and scrolling, undo back to clean state, Unicode save verified on disk,
QuickJS panel rendering and refresh interaction, PTY shell output, binary hex
viewing, and PNG image viewing. The ad-hoc macOS signature verifies.

After adding the native image/video port, the same `z` profile produces
14,094,288 bytes (13.44 MiB) before bundle signing, or 14,030,480 bytes
(13.38 MiB) in the signed app. The port adds 66,864 bytes (65.3 KiB) to the
Cargo executable. Playback uses system AVKit/CoreMedia frameworks, with no
bundled codec library. All 49 current workspace tests pass; native media
interaction checks are recorded in `validation.md`.
