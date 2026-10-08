//! Calling a model from a plugin.
//!
//! `model_ask` sends one request and returns at once. The reply comes back to
//! a closure the plugin passed with it, through the plugin's gate like any
//! other call, so the closure may change what the plugin keeps without a race.
//! Many asks can be in flight together: a plugin that summarises in the
//! background does not wait for one before it sends the next.
//!
//! The requests run on a runtime of their own rather than on the caller's. A
//! plugin is called from the agent's task, from the hub and from the blocking
//! pool, and only some of those are inside a Tokio runtime at all.

use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::time::Duration;

use aphid_agent::{StreamFn, live_stream_fn};
use aphid_core::{
    AssistantMeta, ContentRef, Event, MessageBuffer, Model, SimpleStreamOptions, ThinkingLevel,
    Transcript,
};
use compact_str::CompactString;
use futures_core::Stream;
use rhai::{Array, Dynamic, Engine, EvalAltResult, FnPtr, Map};

use super::script::ScriptPlugin;
use crate::model::Catalog;

/// At most this many requests from all plugins run at once. A plugin sets a
/// lower limit of its own; this one only keeps a runaway loop from opening a
/// thousand connections.
const MOST_AT_ONCE: usize = 16;

/// How long one ask may take when the plugin does not say.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(300);

/// Finds the API key for a model.
pub type KeyFn = Arc<dyn Fn(&Model) -> Option<CompactString> + Send + Sync>;

/// What a host lets its plugins call: the models and how to reach them.
pub struct ModelAccess {
    catalog: Catalog,
    backend: StreamFn,
    key: KeyFn,
}

impl ModelAccess {
    /// The models in `~/.aphid/models.json`, over HTTP, with each key read from
    /// the model's `api_key_env` variable.
    #[must_use]
    pub fn live() -> Self {
        Self::new(Catalog::new(), live_stream_fn(), Arc::new(key_from_env))
    }

    /// The seam a test uses: its own models, a scripted backend and a key.
    #[must_use]
    pub fn new(
        catalog: Catalog,
        backend: StreamFn,
        key: KeyFn,
    ) -> Self {
        Self {
            catalog,
            backend,
            key,
        }
    }
}

impl std::fmt::Debug for ModelAccess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModelAccess")
            .field("models", &self.catalog.models().len())
            .finish_non_exhaustive()
    }
}

fn key_from_env(model: &Model) -> Option<CompactString> {
    let variable = model.api_key_env.as_ref()?;
    std::env::var(variable.as_str())
        .ok()
        .filter(|key| !key.is_empty())
        .map(Into::into)
}

/// The runtime model requests run on, shared by every plugin of a host.
///
/// Started on the first ask, so a session whose plugins never call a model
/// never starts a thread for it. The runtime lives on a thread of its own and
/// is dropped there: dropping a runtime from inside another one panics, and the
/// host is often dropped from inside one.
#[derive(Default)]
pub(crate) struct Models {
    started: OnceLock<Started>,
    next: AtomicI64,
}

struct Started {
    handle: tokio::runtime::Handle,
    limit: Arc<tokio::sync::Semaphore>,
    /// Dropped with the host, which tells the runtime thread to stop.
    _stop: Mutex<mpsc::Sender<()>>,
}

impl Models {
    fn started(&self) -> Result<&Started, String> {
        if let Some(started) = self.started.get() {
            return Ok(started);
        }
        let (handles, handle) = mpsc::channel();
        let (stop, stopped) = mpsc::channel::<()>();
        std::thread::Builder::new()
            .name("aphid-plugin-models".to_owned())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .thread_name("aphid-plugin-model")
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        let _ = handles.send(Err(error.to_string()));
                        return;
                    }
                };
                let _ = handles.send(Ok(runtime.handle().clone()));
                // Returns once the sender is dropped, with the host.
                let _ = stopped.recv();
                runtime.shutdown_background();
            })
            .map_err(|error| format!("could not start the model thread: {error}"))?;
        let handle = handle
            .recv()
            .map_err(|_| "the model thread stopped".to_owned())??;
        let _ = self.started.set(Started {
            handle,
            limit: Arc::new(tokio::sync::Semaphore::new(MOST_AT_ONCE)),
            _stop: Mutex::new(stop),
        });
        self.started
            .get()
            .ok_or_else(|| "the model thread did not start".to_owned())
    }
}

/// How many asks of one plugin are in flight.
#[derive(Default)]
pub(crate) struct Busy(AtomicUsize);

/// `model_ask`, `model_busy` and `model_list`.
pub(crate) fn register(
    engine: &mut Engine,
    access: Option<&Arc<ModelAccess>>,
    models: &Arc<Models>,
    owner: &Arc<Mutex<Option<Arc<ScriptPlugin>>>>,
) {
    let busy = Arc::new(Busy::default());

    let list = access.cloned();
    engine.register_fn("model_list", move || -> Array {
        let Some(access) = &list else {
            return Array::new();
        };
        access
            .catalog
            .models()
            .iter()
            .map(|model| {
                let mut entry = Map::new();
                entry.insert("id".into(), model.id.to_string().into());
                entry.insert("name".into(), model.name.clone().into());
                entry.insert("provider".into(), model.provider.to_string().into());
                entry.insert("input_cost".into(), model.cost.rates.input.into());
                entry.insert("output_cost".into(), model.cost.rates.output.into());
                entry.insert("reasoning".into(), model.reasoning.into());
                Dynamic::from_map(entry)
            })
            .collect()
    });

    let counter = Arc::clone(&busy);
    engine.register_fn("model_busy", move || {
        i64::try_from(counter.0.load(Ordering::Relaxed)).unwrap_or(i64::MAX)
    });

    let access = access.cloned();
    let models = Arc::clone(models);
    let owner = Arc::clone(owner);
    engine.register_fn(
        "model_ask",
        move |request: Map, reply: FnPtr| -> Result<i64, Box<EvalAltResult>> {
            let access = access
                .clone()
                .ok_or("model_ask is not available to plugins here")?;
            let plugin = owner
                .lock()
                .ok()
                .and_then(|slot| slot.clone())
                .ok_or("the plugin is not loaded")?;
            let ask = Ask::read(&access, &request)?;
            let started = models.started()?;
            let id = models.next.fetch_add(1, Ordering::Relaxed) + 1;

            let busy = Arc::clone(&busy);
            busy.0.fetch_add(1, Ordering::Relaxed);
            let limit = Arc::clone(&started.limit);
            let handle = started.handle.clone();
            started.handle.spawn(async move {
                let answer = match limit.acquire_owned().await {
                    Ok(_permit) => ask.run(&access).await,
                    Err(_) => Answer::failed("the model runtime is closing"),
                };
                busy.0.fetch_sub(1, Ordering::Relaxed);
                let result = answer.into_map(id);
                // The reply takes the plugin's gate, which may wait, so it does
                // not wait on a worker the other requests need.
                let _ = handle
                    .spawn_blocking(move || {
                        if let Err(error) = plugin.call_fn(&reply, (result,)) {
                            plugin.report(&format!("a model reply failed: {error}"));
                        }
                    })
                    .await;
            });
            Ok(id)
        },
    );
}

/// One request, read out of the map a script passed.
struct Ask {
    model: Model,
    transcript: Transcript,
    options: SimpleStreamOptions,
    timeout: Duration,
}

impl Ask {
    fn read(access: &ModelAccess, request: &Map) -> Result<Ask, String> {
        let text = |key: &str| {
            request
                .get(key)
                .filter(|value| value.is_string())
                .map(ToString::to_string)
        };
        let name = text("model").ok_or("model_ask needs a `model`")?;
        let model = access
            .catalog
            .resolve(&name)
            .map_err(|error| format!("model `{name}`: {error}"))?;

        let mut transcript = Transcript::new();
        if let Some(system) = text("system") {
            transcript.push_system(&system);
        }
        let messages = request
            .get("messages")
            .and_then(|value| value.read_lock::<Array>().map(|items| items.clone()))
            .ok_or("model_ask needs `messages`, an array of #{ role, text }")?;
        if messages.is_empty() {
            return Err("model_ask needs at least one message".to_owned());
        }
        for message in &messages {
            let message = message
                .read_lock::<Map>()
                .ok_or("each message is a map of #{ role, text }")?;
            let content = message
                .get("text")
                .map(ToString::to_string)
                .unwrap_or_default();
            match message.get("role").map(ToString::to_string).as_deref() {
                Some("assistant") => {
                    let mut buffer = MessageBuffer::new(AssistantMeta::new(
                        model.api.clone(),
                        model.provider.clone(),
                        model.id.clone(),
                    ));
                    let block = buffer.begin_text();
                    buffer.push_delta(block, &content);
                    transcript.commit(buffer);
                }
                Some("user") | None => {
                    transcript.push_user(&content);
                }
                Some(other) => {
                    return Err(format!(
                        "a message role is \"user\" or \"assistant\", not {other:?}"
                    ));
                }
            }
        }

        let mut options = SimpleStreamOptions::default();
        options.stream.request.api_key = (access.key)(&model);
        if let Some(most) = request
            .get("max_tokens")
            .and_then(|value| value.as_int().ok())
        {
            options.stream.max_tokens = u32::try_from(most).ok();
        }
        let wanted = match text("thinking").as_deref() {
            None | Some("off") => None,
            Some(level) => Some(thinking(level)?),
        };
        options.reasoning = crate::model::clamp_thinking(&model, wanted).0;

        let timeout = request
            .get("timeout_ms")
            .and_then(|value| value.as_int().ok())
            .and_then(|ms| u64::try_from(ms).ok())
            .map_or(DEFAULT_TIMEOUT, Duration::from_millis);

        Ok(Ask {
            model,
            transcript,
            options,
            timeout,
        })
    }

    async fn run(self, access: &ModelAccess) -> Answer {
        let work = async {
            let mut stream = access
                .backend
                .stream(&self.model, &self.transcript, &[], &self.options)
                .await;
            // Errors arrive in the finished message, so the events themselves
            // carry nothing this needs.
            while next(&mut stream).await.is_some() {}
            stream.finish_boxed()
        };
        let Ok(buffer) = tokio::time::timeout(self.timeout, work).await else {
            return Answer::failed(&format!(
                "no answer after {} seconds",
                self.timeout.as_secs()
            ));
        };

        let meta = buffer.meta().clone();
        let mut out = Transcript::new();
        let id = out.commit(buffer);
        let text: String = out
            .message(id)
            .content()
            .filter_map(|part| match part {
                ContentRef::Text(text) => Some(text.text()),
                _ => None,
            })
            .collect();

        Answer {
            ok: !meta.stop_reason.is_failure(),
            text,
            error: meta.error_message.unwrap_or_default(),
            stop: super::host::stop_reason(meta.stop_reason).to_owned(),
            input: meta.usage.input,
            output: meta.usage.output,
            cache_read: meta.usage.cache_read,
            cost: meta.usage.cost.total,
        }
    }
}

fn thinking(level: &str) -> Result<ThinkingLevel, String> {
    [
        ThinkingLevel::Minimal,
        ThinkingLevel::Low,
        ThinkingLevel::Medium,
        ThinkingLevel::High,
        ThinkingLevel::XHigh,
        ThinkingLevel::Max,
    ]
    .into_iter()
    .find(|known| known.as_str() == level)
    .ok_or_else(|| format!("`thinking` is \"off\" or a level such as \"medium\", not {level:?}"))
}

/// What came back, before it becomes a map.
struct Answer {
    ok: bool,
    text: String,
    error: String,
    stop: String,
    input: u32,
    output: u32,
    cache_read: u32,
    cost: f64,
}

impl Answer {
    fn failed(error: &str) -> Answer {
        Answer {
            ok: false,
            text: String::new(),
            error: error.to_owned(),
            stop: "error".to_owned(),
            input: 0,
            output: 0,
            cache_read: 0,
            cost: 0.0,
        }
    }

    fn into_map(self, id: i64) -> Map {
        let mut map = Map::new();
        map.insert("id".into(), id.into());
        map.insert("ok".into(), self.ok.into());
        map.insert("text".into(), self.text.into());
        map.insert("error".into(), self.error.into());
        map.insert("stop".into(), self.stop.into());
        map.insert("input".into(), i64::from(self.input).into());
        map.insert("output".into(), i64::from(self.output).into());
        map.insert("cache_read".into(), i64::from(self.cache_read).into());
        map.insert("cost".into(), self.cost.into());
        map
    }
}

async fn next<S: Stream<Item = Event> + Unpin + ?Sized>(stream: &mut S) -> Option<Event> {
    std::future::poll_fn(|cx| std::pin::Pin::new(&mut *stream).poll_next(cx)).await
}
