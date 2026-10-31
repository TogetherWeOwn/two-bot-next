//! Template-assistant HTTPS transport gate (TA-01,
//! `docs/command-wiring-security.md`).
//!
//! The pure-core gate in [`crate::voice_assistant`] decides whether the
//! assistant path is configured; it is not a secure transport. This module is
//! the transport boundary the TA-01 row requires: HTTPS to an
//! environment-fixed destination, with the *resolved* destination validated on
//! every request — including DNS changes — and redirects never followed.
//!
//! Threat model: the endpoint comes from the environment
//! (`TWO_ASSISTANT_ENDPOINT`, [`crate::voice_assistant::AssistantConfig`]),
//! never from interaction input or dashboard settings (both keys are
//! [`crate::settings::SettingClass::EnvOnly`] and the published command offers
//! only the `request` option). SSRF is closed in three places: construction
//! refuses userinfo (including an empty username), empty authorities,
//! plaintext, unapproved ports, `localhost` and IP literals
//! outside globally-routable unicast space; [`AssistantTransport::post`]
//! re-resolves the configured host on every request and refuses empty,
//! failed or non-public answers as a whole before any egress call; any 3xx
//! answer is [`AssistantTransportError::RedirectRefused`] with no follow-up
//! request and no resolution of the redirect target. Any caller-supplied
//! destination goes through [`AssistantTransport::post_to`], which always
//! refuses, so overrides have nowhere to be plumbed. Error text is static:
//! endpoints and bodies never echo into errors, logs or replies, and redacted
//! `Debug` keeps them out of formatted diagnostics too.
//!
//! The address predicate is shared with the feed-fetch seam
//! ([`crate::feeds_http::is_public_address`]) so the two outbound paths
//! cannot drift into different notions of "public". Tests inject a scripted
//! resolver and egress; there is no production HTTPS client in this slice
//! (no handler, endpoint call or credential binding), and nothing here opens
//! a socket, resolves a name or reads the environment.

use std::future::Future;
use std::net::IpAddr;

use thiserror::Error;
use url::{Host, Url};

use crate::feeds_http::is_public_address;
use crate::voice_assistant::AssistantConfig;

/// Transport failures. Messages are static: they name the refusal class, never
/// the endpoint, a resolved address, or the request/response body.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum AssistantTransportError {
    /// The configured endpoint failed the construction-time gate.
    #[error("assistant transport is not configured with a usable endpoint")]
    InvalidConfig,
    /// A destination outside the configured endpoint was requested.
    #[error("assistant destination is not approved")]
    DestinationRefused,
    /// A resolver answer contained an address outside globally-routable
    /// unicast space (or mixed one in with public addresses).
    #[error("assistant resolved address is not globally routable")]
    ResolvedAddressRefused,
    /// The egress answer was a redirect. Never followed, never re-resolved.
    #[error("assistant transport refused a redirect")]
    RedirectRefused {
        /// The 3xx status that was refused.
        status: u16,
    },
    /// A caller-supplied destination was handed to [`AssistantTransport::post_to`].
    #[error("caller-supplied assistant destinations are refused")]
    DestinationOverrideRefused,
    /// Resolution failed or answered empty. Fail closed, no request.
    #[error("assistant host resolution failed")]
    ResolutionFailed,
}

/// Validated, environment-fixed assistant destination. `None` from
/// [`AssistantTransportConfig::from_assistant_config`] means the path is
/// disabled before any resolution or request is attempted.
///
/// `Debug` is redacted: the endpoint URL can carry credential-bearing path or
/// query data, so only the host and port render.
#[derive(Clone, PartialEq, Eq)]
pub struct AssistantTransportConfig {
    endpoint: String,
    host: String,
    port: u16,
}

impl std::fmt::Debug for AssistantTransportConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AssistantTransportConfig")
            .field("endpoint", &"[REDACTED]")
            .field("host", &self.host)
            .field("port", &self.port)
            .finish()
    }
}

impl AssistantTransportConfig {
    /// Validate an [`AssistantConfig`] as a transport destination. Returns
    /// `None` — disabled — unless the endpoint is `https://` with a non-empty
    /// authority, carries no userinfo, uses no port other than 443, has a
    /// non-empty host, and the host is neither `localhost` nor an IP literal
    /// outside globally-routable unicast space.
    #[must_use]
    pub fn from_assistant_config(config: &AssistantConfig) -> Option<Self> {
        let endpoint = config.endpoint();
        let url = Url::parse(endpoint).ok()?;
        if url.scheme() != "https" {
            return None;
        }
        // WHATWG parsing recovers a host from an empty authority
        // (`https:///v1` parses with host `v1`) and tolerates an empty
        // username before `@`, so check the raw authority first: it must be
        // non-empty and carry no userinfo. The `8` skips past the `https://`
        // prefix; a missing prefix fails closed here rather than panicking.
        let authority = endpoint
            .get(8..)
            .and_then(|rest| rest.split(['/', '?', '#']).next())
            .unwrap_or("");
        if authority.is_empty() || authority.contains('@') {
            return None;
        }
        if !url.username().is_empty() || url.password().is_some() {
            return None;
        }
        if url.port().is_some_and(|port| port != 443) {
            return None;
        }
        match url.host()? {
            Host::Domain(domain) => {
                if domain.is_empty()
                    || domain.chars().any(|c| c.is_whitespace() || c.is_control())
                    || domain
                        .trim_end_matches('.')
                        .eq_ignore_ascii_case("localhost")
                {
                    return None;
                }
                Some(Self {
                    endpoint: endpoint.to_owned(),
                    host: domain.to_owned(),
                    port: 443,
                })
            }
            Host::Ipv4(literal) => {
                if !is_public_address(IpAddr::V4(literal)) {
                    return None;
                }
                Some(Self {
                    endpoint: endpoint.to_owned(),
                    host: literal.to_string(),
                    port: 443,
                })
            }
            Host::Ipv6(literal) => {
                if !is_public_address(IpAddr::V6(literal)) {
                    return None;
                }
                Some(Self {
                    endpoint: endpoint.to_owned(),
                    host: literal.to_string(),
                    port: 443,
                })
            }
        }
    }

    /// The configured endpoint URL, verbatim.
    #[must_use]
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// The configured host the resolver is asked about on every request.
    #[must_use]
    pub fn host(&self) -> &str {
        &self.host
    }

    /// The approved port. Always 443: any other port refuses at construction.
    #[must_use]
    pub fn port(&self) -> u16 {
        self.port
    }
}

/// DNS seam: production resolves the configured host over the platform
/// resolver; tests inject scripted answers. Called fresh on *every* request —
/// answers are never cached across requests, so a DNS flip revalidates.
pub trait AssistantResolver {
    /// Resolve `host` for `port`, returning every address the answer carries.
    fn resolve(
        &self,
        host: &str,
        port: u16,
    ) -> impl Future<Output = Result<Vec<IpAddr>, AssistantTransportError>> + Send;
}

/// Transport seam: the actual HTTPS boundary. Production connects only to the
/// handed [`AssistantRequest::resolved_addresses`] — no re-resolution at
/// connect time; tests inject scripted answers. Zero real egress in tests.
pub trait AssistantEgress {
    /// Issue one request and return the buffered answer.
    fn send(
        &self,
        request: AssistantRequest,
    ) -> impl Future<Output = Result<AssistantResponse, AssistantTransportError>> + Send;
}

/// One assistant HTTPS exchange. The `resolved_addresses` are the validated
/// addresses the real transport must connect to.
///
/// `Debug` is redacted: the URL and the body never render, only the host,
/// port, validated addresses and body length.
#[derive(Clone)]
pub struct AssistantRequest {
    /// The configured endpoint URL, verbatim.
    pub url: String,
    /// The configured host.
    pub host: String,
    /// The approved port (always 443).
    pub port: u16,
    /// The validated resolver answer: non-empty, every address
    /// globally-routable unicast.
    pub resolved_addresses: Vec<IpAddr>,
    /// Request body, passed through unchanged.
    pub body: Vec<u8>,
}

impl std::fmt::Debug for AssistantRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AssistantRequest")
            .field("url", &"[REDACTED]")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("resolved_addresses", &self.resolved_addresses)
            .field("body_bytes", &self.body.len())
            .finish()
    }
}

/// A buffered assistant HTTPS answer.
///
/// `Debug` renders the status, whether a redirect target was recorded, and
/// the body length; the location value and the body itself never render.
#[derive(Clone, PartialEq, Eq)]
pub struct AssistantResponse {
    /// Answer status. Any 3xx becomes
    /// [`AssistantTransportError::RedirectRefused`]; all other statuses pass
    /// through to the caller.
    pub status: u16,
    /// Redirect target, if the egress reported one. Recorded, never followed.
    pub location: Option<String>,
    /// Full body.
    pub body: Vec<u8>,
}

impl std::fmt::Debug for AssistantResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AssistantResponse")
            .field("status", &self.status)
            .field("location", &self.location.as_ref().map(|_| "[REDACTED]"))
            .field("body_bytes", &self.body.len())
            .finish()
    }
}

/// The gated transport: a validated config plus an injected resolver and
/// egress. `R` resolves, `E` sends; neither is consulted before the config
/// gate passes.
#[derive(Debug)]
pub struct AssistantTransport<R, E> {
    config: AssistantTransportConfig,
    resolver: R,
    egress: E,
}

impl<R, E> AssistantTransport<R, E> {
    /// Build a transport over an already-validated config. No I/O happens here.
    #[must_use]
    pub fn new(config: AssistantTransportConfig, resolver: R, egress: E) -> Self {
        Self {
            config,
            resolver,
            egress,
        }
    }

    /// The validated destination this transport is pinned to.
    #[must_use]
    pub fn config(&self) -> &AssistantTransportConfig {
        &self.config
    }
}

impl<R: AssistantResolver, E: AssistantEgress> AssistantTransport<R, E> {
    /// Build a transport from an [`AssistantConfig`]. Returns `None` —
    /// disabled — under the same gate as
    /// [`AssistantTransportConfig::from_assistant_config`], before any
    /// resolution or request.
    #[must_use]
    pub fn from_assistant_config(config: &AssistantConfig, resolver: R, egress: E) -> Option<Self> {
        AssistantTransportConfig::from_assistant_config(config)
            .map(|validated| Self::new(validated, resolver, egress))
    }

    /// POST `body` to the configured endpoint: resolve fresh, require a
    /// non-empty answer where *every* address is globally-routable unicast,
    /// make exactly one egress call carrying the validated addresses, and
    /// refuse any 3xx answer with no follow-up request and no re-resolution
    /// of the redirect target.
    pub async fn post(&self, body: &[u8]) -> Result<AssistantResponse, AssistantTransportError> {
        let resolved = self
            .resolver
            .resolve(&self.config.host, self.config.port)
            .await?;
        if resolved.is_empty() {
            return Err(AssistantTransportError::ResolutionFailed);
        }
        if resolved.iter().any(|ip| !is_public_address(*ip)) {
            return Err(AssistantTransportError::ResolvedAddressRefused);
        }
        let response = self
            .egress
            .send(AssistantRequest {
                url: self.config.endpoint.clone(),
                host: self.config.host.clone(),
                port: self.config.port,
                resolved_addresses: resolved,
                body: body.to_vec(),
            })
            .await?;
        if (300..400).contains(&response.status) {
            return Err(AssistantTransportError::RedirectRefused {
                status: response.status,
            });
        }
        Ok(response)
    }

    /// The single auditable door for any caller-supplied destination. It always
    /// refuses with [`AssistantTransportError::DestinationOverrideRefused`] —
    /// even when handed the configured endpoint — and performs no resolution
    /// and no request, so interaction input and dashboard settings have nowhere
    /// to be plumbed.
    pub async fn post_to(
        &self,
        _destination: &str,
        _body: &[u8],
    ) -> Result<AssistantResponse, AssistantTransportError> {
        Err(AssistantTransportError::DestinationOverrideRefused)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, VecDeque};
    use std::sync::{Arc, Mutex};

    fn assistant_config(endpoint: &str) -> Option<AssistantConfig> {
        AssistantConfig::from_map(&HashMap::from([
            ("TWO_ASSISTANT_ENDPOINT".to_string(), endpoint.to_string()),
            (
                "TWO_ASSISTANT_MODEL".to_string(),
                "fixture-model".to_string(),
            ),
            (
                "TWO_ASSISTANT_API_KEY".to_string(),
                "fixture-assistant-credential".to_string(),
            ),
        ]))
    }

    fn public_v4() -> IpAddr {
        "93.184.216.34".parse().expect("fixture IPv4 parses")
    }

    fn public_v6() -> IpAddr {
        "2606:2800:220:1:248:1893:25c8:1946"
            .parse()
            .expect("fixture IPv6 parses")
    }

    #[derive(Clone, Default)]
    struct ScriptResolver {
        inner: Arc<ResolverInner>,
    }

    #[derive(Default)]
    struct ResolverInner {
        answers: Mutex<VecDeque<Result<Vec<IpAddr>, AssistantTransportError>>>,
        calls: Mutex<Vec<(String, u16)>>,
    }

    impl ScriptResolver {
        fn new(answers: Vec<Result<Vec<IpAddr>, AssistantTransportError>>) -> Self {
            Self {
                inner: Arc::new(ResolverInner {
                    answers: Mutex::new(answers.into()),
                    calls: Mutex::new(Vec::new()),
                }),
            }
        }

        fn calls(&self) -> Vec<(String, u16)> {
            self.inner
                .calls
                .lock()
                .expect("resolver calls lock")
                .clone()
        }
    }

    impl AssistantResolver for ScriptResolver {
        async fn resolve(
            &self,
            host: &str,
            port: u16,
        ) -> Result<Vec<IpAddr>, AssistantTransportError> {
            self.inner
                .calls
                .lock()
                .expect("resolver calls lock")
                .push((host.to_string(), port));
            self.inner
                .answers
                .lock()
                .expect("resolver answers lock")
                .pop_front()
                .expect("test scripted enough resolver answers")
        }
    }

    #[derive(Clone, Default)]
    struct ScriptEgress {
        inner: Arc<EgressInner>,
    }

    #[derive(Default)]
    struct EgressInner {
        responses: Mutex<VecDeque<AssistantResponse>>,
        requests: Mutex<Vec<AssistantRequest>>,
    }

    impl ScriptEgress {
        fn new(responses: Vec<AssistantResponse>) -> Self {
            Self {
                inner: Arc::new(EgressInner {
                    responses: Mutex::new(responses.into()),
                    requests: Mutex::new(Vec::new()),
                }),
            }
        }

        fn requests(&self) -> Vec<AssistantRequest> {
            self.inner
                .requests
                .lock()
                .expect("egress requests lock")
                .clone()
        }
    }

    impl AssistantEgress for ScriptEgress {
        async fn send(
            &self,
            request: AssistantRequest,
        ) -> Result<AssistantResponse, AssistantTransportError> {
            self.inner
                .requests
                .lock()
                .expect("egress requests lock")
                .push(request);
            Ok(self
                .inner
                .responses
                .lock()
                .expect("egress responses lock")
                .pop_front()
                .expect("test scripted enough egress responses"))
        }
    }

    fn ok_response() -> AssistantResponse {
        AssistantResponse {
            status: 200,
            location: None,
            body: b"{}".to_vec(),
        }
    }

    type FixtureTransport = AssistantTransport<ScriptResolver, ScriptEgress>;

    fn transport_for(
        endpoint: &str,
        resolver: ScriptResolver,
        egress: ScriptEgress,
    ) -> Option<FixtureTransport> {
        let config = assistant_config(endpoint)?;
        FixtureTransport::from_assistant_config(&config, resolver, egress)
    }

    #[test]
    fn approved_endpoint_enables_with_host_and_port() {
        for endpoint in [
            "https://assistant.example.com/v1",
            "https://assistant.example.com:443/v1",
        ] {
            let config = assistant_config(endpoint).expect("assistant gate must pass fixture");
            let validated = AssistantTransportConfig::from_assistant_config(&config)
                .expect("transport gate must pass fixture");
            assert_eq!(validated.endpoint(), endpoint);
            assert_eq!(validated.host(), "assistant.example.com");
            assert_eq!(validated.port(), 443);
        }
    }

    #[test]
    fn invalid_endpoints_disable_before_any_use() {
        let mut invalid: Vec<String> = vec![
            "http://assistant.example.com/v1",
            "https://user@assistant.example.com/v1",
            "https://assistant.example.com@evil.example.com/v1",
            "https://@evil.example.com/v1",
            "https://assistant.example.com:8443/v1",
            "https://localhost/v1",
            "https://localhost:443/v1",
            "https://",
            "https:///v1",
            "assistant.example.com/v1",
            "https://10.1.2.3/v1",
            "https://127.0.0.1/v1",
            "https://[::1]/v1",
            "https://[fe80::1]/v1",
            "https://[::ffff:10.0.0.7]/v1",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        for literal in [
            "169.254.169.254",
            "192.168.0.9",
            "224.0.0.1",
            "255.255.255.255",
            "2001:db8::1",
        ] {
            invalid.push(format!("https://{literal}/v1"));
        }
        for endpoint in &invalid {
            let resolver = ScriptResolver::new(vec![Ok(vec![public_v4()])]);
            let egress = ScriptEgress::new(vec![ok_response()]);
            assert!(
                transport_for(endpoint, resolver.clone(), egress.clone()).is_none(),
                "invalid endpoint must disable the path: {endpoint}"
            );
            assert!(resolver.calls().is_empty(), "must not resolve: {endpoint}");
            assert!(
                egress.requests().is_empty(),
                "must make no request: {endpoint}"
            );
            if let Some(config) = assistant_config(endpoint) {
                assert!(
                    AssistantTransportConfig::from_assistant_config(&config).is_none(),
                    "transport config must refuse: {endpoint}"
                );
            }
        }
    }

    #[tokio::test]
    async fn post_resolves_every_request_and_pins_validated_addresses() {
        let resolver = ScriptResolver::new(vec![
            Ok(vec![public_v4()]),
            Ok(vec![public_v4(), public_v6()]),
        ]);
        let egress = ScriptEgress::new(vec![ok_response(), ok_response()]);
        let transport = transport_for(
            "https://assistant.example.com/v1",
            resolver.clone(),
            egress.clone(),
        )
        .expect("fixture endpoint must enable the transport");

        transport
            .post(b"first")
            .await
            .expect("first post must work");
        transport
            .post(b"second")
            .await
            .expect("second post must work");
        assert_eq!(
            resolver.calls(),
            vec![
                ("assistant.example.com".to_string(), 443),
                ("assistant.example.com".to_string(), 443),
            ],
            "every request re-resolves; answers are never cached"
        );
        let requests = egress.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].resolved_addresses, vec![public_v4()]);
        assert_eq!(
            requests[1].resolved_addresses,
            vec![public_v4(), public_v6()]
        );
        assert_eq!(requests[1].url, "https://assistant.example.com/v1");
        assert_eq!(requests[1].host, "assistant.example.com");
        assert_eq!(requests[1].port, 443);
        assert_eq!(requests[1].body, b"second");
    }

    #[tokio::test]
    async fn refused_resolved_addresses_make_no_request() {
        for answer in [
            Ok(vec!["10.0.0.7".parse().expect("fixture parses")]),
            Ok(vec!["::ffff:127.0.0.1".parse().expect("fixture parses")]),
            Ok(vec![
                public_v4(),
                "192.168.1.20".parse().expect("fixture parses"),
            ]),
            Ok(Vec::new()),
            Err(AssistantTransportError::ResolutionFailed),
        ] {
            let resolver = ScriptResolver::new(vec![answer]);
            let egress = ScriptEgress::new(vec![ok_response()]);
            let transport =
                transport_for("https://assistant.example.com/v1", resolver, egress.clone())
                    .expect("fixture endpoint must enable the transport");
            let result = transport.post(b"probe").await;
            assert!(
                matches!(
                    result,
                    Err(AssistantTransportError::ResolvedAddressRefused)
                        | Err(AssistantTransportError::ResolutionFailed)
                ),
                "refused resolution must fail closed: {result:?}"
            );
            assert!(egress.requests().is_empty(), "no request on refusal");
        }
    }

    #[tokio::test]
    async fn redirect_statuses_are_refused_without_follow_up() {
        for status in [300_u16, 301, 302, 303, 307, 308] {
            let resolver = ScriptResolver::new(vec![Ok(vec![public_v4()])]);
            let egress = ScriptEgress::new(vec![
                AssistantResponse {
                    status,
                    location: Some("https://evil.example.com/steal".to_string()),
                    body: Vec::new(),
                },
                ok_response(),
            ]);
            let transport = transport_for(
                "https://assistant.example.com/v1",
                resolver.clone(),
                egress.clone(),
            )
            .expect("fixture endpoint must enable the transport");
            assert_eq!(
                transport.post(b"probe").await,
                Err(AssistantTransportError::RedirectRefused { status }),
                "redirect {status} must be refused, not followed"
            );
            assert_eq!(egress.requests().len(), 1, "no follow-up request");
            assert_eq!(resolver.calls().len(), 1, "redirect target never resolved");
        }
    }

    #[tokio::test]
    async fn non_redirect_statuses_pass_through() {
        for status in [200_u16, 201, 400, 404, 500] {
            let resolver = ScriptResolver::new(vec![Ok(vec![public_v4()])]);
            let egress = ScriptEgress::new(vec![AssistantResponse {
                status,
                location: Some("https://evil.example.com/steal".to_string()),
                body: b"payload".to_vec(),
            }]);
            let transport = transport_for("https://assistant.example.com/v1", resolver, egress)
                .expect("fixture endpoint must enable the transport");
            let response = transport
                .post(b"probe")
                .await
                .expect("non-redirect statuses pass through");
            assert_eq!(response.status, status);
            assert_eq!(response.body, b"payload");
        }
    }

    #[tokio::test]
    async fn post_to_always_refuses_without_resolution_or_request() {
        let resolver = ScriptResolver::new(vec![Ok(vec![public_v4()])]);
        let egress = ScriptEgress::new(vec![ok_response()]);
        let transport = transport_for(
            "https://assistant.example.com/v1",
            resolver.clone(),
            egress.clone(),
        )
        .expect("fixture endpoint must enable the transport");
        for destination in [
            "https://evil.example.com/v1",
            "https://assistant.example.com/v1",
        ] {
            assert_eq!(
                transport.post_to(destination, b"probe").await,
                Err(AssistantTransportError::DestinationOverrideRefused),
                "caller-supplied destination must be refused: {destination}"
            );
        }
        assert!(resolver.calls().is_empty(), "override must not resolve");
        assert!(egress.requests().is_empty(), "override must not send");
    }

    #[test]
    fn errors_never_echo_endpoint_or_body() {
        let endpoint = "https://assistant.example.com/hunter2-path";
        let body = b"hunter2-body";
        for error in [
            AssistantTransportError::InvalidConfig,
            AssistantTransportError::DestinationRefused,
            AssistantTransportError::ResolvedAddressRefused,
            AssistantTransportError::RedirectRefused { status: 302 },
            AssistantTransportError::DestinationOverrideRefused,
            AssistantTransportError::ResolutionFailed,
        ] {
            let rendered = format!("{error} {error:?}");
            assert!(!rendered.contains("hunter2"), "leaked into {rendered:?}");
            assert!(!rendered.contains(endpoint), "leaked into {rendered:?}");
            assert!(
                !rendered.contains(str::from_utf8(body).expect("fixture body is UTF-8")),
                "leaked into {rendered:?}"
            );
        }
    }

    #[test]
    fn debug_never_exposes_endpoint_or_bodies() {
        // Config refuses query credentials; keep an admitted path sentinel
        // and exercise query redaction on the raw request/response types too.
        let endpoint = "https://assistant.example.com/hunter2-path";
        let validated = AssistantTransportConfig::from_assistant_config(
            &assistant_config(endpoint).expect("fixture endpoint must parse"),
        )
        .expect("fixture endpoint must enable the transport");
        let request = AssistantRequest {
            url: format!("{endpoint}?token=hunter2"),
            host: validated.host().to_owned(),
            port: validated.port(),
            resolved_addresses: vec![public_v4()],
            body: b"hunter2-body".to_vec(),
        };
        let response = AssistantResponse {
            status: 200,
            location: Some("https://assistant.example.com/next?token=hunter2".to_string()),
            body: b"hunter2-response".to_vec(),
        };
        for rendered in [
            format!("{validated:?}"),
            format!("{request:?}"),
            format!("{response:?}"),
            format!("{validated:#?}"),
            format!("{request:#?}"),
            format!("{response:#?}"),
        ] {
            assert!(!rendered.contains("hunter2"), "leaked into {rendered:?}");
            assert!(!rendered.contains(endpoint), "leaked into {rendered:?}");
        }
    }
}
