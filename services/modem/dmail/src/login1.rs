//! logind's notice of a suspend, so dmail can quiet the modem before one.

use zbus::proxy;
use zbus::zvariant::OwnedFd;

#[proxy(
    interface = "org.freedesktop.login1.Manager",
    default_service = "org.freedesktop.login1",
    default_path = "/org/freedesktop/login1"
)]
pub trait Manager {
    /// An inhibitor lock, held while the returned descriptor is open.
    fn inhibit(&self, what: &str, who: &str, why: &str, mode: &str) -> zbus::Result<OwnedFd>;

    /// Sent before a suspend (true) and after the resume (false).
    #[zbus(signal)]
    fn prepare_for_sleep(&self, start: bool) -> zbus::Result<()>;
}
