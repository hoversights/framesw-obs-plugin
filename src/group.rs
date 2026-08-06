// SPDX-License-Identifier: GPL-2.0-or-later
//
// FrameSW Companion Plugin for OBS Studio
// Copyright (C) 2026 Hoversights
//
// This program is free software; you can redistribute it and/or modify it
// under the terms of the GNU General Public License as published by the
// Free Software Foundation; either version 2 of the License, or (at your
// option) any later version.
//
// This program is distributed in the hope that it will be useful, but
// WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the GNU
// General Public License for more details.
//
// You should have received a copy of the GNU General Public License along
// with this program; if not, see <https://www.gnu.org/licenses/>.

//! SPIKE (throwaway): proves the companion plugin can create and manage
//! real OBS "groups" (`obs_scene_add_group` et al.) — obs-websocket's own
//! request surface has no equivalent (its scene-item requests operate on
//! individual items only, never groups), so this is only reachable
//! through a real plugin. Exists to prove four capabilities via one
//! minimal vendor request (`lib.rs`'s `handle_manage_group`); not the
//! shape a real shot-creation integration would eventually use, and not
//! wired into anything except that one demo request.
//!
//! Every FFI declaration checked against real, verbatim source
//! (`obsproject/obs-studio@master`: `libobs/obs.h` for signatures,
//! `libobs/obs-scene.c` for the one behavioral quirk that isn't obvious
//! from the header alone — see `add_item_to_group`'s doc comment).

use std::ffi::{c_char, CString};

use crate::ObsSourceT;

/// Opaque — `typedef struct obs_scene obs_scene_t;` (`libobs/obs.h`).
pub enum ObsSceneT {}
/// Opaque — `typedef struct obs_sceneitem obs_sceneitem_t;` (`libobs/obs.h`).
pub enum ObsSceneItemT {}

// Resolved at runtime, same as every other libobs symbol in this crate —
// see `platform.rs`'s module doc for why. `obs_get_source_by_name`/
// `obs_source_release` are already declared in `lib.rs`; redeclared here
// rather than made `pub(crate)` there, since `resolved_fn!` is designed
// to be invoked independently per module (each expansion is its own
// cheap, independently-cached lookup, not a shared registry) — matching
// this crate's existing precedent of each module declaring exactly the
// symbols it needs.
crate::resolved_fn!(obs_get_source_by_name: extern "C" fn(*const c_char) -> *mut ObsSourceT);
crate::resolved_fn!(obs_source_release: extern "C" fn(*mut ObsSourceT));
// `libobs/obs.h`: "Gets a scene from its source, or NULL if not a scene."
crate::resolved_fn!(obs_scene_from_source: extern "C" fn(*const ObsSourceT) -> *mut ObsSceneT);
// `libobs/obs.h`: "Adds/creates a new scene item for a source" (the
// group's own name doubles as the name of the private source libobs
// creates to back it — same as any other source name).
crate::resolved_fn!(obs_scene_add_group: extern "C" fn(*mut ObsSceneT, *const c_char) -> *mut ObsSceneItemT);
crate::resolved_fn!(obs_scene_find_source: extern "C" fn(*mut ObsSceneT, *const c_char) -> *mut ObsSceneItemT);
crate::resolved_fn!(obs_sceneitem_group_add_item: extern "C" fn(*mut ObsSceneItemT, *mut ObsSceneItemT));
crate::resolved_fn!(obs_sceneitem_group_remove_item: extern "C" fn(*mut ObsSceneItemT, *mut ObsSceneItemT));
crate::resolved_fn!(obs_sceneitem_set_locked: extern "C" fn(*mut ObsSceneItemT, bool) -> bool);
// `libobs/obs.h`: "Gets the scene parent associated with the scene item"
// for a *group* item specifically — a group's contents live in their own
// private sub-scene (its source's `context.data`, confirmed in
// `obs-scene.c`'s `obs_sceneitem_group_add_item`), invisible to
// `obs_scene_find_source` on the *outer* scene once an item has actually
// been moved in. This is what makes `remove_item_from_group` able to
// find it again.
crate::resolved_fn!(obs_sceneitem_group_get_scene: extern "C" fn(*const ObsSceneItemT) -> *mut ObsSceneT);

fn cstr(s: &str) -> CString {
    CString::new(s).unwrap_or_else(|e| {
        let valid_len = e.nul_position();
        CString::new(&e.into_vec()[..valid_len]).unwrap_or_default()
    })
}

/// Looks up `scene_name`'s real OBS scene (e.g. FrameSW's own "PGM-A"),
/// same `obs_get_source_by_name` + `obs_scene_from_source` pattern
/// `lib.rs`'s `attach_scene_audio_taps` already uses. The returned
/// `obs_source_t*` reference is released before returning — `obs_scene_t*`
/// itself needs no separate release (`obs_scene_from_source` is a
/// borrowed pointer, confirmed against the real header: no accompanying
/// `obs_scene_release` call anywhere it's used that way in libobs's own
/// source).
fn find_scene(scene_name: &str) -> Result<*mut ObsSceneT, String> {
    let (Some(obs_get_source_by_name), Some(obs_scene_from_source), Some(obs_source_release)) =
        (self::obs_get_source_by_name(), self::obs_scene_from_source(), self::obs_source_release())
    else {
        return Err("required libobs symbol not resolved".into());
    };
    let cname = cstr(scene_name);
    let source = obs_get_source_by_name(cname.as_ptr());
    if source.is_null() {
        return Err(format!("no source named '{scene_name}' (scene not created yet?)"));
    }
    let scene = obs_scene_from_source(source);
    obs_source_release(source);
    if scene.is_null() {
        return Err(format!("source '{scene_name}' exists but is not a scene"));
    }
    Ok(scene)
}

/// Creates a new group named `group_name` in `scene_name`.
///
/// **Quirk hit while building this spike**: `obs_scene_add_group` does
/// *not* check whether a group with this name already exists first —
/// confirmed against `obs_scene_insert_group`'s real body
/// (`obs-scene.c`): it unconditionally calls `create_id`/
/// `obs_scene_add_internal`. Calling this twice with the same name
/// creates two distinct, identically-named groups, not an error and not
/// an idempotent no-op. A real integration wanting "create if missing"
/// must call `obs_scene_get_group` itself first and branch on that —
/// out of scope for this spike (each demo call uses a name known to be
/// fresh), but a real gap to close before this becomes production code.
pub fn create_group(scene_name: &str, group_name: &str) -> Result<(), String> {
    let scene = find_scene(scene_name)?;
    let Some(obs_scene_add_group) = self::obs_scene_add_group() else {
        return Err("obs_scene_add_group not resolved".into());
    };
    let cname = cstr(group_name);
    let item = obs_scene_add_group(scene, cname.as_ptr());
    if item.is_null() {
        return Err(format!("obs_scene_add_group('{group_name}') returned null"));
    }
    Ok(())
}

/// Moves the existing top-level scene item backing `source_name` into
/// `group_name`, both within `scene_name`.
///
/// **ABI-critical detail confirmed by reading `obs-scene.c` directly**,
/// not just the header: `obs_sceneitem_group_add_item` silently does
/// *nothing* (`return;`, no error signal at all) unless the item's
/// current parent is exactly the group's own parent scene
/// (`if (item->parent != scene) return;`). So the source must already be
/// a *direct* item of `scene_name` — not already in some other group,
/// not in a different scene — or this call is a silent no-op. Located
/// via `obs_scene_find_source` (search-by-name within the top-level
/// scene only, matching that same parent requirement).
pub fn add_item_to_group(scene_name: &str, group_name: &str, source_name: &str) -> Result<(), String> {
    let scene = find_scene(scene_name)?;
    let (Some(obs_scene_find_source), Some(obs_sceneitem_group_add_item)) =
        (self::obs_scene_find_source(), self::obs_sceneitem_group_add_item())
    else {
        return Err("required libobs symbol not resolved".into());
    };
    let group_cname = cstr(group_name);
    let group_item = obs_scene_find_source(scene, group_cname.as_ptr());
    if group_item.is_null() {
        return Err(format!("no group named '{group_name}' in scene '{scene_name}'"));
    }
    let source_cname = cstr(source_name);
    let source_item = obs_scene_find_source(scene, source_cname.as_ptr());
    if source_item.is_null() {
        return Err(format!(
            "no top-level item for source '{source_name}' in scene '{scene_name}' \
             (already in a group, or not in this scene at all?)"
        ));
    }
    obs_sceneitem_group_add_item(group_item, source_item);
    Ok(())
}

/// Moves `source_name`'s scene item back out of `group_name`, into
/// `scene_name`'s top level.
///
/// **Quirk hit while building this spike**: a naive
/// `obs_scene_find_source(scene, source_name)` — the same call
/// `add_item_to_group` uses — does *not* find the item once it's
/// actually inside the group: a group's contents live in their own
/// private sub-scene, invisible to a find-by-name on the *outer* scene.
/// Confirmed by reading `obs_sceneitem_group_add_item`'s real body
/// (`obs-scene.c`): it reparents the item into
/// `group->source->context.data` (the group's own sub-scene), not
/// anywhere `scene`'s own top-level search walks. Fixed by resolving the
/// group's sub-scene via `obs_sceneitem_group_get_scene` first and
/// searching *that* instead of `scene`.
pub fn remove_item_from_group(scene_name: &str, group_name: &str, source_name: &str) -> Result<(), String> {
    let scene = find_scene(scene_name)?;
    let (
        Some(obs_scene_find_source),
        Some(obs_sceneitem_group_get_scene),
        Some(obs_sceneitem_group_remove_item),
    ) = (
        self::obs_scene_find_source(),
        self::obs_sceneitem_group_get_scene(),
        self::obs_sceneitem_group_remove_item(),
    )
    else {
        return Err("required libobs symbol not resolved".into());
    };
    let group_cname = cstr(group_name);
    let group_item = obs_scene_find_source(scene, group_cname.as_ptr());
    if group_item.is_null() {
        return Err(format!("no group named '{group_name}' in scene '{scene_name}'"));
    }
    let group_scene = obs_sceneitem_group_get_scene(group_item);
    if group_scene.is_null() {
        return Err(format!("'{group_name}' has no group sub-scene (is it really a group?)"));
    }
    let source_cname = cstr(source_name);
    let source_item = obs_scene_find_source(group_scene, source_cname.as_ptr());
    if source_item.is_null() {
        return Err(format!("'{source_name}' is not currently inside group '{group_name}'"));
    }
    obs_sceneitem_group_remove_item(group_item, source_item);
    Ok(())
}

/// Locks or unlocks `group_name`'s own scene item (the padlock in OBS's
/// Sources panel) — `obs_sceneitem_set_locked` returns the *previous*
/// lock state, not a success flag (confirmed in the real header: "returns
/// if the item was locked or not" — easy to misread as success/failure at
/// a glance), so this ignores that return value entirely rather than
/// treating it as an error signal.
pub fn set_group_locked(scene_name: &str, group_name: &str, locked: bool) -> Result<(), String> {
    let scene = find_scene(scene_name)?;
    let (Some(obs_scene_find_source), Some(obs_sceneitem_set_locked)) =
        (self::obs_scene_find_source(), self::obs_sceneitem_set_locked())
    else {
        return Err("required libobs symbol not resolved".into());
    };
    let cname = cstr(group_name);
    let item = obs_scene_find_source(scene, cname.as_ptr());
    if item.is_null() {
        return Err(format!("no group named '{group_name}' in scene '{scene_name}'"));
    }
    let _previous_lock_state = obs_sceneitem_set_locked(item, locked);
    Ok(())
}
