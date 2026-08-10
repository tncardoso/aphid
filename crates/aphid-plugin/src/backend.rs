//! Letting a script see the request before it is sent.
//!
//! The agent loop hands a [`Backend`] the transcript, not a request body — the
//! body is encoded inside the transport, and a test backend has none at all. So
//! `on_request` is not a loop hook: it is a decorator that wraps whichever
//! backend the agent was built with, encodes the body itself, shows it to the
//! scripts, and sends what comes back.
//!
//! Because it owns the transport, this **replaces** a backend rather than
//! wrapping one. [`ScriptBackend::install`] hands back a backend only when a
//! script actually subscribes, so a caller that has its own backend — a test
//! script, a router — can see the conflict and decide, instead of having its
//! backend quietly bypassed.

use std::sync::Arc;

use aphid_agent::{Backend, BoxFuture, BoxStream, StreamFn};
use aphid_core::{Model, SimpleStreamOptions, Tool, Transcript, api};

use crate::convert;
use crate::host::PluginHost;

/// The hook this decorator dispatches.
const HOOK: &str = "on_request";

/// The live transport, with each request body shown to the loaded scripts.
pub struct ScriptBackend {
    host: Arc<PluginHost>,
}

impl ScriptBackend {
    /// A backend for these plugins, or `None` when no script wants requests.
    ///
    /// `None` is the common answer, and it means the caller should keep whatever
    /// backend it already had.
    #[must_use]
    pub fn install(host: &Arc<PluginHost>) -> Option<StreamFn> {
        host.plugins()
            .iter()
            .any(|plugin| plugin.defines(HOOK))
            .then(|| {
                Arc::new(Self {
                    host: Arc::clone(host),
                }) as StreamFn
            })
    }

    /// Show an encoded body to every subscriber, in load order.
    ///
    /// A hook returns a map to replace the body wholesale, or unit to leave it
    /// alone. A script that raises leaves the body untouched — failing open,
    /// because a rewrite nobody completed is not a reason to drop the request.
    fn rewrite(&self, body: String) -> String {
        let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&body) else {
            return body;
        };
        let mut current = parsed;

        for plugin in self.host.plugins().iter().filter(|p| p.defines(HOOK)) {
            let payload = convert::object_to_map(&current);
            let Some(returned) = plugin.call(HOOK, (payload,)) else {
                continue;
            };
            if returned.is_map() {
                current = convert::to_json(&returned);
            }
        }

        serde_json::to_string(&current).unwrap_or(body)
    }
}

impl Backend for ScriptBackend {
    fn stream<'a>(
        &'a self,
        model: &'a Model,
        transcript: &'a Transcript,
        tools: &'a [Tool],
        options: &'a SimpleStreamOptions,
    ) -> BoxFuture<'a, BoxStream> {
        Box::pin(async move {
            // `api::stream` reports an encoding failure as a stream that opens
            // with an error, which is the one shape the loop knows how to
            // handle. Re-encoding to get there costs nothing on a path that is
            // about to fail anyway.
            let Ok(body) = api::encode_request(model, transcript, tools, options) else {
                return Box::new(api::stream(model, transcript, tools, options).await) as BoxStream;
            };

            let body = self.rewrite(body);
            Box::new(api::stream_body(model, body, options).await) as BoxStream
        })
    }
}

impl std::fmt::Debug for ScriptBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ScriptBackend")
    }
}
