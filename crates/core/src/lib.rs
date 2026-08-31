// SPDX-License-Identifier: GPL-2.0-or-later
//! Plumbing for the plugins built from this repo.
//!
//! Deliberately free of FrameSW specifics: no vendor name, no log prefix,
//! no scene names. A consumer supplies those via `set_identity`.
//!
//! # A second copy of this code exists — fix bugs in both
//!
//! `hoversights/obs-studio-mode-meters` used to consume this crate by git
//! revision, pinned at `a32dcac`. It no longer does. On 2026-08-30 that
//! plugin took its own copy and cut the dependency, because a community
//! plugin strangers load into their own OBS should not need this
//! repository in order to build, or move on FrameSW's release schedule.
//!
//! The copies are deliberately **not** identical. Theirs is a subset:
//! everything serving a vendor request, an NDI audio tap, or an OBS config
//! write was removed there, and each removal left a comment where the code
//! was. But the FFI declarations, the refcount conventions and the metering
//! callback are shared ancestry, and a mistake in any of them crashes OBS
//! itself rather than just the plugin.
//!
//! **So: a fix to anything FFI-critical here almost certainly applies over
//! there too, and vice versa.** There is no tooling that will remind you.
//! The same warning is at the top of the other copy.
//!
//! Nothing in this crate reads or writes OBS state beyond what it is handed
//! — that property is what let the public plugin be described honestly as
//! read-only, and it is worth keeping true.

pub mod calldata;
pub mod obs_data;
pub mod metering;
pub mod platform;

use std::ffi::{c_char, c_int, CString};
use std::sync::OnceLock;

/// Who is using this crate — supplied once, at module load.
///
/// Exists because everything user-visible in here used to be hardcoded to
/// FrameSW. A community plugin writing `[framesw]` into a stranger's OBS
/// log, or claiming the `framesw` obs-websocket vendor name, would be
/// indefensible — and the vendor name in particular is a public API
/// contract that clients match on.
#[derive(Debug, Clone, Copy)]
pub struct Identity {
    /// obs-websocket vendor name. Clients call requests and match events
    /// against this string, so it is permanent once published.
    pub vendor: &'static str,
    /// Bracketed tag on every log line, e.g. `[studio-mode-meters]`.
    pub log_prefix: &'static str,
}

static IDENTITY: OnceLock<Identity> = OnceLock::new();

/// Registers the consumer's identity. First call wins.
///
/// Deliberately infallible and silent on a second call: this runs inside
/// `obs_module_load`, where returning an error or panicking would take OBS
/// down with it. A duplicate call means a programming error in the
/// consumer, not a condition a user can hit.
pub fn set_identity(identity: Identity) {
    let _ = IDENTITY.set(identity);
}

/// The registered identity, or a neutral fallback.
///
/// The fallback exists so a logging call that somehow beats `set_identity`
/// still produces a line rather than vanishing — a missing log during
/// module load is exactly when you need it most.
pub fn identity() -> Identity {
    *IDENTITY.get().unwrap_or(&Identity {
        vendor: "obs-plugin",
        log_prefix: "[obs-plugin]",
    })
}

const LOG_INFO: c_int = 300;

resolved_fn!(blog: extern "C" fn(c_int, *const c_char, ...));

/// Writes one line to OBS's own log, tagged with the consumer's prefix.
///
/// Silently does nothing if `blog` cannot be resolved — a plugin that
/// cannot log must still run, and there is nowhere to report the failure
/// to anyway.
pub fn log_line(msg: &str) {
    let Some(blog) = blog() else {
        return;
    };
    let Ok(fmt) = CString::new(format!("{} %s", identity().log_prefix)) else {
        return;
    };
    let msg = CString::new(msg).unwrap_or_else(|_| CString::new("[unprintable log line]").unwrap());
    blog(LOG_INFO, fmt.as_ptr(), msg.as_ptr());
}

#[cfg(test)]
mod identity_tests {
    use super::*;

    #[test]
    fn falls_back_to_a_neutral_name_when_unset() {
        // Whichever test runs first may set it; only the shape matters.
        let id = identity();
        assert!(!id.vendor.is_empty() && id.log_prefix.starts_with('['));
    }

    #[test]
    fn the_fallback_is_not_framesw() {
        // The whole point of this module: nothing in core may default to
        // the consuming product's name.
        let id = identity();
        assert!(!id.vendor.contains("framesw"), "core must not default to FrameSW's vendor name");
        assert!(!id.log_prefix.contains("framesw"), "core must not default to FrameSW's log prefix");
    }
}
