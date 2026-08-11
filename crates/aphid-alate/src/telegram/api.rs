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

/// What the bridge needs of Telegram.
pub trait Api: Send + Sync {
    /// Call `method`, and give back the `result` it answered with.
    ///
    /// The method is `&'static str` because every one of them is a literal, and
    /// that saves the future an owned copy.
    fn call(&self, method: &'static str, body: Value) -> Call<'_>;
}

/// A shared API, held by the poll loop and by every chat.
pub type ApiFn = Arc<dyn Api>;

/// The real one.
#[derive(Debug)]
pub struct Live {
    client: reqwest::Client,
    /// `<api>/bot<token>`. Held made up, so the token is written once.
    base: String,
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
        Ok(Self {
            client,
            base: format!("{}/bot{token}", api.trim_end_matches('/')),
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
}
