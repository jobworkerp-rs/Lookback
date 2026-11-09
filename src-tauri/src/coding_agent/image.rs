use async_trait::async_trait;
use thiserror::Error;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedImage {
    pub reference: String,
    pub digest: String,
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
#[error("the configured image reference could not be resolved")]
pub struct ImageResolutionError;

#[async_trait]
// async_trait annotates boxed futures that already carry must_use.
#[allow(clippy::double_must_use)]
pub trait ImageDigestResolver: Send + Sync {
    async fn manifest_digest(&self, reference: &str) -> Result<String, ImageResolutionError>;
}

pub async fn resolve_image_reference(
    reference: &str,
    resolver: &dyn ImageDigestResolver,
) -> Result<ResolvedImage, ImageResolutionError> {
    if !super::settings::valid_image_reference(reference) {
        return Err(ImageResolutionError);
    }
    let parsed: oci_client::Reference = reference.parse().map_err(|_| ImageResolutionError)?;
    let digest = match parsed.digest() {
        Some(digest) => digest.to_owned(),
        None => resolver.manifest_digest(reference).await?,
    };
    if !valid_digest(&digest) {
        return Err(ImageResolutionError);
    }
    let pinned = oci_client::Reference::with_digest(
        parsed.registry().to_owned(),
        parsed.repository().to_owned(),
        digest.clone(),
    );
    Ok(ResolvedImage {
        reference: pinned.whole(),
        digest,
    })
}

fn valid_digest(digest: &str) -> bool {
    digest.strip_prefix("sha256:").is_some_and(|hash| {
        hash.len() == 64
            && hash
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    })
}

/// Registry metadata is resolved afresh; no Docker credential file or host home is scanned.
pub struct OciImageDigestResolver {
    client: oci_client::Client,
}

impl Default for OciImageDigestResolver {
    fn default() -> Self {
        Self {
            client: oci_client::Client::new(Default::default()),
        }
    }
}

#[async_trait]
impl ImageDigestResolver for OciImageDigestResolver {
    async fn manifest_digest(&self, reference: &str) -> Result<String, ImageResolutionError> {
        let image: oci_client::Reference = reference.parse().map_err(|_| ImageResolutionError)?;
        let (_, digest) = tokio::time::timeout(
            std::time::Duration::from_secs(8),
            self.client
                .pull_image_manifest(&image, &oci_client::secrets::RegistryAuth::Anonymous),
        )
        .await
        .map_err(|_| ImageResolutionError)?
        .map_err(|_| ImageResolutionError)?;
        Ok(digest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    struct Resolver {
        calls: AtomicUsize,
        next: Mutex<Result<String, ImageResolutionError>>,
    }
    #[async_trait]
    impl ImageDigestResolver for Resolver {
        async fn manifest_digest(&self, _: &str) -> Result<String, ImageResolutionError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.next.lock().unwrap().clone()
        }
    }

    #[tokio::test]
    async fn registry_resolution_fetches_real_manifest_digests_on_each_new_execution() {
        use sha2::{Digest, Sha256};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let manifest = Arc::new(Mutex::new(serde_json::json!({
            "schemaVersion": 2, "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "config": {"mediaType": "application/vnd.oci.image.config.v1+json", "size": 2, "digest": format!("sha256:{}", "a".repeat(64))},
            "layers": [], "annotations": {"test": "first"}
        }).to_string()));
        let paths = Arc::new(Mutex::new(Vec::<String>::new()));
        let current = Arc::clone(&manifest);
        let requests = Arc::clone(&paths);
        let task = tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = vec![0; 8192];
                let len = socket.read(&mut bytes).await.unwrap();
                let request = String::from_utf8_lossy(&bytes[..len]);
                let path = request
                    .lines()
                    .next()
                    .unwrap()
                    .split_whitespace()
                    .nth(1)
                    .unwrap()
                    .to_owned();
                requests.lock().unwrap().push(path.clone());
                let body = if path.contains("/manifests/") {
                    current.lock().unwrap().clone()
                } else {
                    "{}".to_owned()
                };
                let digest = format!("sha256:{:x}", Sha256::digest(body.as_bytes()));
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/vnd.oci.image.manifest.v1+json\r\nDocker-Content-Digest: {digest}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            }
        });
        let resolver = OciImageDigestResolver {
            client: oci_client::Client::new(oci_client::client::ClientConfig {
                protocol: oci_client::client::ClientProtocol::Http,
                ..Default::default()
            }),
        };
        let reference = format!("{address}/org/coder:release");
        let first = resolve_image_reference(&reference, &resolver)
            .await
            .unwrap();
        assert_eq!(
            first.digest,
            format!(
                "sha256:{:x}",
                Sha256::digest(manifest.lock().unwrap().as_bytes())
            )
        );
        let changed = manifest.lock().unwrap().replace("first", "second");
        *manifest.lock().unwrap() = changed;
        let second = resolve_image_reference(&reference, &resolver)
            .await
            .unwrap();
        assert_ne!(first.digest, second.digest);
        let paths = paths.lock().unwrap();
        assert_eq!(
            paths
                .iter()
                .filter(|path| path.as_str() == "/v2/org/coder/manifests/release")
                .count(),
            2
        );
        assert!(paths.iter().all(|path| !path.contains("/blobs/")));
        task.abort();
    }

    #[tokio::test]
    async fn tags_are_resolved_afresh_and_previous_execution_pins_are_unchanged() {
        let resolver = Resolver {
            calls: AtomicUsize::new(0),
            next: Mutex::new(Ok(format!("sha256:{}", "a".repeat(64)))),
        };
        let previous = resolve_image_reference("new.registry:5043/org/coder:release", &resolver)
            .await
            .unwrap();
        *resolver.next.lock().unwrap() = Ok(format!("sha256:{}", "b".repeat(64)));
        let current = resolve_image_reference("new.registry:5043/org/coder:release", &resolver)
            .await
            .unwrap();
        assert_eq!(
            previous.reference,
            format!("new.registry:5043/org/coder@sha256:{}", "a".repeat(64))
        );
        assert_ne!(current.digest, previous.digest);
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 2);
        let pinned = resolve_image_reference(&previous.reference, &resolver)
            .await
            .unwrap();
        assert_eq!(pinned, previous);
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn unresolved_or_invalid_registry_data_cannot_become_a_runtime_pin() {
        let resolver = Resolver {
            calls: AtomicUsize::new(0),
            next: Mutex::new(Err(ImageResolutionError)),
        };
        assert!(
            resolve_image_reference("registry/org/coder:release", &resolver)
                .await
                .is_err()
        );
        for digest in ["sha256:bad", "not-a-digest", ""] {
            *resolver.next.lock().unwrap() = Ok(digest.into());
            assert!(
                resolve_image_reference("registry/org/coder:release", &resolver)
                    .await
                    .is_err()
            );
        }
        let count = resolver.calls.load(Ordering::SeqCst);
        for reference in [
            "https://registry/org/coder:release",
            "registry/org/coder@sha256:bad",
            "registry/../coder:release",
            "",
        ] {
            assert!(resolve_image_reference(reference, &resolver).await.is_err());
        }
        assert_eq!(resolver.calls.load(Ordering::SeqCst), count);
    }
}
