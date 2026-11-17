//! Test-only transport doubles; these do not model durable admission fencing.
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::AsyncReadExt,
    net::{TcpListener, TcpStream},
    time::timeout,
};
use two_bot_core::send_admission::{
    AdmissionError, AdmissionFuture, AdmissionPermit, SendAdmission, SendCompletion, SendCooldown,
    TokenKey,
};

#[derive(Debug)]
pub(crate) struct Admission {
    key: TokenKey,
    pub ordinary: Option<AdmissionError>,
    pub strict: Option<AdmissionError>,
    pub completion: Result<(), AdmissionError>,
    pub completed: Arc<Mutex<Vec<Option<SendCooldown>>>>,
}

impl Admission {
    pub fn new(
        token: &str,
        ordinary: Option<AdmissionError>,
        strict: Option<AdmissionError>,
    ) -> Self {
        Self {
            key: TokenKey::for_bot_token(token).unwrap(),
            ordinary,
            strict,
            completion: Ok(()),
            completed: Arc::default(),
        }
    }

    fn permit(&self, refusal: Option<AdmissionError>) -> Result<AdmissionPermit, AdmissionError> {
        if let Some(error) = refusal {
            return Err(error);
        }
        Ok(AdmissionPermit::new(Box::new(Completion {
            result: self.completion,
            completed: self.completed.clone(),
        })))
    }
}

impl SendAdmission for Admission {
    fn token_key(&self) -> &TokenKey {
        &self.key
    }
    fn admit(&self) -> AdmissionFuture<'_, Result<AdmissionPermit, AdmissionError>> {
        Box::pin(async { self.permit(self.ordinary) })
    }
    fn admit_strict(&self) -> AdmissionFuture<'_, Result<AdmissionPermit, AdmissionError>> {
        Box::pin(async { self.permit(self.strict) })
    }
}

struct Completion {
    result: Result<(), AdmissionError>,
    completed: Arc<Mutex<Vec<Option<SendCooldown>>>>,
}
impl SendCompletion for Completion {
    fn complete(
        self: Box<Self>,
        cooldown: Option<SendCooldown>,
    ) -> AdmissionFuture<'static, Result<(), AdmissionError>> {
        Box::pin(async move {
            self.completed.lock().unwrap().push(cooldown);
            self.result
        })
    }
}

pub(crate) async fn loopback() -> (TcpListener, String) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    (listener, origin)
}

/// Read a single bounded request, including its Content-Length body.
pub(crate) async fn read_request(stream: &mut TcpStream) -> Vec<u8> {
    timeout(Duration::from_secs(2), async {
        let mut request = Vec::new();
        let mut buf = [0; 1024];
        loop {
            let n = stream.read(&mut buf).await.unwrap();
            assert_ne!(n, 0, "request ended early");
            request.extend_from_slice(&buf[..n]);
            assert!(request.len() <= 16 * 1024, "request too large");
            if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                let headers = std::str::from_utf8(&request[..end]).unwrap();
                let length = headers
                    .lines()
                    .filter_map(|line| line.split_once(':'))
                    .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                    .map_or(0, |(_, value)| value.trim().parse::<usize>().unwrap());
                if request.len() >= end + 4 + length {
                    return request;
                }
            }
        }
    })
    .await
    .expect("request timed out")
}
