//! `org.braun.Camera1` on `/org/braun`: the cameras there are, and a
//! session for each client that connects (`proto`).

use std::collections::HashMap;
use std::sync::OnceLock;

use zbus::message::Header;
use zbus::zvariant::{OwnedFd, OwnedObjectPath, OwnedValue, Value};
use zbus::{fdo, interface};

use crate::{NAME, PATH, proto, session};

struct Service;

fn owned<'a>(value: impl Into<Value<'a>>) -> OwnedValue {
    value
        .into()
        .try_to_owned()
        .expect("no descriptors in a camera's description")
}

#[interface(name = "org.braun.Camera1")]
impl Service {
    /// A session socket: the other end is served on a thread of its own
    /// until the client closes it.
    async fn connect(
        &self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &zbus::Connection,
    ) -> fdo::Result<OwnedFd> {
        let caller = allowed(connection, &header).await?;
        let (ours, theirs) =
            proto::pair().map_err(|error| fdo::Error::Failed(error.to_string()))?;
        session::start(ours, app_name(caller.pid), caller.user)
            .map_err(|error| fdo::Error::Failed(error.to_string()))?;
        Ok(theirs.into())
    }

    /// Each camera: its `id`, `name`, `facing` (front, back or external),
    /// `role` (main, ultrawide, telephoto, auxiliary, or empty), the `zoom`
    /// at which the main camera shows what it does (when the device says),
    /// its frame upright (`width`, `height`), whether it has a `lens` to
    /// focus and whether it is `tuned`.
    #[zbus(property)]
    fn cameras(&self) -> Vec<HashMap<String, OwnedValue>> {
        session::cameras(false)
            .iter()
            .map(|camera| {
                let (width, height) = camera.upright();
                let mut description = HashMap::from([
                    ("id".to_string(), owned(camera.id.as_str())),
                    ("name".to_string(), owned(camera.name.as_str())),
                    ("facing".to_string(), owned(camera.facing.name())),
                    (
                        "role".to_string(),
                        owned(camera.role.map_or("", |role| role.name())),
                    ),
                    ("width".to_string(), owned(width)),
                    ("height".to_string(), owned(height)),
                    ("lens".to_string(), owned(camera.lens.is_some())),
                    ("tuned".to_string(), owned(camera.tuned)),
                ]);
                if let Some(zoom) = camera.zoom {
                    description.insert("zoom".to_string(), owned(f64::from(zoom)));
                }
                description
            })
            .collect()
    }
}

/// Who connected, as far as the bus knows.
struct Caller {
    pid: Option<u32>,
    user: Option<u32>,
}

/// Whoever may open the camera nodes may connect: root, the `video` group,
/// and the user of an active local session, as uaccess has it.
async fn allowed(connection: &zbus::Connection, header: &Header<'_>) -> fdo::Result<Caller> {
    let sender = header
        .sender()
        .ok_or_else(|| fdo::Error::AccessDenied("no sender".into()))?;
    let credentials = fdo::DBusProxy::new(connection)
        .await?
        .get_connection_credentials(sender.clone().into())
        .await?;
    let caller = Caller {
        pid: credentials.process_id(),
        user: credentials.unix_user_id(),
    };
    if caller.user == Some(0) {
        return Ok(caller);
    }
    if let (Some(video), Some(groups)) = (video_group(), credentials.unix_group_ids())
        && groups.contains(&video)
    {
        return Ok(caller);
    }
    if let Some(uid) = credentials.unix_user_id()
        && active_local_user(connection, uid).await.unwrap_or(false)
    {
        return Ok(caller);
    }
    Err(fdo::Error::AccessDenied(
        "the cameras are for root, the video group and the active local session".into(),
    ))
}

/// The app behind a process, as it names itself: the file name it was
/// started as, else its `comm` (cut at 15 bytes). Anyone may read both,
/// unlike the executable of another user's process.
fn app_name(pid: Option<u32>) -> String {
    let Some(pid) = pid else {
        return "unknown".to_string();
    };
    let started_as = std::fs::read(format!("/proc/{pid}/cmdline"))
        .ok()
        .and_then(|cmdline| {
            let first = cmdline.split(|&byte| byte == 0).next()?;
            let name = String::from_utf8_lossy(first);
            let name = name.rsplit('/').next()?.trim().to_string();
            (!name.is_empty()).then_some(name)
        });
    started_as
        .or_else(|| {
            std::fs::read_to_string(format!("/proc/{pid}/comm"))
                .ok()
                .map(|name| name.trim().to_string())
                .filter(|name| !name.is_empty())
        })
        .unwrap_or_else(|| "unknown".to_string())
}

fn video_group() -> Option<u32> {
    static VIDEO: OnceLock<Option<u32>> = OnceLock::new();
    *VIDEO.get_or_init(|| {
        // SAFETY: the first call only, under the OnceLock; the entry is read
        // before any other getgr* call.
        let group = unsafe { libc::getgrnam(c"video".as_ptr()) };
        (!group.is_null()).then(|| unsafe { (*group).gr_gid })
    })
}

/// Whether `uid` has an active session on a seat, not a remote one. That is
/// the user uaccess gives the camera nodes to, whichever of their processes
/// calls: a service of their systemd user manager is in no session of its own.
async fn active_local_user(connection: &zbus::Connection, uid: u32) -> zbus::Result<bool> {
    const LOGIN: &str = "org.freedesktop.login1";
    let manager = zbus::Proxy::new(
        connection,
        LOGIN,
        "/org/freedesktop/login1",
        "org.freedesktop.login1.Manager",
    )
    .await?;
    let sessions: Vec<(String, u32, String, String, OwnedObjectPath)> =
        manager.call("ListSessions", &()).await?;
    for (_, owner, _, seat, path) in sessions {
        if owner != uid || seat.is_empty() {
            continue;
        }
        let session =
            zbus::Proxy::new(connection, LOGIN, path, "org.freedesktop.login1.Session").await?;
        let active: bool = session.get_property("Active").await?;
        let remote: bool = session.get_property("Remote").await?;
        if active && !remote {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Serves the interface on the system bus for as long as the connection is
/// kept.
pub fn serve() -> zbus::Result<zbus::blocking::Connection> {
    zbus::blocking::connection::Builder::system()?
        .serve_at(PATH, Service)?
        .name(NAME)?
        .build()
}
