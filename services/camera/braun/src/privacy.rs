//! The user's camera switches: whether an app may stream at all.
//!
//! A device's privacy service (nx809j-privacy on the RedMagic) copies each
//! user's switches to `/run/nx809j-privacy/users/<uid>.json`, a file that
//! user owns:
//!
//! ```json
//! {"camera": true, "apps": {"camera": {"firefox": false}}}
//! ```
//!
//! `"camera": false` turns the camera off for every app of that user, and
//! an app's own `false` for that app only. Apps are known by the name they
//! run as, so this keeps honest apps out; it is no sandbox. Without the
//! file, or for a file someone else wrote, every app may stream.

use std::os::unix::fs::MetadataExt;

const DIR: &str = "/run/nx809j-privacy/users";

/// Whether `app`, run by `user`, may use the camera now.
pub fn camera_allowed(user: Option<u32>, app: &str) -> bool {
    let Some(user) = user else { return true };
    let path = format!("{DIR}/{user}.json");
    let Ok(file) = std::fs::File::open(&path) else {
        return true;
    };
    if file.metadata().map(|meta| meta.uid()).ok() != Some(user) {
        return true;
    }
    let Ok(switches) = serde_json::from_reader::<_, serde_json::Value>(file) else {
        return true;
    };
    allows(&switches, app)
}

fn allows(switches: &serde_json::Value, app: &str) -> bool {
    if switches.get("camera").and_then(serde_json::Value::as_bool) == Some(false) {
        return false;
    }
    switches
        .pointer("/apps/camera")
        .and_then(|apps| apps.get(app))
        .and_then(serde_json::Value::as_bool)
        != Some(false)
}

#[cfg(test)]
mod tests {
    use super::allows;
    use serde_json::json;

    #[test]
    fn the_switch_turns_every_app_off() {
        assert!(!allows(&json!({"camera": false}), "camera"));
        assert!(allows(&json!({"camera": true}), "camera"));
        assert!(allows(&json!({}), "camera"));
    }

    #[test]
    fn an_app_can_be_turned_off_alone() {
        let switches =
            json!({"camera": true, "apps": {"camera": {"firefox": false, "camera": true}}});
        assert!(!allows(&switches, "firefox"));
        assert!(allows(&switches, "camera"));
        assert!(allows(&switches, "other"));
    }
}
