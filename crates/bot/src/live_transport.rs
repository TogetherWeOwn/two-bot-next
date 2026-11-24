//! Dormant HTTP transport for already-numbered Discord realtime v1 deltas.

use http_body_util::Full;
use hyper::{body::Bytes, header::HeaderValue, Request, StatusCode, Uri};
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
use hyper_util::{
    client::legacy::{connect::HttpConnector, Client},
    rt::TokioExecutor,
};
use time::OffsetDateTime;
use two_bot_core::{
    realtime_signing::{sign_ingest, INGEST_PATH},
    Secret,
};

use crate::live_delivery::Delivery;

type HttpClient = Client<HttpsConnector<HttpConnector>, Full<Bytes>>;

/// One fixed Web ingress destination and its dedicated Bot-to-Web signing key.
#[derive(Debug)]
pub struct LiveTransport {
    client: HttpClient,
    endpoint: Secret<Uri>,
    key_id: HeaderValue,
    key: Secret<String>,
}

impl LiveTransport {
    /// Build for a trusted, server-configured canonical HTTPS origin, with an
    /// optional trailing slash. Use the Web ingress key, separate from the
    /// snapshot and internal-action credentials. Construction does no I/O.
    pub fn new(origin: &str, key_id: &str, key: Secret<String>) -> Option<Self> {
        let url = url::Url::parse(origin).ok()?;
        let canonical = url.origin().ascii_serialization();
        if url.scheme() != "https" || origin.strip_suffix('/').unwrap_or(origin) != canonical {
            return None;
        }
        if key.expose().is_empty()
            || !(1..=128).contains(&key_id.len())
            || !key_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"_.:-".contains(&byte))
        {
            return None;
        }
        let endpoint = format!("{canonical}{INGEST_PATH}").parse().ok()?;
        let mut key_id = HeaderValue::from_str(key_id).ok()?;
        key_id.set_sensitive(true);
        let connector = HttpsConnectorBuilder::new()
            .with_webpki_roots()
            .https_only()
            .enable_http1()
            .build();
        Some(Self {
            client: build_client(connector),
            endpoint: Secret::new(endpoint),
            key_id,
            key,
        })
    }

    /// Send one attempt with a fresh signature over the supplied encoded delta.
    /// Run under the existing delivery worker, which owns deadlines and retries.
    pub async fn attempt(&self, encoded_delta: &[u8]) -> Delivery {
        let timestamp = OffsetDateTime::now_utc().unix_timestamp();
        let Some(request) = self.request(encoded_delta, timestamp, rand::random()) else {
            return Delivery::Rejected;
        };
        let Ok(response) = self.client.request(request).await else {
            return Delivery::Retry;
        };
        // The ingress contract is status-only. In particular, 409 means the
        // hub needs recovery and repeating the same delta cannot supply it.
        match response.status() {
            StatusCode::NO_CONTENT => Delivery::Accepted,
            StatusCode::TOO_MANY_REQUESTS => Delivery::Retry,
            status if status.is_server_error() => Delivery::Retry,
            _ => Delivery::Rejected,
        }
    }

    fn request(
        &self,
        encoded_delta: &[u8],
        timestamp: i64,
        nonce: [u8; 16],
    ) -> Option<Request<Full<Bytes>>> {
        let signed = sign_ingest(&self.key, timestamp, nonce, encoded_delta)?;
        let mut mac = HeaderValue::from_str(signed.signature.expose()).ok()?;
        mac.set_sensitive(true);
        Request::post(self.endpoint.expose().clone())
            .header("content-type", "application/json")
            .header("x-two-key-id", self.key_id.clone())
            .header("x-two-timestamp", signed.timestamp)
            .header("x-two-nonce", signed.nonce)
            .header("x-two-signature", mac)
            .body(Full::new(Bytes::copy_from_slice(encoded_delta)))
            .ok()
    }
}

fn build_client(connector: HttpsConnector<HttpConnector>) -> HttpClient {
    // Hyper does not follow redirects. Disable its pooled-connection retry so
    // that every wire attempt is owned and freshly signed by the worker.
    Client::builder(TokioExecutor::new())
        .retry_canceled_requests(false)
        .build(connector)
}

#[cfg(test)]
mod tests;
