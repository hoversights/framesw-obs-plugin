# FrameSW Companion Plugin for OBS Studio

A small native OBS Studio plugin that reports real, accurate audio levels
for every source OBS knows about — including sources that are only staged
in Preview and not yet live, which OBS's own `InputVolumeMeters`
WebSocket event cannot report at all (it only reports for sources OBS
considers "active," which excludes Preview-only content under Studio
Mode).

This plugin is the companion connector for
[FrameSW](https://framesw.com), a live-switcher application that adds a
Preview-before-Live workflow on top of OBS. FrameSW itself is proprietary
and talks to OBS over the standard [obs-websocket](https://github.com/obsproject/obs-websocket)
protocol; this plugin is the one piece that has to run *inside* OBS's own
process, because the audio data it needs isn't reachable any other way.
It's independently useful to anyone who wants accurate Preview-only audio
metering over obs-websocket, not just FrameSW.

## How it works

The plugin attaches a native audio capture callback
(`obs_source_add_audio_capture_callback`) to every audio-capable source
and to OBS's currently active Program/Preview scenes, computes a real,
post-fader peak level for each (matching OBS's own mixer meter — capture
callbacks receive audio *before* the fader is applied, so this plugin
applies the source's current volume and mute state itself, the same way
OBS's own `obs_volmeter` does), and forwards a batched update about 10
times a second over obs-websocket's vendor-event mechanism (registering
as vendor `framesw`). If obs-websocket isn't installed, the plugin still
loads and logs locally — nothing here requires FrameSW or obs-websocket
to be present to load cleanly.

### Monitor-speaker audio taps

The same audio capture callback also backs three vendor *requests* —
`start_audio_tap`/`stop_audio_tap`/`tap_status` — that let FrameSW ask
for a chosen source's real audio to be forwarded out as an audio-only NDI
sender (named `FrameSW-Monitor-{bus_id}`), so an operator can listen to
content that's only staged in Preview (or any layer, live or not)
through their own headphones/monitor speaker. This exists specifically
because OBS's own audio monitoring device only ever reflects the
Program/output mix — it has no concept of monitoring Preview-only
content — and it's a **read-only tap**: the plugin only ever copies the
same samples it already reads for metering into a separate NDI send
buffer, never touching OBS's own routing, so it's structurally incapable
of affecting what's actually streamed or recorded. Requires the [NDI
Runtime](https://ndi.video/tools/) to be installed (FrameSW bundles it);
without it, taps simply produce no audio — the rest of the plugin is
unaffected.

## Requirements

- **OBS Studio 30.2 or newer**.
- **obs-websocket** (bundled with OBS Studio by default since 28.0) if
  you want the audio-levels data actually forwarded anywhere — the
  plugin loads and runs without it, it just has nowhere to send data.
- **Rust** (stable toolchain) to build from source. No C/C++ toolchain,
  no libobs SDK headers, and no bindgen step are needed — every libobs
  and obs-websocket function this plugin calls is resolved at runtime
  (`dlsym` on macOS, `GetProcAddress` + module enumeration on Windows)
  against whatever's already loaded in the OBS process, not linked at
  build time.

## Building

**macOS:**

```sh
./package-macos.sh
```

Builds a `.plugin` bundle at `target/framesw-companion.plugin` for the
current architecture, ad-hoc signed. Pass `--release "<Developer ID
Application: ...>"` for a signed, universal (arm64 + x86_64) release
build instead.

**Windows:**

```powershell
powershell -ExecutionPolicy Bypass -File package-windows.ps1
```

Builds `target\framesw-companion\bin\64bit\framesw-companion.dll`.

## Installing

Copy the built plugin into OBS's plugin directory, then fully quit and
relaunch OBS Studio:

| Platform | Location |
|---|---|
| macOS | `~/Library/Application Support/obs-studio/plugins/framesw-companion.plugin` |
| Windows | `%ProgramData%\obs-studio\plugins\framesw-companion\bin\64bit\framesw-companion.dll` (or the flat `<OBS install dir>\obs-plugins\64bit\` layout, depending on your OBS install method) |

Check OBS's own log (Help → Log Files → View Current Log) for a line
starting with `[framesw]` to confirm it loaded.

## License

Copyright (C) 2026 Hoversights.

This program is free software: you can redistribute it and/or modify
it under the terms of the GNU General Public License as published by
the Free Software Foundation, either version 2 of the License, or
(at your option) any later version — the same licence as OBS Studio
itself.

See [LICENSE](LICENSE) for the full text. Every source file carries an
`SPDX-License-Identifier: GPL-2.0-or-later` header and `Cargo.toml`
declares the same, so source, package metadata and licence file all
agree.

### Licence and derivative-work boundary

This plugin is GPL. **The FrameSW application is not, and is not a
derivative work of it or of OBS.** The reasoning, stated plainly so it
can be checked rather than taken on trust:

- The plugin runs **inside** OBS's process and calls libobs, so it is
  unambiguously bound by OBS's GPL. That is why it is GPL, and why its
  source is here in full.
- FrameSW is a **separate program in a separate process**. It links no
  OBS code, includes no OBS headers, and is not linked against this
  plugin. The two communicate only over
  [obs-websocket](https://github.com/obsproject/obs-websocket)'s
  documented network protocol — the same public, arms-length interface
  used by Bitfocus Companion, Streamer.bot, Stream Deck integrations and
  every other third-party OBS controller, commercial ones included.
- The vendor requests this plugin exposes are part of that same
  obs-websocket surface. They are not a private linkage channel, and any
  obs-websocket client can call them.

If you want to check that boundary, inspect the process and linkage
model rather than this paragraph. Nothing in this repository is built
into, or linked against, the FrameSW application binary.

### Third-party licences

The dependency tree is deliberately tiny, and is empty on macOS.

| Dependency | Platform | Licence |
|---|---|---|
| `windows-sys`, `windows-targets`, and the eight `windows_*` target crates | Windows only | `MIT OR Apache-2.0` |

Those are Microsoft's own `windows-rs` bindings, used for `platform.rs`'s
symbol-resolution fallback (Windows has no `RTLD_DEFAULT` equivalent).
They are dual-licensed and **this project elects the MIT option**, which
is GPL-2.0 compatible. Apache-2.0 alone is *not* compatible with GPL-2.0,
so the election is a real decision and is stated here deliberately rather
than left for a reviewer to work out.

macOS builds and all cross-platform logic use only the Rust standard
library and raw FFI — no third-party crates at all.

**No OBS source is vendored or redistributed here.** Every libobs and
obs-websocket symbol is resolved at runtime against whatever OBS already
has loaded (`dlsym` on macOS, `GetProcAddress` plus module enumeration on
Windows), so there are no SDK headers, no generated bindings, and no
copied OBS code in this repository.
