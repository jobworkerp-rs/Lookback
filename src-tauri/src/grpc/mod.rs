pub mod proto;

use std::time::Duration;

use tonic::transport::{Channel, ClientTlsConfig, Endpoint};

use crate::error::{AppError, AppResult};

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
    let endpoint = Endpoint::from_shared(url.to_string())
        .map_err(|e| AppError::Config(format!("invalid grpc url {url}: {e}")))?
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(30))
        .tcp_keepalive(Some(Duration::from_secs(30)))
        .http2_keep_alive_interval(Duration::from_secs(15))
        .keep_alive_timeout(Duration::from_secs(10));

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
    use super::needs_tls;

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
