//! Acceptance tests: fail-closed assistant transport gate (TA-01).
//!
//! Spec: `docs/command-wiring-security.md` row TA-01 — HTTPS to an
//! explicitly approved, environment-fixed host/origin; interaction input
//! and dashboard settings cannot choose a destination; userinfo, plaintext,
//! unapproved ports/hosts and private, loopback, link-local or reserved
//! destinations are rejected; the *resolved* destination is validated,
//! including DNS changes; redirects are never followed. Minimum evidence:
//! invalid config disables the path before any request,
//! public-host-to-private-address resolution and every redirect class make
//! no follow-up request, and the actual transport is tested — not only a
//! URL string predicate.
//!
//! These tests are written blind to the implementation, against the
//! wished-for public contract below. They are RED until the Builder lands
//! it on this branch **without editing this file**:
//!
//! - New module `two_bot_core::assistant_transport` (exported from
//!   `crates/core/src/lib.rs`), mirroring the `twitch_helix` seam: a
//!   validated config, an injected resolver, an injected egress (the actual
//!   transport boundary; production supplies the real HTTPS client, tests
//!   inject scripted fakes — zero real egress, no sockets, no DNS).
//! - `AssistantTransportConfig::from_assistant_config(&AssistantConfig)
//!   -> Option<Self>`: `None` (path disabled) unless the endpoint is
//!   `https://`, has a non-empty host, carries no userinfo, uses no port
//!   other than 443, and the host is neither `localhost` nor an IP literal
//!   outside globally-routable unicast space.
//! - `AssistantResolver`: `fn resolve(&self, host: &str, port: u16)
//!   -> impl Future<Output = Result<Vec<IpAddr>, AssistantTransportError>>
//!   + Send`. Called fresh on *every* request (DNS changes revalidate).
//! - `AssistantEgress`: `fn send(&self, request: AssistantRequest)
//!   -> impl Future<Output = Result<AssistantResponse,
//!   AssistantTransportError>> + Send`. `AssistantRequest` has public
//!   fields `url: String`, `host: String`, `port: u16`,
//!   `resolved_addresses: Vec<IpAddr>` (the validated addresses the real
//!   transport must connect to — no re-resolution at connect time) and
//!   `body: Vec<u8>` (`#[derive(Debug, Clone)]`, like `HelixRequest`).
//!   `AssistantResponse` has public fields `status: u16`,
//!   `location: Option<String>`, `body: Vec<u8>`
//!   (`#[derive(Debug, Clone, PartialEq, Eq)]`, like `HelixResponse`).
//! - `AssistantTransport<R, E>::from_assistant_config(&AssistantConfig,
//!   resolver, egress) -> Option<Self>` (same gate as the config: `None`
//!   disables before any resolution or request), plus
//!   `AssistantTransport::new(AssistantTransportConfig, resolver, egress)`.
//! - `async fn post(&self, body: &[u8]) -> Result<AssistantResponse,
//!   AssistantTransportError>`: resolve → require a non-empty answer where
//!   *every* address is globally-routable unicast → exactly one egress
//!   call → any 3xx status is `RedirectRefused` with no follow-up request
//!   and no re-resolution of the redirect target.
//! - `async fn post_to(&self, destination: &str, body: &[u8])
//!   -> Result<AssistantResponse, AssistantTransportError>`: the single
//!   auditable door for any caller-supplied destination. It always
//!   refuses with `DestinationOverrideRefused` — even when handed the
//!   configured endpoint — and performs no resolution and no request, so
//!   interaction/dashboard overrides have nowhere else to be plumbed.
//! - `AssistantTransportError` is `Debug + Clone + PartialEq + Eq` with at
//!   least the variants `InvalidConfig`, `DestinationRefused`,
//!   `ResolvedAddressRefused`, `RedirectRefused { status: u16 }`,
//!   `DestinationOverrideRefused` and `ResolutionFailed` (empty or failed
//!   resolution). Error text must never echo the endpoint or body (TA-04
//!   territory; not asserted here).
//!
//! Fixture boundaries, stated honestly: every host is an `example.com`
//! fixture and every "public" address is `93.184.216.34` /
//! `2606:2800:220:1:248:1893:25c8:1946` (example.com's published
//! addresses) fed through the scripted resolver. Nothing in this file
//! opens a socket, resolves a name, or reads the environment. What this
//! suite proves is the gate logic at the transport boundary; the
//! production egress's TLS/connection pinning to `resolved_addresses` is
//! the Builder's unit-test burden, stated in the contract above.

use std::collections::{HashMap, VecDeque};
use std::net::IpAddr;
use std::sync::{Arc, Mutex};

use two_bot_core::assistant_transport::{
    AssistantEgress, AssistantRequest, AssistantResolver, AssistantResponse, AssistantTransport,
    AssistantTransportConfig, AssistantTransportError,
};
use two_bot_core::settings::{assert_storable_key, classify_key, SettingClass};
use two_bot_core::voice_assistant::{assistant_commands, AssistantConfig};

/// The one environment-fixed, approved destination used by the positive
/// control and as the base for hostile mutations.
const APPROVED_ENDPOINT: &str = "https://assistant.example.com/v1";
const APPROVED_HOST: &str = "assistant.example.com";

fn public_v4() -> IpAddr {
    "93.184.216.34".parse().expect("fixture public IPv4 parses")
}

fn public_v6() -> IpAddr {
    "2606:2800:220:1:248:1893:25c8:1946"
        .parse()
        .expect("fixture public IPv6 parses")
}

fn ip(text: &str) -> IpAddr {
    text.parse()
        .unwrap_or_else(|_| panic!("fixture IP parses: {text}"))
}

/// Every non-globally-routable-unicast class the gate must refuse, whether
/// it arrives as a URL host literal or as a resolver answer for a public
/// hostname. One representative per class.
const REFUSED_ADDRESSES: &[&str] = &[
    "10.1.2.3",         // private 10/8
    "172.16.0.9",       // private 172.16/12
    "192.168.0.9",      // private 192.168/16
    "127.0.0.1",        // loopback 127/8
    "127.53.0.1",       // loopback, non-.1 tail
    "169.254.169.254",  // link-local 169.254/16 (cloud metadata shape)
    "100.64.0.1",       // shared/CGNAT 100.64/10
    "0.0.0.0",          // unspecified
    "192.0.2.1",        // documentation TEST-NET-1
    "198.51.100.1",     // documentation TEST-NET-2
    "203.0.113.1",      // documentation TEST-NET-3
    "198.18.0.1",       // benchmarking 198.18/15
    "240.0.0.1",        // reserved 240/4
    "224.0.0.1",        // multicast
    "255.255.255.255",  // broadcast
    "::1",              // IPv6 loopback
    "::",               // IPv6 unspecified
    "fe80::1",          // IPv6 link-local
    "fd12:3456::1",     // IPv6 unique-local fc00::/7
    "ff02::1",          // IPv6 multicast
    "2001:db8::1",      // IPv6 documentation
    "::ffff:127.0.0.1", // IPv4-mapped loopback must not smuggle past
    "::ffff:10.0.0.7",  // IPv4-mapped private must not smuggle past
];

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

/// Scripted DNS: hands out queued answers in order and records every
/// query, so a test can flip a public host to a private address between
/// requests and count revalidations.
#[derive(Clone, Default)]
struct ScriptedResolver {
    inner: Arc<ResolverInner>,
}

#[derive(Default)]
struct ResolverInner {
    answers: Mutex<VecDeque<Result<Vec<IpAddr>, AssistantTransportError>>>,
    calls: Mutex<Vec<(String, u16)>>,
}

impl ScriptedResolver {
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

impl AssistantResolver for ScriptedResolver {
    async fn resolve(&self, host: &str, port: u16) -> Result<Vec<IpAddr>, AssistantTransportError> {
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

/// Scripted transport boundary: returns queued responses and records every
/// request it was asked to make, so "no follow-up request" is a count, not
/// an impression.
#[derive(Clone, Default)]
struct ScriptedEgress {
    inner: Arc<EgressInner>,
}

#[derive(Default)]
struct EgressInner {
    responses: Mutex<VecDeque<AssistantResponse>>,
    requests: Mutex<Vec<AssistantRequest>>,
}

impl ScriptedEgress {
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

impl AssistantEgress for ScriptedEgress {
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

type FixtureTransport = AssistantTransport<ScriptedResolver, ScriptedEgress>;

/// Build the gated transport for `endpoint`, chaining the existing config
/// gate and the new transport gate. Either layer returning `None` means
/// the path is disabled; the acceptance bar is that *something* refuses
/// before resolution or egress, never that a specific layer does.
fn transport_for(
    endpoint: &str,
    resolver: ScriptedResolver,
    egress: ScriptedEgress,
) -> Option<FixtureTransport> {
    let config = assistant_config(endpoint)?;
    AssistantTransport::from_assistant_config(&config, resolver, egress)
}

fn approved_transport(resolver: ScriptedResolver, egress: ScriptedEgress) -> FixtureTransport {
    transport_for(APPROVED_ENDPOINT, resolver, egress)
        .expect("approved fixture endpoint must enable the transport")
}

#[test]
fn invalid_assistant_transport_config_disables_path_before_any_request() {
    let mut invalid_endpoints: Vec<String> = vec![
        // Plaintext and non-HTTPS schemes.
        "http://assistant.example.com/v1".to_string(),
        "ftp://assistant.example.com/v1".to_string(),
        // Userinfo can disguise the real host; both shapes refuse.
        "https://user@assistant.example.com/v1".to_string(),
        "https://assistant.example.com@evil.example.com/v1".to_string(),
        // Unapproved ports (443 is the only approved port).
        "https://assistant.example.com:8443/v1".to_string(),
        "https://assistant.example.com:80/v1".to_string(),
        "https://assistant.example.com:22/v1".to_string(),
        // Hostname aliases for this machine.
        "https://localhost/v1".to_string(),
        "https://localhost:443/v1".to_string(),
        // No host at all.
        "https://".to_string(),
        "https:///v1".to_string(),
        // Bare authority, no scheme.
        "assistant.example.com/v1".to_string(),
    ];
    // Every refused address class as a URL host literal.
    for address in REFUSED_ADDRESSES {
        if address.contains(':') {
            invalid_endpoints.push(format!("https://[{address}]/v1"));
        } else {
            invalid_endpoints.push(format!("https://{address}/v1"));
        }
    }

    for endpoint in &invalid_endpoints {
        let resolver = ScriptedResolver::new(vec![Ok(vec![public_v4()])]);
        let egress = ScriptedEgress::new(vec![ok_response()]);
        let transport = transport_for(endpoint, resolver.clone(), egress.clone());
        assert!(
            transport.is_none(),
            "invalid endpoint must disable the path: {endpoint}"
        );
        assert!(
            resolver.calls().is_empty(),
            "invalid endpoint must not be resolved: {endpoint}"
        );
        assert!(
            egress.requests().is_empty(),
            "invalid endpoint must make no request: {endpoint}"
        );

        // The config gate alone must agree whenever the lower config gate
        // let the value through: refusing at transport construction is
        // the contract, not an accident of layering.
        if let Some(config) = assistant_config(endpoint) {
            assert!(
                AssistantTransportConfig::from_assistant_config(&config).is_none(),
                "transport config must refuse invalid endpoint: {endpoint}"
            );
        }
    }
}

#[tokio::test]
async fn approved_destination_with_public_resolution_reaches_egress_once_with_pinned_addresses() {
    // Positive control: proves the harness can pass, so the refusal tests
    // below are not vacuous. Explicit :443 is the same approved origin.
    for endpoint in [APPROVED_ENDPOINT, "https://assistant.example.com:443/v1"] {
        let resolver = ScriptedResolver::new(vec![Ok(vec![public_v4(), public_v6()])]);
        let egress = ScriptedEgress::new(vec![ok_response()]);
        let transport = transport_for(endpoint, resolver.clone(), egress.clone())
            .expect("approved endpoint must enable the transport");

        let response = transport
            .post(b"{\"fixture\":true}")
            .await
            .expect("approved destination with public resolution must succeed");
        assert_eq!(response.status, 200);

        assert_eq!(
            resolver.calls(),
            vec![(APPROVED_HOST.to_string(), 443)],
            "exactly one resolution of the configured host on the approved port"
        );
        let requests = egress.requests();
        assert_eq!(requests.len(), 1, "exactly one egress request");
        let request = &requests[0];
        assert_eq!(
            request.url, endpoint,
            "request goes to the configured endpoint only"
        );
        assert_eq!(request.host, APPROVED_HOST);
        assert_eq!(request.port, 443);
        assert_eq!(
            request.resolved_addresses,
            vec![public_v4(), public_v6()],
            "egress must be handed the validated addresses to pin the connection"
        );
        assert_eq!(
            request.body, b"{\"fixture\":true}",
            "body passes through unchanged"
        );
    }
}

#[tokio::test]
async fn public_host_to_private_address_resolution_makes_no_follow_up_request() {
    // Synthetic DNS flip: the approved public host resolves publicly for
    // the first request, then flips to a private address. The second
    // request must be refused after revalidation, with no egress call.
    let resolver = ScriptedResolver::new(vec![Ok(vec![public_v4()]), Ok(vec![ip("10.0.0.7")])]);
    let egress = ScriptedEgress::new(vec![ok_response(), ok_response()]);
    let transport = approved_transport(resolver.clone(), egress.clone());

    transport
        .post(b"first")
        .await
        .expect("public resolution must succeed before the flip");
    let flipped = transport.post(b"second").await;
    assert_eq!(
        flipped,
        Err(AssistantTransportError::ResolvedAddressRefused),
        "a DNS flip to a private address must refuse the request"
    );
    assert_eq!(
        resolver.calls().len(),
        2,
        "resolution must be revalidated on every request, never cached across the flip"
    );
    assert_eq!(
        egress.requests().len(),
        1,
        "the flipped request must make no follow-up egress call"
    );

    // Every refused class, delivered as the resolver answer for the
    // approved public host, refuses before any request.
    for address in REFUSED_ADDRESSES {
        let resolver = ScriptedResolver::new(vec![Ok(vec![ip(address)])]);
        let egress = ScriptedEgress::new(vec![ok_response()]);
        let transport = approved_transport(resolver.clone(), egress.clone());
        assert_eq!(
            transport.post(b"probe").await,
            Err(AssistantTransportError::ResolvedAddressRefused),
            "resolved address must be refused: {address}"
        );
        assert!(
            egress.requests().is_empty(),
            "refused resolved address must make no request: {address}"
        );
    }

    // A mixed answer (one public, one private) refuses as a whole: the
    // transport must not quietly keep only the public address, because
    // connection racing could still pick the private one.
    let resolver = ScriptedResolver::new(vec![Ok(vec![public_v4(), ip("192.168.1.20")])]);
    let egress = ScriptedEgress::new(vec![ok_response()]);
    let transport = approved_transport(resolver, egress.clone());
    assert_eq!(
        transport.post(b"mixed").await,
        Err(AssistantTransportError::ResolvedAddressRefused),
        "a mixed public+private answer must refuse as a whole"
    );
    assert!(
        egress.requests().is_empty(),
        "mixed answer must make no request"
    );
}

#[tokio::test]
async fn resolution_failure_and_empty_resolution_make_no_request() {
    for answer in [
        Err(AssistantTransportError::ResolutionFailed),
        Ok(Vec::new()),
    ] {
        let resolver = ScriptedResolver::new(vec![answer]);
        let egress = ScriptedEgress::new(vec![ok_response()]);
        let transport = approved_transport(resolver, egress.clone());
        assert_eq!(
            transport.post(b"probe").await,
            Err(AssistantTransportError::ResolutionFailed),
            "failed or empty resolution must fail closed"
        );
        assert!(
            egress.requests().is_empty(),
            "unresolved destination must make no request"
        );
    }
}

#[tokio::test]
async fn every_redirect_class_makes_no_follow_up_request() {
    // Every redirect class, to an attacker host, to a relative path, and
    // with no Location at all: never followed, never re-resolved.
    for status in [300_u16, 301, 302, 303, 307, 308] {
        for location in [
            Some("https://evil.example.com/steal".to_string()),
            Some("https://assistant.example.com/other".to_string()),
            Some("/relative-elsewhere".to_string()),
            None,
        ] {
            let resolver = ScriptedResolver::new(vec![Ok(vec![public_v4()])]);
            let egress = ScriptedEgress::new(vec![
                AssistantResponse {
                    status,
                    location: location.clone(),
                    body: Vec::new(),
                },
                ok_response(), // a follow-up would consume this and be counted
            ]);
            let transport = approved_transport(resolver.clone(), egress.clone());

            assert_eq!(
                transport.post(b"probe").await,
                Err(AssistantTransportError::RedirectRefused { status }),
                "redirect {status} (location {location:?}) must be refused, not followed"
            );
            assert_eq!(
                egress.requests().len(),
                1,
                "redirect {status} must make no follow-up request"
            );
            assert_eq!(
                resolver.calls().len(),
                1,
                "redirect {status} target must never be resolved"
            );
        }
    }
}

#[tokio::test]
async fn dashboard_and_interaction_destination_override_is_refused() {
    // Transport door: any caller-supplied destination refuses — an
    // attacker host, a look-alike, and even the configured endpoint
    // itself (no equality shortcut that later callers could widen).
    let resolver = ScriptedResolver::new(vec![Ok(vec![public_v4()])]);
    let egress = ScriptedEgress::new(vec![ok_response()]);
    let transport = approved_transport(resolver.clone(), egress.clone());
    for destination in [
        "https://evil.example.com/v1",
        "https://assistant.example.com.evil.example.com/v1",
        "http://assistant.example.com/v1",
        APPROVED_ENDPOINT,
    ] {
        assert_eq!(
            transport.post_to(destination, b"probe").await,
            Err(AssistantTransportError::DestinationOverrideRefused),
            "caller-supplied destination must be refused: {destination}"
        );
    }
    assert!(
        resolver.calls().is_empty(),
        "override attempts must not resolve anything"
    );
    assert!(
        egress.requests().is_empty(),
        "override attempts must make no request"
    );

    // Dashboard door: the endpoint and model keys are environment-only in
    // the settings catalog, so a dashboard write cannot choose or change
    // the destination.
    for key in ["TWO_ASSISTANT_ENDPOINT", "TWO_ASSISTANT_MODEL"] {
        assert_eq!(
            classify_key(key),
            Some(SettingClass::EnvOnly),
            "{key} must stay environment-only"
        );
        assert!(
            assert_storable_key(key).is_err(),
            "dashboard write of {key} must be refused"
        );
    }

    // Interaction door: the published command exposes exactly one option
    // (the request text). No option can carry a destination, URL, host,
    // endpoint or model override.
    let defs = assistant_commands();
    assert_eq!(defs.len(), 1, "exactly one assistant command is published");
    let option_names: Vec<&str> = defs[0].options.iter().map(|o| o.name.as_str()).collect();
    assert_eq!(
        option_names,
        vec!["request"],
        "the interaction must offer no destination-bearing option"
    );
}
