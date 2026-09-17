//! Fetching and caching the official ACP Registry.
//!
//! A refresh failure is never destructive. The last catalog that parsed stays
//! on disk and in memory, installed agents keep their pinned launch data, and
//! live sessions are untouched. `RegistryClient` therefore answers a catalog
//! query from the cache whenever the network cannot answer it.
use super::manifest::{parse_catalog, RegistryCatalog, RegistryRejection};
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, RwLock};
use std::time::Duration;

/// The published registry document this build reads.
pub const DEFAULT_REGISTRY_URL: &str =
    "https://cdn.agentclientprotocol.com/registry/v1/latest/registry.json";

/// Refuse a registry document larger than this.
pub const MAX_REGISTRY_BYTES: u64 = 32 * 1024 * 1024;
/// Refuse an archive larger than this.
pub const MAX_DOWNLOAD_BYTES: u64 = 1024 * 1024 * 1024;

const CATALOG_FILE: &str = "registry.json";
const METADATA_FILE: &str = "registry-meta.json";

#[derive(Debug)]
pub struct EmptyCatalog {
    pub count: usize,
    pub rejected: Vec<RegistryRejection>,
}

impl std::fmt::Display for EmptyCatalog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "The Registry contains zero accepted entries ({} rejected).",
            self.count
        )
    }
}

impl std::error::Error for EmptyCatalog {}

impl EmptyCatalog {
    pub fn from_catalog(catalog: &RegistryCatalog) -> Self {
        Self {
            count: catalog.rejected.len(),
            rejected: catalog.rejected.clone(),
        }
    }
}

pub type FetchFuture<'a> = Pin<Box<dyn Future<Output = anyhow::Result<Vec<u8>>> + Send + 'a>>;

/// Callback invoked as chunks arrive during a download: `(downloaded_bytes, total_bytes)`.
pub type ProgressReporter = Arc<dyn Fn(u64, Option<u64>) + Send + Sync>;

/// The one place this subsystem touches the network. It is a trait so every
/// registry, install, and update test runs against local fixtures.
pub trait HttpFetch: Send + Sync + 'static {
    fn fetch(&self, url: String, max_bytes: u64) -> FetchFuture<'_>;

    fn fetch_with_progress(
        &self,
        url: String,
        max_bytes: u64,
        _on_progress: Option<ProgressReporter>,
    ) -> FetchFuture<'_> {
        self.fetch(url, max_bytes)
    }
}

/// The real client. TLS trust anchors are compiled in, so no system
/// certificate store is needed.
pub struct HttpsFetch {
    client: reqwest::Client,
}

impl HttpsFetch {
    pub fn new() -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .user_agent(concat!("batey/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(600))
            .redirect(reqwest::redirect::Policy::limited(5))
            .https_only(true)
            .build()?;
        Ok(Self { client })
    }
}

impl HttpFetch for HttpsFetch {
    fn fetch(&self, url: String, max_bytes: u64) -> FetchFuture<'_> {
        self.fetch_with_progress(url, max_bytes, None)
    }

    fn fetch_with_progress(
        &self,
        url: String,
        max_bytes: u64,
        on_progress: Option<ProgressReporter>,
    ) -> FetchFuture<'_> {
        Box::pin(async move {
            anyhow::ensure!(
                super::manifest::is_https(&url),
                "Registry downloads use https only, but the URL was '{url}'"
            );
            let mut response = self.client.get(&url).send().await?;
            let status = response.status();
            anyhow::ensure!(
                status.is_success(),
                "Download of {url} failed with {status}"
            );
            let total_bytes = response.content_length();
            if let Some(length) = total_bytes {
                anyhow::ensure!(
                    length <= max_bytes,
                    "Download of {url} is {length} bytes, over the {max_bytes} byte limit"
                );
            }
            if let Some(ref reporter) = on_progress {
                reporter(0, total_bytes);
            }
            let mut downloaded: u64 = 0;
            let mut body = Vec::with_capacity(total_bytes.unwrap_or(0).min(max_bytes) as usize);
            while let Some(chunk) = response.chunk().await? {
                downloaded = downloaded.saturating_add(chunk.len() as u64);
                anyhow::ensure!(
                    downloaded <= max_bytes,
                    "Download of {url} is over the {max_bytes} byte limit"
                );
                body.extend_from_slice(&chunk);
                if let Some(ref reporter) = on_progress {
                    reporter(downloaded, total_bytes);
                }
            }
            Ok(body)
        })
    }
}

/// The transport used when the TLS client could not be built. It reports the
/// build failure on every call instead of failing at startup, so a machine
/// without a usable TLS stack still runs its installed agents.
pub struct UnavailableFetch {
    reason: String,
}

impl UnavailableFetch {
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }
}

impl HttpFetch for UnavailableFetch {
    fn fetch(&self, url: String, _max_bytes: u64) -> FetchFuture<'_> {
        let reason = self.reason.clone();
        Box::pin(async move { Err(anyhow::anyhow!("Cannot download {url}: {reason}")) })
    }
}

/// The default transport: TLS when it builds, a reporting stub otherwise.
pub fn default_fetch() -> Arc<dyn HttpFetch> {
    match HttpsFetch::new() {
        Ok(client) => Arc::new(client),
        Err(error) => {
            tracing::warn!(%error, "Could not build the HTTPS client for the ACP Registry");
            Arc::new(UnavailableFetch::new(error.to_string()))
        }
    }
}

/// What the cache knows beside the catalog itself.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CacheMetadata {
    pub source_url: String,
    pub fetched_at: chrono::DateTime<chrono::Utc>,
}

/// A catalog plus where it came from.
#[derive(Debug, Clone, PartialEq)]
pub struct CachedCatalog {
    pub catalog: RegistryCatalog,
    pub metadata: CacheMetadata,
    /// True when the catalog came from disk rather than from a live fetch.
    pub from_cache: bool,
}

pub struct RegistryClient {
    url: String,
    cache_dir: PathBuf,
    http: Arc<dyn HttpFetch>,
    /// The parsed catalog, so browsing does not re-read and re-parse the file.
    memory: RwLock<Option<CachedCatalog>>,
}

impl RegistryClient {
    pub fn new(url: impl Into<String>, cache_dir: PathBuf, http: Arc<dyn HttpFetch>) -> Self {
        Self {
            url: url.into(),
            cache_dir,
            http,
            memory: RwLock::new(None),
        }
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }

    pub fn http(&self) -> Arc<dyn HttpFetch> {
        self.http.clone()
    }

    /// The best catalog available without going to the network: the in-memory
    /// copy, else the durable cache. `None` means the registry has never been
    /// fetched on this machine.
    pub fn cached(&self) -> Option<CachedCatalog> {
        if let Some(cached) = self
            .memory
            .read()
            .expect("registry cache lock poisoned")
            .clone()
        {
            return Some(cached);
        }
        let loaded = self.read_cache()?;
        *self.memory.write().expect("registry cache lock poisoned") = Some(loaded.clone());
        Some(loaded)
    }

    /// Fetches, validates, and caches the registry. The cache is replaced only
    /// after the new document parses, so a bad response cannot destroy the
    /// last known good catalog.
    pub async fn refresh(&self) -> anyhow::Result<CachedCatalog> {
        let body = self
            .http
            .fetch(self.url.clone(), MAX_REGISTRY_BYTES)
            .await
            .map_err(|error| anyhow::anyhow!(sanitize_fetch_error(&error.to_string())))?;
        let text = String::from_utf8(body)
            .map_err(|_| anyhow::anyhow!("Registry document is not valid UTF-8"))?;
        let catalog = parse_catalog(&text)?;
        if catalog.agents.is_empty() {
            return Err(EmptyCatalog::from_catalog(&catalog).into());
        }
        let metadata = CacheMetadata {
            source_url: self.url.clone(),
            fetched_at: chrono::Utc::now(),
        };
        // A cache write failure leaves the fetched catalog usable for this
        // run, so a read-only data directory degrades instead of failing.
        if let Err(error) = self.write_cache(&text, &metadata) {
            tracing::warn!(%error, "Could not write the registry cache; using the fetched catalog for this run");
        }
        let cached = CachedCatalog {
            catalog,
            metadata,
            from_cache: true,
        };
        *self.memory.write().expect("registry cache lock poisoned") = Some(cached.clone());
        // Only the direct return of a successful fetch is fresh. Every later
        // read of the same catalog is a cache hit, including the fallback a
        // failed refresh returns.
        Ok(CachedCatalog {
            from_cache: false,
            ..cached
        })
    }

    /// Refreshes, and falls back to the cache when the refresh fails. The
    /// error is returned beside the cached catalog so the caller can report
    /// the outage without losing the catalog.
    pub async fn refresh_or_cached(&self) -> (Option<CachedCatalog>, Option<anyhow::Error>) {
        match self.refresh().await {
            Ok(cached) => (Some(cached), None),
            Err(error) => (self.cached(), Some(error)),
        }
    }

    fn read_cache(&self) -> Option<CachedCatalog> {
        let text = std::fs::read_to_string(self.cache_dir.join(CATALOG_FILE)).ok()?;
        let catalog = match parse_catalog(&text) {
            Ok(catalog) => catalog,
            Err(error) => {
                tracing::warn!(%error, "Cached registry document is unreadable; a refresh is needed");
                return None;
            }
        };
        let metadata = std::fs::read_to_string(self.cache_dir.join(METADATA_FILE))
            .ok()
            .and_then(|raw| serde_json::from_str::<CacheMetadata>(&raw).ok())
            .unwrap_or_else(|| CacheMetadata {
                source_url: self.url.clone(),
                fetched_at: chrono::Utc::now(),
            });
        Some(CachedCatalog {
            catalog,
            metadata,
            from_cache: true,
        })
    }

    /// Writes through a temporary file and renames, so a crash or a full disk
    /// never leaves a half-written catalog behind.
    fn write_cache(&self, document: &str, metadata: &CacheMetadata) -> anyhow::Result<()> {
        std::fs::create_dir_all(&self.cache_dir)?;
        write_atomic(&self.cache_dir.join(CATALOG_FILE), document.as_bytes())?;
        write_atomic(
            &self.cache_dir.join(METADATA_FILE),
            serde_json::to_string_pretty(metadata)?.as_bytes(),
        )?;
        Ok(())
    }
}

fn write_atomic(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let temporary = path.with_extension(format!("tmp-{}", uuid::Uuid::new_v4()));
    std::fs::write(&temporary, bytes)?;
    match std::fs::rename(&temporary, path) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = std::fs::remove_file(&temporary);
            Err(error.into())
        }
    }
}

/// Remove URL user information from a fetch error.
fn sanitize_fetch_error(message: &str) -> String {
    let mut output = String::new();
    let mut remaining = message;
    while let Some(protocol_end) = remaining.find("://") {
        let prefix_end = protocol_end + 3;
        output.push_str(&remaining[..prefix_end]);
        let after_protocol = &remaining[prefix_end..];
        let authority_end = after_protocol
            .find(|character: char| {
                character == '/'
                    || character == '?'
                    || character == '#'
                    || character.is_whitespace()
            })
            .unwrap_or(after_protocol.len());
        let authority = &after_protocol[..authority_end];
        if let Some(at) = authority.rfind('@') {
            output.push_str("***@");
            remaining = &after_protocol[at + 1..];
        } else {
            remaining = after_protocol;
        }
    }
    output.push_str(remaining);
    output
}

#[cfg(test)]
pub(crate) mod testing {
    use super::*;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    /// An `HttpFetch` backed by fixtures. Every registry test uses it, so no
    /// test reaches the public registry.
    type StreamEntry = (Vec<Vec<u8>>, Option<u64>);

    /// An `HttpFetch` backed by fixtures. Every registry test uses it, so no
    /// test reaches the public registry.
    #[derive(Default)]
    pub struct FixtureFetch {
        responses: Mutex<HashMap<String, anyhow::Result<Vec<u8>>>>,
        streams: Mutex<HashMap<String, StreamEntry>>,
        pub calls: AtomicUsize,
    }

    impl FixtureFetch {
        pub fn new() -> Self {
            Self::default()
        }

        pub fn with(self, url: &str, body: impl Into<Vec<u8>>) -> Self {
            self.responses
                .lock()
                .unwrap()
                .insert(url.to_string(), Ok(body.into()));
            self
        }

        pub fn with_stream(
            self,
            url: &str,
            chunks: Vec<Vec<u8>>,
            total_bytes: Option<u64>,
        ) -> Self {
            self.streams
                .lock()
                .unwrap()
                .insert(url.to_string(), (chunks, total_bytes));
            self
        }

        pub fn failing(self, url: &str, message: &str) -> Self {
            self.responses
                .lock()
                .unwrap()
                .insert(url.to_string(), Err(anyhow::anyhow!("{}", message)));
            self
        }

        pub fn set(&self, url: &str, body: impl Into<Vec<u8>>) {
            self.responses
                .lock()
                .unwrap()
                .insert(url.to_string(), Ok(body.into()));
        }

        pub fn set_failing(&self, url: &str, message: &str) {
            self.responses
                .lock()
                .unwrap()
                .insert(url.to_string(), Err(anyhow::anyhow!("{}", message)));
        }

        pub fn call_count(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl HttpFetch for FixtureFetch {
        fn fetch(&self, url: String, max_bytes: u64) -> FetchFuture<'_> {
            self.fetch_with_progress(url, max_bytes, None)
        }

        fn fetch_with_progress(
            &self,
            url: String,
            max_bytes: u64,
            on_progress: Option<ProgressReporter>,
        ) -> FetchFuture<'_> {
            self.calls.fetch_add(1, Ordering::SeqCst);

            let stream_entry = self.streams.lock().unwrap().get(&url).cloned();
            if let Some((chunks, total_bytes)) = stream_entry {
                return Box::pin(async move {
                    if let Some(total) = total_bytes {
                        if total > max_bytes {
                            return Err(anyhow::anyhow!(
                                "Download of {url} is {total} bytes, over the {max_bytes} byte limit"
                            ));
                        }
                    }
                    if let Some(ref reporter) = on_progress {
                        reporter(0, total_bytes);
                    }
                    let mut downloaded: u64 = 0;
                    let mut body = Vec::new();
                    for chunk in chunks {
                        downloaded = downloaded.saturating_add(chunk.len() as u64);
                        if downloaded > max_bytes {
                            return Err(anyhow::anyhow!(
                                "Download of {url} is over the {max_bytes} byte limit"
                            ));
                        }
                        body.extend_from_slice(&chunk);
                        if let Some(ref reporter) = on_progress {
                            reporter(downloaded, total_bytes);
                        }
                    }
                    Ok(body)
                });
            }

            let answer = match self.responses.lock().unwrap().get(&url) {
                Some(Ok(body)) if body.len() as u64 > max_bytes => {
                    Err(anyhow::anyhow!("Download of {url} is over the byte limit"))
                }
                Some(Ok(body)) => {
                    let body_len = body.len() as u64;
                    if let Some(ref reporter) = on_progress {
                        reporter(0, Some(body_len));
                        reporter(body_len, Some(body_len));
                    }
                    Ok(body.clone())
                }
                Some(Err(error)) => Err(anyhow::anyhow!("{error}")),
                None => Err(anyhow::anyhow!("No fixture for {url}")),
            };
            Box::pin(async move { answer })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::FixtureFetch;
    use super::*;

    const URL: &str = "https://registry.invalid/registry.json";

    fn document(version: &str, agent: &str) -> String {
        format!(
            r#"{{"version":"1.0.0","agents":[
                {{"id":"{agent}","name":"Agent","version":"{version}","description":"d",
                 "distribution":{{"npx":{{"package":"{agent}@{version}"}}}}}}]}}"#
        )
    }

    fn client(tmp: &tempfile::TempDir, http: Arc<FixtureFetch>) -> RegistryClient {
        RegistryClient::new(URL, tmp.path().join("registry-cache"), http)
    }

    #[tokio::test]
    async fn zero_accepted_entries_never_replace_the_last_good_cache() {
        let tmp = tempfile::tempdir().unwrap();
        let http = Arc::new(FixtureFetch::new().with(URL, document("1.0.0", "example")));
        let client = client(&tmp, http.clone());
        let first = client.refresh().await.unwrap();
        http.set(URL, r#"{"version":"1.0.0","agents":[{"id":"bad"}]}"#);
        let (cached, error) = client.refresh_or_cached().await;
        let error = error.unwrap();
        let empty = error.downcast_ref::<EmptyCatalog>().unwrap();
        assert_eq!(empty.count, 1);
        assert_eq!(empty.rejected[0].reason, "missing name");
        assert_eq!(cached.unwrap().catalog, first.catalog);
        let offline = Arc::new(FixtureFetch::new().failing(URL, "offline"));
        let restarted =
            RegistryClient::new(URL, tmp.path().join("registry-cache"), offline.clone());
        let cached = restarted.cached().unwrap();
        assert!(cached.from_cache);
        assert_eq!(cached.catalog, first.catalog);
        assert_eq!(cached.metadata, first.metadata);
        assert_eq!(offline.call_count(), 0);
        let (cached, error) = restarted.refresh_or_cached().await;
        assert_eq!(cached.unwrap().catalog, first.catalog);
        assert!(error.is_some());
    }

    #[tokio::test]
    async fn refresh_parses_and_caches_the_catalog() {
        let tmp = tempfile::tempdir().unwrap();
        let http = Arc::new(FixtureFetch::new().with(URL, document("1.0.0", "example")));
        let client = client(&tmp, http);

        let fetched = client.refresh().await.unwrap();
        assert!(!fetched.from_cache);
        assert_eq!(fetched.catalog.agents.len(), 1);
        assert_eq!(fetched.metadata.source_url, URL);
        assert!(tmp.path().join("registry-cache/registry.json").is_file());
        assert!(tmp
            .path()
            .join("registry-cache/registry-meta.json")
            .is_file());
        // No `.tmp-` file survives an atomic write.
        let leftovers: Vec<_> = std::fs::read_dir(tmp.path().join("registry-cache"))
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "temporary cache file was left behind");
    }

    /// The core resilience rule: an outage keeps the last good catalog.
    #[tokio::test]
    async fn a_failed_refresh_keeps_the_cached_catalog() {
        let tmp = tempfile::tempdir().unwrap();
        let http = Arc::new(FixtureFetch::new().with(URL, document("1.0.0", "example")));
        let client = client(&tmp, http.clone());
        client.refresh().await.unwrap();

        http.set_failing(URL, "network is unreachable");
        let error = client.refresh().await.unwrap_err();
        assert!(error.to_string().contains("unreachable"));

        let (catalog, reported) = client.refresh_or_cached().await;
        let catalog = catalog.expect("cached catalog was lost");
        assert_eq!(catalog.catalog.agent("example").unwrap().version, "1.0.0");
        assert!(reported.is_some(), "the outage was not reported");
        assert!(tmp.path().join("registry-cache/registry.json").is_file());
    }

    /// A malformed response must not overwrite the good cache either.
    #[tokio::test]
    async fn a_malformed_response_does_not_replace_the_cache() {
        let tmp = tempfile::tempdir().unwrap();
        let http = Arc::new(FixtureFetch::new().with(URL, document("1.0.0", "example")));
        let client = client(&tmp, http.clone());
        client.refresh().await.unwrap();

        http.set(URL, "{ not a registry");
        assert!(client.refresh().await.is_err());

        let fresh = RegistryClient::new(URL, tmp.path().join("registry-cache"), http);
        let cached = fresh.cached().expect("cache was destroyed");
        assert!(cached.from_cache);
        assert_eq!(cached.catalog.agent("example").unwrap().version, "1.0.0");
    }

    #[tokio::test]
    async fn the_cache_survives_a_restart_and_is_read_once() {
        let tmp = tempfile::tempdir().unwrap();
        let http = Arc::new(FixtureFetch::new().with(URL, document("2.0.0", "example")));
        client(&tmp, http.clone()).refresh().await.unwrap();
        assert_eq!(http.call_count(), 1);

        let restarted = client(&tmp, http.clone());
        let first = restarted.cached().unwrap();
        let second = restarted.cached().unwrap();
        assert_eq!(first, second);
        assert_eq!(first.catalog.agent("example").unwrap().version, "2.0.0");
        // Reading the cache never goes to the network.
        assert_eq!(http.call_count(), 1);
    }

    #[tokio::test]
    async fn no_cache_and_no_network_reports_nothing_cached() {
        let tmp = tempfile::tempdir().unwrap();
        let http = Arc::new(FixtureFetch::new().failing(URL, "offline"));
        let client = client(&tmp, http);
        assert!(client.cached().is_none());
        let (catalog, error) = client.refresh_or_cached().await;
        assert!(catalog.is_none());
        assert!(error.unwrap().to_string().contains("offline"));
    }

    #[test]
    fn fetch_errors_remove_url_user_information() {
        let error = sanitize_fetch_error(
            "request failed for https://secret:token@example.invalid/registry.json: DNS error",
        );
        assert_eq!(
            error,
            "request failed for https://***@example.invalid/registry.json: DNS error"
        );
        assert!(!error.contains("secret"));
        assert!(!error.contains("token"));
    }

    #[tokio::test]
    async fn fixture_fetch_streams_chunks_with_progress_and_enforces_limit() {
        let chunks = vec![b"chunk1".to_vec(), b"chunk2".to_vec(), b"chunk3".to_vec()];
        let progress_records = Arc::new(std::sync::Mutex::new(Vec::new()));
        let records = progress_records.clone();
        let reporter = Arc::new(move |dl, total| {
            records.lock().unwrap().push((dl, total));
        });

        let http = FixtureFetch::new().with_stream("https://example.com/test", chunks, Some(18));
        let body = http
            .fetch_with_progress("https://example.com/test".into(), 100, Some(reporter))
            .await
            .unwrap();
        assert_eq!(body, b"chunk1chunk2chunk3");

        let recorded = progress_records.lock().unwrap().clone();
        assert_eq!(
            recorded,
            vec![(0, Some(18)), (6, Some(18)), (12, Some(18)), (18, Some(18))]
        );

        let chunks_over = vec![vec![0u8; 50], vec![0u8; 60]];
        let http_over =
            FixtureFetch::new().with_stream("https://example.com/over", chunks_over, None);
        let err = http_over
            .fetch_with_progress("https://example.com/over".into(), 100, None)
            .await;
        assert!(err.is_err());
        assert!(err
            .unwrap_err()
            .to_string()
            .contains("over the 100 byte limit"));
    }

    #[test]
    fn default_registry_url_is_the_official_latest_catalog() {
        assert_eq!(
            DEFAULT_REGISTRY_URL,
            "https://cdn.agentclientprotocol.com/registry/v1/latest/registry.json"
        );
    }

    #[tokio::test]
    async fn a_refresh_replaces_the_cached_catalog_on_success() {
        let tmp = tempfile::tempdir().unwrap();
        let http = Arc::new(FixtureFetch::new().with(URL, document("1.0.0", "example")));
        let client = client(&tmp, http.clone());
        client.refresh().await.unwrap();

        http.set(URL, document("3.0.0", "example"));
        let refreshed = client.refresh().await.unwrap();
        assert_eq!(refreshed.catalog.agent("example").unwrap().version, "3.0.0");
        assert_eq!(
            client
                .cached()
                .unwrap()
                .catalog
                .agent("example")
                .unwrap()
                .version,
            "3.0.0"
        );
    }

    #[tokio::test]
    async fn an_unreadable_cache_file_is_reported_not_served() {
        let tmp = tempfile::tempdir().unwrap();
        let cache_dir = tmp.path().join("registry-cache");
        std::fs::create_dir_all(&cache_dir).unwrap();
        std::fs::write(cache_dir.join("registry.json"), "{ broken").unwrap();
        let http = Arc::new(FixtureFetch::new());
        let client = RegistryClient::new(URL, cache_dir, http);
        assert!(client.cached().is_none());
    }

    #[tokio::test]
    async fn an_oversized_document_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let huge = vec![b'x'; (MAX_REGISTRY_BYTES + 1) as usize];
        let http = Arc::new(FixtureFetch::new().with(URL, huge));
        let client = client(&tmp, http);
        assert!(client
            .refresh()
            .await
            .unwrap_err()
            .to_string()
            .contains("limit"));
    }
}
