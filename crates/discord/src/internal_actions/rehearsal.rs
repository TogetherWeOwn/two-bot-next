use std::{
    future::Future,
    io,
    net::Ipv4Addr,
    pin::Pin,
    task::{Context, Poll},
};

use bytes::Bytes;
use http::{Request, Response, Uri};
use http_body_util::Full;
use hyper::body::Incoming;
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
use hyper_util::{
    client::legacy::{Client, Error},
    rt::{TokioExecutor, TokioIo},
};
use rustls::{pki_types::CertificateDer, ClientConfig, RootCertStore};
use tokio::net::TcpStream;
use tower_service::Service;
use two_bot_core::send_admission::AdmissionError;

/// Transport for the isolated release-binary rehearsal. The single fixture
/// root is private to this client. Rustls verifies the chain and the URI host.
#[derive(Clone)]
pub struct RehearsalAnnouncementConnector(Client<HttpsConnector<RecorderTcp>, Full<Bytes>>);

impl RehearsalAnnouncementConnector {
    /// Build with one DER-encoded fixture trust root and no default roots.
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
            .wrap_connector(RecorderTcp);
        Ok(Self(
            Client::builder(TokioExecutor::new())
                .retry_canceled_requests(false)
                .build(https),
        ))
    }

    /// Match the release rehearsal's generated raw Discord credential.
    #[must_use]
    pub fn is_synthetic_token(token: &str) -> bool {
        token.strip_prefix("synthetic-").is_some_and(|value| {
            value.len() == 48
                && value
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        })
    }

    pub(super) async fn request(
        &self,
        request: Request<Full<Bytes>>,
    ) -> Result<Response<Incoming>, Error> {
        self.0.request(request).await
    }
}

#[derive(Clone, Copy)]
struct RecorderTcp;

impl Service<Uri> for RecorderTcp {
    type Response = TokioIo<TcpStream>;
    type Error = io::Error;
    type Future = Pin<Box<dyn Future<Output = io::Result<Self::Response>> + Send>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, uri: Uri) -> Self::Future {
        Box::pin(async move {
            if uri.scheme_str() != Some("https")
                || uri.authority().map(http::uri::Authority::as_str) != Some("discord.com")
            {
                return Err(io::Error::other("announcement rehearsal origin refused"));
            }
            TcpStream::connect((Ipv4Addr::LOCALHOST, 443))
                .await
                .map(TokioIo::new)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_generated_token_shape_is_synthetic() {
        let token = format!("synthetic-{}", "0123456789abcdef".repeat(3));
        assert!(RehearsalAnnouncementConnector::is_synthetic_token(&token));
        for value in [
            "ordinary.fixture.token".to_owned(),
            token.to_ascii_uppercase(),
            format!("Bot {token}"),
            format!("{token}0"),
            token.replace('a', "g"),
        ] {
            assert!(!RehearsalAnnouncementConnector::is_synthetic_token(&value));
        }
        assert!(RehearsalAnnouncementConnector::new(Vec::new()).is_err());
    }

    #[tokio::test]
    async fn socket_connector_refuses_any_other_authority() {
        for uri in [
            "http://discord.com",
            "https://discord.com:443",
            "https://127.0.0.1",
            "https://discord.com.example",
        ] {
            let error = RecorderTcp.call(uri.parse().unwrap()).await.err().unwrap();
            assert_eq!(error.to_string(), "announcement rehearsal origin refused");
        }
    }
}
