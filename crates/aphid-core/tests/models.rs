//! Model metadata, pricing and the DeepSeek profile.

use aphid_core::{
    Compat, Cost, InputModalities, LevelMapping, MaxTokensField, ModelCost, ModelCostRates,
    ModelCostTier, ModelThinkingLevel, OpenAiCompletionsCompat, ProviderId, ThinkingBudgets,
    ThinkingFormat, ThinkingLevel, ThinkingLevelMap, Usage, providers::deepseek,
};

#[test]
fn the_deepseek_compat_profile_is_what_the_api_documents() {
    let c = OpenAiCompletionsCompat::deepseek();
    assert_eq!(c.max_tokens_field, MaxTokensField::MaxTokens);
    assert_eq!(c.thinking_format, ThinkingFormat::DeepSeek);
    assert!(c.requires_reasoning_content_on_assistant_messages);
    assert!(c.supports_reasoning_effort);
    assert!(c.supports_strict_mode);
    assert!(c.supports_usage_in_streaming);
    assert!(c.supports_finish_reason);
    assert!(c.supports_long_cache_retention);
    assert!(!c.supports_store);
    assert!(!c.supports_developer_role);
    // Thinking mode rejects sampling parameters.
    assert!(!c.supports_temperature_while_thinking);
    assert!(!c.requires_tool_result_name);
    assert!(!c.requires_assistant_after_tool_result);
    assert!(!c.requires_thinking_as_text);

    // It really is a deviation from stock OpenAI.
    assert_ne!(c, OpenAiCompletionsCompat::default());
}

#[test]
fn deepseek_models_are_described_consistently() {
    for model in deepseek::models() {
        assert_eq!(model.provider, ProviderId::DEEPSEEK);
        assert_eq!(model.base_url, deepseek::BASE_URL);
        assert!(model.reasoning);
        assert!(model.input.contains(InputModalities::TEXT));
        assert!(!model.input.contains(InputModalities::IMAGE));
        assert_eq!(model.context_window, deepseek::CONTEXT_WINDOW);
        assert_eq!(model.max_tokens, deepseek::MAX_OUTPUT_TOKENS);
        assert_eq!(
            model.compat.openai_completions(),
            Some(&OpenAiCompletionsCompat::deepseek())
        );
    }
    assert_eq!(deepseek::flash().id, "deepseek-v4-flash");
    assert_eq!(deepseek::pro().id, "deepseek-v4-pro");
    // Pro is the more expensive model.
    assert!(deepseek::pro().cost.rates.output > deepseek::flash().cost.rates.output);
}

#[test]
fn every_thinking_level_maps_onto_one_deepseek_effort() {
    let map = deepseek::thinking_levels();
    use ThinkingLevel::{High, Low, Max, Medium, Minimal, XHigh};

    // DeepSeek accepts only low / high / max.
    assert_eq!(map.resolve(Minimal.into()), Some("low"));
    assert_eq!(map.resolve(Low.into()), Some("low"));
    assert_eq!(map.resolve(Medium.into()), Some("high"));
    assert_eq!(map.resolve(High.into()), Some("high"));
    assert_eq!(map.resolve(XHigh.into()), Some("max"));
    assert_eq!(map.resolve(Max.into()), Some("max"));
    assert_eq!(map.resolve(ModelThinkingLevel::Off), Some("disabled"));

    for level in [Minimal, Low, Medium, High, XHigh, Max] {
        assert!(map.supports(level.into()), "{level:?} should be supported");
    }
}

#[test]
fn an_unsupported_level_resolves_to_nothing() {
    let map =
        ThinkingLevelMap::all_default().with(ThinkingLevel::Max.into(), LevelMapping::Unsupported);
    assert_eq!(map.resolve(ThinkingLevel::Max.into()), None);
    assert!(!map.supports(ThinkingLevel::Max.into()));
    // Untouched levels fall through to their canonical names.
    assert_eq!(map.resolve(ThinkingLevel::Medium.into()), Some("medium"));
}

#[test]
fn flat_pricing_bills_each_bucket_at_its_own_rate() {
    let cost = ModelCost::flat(1.0, 2.0, 0.1, 0.5);
    let usage = Usage {
        input: 1_000_000,
        output: 1_000_000,
        cache_read: 1_000_000,
        cache_write: 1_000_000,
        ..Usage::default()
    };
    let Cost {
        input,
        output,
        cache_read,
        cache_write,
        total,
    } = cost.cost_of(&usage);
    assert_eq!(
        (input, output, cache_read, cache_write),
        (1.0, 2.0, 0.1, 0.5)
    );
    assert!((total - 3.6).abs() < 1e-9);
}

#[test]
fn the_highest_matching_tier_prices_the_whole_request() {
    let cost = ModelCost {
        rates: ModelCostRates {
            input: 1.0,
            output: 1.0,
            cache_read: 0.0,
            cache_write: 0.0,
        },
        tiers: vec![
            ModelCostTier {
                input_tokens_above: 128_000,
                rates: ModelCostRates {
                    input: 2.0,
                    output: 2.0,
                    cache_read: 0.0,
                    cache_write: 0.0,
                },
            },
            ModelCostTier {
                input_tokens_above: 1_000_000,
                rates: ModelCostRates {
                    input: 4.0,
                    output: 4.0,
                    cache_read: 0.0,
                    cache_write: 0.0,
                },
            },
        ],
    };
    assert_eq!(cost.rates_for(1_000).input, 1.0);
    assert_eq!(cost.rates_for(128_000).input, 1.0, "threshold is exclusive");
    assert_eq!(cost.rates_for(128_001).input, 2.0);
    assert_eq!(cost.rates_for(2_000_000).input, 4.0);
}

#[test]
fn output_tokens_are_bounded_by_whichever_limit_binds_first() {
    let model = deepseek::flash();
    // Plenty of room: the model's own output cap binds.
    assert_eq!(
        model.available_output_tokens(1_000, 4_096),
        deepseek::MAX_OUTPUT_TOKENS
    );
    // Nearly full context: the remaining window binds.
    assert_eq!(
        model.available_output_tokens(deepseek::CONTEXT_WINDOW - 5_000, 1_000),
        4_000
    );
    // Over-full context cannot go negative.
    assert_eq!(
        model.available_output_tokens(deepseek::CONTEXT_WINDOW + 1, 0),
        0
    );
}

#[test]
fn thinking_budgets_fall_back_to_the_builtin_ladder() {
    let budgets = ThinkingBudgets::default();
    assert_eq!(budgets.resolve(ThinkingLevel::Minimal), 1024);
    assert_eq!(budgets.resolve(ThinkingLevel::Medium), 8192);
    // Levels above `high` share its budget.
    assert_eq!(
        budgets.resolve(ThinkingLevel::Max),
        budgets.resolve(ThinkingLevel::High)
    );

    let custom = ThinkingBudgets::new().with(ThinkingLevel::Medium, 4096);
    assert_eq!(custom.resolve(ThinkingLevel::Medium), 4096);
    assert_eq!(custom.resolve(ThinkingLevel::Low), 2048);
}

#[test]
fn usage_accumulates_without_inventing_unreported_fields() {
    let mut total = Usage {
        input: 10,
        output: 5,
        reasoning: None,
        ..Usage::default()
    };
    total += Usage {
        input: 3,
        output: 2,
        reasoning: None,
        ..Usage::default()
    };
    assert_eq!(total.input, 13);
    assert_eq!(
        total.reasoning, None,
        "a field no provider reported stays unreported"
    );

    total += Usage {
        reasoning: Some(7),
        ..Usage::default()
    };
    assert_eq!(total.reasoning, Some(7));
}

#[test]
fn compat_defaults_to_none_and_converts_from_a_profile() {
    assert!(Compat::default().openai_completions().is_none());
    let compat = Compat::from(OpenAiCompletionsCompat::deepseek());
    assert!(compat.openai_completions().is_some());
}

#[test]
fn the_data_layout_stays_dense() {
    // Mirrors the compile-time assertions in `layout`, so a regression shows up
    // as a readable test failure rather than only a build error.
    assert_eq!(size_of::<aphid_core::Span>(), 8);
    assert!(size_of::<aphid_core::Event>() <= 16);
}
