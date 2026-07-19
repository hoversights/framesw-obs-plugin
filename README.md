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

This program is free software: you can redistribute it and/or modify
it under the terms of the GNU General Public License as published by
the Free Software Foundation, either version 2 of the License, or
(at your option) any later version.

See [LICENSE](LICENSE) for the full text.
