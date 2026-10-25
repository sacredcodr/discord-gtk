//! Minimal Discord REST client.
//!
//! All requests go through `Rest::request`, which is the only place that
//! knows about rate limits. Callers use `?` on the returned `Result`.

use std::time::Duration;

use reqwest::Method;
use serde::{de::DeserializeOwned, Deserialize};
use serde_json::Value;

use crate::error::Error;

const API: &str = "https://discord.com/api/v10";

#[derive(Clone)]
pub struct Rest {
    http: reqwest::Client,
    /// Sent verbatim as the `Authorization` header. Bot tokens must include
    /// the "Bot " prefix, user tokens must not.
    token: String,
}

impl Rest {
    /// Total tries for one logical request, including the first.
    const MAX_ATTEMPTS: u32 = 3;
    /// Upper bound on a single rate-limit wait. Discord can ask for much longer
    /// (global limits); beyond this the UI is better off showing an error.
    const MAX_WAIT_SECS: f64 = 30.0;

    pub fn new(token: &str) -> Self {
        let http = reqwest::Client::builder()
            .user_agent("discord-gtk/0.1")
            .build()
            .expect("reqwest client with default TLS configuration");
        Self { http, token: token.to_owned() }
    }

    async fn request(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<reqwest::Response, Error> {
        #[derive(Deserialize)]
        struct RateLimit {
            retry_after: f64,
        }

        for _ in 0..Self::MAX_ATTEMPTS {
            let mut request = self
                .http
                .request(method.clone(), format!("{API}{path}"))
                .header("Authorization", &self.token);
            if let Some(body) = body {
                request = request.json(body);
            }

            let response = request.send().await.map_err(|e| Error::Network(e.to_string()))?;
            let status = response.status();
            if status.as_u16() == 429 {
                // A body we cannot parse still means "slow down": wait one second.
                let wait = response.json::<RateLimit>().await.map_or(1.0, |r| r.retry_after);
                tokio::time::sleep(Duration::from_secs_f64(wait.min(Self::MAX_WAIT_SECS))).await;
                continue;
            }
            if !status.is_success() {
                return Err(Error::Http { status: status.as_u16(), path: path.to_owned() });
            }
            return Ok(response);
        }
        Err(Error::RateLimited { path: path.to_owned() })
    }

    pub async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T, Error> {
        let response = self.request(Method::GET, path, None).await?;
        response.json().await.map_err(|e| Error::Decode {
            path: path.to_owned(),
            reason: e.to_string(),
        })
    }

    /// POST that returns the JSON answer.
    pub async fn post_json<T: DeserializeOwned>(&self, path: &str, body: &Value) -> Result<T, Error> {
        let response = self.request(Method::POST, path, Some(body)).await?;
        response.json().await.map_err(|e| Error::Decode { path: path.to_owned(), reason: e.to_string() })
    }

    pub async fn put_json(&self, path: &str, body: &Value) -> Result<(), Error> {
        self.request(Method::PUT, path, Some(body)).await.map(drop)
    }

    pub async fn post(&self, path: &str, body: &Value) -> Result<(), Error> {
        self.request(Method::POST, path, Some(body)).await.map(drop)
    }
}

impl Rest {
    /// Largest image the UI will decode. A malicious or huge file must not
    /// be able to balloon memory.
    const MAX_IMAGE_BYTES: usize = 8 * 1024 * 1024;

    /// Downloads a CDN file. Sent without the token: the CDN is public and
    /// the token must never leave for hosts other than Discord's API.
    pub async fn fetch_image(&self, url: &str) -> Result<Vec<u8>, Error> {
        let path = || url.to_owned();
        let response = self
            .http
            .get(url)
            .send()
            .await
            .map_err(|e| Error::Network(e.to_string()))?;
        let status = response.status();
        if !status.is_success() {
            return Err(Error::Http { status: status.as_u16(), path: path() });
        }
        if response.content_length().is_some_and(|n| n > Self::MAX_IMAGE_BYTES as u64) {
            return Err(Error::Decode { path: path(), reason: "image too large".into() });
        }
        let bytes = response.bytes().await.map_err(|e| Error::Network(e.to_string()))?;
        if bytes.len() > Self::MAX_IMAGE_BYTES {
            return Err(Error::Decode { path: path(), reason: "image too large".into() });
        }
        Ok(bytes.to_vec())
    }
}

impl Rest {
    /// Uploads bytes to the storage URL Discord handed out. No account
    /// credentials go to that host.
    pub async fn upload_to(&self, url: &str, data: Vec<u8>, content_type: &str) -> Result<(), Error> {
        let response = self
            .http
            .put(url)
            .header("Content-Type", content_type)
            .body(data)
            .send()
            .await
            .map_err(|e| Error::Network(e.to_string()))?;
        let status = response.status();
        if !status.is_success() {
            return Err(Error::Http { status: status.as_u16(), path: "attachment upload".to_owned() });
        }
        Ok(())
    }

    pub async fn put(&self, path: &str) -> Result<(), Error> {
        self.request(Method::PUT, path, None).await.map(drop)
    }

    pub async fn delete(&self, path: &str) -> Result<(), Error> {
        self.request(Method::DELETE, path, None).await.map(drop)
    }

    pub async fn patch(&self, path: &str, body: &Value) -> Result<(), Error> {
        self.request(Method::PATCH, path, Some(body)).await.map(drop)
    }
}
