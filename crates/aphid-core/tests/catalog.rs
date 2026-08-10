//! The `~/.aphid/models.json` format: what survives a round trip, and what a
//! broken file does.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use aphid_core::catalog::{
    self, CompatEntry, CompatProfile, ConfigError, EntryError, LevelEntry, ModelEntry,
    ModelsConfig, TierEntry,
};
use aphid_core::{
    Compat, LevelMapping, MaxTokensField, Model, ModelThinkingLevel, OpenAiCompletionsCompat,
    ThinkingLevel, providers::deepseek,
};

struct Temp {
    root: PathBuf,
}

impl Temp {
    fn new() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "aphid-catalog-{}-{}",
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

#[test]
fn a_missing_file_is_an_empty_catalog() {
    let temp = Temp::new();
    let config = catalog::load(&temp.path("nothing-here.json")).expect("a missing file is fine");
    assert!(config.models().is_empty());
    assert_eq!(config.version, catalog::VERSION);
}

#[test]
fn an_empty_file_is_an_empty_catalog() {
    // What a truncated write leaves behind. Reporting a parse error for it would
    // tell the user nothing they could act on.
    let temp = Temp::new();
    let path = temp.path("empty.json");
    std::fs::write(&path, "   \n").expect("write");
    assert!(
        catalog::load(&path)
            .expect("empty is fine")
            .models()
            .is_empty()
    );
}

#[test]
fn a_broken_file_reports_rather_than_panicking() {
    let temp = Temp::new();
    let path = temp.path("broken.json");
    std::fs::write(&path, "{ not json").expect("write");
    assert!(matches!(
        catalog::load(&path),
        Err(ConfigError::Parse { .. })
    ));
}

#[test]
fn a_newer_format_is_refused_by_name() {
    let temp = Temp::new();
    let path = temp.path("future.json");
    std::fs::write(&path, r#"{"version": 99, "models": []}"#).expect("write");
    let Err(ConfigError::Version { found, .. }) = catalog::load(&path) else {
        panic!("expected a version error");
    };
    assert_eq!(found, 99);
}

#[test]
fn a_built_in_model_round_trips_through_the_file() {
    let temp = Temp::new();
    let path = temp.path("models.json");

    let mut config = ModelsConfig::default();
    for model in deepseek::models() {
        config.push_or_replace(ModelEntry::from(&model));
    }
    catalog::save(&path, &config).expect("save");

    let loaded = catalog::load(&path).expect("load");
    for (entry, original) in loaded.models().iter().zip(deepseek::models()) {
        let model = Model::try_from(entry).expect("a complete entry");
        assert_eq!(model, original, "{} did not survive the trip", original.id);
    }
}

#[test]
fn an_exact_profile_writes_only_its_name() {
    // The point of naming profiles: DeepSeek's fifteen flags come back as one
    // word, so the file stays readable.
    let entry = ModelEntry::from(&deepseek::pro());
    assert_eq!(entry.compat.profile, CompatProfile::Deepseek);
    assert_eq!(
        entry.compat,
        CompatEntry {
            profile: CompatProfile::Deepseek,
            ..CompatEntry::default()
        }
    );

    let json = serde_json::to_string(&entry.compat).expect("serialize");
    assert_eq!(json, r#"{"profile":"deepseek"}"#);
}

#[test]
fn a_profile_records_only_the_flags_that_differ() {
    let flags = OpenAiCompletionsCompat {
        supports_strict_mode: true,
        ..OpenAiCompletionsCompat::compatible()
    };
    let entry = CompatEntry::from_compat(&Compat::from(flags.clone()));

    assert_eq!(entry.profile, CompatProfile::Compatible);
    assert_eq!(entry.supports_strict_mode, Some(true));
    assert_eq!(entry.supports_store, None, "the profile already says this");

    // And it rebuilds exactly what it described.
    assert_eq!(
        entry.to_compat(&aphid_core::Api::OpenAiCompletions),
        Compat::from(flags)
    );
}

#[test]
fn tiered_pricing_survives_the_file() {
    let temp = Temp::new();
    let path = temp.path("models.json");

    let mut entry = ModelEntry::from(&deepseek::pro());
    entry.id = "tiered".to_owned();
    entry.cost.tiers = vec![TierEntry {
        input_tokens_above: 200_000,
        input: 4.0,
        output: 18.0,
        cache_read: 0.4,
        cache_write: 0.0,
    }];

    let mut config = ModelsConfig::default();
    config.push_or_replace(entry);
    catalog::save(&path, &config).expect("save");

    let loaded = catalog::load(&path).expect("load");
    let model = Model::try_from(&loaded.models()[0]).expect("a complete entry");
    assert_eq!(model.cost.rates_for(100_000).input, 0.435);
    assert_eq!(model.cost.rates_for(300_000).input, 4.0);
}

#[test]
fn thinking_levels_distinguish_unsupported_from_absent() {
    let mut model = deepseek::flash();
    model
        .thinking_levels
        .set(ThinkingLevel::Max.into(), LevelMapping::Unsupported);
    model
        .thinking_levels
        .set(ThinkingLevel::High.into(), LevelMapping::Default);

    let entry = ModelEntry::from(&model);
    assert_eq!(
        entry.thinking_levels.max,
        Some(LevelEntry::Supported(false))
    );
    assert_eq!(entry.thinking_levels.high, None, "a default writes nothing");

    let rebuilt = Model::try_from(&entry).expect("a complete entry");
    assert_eq!(rebuilt.thinking_levels, model.thinking_levels);
    assert!(!rebuilt.thinking_levels.supports(ThinkingLevel::Max.into()));
}

#[test]
fn a_model_that_does_not_reason_needs_no_thinking_block() {
    let mut model = deepseek::flash();
    model.reasoning = false;
    for level in [
        ModelThinkingLevel::Off,
        ThinkingLevel::Minimal.into(),
        ThinkingLevel::Low.into(),
        ThinkingLevel::Medium.into(),
        ThinkingLevel::High.into(),
        ThinkingLevel::XHigh.into(),
        ThinkingLevel::Max.into(),
    ] {
        model.thinking_levels.set(level, LevelMapping::Unsupported);
    }

    let entry = ModelEntry::from(&model);
    assert!(
        entry.thinking_levels.is_empty(),
        "every level unsupported is what `reasoning: false` already means"
    );
    assert_eq!(
        Model::try_from(&entry)
            .expect("a complete entry")
            .thinking_levels,
        model.thinking_levels
    );
}

#[test]
fn an_entry_without_a_base_url_is_refused() {
    let mut entry = ModelEntry::from(&deepseek::flash());
    entry.base_url = String::new();
    assert_eq!(
        Model::try_from(&entry),
        Err(EntryError::MissingBaseUrl {
            id: "deepseek-v4-flash".to_owned()
        })
    );
}

#[test]
fn a_hand_written_entry_needs_only_five_fields() {
    // What a user can reasonably be expected to type. Everything else defaults.
    let entry: ModelEntry = serde_json::from_str(
        r#"{
            "id": "local-llama",
            "base_url": "http://localhost:8080/v1",
            "context_window": 32768,
            "max_tokens": 4096
        }"#,
    )
    .expect("parse");

    let model = Model::try_from(&entry).expect("a complete entry");
    assert_eq!(model.id, "local-llama");
    assert_eq!(model.name, "local-llama", "the name falls back to the id");
    assert_eq!(model.api, aphid_core::Api::OpenAiCompletions);
    assert!(model.input.contains(aphid_core::InputModalities::TEXT));
    assert!(!model.reasoning);
    assert_eq!(model.api_key_env, None);
}

#[test]
fn saving_replaces_rather_than_appends() {
    let temp = Temp::new();
    let path = temp.path("models.json");

    let mut config = ModelsConfig::default();
    assert!(!config.push_or_replace(ModelEntry::from(&deepseek::flash())));
    assert!(config.push_or_replace(ModelEntry::from(&deepseek::flash())));
    assert_eq!(config.models().len(), 1);

    catalog::save(&path, &config).expect("save");
    assert_eq!(catalog::load(&path).expect("load").models().len(), 1);

    assert!(config.remove("deepseek-v4-flash").is_some());
    assert!(config.remove("deepseek-v4-flash").is_none());
}

#[test]
fn a_write_leaves_no_temporary_behind() {
    let temp = Temp::new();
    let path = temp.path("nested/models.json");
    catalog::save(&path, &ModelsConfig::default()).expect("save creates the directory");

    let left: Vec<_> = std::fs::read_dir(path.parent().expect("parent"))
        .expect("read dir")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name())
        .collect();
    assert_eq!(left, vec![std::ffi::OsString::from("models.json")]);
}

#[test]
fn the_legacy_max_tokens_field_survives_an_override() {
    let flags = OpenAiCompletionsCompat {
        max_tokens_field: MaxTokensField::MaxTokens,
        ..OpenAiCompletionsCompat::default()
    };
    let entry = CompatEntry::from_compat(&Compat::from(flags));
    assert_eq!(
        entry
            .to_compat(&aphid_core::Api::OpenAiCompletions)
            .openai_completions()
            .expect("openai completions")
            .max_tokens_field,
        MaxTokensField::MaxTokens
    );
}
