#![allow(dead_code)]

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use rsa::RsaPrivateKey;
use rsa::pkcs1::EncodeRsaPrivateKey;
use rsa::traits::PublicKeyParts;
use serde_json::{Value, json};
use tokio::task::JoinHandle;

pub struct TestKeys {
    encoding: EncodingKey,
    n: String,
    e: String,
}

impl TestKeys {
    pub const KID: &'static str = "test-1";

    pub fn get() -> &'static TestKeys {
        static KEYS: OnceLock<TestKeys> = OnceLock::new();
        KEYS.get_or_init(|| {
            let key = RsaPrivateKey::new(&mut rand::thread_rng(), 2048).expect("rsa keygen");
            let pem = key.to_pkcs1_pem(rsa::pkcs8::LineEnding::LF).expect("pem");
            let public = key.to_public_key();
            TestKeys {
                encoding: EncodingKey::from_rsa_pem(pem.as_bytes()).expect("encoding key"),
                n: URL_SAFE_NO_PAD.encode(public.n().to_bytes_be()),
                e: URL_SAFE_NO_PAD.encode(public.e().to_bytes_be()),
            }
        })
    }

    /// JWKS containing the test public key under `kid = "test-1"`.
    pub fn jwk_set_json(&self) -> Value {
        self.jwk_set_json_with_kid(Self::KID)
    }

    pub fn jwk_set_json_with_kid(&self, kid: &str) -> Value {
        json!({"keys": [{
            "kty": "RSA", "alg": "RS256", "use": "sig",
            "kid": kid, "n": self.n, "e": self.e,
        }]})
    }

    pub fn mint(&self, claims: Value) -> String {
        self.mint_with_kid(Self::KID, claims)
    }

    pub fn mint_with_kid(&self, kid: &str, claims: Value) -> String {
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some(kid.to_string());
        jsonwebtoken::encode(&header, &claims, &self.encoding).expect("mint")
    }
}

pub struct JwksServer {
    pub base_url: String,
    pub handle: JoinHandle<()>,
    fetches: Arc<AtomicUsize>,
    body: Arc<Mutex<Value>>,
    delay_ms: Arc<AtomicU64>,
}

impl JwksServer {
    /// Makes every later response wait this long before answering.
    pub fn set_delay_ms(&self, ms: u64) {
        self.delay_ms.store(ms, Ordering::SeqCst);
    }

    pub fn fetch_count(&self) -> usize {
        self.fetches.load(Ordering::SeqCst)
    }

    pub fn set_body(&self, json: Value) {
        *self.body.lock().unwrap() = json;
    }
}

/// Serves `json` at `/jwks`, counting fetches. Returns the server handle.
pub async fn serve_jwks(json: Value) -> JwksServer {
    use axum::{Json, Router, extract::State, routing::get};

    #[derive(Clone)]
    struct S {
        fetches: Arc<AtomicUsize>,
        body: Arc<Mutex<Value>>,
        delay_ms: Arc<AtomicU64>,
    }
    let delay_ms = Arc::new(AtomicU64::new(0));
    let fetches = Arc::new(AtomicUsize::new(0));
    let body = Arc::new(Mutex::new(json));
    let state = S {
        fetches: fetches.clone(),
        body: body.clone(),
        delay_ms: delay_ms.clone(),
    };
    let app = Router::new()
        .route(
            "/jwks",
            get(|State(s): State<S>| async move {
                s.fetches.fetch_add(1, Ordering::SeqCst);
                let d = s.delay_ms.load(Ordering::SeqCst);
                if d > 0 {
                    tokio::time::sleep(std::time::Duration::from_millis(d)).await;
                }
                Json(s.body.lock().unwrap().clone())
            }),
        )
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    JwksServer {
        base_url: format!("http://{addr}/jwks"),
        handle,
        fetches,
        body,
        delay_ms,
    }
}

/// Fixture-only actual profiled authority builder; never production seeding.
pub mod profiled;

pub mod native_started;

pub mod source_exports;

pub mod selection;

pub mod native_preparation;

pub mod native_cancellation;
pub mod native_offer;

pub mod closure;

/// Shared source-only presence fixture; independent of phase protocol profile.
pub mod presence;
