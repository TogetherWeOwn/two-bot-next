use std::{future::Future, io, net::Ipv4Addr, pin::Pin};

use super::{Bytes, Client, Full, HttpsConnector, HttpsConnectorBuilder, TokioExecutor};
use http::Uri;
use hyper_util::rt::TokioIo;
use rustls::{pki_types::CertificateDer, ClientConfig, RootCertStore};
use tokio::net::TcpStream;
use tower::{service_fn, util::ServiceFn};
use two_bot_core::send_admission::AdmissionError;

type RecorderDial = Pin<Box<dyn Future<Output = io::Result<TokioIo<TcpStream>>> + Send>>;
type RecorderTcp = ServiceFn<fn(Uri) -> RecorderDial>;

/// Isolated transport with one fixture root and standard rustls verification.
#[derive(Clone)]
pub struct RehearsalAnnouncementConnector(
    pub(super) Client<HttpsConnector<RecorderTcp>, Full<Bytes>>,
);

impl RehearsalAnnouncementConnector {
    pub fn new(ca_der: Vec<u8>) -> Result<Self, AdmissionError> {
        let mut roots = RootCertStore::empty();
        roots
            .add(CertificateDer::from(ca_der))
            .map_err(|_| AdmissionError::Configuration)?;
        let tls = ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let https = HttpsConnectorBuilder::new()
            .with_tls_config(tls)
            .https_only()
            .enable_http1()
            .wrap_connector(service_fn(dial_recorder as fn(Uri) -> RecorderDial));
        Ok(Self(
            Client::builder(TokioExecutor::new())
                .retry_canceled_requests(false)
                .build(https),
        ))
    }

    #[must_use]
    pub fn is_synthetic_token(token: &str) -> bool {
        token.strip_prefix("synthetic-").is_some_and(|value| {
            value.len() == 48
                && value
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        })
    }
}

fn dial_recorder(uri: Uri) -> RecorderDial {
    Box::pin(async move {
        if uri.scheme_str() != Some("https")
            || uri.authority().map(http::uri::Authority::as_str) != Some("discord.com")
        {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        TcpStream::connect((Ipv4Addr::LOCALHOST, 443))
            .await
            .map(TokioIo::new)
    })
}

#[cfg(test)]
#[tokio::test]
async fn socket_connector_refuses_any_other_authority() {
    for uri in [
        "http://discord.com",
        "https://discord.com:443",
        "https://127.0.0.1",
    ] {
        let result = dial_recorder(uri.parse().unwrap())
            .await
            .map_err(|e| e.kind());
        assert!(matches!(result, Err(io::ErrorKind::InvalidInput)));
    }
}
