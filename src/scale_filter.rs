//! Scale filtering of a scene's items, set from FrameSW.
//!
//! FrameSW's 9:16 scenes enlarge some of their items past the source's own
//! pixels: a 1080p camera made to fill a 1080x1920 frame is drawn 1.8x.
//! OBS's default for a scaled item (`OBS_SCALE_DISABLE`, which samples
//! bilinearly) is soft at that size; Lanczos keeps edges and letters
//! crisper (operator, 2026-10-10: "the scaling of the image is causing a
//! fuzzy out of focus look"). It is OBS's own per-item setting, Scale
//! Filtering in an item's right-click menu.
//!
//! obs-websocket 5 has no request for it (it carries an item's blend mode,
//! not its scale filter), so FrameSW cannot set it by itself.
//!
//! One request sets a whole scene: the items named get Lanczos, every
//! other item of that scene goes back to OBS's default. So the answer is
//! the same however often it is sent, and an item FrameSW no longer
//! enlarges is put back without being named. Only ever sent for scenes
//! FrameSW made; nothing here is saved anywhere but in the scene itself.

use std::ffi::{c_int, c_void, CString};

use studio_mode_meters_core::metering::{
    ffi_guard, obs_get_source_by_name, obs_queue_task, obs_source_release, ObsSourceT, OBS_TASK_UI,
};
use studio_mode_meters_core::obs_data;
use studio_mode_meters_core::{calldata, log_line};

/// `obs_scene_t` and `obs_sceneitem_t`: opaque to this plugin.
#[repr(C)]
pub struct ObsSceneT {
    _private: [u8; 0],
}
#[repr(C)]
pub struct ObsSceneItemT {
    _private: [u8; 0],
}

// Signatures verified against obsproject/obs-studio@32.0.4, 2026-10-10:
// libobs/obs.h
//   enum obs_scale_type {
//       OBS_SCALE_DISABLE, OBS_SCALE_POINT, OBS_SCALE_BICUBIC,
//       OBS_SCALE_BILINEAR, OBS_SCALE_LANCZOS, OBS_SCALE_AREA,
//   };
//   EXPORT obs_scene_t *obs_scene_from_source(const obs_source_t *source);
//       (no reference added: obs-scene.c returns `source->context.data`,
//       NULL for a source that is not a scene)
//   EXPORT void obs_scene_enum_items(obs_scene_t *scene,
//       bool (*callback)(obs_scene_t *, obs_sceneitem_t *, void *), void *param);
//       (holds the scene's locks for the whole walk: obs-scene.c
//       `full_lock(scene)` ... `full_unlock(scene)`)
//   EXPORT int64_t obs_sceneitem_get_id(const obs_sceneitem_t *item);
//   EXPORT void obs_sceneitem_set_scale_filter(obs_sceneitem_t *item, enum obs_scale_type filter);
//       (takes no lock: obs-scene.c sets `item->scale_filter` and the
//       atomic `update_transform`, so it is safe inside the walk above)
//   EXPORT enum obs_scale_type obs_sceneitem_get_scale_filter(obs_sceneitem_t *item);
studio_mode_meters_core::resolved_fn!(obs_scene_from_source: extern "C" fn(*const ObsSourceT) -> *mut ObsSceneT);
studio_mode_meters_core::resolved_fn!(
    obs_scene_enum_items:
        extern "C" fn(*mut ObsSceneT, extern "C" fn(*mut ObsSceneT, *mut ObsSceneItemT, *mut c_void) -> bool, *mut c_void)
);
studio_mode_meters_core::resolved_fn!(obs_sceneitem_get_id: extern "C" fn(*const ObsSceneItemT) -> i64);
studio_mode_meters_core::resolved_fn!(obs_sceneitem_set_scale_filter: extern "C" fn(*mut ObsSceneItemT, c_int));
studio_mode_meters_core::resolved_fn!(obs_sceneitem_get_scale_filter: extern "C" fn(*mut ObsSceneItemT) -> c_int);

/// `enum obs_scale_type`, the two values used.
const OBS_SCALE_DISABLE: c_int = 0;
const OBS_SCALE_LANCZOS: c_int = 4;

/// What the request asks, and what came of it. Owned by the (blocked)
/// request handler for the whole of the UI task.
struct Work {
    scene: CString,
    /// The items that get Lanczos, by scene item id.
    lanczos: Vec<i64>,
    /// False if the task never ran: OBS had no UI task handler.
    ran: bool,
    /// Whether `scene` names a scene.
    found: bool,
    /// Why nothing could be done, when a libobs symbol is missing.
    missing: bool,
    items: i64,
    changed: i64,
}

/// One item of the scene: Lanczos if it is named, OBS's default if not.
/// Written only when it differs, so an unchanged scene is left alone.
extern "C" fn each_item(_scene: *mut ObsSceneT, item: *mut ObsSceneItemT, param: *mut c_void) -> bool {
    ffi_guard(
        "scale_filter::each_item",
        true,
        std::panic::AssertUnwindSafe(|| {
            if item.is_null() || param.is_null() {
                return true;
            }
            let (Some(get_id), Some(get), Some(set)) =
                (obs_sceneitem_get_id(), obs_sceneitem_get_scale_filter(), obs_sceneitem_set_scale_filter())
            else {
                return false;
            };
            let work = unsafe { &mut *param.cast::<Work>() };
            work.items += 1;
            let want = if work.lanczos.contains(&get_id(item)) { OBS_SCALE_LANCZOS } else { OBS_SCALE_DISABLE };
            if get(item) != want {
                set(item, want);
                work.changed += 1;
            }
            true
        }),
    )
}

/// Runs on OBS's UI thread (`obs_queue_task(OBS_TASK_UI, ...)`), where OBS
/// itself changes an item's scale filter. `param` is a `*mut Work` owned
/// by the blocked caller for the whole of the task.
extern "C" fn set_on_ui_thread(param: *mut c_void) {
    ffi_guard(
        "scale_filter::set_on_ui_thread",
        (),
        std::panic::AssertUnwindSafe(|| {
            if param.is_null() {
                return;
            }
            let work = unsafe { &mut *param.cast::<Work>() };
            work.ran = true;
            let (Some(get_source), Some(release), Some(from_source), Some(enum_items)) =
                (obs_get_source_by_name(), obs_source_release(), obs_scene_from_source(), obs_scene_enum_items())
            else {
                work.missing = true;
                return;
            };
            if obs_sceneitem_get_id().is_none()
                || obs_sceneitem_get_scale_filter().is_none()
                || obs_sceneitem_set_scale_filter().is_none()
            {
                work.missing = true;
                return;
            }
            // A strong reference to the scene's source, held for the walk.
            let source = get_source(work.scene.as_ptr());
            if source.is_null() {
                return;
            }
            let scene = from_source(source);
            if !scene.is_null() {
                work.found = true;
                enum_items(scene, each_item, param);
            }
            release(source);
        }),
    );
}

extern "C" fn handle_set_scale_filters(request_data: *mut c_void, response_data: *mut c_void, priv_data: *mut c_void) {
    ffi_guard(
        "handle_set_scale_filters",
        (),
        std::panic::AssertUnwindSafe(|| handle_set_scale_filters_impl(request_data, response_data, priv_data)),
    );
}

/// Request: `{"scene": "FrameSW A · Portrait", "lanczos": "12,15"}`: the
/// scene by name, and the ids of its items that get Lanczos scaling, as a
/// comma-separated list (empty: none). Every other item of the scene is
/// set to OBS's default.
///
/// Response: `{"ok": true, "items": n, "changed": m}`, or `{"ok": false,
/// "error": "..."}` when the scene does not exist, OBS has no UI task
/// handler, or a libobs symbol is missing.
fn handle_set_scale_filters_impl(request_data: *mut c_void, response_data: *mut c_void, _priv_data: *mut c_void) {
    let request_data = obs_data::from_void(request_data);
    let response_data = obs_data::from_void(response_data);
    let fail = |error: &str| {
        obs_data::set_bool(response_data, "ok", false);
        obs_data::set_string(response_data, "error", error);
    };
    let name = obs_data::get_string(request_data, "scene").unwrap_or_default();
    let Ok(scene) = CString::new(name.as_str()) else {
        return fail("invalid scene name");
    };
    if name.is_empty() {
        return fail("no scene named");
    }
    let lanczos = parse_ids(&obs_data::get_string(request_data, "lanczos").unwrap_or_default());
    let Some(obs_queue_task) = obs_queue_task() else {
        return fail("obs_queue_task unavailable");
    };
    let mut work = Work { scene, lanczos, ran: false, found: false, missing: false, items: 0, changed: 0 };
    obs_queue_task(OBS_TASK_UI, set_on_ui_thread, (&mut work as *mut Work).cast(), true);
    if !work.ran {
        return fail("UI task handler unavailable");
    }
    if work.missing {
        return fail("required libobs symbols unavailable");
    }
    if !work.found {
        return fail("no such scene");
    }
    if work.changed > 0 {
        log_line(&format!("scale filters: {} of {} items changed in '{name}'", work.changed, work.items));
    }
    obs_data::set_bool(response_data, "ok", true);
    obs_data::set_int(response_data, "items", work.items);
    obs_data::set_int(response_data, "changed", work.changed);
}

/// The ids in a comma-separated list; anything that is not a number is
/// passed over.
fn parse_ids(list: &str) -> Vec<i64> {
    list.split(',').filter_map(|part| part.trim().parse::<i64>().ok()).collect()
}

pub fn requests() -> [(&'static str, calldata::RequestCallbackFn); 1] {
    [("set_scale_filters", handle_set_scale_filters as calldata::RequestCallbackFn)]
}

#[cfg(test)]
mod tests {
    use super::parse_ids;

    #[test]
    fn ids_are_read_from_a_list() {
        assert_eq!(parse_ids("12,15"), vec![12, 15]);
        assert_eq!(parse_ids(" 7 , x, ,9"), vec![7, 9]);
        assert!(parse_ids("").is_empty());
    }
}
