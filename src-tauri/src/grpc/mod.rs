pub mod proto;

use std::time::Duration;

use tonic::transport::{Channel, ClientTlsConfig, Endpoint};

use crate::error::{AppError, AppResult};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const TCP_KEEPALIVE: Option<Duration> = Some(Duration::from_secs(30));

#[derive(Clone, Copy)]
struct ConnectProfile {
    request_timeout: Duration,
    http2_keep_alive_interval: Duration,
    http2_keep_alive_timeout: Duration,
}

impl ConnectProfile {
    const fn normal() -> Self {
        Self {
            request_timeout: Duration::from_secs(30),
            http2_keep_alive_interval: Duration::from_secs(15),
            http2_keep_alive_timeout: Duration::from_secs(10),
        }
    }

    const fn startup_probe() -> Self {
        Self {
            request_timeout: Duration::from_secs(90),
            http2_keep_alive_interval: Duration::from_secs(15),
            http2_keep_alive_timeout: Duration::from_secs(60),
        }
    }
}

/// Build a tonic Channel to the memories sidecar (or a remote override) with
/// sensible defaults (HTTP/2 keepalive, short connect timeout). All callers
/// share these.
///
/// An `https://` target needs TLS configured explicitly: tonic does NOT enable
/// it from the scheme alone, so without this a remote memories URL silently
/// fails the handshake even though `grpcurl` (which negotiates TLS by default)
/// connects. Mirrors jobworkerp-client's `GrpcConnection` setup so both clients
/// behave the same against the same remote.
pub async fn connect(url: &str) -> AppResult<Channel> {
    connect_with_profile(url, ConnectProfile::normal()).await
}

/// Build a tonic Channel for the local sidecar startup compatibility probe.
///
/// The bundled memories process can take longer to answer its first reflection
/// request while opening the local database. Keep this relaxed profile scoped
/// to startup diagnostics so regular RPCs retain their normal deadlines.
pub async fn connect_startup_probe(url: &str) -> AppResult<Channel> {
    connect_with_profile(url, ConnectProfile::startup_probe()).await
}

async fn connect_with_profile(url: &str, profile: ConnectProfile) -> AppResult<Channel> {
    let endpoint = Endpoint::from_shared(url.to_string())
        .map_err(|e| AppError::Config(format!("invalid grpc url {url}: {e}")))?
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(profile.request_timeout)
        .tcp_keepalive(TCP_KEEPALIVE)
        .http2_keep_alive_interval(profile.http2_keep_alive_interval)
        .keep_alive_timeout(profile.http2_keep_alive_timeout);

    let endpoint = if needs_tls(url) {
        // rustls needs a process-wide CryptoProvider before the first
        // handshake; `install_default` is idempotent (Err if already set), so
        // ignore the result. See rustls/rustls#1938.
        let _ = rustls::crypto::ring::default_provider().install_default();
        endpoint
            .tls_config(ClientTlsConfig::new().with_enabled_roots())
            .map_err(|e| AppError::Config(format!("tls config for {url}: {e}")))?
    } else {
        endpoint
    };

    Ok(endpoint.connect().await?)
}

/// Whether a gRPC target needs TLS. tonic keys this off the explicit
/// `tls_config`, not the scheme, so we drive it from the URL ourselves. Parses
/// the scheme via the `url` crate (not `starts_with`) so a case variant like
/// `HTTPS://` — which the crate lowercases — isn't mistaken for plaintext. An
/// unparseable URL never reaches here (`Endpoint::from_shared` rejects it
/// first), so a parse miss safely defaults to no-TLS. Kept pure so the
/// http/https split is unit-testable without a live server.
fn needs_tls(url: &str) -> bool {
    url::Url::parse(url).is_ok_and(|u| u.scheme() == "https")
}

#[cfg(test)]
mod tests {
    use super::{ConnectProfile, needs_tls};

    #[test]
    fn startup_probe_profile_is_more_permissive_than_normal() {
        let normal = ConnectProfile::normal();
        let startup = ConnectProfile::startup_probe();

        assert!(startup.request_timeout > normal.request_timeout);
        assert!(startup.http2_keep_alive_timeout > normal.http2_keep_alive_timeout);
        assert_eq!(
            startup.http2_keep_alive_interval,
            normal.http2_keep_alive_interval
        );
    }

    #[test]
    fn https_targets_need_tls() {
        assert!(needs_tls("https://memories.example.com:9000"));
        assert!(needs_tls("https://[2001:db8::1]:9010"));
        // The url crate lowercases the scheme, so a case variant still matches.
        assert!(needs_tls("HTTPS://memories.example.com:9000"));
    }

    #[test]
    fn http_and_local_targets_skip_tls() {
        assert!(!needs_tls("http://127.0.0.1:9010"));
        assert!(!needs_tls("http://memories.example.com:9000"));
    }
}
