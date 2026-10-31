//! Loopback-only REST double for the containment worker journeys.
//! Keep only the surface this suite uses; gateway and fault-injection
//! helpers belong to their own fixtures, not a dead-code suppression.

use std::sync::{Arc, Mutex};

use axum::body::to_bytes;
use axum::extract::Request;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Router;
use tokio::net::TcpListener;

#[derive(Clone)]
pub struct RestRequest {
    pub method: String,
    pub path: String,
    pub body: Vec<u8>,
}

pub struct ScriptedResponse(Response);

impl ScriptedResponse {
    pub fn status(status: u16) -> Self {
        Self(
            StatusCode::from_u16(status)
                .expect("valid mock status")
                .into_response(),
        )
    }

    pub fn json(status: u16, body: serde_json::Value) -> Self {
        Self(
            (
                StatusCode::from_u16(status).expect("valid mock status"),
                axum::Json(body),
            )
                .into_response(),
        )
    }
}

pub struct MockRest {
    origin: String,
    recorded: Arc<Mutex<Vec<RestRequest>>>,
    handle: tokio::task::JoinHandle<()>,
}

impl MockRest {
    pub async fn with_responder(
        responder: impl Fn(&RestRequest) -> ScriptedResponse + Send + Sync + 'static,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind rest");
        let origin = format!("http://{}", listener.local_addr().expect("rest addr"));
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let requests = Arc::clone(&recorded);
        let responder = Arc::new(responder);
        let app = Router::new().fallback(move |request: Request| {
            let requests = Arc::clone(&requests);
            let responder = Arc::clone(&responder);
            async move {
                let (parts, body) = request.into_parts();
                let request = RestRequest {
                    method: parts.method.to_string(),
                    path: parts.uri.path().to_owned(),
                    body: to_bytes(body, 1024 * 1024)
                        .await
                        .expect("bounded mock request body")
                        .to_vec(),
                };
                requests.lock().expect("recorded").push(request.clone());
                let response = responder(&request);
                response.0
            }
        });
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve mock rest");
        });
        Self {
            origin,
            recorded,
            handle,
        }
    }

    pub fn origin(&self) -> String {
        self.origin.clone()
    }

    pub fn requests(&self) -> Vec<RestRequest> {
        self.recorded.lock().expect("recorded").clone()
    }

    pub async fn shutdown(mut self) {
        self.handle.abort();
        let _ = (&mut self.handle).await;
    }
}

impl Drop for MockRest {
    fn drop(&mut self) {
        self.handle.abort();
    }
}
