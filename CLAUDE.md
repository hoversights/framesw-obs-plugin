# CLAUDE.md — session guidance for this repo

This is the **FrameSW Companion Plugin**: a GPL-2.0 native OBS Studio
plugin (Rust, no C shim) that reports real post-fader audio levels for
every source — including Preview-only content that OBS's own
`InputVolumeMeters` event structurally cannot report — over
obs-websocket's vendor-event mechanism, registered as vendor `framesw`.
README.md has the full user-facing story; keep the two in sync.

## Build / package

- **macOS**: `./package-macos.sh` (dev build: current arch, debug,
  ad-hoc signed) or `./package-macos.sh --release "<Developer ID
  Application: ...>"` (universal arm64+x86_64, release, signed).
  Output: `target/framesw-companion.plugin`.
- **Windows**: `powershell -ExecutionPolicy Bypass -File
  package-windows.ps1`. Output:
  `target\framesw-companion\bin\64bit\framesw-companion.dll`.
- Plain `cargo build` works for compile checks; there is no libobs SDK,
  no bindgen — every libobs/obs-websocket symbol is resolved at runtime
  (`src/platform.rs`), so the build needs only stable Rust.
- `package-windows.ps1` must stay **pure ASCII**: Windows PowerShell 5.1
  reads BOM-less UTF-8 as ANSI, and an em-dash decodes to a smart quote
  that silently terminates a string (this has broken the script twice).

## Invariants — do not break these

1. **Every `extern "C"` entry point stays wrapped in
   `ffi_guard`/`catch_unwind`** (`src/lib.rs`) — module lifecycle
   exports and every callback handed to libobs alike. A Rust panic
   unwinding into OBS's C frames is undefined behavior inside a user's
   live-streaming process. Any new export or callback gets the same
   wrapper before it ships. Corollary of the same principle: a missing
   runtime symbol degrades gracefully (feature off, host untouched) —
   never unwrap a resolved function.
2. **The vendor event contract is frozen without a versioned bump.**
   Consumers depend on exactly: vendor `framesw`, event `audio_levels`,
   payload `{"levels": [{"name": string, "peak_db": number (post-fader,
   -100.0 = silence/muted), "active": bool}]}`, emitted ~10x/second.
   Changing names, types, semantics, or cadence breaks apps built
   against it. Additive-only changes go behind new fields; anything
   else requires an explicit protocol version (e.g. a new event name or
   a version field) negotiated before release.

FFI hygiene when touching `src/`: every FFI declaration here was ported
from verbatim-fetched obs-studio/obs-websocket headers, with provenance
noted in comments — keep that standard for any new declaration (fetch
the real header, cite it, match the ABI exactly; never guess).
