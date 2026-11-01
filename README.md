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

## Vendor requests — the complete list

The plugin registers as obs-websocket vendor `framesw` and exposes 36
requests. Any obs-websocket client can call them; they are not private to
FrameSW.

**Eleven of them change your OBS, three more stream from it, and two more
record files from it.** They are listed first and marked, because if you
install this from a plugin directory you should know that before you do,
not after.

### Requests that modify OBS state

| Request | What it does |
|---|---|
| `create_scene` | Creates a scene by name. Idempotent — succeeds if it already exists. |
| `ensure_profile` | Creates an OBS **Profile** if missing and switches to it. Runs on OBS's UI thread deliberately: doing this over obs-websocket's pooled thread raced `config_save_safe` and left a half-written `basic.ini`. |
| `projector_on_top` | Toggles OBS's "projectors always on top" setting, which lives in `user.ini`. Exposed here because obs-websocket has no request for it and the file cannot be safely edited on disk while OBS is running. |
| `ndi_outputs` | Reads and sets DistroAV's Main/Preview NDI output switches, also in `user.ini`, for the same reason. No effect if DistroAV is not installed. |
| `set_scale_filters` | Sets OBS's own **Scale Filtering** on the items of one named `scene`: the items whose ids are listed in `lanczos` (comma-separated) get Lanczos, every other item of that scene goes back to OBS's default. FrameSW sends it for its 9:16 scenes, for a picture drawn larger than its own pixels. Runs on OBS's UI thread. Exposed here because obs-websocket has no request for an item's scale filter. |
| `monitoring_device` | Reads, or with `name` and `id` sets, OBS's **audio monitoring device** (Settings → Audio → Monitoring Device), applied at once the way OBS's own Settings dialog applies it. Always returns the list of devices. Exposed here because obs-websocket's `SetProfileParameter` writes the setting without re-opening the monitoring output. |
| `renderer` | Reads, or with `renderer` sets, OBS's **renderer** (Settings → Advanced → Video → Renderer: "Metal", "OpenGL", "Direct3D 11"), which OBS uses from its next start. Returns the value as it stands after the write. Exposed here because `global.ini` can't be edited from outside while OBS runs: OBS rewrites it at exit. |
| `mono` | Reads, or with `mono` sets, one input's **Downmix to Mono** (Advanced Audio Properties), read back after the change. obs-websocket has no request for it. |
| `main_window` | Hides (`show: false`) or shows and brings to the front (`show: true`) **OBS's main window**. Projectors and dialogs keep their own visibility. FrameSW uses it to run OBS out of sight. |
| `set_source_showing` | Makes a `source` that no scene shows render anyway (`showing: true`), the way a projector window does, without opening a window; `false` lets it go. FrameSW uses it for its browser-render check: OBS's browser source paints nothing while nothing shows it. |
| `warm_browser_engine` | Creates a private browser source on OBS's UI thread and releases it at once, which starts OBS's browser engine so the next browser source paints straight away. Nothing it makes is kept. |

### Requests that stream

These **send a stream over the network**, beside OBS's own: a second
picture laid out for a phone (1080×1920 unless asked otherwise), encoded on
its own and pushed to an RTMP server. They don't change OBS's settings, its
scenes or its own stream. The stream key goes into the stream's service and
nowhere else: never logged, never returned.

| Request | What it does |
|---|---|
| `start_portrait_out` | Starts a portrait stream to `server` with `key`, of a named `scene`, or with `follow_program` of `FrameSW A · Portrait` / `FrameSW B · Portrait` as FrameSW's Program changes, through a transition of OBS's own kind. Several run at once, one per `id`. |
| `start_site_out` | One more site on encoders that are already running: `share` names a running portrait stream's `id`, or `obs` for OBS's own stream (which must be live). It has a connection of its own and no encoder, so it costs upload and no second encode. It stops with what it shares. |
| `stop_portrait_out` | Stops the stream or the site with that `id`. |
| `portrait_out_status` | Whether it is live or reconnecting, frames sent and dropped, bytes sent, and its last error. Read-only. |

### Requests that record

These **write video files** beside OBS's own recording: one source per
file, its picture at its own size and its sound from one of OBS's six audio
tracks, encoded on their own. They don't change OBS's settings, its scenes
or its own recording. Which sources are on a track is set with
obs-websocket's own `SetInputAudioTracks`; OBS mixes a track only from
sources on Program.

| Request | What it does |
|---|---|
| `start_iso` | Records one `source` to `path` now, with its sound from audio `track` (1–6, default 2), with the encoder OBS's own recording uses, at OBS's "Indistinguishable quality". Optional: `max_height` (the picture scaled down to it), `smaller` (a smaller file at a quality a little under that), `quality` (the encoder's own number), `video_encoder`, `audio_encoder`. One file per `id`. Stops when OBS's recording stops, at a scene collection change and when OBS quits. |
| `arm_iso` | The same, from OBS's next recording on (or at once, if OBS is recording): when OBS's recording starts, whoever starts it, the source is recorded beside OBS's file, named after it with `name` added (`2026-10-31 17-30-00 PiP 1.mov` beside `2026-10-31 17-30-00.mov`), and stops as it stops. An `id` armed again is replaced. |
| `disarm_isos` | Nothing is armed any more. Files already recording go on to the end of OBS's recording. |
| `stop_iso` | Stops every file being recorded. |

### Requests that send monitor pictures

These copy small pictures of what OBS is showing into shared memory on the
same computer, for FrameSW's own Preview and Program monitors. Nothing
leaves the machine, and nothing in OBS's scenes, settings or outputs is
changed: each picture is rendered in a small mix of its own.

| Request | What it does |
|---|---|
| `start_video_feed` | Sets which pictures are sent: OBS's `preview` and `program` at `width`×`height` (640×360 unless asked otherwise) and, with `portrait`, the two scenes `FrameSW A · Portrait` and `FrameSW B · Portrait` at `portrait_width`×`portrait_height` (540×960), each laid out in `portrait_base_width`×`portrait_base_height` pixels (1080×1920). A picture not asked for is stopped; one already running with the same settings is left alone. A portrait scene is found by name and followed as FrameSW makes and removes it. |
| `stop_video_feed` | Stops them all. |
| `video_feed_status` | Which are running, their sizes and frame counters. Read-only. |

### Read-only requests

| Request | What it does |
|---|---|
| `get_current_scenes` | Returns the current Program and Preview scene names. Exists because obs-websocket's own `GetCurrentProgramScene` calls `strlen()` on a null pointer and **crashes OBS** when there is no current scene — which is exactly the state left behind when the Program scene is deleted. Filed upstream as obsproject/obs-websocket#1349. |
| `list_video_outputs` | Returns the output list. Same motivation: obs-websocket's `GetOutputList` reads output dimensions through pointers `obs_reset_video` has freed, taking OBS down with a SIGSEGV. |
| `tap_status` | Whether a given audio tap bus is active. |
| `mix_status` | Whether a mix bus is active, and which sources it carries. |
| `list_devices` | The choices of a source kind's device list (cameras, microphones, NDI sources: `kind` and `property`), read from a private source that is never added to a scene. |
| `stream_quality` | OBS's stream as the plugin samples it ten times a second (jitter, stalls, peak against mean congestion, which a 2-second average hides), and the configured bitrate, which obs-websocket can't read in Advanced output mode. |
| `probe_input` | Listens to one input's sound for `window_ms` (300–15000) and reports its level, its loudness, its quiet floor and how much of it was silence, as the `input_probe_result` event. Device inputs only. |

### Audio monitoring

These forward a **copy** of audio the plugin already reads for metering out
to an NDI sender. They never touch OBS's own routing, so they cannot alter
what is streamed or recorded.

| Request | What it does |
|---|---|
| `start_audio_tap` / `stop_audio_tap` | Start/stop forwarding one source's audio to an NDI sender named `FrameSW-Monitor-{bus_id}`. |
| `set_mix_sources` / `stop_mix_bus` | Same, for a mixed bus of several sources. |

### Plugin-internal

These affect only this plugin's own bookkeeping, never OBS.

| Request | What it does |
|---|---|
| `rescan_now` | Attach audio callbacks immediately rather than waiting for the periodic pass, so a just-created input meters straight away. |
| `pause_rescan` / `resume_rescan` | Suspend the periodic attach pass while a client is recreating its own scenes. |

## Requirements

- **OBS Studio.** The module declares compatibility with **30.0 and
  newer** (`obs_module_ver`), which is deliberately conservative: every
  libobs call it makes has been stable API for years, and a module that
  claims a version *newer* than its host is hard-rejected at load. In
  practice it has been **run and verified against 32.1.2 and 32.2.x
  only** — anything between 30.0 and 32.1 should work but is untested, so
  please report rather than assume. On macOS note that OBS 32.2.0+ itself
  requires macOS 13; on macOS 12 the correct OBS build is 32.1.2.
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

## Reporting problems

Open an issue on this repository's GitHub tracker. Include your OBS
version, your platform, and the `[framesw]` lines from OBS's own log
(Help -> Log Files -> View Current Log) -- the plugin logs the reason for
every failure it can detect, so those lines usually identify the cause on
their own.

## Development disclosure

This plugin was written with substantial use of AI coding tools (Claude).
They were used for implementation, refactoring and test-writing
throughout. Direction, testing and release engineering are the
maintainer's; much of the implementation was AI-assisted.

It is actively maintained. Every release is built, signed and tested
against real OBS installations on macOS and Windows before it ships, and
reported bugs are triaged and answered. The complete source is
GPL-2.0-or-later and public here, every vendor request the plugin
registers is documented above -- including each one that modifies OBS
state -- and the commit history records what changed and why.

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

### NDI

The monitor-audio tap sends over NDI. This repository contains no NDI SDK
code and redistributes no NDI runtime: `ndi_ffi.rs` resolves a handful of
`NDIlib_*` symbols at runtime from whatever NDI Runtime is already
installed on the machine (FrameSW bundles one; a system install works
too), exactly as it does for libobs.

NDI® is a registered trademark of Vizrt NDI AB — see
[ndi.video](https://ndi.video/).

**No OBS source is vendored or redistributed here.** Every libobs and
obs-websocket symbol is resolved at runtime against whatever OBS already
has loaded (`dlsym` on macOS, `GetProcAddress` plus module enumeration on
Windows), so there are no SDK headers, no generated bindings, and no
copied OBS code in this repository.
