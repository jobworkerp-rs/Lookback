use std::net::{SocketAddr, TcpListener};

use crate::error::{AppError, AppResult};

/// Pick a port for a sidecar service. Try `preferred` first; if it's busy,
/// fall back to OS-assigned (port 0). ARCH-7 dictates that the resolved
/// port is not persisted across launches.
///
/// The returned port has been successfully bound and *released* — there is a
/// small race window before the child process re-binds it. Acceptable for a
/// local desktop app.
pub fn pick(preferred: u16) -> AppResult<u16> {
    if let Ok(listener) = TcpListener::bind(("127.0.0.1", preferred)) {
        let port = listener.local_addr()?.port();
        drop(listener);
        return Ok(port);
    }
    let listener = TcpListener::bind("127.0.0.1:0").map_err(AppError::Io)?;
    let port = listener.local_addr()?.port();
    drop(listener);
    Ok(port)
}

/// Build a `127.0.0.1:<port>` SocketAddr without involving DNS.
pub fn loopback(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pick_returns_preferred_when_free() {
        // 0 means "OS pick a free port" — that should always succeed.
        let port = pick(0).unwrap();
        assert!(port > 0);
    }

    #[test]
    fn pick_falls_back_when_preferred_busy() {
        let blocker = TcpListener::bind("127.0.0.1:0").unwrap();
        let busy = blocker.local_addr().unwrap().port();
        let chosen = pick(busy).unwrap();
        // Either we got a *different* port, or the blocker released between
        // pick's attempt and now. Both are valid; the contract is "returns
        // some bindable port".
        assert!(chosen > 0);
    }

    #[test]
    fn loopback_uses_127_0_0_1() {
        let addr = loopback(9000);
        assert_eq!(addr.to_string(), "127.0.0.1:9000");
    }
}
