//! Reading models.dev: name resolution, the conversion into a [`Model`], and
//! the cache.
//!
//! Everything here runs against a checked-in slice of the real document, so no
//! test needs the network. The slice is small on purpose but every record in it
//! is verbatim: the shapes that broke the parser once — a `-1` token budget, an
//! `interleaved` that is a bare `true`, a `null` inside a `values` array — are
//! the ones worth keeping honest.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use aphid_core::models_dev::{
    self, CachePolicy, ConvertError, FindError, Index, Overrides, Source,
};
use aphid_core::{LevelMapping, Model, ModelThinkingLevel, ThinkingLevel, providers::deepseek};

const FIXTURE: &str = include_str!("fixtures/models_dev.json");

fn index() -> Index {
    models_dev::parse(FIXTURE).expect("the fixture is a models.dev document")
}

fn convert(name: &str) -> Model {
    let index = index();
    let entry = models_dev::find(&index, name).expect("a known model");
    models_dev::to_model(&entry, &Overrides::default())
        .expect("a convertible model")
        .model
}

struct Temp {
    root: PathBuf,
}

impl Temp {
    fn new() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "aphid-models-dev-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).expect("temp dir");
        Self { root }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

// ---------------------------------------------------------------------------
// Resolution
// ---------------------------------------------------------------------------

#[test]
fn a_qualified_name_resolves() {
    let index = index();
    let entry = models_dev::find(&index, "deepseek/deepseek-v4-flash").expect("a known model");
    assert_eq!(entry.provider_id, "deepseek");
    assert_eq!(entry.model_id, "deepseek-v4-flash");
    assert_eq!(entry.qualified(), "deepseek/deepseek-v4-flash");
}

#[test]
fn a_bare_id_resolves_when_only_one_provider_serves_it() {
    let index = index();
    let entry = models_dev::find(&index, "glm-5").expect("a known model");
    assert_eq!(entry.qualified(), "zhipuai/glm-5");
}

#[test]
fn a_bare_id_several_providers_serve_is_refused_with_the_list() {
    let index = index();
    let Err(FindError::Ambiguous { providers, .. }) = models_dev::find(&index, "deepseek-v4-pro")
    else {
        panic!("expected an ambiguity");
    };
    assert!(providers.contains(&"deepseek".to_owned()));
    assert!(providers.contains(&"venice".to_owned()));
}

#[test]
fn a_model_id_that_contains_a_slash_still_resolves() {
    // `wandb` serves a model literally called `openai/gpt-oss-120b`. Splitting
    // the name at the first slash first would read `openai` as the provider.
    let index = index();
    assert_eq!(
        models_dev::find(&index, "openai/gpt-oss-120b")
            .expect("the bare id")
            .qualified(),
        "wandb/openai/gpt-oss-120b"
    );
    assert_eq!(
        models_dev::find(&index, "wandb/openai/gpt-oss-120b")
            .expect("the qualified id")
            .qualified(),
        "wandb/openai/gpt-oss-120b"
    );
}

#[test]
fn an_unknown_name_says_so() {
    let index = index();
    assert!(matches!(
        models_dev::find(&index, "gpt-9-ultra"),
        Err(FindError::Unknown { .. })
    ));
}

#[test]
fn search_matches_the_provider_the_id_and_the_name() {
    let index = index();
    assert!(!models_dev::search(&index, "GLM").is_empty(), "by name");
    assert!(
        !models_dev::search(&index, "venice").is_empty(),
        "by provider"
    );
    assert!(!models_dev::search(&index, "v4-flash").is_empty(), "by id");
    assert!(models_dev::search(&index, "nothing-like-this").is_empty());
}

// ---------------------------------------------------------------------------
// Conversion
// ---------------------------------------------------------------------------

#[test]
fn deepseek_flash_converts_to_the_model_aphid_ships() {
    // The one case with an independently written answer to check against.
    assert_eq!(convert("deepseek/deepseek-v4-flash"), deepseek::flash());
}

#[test]
fn efforts_round_up_to_what_the_model_actually_offers() {
    // models.dev says pro offers `high` and `max` only, so everything below
    // `high` is sent as `high`. Asking for more reasoning than requested is the
    // safe direction; sending an effort the endpoint rejects is not.
    let model = convert("deepseek/deepseek-v4-pro");
    let resolve = |level: ThinkingLevel| {
        model
            .thinking_levels
            .resolve(ModelThinkingLevel::Level(level))
            .map(ToOwned::to_owned)
    };
    assert_eq!(resolve(ThinkingLevel::Minimal).as_deref(), Some("high"));
    assert_eq!(resolve(ThinkingLevel::Medium).as_deref(), Some("high"));
    assert_eq!(resolve(ThinkingLevel::High).as_deref(), Some("high"));
    assert_eq!(resolve(ThinkingLevel::XHigh).as_deref(), Some("max"));
    assert_eq!(resolve(ThinkingLevel::Max).as_deref(), Some("max"));
}

#[test]
fn a_toggle_becomes_the_off_level() {
    let model = convert("deepseek/deepseek-v4-flash");
    assert_eq!(
        model.thinking_levels.get(ModelThinkingLevel::Off),
        &LevelMapping::Value("disabled".into())
    );
}

#[test]
fn a_context_tier_becomes_a_price_threshold() {
    let model = convert("impossibl/google/gemini-3.1-pro-preview");
    assert_eq!(model.cost.tiers.len(), 1);
    assert_eq!(model.cost.tiers[0].input_tokens_above, 200_000);
    assert!(
        model.cost.rates_for(300_000).input > model.cost.rates_for(100_000).input,
        "the tier is the more expensive one"
    );
}

#[test]
fn the_provider_key_variable_is_recorded() {
    assert_eq!(
        convert("zhipuai/glm-5").api_key_env.as_deref(),
        Some("ZHIPU_API_KEY")
    );
}

#[test]
fn a_generic_endpoint_gets_the_conservative_profile() {
    // A server that only claims to speak the protocol is assumed to want the
    // legacy `max_tokens` and to reject the newer fields.
    let flags = convert("zhipuai/glm-5")
        .compat
        .openai_completions()
        .cloned()
        .expect("openai completions");
    assert_eq!(
        flags.max_tokens_field,
        aphid_core::MaxTokensField::MaxTokens
    );
    assert!(!flags.supports_store);
    assert!(!flags.supports_developer_role);
}

#[test]
fn a_protocol_aphid_does_not_speak_is_refused() {
    let index = index();
    let entry = models_dev::find(&index, "anthropic/claude-sonnet-4-6").expect("a known model");
    let Err(ConvertError::UnsupportedApi { provider, npm }) =
        models_dev::to_model(&entry, &Overrides::default())
    else {
        panic!("expected a refusal");
    };
    assert_eq!(provider, "anthropic");
    assert_eq!(npm, "@ai-sdk/anthropic");
}

#[test]
fn the_refusal_can_be_overridden() {
    let index = index();
    let entry = models_dev::find(&index, "anthropic/claude-sonnet-4-6").expect("a known model");
    let overrides = Overrides {
        api: Some(aphid_core::Api::OpenAiCompletions),
        base_url: Some("http://localhost:8080/v1".to_owned()),
        ..Overrides::default()
    };
    let model = models_dev::to_model(&entry, &overrides)
        .expect("the override wins")
        .model;
    assert_eq!(model.base_url, "http://localhost:8080/v1");
}

#[test]
fn a_provider_without_an_endpoint_asks_for_one() {
    let index = index();
    let entry = models_dev::find(&index, "azure-cognitive-services/gpt-chat-latest")
        .expect("a known model");
    assert!(matches!(
        models_dev::to_model(&entry, &Overrides::default()),
        Err(ConvertError::MissingBaseUrl { .. })
    ));

    let overrides = Overrides {
        base_url: Some("https://example.openai.azure.com".to_owned()),
        ..Overrides::default()
    };
    assert!(models_dev::to_model(&entry, &overrides).is_ok());
}

// ---------------------------------------------------------------------------
// The cache
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_fresh_cache_is_used_without_touching_the_network() {
    let temp = Temp::new();
    let path = temp.path("models.dev.json");
    models_dev::write_cache(&path, FIXTURE).expect("write");

    let (index, source) = models_dev::load(&path, CachePolicy::Ttl(Duration::from_secs(3600)))
        .await
        .expect("the cache is enough");
    assert_eq!(index.provider_count(), 7);
    assert!(matches!(source, Source::Cache { stale: false, .. }));
}

#[tokio::test]
async fn the_cache_only_policy_fails_when_there_is_no_cache() {
    let temp = Temp::new();
    let error = models_dev::load(&temp.path("absent.json"), CachePolicy::Offline)
        .await
        .expect_err("nothing to read");
    assert!(
        error.to_string().contains("aphid model update"),
        "the message should say how to fix it: {error}"
    );
}

#[tokio::test]
async fn a_cache_past_its_ttl_is_stale() {
    let temp = Temp::new();
    let path = temp.path("models.dev.json");
    models_dev::write_cache(&path, FIXTURE).expect("write");

    // A zero TTL is the same question as an old file, without the wait.
    let cached = models_dev::read_cache(&path).expect("a cache");
    assert!(cached.is_fresh(Duration::from_secs(3600)));
    assert!(!cached.is_fresh(Duration::ZERO));

    let (_, source) = models_dev::load(&path, CachePolicy::Offline)
        .await
        .expect("stale still beats nothing");
    assert!(matches!(source, Source::Cache { stale: true, .. }));
}

#[test]
fn a_clock_that_runs_backwards_does_not_break_freshness() {
    let temp = Temp::new();
    let path = temp.path("models.dev.json");
    models_dev::write_cache(&path, FIXTURE).expect("write");
    std::fs::File::open(&path)
        .expect("open")
        .set_modified(SystemTime::now() + Duration::from_secs(86_400))
        .expect("set a modification time in the future");

    let cached = models_dev::read_cache(&path).expect("a cache");
    assert_eq!(cached.age, Duration::ZERO);
    assert!(cached.is_fresh(Duration::from_secs(1)));
}

#[test]
fn writing_the_cache_leaves_no_temporary_behind() {
    let temp = Temp::new();
    let path = temp.path("nested/models.dev.json");
    models_dev::write_cache(&path, FIXTURE).expect("write creates the directory");

    let left: Vec<_> = std::fs::read_dir(path.parent().expect("parent"))
        .expect("read dir")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name())
        .collect();
    assert_eq!(left, vec![std::ffi::OsString::from("models.dev.json")]);
}

#[test]
fn the_awkward_shapes_in_the_real_document_still_parse() {
    // Each of these failed the parser at some point, and each is verbatim from
    // models.dev: an `interleaved` written as a bare `true`, a `-1` token
    // budget, a `null` among a `values` array.
    let document = r#"{
      "p": {
        "id": "p", "name": "P", "npm": "@ai-sdk/openai-compatible",
        "api": "https://example.test/v1", "doc": null, "env": ["P_KEY", null],
        "models": {
          "m": {
            "id": "m", "name": null, "reasoning": true,
            "reasoning_options": [
              {"type": "budget_tokens", "min": -1, "max": 32768},
              {"type": "effort", "values": [null, "low", "medium", "high"]},
              {"type": "something-new"}
            ],
            "interleaved": true,
            "modalities": {"input": ["text", "video"], "output": ["text"]},
            "limit": {"context": 32768, "output": 4096},
            "cost": {"input": 1.0, "output": 2.0}
          }
        }
      }
    }"#;

    let index = models_dev::parse(document).expect("an awkward but valid document");
    let entry = models_dev::find(&index, "m").expect("the model");
    let converted = models_dev::to_model(&entry, &Overrides::default()).expect("convertible");

    assert_eq!(
        converted.model.name, "m",
        "a null name falls back to the id"
    );
    assert_eq!(converted.model.api_key_env.as_deref(), Some("P_KEY"));
    assert_eq!(
        converted
            .model
            .thinking_levels
            .resolve(ModelThinkingLevel::Level(ThinkingLevel::Max)),
        Some("high"),
        "the null effort is dropped, and max rounds to the strongest left"
    );
    assert!(
        converted.notes.iter().any(|note| note.contains("video")),
        "an unusable modality is reported: {:?}",
        converted.notes
    );
    assert!(
        converted
            .notes
            .iter()
            .any(|note| note.contains("token budget")),
        "a budget-graded model is reported: {:?}",
        converted.notes
    );
}
