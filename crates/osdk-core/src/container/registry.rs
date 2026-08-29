//! Bounded, anonymous OCI Distribution registry diagnostics.
//!
//! The diagnostic engine is an injectable state machine. Its production
//! transport accepts HTTPS endpoints only, disables ambient credentials and
//! automatic redirects, caps every body, and never exposes tokens, raw headers,
//! or response bytes through the serializable report. HTTP endpoints can only
//! be constructed by tests and must resolve to loopback.

use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::net::IpAddr;
use std::pin::Pin;
use std::time::Duration;

use reqwest::header::HeaderMap;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use super::reference::{ImageReference, ImageSelector, OciDigest, OciPlatform, RegistryName};

pub const REGISTRY_DIAGNOSTIC_SCHEMA_VERSION: u32 = 1;
pub const DEFAULT_MAX_REQUESTS: usize = 12;
pub const DEFAULT_MAX_REDIRECTS: usize = 3;
pub const DEFAULT_MAX_BODY_BYTES: usize = 4 * 1024 * 1024;
pub const DEFAULT_MAX_MANIFEST_BYTES: usize = 2 * 1024 * 1024;
pub const DEFAULT_BLOB_SAMPLE_BYTES: usize = 16 * 1024;
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
pub const DEFAULT_TOTAL_TIMEOUT: Duration = Duration::from_secs(30);
const HARD_MAX_REQUESTS: usize = 64;
const HARD_MAX_REDIRECTS: usize = 5;
const HARD_MAX_BODY_BYTES: usize = 8 * 1024 * 1024;
const HARD_MAX_MANIFEST_BYTES: usize = 4 * 1024 * 1024;
const HARD_MAX_BLOB_SAMPLE_BYTES: usize = 1024 * 1024;
const HARD_MAX_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const HARD_MAX_TOTAL_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const HARD_MAX_ENDPOINT_BYTES: usize = 2 * 1024;
const HARD_MAX_ENDPOINT_PATH_BYTES: usize = 512;
const HARD_MAX_MIRRORS: usize = 8;
const HARD_MAX_CHALLENGE_BYTES: usize = 4 * 1024;
const HARD_MAX_HEADER_VALUES: usize = 8;

const ACCEPT_MANIFESTS: &str = concat!(
    "application/vnd.oci.image.index.v1+json, ",
    "application/vnd.oci.image.manifest.v1+json, ",
    "application/vnd.docker.distribution.manifest.list.v2+json, ",
    "application/vnd.docker.distribution.manifest.v2+json"
);
const OCI_INDEX: &str = "application/vnd.oci.image.index.v1+json";
const OCI_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
const DOCKER_INDEX: &str = "application/vnd.docker.distribution.manifest.list.v2+json";
const DOCKER_MANIFEST: &str = "application/vnd.docker.distribution.manifest.v2+json";

pub type RegistryTransportFuture<'a> =
    Pin<Box<dyn Future<Output = Result<RegistryResponse, RegistryTransportError>> + Send + 'a>>;

/// A validated registry base endpoint. The path is empty or a bounded prefix;
/// query strings, fragments, and user information are forbidden.
#[derive(Clone, PartialEq, Eq)]
pub struct RegistryEndpoint {
    url: reqwest::Url,
    registry: RegistryName,
    report_origin: String,
}

impl fmt::Debug for RegistryEndpoint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RegistryEndpoint")
            .field("origin", &self.report_origin)
            .field(
                "has_path_prefix",
                &(self.url.path() != "/" && !self.url.path().is_empty()),
            )
            .finish()
    }
}

impl RegistryEndpoint {
    pub fn for_registry(registry: RegistryName) -> Result<Self, RegistryProtocolError> {
        let transport_authority = if registry.is_docker_hub() {
            "registry-1.docker.io"
        } else {
            registry.as_str()
        };
        let mut endpoint = Self::parse(&format!("https://{transport_authority}"), false)?;
        endpoint.registry = registry;
        Ok(endpoint)
    }

    pub fn parse_https(value: &str) -> Result<Self, RegistryProtocolError> {
        Self::parse(value, false)
    }

    fn parse(value: &str, allow_loopback_http: bool) -> Result<Self, RegistryProtocolError> {
        if value.len() > HARD_MAX_ENDPOINT_BYTES
            || value.contains('%')
            || value.contains('\\')
            || value
                .split('/')
                .any(|component| matches!(component, "." | ".."))
        {
            return Err(RegistryProtocolError::InvalidEndpoint);
        }
        let mut url =
            reqwest::Url::parse(value).map_err(|_| RegistryProtocolError::InvalidEndpoint)?;
        if !url.username().is_empty() || url.password().is_some() {
            return Err(RegistryProtocolError::InvalidEndpoint);
        }
        if url.query().is_some() || url.fragment().is_some() {
            return Err(RegistryProtocolError::InvalidEndpoint);
        }
        if url.scheme() != "https"
            && !(allow_loopback_http && url.scheme() == "http" && url_is_loopback(&url))
        {
            return Err(RegistryProtocolError::InsecureEndpoint);
        }
        let host = url
            .host_str()
            .ok_or(RegistryProtocolError::InvalidEndpoint)?;
        let authority = registry_authority(host, url.port())?;
        let registry =
            RegistryName::parse(&authority).map_err(|_| RegistryProtocolError::InvalidEndpoint)?;
        let path = normalize_endpoint_path(url.path())?;
        url.set_path(&path);
        let report_origin = redacted_origin(&url);
        Ok(Self {
            url,
            registry,
            report_origin,
        })
    }

    #[cfg(test)]
    fn parse_loopback_http(value: &str) -> Result<Self, RegistryProtocolError> {
        Self::parse(value, true)
    }

    pub fn registry(&self) -> &RegistryName {
        &self.registry
    }

    /// A secret-safe origin suitable for diagnostic JSON.
    pub fn report_origin(&self) -> &str {
        &self.report_origin
    }

    /// Whether requests use transport encryption. Production construction
    /// always returns true; loopback HTTP exists only in this module's tests.
    pub fn is_https(&self) -> bool {
        self.url.scheme() == "https"
    }

    fn url_for(&self, path: &str) -> Result<reqwest::Url, RegistryProtocolError> {
        if !path.starts_with('/')
            || path.contains(['?', '#', '\\'])
            || path.split('/').any(|component| component == "..")
        {
            return Err(RegistryProtocolError::InvalidPath);
        }
        let prefix = self.url.path().trim_end_matches('/');
        let mut url = self.url.clone();
        url.set_path(&format!("{prefix}{path}"));
        Ok(url)
    }

    fn origin_key(&self) -> String {
        self.url.origin().ascii_serialization()
    }

    fn same_origin(&self, url: &reqwest::Url) -> bool {
        self.origin_key() == url.origin().ascii_serialization()
    }
}

fn normalize_endpoint_path(path: &str) -> Result<String, RegistryProtocolError> {
    if path.len() > HARD_MAX_ENDPOINT_PATH_BYTES
        || path.contains(['\\', '%'])
        || path
            .split('/')
            .any(|component| matches!(component, "." | ".."))
    {
        return Err(RegistryProtocolError::InvalidEndpoint);
    }
    if path == "/" {
        Ok(String::new())
    } else {
        Ok(path.trim_end_matches('/').to_owned())
    }
}

fn registry_authority(host: &str, port: Option<u16>) -> Result<String, RegistryProtocolError> {
    let host = if host
        .parse::<IpAddr>()
        .is_ok_and(|address| matches!(address, IpAddr::V6(_)))
    {
        format!("[{host}]")
    } else {
        host.to_owned()
    };
    Ok(match port {
        Some(port) => format!("{host}:{port}"),
        None => host,
    })
}

fn url_is_loopback(url: &reqwest::Url) -> bool {
    url.host_str().is_some_and(|host| {
        let host = host.trim_start_matches('[').trim_end_matches(']');
        host.eq_ignore_ascii_case("localhost")
            || host
                .parse::<IpAddr>()
                .is_ok_and(|address| address.is_loopback())
    })
}

/// Hard bounds applied to an entire diagnostic run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RegistryLimits {
    pub max_requests: usize,
    pub max_redirects: usize,
    pub max_body_bytes: usize,
    pub max_manifest_bytes: usize,
    pub blob_sample_bytes: usize,
    pub request_timeout: Duration,
    pub total_timeout: Duration,
}

impl Default for RegistryLimits {
    fn default() -> Self {
        Self {
            max_requests: DEFAULT_MAX_REQUESTS,
            max_redirects: DEFAULT_MAX_REDIRECTS,
            max_body_bytes: DEFAULT_MAX_BODY_BYTES,
            max_manifest_bytes: DEFAULT_MAX_MANIFEST_BYTES,
            blob_sample_bytes: DEFAULT_BLOB_SAMPLE_BYTES,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            total_timeout: DEFAULT_TOTAL_TIMEOUT,
        }
    }
}

impl RegistryLimits {
    fn validate(self) -> Result<Self, RegistryProtocolError> {
        if self.max_requests == 0
            || self.max_requests > HARD_MAX_REQUESTS
            || self.max_redirects > self.max_requests
            || self.max_redirects > HARD_MAX_REDIRECTS
            || self.max_body_bytes == 0
            || self.max_body_bytes > HARD_MAX_BODY_BYTES
            || self.max_manifest_bytes == 0
            || self.max_manifest_bytes > HARD_MAX_MANIFEST_BYTES
            || self.max_manifest_bytes > self.max_body_bytes
            || self.blob_sample_bytes == 0
            || self.blob_sample_bytes > HARD_MAX_BLOB_SAMPLE_BYTES
            || self.blob_sample_bytes > self.max_body_bytes
            || self.request_timeout.is_zero()
            || self.total_timeout.is_zero()
            || self.request_timeout > self.total_timeout
            || self.request_timeout > HARD_MAX_REQUEST_TIMEOUT
            || self.total_timeout > HARD_MAX_TOTAL_TIMEOUT
        {
            return Err(RegistryProtocolError::InvalidLimits);
        }
        Ok(self)
    }
}

/// Inputs for one anonymous diagnostic. Mirror order is significant and is
/// retained in the report.
#[derive(Clone, Debug)]
pub struct RegistryDiagnosticOptions {
    pub upstream: RegistryEndpoint,
    pub mirrors: Vec<RegistryEndpoint>,
    pub image: Option<ImageReference>,
    pub platform: Option<OciPlatform>,
    pub limits: RegistryLimits,
}

impl RegistryDiagnosticOptions {
    pub fn new(upstream: RegistryEndpoint) -> Self {
        Self {
            upstream,
            mirrors: Vec::new(),
            image: None,
            platform: None,
            limits: RegistryLimits::default(),
        }
    }

    pub fn with_mirrors(mut self, mirrors: Vec<RegistryEndpoint>) -> Self {
        self.mirrors = mirrors;
        self
    }

    pub fn with_image(mut self, image: ImageReference) -> Self {
        self.image = Some(image);
        self
    }

    pub fn with_platform(mut self, platform: OciPlatform) -> Self {
        self.platform = Some(platform);
        self
    }

    pub fn with_limits(mut self, limits: RegistryLimits) -> Self {
        self.limits = limits;
        self
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegistryMethod {
    Get,
    Head,
}

/// One transport request. Only a fixed allowlist of non-sensitive request
/// headers can be constructed. Authorization is held separately and its value
/// is deliberately absent from Debug and Serialize implementations.
#[derive(Clone)]
pub struct RegistryRequest {
    pub method: RegistryMethod,
    pub url: reqwest::Url,
    pub accept: Option<&'static str>,
    pub range: Option<(u64, u64)>,
    authorization: Option<BearerToken>,
    pub max_body_bytes: usize,
    pub timeout: Duration,
}

impl fmt::Debug for RegistryRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RegistryRequest")
            .field("method", &self.method)
            .field("origin", &redacted_origin(&self.url))
            .field("accept", &self.accept)
            .field("range", &self.range)
            .field(
                "authorization",
                &self.authorization.as_ref().map(|_| "[redacted]"),
            )
            .field("max_body_bytes", &self.max_body_bytes)
            .field("timeout", &self.timeout)
            .finish()
    }
}

impl RegistryRequest {
    pub fn has_authorization(&self) -> bool {
        self.authorization.is_some()
    }
}

#[derive(Clone, Default)]
pub struct RegistryResponse {
    pub status: u16,
    pub headers: BTreeMap<String, Vec<String>>,
    pub body: Vec<u8>,
    /// True when a range probe intentionally stopped after its byte budget.
    pub body_truncated: bool,
}

impl fmt::Debug for RegistryResponse {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RegistryResponse")
            .field("status", &self.status)
            .field("header_names", &self.headers.keys().collect::<Vec<_>>())
            .field("body_bytes", &self.body.len())
            .field("body_truncated", &self.body_truncated)
            .finish()
    }
}

impl RegistryResponse {
    pub fn new(status: u16) -> Self {
        Self {
            status,
            headers: BTreeMap::new(),
            body: Vec::new(),
            body_truncated: false,
        }
    }

    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.headers
            .entry(name.to_ascii_lowercase())
            .or_default()
            .push(value.to_owned());
        self
    }

    pub fn body(mut self, body: impl Into<Vec<u8>>) -> Self {
        self.body = body.into();
        self
    }

    #[cfg(test)]
    fn truncated_body(mut self, body: impl Into<Vec<u8>>) -> Self {
        self.body = body.into();
        self.body_truncated = true;
        self
    }

    fn header_values(&self, name: &str) -> impl Iterator<Item = &str> {
        self.headers
            .get(name)
            .into_iter()
            .flatten()
            .map(String::as_str)
    }

    fn one_header(&self, name: &str) -> Option<&str> {
        let mut values = self.header_values(name);
        let first = values.next()?;
        values.next().is_none().then_some(first)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RegistryTransportError {
    #[error("registry transport failed")]
    Failed,
    #[error("registry request timed out")]
    Timeout,
    #[error("registry response exceeded its body limit")]
    BodyTooLarge,
    #[error("registry redirect violated policy")]
    RedirectRejected,
    #[error("registry request budget was exhausted")]
    RequestLimit,
}

/// Injectable transport used by the protocol state machine. Implementations
/// must honor request body and timeout limits.
pub trait RegistryTransport: Send + Sync {
    fn execute(&self, request: RegistryRequest) -> RegistryTransportFuture<'_>;
}

/// Production HTTPS transport. It has no cookie jar, proxy auto-authentication,
/// client certificate, ambient registry credentials, or automatic redirects.
#[derive(Clone, Debug)]
pub struct ReqwestRegistryTransport {
    client: reqwest::Client,
}

impl ReqwestRegistryTransport {
    pub fn new() -> Result<Self, RegistryTransportError> {
        let client = reqwest::Client::builder()
            .user_agent(concat!("osdk/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(DEFAULT_REQUEST_TIMEOUT)
            .pool_idle_timeout(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .no_gzip()
            .https_only(true)
            .build()
            .map_err(|_| RegistryTransportError::Failed)?;
        Ok(Self { client })
    }
}

impl RegistryTransport for ReqwestRegistryTransport {
    fn execute(&self, request: RegistryRequest) -> RegistryTransportFuture<'_> {
        Box::pin(async move {
            if request.url.scheme() != "https" {
                return Err(RegistryTransportError::Failed);
            }
            let mut builder = match request.method {
                RegistryMethod::Get => self.client.get(request.url),
                RegistryMethod::Head => self.client.head(request.url),
            };
            builder = builder
                .timeout(request.timeout)
                .header(reqwest::header::ACCEPT_ENCODING, "identity");
            if let Some(accept) = request.accept {
                builder = builder.header(reqwest::header::ACCEPT, accept);
            }
            if let Some((start, end)) = request.range {
                builder = builder.header(reqwest::header::RANGE, format!("bytes={start}-{end}"));
            }
            if let Some(token) = &request.authorization {
                builder = builder.bearer_auth(token.expose());
            }
            let response = builder.send().await.map_err(map_reqwest_error)?;
            let status = response.status().as_u16();
            let headers = copy_safe_response_headers(response.headers());
            if request.method == RegistryMethod::Head {
                return Ok(RegistryResponse {
                    status,
                    headers,
                    body: Vec::new(),
                    body_truncated: false,
                });
            }
            let (body, body_truncated) =
                read_bounded_response(response, request.max_body_bytes).await?;
            if body_truncated && request.range.is_none() {
                return Err(RegistryTransportError::BodyTooLarge);
            }
            Ok(RegistryResponse {
                status,
                headers,
                body,
                body_truncated,
            })
        })
    }
}

fn map_reqwest_error(error: reqwest::Error) -> RegistryTransportError {
    if error.is_timeout() {
        RegistryTransportError::Timeout
    } else {
        RegistryTransportError::Failed
    }
}

async fn read_bounded_response(
    mut response: reqwest::Response,
    limit: usize,
) -> Result<(Vec<u8>, bool), RegistryTransportError> {
    let declared_oversize = response
        .content_length()
        .is_some_and(|length| length > limit as u64);
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(map_reqwest_error)? {
        let remaining = limit.saturating_sub(body.len());
        if chunk.len() > remaining {
            body.extend_from_slice(&chunk[..remaining]);
            return Ok((body, true));
        }
        body.extend_from_slice(&chunk);
        if body.len() == limit && declared_oversize {
            return Ok((body, true));
        }
    }
    Ok((body, false))
}

fn copy_safe_response_headers(headers: &HeaderMap) -> BTreeMap<String, Vec<String>> {
    const SAFE: [&str; 7] = [
        "www-authenticate",
        "location",
        "content-type",
        "content-length",
        "content-range",
        "docker-content-digest",
        "etag",
    ];
    let mut copied = BTreeMap::new();
    for name in SAFE {
        let values = headers
            .get_all(name)
            .iter()
            .take(HARD_MAX_HEADER_VALUES)
            .filter_map(|value| value.to_str().ok())
            .filter(|value| value.len() <= HARD_MAX_CHALLENGE_BYTES)
            .map(str::to_owned)
            .collect::<Vec<_>>();
        if !values.is_empty() {
            copied.insert(name.to_owned(), values);
        }
    }
    copied
}

#[derive(Clone)]
struct BearerToken {
    value: String,
    bound_origin: String,
}

impl BearerToken {
    fn expose(&self) -> &str {
        &self.value
    }

    fn is_bound_to_url(&self, url: &reqwest::Url) -> bool {
        self.bound_origin == url.origin().ascii_serialization()
    }
}

impl fmt::Debug for BearerToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("BearerToken([redacted])")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RegistryProtocolError {
    #[error("invalid registry endpoint")]
    InvalidEndpoint,
    #[error("registry endpoints must use HTTPS")]
    InsecureEndpoint,
    #[error("invalid registry request path")]
    InvalidPath,
    #[error("invalid registry diagnostic limits")]
    InvalidLimits,
    #[error("image registry does not match the diagnostic upstream")]
    RegistryMismatch,
    #[error("too many registry mirrors")]
    TooManyMirrors,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RegistryDiagnosticStatus {
    Healthy,
    Degraded,
    AuthenticationRequired,
    AccessDenied,
    RateLimited,
    NotFound,
    Unreachable,
    TimedOut,
    ProtocolError,
    Corrupt,
    Unsupported,
    LimitExceeded,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ApiCheckStatus {
    Available,
    BearerChallenge,
    AuthenticationRequired,
    AccessDenied,
    RateLimited,
    NotFound,
    ServerError,
    RedirectRejected,
    InvalidChallenge,
    UnexpectedResponse,
    Unreachable,
    TimedOut,
    BodyTooLarge,
    RequestLimit,
    NotTested,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ManifestCheckStatus {
    Verified,
    AuthenticationRequired,
    AccessDenied,
    RateLimited,
    NotFound,
    ServerError,
    RedirectRejected,
    InvalidMediaType,
    InvalidManifest,
    DigestMismatch,
    SizeMismatch,
    BodyTooLarge,
    PlatformNotFound,
    Unreachable,
    TimedOut,
    RequestLimit,
    NotRequested,
    NotTested,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum BlobRangeStatus {
    Supported,
    Ignored,
    Unsatisfiable,
    Malformed,
    DigestMismatch,
    AuthenticationRequired,
    AccessDenied,
    RateLimited,
    NotFound,
    ServerError,
    RedirectRejected,
    BodyTooLarge,
    Unreachable,
    TimedOut,
    RequestLimit,
    NotAvailable,
    NotTested,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum MirrorCheckStatus {
    Equivalent,
    Diverged,
    AuthenticationRequired,
    AccessDenied,
    RateLimited,
    NotFound,
    ServerError,
    RedirectRejected,
    InvalidResponse,
    Unreachable,
    TimedOut,
    BodyTooLarge,
    RequestLimit,
    NotTested,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ManifestKind {
    Image,
    Index,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ApiCheck {
    pub status: ApiCheckStatus,
    pub http_status: Option<u16>,
    pub bearer_challenge: bool,
    pub challenge_service_present: bool,
    pub challenge_scope_matches: Option<bool>,
}

impl Default for ApiCheck {
    fn default() -> Self {
        Self {
            status: ApiCheckStatus::NotTested,
            http_status: None,
            bearer_challenge: false,
            challenge_service_present: false,
            challenge_scope_matches: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ManifestCheck {
    pub status: ManifestCheckStatus,
    pub kind: Option<ManifestKind>,
    pub media_type: Option<String>,
    pub digest: Option<OciDigest>,
    pub child_digest: Option<OciDigest>,
    pub selected_platform: Option<OciPlatform>,
    pub selected_os_version: Option<String>,
    pub byte_size: Option<u64>,
}

impl Default for ManifestCheck {
    fn default() -> Self {
        Self {
            status: ManifestCheckStatus::NotRequested,
            kind: None,
            media_type: None,
            digest: None,
            child_digest: None,
            selected_platform: None,
            selected_os_version: None,
            byte_size: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct BlobRangeCheck {
    pub status: BlobRangeStatus,
    pub digest: Option<OciDigest>,
    pub requested_bytes: u64,
    pub received_bytes: u64,
    pub total_bytes: Option<u64>,
}

impl Default for BlobRangeCheck {
    fn default() -> Self {
        Self {
            status: BlobRangeStatus::NotTested,
            digest: None,
            requested_bytes: 0,
            received_bytes: 0,
            total_bytes: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct MirrorCheck {
    pub order: usize,
    pub origin: String,
    pub status: MirrorCheckStatus,
    pub digest: Option<OciDigest>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RegistryDiagnosticReport {
    pub schema_version: u32,
    pub status: RegistryDiagnosticStatus,
    pub upstream: RegistryName,
    pub upstream_origin: String,
    pub image: Option<ImageReference>,
    pub requested_platform: Option<OciPlatform>,
    pub api: ApiCheck,
    pub manifest: ManifestCheck,
    pub blob_range: BlobRangeCheck,
    pub mirrors: Vec<MirrorCheck>,
    pub request_count: usize,
}

struct DiagnosticState<'a, T> {
    transport: &'a T,
    options: RegistryDiagnosticOptions,
    request_count: usize,
    started: tokio::time::Instant,
}

#[derive(Clone, Copy)]
struct RequestPolicy {
    accept: Option<&'static str>,
    range: Option<(u64, u64)>,
    max_body_bytes: usize,
}

/// Run a bounded, read-only, anonymous registry diagnostic.
pub async fn diagnose_registry<T: RegistryTransport>(
    transport: &T,
    options: RegistryDiagnosticOptions,
) -> Result<RegistryDiagnosticReport, RegistryProtocolError> {
    let limits = options.limits.validate()?;
    if options.mirrors.len() > HARD_MAX_MIRRORS {
        return Err(RegistryProtocolError::TooManyMirrors);
    }
    if let Some(image) = &options.image {
        if image.registry() != options.upstream.registry() {
            return Err(RegistryProtocolError::RegistryMismatch);
        }
    }
    let mut state = DiagnosticState {
        transport,
        options,
        request_count: 0,
        started: tokio::time::Instant::now(),
    };
    state.options.limits = limits;

    let mut report = RegistryDiagnosticReport {
        schema_version: REGISTRY_DIAGNOSTIC_SCHEMA_VERSION,
        status: RegistryDiagnosticStatus::Healthy,
        upstream: state.options.upstream.registry().clone(),
        upstream_origin: state.options.upstream.report_origin().to_owned(),
        image: state.options.image.clone(),
        requested_platform: state.options.platform.clone(),
        api: ApiCheck::default(),
        manifest: ManifestCheck::default(),
        blob_range: BlobRangeCheck::default(),
        mirrors: state
            .options
            .mirrors
            .iter()
            .enumerate()
            .map(|(order, mirror)| MirrorCheck {
                order,
                origin: mirror.report_origin().to_owned(),
                status: MirrorCheckStatus::NotTested,
                digest: None,
            })
            .collect(),
        request_count: 0,
    };

    let upstream = state.options.upstream.clone();
    let mut upstream_auth = None;
    let api_probe = probe_api(&mut state, &upstream).await;
    report.api = api_probe.check;
    if let (Some(challenge), Some(image)) = (api_probe.challenge, state.options.image.clone()) {
        let scope_matches =
            valid_challenge_scope(challenge.scope.as_deref(), image.repository().as_str());
        report.api.challenge_scope_matches = Some(scope_matches);
        if scope_matches {
            match obtain_anonymous_token(&mut state, &upstream, &challenge, &image).await {
                Ok(token) => upstream_auth = Some(token),
                Err(status) => report.api.status = status,
            }
        } else {
            report.api.status = ApiCheckStatus::AuthenticationRequired;
        }
    }

    if matches!(
        report.api.status,
        ApiCheckStatus::Available | ApiCheckStatus::BearerChallenge
    ) {
        if let Some(image) = state.options.image.clone() {
            let platform = state.options.platform.clone();
            let result = inspect_image(
                &mut state,
                &upstream,
                &image,
                platform.as_ref(),
                upstream_auth.as_ref(),
            )
            .await;
            report.manifest = result.manifest;
            report.blob_range = result.blob_range;

            if let Some(resolved) = result.resolved_digest {
                for (index, mirror) in state.options.mirrors.clone().into_iter().enumerate() {
                    report.mirrors[index] =
                        check_mirror(&mut state, index, &mirror, &image, &resolved).await;
                }
            }
        } else {
            report.manifest.status = ManifestCheckStatus::NotRequested;
        }
    }

    report.request_count = state.request_count;
    report.status = aggregate_status(&report);
    Ok(report)
}

impl<'a, T: RegistryTransport> DiagnosticState<'a, T> {
    async fn execute(
        &mut self,
        endpoint: &RegistryEndpoint,
        method: RegistryMethod,
        path: &str,
        authorization: Option<&BearerToken>,
        policy: RequestPolicy,
    ) -> Result<RegistryResponse, RegistryTransportError> {
        let mut url = endpoint
            .url_for(path)
            .map_err(|_| RegistryTransportError::Failed)?;
        let mut redirects = 0;
        let authorization_allowed = authorization.is_some_and(|token| token.is_bound_to_url(&url));
        loop {
            if self.request_count >= self.options.limits.max_requests {
                return Err(RegistryTransportError::RequestLimit);
            }
            let elapsed = self.started.elapsed();
            let remaining = self
                .options
                .limits
                .total_timeout
                .checked_sub(elapsed)
                .ok_or(RegistryTransportError::Timeout)?;
            let timeout = remaining.min(self.options.limits.request_timeout);
            let request_authorization = authorization.filter(|_| authorization_allowed);
            let request = RegistryRequest {
                method,
                url: url.clone(),
                accept: policy.accept,
                range: policy.range,
                authorization: request_authorization.cloned(),
                max_body_bytes: policy.max_body_bytes,
                timeout,
            };
            self.request_count += 1;
            let response = tokio::time::timeout(timeout, self.transport.execute(request))
                .await
                .map_err(|_| RegistryTransportError::Timeout)??;
            if response.body.len() > policy.max_body_bytes
                || (response.body_truncated && policy.range.is_none())
            {
                return Err(RegistryTransportError::BodyTooLarge);
            }
            if !(300..400).contains(&response.status) {
                return Ok(response);
            }
            if redirects >= self.options.limits.max_redirects {
                return Err(RegistryTransportError::RedirectRejected);
            }
            let location = response
                .one_header("location")
                .ok_or(RegistryTransportError::RedirectRejected)?;
            let next = url
                .join(location)
                .map_err(|_| RegistryTransportError::RedirectRejected)?;
            if next.scheme() != "https"
                || !endpoint.same_origin(&next)
                || !next.username().is_empty()
                || next.password().is_some()
                || next.fragment().is_some()
                || next.host_str().is_none()
            {
                return Err(RegistryTransportError::RedirectRejected);
            }
            redirects += 1;
            url = next;
        }
    }
}

struct ApiProbe {
    check: ApiCheck,
    challenge: Option<BearerChallenge>,
}

async fn probe_api<T: RegistryTransport>(
    state: &mut DiagnosticState<'_, T>,
    endpoint: &RegistryEndpoint,
) -> ApiProbe {
    let response = state
        .execute(
            endpoint,
            RegistryMethod::Get,
            "/v2/",
            None,
            RequestPolicy {
                accept: None,
                range: None,
                max_body_bytes: state.options.limits.max_body_bytes.min(8 * 1024),
            },
        )
        .await;
    let response = match response {
        Ok(response) => response,
        Err(error) => {
            return ApiProbe {
                check: ApiCheck {
                    status: api_transport_status(error),
                    ..ApiCheck::default()
                },
                challenge: None,
            };
        }
    };
    if response.status == 200 {
        return ApiProbe {
            check: ApiCheck {
                status: ApiCheckStatus::Available,
                http_status: Some(200),
                ..ApiCheck::default()
            },
            challenge: None,
        };
    }
    if response.status == 401 {
        return match parse_bearer_challenge(&response) {
            Some(challenge) => ApiProbe {
                check: ApiCheck {
                    status: ApiCheckStatus::BearerChallenge,
                    http_status: Some(401),
                    bearer_challenge: true,
                    challenge_service_present: challenge.service.is_some(),
                    challenge_scope_matches: None,
                },
                challenge: Some(challenge),
            },
            None => ApiProbe {
                check: ApiCheck {
                    status: ApiCheckStatus::InvalidChallenge,
                    http_status: Some(401),
                    ..ApiCheck::default()
                },
                challenge: None,
            },
        };
    }
    ApiProbe {
        check: ApiCheck {
            status: api_status_for_http(response.status),
            http_status: Some(response.status),
            ..ApiCheck::default()
        },
        challenge: None,
    }
}

async fn obtain_anonymous_token<T: RegistryTransport>(
    state: &mut DiagnosticState<'_, T>,
    endpoint: &RegistryEndpoint,
    challenge: &BearerChallenge,
    image: &ImageReference,
) -> Result<BearerToken, ApiCheckStatus> {
    if !valid_challenge_scope(challenge.scope.as_deref(), image.repository().as_str()) {
        return Err(ApiCheckStatus::AuthenticationRequired);
    }
    let realm =
        reqwest::Url::parse(&challenge.realm).map_err(|_| ApiCheckStatus::InvalidChallenge)?;
    if realm.scheme() != "https"
        || !endpoint.same_origin(&realm)
        || !realm.username().is_empty()
        || realm.password().is_some()
        || realm.fragment().is_some()
    {
        return Err(ApiCheckStatus::InvalidChallenge);
    }
    // Token realms are intentionally not followed by the generic registry
    // request path. Anonymous token exchange is performed by the production
    // transport only through a synthetic, tightly bounded GET.
    let mut token_url = realm;
    {
        let mut query = token_url.query_pairs_mut();
        if let Some(service) = challenge.service.as_deref() {
            query.append_pair("service", service);
        }
        query.append_pair(
            "scope",
            &format!("repository:{}:pull", image.repository().as_str()),
        );
    }
    let elapsed = state.started.elapsed();
    let remaining = state
        .options
        .limits
        .total_timeout
        .checked_sub(elapsed)
        .ok_or(ApiCheckStatus::TimedOut)?;
    let timeout = remaining.min(state.options.limits.request_timeout);
    if state.request_count >= state.options.limits.max_requests {
        return Err(ApiCheckStatus::RequestLimit);
    }
    let body_limit = state.options.limits.max_body_bytes.min(64 * 1024);
    state.request_count += 1;
    let response = tokio::time::timeout(
        timeout,
        state.transport.execute(RegistryRequest {
            method: RegistryMethod::Get,
            url: token_url,
            accept: Some("application/json"),
            range: None,
            authorization: None,
            max_body_bytes: body_limit,
            timeout,
        }),
    )
    .await
    .map_err(|_| ApiCheckStatus::TimedOut)?
    .map_err(api_transport_status)?;
    if response.status != 200 {
        return Err(api_status_for_http(response.status));
    }
    if response.body.len() > body_limit {
        return Err(ApiCheckStatus::BodyTooLarge);
    }
    #[derive(Deserialize)]
    struct TokenResponse {
        #[serde(default)]
        token: Option<String>,
        #[serde(default)]
        access_token: Option<String>,
    }
    let token: TokenResponse =
        serde_json::from_slice(&response.body).map_err(|_| ApiCheckStatus::UnexpectedResponse)?;
    let token = token
        .token
        .or(token.access_token)
        .ok_or(ApiCheckStatus::UnexpectedResponse)?;
    if token.is_empty()
        || token.len() > 16 * 1024
        || token.bytes().any(|byte| byte.is_ascii_control())
    {
        return Err(ApiCheckStatus::UnexpectedResponse);
    }
    Ok(BearerToken {
        value: token,
        bound_origin: endpoint.origin_key(),
    })
}

#[derive(Clone)]
struct BearerChallenge {
    realm: String,
    service: Option<String>,
    scope: Option<String>,
}

impl fmt::Debug for BearerChallenge {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BearerChallenge")
            .field("realm", &"[redacted]")
            .field("service_present", &self.service.is_some())
            .field("scope_present", &self.scope.is_some())
            .finish()
    }
}

fn parse_bearer_challenge(response: &RegistryResponse) -> Option<BearerChallenge> {
    response
        .header_values("www-authenticate")
        .find_map(parse_one_bearer_challenge)
}

fn parse_one_bearer_challenge(value: &str) -> Option<BearerChallenge> {
    if value.len() > HARD_MAX_CHALLENGE_BYTES {
        return None;
    }
    let (scheme, parameters) = value.trim().split_once(char::is_whitespace)?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let parameters = parse_auth_parameters(parameters)?;
    let realm = parameters.get("realm")?.to_owned();
    let realm_url = reqwest::Url::parse(&realm).ok()?;
    if realm.is_empty()
        || realm_url.scheme() != "https"
        || realm_url.host_str().is_none()
        || !realm_url.username().is_empty()
        || realm_url.password().is_some()
        || realm_url.fragment().is_some()
    {
        return None;
    }
    Some(BearerChallenge {
        realm,
        service: parameters.get("service").cloned(),
        scope: parameters.get("scope").cloned(),
    })
}

fn parse_auth_parameters(value: &str) -> Option<BTreeMap<String, String>> {
    let mut result = BTreeMap::new();
    let bytes = value.as_bytes();
    let mut cursor = 0;
    while cursor < bytes.len() {
        while bytes
            .get(cursor)
            .is_some_and(|byte| byte.is_ascii_whitespace() || *byte == b',')
        {
            cursor += 1;
        }
        let key_start = cursor;
        while bytes
            .get(cursor)
            .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'-')
        {
            cursor += 1;
        }
        if cursor == key_start || bytes.get(cursor) != Some(&b'=') {
            return None;
        }
        let key = value.get(key_start..cursor)?.to_ascii_lowercase();
        cursor += 1;
        if bytes.get(cursor) != Some(&b'"') {
            return None;
        }
        cursor += 1;
        let mut parsed = String::new();
        let mut closed = false;
        while let Some(byte) = bytes.get(cursor).copied() {
            cursor += 1;
            match byte {
                b'"' => {
                    closed = true;
                    break;
                }
                b'\\' => {
                    let escaped = bytes.get(cursor).copied()?;
                    cursor += 1;
                    if !escaped.is_ascii() || escaped.is_ascii_control() {
                        return None;
                    }
                    parsed.push(char::from(escaped));
                }
                byte if byte.is_ascii_control() || !byte.is_ascii() => return None,
                byte => parsed.push(char::from(byte)),
            }
        }
        if !closed || result.insert(key, parsed).is_some() {
            return None;
        }
        while bytes.get(cursor).is_some_and(u8::is_ascii_whitespace) {
            cursor += 1;
        }
        if cursor < bytes.len() && bytes[cursor] != b',' {
            return None;
        }
    }
    Some(result)
}

fn valid_challenge_scope(scope: Option<&str>, repository: &str) -> bool {
    match scope {
        None => true,
        Some(scope) => scope == format!("repository:{repository}:pull"),
    }
}

struct ImageInspection {
    manifest: ManifestCheck,
    blob_range: BlobRangeCheck,
    resolved_digest: Option<OciDigest>,
}

async fn inspect_image<T: RegistryTransport>(
    state: &mut DiagnosticState<'_, T>,
    endpoint: &RegistryEndpoint,
    image: &ImageReference,
    platform: Option<&OciPlatform>,
    authorization: Option<&BearerToken>,
) -> ImageInspection {
    let initial_path = manifest_path(image, image.selector().as_str());
    let initial = fetch_manifest(state, endpoint, &initial_path, authorization, None).await;
    let mut manifest = match initial {
        Ok(manifest) => manifest,
        Err(status) => {
            return ImageInspection {
                manifest: ManifestCheck {
                    status,
                    ..ManifestCheck::default()
                },
                blob_range: BlobRangeCheck::default(),
                resolved_digest: None,
            };
        }
    };
    if let ImageSelector::Digest(expected) = image.selector() {
        if &manifest.digest != expected {
            return ImageInspection {
                manifest: ManifestCheck {
                    status: ManifestCheckStatus::DigestMismatch,
                    digest: Some(manifest.digest),
                    byte_size: Some(manifest.body.len() as u64),
                    ..ManifestCheck::default()
                },
                blob_range: BlobRangeCheck::default(),
                resolved_digest: None,
            };
        }
    }
    let resolved_digest = manifest.digest.clone();
    let top_kind = manifest.kind;
    let top_media_type = manifest.media_type.clone();
    let top_byte_size = manifest.body.len() as u64;
    let mut selected_platform = None;
    let mut selected_os_version = None;
    let mut child_digest = None;

    if manifest.kind == ManifestKind::Index {
        let Some(platform) = platform else {
            return ImageInspection {
                manifest: ManifestCheck {
                    status: ManifestCheckStatus::PlatformNotFound,
                    kind: Some(ManifestKind::Index),
                    media_type: Some(manifest.media_type),
                    digest: Some(resolved_digest.clone()),
                    byte_size: Some(manifest.body.len() as u64),
                    ..ManifestCheck::default()
                },
                blob_range: BlobRangeCheck::default(),
                resolved_digest: Some(resolved_digest),
            };
        };
        let descriptor = match select_platform_descriptor(&manifest.body, platform) {
            Some(descriptor) => descriptor,
            None => {
                return ImageInspection {
                    manifest: ManifestCheck {
                        status: ManifestCheckStatus::PlatformNotFound,
                        kind: Some(ManifestKind::Index),
                        media_type: Some(manifest.media_type),
                        digest: Some(resolved_digest.clone()),
                        byte_size: Some(manifest.body.len() as u64),
                        ..ManifestCheck::default()
                    },
                    blob_range: BlobRangeCheck::default(),
                    resolved_digest: Some(resolved_digest),
                };
            }
        };
        let path = manifest_path(image, descriptor.digest.as_str());
        let child = fetch_manifest(
            state,
            endpoint,
            &path,
            authorization,
            Some((&descriptor.digest, descriptor.size)),
        )
        .await;
        manifest = match child {
            Ok(child) if child.kind == ManifestKind::Image => child,
            Ok(_) => {
                return ImageInspection {
                    manifest: ManifestCheck {
                        status: ManifestCheckStatus::InvalidMediaType,
                        kind: Some(ManifestKind::Index),
                        digest: Some(resolved_digest.clone()),
                        child_digest: Some(descriptor.digest),
                        selected_platform: Some(platform.clone()),
                        selected_os_version: descriptor
                            .platform
                            .as_ref()
                            .and_then(|platform| platform.os_version.clone()),
                        ..ManifestCheck::default()
                    },
                    blob_range: BlobRangeCheck::default(),
                    resolved_digest: Some(resolved_digest),
                };
            }
            Err(status) => {
                return ImageInspection {
                    manifest: ManifestCheck {
                        status,
                        kind: Some(ManifestKind::Index),
                        digest: Some(resolved_digest.clone()),
                        child_digest: Some(descriptor.digest),
                        selected_platform: Some(platform.clone()),
                        selected_os_version: descriptor
                            .platform
                            .as_ref()
                            .and_then(|platform| platform.os_version.clone()),
                        ..ManifestCheck::default()
                    },
                    blob_range: BlobRangeCheck::default(),
                    resolved_digest: Some(resolved_digest),
                };
            }
        };
        child_digest = Some(descriptor.digest);
        selected_platform = Some(platform.clone());
        selected_os_version = descriptor.platform.and_then(|platform| platform.os_version);
    }

    let blob = first_layer_descriptor(&manifest.body)
        .expect("a verified image manifest has at least one valid layer");
    let blob_range = check_blob_range(state, endpoint, image, authorization, blob).await;
    ImageInspection {
        manifest: ManifestCheck {
            status: ManifestCheckStatus::Verified,
            kind: Some(top_kind),
            media_type: Some(top_media_type),
            digest: Some(resolved_digest.clone()),
            child_digest,
            selected_platform,
            selected_os_version,
            byte_size: Some(top_byte_size),
        },
        blob_range,
        resolved_digest: Some(resolved_digest),
    }
}

struct VerifiedManifest {
    kind: ManifestKind,
    media_type: String,
    digest: OciDigest,
    body: Vec<u8>,
}

async fn fetch_manifest<T: RegistryTransport>(
    state: &mut DiagnosticState<'_, T>,
    endpoint: &RegistryEndpoint,
    path: &str,
    authorization: Option<&BearerToken>,
    expected: Option<(&OciDigest, u64)>,
) -> Result<VerifiedManifest, ManifestCheckStatus> {
    let response = state
        .execute(
            endpoint,
            RegistryMethod::Get,
            path,
            authorization,
            RequestPolicy {
                accept: Some(ACCEPT_MANIFESTS),
                range: None,
                max_body_bytes: state.options.limits.max_manifest_bytes,
            },
        )
        .await
        .map_err(manifest_transport_status)?;
    if response.status != 200 {
        return Err(manifest_status_for_http(response.status));
    }
    if response.body.len() > state.options.limits.max_manifest_bytes {
        return Err(ManifestCheckStatus::BodyTooLarge);
    }
    if let Some(content_length) = response.one_header("content-length") {
        if content_length.parse::<u64>().ok() != Some(response.body.len() as u64) {
            return Err(ManifestCheckStatus::SizeMismatch);
        }
    }
    let digest = sha256_digest(&response.body).map_err(|_| ManifestCheckStatus::InvalidManifest)?;
    if let Some(header) = response.one_header("docker-content-digest") {
        if OciDigest::parse(header).ok().as_ref() != Some(&digest) {
            return Err(ManifestCheckStatus::DigestMismatch);
        }
    }
    if let Some((expected_digest, expected_size)) = expected {
        if expected_digest != &digest {
            return Err(ManifestCheckStatus::DigestMismatch);
        }
        if expected_size != response.body.len() as u64 {
            return Err(ManifestCheckStatus::SizeMismatch);
        }
    }
    let media_type = response
        .one_header("content-type")
        .and_then(parse_media_type)
        .ok_or(ManifestCheckStatus::InvalidMediaType)?;
    let kind = manifest_kind(&media_type).ok_or(ManifestCheckStatus::InvalidMediaType)?;
    let body_value: ManifestEnvelope =
        serde_json::from_slice(&response.body).map_err(|_| ManifestCheckStatus::InvalidManifest)?;
    if body_value.schema_version != 2 || body_value.media_type.as_deref() != Some(&media_type) {
        return Err(ManifestCheckStatus::InvalidManifest);
    }
    let structurally_valid = match kind {
        ManifestKind::Image => validate_image_manifest(&response.body),
        ManifestKind::Index => validate_image_index(&response.body),
    };
    if !structurally_valid {
        return Err(ManifestCheckStatus::InvalidManifest);
    }
    Ok(VerifiedManifest {
        kind,
        media_type,
        digest,
        body: response.body,
    })
}

fn manifest_kind(media_type: &str) -> Option<ManifestKind> {
    match media_type {
        OCI_INDEX | DOCKER_INDEX => Some(ManifestKind::Index),
        OCI_MANIFEST | DOCKER_MANIFEST => Some(ManifestKind::Image),
        _ => None,
    }
}

fn parse_media_type(value: &str) -> Option<String> {
    let media_type = value.split(';').next()?.trim().to_ascii_lowercase();
    manifest_kind(&media_type).map(|_| media_type)
}

#[derive(Deserialize)]
struct ManifestEnvelope {
    #[serde(rename = "schemaVersion")]
    schema_version: u32,
    #[serde(rename = "mediaType", default)]
    media_type: Option<String>,
}

#[derive(Deserialize)]
struct ImageIndex {
    manifests: Vec<Descriptor>,
}

#[derive(Deserialize)]
struct ImageManifest {
    config: Descriptor,
    layers: Vec<Descriptor>,
}

#[derive(Clone, Deserialize)]
struct Descriptor {
    #[serde(rename = "mediaType")]
    media_type: String,
    digest: OciDigest,
    size: u64,
    #[serde(default)]
    platform: Option<DescriptorPlatform>,
}

#[derive(Clone, Deserialize)]
struct DescriptorPlatform {
    os: String,
    architecture: String,
    #[serde(default)]
    variant: Option<String>,
    #[serde(default, rename = "os.version")]
    os_version: Option<String>,
    #[serde(default, rename = "os.features")]
    os_features: Vec<String>,
}

fn select_platform_descriptor(body: &[u8], requested: &OciPlatform) -> Option<Descriptor> {
    let index: ImageIndex = serde_json::from_slice(body).ok()?;
    let mut candidates = index.manifests.into_iter().filter(|descriptor| {
        if !valid_manifest_descriptor(descriptor) {
            return false;
        }
        let Some(platform) = &descriptor.platform else {
            return false;
        };
        if !platform.os_features.is_empty() {
            return false;
        }
        OciPlatform::new(
            &platform.os,
            &platform.architecture,
            platform.variant.as_deref(),
        )
        .is_ok_and(|candidate| &candidate == requested)
    });
    let selected = candidates.next()?;
    candidates.next().is_none().then_some(selected)
}

fn first_layer_descriptor(body: &[u8]) -> Option<Descriptor> {
    let manifest = serde_json::from_slice::<ImageManifest>(body).ok()?;
    if !valid_config_descriptor(&manifest.config)
        || manifest.layers.is_empty()
        || manifest
            .layers
            .iter()
            .any(|layer| !valid_layer_descriptor(layer))
    {
        return None;
    }
    manifest
        .layers
        .into_iter()
        .min_by_key(|descriptor| descriptor.size)
}

fn validate_image_manifest(body: &[u8]) -> bool {
    let Ok(manifest) = serde_json::from_slice::<ImageManifest>(body) else {
        return false;
    };
    valid_config_descriptor(&manifest.config)
        && !manifest.layers.is_empty()
        && manifest.layers.iter().all(valid_layer_descriptor)
}

fn validate_image_index(body: &[u8]) -> bool {
    serde_json::from_slice::<ImageIndex>(body).is_ok_and(|index| {
        !index.manifests.is_empty() && index.manifests.iter().all(valid_manifest_descriptor)
    })
}

fn valid_manifest_descriptor(descriptor: &Descriptor) -> bool {
    descriptor.size > 0
        && matches!(
            descriptor.media_type.as_str(),
            OCI_INDEX | OCI_MANIFEST | DOCKER_INDEX | DOCKER_MANIFEST
        )
}

fn valid_config_descriptor(descriptor: &Descriptor) -> bool {
    descriptor.size > 0
        && matches!(
            descriptor.media_type.as_str(),
            "application/vnd.oci.image.config.v1+json"
                | "application/vnd.docker.container.image.v1+json"
        )
        && descriptor.platform.is_none()
}

fn valid_layer_descriptor(descriptor: &Descriptor) -> bool {
    descriptor.size > 0
        && descriptor.platform.is_none()
        && matches!(
            descriptor.media_type.as_str(),
            "application/vnd.oci.image.layer.v1.tar"
                | "application/vnd.oci.image.layer.v1.tar+gzip"
                | "application/vnd.oci.image.layer.v1.tar+zstd"
                | "application/vnd.oci.image.layer.nondistributable.v1.tar"
                | "application/vnd.oci.image.layer.nondistributable.v1.tar+gzip"
                | "application/vnd.oci.image.layer.nondistributable.v1.tar+zstd"
                | "application/vnd.docker.image.rootfs.diff.tar.gzip"
                | "application/vnd.docker.image.rootfs.foreign.diff.tar.gzip"
        )
}

async fn check_blob_range<T: RegistryTransport>(
    state: &mut DiagnosticState<'_, T>,
    endpoint: &RegistryEndpoint,
    image: &ImageReference,
    authorization: Option<&BearerToken>,
    descriptor: Descriptor,
) -> BlobRangeCheck {
    let requested = descriptor
        .size
        .min(state.options.limits.blob_sample_bytes as u64);
    let mut check = BlobRangeCheck {
        digest: Some(descriptor.digest.clone()),
        requested_bytes: requested,
        ..BlobRangeCheck::default()
    };
    if requested == 0 {
        check.status = BlobRangeStatus::Malformed;
        return check;
    }
    let path = blob_path(image, descriptor.digest.as_str());
    let response = state
        .execute(
            endpoint,
            RegistryMethod::Get,
            &path,
            authorization,
            RequestPolicy {
                accept: None,
                range: Some((0, requested - 1)),
                max_body_bytes: state.options.limits.blob_sample_bytes,
            },
        )
        .await;
    let response = match response {
        Ok(response) => response,
        Err(error) => {
            check.status = blob_transport_status(error);
            return check;
        }
    };
    check.received_bytes = response.body.len() as u64;
    match response.status {
        206 => {
            let content_range = response
                .one_header("content-range")
                .and_then(parse_content_range);
            if content_range != Some((0, requested - 1, descriptor.size))
                || response.body.len() as u64 != requested
            {
                check.status = BlobRangeStatus::Malformed;
            } else {
                check.status = BlobRangeStatus::Supported;
                check.total_bytes = Some(descriptor.size);
                if requested == descriptor.size
                    && sha256_digest(&response.body).ok().as_ref() != Some(&descriptor.digest)
                {
                    check.status = BlobRangeStatus::DigestMismatch;
                }
            }
        }
        200 => check.status = BlobRangeStatus::Ignored,
        416 => check.status = BlobRangeStatus::Unsatisfiable,
        status => check.status = blob_status_for_http(status),
    }
    check
}

fn parse_content_range(value: &str) -> Option<(u64, u64, u64)> {
    let range = value.strip_prefix("bytes ")?;
    let (bounds, total) = range.split_once('/')?;
    let (start, end) = bounds.split_once('-')?;
    let start = start.parse().ok()?;
    let end = end.parse().ok()?;
    let total = total.parse().ok()?;
    (start <= end && end < total).then_some((start, end, total))
}

async fn check_mirror<T: RegistryTransport>(
    state: &mut DiagnosticState<'_, T>,
    order: usize,
    mirror: &RegistryEndpoint,
    image: &ImageReference,
    expected: &OciDigest,
) -> MirrorCheck {
    let api_probe = probe_api(state, mirror).await;
    let authorization = match api_probe.check.status {
        ApiCheckStatus::Available => None,
        ApiCheckStatus::BearerChallenge => {
            let Some(challenge) = api_probe.challenge else {
                return mirror_failure(order, mirror, MirrorCheckStatus::InvalidResponse);
            };
            if !valid_challenge_scope(challenge.scope.as_deref(), image.repository().as_str()) {
                return mirror_failure(order, mirror, MirrorCheckStatus::AuthenticationRequired);
            }
            match obtain_anonymous_token(state, mirror, &challenge, image).await {
                Ok(token) => Some(token),
                Err(status) => {
                    return mirror_failure(order, mirror, mirror_status_from_api(status));
                }
            }
        }
        ApiCheckStatus::AuthenticationRequired | ApiCheckStatus::InvalidChallenge => {
            return mirror_failure(order, mirror, MirrorCheckStatus::AuthenticationRequired);
        }
        ApiCheckStatus::AccessDenied => {
            return mirror_failure(order, mirror, MirrorCheckStatus::AccessDenied);
        }
        ApiCheckStatus::RateLimited => {
            return mirror_failure(order, mirror, MirrorCheckStatus::RateLimited);
        }
        ApiCheckStatus::NotFound => {
            return mirror_failure(order, mirror, MirrorCheckStatus::NotFound);
        }
        ApiCheckStatus::ServerError => {
            return mirror_failure(order, mirror, MirrorCheckStatus::ServerError);
        }
        ApiCheckStatus::RedirectRejected => {
            return mirror_failure(order, mirror, MirrorCheckStatus::RedirectRejected);
        }
        ApiCheckStatus::TimedOut => {
            return mirror_failure(order, mirror, MirrorCheckStatus::TimedOut);
        }
        ApiCheckStatus::BodyTooLarge => {
            return mirror_failure(order, mirror, MirrorCheckStatus::BodyTooLarge);
        }
        ApiCheckStatus::RequestLimit => {
            return mirror_failure(order, mirror, MirrorCheckStatus::RequestLimit);
        }
        ApiCheckStatus::Unreachable => {
            return mirror_failure(order, mirror, MirrorCheckStatus::Unreachable);
        }
        ApiCheckStatus::UnexpectedResponse | ApiCheckStatus::NotTested => {
            return mirror_failure(order, mirror, MirrorCheckStatus::InvalidResponse);
        }
    };
    let path = manifest_path(image, expected.as_str());
    let result = fetch_manifest(state, mirror, &path, authorization.as_ref(), None).await;
    match result {
        Ok(manifest) => MirrorCheck {
            order,
            origin: mirror.report_origin().to_owned(),
            status: if &manifest.digest == expected {
                MirrorCheckStatus::Equivalent
            } else {
                MirrorCheckStatus::Diverged
            },
            digest: Some(manifest.digest),
        },
        Err(status) => MirrorCheck {
            order,
            origin: mirror.report_origin().to_owned(),
            status: mirror_status_from_manifest(status),
            digest: None,
        },
    }
}

fn mirror_failure(
    order: usize,
    mirror: &RegistryEndpoint,
    status: MirrorCheckStatus,
) -> MirrorCheck {
    MirrorCheck {
        order,
        origin: mirror.report_origin().to_owned(),
        status,
        digest: None,
    }
}

fn manifest_path(image: &ImageReference, selector: &str) -> String {
    format!("/v2/{}/manifests/{selector}", image.repository().as_str())
}

fn blob_path(image: &ImageReference, digest: &str) -> String {
    format!("/v2/{}/blobs/{digest}", image.repository().as_str())
}

fn sha256_digest(body: &[u8]) -> Result<OciDigest, RegistryProtocolError> {
    OciDigest::parse(&format!("sha256:{:x}", Sha256::digest(body)))
        .map_err(|_| RegistryProtocolError::InvalidPath)
}

fn redacted_origin(url: &reqwest::Url) -> String {
    let host = url.host_str().unwrap_or("invalid");
    let authority = registry_authority(host, url.port()).unwrap_or_else(|_| "invalid".to_owned());
    format!("{}://{authority}", url.scheme())
}

fn api_transport_status(error: RegistryTransportError) -> ApiCheckStatus {
    match error {
        RegistryTransportError::Timeout => ApiCheckStatus::TimedOut,
        RegistryTransportError::BodyTooLarge => ApiCheckStatus::BodyTooLarge,
        RegistryTransportError::RedirectRejected => ApiCheckStatus::RedirectRejected,
        RegistryTransportError::RequestLimit => ApiCheckStatus::RequestLimit,
        RegistryTransportError::Failed => ApiCheckStatus::Unreachable,
    }
}

fn manifest_transport_status(error: RegistryTransportError) -> ManifestCheckStatus {
    match error {
        RegistryTransportError::Timeout => ManifestCheckStatus::TimedOut,
        RegistryTransportError::BodyTooLarge => ManifestCheckStatus::BodyTooLarge,
        RegistryTransportError::RedirectRejected => ManifestCheckStatus::RedirectRejected,
        RegistryTransportError::RequestLimit => ManifestCheckStatus::RequestLimit,
        RegistryTransportError::Failed => ManifestCheckStatus::Unreachable,
    }
}

fn blob_transport_status(error: RegistryTransportError) -> BlobRangeStatus {
    match error {
        RegistryTransportError::Timeout => BlobRangeStatus::TimedOut,
        RegistryTransportError::BodyTooLarge => BlobRangeStatus::BodyTooLarge,
        RegistryTransportError::RedirectRejected => BlobRangeStatus::RedirectRejected,
        RegistryTransportError::RequestLimit => BlobRangeStatus::RequestLimit,
        RegistryTransportError::Failed => BlobRangeStatus::Unreachable,
    }
}

fn api_status_for_http(status: u16) -> ApiCheckStatus {
    match status {
        401 => ApiCheckStatus::AuthenticationRequired,
        403 => ApiCheckStatus::AccessDenied,
        404 => ApiCheckStatus::NotFound,
        429 => ApiCheckStatus::RateLimited,
        500..=599 => ApiCheckStatus::ServerError,
        _ => ApiCheckStatus::UnexpectedResponse,
    }
}

fn manifest_status_for_http(status: u16) -> ManifestCheckStatus {
    match status {
        401 => ManifestCheckStatus::AuthenticationRequired,
        403 => ManifestCheckStatus::AccessDenied,
        404 => ManifestCheckStatus::NotFound,
        429 => ManifestCheckStatus::RateLimited,
        500..=599 => ManifestCheckStatus::ServerError,
        _ => ManifestCheckStatus::InvalidManifest,
    }
}

fn blob_status_for_http(status: u16) -> BlobRangeStatus {
    match status {
        401 => BlobRangeStatus::AuthenticationRequired,
        403 => BlobRangeStatus::AccessDenied,
        404 => BlobRangeStatus::NotFound,
        429 => BlobRangeStatus::RateLimited,
        500..=599 => BlobRangeStatus::ServerError,
        _ => BlobRangeStatus::Malformed,
    }
}

fn mirror_status_from_api(status: ApiCheckStatus) -> MirrorCheckStatus {
    match status {
        ApiCheckStatus::AuthenticationRequired | ApiCheckStatus::InvalidChallenge => {
            MirrorCheckStatus::AuthenticationRequired
        }
        ApiCheckStatus::AccessDenied => MirrorCheckStatus::AccessDenied,
        ApiCheckStatus::RateLimited => MirrorCheckStatus::RateLimited,
        ApiCheckStatus::NotFound => MirrorCheckStatus::NotFound,
        ApiCheckStatus::ServerError => MirrorCheckStatus::ServerError,
        ApiCheckStatus::RedirectRejected => MirrorCheckStatus::RedirectRejected,
        ApiCheckStatus::Unreachable => MirrorCheckStatus::Unreachable,
        ApiCheckStatus::TimedOut => MirrorCheckStatus::TimedOut,
        ApiCheckStatus::BodyTooLarge => MirrorCheckStatus::BodyTooLarge,
        ApiCheckStatus::RequestLimit => MirrorCheckStatus::RequestLimit,
        ApiCheckStatus::Available
        | ApiCheckStatus::BearerChallenge
        | ApiCheckStatus::UnexpectedResponse
        | ApiCheckStatus::NotTested => MirrorCheckStatus::InvalidResponse,
    }
}

fn mirror_status_from_manifest(status: ManifestCheckStatus) -> MirrorCheckStatus {
    match status {
        ManifestCheckStatus::AuthenticationRequired => MirrorCheckStatus::AuthenticationRequired,
        ManifestCheckStatus::AccessDenied => MirrorCheckStatus::AccessDenied,
        ManifestCheckStatus::RateLimited => MirrorCheckStatus::RateLimited,
        ManifestCheckStatus::NotFound => MirrorCheckStatus::NotFound,
        ManifestCheckStatus::ServerError => MirrorCheckStatus::ServerError,
        ManifestCheckStatus::RedirectRejected => MirrorCheckStatus::RedirectRejected,
        ManifestCheckStatus::Unreachable => MirrorCheckStatus::Unreachable,
        ManifestCheckStatus::TimedOut => MirrorCheckStatus::TimedOut,
        ManifestCheckStatus::BodyTooLarge => MirrorCheckStatus::BodyTooLarge,
        ManifestCheckStatus::RequestLimit => MirrorCheckStatus::RequestLimit,
        _ => MirrorCheckStatus::InvalidResponse,
    }
}

fn aggregate_status(report: &RegistryDiagnosticReport) -> RegistryDiagnosticStatus {
    match report.api.status {
        ApiCheckStatus::AuthenticationRequired | ApiCheckStatus::InvalidChallenge => {
            return RegistryDiagnosticStatus::AuthenticationRequired;
        }
        ApiCheckStatus::AccessDenied => return RegistryDiagnosticStatus::AccessDenied,
        ApiCheckStatus::RateLimited => return RegistryDiagnosticStatus::RateLimited,
        ApiCheckStatus::NotFound => return RegistryDiagnosticStatus::NotFound,
        ApiCheckStatus::Unreachable => return RegistryDiagnosticStatus::Unreachable,
        ApiCheckStatus::TimedOut => return RegistryDiagnosticStatus::TimedOut,
        ApiCheckStatus::RequestLimit => return RegistryDiagnosticStatus::LimitExceeded,
        ApiCheckStatus::BodyTooLarge
        | ApiCheckStatus::ServerError
        | ApiCheckStatus::RedirectRejected
        | ApiCheckStatus::UnexpectedResponse
        | ApiCheckStatus::NotTested => return RegistryDiagnosticStatus::ProtocolError,
        ApiCheckStatus::Available | ApiCheckStatus::BearerChallenge => {}
    }
    match report.manifest.status {
        ManifestCheckStatus::DigestMismatch | ManifestCheckStatus::SizeMismatch => {
            return RegistryDiagnosticStatus::Corrupt;
        }
        ManifestCheckStatus::AuthenticationRequired => {
            return RegistryDiagnosticStatus::AuthenticationRequired;
        }
        ManifestCheckStatus::AccessDenied => return RegistryDiagnosticStatus::AccessDenied,
        ManifestCheckStatus::RateLimited => return RegistryDiagnosticStatus::RateLimited,
        ManifestCheckStatus::NotFound => return RegistryDiagnosticStatus::NotFound,
        ManifestCheckStatus::Unreachable => return RegistryDiagnosticStatus::Unreachable,
        ManifestCheckStatus::TimedOut => return RegistryDiagnosticStatus::TimedOut,
        ManifestCheckStatus::RequestLimit => return RegistryDiagnosticStatus::LimitExceeded,
        ManifestCheckStatus::InvalidMediaType
        | ManifestCheckStatus::InvalidManifest
        | ManifestCheckStatus::BodyTooLarge
        | ManifestCheckStatus::ServerError
        | ManifestCheckStatus::RedirectRejected => {
            return RegistryDiagnosticStatus::ProtocolError;
        }
        ManifestCheckStatus::PlatformNotFound => return RegistryDiagnosticStatus::Unsupported,
        ManifestCheckStatus::Verified
        | ManifestCheckStatus::NotRequested
        | ManifestCheckStatus::NotTested => {}
    }
    if report
        .mirrors
        .iter()
        .any(|mirror| mirror.status == MirrorCheckStatus::Diverged)
    {
        return RegistryDiagnosticStatus::Corrupt;
    }
    if report.blob_range.status == BlobRangeStatus::DigestMismatch {
        return RegistryDiagnosticStatus::Corrupt;
    }
    if !matches!(
        report.blob_range.status,
        BlobRangeStatus::Supported | BlobRangeStatus::NotAvailable | BlobRangeStatus::NotTested
    ) || report
        .mirrors
        .iter()
        .any(|mirror| mirror.status != MirrorCheckStatus::Equivalent)
    {
        RegistryDiagnosticStatus::Degraded
    } else {
        RegistryDiagnosticStatus::Healthy
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    use super::*;

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct SeenRequest {
        method: RegistryMethod,
        origin: String,
        path: String,
        query: Option<String>,
        accept: Option<&'static str>,
        range: Option<(u64, u64)>,
        authorized: bool,
        max_body_bytes: usize,
    }

    #[derive(Clone)]
    struct MockTransport {
        steps: Arc<Mutex<VecDeque<MockStep>>>,
        seen: Arc<Mutex<Vec<SeenRequest>>>,
    }

    enum MockStep {
        Response(RegistryResponse),
        Error(RegistryTransportError),
    }

    impl MockTransport {
        fn new(steps: impl IntoIterator<Item = MockStep>) -> Self {
            Self {
                steps: Arc::new(Mutex::new(steps.into_iter().collect())),
                seen: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn seen(&self) -> Vec<SeenRequest> {
            self.seen.lock().unwrap().clone()
        }
    }

    impl RegistryTransport for MockTransport {
        fn execute(&self, request: RegistryRequest) -> RegistryTransportFuture<'_> {
            self.seen.lock().unwrap().push(SeenRequest {
                method: request.method,
                origin: redacted_origin(&request.url),
                path: request.url.path().to_owned(),
                query: request.url.query().map(str::to_owned),
                accept: request.accept,
                range: request.range,
                authorized: request.has_authorization(),
                max_body_bytes: request.max_body_bytes,
            });
            let result = self
                .steps
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(MockStep::Error(RegistryTransportError::Failed));
            Box::pin(async move {
                match result {
                    MockStep::Response(response) => Ok(response),
                    MockStep::Error(error) => Err(error),
                }
            })
        }
    }

    fn endpoint(host: &str) -> RegistryEndpoint {
        RegistryEndpoint::parse_https(&format!("https://{host}")).unwrap()
    }

    fn image(value: &str) -> ImageReference {
        ImageReference::parse(value).unwrap()
    }

    fn response(status: u16) -> MockStep {
        MockStep::Response(RegistryResponse::new(status))
    }

    fn manifest_response(body: Vec<u8>) -> MockStep {
        let digest = sha256_digest(&body).unwrap();
        MockStep::Response(
            RegistryResponse::new(200)
                .header("content-type", OCI_MANIFEST)
                .header("docker-content-digest", digest.as_str())
                .body(body),
        )
    }

    fn index_response(body: Vec<u8>) -> MockStep {
        let digest = sha256_digest(&body).unwrap();
        MockStep::Response(
            RegistryResponse::new(200)
                .header("content-type", OCI_INDEX)
                .header("docker-content-digest", digest.as_str())
                .body(body),
        )
    }

    fn image_manifest(layer: &[u8]) -> (Vec<u8>, OciDigest) {
        let digest = sha256_digest(layer).unwrap();
        let body = serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 2,
            "mediaType": OCI_MANIFEST,
            "config": {
                "mediaType": "application/vnd.oci.image.config.v1+json",
                "digest": digest,
                "size": layer.len()
            },
            "layers": [{
                "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
                "digest": digest,
                "size": layer.len()
            }]
        }))
        .unwrap();
        (body, digest)
    }

    fn options(image: Option<ImageReference>) -> RegistryDiagnosticOptions {
        let mut options = RegistryDiagnosticOptions::new(endpoint("registry.example"));
        options.image = image;
        options
    }

    #[test]
    fn endpoints_are_https_only_and_redacted() {
        assert_eq!(
            RegistryEndpoint::parse_https("http://registry.example"),
            Err(RegistryProtocolError::InsecureEndpoint)
        );
        assert_eq!(
            RegistryEndpoint::parse_https("https://alice:secret@registry.example"),
            Err(RegistryProtocolError::InvalidEndpoint)
        );
        for invalid in [
            "https://registry.example?token=secret",
            "https://registry.example#fragment",
            "https://registry.example/%2e%2e/private",
            "https://registry.example/a/../private",
        ] {
            assert!(RegistryEndpoint::parse_https(invalid).is_err(), "{invalid}");
        }
        assert!(RegistryEndpoint::parse_loopback_http("http://127.0.0.1:5000").is_ok());
        assert!(RegistryEndpoint::parse_loopback_http("http://[::1]:5000").is_ok());
        assert!(RegistryEndpoint::parse_loopback_http("http://example.test:5000").is_err());
        assert!(endpoint("registry.example").is_https());

        let endpoint = RegistryEndpoint::parse_https("https://registry.example/prefix").unwrap();
        assert_eq!(endpoint.report_origin(), "https://registry.example");
        let debug = format!("{endpoint:?}");
        assert!(!debug.contains("/prefix"));
        assert!(debug.contains("has_path_prefix"));

        let docker =
            RegistryEndpoint::for_registry(RegistryName::parse("docker.io").unwrap()).unwrap();
        assert_eq!(docker.registry().as_str(), "docker.io");
        assert_eq!(docker.report_origin(), "https://registry-1.docker.io");

        let oversized = format!(
            "https://example.test/{}",
            "a".repeat(HARD_MAX_ENDPOINT_BYTES)
        );
        assert_eq!(
            RegistryEndpoint::parse_https(&oversized),
            Err(RegistryProtocolError::InvalidEndpoint)
        );
    }

    #[tokio::test]
    async fn v2_statuses_and_bearer_challenges_are_typed() {
        for (status, expected) in [
            (200, ApiCheckStatus::Available),
            (403, ApiCheckStatus::AccessDenied),
            (404, ApiCheckStatus::NotFound),
            (429, ApiCheckStatus::RateLimited),
            (503, ApiCheckStatus::ServerError),
        ] {
            let transport = MockTransport::new([response(status)]);
            let report = diagnose_registry(&transport, options(None)).await.unwrap();
            assert_eq!(report.api.status, expected);
            assert_eq!(report.api.http_status, Some(status));
            assert_eq!(report.request_count, 1);
        }

        let challenge = RegistryResponse::new(401).header(
            "www-authenticate",
            r#"Bearer realm="https://registry.example/token",service="registry.example""#,
        );
        let transport = MockTransport::new([MockStep::Response(challenge)]);
        let report = diagnose_registry(&transport, options(None)).await.unwrap();
        assert_eq!(report.status, RegistryDiagnosticStatus::Healthy);
        assert_eq!(report.api.status, ApiCheckStatus::BearerChallenge);
        assert!(report.api.bearer_challenge);
        assert!(report.api.challenge_service_present);

        let transport = MockTransport::new([MockStep::Response(
            RegistryResponse::new(401).header("www-authenticate", "Basic realm=\"private\""),
        )]);
        let report = diagnose_registry(&transport, options(None)).await.unwrap();
        assert_eq!(report.api.status, ApiCheckStatus::InvalidChallenge);
    }

    #[tokio::test]
    async fn anonymous_bearer_token_is_scoped_and_never_serialized() {
        let layer = b"tiny layer";
        let (manifest, _) = image_manifest(layer);
        let challenge = RegistryResponse::new(401).header(
            "www-authenticate",
            r#"Bearer realm="https://registry.example/token?account=anonymous",service="registry.example",scope="repository:team/app:pull""#,
        );
        let transport = MockTransport::new([
            MockStep::Response(challenge),
            MockStep::Response(RegistryResponse::new(200).body(br#"{"token":"secret-token"}"#)),
            manifest_response(manifest),
            MockStep::Response(
                RegistryResponse::new(206)
                    .header("content-range", "bytes 0-9/10")
                    .body(layer.to_vec()),
            ),
        ]);
        let report = diagnose_registry(
            &transport,
            options(Some(image("registry.example/team/app:latest"))),
        )
        .await
        .unwrap();
        assert_eq!(report.status, RegistryDiagnosticStatus::Healthy);
        assert_eq!(report.api.challenge_scope_matches, Some(true));
        assert_eq!(report.manifest.status, ManifestCheckStatus::Verified);
        assert_eq!(report.blob_range.status, BlobRangeStatus::Supported);

        let seen = transport.seen();
        assert_eq!(seen.len(), 4);
        assert_eq!(seen[0].path, "/v2/");
        assert_eq!(seen[1].origin, "https://registry.example");
        assert!(!seen[1].authorized);
        assert!(seen[1]
            .query
            .as_deref()
            .unwrap()
            .contains("scope=repository%3Ateam%2Fapp%3Apull"));
        assert!(seen[2].authorized);
        assert!(seen[3].authorized);
        let json = serde_json::to_string(&report).unwrap();
        assert!(!json.contains("secret-token"));
        assert!(!json.contains("account=anonymous"));
    }

    #[tokio::test]
    async fn image_manifest_and_full_small_blob_are_verified_exactly() {
        let layer = b"0123456789abcdef";
        let (manifest, layer_digest) = image_manifest(layer);
        let manifest_digest = sha256_digest(&manifest).unwrap();
        let reference = image(&format!("registry.example/team/app@{manifest_digest}"));
        let transport = MockTransport::new([
            response(200),
            manifest_response(manifest),
            MockStep::Response(
                RegistryResponse::new(206)
                    .header("content-range", "bytes 0-15/16")
                    .body(layer.to_vec()),
            ),
        ]);
        let report = diagnose_registry(&transport, options(Some(reference)))
            .await
            .unwrap();
        assert_eq!(report.status, RegistryDiagnosticStatus::Healthy);
        assert_eq!(report.manifest.digest, Some(manifest_digest));
        assert_eq!(report.blob_range.digest, Some(layer_digest));
        assert_eq!(report.blob_range.status, BlobRangeStatus::Supported);
        let seen = transport.seen();
        assert_eq!(seen[1].accept, Some(ACCEPT_MANIFESTS));
        assert_eq!(seen[2].range, Some((0, 15)));
        assert_eq!(seen[2].max_body_bytes, DEFAULT_BLOB_SAMPLE_BYTES);
    }

    #[tokio::test]
    async fn index_selects_exactly_one_platform_child() {
        let layer = b"arm64 layer";
        let (child, _) = image_manifest(layer);
        let child_digest = sha256_digest(&child).unwrap();
        let index = serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 2,
            "mediaType": OCI_INDEX,
            "manifests": [
                {
                    "mediaType": OCI_MANIFEST,
                    "digest": sha256_digest(b"unused").unwrap(),
                    "size": 6,
                    "platform": {"os": "linux", "architecture": "amd64"}
                },
                {
                    "mediaType": OCI_MANIFEST,
                    "digest": child_digest,
                    "size": child.len(),
                    "platform": {"os": "linux", "architecture": "arm64", "variant": "v8"}
                },
                {
                    "mediaType": OCI_MANIFEST,
                    "digest": sha256_digest(b"windows").unwrap(),
                    "size": 7,
                    "platform": {"os": "windows", "architecture": "amd64", "os.version": "10.0.20348.0"}
                }
            ]
        }))
        .unwrap();
        let transport = MockTransport::new([
            response(200),
            index_response(index),
            manifest_response(child),
            MockStep::Response(
                RegistryResponse::new(206)
                    .header("content-range", "bytes 0-10/11")
                    .body(layer.to_vec()),
            ),
        ]);
        let opts = options(Some(image("registry.example/team/app:v1")))
            .with_platform(OciPlatform::parse("linux/arm64/v8").unwrap());
        let report = diagnose_registry(&transport, opts).await.unwrap();
        assert_eq!(report.manifest.status, ManifestCheckStatus::Verified);
        assert_eq!(report.manifest.kind, Some(ManifestKind::Index));
        assert_eq!(report.manifest.child_digest, Some(child_digest.clone()));
        assert_eq!(transport.seen().len(), 4);
        assert!(transport.seen()[2].path.ends_with(child_digest.as_str()));
    }

    #[tokio::test]
    async fn duplicate_platform_matches_are_rejected_as_ambiguous() {
        let child = b"child";
        let digest = sha256_digest(child).unwrap();
        let descriptor = serde_json::json!({
            "mediaType": OCI_MANIFEST,
            "digest": digest,
            "size": child.len(),
            "platform": {"os": "linux", "architecture": "amd64"}
        });
        let index = serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 2,
            "mediaType": OCI_INDEX,
            "manifests": [descriptor.clone(), descriptor]
        }))
        .unwrap();
        let transport = MockTransport::new([response(200), index_response(index)]);
        let opts = options(Some(image("registry.example/team/app:v1")))
            .with_platform(OciPlatform::parse("linux/amd64").unwrap());
        let report = diagnose_registry(&transport, opts).await.unwrap();
        assert_eq!(report.status, RegistryDiagnosticStatus::Unsupported);
        assert_eq!(
            report.manifest.status,
            ManifestCheckStatus::PlatformNotFound
        );
        assert_eq!(transport.seen().len(), 2);
    }

    #[tokio::test]
    async fn windows_platform_accepts_and_reports_one_valid_os_version() {
        let layer = b"windows-layer";
        let (child, _) = image_manifest(layer);
        let digest = sha256_digest(&child).unwrap();
        let index = serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 2,
            "mediaType": OCI_INDEX,
            "manifests": [{
                "mediaType": OCI_MANIFEST,
                "digest": digest,
                "size": child.len(),
                "platform": {
                    "os": "windows",
                    "architecture": "amd64",
                    "os.version": "10.0.20348.0"
                }
            }]
        }))
        .unwrap();
        let transport = MockTransport::new([
            response(200),
            index_response(index),
            manifest_response(child),
            MockStep::Response(
                RegistryResponse::new(206)
                    .header("content-range", "bytes 0-12/13")
                    .body(layer.to_vec()),
            ),
        ]);
        let opts = options(Some(image("registry.example/team/app:v1")))
            .with_platform(OciPlatform::parse("windows/amd64").unwrap());
        let report = diagnose_registry(&transport, opts).await.unwrap();
        assert_eq!(report.status, RegistryDiagnosticStatus::Healthy);
        assert_eq!(report.manifest.status, ManifestCheckStatus::Verified);
        assert_eq!(
            report.manifest.selected_os_version.as_deref(),
            Some("10.0.20348.0")
        );
        assert_eq!(transport.seen().len(), 4);
    }

    #[tokio::test]
    async fn manifest_corruption_size_and_body_limits_are_typed() {
        let body = serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 2,
            "mediaType": OCI_MANIFEST,
            "layers": []
        }))
        .unwrap();
        let wrong = OciDigest::parse(&format!("sha256:{}", "0".repeat(64))).unwrap();
        let transport = MockTransport::new([
            response(200),
            MockStep::Response(
                RegistryResponse::new(200)
                    .header("content-type", OCI_MANIFEST)
                    .header("docker-content-digest", wrong.as_str())
                    .body(body),
            ),
        ]);
        let report = diagnose_registry(
            &transport,
            options(Some(image("registry.example/team/app:v1"))),
        )
        .await
        .unwrap();
        assert_eq!(report.status, RegistryDiagnosticStatus::Corrupt);
        assert_eq!(report.manifest.status, ManifestCheckStatus::DigestMismatch);

        let transport = MockTransport::new([
            response(200),
            MockStep::Error(RegistryTransportError::BodyTooLarge),
        ]);
        let report = diagnose_registry(
            &transport,
            options(Some(image("registry.example/team/app:v1"))),
        )
        .await
        .unwrap();
        assert_eq!(report.manifest.status, ManifestCheckStatus::BodyTooLarge);

        let invalid_body = serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 2,
            "mediaType": OCI_MANIFEST,
            "config": {
                "mediaType": "text/plain",
                "digest": sha256_digest(b"config").unwrap(),
                "size": 6
            },
            "layers": []
        }))
        .unwrap();
        let transport = MockTransport::new([response(200), manifest_response(invalid_body)]);
        let report = diagnose_registry(
            &transport,
            options(Some(image("registry.example/team/app:v1"))),
        )
        .await
        .unwrap();
        assert_eq!(report.manifest.status, ManifestCheckStatus::InvalidManifest);
        assert_ne!(report.manifest.status, ManifestCheckStatus::Verified);
    }

    #[tokio::test]
    async fn blob_range_outcomes_are_typed() {
        let layer = b"0123456789abcdef";
        for (range_response, expected) in [
            (
                RegistryResponse::new(200).body(layer.to_vec()),
                BlobRangeStatus::Ignored,
            ),
            (RegistryResponse::new(416), BlobRangeStatus::Unsatisfiable),
            (
                RegistryResponse::new(206)
                    .header("content-range", "bytes 1-15/16")
                    .body(layer.to_vec()),
                BlobRangeStatus::Malformed,
            ),
            (
                RegistryResponse::new(206)
                    .header("content-range", "bytes 0-15/16")
                    .body(b"xxxxxxxxxxxxxxxx".to_vec()),
                BlobRangeStatus::DigestMismatch,
            ),
        ] {
            let (manifest, _) = image_manifest(layer);
            let transport = MockTransport::new([
                response(200),
                manifest_response(manifest),
                MockStep::Response(range_response),
            ]);
            let report = diagnose_registry(
                &transport,
                options(Some(image("registry.example/team/app:v1"))),
            )
            .await
            .unwrap();
            assert_eq!(report.blob_range.status, expected);
        }
    }

    #[tokio::test]
    async fn ignored_range_keeps_a_bounded_sample_instead_of_failing_body_limit() {
        let layer = b"0123456789abcdef";
        let (manifest, _) = image_manifest(layer);
        let transport = MockTransport::new([
            response(200),
            manifest_response(manifest),
            MockStep::Response(
                RegistryResponse::new(200)
                    .header("content-length", "16")
                    .truncated_body(layer[..4].to_vec()),
            ),
        ]);
        let limits = RegistryLimits {
            blob_sample_bytes: 4,
            ..RegistryLimits::default()
        };
        let report = diagnose_registry(
            &transport,
            options(Some(image("registry.example/team/app:v1"))).with_limits(limits),
        )
        .await
        .unwrap();
        assert_eq!(report.blob_range.status, BlobRangeStatus::Ignored);
        assert_eq!(report.blob_range.received_bytes, 4);
    }

    #[tokio::test]
    async fn upstream_tag_is_resolved_once_and_mirrors_use_that_digest_in_order() {
        let (manifest, _) = image_manifest(b"layer");
        let digest = sha256_digest(&manifest).unwrap();
        let (mirror_two_body, _) = image_manifest(b"different-layer");
        let transport = MockTransport::new([
            response(200),
            manifest_response(manifest.clone()),
            MockStep::Response(
                RegistryResponse::new(206)
                    .header("content-range", "bytes 0-4/5")
                    .body(b"layer".to_vec()),
            ),
            response(200),
            manifest_response(manifest),
            response(200),
            manifest_response(mirror_two_body),
        ]);
        let opts = options(Some(image("registry.example/team/app:moving"))).with_mirrors(vec![
            endpoint("mirror-one.example"),
            endpoint("mirror-two.example"),
        ]);
        let report = diagnose_registry(&transport, opts).await.unwrap();
        assert_eq!(
            report
                .mirrors
                .iter()
                .map(|mirror| mirror.status)
                .collect::<Vec<_>>(),
            [MirrorCheckStatus::Equivalent, MirrorCheckStatus::Diverged]
        );
        assert_eq!(report.status, RegistryDiagnosticStatus::Corrupt);
        let manifest_paths = transport
            .seen()
            .into_iter()
            .filter(|request| request.path.contains("/manifests/"))
            .collect::<Vec<_>>();
        assert_eq!(manifest_paths[0].path, "/v2/team/app/manifests/moving");
        assert_eq!(manifest_paths[1].origin, "https://mirror-one.example");
        assert_eq!(manifest_paths[2].origin, "https://mirror-two.example");
        assert!(manifest_paths[1].path.ends_with(digest.as_str()));
        assert!(manifest_paths[2].path.ends_with(digest.as_str()));
    }

    #[tokio::test]
    async fn upstream_token_never_crosses_to_mirror_origin() {
        let (manifest, _) = image_manifest(b"layer");
        let upstream_challenge = RegistryResponse::new(401).header(
            "www-authenticate",
            r#"Bearer realm="https://registry.example/token",service="registry.example""#,
        );
        let mirror_challenge = RegistryResponse::new(401).header(
            "www-authenticate",
            r#"Bearer realm="https://mirror.example/token",service="mirror.example""#,
        );
        let transport = MockTransport::new([
            MockStep::Response(upstream_challenge),
            MockStep::Response(RegistryResponse::new(200).body(br#"{"token":"upstream-secret"}"#)),
            manifest_response(manifest.clone()),
            MockStep::Response(
                RegistryResponse::new(206)
                    .header("content-range", "bytes 0-4/5")
                    .body(b"layer".to_vec()),
            ),
            MockStep::Response(mirror_challenge),
            MockStep::Response(RegistryResponse::new(200).body(br#"{"token":"mirror-secret"}"#)),
            manifest_response(manifest),
        ]);
        let opts = options(Some(image("registry.example/team/app:v1")))
            .with_mirrors(vec![endpoint("mirror.example")]);
        let report = diagnose_registry(&transport, opts).await.unwrap();
        assert_eq!(report.mirrors[0].status, MirrorCheckStatus::Equivalent);
        let seen = transport.seen();
        let mirror_probe = seen
            .iter()
            .find(|request| request.origin == "https://mirror.example" && request.path == "/v2/")
            .unwrap();
        assert!(!mirror_probe.authorized);
        let mirror_manifest = seen
            .iter()
            .find(|request| {
                request.origin == "https://mirror.example" && request.path.contains("/manifests/")
            })
            .unwrap();
        assert!(mirror_manifest.authorized);
    }

    #[tokio::test]
    async fn cross_origin_redirect_and_bearer_realm_fail_closed() {
        let transport = MockTransport::new([MockStep::Response(
            RegistryResponse::new(307).header("location", "https://cdn.example/v2/"),
        )]);
        let report = diagnose_registry(&transport, options(None)).await.unwrap();
        assert_eq!(report.api.status, ApiCheckStatus::RedirectRejected);
        assert_eq!(transport.seen().len(), 1);

        let challenge = RegistryResponse::new(401).header(
            "www-authenticate",
            r#"Bearer realm="https://auth.example/token",service="registry.example""#,
        );
        let transport = MockTransport::new([MockStep::Response(challenge)]);
        let report = diagnose_registry(
            &transport,
            options(Some(image("registry.example/team/app:v1"))),
        )
        .await
        .unwrap();
        assert_eq!(report.api.status, ApiCheckStatus::InvalidChallenge);
        assert_eq!(transport.seen().len(), 1);
    }

    #[tokio::test]
    async fn redirects_and_request_budgets_fail_closed() {
        let transport = MockTransport::new([
            MockStep::Response(RegistryResponse::new(307).header("location", "/v2/ready")),
            response(200),
        ]);
        let report = diagnose_registry(&transport, options(None)).await.unwrap();
        assert_eq!(report.api.status, ApiCheckStatus::Available);
        assert_eq!(transport.seen().len(), 2);

        let transport = MockTransport::new([MockStep::Response(
            RegistryResponse::new(307).header("location", "http://other.example/v2/"),
        )]);
        let report = diagnose_registry(&transport, options(None)).await.unwrap();
        assert_eq!(report.api.status, ApiCheckStatus::RedirectRejected);
        assert_eq!(transport.seen().len(), 1);

        let limits = RegistryLimits {
            max_requests: 1,
            max_redirects: 1,
            ..RegistryLimits::default()
        };
        let transport = MockTransport::new([response(200)]);
        let report = diagnose_registry(
            &transport,
            options(Some(image("registry.example/team/app:v1"))).with_limits(limits),
        )
        .await
        .unwrap();
        assert_eq!(report.status, RegistryDiagnosticStatus::LimitExceeded);
        assert_eq!(report.manifest.status, ManifestCheckStatus::RequestLimit);
        assert_eq!(report.request_count, 1);
    }

    #[test]
    fn invalid_limits_and_registry_mismatch_are_rejected_without_io() {
        let invalid = RegistryLimits {
            max_requests: 0,
            ..RegistryLimits::default()
        };
        assert_eq!(
            invalid.validate(),
            Err(RegistryProtocolError::InvalidLimits)
        );

        let transport = MockTransport::new([]);
        let error = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(diagnose_registry(
                &transport,
                options(Some(image("other.example/team/app:v1"))),
            ))
            .unwrap_err();
        assert_eq!(error, RegistryProtocolError::RegistryMismatch);
        assert!(transport.seen().is_empty());

        let mirrors = (0..=HARD_MAX_MIRRORS)
            .map(|index| endpoint(&format!("mirror-{index}.example")))
            .collect();
        let transport = MockTransport::new([]);
        let error = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(diagnose_registry(
                &transport,
                options(None).with_mirrors(mirrors),
            ))
            .unwrap_err();
        assert_eq!(error, RegistryProtocolError::TooManyMirrors);
        assert!(transport.seen().is_empty());
    }

    #[tokio::test]
    async fn skipped_mirrors_are_prepopulated_in_configured_order() {
        let transport = MockTransport::new([response(403)]);
        let report = diagnose_registry(
            &transport,
            options(Some(image("registry.example/team/app:v1")))
                .with_mirrors(vec![endpoint("first.example"), endpoint("second.example")]),
        )
        .await
        .unwrap();
        assert_eq!(
            report
                .mirrors
                .iter()
                .map(|mirror| (mirror.order, mirror.origin.as_str(), mirror.status))
                .collect::<Vec<_>>(),
            [
                (0, "https://first.example", MirrorCheckStatus::NotTested),
                (1, "https://second.example", MirrorCheckStatus::NotTested),
            ]
        );
    }

    #[test]
    fn debug_and_json_never_echo_secrets_or_response_bodies() {
        let token = BearerToken {
            value: "secret-token".to_owned(),
            bound_origin: "https://registry.example".to_owned(),
        };
        assert!(!format!("{token:?}").contains("secret-token"));
        let response = RegistryResponse::new(401)
            .header("www-authenticate", "Bearer secret-token")
            .body(b"secret-body".to_vec());
        let debug = format!("{response:?}");
        assert!(!debug.contains("secret-token"));
        assert!(!debug.contains("secret-body"));
    }

    #[test]
    fn report_json_has_a_stable_typed_shape() {
        let report = RegistryDiagnosticReport {
            schema_version: REGISTRY_DIAGNOSTIC_SCHEMA_VERSION,
            status: RegistryDiagnosticStatus::Healthy,
            upstream: RegistryName::parse("registry.example").unwrap(),
            upstream_origin: "https://registry.example".to_owned(),
            image: None,
            requested_platform: None,
            api: ApiCheck {
                status: ApiCheckStatus::Available,
                http_status: Some(200),
                ..ApiCheck::default()
            },
            manifest: ManifestCheck::default(),
            blob_range: BlobRangeCheck::default(),
            mirrors: Vec::new(),
            request_count: 1,
        };
        assert_eq!(
            serde_json::to_string(&report).unwrap(),
            r#"{"schema_version":1,"status":"healthy","upstream":"registry.example","upstream_origin":"https://registry.example","image":null,"requested_platform":null,"api":{"status":"available","http_status":200,"bearer_challenge":false,"challenge_service_present":false,"challenge_scope_matches":null},"manifest":{"status":"not-requested","kind":null,"media_type":null,"digest":null,"child_digest":null,"selected_platform":null,"selected_os_version":null,"byte_size":null},"blob_range":{"status":"not-tested","digest":null,"requested_bytes":0,"received_bytes":0,"total_bytes":null},"mirrors":[],"request_count":1}"#
        );
    }
}
