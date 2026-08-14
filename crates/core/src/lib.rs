// SPDX-License-Identifier: GPL-2.0-or-later
//! Plumbing shared by every plugin built from this repo.
//!
//! Split out so the community metering plugin
//! (`hoversights/obs-studio-mode-meters`) and FrameSW's own companion
//! plugin build on one copy of the FFI-critical code rather than two that
//! drift. Everything here is deliberately free of FrameSW specifics: no
//! vendor name, no log prefix, no scene names. A consumer supplies those.
//!
//! Nothing in this crate reads or writes OBS state beyond what it is handed
//! — that property is what lets the public plugin be described honestly as
//! read-only.

pub mod calldata;
pub mod obs_data;
pub mod platform;
