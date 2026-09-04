//! The Bot API, and the seam a test puts something else in.
//!
//! One method, because the Bot API is one method: every call is `POST
//! /bot<token>/<name>` with a JSON body, and every answer is `{"ok": true,
//! "result": ...}` or `{"ok": false, "description": "..."}`. Naming each call in
//! Rust would be a hundred lines of struct for no more safety than the JSON
//! already has.
//!
//! It is a trait for the reason [`StreamFn`] is one: a test has to answer
//! `getUpdates` from a script and read back what was sent, without a network and
//! without a token.
//!
//! [`StreamFn`]: aphid_agent::StreamFn

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

/// The real Telegram, for the configuration that names none.
pub const API: &str = "https://api.telegram.org";

/// One call in flight.
pub type Call<'a> = Pin<Box<dyn Future<Output = Result<Value, String>> + Send + 'a>>;

/// One download in flight.
pub type Fetch<'a> = Pin<Box<dyn Future<Output = Result<Vec<u8>, String>> + Send + 'a>>;

/// What the bridge needs of Telegram.
pub trait Api: Send + Sync {
    /// Call `method`, and give back the `result` it answered with.
    ///
    /// The method is `&'static str` because every one of them is a literal, and
    /// that saves the future an owned copy.
    fn call(&self, method: &'static str, body: Value) -> Call<'_>;

    /// Get one file, by the `file_path` that `getFile` answered with.
    ///
    /// This is the one thing the Bot API does not do the way everything else
    /// does: a file is a `GET` of `/file/bot<token>/<path>` and comes back as
    /// bytes rather than as JSON. It is here, and not on a client of its own,
    /// so that the seam a test replaces stays one thing.
    fn fetch(&self, path: &str) -> Fetch<'_>;

    /// Send a file as a Telegram document.
    fn document(&self, chat: i64, name: String, data: Vec<u8>, caption: Option<String>)
    -> Call<'_>;
}

/// A shared API, held by the poll loop and by every chat.
pub type ApiFn = Arc<dyn Api>;

/// The real one.
#[derive(Debug)]
pub struct Live {
    client: reqwest::Client,
    /// `<api>/bot<token>`. Held made up, so the token is written once.
    base: String,
    /// `<api>/file/bot<token>`, which is where files are and methods are not.
    files: String,
}

impl Live {
    /// An API that talks to `api` as the bot `token` names.
    ///
    /// # Errors
    ///
    /// Fails when the HTTP client cannot be built, which means the TLS
    /// back end could not start.
    pub fn new(api: &str, token: &str, poll: Duration) -> Result<Self, String> {
        let client = reqwest::Client::builder()
            // Longer than the poll it holds open. A timeout shorter than
            // `getUpdates` waits would cancel every long poll at the moment it
            // was doing its job, and look like a network that never works.
            .timeout(poll + Duration::from_secs(10))
            .build()
            .map_err(|error| format!("could not build an HTTP client: {error}"))?;
        let api = api.trim_end_matches('/');
        Ok(Self {
            client,
            base: format!("{api}/bot{token}"),
            files: format!("{api}/file/bot{token}"),
        })
    }
}

impl Api for Live {
    fn call(&self, method: &'static str, body: Value) -> Call<'_> {
        Box::pin(async move {
            let response = self
                .client
                .post(format!("{}/{method}", self.base))
                .json(&body)
                .send()
                .await
                // Without the URL, always: the URL has the bot token in it, and
                // this string goes to the terminals and into `alate.log`.
                .map_err(|error| format!("{method} failed: {}", error.without_url()))?;

            let answer: Value = response
                .json()
                .await
                .map_err(|error| format!("{method} gave no JSON: {}", error.without_url()))?;

            if answer.get("ok").and_then(Value::as_bool) != Some(true) {
                let why = answer
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or("no reason given");
                return Err(format!("{method} was refused: {why}"));
            }
            Ok(answer.get("result").cloned().unwrap_or(Value::Null))
        })
    }

    fn fetch(&self, path: &str) -> Fetch<'_> {
        let url = format!("{}/{path}", self.files);
        Box::pin(async move {
            let response = self
                .client
                .get(url)
                .send()
                .await
                // Without the URL, as above: this one carries the token too.
                .and_then(reqwest::Response::error_for_status)
                .map_err(|error| {
                    format!("the file could not be fetched: {}", error.without_url())
                })?;

            response
                .bytes()
                .await
                .map(|bytes| bytes.to_vec())
                .map_err(|error| format!("the file stopped coming: {}", error.without_url()))
        })
    }

    fn document(
        &self,
        chat: i64,
        name: String,
        data: Vec<u8>,
        caption: Option<String>,
    ) -> Call<'_> {
        Box::pin(async move {
            let mut form = reqwest::multipart::Form::new()
                .text("chat_id", chat.to_string())
                .part(
                    "document",
                    reqwest::multipart::Part::bytes(data).file_name(name),
                );
            if let Some(caption) = caption {
                form = form.text("caption", caption);
            }
            let response = self
                .client
                .post(format!("{}/sendDocument", self.base))
                .multipart(form)
                .send()
                .await
                .map_err(|error| format!("sendDocument failed: {}", error.without_url()))?;
            let answer: Value = response
                .json()
                .await
                .map_err(|error| format!("sendDocument gave no JSON: {}", error.without_url()))?;
            if answer.get("ok").and_then(Value::as_bool) != Some(true) {
                let why = answer
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or("no reason given");
                return Err(format!("sendDocument was refused: {why}"));
            }
            Ok(answer.get("result").cloned().unwrap_or(Value::Null))
        })
    }
}
