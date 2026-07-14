use super::*;
use crate::ui::commands::ExtensionCommandItem;
use crate::ui::commands::{
    build_extension_slash_commands, command_table, permission_choices, theme_choices,
    CausalDagStats, CompactionSettings, EffortChoice, ExtensionManagerItem, ModelChoice,
    PermissionPosture, ResumeItem,
};
use crate::ui::theme::ThemeChoice;
use euler_core::{ApprovalMode, ReasoningEffort};

fn code_swarm_picker_surface(selected: Vec<String>) -> BottomSurface {
    let mut surface = BottomSurface::new(CommandContext::default());
    let choices = vec![
        ModelChoice::new("openrouter", "z-ai/glm-5.2"),
        ModelChoice::new("anthropic", "claude-opus-5"),
        ModelChoice::new("openai", "gpt-5.5"),
        ModelChoice::new("mistral", "large-3"),
        ModelChoice::new("google", "gemini-3-pro"),
        ModelChoice::new("fixture", "echo"),
    ];
    surface.open_picker(PickerSpec::CodeSwarmModels {
        choices,
        selected,
        user_tier: false,
    });
    surface
}

fn code_swarm_checked(surface: &BottomSurface) -> usize {
    let BottomOwner::Picker(picker) = surface.owner() else {
        panic!("picker should own surface");
    };
    picker.items.iter().filter(|item| item.current).count()
}

fn causal_dag_surface() -> BottomSurface {
    let extension_items = vec![ExtensionManagerItem {
        id: "causal-dag".to_owned(),
        display_name: "Causal DAG".to_owned(),
        enabled: true,
        bundled: true,
        materialization: None,
        version: "0.2.0".to_owned(),
        commands: vec![
            ExtensionCommandItem::user("view"),
            ExtensionCommandItem::user("export"),
            ExtensionCommandItem::user("refresh"),
        ],
        capabilities: vec![],
        audit_status: None,
    }];
    let context = CommandContext {
        extension_slash_commands: build_extension_slash_commands(&extension_items),
        extension_items,
        causal_dag_stats: Some(CausalDagStats {
            session_id: "01KX8VEXAMPLE".to_owned(),
            node_count: 35,
            cross_arc_count: 7,
        }),
        ..CommandContext::default()
    };
    BottomSurface::new(context)
}

#[test]
fn causal_dag_picker_drills_into_formats_and_steps_back() {
    let mut surface = causal_dag_surface();
    surface.open_palette();
    surface.palette_insert("causal-dag");
    assert_eq!(surface.confirm(), SurfaceEvent::None);
    let actions = surface
        .surface_lines(100)
        .expect("action picker")
        .join("\n");
    assert!(actions.contains("CAUSAL DAG · session 01KX8V… · 35 nodes · 7 cross-arcs"));
    assert!(actions.contains("view      Show current graph"));
    assert!(actions.contains("refresh   Re-observe recent activity"));

    surface.move_selection_down();
    assert_eq!(surface.confirm(), SurfaceEvent::None);
    let formats = surface
        .surface_lines(100)
        .expect("format picker")
        .join("\n");
    assert!(formats.contains("CAUSAL DAG › EXPORT · 35 nodes"));
    assert!(formats.contains("html      Interactive viewer"));
    assert!(formats.contains("summary   Compact GRAPH: slot text"));
    assert!(formats.contains("⌫ back"));

    assert!(surface.picker_backspace_steps_back());
    assert!(matches!(
        surface.owner(),
        BottomOwner::Picker(picker) if picker.kind == PickerKind::CausalDagActions
    ));
    assert!(surface.picker_backspace_steps_back());
    assert!(matches!(surface.owner(), BottomOwner::Palette(_)));
}

#[test]
fn causal_dag_format_selection_runs_the_extension_with_selected_format() {
    let mut surface = causal_dag_surface();
    surface.open_picker(PickerSpec::CausalDagFormats(CausalDagStats {
        session_id: "session".to_owned(),
        node_count: 3,
        cross_arc_count: 1,
    }));
    assert_eq!(
        surface.confirm(),
        SurfaceEvent::Action(CommandAction::ExtensionRun {
            id: "causal-dag".to_owned(),
            command: "export".to_owned(),
            input: serde_json::json!({"format": "html"}),
            raw_args: None,
        })
    );
}

#[test]
fn palette_confirm_on_code_swarm_opens_config_not_extension_run() {
    // Review v2 §4: selecting /code-swarm from the palette must open the
    // reviewer-model config, never dispatch a "swarm" command to the host.
    let context = CommandContext {
        model_choices: vec![
            ModelChoice::new("openrouter", "z-ai/glm-5.2"),
            ModelChoice::new("anthropic", "claude-sonnet-5"),
        ],
        extension_items: vec![crate::ui::commands::ExtensionManagerItem {
            id: "code-swarm".to_owned(),
            display_name: "CodeSwarm Review".to_owned(),
            enabled: true,
            bundled: true,
            materialization: None,
            version: "0.1.0".to_owned(),
            commands: vec![
                ExtensionCommandItem::user("review-brief"),
                ExtensionCommandItem::user("review-report"),
            ],
            capabilities: vec![],
            audit_status: None,
        }],
        ..CommandContext::default()
    };
    let slash = crate::ui::commands::build_extension_slash_commands(&context.extension_items);
    let mut context = context;
    context.extension_slash_commands = slash;
    let mut surface = BottomSurface::new(context);
    surface.open_palette();
    surface.palette_insert("code-swarm");
    let event = surface.confirm();
    assert_eq!(event, SurfaceEvent::None, "should open a picker, not act");
    assert!(
        surface.is_code_swarm_picker(),
        "code-swarm config picker should own the surface"
    );

    // A real extension command still dispatches to the host.
    let mut surface2 = BottomSurface::new(surface.context.clone());
    surface2.open_palette();
    surface2.palette_insert("review-brief");
    match surface2.confirm() {
        SurfaceEvent::Action(CommandAction::ExtensionRun { id, command, .. }) => {
            assert_eq!(id, "code-swarm");
            assert_eq!(command, "review-brief");
        }
        other => panic!("expected extension run, got {other:?}"),
    }
}

#[test]
fn compaction_picker_shows_defaults_and_applies_independent_toggles() {
    let mut surface = BottomSurface::new(CommandContext {
        compaction: CompactionSettings {
            automatic: true,
            stubs: true,
        },
        ..CommandContext::default()
    });
    surface.open_palette();
    surface.palette_insert("compaction");
    assert_eq!(surface.confirm(), SurfaceEvent::None);

    let rendered = surface
        .surface_lines(100)
        .expect("compaction picker")
        .join("\n");
    assert!(rendered.contains("COMPACTION · automatic on · stubs on"));
    assert!(rendered.contains("[✓] automatic compaction"));
    assert!(rendered.contains("[✓] tool stubs"));
    assert!(rendered.contains("space toggle"));

    surface.move_selection_down();
    assert_eq!(surface.compaction_toggle(), Some(SurfaceEvent::None));
    surface.move_selection_down();
    assert_eq!(surface.compaction_toggle(), Some(SurfaceEvent::None));
    assert_eq!(
        surface.confirm(),
        SurfaceEvent::Action(CommandAction::SetCompactionPolicy {
            automatic: false,
            stubs: false,
        })
    );
}

#[test]
fn palette_confirm_on_extension_entry_keeps_typed_arguments() {
    // Review follow-up: selected extension entries dispatched only the bare
    // token, silently dropping anything typed after the command — the run
    // looked accepted while executing without its input.
    let context = CommandContext {
        extension_items: vec![crate::ui::commands::ExtensionManagerItem {
            id: "code-swarm".to_owned(),
            display_name: "CodeSwarm Review".to_owned(),
            enabled: true,
            bundled: true,
            materialization: None,
            version: "0.1.0".to_owned(),
            commands: vec![
                ExtensionCommandItem::user("review-brief"),
                ExtensionCommandItem::user("review-report"),
            ],
            capabilities: vec![],
            audit_status: None,
        }],
        ..CommandContext::default()
    };
    let slash = crate::ui::commands::build_extension_slash_commands(&context.extension_items);
    let mut context = context;
    context.extension_slash_commands = slash;
    let mut surface = BottomSurface::new(context);
    surface.open_palette();
    surface.palette_insert("review-brief {\"reviewers\":[\"tests\"]}");
    match surface.confirm() {
        SurfaceEvent::Action(CommandAction::ExtensionRun {
            id, command, input, ..
        }) => {
            assert_eq!(id, "code-swarm");
            assert_eq!(command, "review-brief");
            assert_eq!(input, serde_json::json!({"reviewers": ["tests"]}));
        }
        other => panic!("expected extension run with input, got {other:?}"),
    }
}

#[test]
fn palette_confirm_on_disabled_extension_returns_muted_notice_every_time() {
    // Review v2 §14.4: selecting a disabled extension command from the
    // palette must teach (not error), and must teach again on every
    // subsequent selection — no "only once per session" gating.
    let context = CommandContext {
        extension_items: vec![crate::ui::commands::ExtensionManagerItem {
            id: "code-swarm".to_owned(),
            display_name: "CodeSwarm Review".to_owned(),
            enabled: false,
            bundled: true,
            materialization: None,
            version: "0.1.0".to_owned(),
            commands: vec![
                ExtensionCommandItem::user("review-brief"),
                ExtensionCommandItem::user("review-report"),
            ],
            capabilities: vec![],
            audit_status: None,
        }],
        ..CommandContext::default()
    };
    let slash = crate::ui::commands::build_extension_slash_commands(&context.extension_items);
    let mut context = context;
    context.extension_slash_commands = slash;
    let mut surface = BottomSurface::new(context);
    let expected = SurfaceEvent::Notice(crate::ui::commands::disabled_extension_teach(
        "/code-swarm",
        "code-swarm",
    ));

    surface.open_palette();
    surface.palette_insert("code-swarm");
    assert_eq!(surface.confirm(), expected);

    // Invoke the same disabled command again: still teaches, not silenced.
    surface.open_palette();
    surface.palette_insert("code-swarm");
    assert_eq!(surface.confirm(), expected);
}

#[test]
fn code_swarm_picker_defaults_to_first_three_checked() {
    let surface = code_swarm_picker_surface(Vec::new());
    assert_eq!(code_swarm_checked(&surface), 3);
    let lines = {
        let BottomOwner::Picker(picker) = surface.owner() else {
            panic!("picker should own surface");
        };
        picker.render_lines(120).join("\n")
    };
    assert!(lines.contains("3 selected · 1–5"), "lines: {lines}");
    assert!(
        lines.contains("[x] openrouter::z-ai/glm-5.2"),
        "lines: {lines}"
    );
    assert!(lines.contains("[ ] mistral::large-3"), "lines: {lines}");
    assert!(lines.contains("min 1 · max 5"), "lines: {lines}");
}

#[test]
fn code_swarm_picker_restores_saved_selection() {
    let surface = code_swarm_picker_surface(vec![
        "openai::gpt-5.5".to_owned(),
        "google::gemini-3-pro".to_owned(),
    ]);
    assert_eq!(code_swarm_checked(&surface), 2);
}

#[test]
fn code_swarm_toggle_enforces_cap_and_confirm_enforces_min() {
    let mut surface = code_swarm_picker_surface(Vec::new());
    // Check rows 4 and 5 (3 defaults + 2 = cap).
    surface.move_selection_down();
    surface.move_selection_down();
    surface.move_selection_down();
    assert_eq!(surface.code_swarm_toggle(), Some(SurfaceEvent::None));
    surface.move_selection_down();
    assert_eq!(surface.code_swarm_toggle(), Some(SurfaceEvent::None));
    assert_eq!(code_swarm_checked(&surface), 5);
    // Sixth check is refused.
    surface.move_selection_down();
    assert!(matches!(
        surface.code_swarm_toggle(),
        Some(SurfaceEvent::Message(message)) if message.contains("5/5")
    ));
    assert_eq!(code_swarm_checked(&surface), 5);

    // Save collects exactly the checked set.
    match surface.confirm() {
        SurfaceEvent::Action(CommandAction::CodeSwarmSaveModels { models, user_tier }) => {
            assert!(!user_tier, "default save targets the project tier");
            assert_eq!(models.len(), 5);
            assert!(models.contains(&"mistral::large-3".to_owned()));
            assert!(!models.contains(&"fixture::echo".to_owned()));
        }
        other => panic!("expected save action, got {other:?}"),
    }

    // Unchecking everything and saving is refused (min 1).
    let mut empty = code_swarm_picker_surface(vec!["fixture::echo".to_owned()]);
    // fixture::echo is the last row; move there and uncheck it.
    for _ in 0..5 {
        empty.move_selection_down();
    }
    assert_eq!(empty.code_swarm_toggle(), Some(SurfaceEvent::None));
    assert_eq!(code_swarm_checked(&empty), 0);
    assert!(matches!(
        empty.confirm(),
        SurfaceEvent::Message(message) if message.contains("at least 1")
    ));
    // Picker stays open for correction.
    assert!(matches!(empty.owner(), BottomOwner::Picker(_)));
}

#[test]
fn palette_opens_filters_navigates_autocompletes_confirms_and_cancels() {
    let mut surface = BottomSurface::new(CommandContext::default());
    surface.open_palette();
    surface.palette_insert("mo");

    let BottomOwner::Palette(palette) = surface.owner() else {
        panic!("palette should own surface");
    };
    assert_eq!(palette.selected_token(), Some("/model".to_owned()));

    surface.move_selection_down();
    surface.move_selection_up();
    surface.autocomplete();
    let BottomOwner::Palette(palette) = surface.owner() else {
        panic!("palette should still own surface");
    };
    assert_eq!(palette.input(), "/model");

    assert_eq!(surface.confirm(), SurfaceEvent::None);
    assert!(matches!(surface.owner(), BottomOwner::Picker(_)));

    let mut cancel_surface = BottomSurface::new(CommandContext::default());
    cancel_surface.composer_mut().insert_text("draft");
    cancel_surface.open_palette();
    cancel_surface.palette_insert("help");
    assert_eq!(cancel_surface.cancel(), SurfaceEvent::None);
    assert_eq!(cancel_surface.composer().submit_text(), "draft");
}

#[test]
fn palette_backspace_corrects_extra_typed_characters() {
    let mut surface = BottomSurface::new(CommandContext::default());
    surface.open_palette();
    surface.palette_insert("eff//dddf");

    let BottomOwner::Palette(palette) = surface.owner() else {
        panic!("palette should own surface");
    };
    assert_eq!(palette.input(), "/eff//dddf");
    assert_eq!(palette.selected_token(), None);

    for _ in 0..6 {
        surface.palette_backspace();
    }

    let BottomOwner::Palette(palette) = surface.owner() else {
        panic!("palette should still own surface");
    };
    assert_eq!(palette.input(), "/eff");
    assert_eq!(palette.selected_token(), Some("/effort".to_owned()));
}

#[test]
fn palette_cursor_editing_keeps_slash_command_shape() {
    let mut surface = BottomSurface::new(CommandContext::default());
    surface.open_palette();
    surface.palette_insert("efort");
    for _ in 0..3 {
        surface.palette_move_left();
    }
    surface.palette_insert("f");

    let BottomOwner::Palette(palette) = surface.owner() else {
        panic!("palette should own surface");
    };
    assert_eq!(palette.input(), "/effort");
    assert_eq!(palette.cursor(), 4);
    assert_eq!(palette.selected_token(), Some("/effort".to_owned()));

    surface.palette_move_home();
    surface.palette_backspace();
    surface.palette_delete();

    let BottomOwner::Palette(palette) = surface.owner() else {
        panic!("palette should still own surface");
    };
    assert_eq!(palette.input(), "/ffort");
    assert_eq!(palette.cursor(), 1);
    assert!(palette.input().starts_with('/'));
}

#[test]
fn palette_confirm_activates_highlighted_command_token() {
    let mut bare = BottomSurface::new(CommandContext::default());
    bare.open_palette();
    assert_eq!(bare.confirm(), SurfaceEvent::None);
    assert!(matches!(bare.owner(), BottomOwner::Picker(_)));

    let mut prefix = BottomSurface::new(CommandContext::default());
    prefix.open_palette();
    prefix.palette_insert("cop");
    assert_eq!(
        prefix.confirm(),
        SurfaceEvent::Action(CommandAction::CopyLastAssistantResponse)
    );

    let mut with_arg = BottomSurface::new(CommandContext::default());
    with_arg.open_palette();
    with_arg.palette_insert("ef large");
    assert_eq!(
        with_arg.confirm(),
        SurfaceEvent::Action(CommandAction::SetReasoningEffort {
            effort: ReasoningEffort::Large,
        })
    );

    let mut unknown = BottomSurface::new(CommandContext::default());
    unknown.open_palette();
    unknown.palette_insert("zz arg");
    assert_eq!(
        unknown.confirm(),
        SurfaceEvent::Message("unknown command: /zz".to_owned())
    );
}

#[test]
fn model_picker_selects_switch_model_action() {
    let mut surface = BottomSurface::new(CommandContext {
        model_choices: vec![
            ModelChoice::current("fixture", "echo"),
            ModelChoice::new("openrouter", "glm-5.2"),
        ],
        ..CommandContext::default()
    });
    surface.open_palette();
    surface.palette_insert("model");
    assert_eq!(surface.confirm(), SurfaceEvent::None);
    let rendered = surface.surface_lines(80).expect("model picker").join("\n");
    assert!(rendered.contains("Select Model"));
    assert!(rendered.contains("→ fixture::echo ✓"));
    assert!(rendered.contains("  openrouter::glm-5.2"));
    assert!(rendered.contains("Filter: "));
    assert!(rendered.contains("(1/2)"));
    assert!(rendered.contains("Provider: fixture  Model: echo"));
    assert!(rendered.contains("Press enter to confirm or esc to go back"));

    surface.move_selection_down();

    assert_eq!(
        surface.confirm(),
        SurfaceEvent::Action(CommandAction::SwitchModel {
            provider: "openrouter".to_owned(),
            model: "glm-5.2".to_owned(),
        })
    );
    assert_eq!(surface.composer().submit_text(), "");
}

#[test]
fn model_picker_filters_by_provider_model_and_label() {
    let mut alias = ModelChoice::new("custom-provider", "model-a");
    alias.label = "Friendly Alias".to_owned();
    let mut surface = BottomSurface::new(CommandContext {
        model_choices: vec![
            ModelChoice::current("fixture", "echo"),
            ModelChoice::new("openrouter", "openai/gpt-4.1-mini"),
            ModelChoice::with_metadata("anthropic", "claude-sonnet", Some(1_000_000), Some(true)),
            alias,
        ],
        ..CommandContext::default()
    });
    surface.open_palette();
    surface.palette_insert("model");
    assert_eq!(surface.confirm(), SurfaceEvent::None);

    surface.palette_insert("openrouter gpt");
    let rendered = surface.surface_lines(80).expect("model picker").join("\n");
    assert!(rendered.contains("Filter: openrouter gpt"));
    assert!(rendered.contains("→ openrouter::openai/gpt-4.1-mini"));
    assert!(rendered.contains("Provider: openrouter  Model: openai/gpt-4.1-mini"));
    assert!(rendered.contains("(1/1)"));
    assert!(!rendered.contains("fixture::echo"));

    assert_eq!(
        surface.confirm(),
        SurfaceEvent::Action(CommandAction::SwitchModel {
            provider: "openrouter".to_owned(),
            model: "openai/gpt-4.1-mini".to_owned(),
        })
    );

    let mut alias_surface = BottomSurface::new(CommandContext {
        model_choices: vec![
            ModelChoice::new("fixture", "echo"),
            ModelChoice {
                provider: "custom-provider".to_owned(),
                model: "model-a".to_owned(),
                label: "Friendly Alias".to_owned(),
                current: false,
            },
        ],
        ..CommandContext::default()
    });
    alias_surface.open_palette();
    alias_surface.palette_insert("model");
    assert_eq!(alias_surface.confirm(), SurfaceEvent::None);
    alias_surface.palette_insert("friendly");
    let rendered = alias_surface
        .surface_lines(80)
        .expect("model picker")
        .join("\n");
    assert!(rendered.contains("Filter: friendly"));
    assert!(rendered.contains("→ Friendly Alias"));
    assert!(rendered.contains("Provider: custom-provider  Model: model-a"));
    assert!(!rendered.contains("fixture::echo"));

    let mut value_surface = BottomSurface::new(CommandContext {
        model_choices: vec![ModelChoice::with_metadata(
            "anthropic",
            "claude-sonnet-5",
            Some(1_000_000),
            Some(true),
        )],
        ..CommandContext::default()
    });
    value_surface.open_palette();
    value_surface.palette_insert("model");
    assert_eq!(value_surface.confirm(), SurfaceEvent::None);
    value_surface.palette_insert("anthropic:: sonnet");
    let rendered = value_surface
        .surface_lines(80)
        .expect("model picker")
        .join("\n");
    assert!(rendered.contains("→ anthropic::claude-sonnet-5 — 1M ctx, reasoning"));

    let mut metadata_surface = BottomSurface::new(CommandContext {
        model_choices: vec![ModelChoice::with_metadata(
            "anthropic",
            "claude-sonnet-5",
            Some(1_000_000),
            Some(true),
        )],
        ..CommandContext::default()
    });
    metadata_surface.open_palette();
    metadata_surface.palette_insert("model");
    assert_eq!(metadata_surface.confirm(), SurfaceEvent::None);
    metadata_surface.palette_insert("reasoning");
    let rendered = metadata_surface
        .surface_lines(80)
        .expect("model picker")
        .join("\n");
    assert!(rendered.contains("No matching models"));
}

#[test]
fn model_picker_no_match_stays_open() {
    let mut surface = BottomSurface::new(CommandContext {
        model_choices: vec![ModelChoice::current("fixture", "echo")],
        ..CommandContext::default()
    });
    surface.open_palette();
    surface.palette_insert("model");
    assert_eq!(surface.confirm(), SurfaceEvent::None);

    surface.palette_insert("missing");
    let rendered = surface.surface_lines(80).expect("model picker").join("\n");
    assert!(rendered.contains("Filter: missing"));
    assert!(rendered.contains("No matching models"));
    assert!(rendered.contains("(0/0)"));
    assert_eq!(surface.confirm(), SurfaceEvent::None);
    assert!(matches!(surface.owner(), BottomOwner::Picker(_)));
}

#[test]
fn model_picker_query_backspace_delete_and_navigation_are_bounded() {
    let mut surface = BottomSurface::new(CommandContext {
        model_choices: vec![
            ModelChoice::new("openrouter", "openai/gpt-4.1-mini"),
            ModelChoice::new("openrouter", "z-ai/glm-5.2"),
            ModelChoice::new("anthropic", "claude-sonnet"),
        ],
        ..CommandContext::default()
    });
    surface.set_picker_visible_rows(1);
    surface.open_palette();
    surface.palette_insert("model");
    assert_eq!(surface.confirm(), SurfaceEvent::None);

    surface.palette_insert("openrouter");
    surface.move_selection_down();
    let BottomOwner::Picker(picker) = surface.owner() else {
        panic!("model picker should own surface");
    };
    assert_eq!(picker.position_indicator(), "(2/2)");
    assert_eq!(picker.visible_rows(80).len(), 1);

    surface.palette_backspace();
    let rendered = surface.surface_lines(80).expect("model picker").join("\n");
    assert!(rendered.contains("Filter: openroute"));
    assert!(rendered.contains("(1/2)"));

    surface.palette_delete();
    let rendered = surface.surface_lines(80).expect("model picker").join("\n");
    assert!(rendered.contains("Filter: "));
    assert!(rendered.contains("(1/3)"));
}

#[test]
fn effort_and_theme_pickers_mark_current_choice() {
    let mut effort = BottomSurface::new(CommandContext {
        effort_choices: ReasoningEffort::ALL
            .into_iter()
            .map(|choice| EffortChoice::new(choice, ReasoningEffort::Medium))
            .collect(),
        ..CommandContext::default()
    });
    effort.open_palette();
    effort.palette_insert("effort");
    assert_eq!(effort.confirm(), SurfaceEvent::None);
    let rendered = effort.surface_lines(80).expect("effort picker").join("\n");
    assert!(rendered.contains("Reasoning Effort"));
    assert!(rendered.contains("medium - balanced default"));
    assert!(rendered.contains("current"));

    effort.move_selection_down();
    assert_eq!(
        effort.confirm(),
        SurfaceEvent::Action(CommandAction::SetReasoningEffort {
            effort: ReasoningEffort::Small,
        })
    );

    let mut theme = BottomSurface::new(CommandContext {
        theme_choices: theme_choices(ThemeChoice::GruvboxLight),
        ..CommandContext::default()
    });
    theme.open_palette();
    theme.palette_insert("theme");
    assert_eq!(theme.confirm(), SurfaceEvent::None);
    let rendered = theme.surface_lines(80).expect("theme picker").join("\n");
    assert!(rendered.contains("Theme"));
    assert!(rendered.contains("Gruvbox Light"));
    assert!(rendered.contains("current"));
}

#[test]
fn inline_model_command_returns_action_without_picker() {
    let mut surface = BottomSurface::new(CommandContext::default());
    surface.open_palette();
    surface.palette_insert("model openrouter::openai/gpt-4.1-mini");

    assert_eq!(
        surface.confirm(),
        SurfaceEvent::Action(CommandAction::SwitchModel {
            provider: "openrouter".to_owned(),
            model: "openai/gpt-4.1-mini".to_owned(),
        })
    );
    assert!(matches!(surface.owner(), BottomOwner::Composer));

    let mut first_slash = BottomSurface::new(CommandContext::default());
    first_slash.open_palette();
    first_slash.palette_insert("model openrouter/openai/gpt-4.1-mini");

    assert_eq!(
        first_slash.confirm(),
        SurfaceEvent::Action(CommandAction::SwitchModel {
            provider: "openrouter".to_owned(),
            model: "openai/gpt-4.1-mini".to_owned(),
        })
    );
}

#[test]
fn permissions_palette_opens_via_action() {
    let mut surface = BottomSurface::new(CommandContext::default());
    surface.open_palette();
    surface.palette_insert("permissions");
    assert_eq!(
        surface.confirm(),
        SurfaceEvent::Action(CommandAction::OpenPermissions)
    );
}

#[test]
fn permissions_picker_leads_with_honest_session_postures() {
    let mut surface = BottomSurface::new(CommandContext::default());
    surface.open_picker(PickerSpec::Permissions(permission_choices()));
    let rendered = surface
        .surface_lines(80)
        .expect("permissions picker")
        .join("\n");
    assert!(rendered.contains("Permissions (1/40)"));
    assert!(rendered.contains("Quick settings - Read only"));
    assert!(rendered.contains("Full access (unsandboxed)"));
    assert!(rendered.contains("Auto in workspace sandbox (not available)"));
    assert!(!rendered.contains('%'));

    assert_eq!(
        surface.confirm(),
        SurfaceEvent::Action(CommandAction::SetPermissionPosture {
            posture: PermissionPosture::ReadOnly,
        })
    );
}

#[test]
fn permissions_picker_keeps_per_capability_controls_under_advanced() {
    let mut surface = BottomSurface::new(CommandContext::default());
    surface.open_picker(PickerSpec::Permissions(permission_choices()));
    for _ in 0..8 {
        surface.move_selection_down();
    }

    let rendered = surface
        .surface_lines(80)
        .expect("permissions picker")
        .join("\n");
    assert!(rendered.contains("Advanced · Files: write - Allow file writes this session"));
    assert_eq!(
        surface.confirm(),
        SurfaceEvent::Action(CommandAction::SetPermissionMode {
            capability: Capability::FsWrite,
            mode: ApprovalMode::SessionAllow,
        })
    );
}

#[test]
fn permissions_picker_marks_sandbox_posture_unavailable_instead_of_faking_it() {
    let mut surface = BottomSurface::new(CommandContext::default());
    surface.open_picker(PickerSpec::Permissions(permission_choices()));
    for _ in 0..3 {
        surface.move_selection_down();
    }

    assert_eq!(
        surface.confirm(),
        SurfaceEvent::Action(CommandAction::PermissionSandboxUnavailable)
    );
}

#[test]
fn permissions_picker_exposes_agent_spawn_controls() {
    let mut surface = BottomSurface::new(CommandContext::default());
    surface.open_picker(PickerSpec::Permissions(permission_choices()));
    for _ in 0..22 {
        surface.move_selection_down();
    }

    let rendered = surface
        .surface_lines(80)
        .expect("permissions picker")
        .join("\n");
    assert!(rendered.contains("Advanced · Agents - Ask before spawning agents"));
    assert_eq!(
        surface.confirm(),
        SurfaceEvent::Action(CommandAction::SetPermissionMode {
            capability: Capability::AgentSpawn,
            mode: ApprovalMode::Ask,
        })
    );
}

#[test]
fn resume_picker_is_list_mode_with_indicator_and_action() {
    let mut first = ResumeItem::new("s1", "2026-06-19 research");
    first.status = Some("4m ago".to_owned());
    first.preview = Some("s1  /repo".to_owned());
    first.group = Some("tui".to_owned());
    let context = CommandContext {
        resume_items: vec![first, ResumeItem::new("s2", "2026-06-18 coding")],
        ..CommandContext::default()
    };
    let mut surface = BottomSurface::new(context);
    surface.set_picker_visible_rows(1);
    surface.open_palette();
    surface.palette_insert("resume");
    assert_eq!(surface.confirm(), SurfaceEvent::None);

    let BottomOwner::Picker(picker) = surface.owner() else {
        panic!("picker should own surface");
    };
    assert_eq!(picker.position_indicator(), "(1/2)");
    assert_eq!(picker.visible_rows(80).len(), 1);
    let rendered = surface.surface_lines(80).expect("resume picker").join("\n");
    assert!(rendered.contains("Resume a previous session"));
    assert!(rendered.contains("Type to search"));
    assert!(rendered.contains("4m ago"));
    assert!(rendered.contains("2026-06-19 research"));
    assert!(rendered.contains("tui"));
    // Selected-row preview (id + root), not a footer "Session:" detail.
    assert!(rendered.contains("s1  /repo"));
    assert!(!rendered.contains("Session:"));
    assert!(rendered.contains("newest first"));
    assert!(rendered.contains("ctrl+o preview"));
    assert!(!rendered.contains("Type: [All]"));

    surface.move_selection_down();
    let BottomOwner::Picker(picker) = surface.owner() else {
        panic!("picker should still own surface");
    };
    assert_eq!(picker.position_indicator(), "(2/2)");
    assert_eq!(
        surface.confirm(),
        SurfaceEvent::Action(CommandAction::ResumeSession {
            session_id: "s2".to_owned(),
        })
    );
}

#[test]
fn resume_picker_searches_label_id_and_root_path() {
    let mut first = ResumeItem::new("s1", "backend cleanup");
    first.status = Some("2h ago".to_owned());
    first.group = Some("tui".to_owned());
    let mut second = ResumeItem::new("s2", "token budget review");
    second.preview = Some("01TOKEN  /repo".to_owned());
    second.group = Some("exec".to_owned());
    let context = CommandContext {
        resume_items: vec![first, second],
        ..CommandContext::default()
    };
    let mut surface = BottomSurface::new(context);
    surface.open_palette();
    surface.palette_insert("resume");
    assert_eq!(surface.confirm(), SurfaceEvent::None);

    // Filter is label/id/root only — group label "exec" is not a match key.
    surface.palette_insert("token /repo");
    let rendered = surface.surface_lines(80).expect("resume picker").join("\n");

    assert!(rendered.contains("Search: token /repo"));
    assert!(rendered.contains("token budget review"));
    assert!(!rendered.contains("backend cleanup"));
    assert_eq!(
        surface.confirm(),
        SurfaceEvent::Action(CommandAction::ResumeSession {
            session_id: "s2".to_owned(),
        })
    );
}

#[test]
fn resume_picker_accepts_ledger_tail_preview() {
    let mut first = ResumeItem::new("s1", "preview me");
    first.status = Some("just now".to_owned());
    first.group = Some("tui".to_owned());
    let context = CommandContext {
        resume_items: vec![first],
        ..CommandContext::default()
    };
    let mut surface = BottomSurface::new(context);
    surface.open_palette();
    surface.palette_insert("resume");
    assert_eq!(surface.confirm(), SurfaceEvent::None);
    assert_eq!(
        surface.resume_picker_selected_session_id().as_deref(),
        Some("s1")
    );

    surface.set_resume_ledger_preview(vec![
        "user: hello".to_owned(),
        "assistant: world".to_owned(),
    ]);
    let rendered = surface.surface_lines(80).expect("resume picker").join("\n");
    assert!(rendered.contains("ledger tail (read-only)"));
    assert!(rendered.contains("user: hello"));
    assert!(rendered.contains("assistant: world"));
}

#[test]
fn name_effort_new_and_help_actions_are_palette_actions() {
    let mut effort = BottomSurface::new(CommandContext::default());
    effort.open_palette();
    effort.palette_insert("effort xlarge");
    assert_eq!(
        effort.confirm(),
        SurfaceEvent::Action(CommandAction::SetReasoningEffort {
            effort: ReasoningEffort::XLarge,
        })
    );

    let mut name = BottomSurface::new(CommandContext::default());
    name.open_palette();
    name.palette_insert("name demo");
    assert_eq!(
        name.confirm(),
        SurfaceEvent::Action(CommandAction::NameSession {
            name: "demo".to_owned(),
        })
    );

    let mut new_session = BottomSurface::new(CommandContext::default());
    new_session.open_palette();
    new_session.palette_insert("new");
    assert_eq!(
        new_session.confirm(),
        SurfaceEvent::Action(CommandAction::NewSession)
    );

    let mut help = BottomSurface::new(CommandContext::default());
    help.open_palette();
    help.palette_insert("help");
    let SurfaceEvent::Action(CommandAction::ShowHelp { text }) = help.confirm() else {
        panic!("help should return command table text");
    };
    assert!(text.contains("/model [provider::model]"));
    assert!(text.contains("/quit"));
}

#[test]
fn picker_cancel_restores_exact_paste_token_draft() {
    let payload = (1..=11)
        .map(|line| format!("line{line}"))
        .collect::<Vec<_>>()
        .join("\n");
    let mut surface = BottomSurface::new(CommandContext {
        model_choices: vec![ModelChoice::new("fixture", "echo")],
        ..CommandContext::default()
    });
    surface.composer_mut().insert_text("before ");
    surface.composer_mut().insert_bracketed_paste(&payload);
    surface.composer_mut().insert_text(" after");
    let original = surface.composer().clone();

    surface.open_palette();
    surface.palette_insert("model");
    assert_eq!(surface.confirm(), SurfaceEvent::None);
    assert_eq!(surface.cancel(), SurfaceEvent::None);

    assert_eq!(surface.composer(), &original);
    assert_eq!(
        surface.composer().submit_text(),
        format!("before {payload} after")
    );
}

#[test]
fn palette_cancel_restores_exact_paste_token_draft() {
    let payload = "x".repeat(1_001);
    let mut surface = BottomSurface::new(CommandContext::default());
    surface.composer_mut().insert_bracketed_paste(&payload);
    let original = surface.composer().clone();

    surface.open_palette();
    surface.palette_insert("help");
    assert_eq!(surface.cancel(), SurfaceEvent::None);

    assert_eq!(surface.composer(), &original);
    assert_eq!(surface.composer().submit_text(), payload);
}

#[test]
fn command_table_has_no_exit_alias() {
    assert!(!command_table().iter().any(|spec| spec.token == "/exit"));
}

#[test]
fn palette_render_keeps_selected_command_visible() {
    let mut surface = BottomSurface::new(CommandContext::default());
    surface.open_palette();
    for _ in 0..6 {
        surface.move_selection_down();
    }

    let BottomOwner::Palette(palette) = surface.owner() else {
        panic!("palette should own surface");
    };
    let selected = palette.selected_token().expect("selected command");
    let rendered_lines = palette.render_lines(80);
    let rendered = rendered_lines.join("\n");

    assert!(rendered_lines.iter().all(|line| line.chars().count() <= 80));
    assert_eq!(usize::from(palette.line_count()), rendered_lines.len());
    assert!(rendered_lines[0].starts_with(PALETTE_QUERY_PREFIX));
    assert!(rendered.contains(&format!("> {selected}")));
    assert!(rendered.contains(&format!(
        "({}/{})",
        palette.selected.saturating_add(1),
        command_table().len()
    )));
}

/// Issue #23: 8 visible rows (raised from a prior 4).
#[test]
fn palette_shows_up_to_eight_match_rows() {
    let mut surface = BottomSurface::new(CommandContext::default());
    surface.open_palette();

    let BottomOwner::Palette(palette) = surface.owner() else {
        panic!("palette should own surface");
    };
    assert!(
        palette.matches().len() > 8,
        "fixture command table should exceed one page for this test to be meaningful"
    );
    let rendered = palette.render_lines(80);
    // query row + 8 match rows + position/hint row.
    assert_eq!(rendered.len(), 10, "rendered: {rendered:?}");
}

/// Issue #23: backspacing over the leading `/` with nothing else typed
/// exits the palette (checked at the `BottomSurface` level the app's key
/// handler consults before calling `palette_backspace`).
#[test]
fn palette_backspace_would_exit_only_at_bare_leading_slash() {
    let mut surface = BottomSurface::new(CommandContext::default());
    surface.open_palette();
    assert!(surface.palette_backspace_would_exit());

    surface.palette_insert("mo");
    assert!(!surface.palette_backspace_would_exit());

    surface.palette_backspace();
    surface.palette_backspace();
    assert!(surface.palette_backspace_would_exit());
}

/// Issue #23: the selected row is a full-width select-bar (selection token
/// background) with gold (warning-token) text, routed through `Theme`
/// rather than a hardcoded hex.
#[test]
fn palette_selected_row_uses_full_width_select_bar_and_warning_text() {
    let mut surface = BottomSurface::new(CommandContext::default());
    surface.open_palette();
    let theme = Theme::warm_ledger();
    let width = 40u16;

    let lines = surface
        .surface_canvas_lines(&theme, width)
        .expect("palette lines");
    let selected_line = &lines[1]; // query row, then first match row (selected).
    assert_eq!(selected_line.spans.len(), 1);
    let span = &selected_line.spans[0];
    assert_eq!(span.style.fg, Some(theme.palette.warning));
    assert_eq!(span.style.bg, Some(theme.palette.selection));
    assert_eq!(
        crate::ui::text::display_width(span.text.as_str()),
        usize::from(width),
        "select bar must span the full row width"
    );
}

/// Issue #24: the `/code-swarm` checklist reuses the palette's select-bar
/// styling on its highlighted row.
#[test]
fn code_swarm_picker_selected_row_uses_same_select_bar_styling() {
    let surface = code_swarm_picker_surface(Vec::new());
    let theme = Theme::warm_ledger();
    let width = 40u16;

    let lines = surface
        .surface_canvas_lines(&theme, width)
        .expect("picker lines");
    let selected_line = &lines[1]; // title row, then first checklist row (selected).
    assert_eq!(selected_line.spans.len(), 1);
    let span = &selected_line.spans[0];
    assert_eq!(span.style.fg, Some(theme.palette.warning));
    assert_eq!(span.style.bg, Some(theme.palette.selection));
}

/// Issue #24: `⌫` steps back to the slash palette when the code-swarm
/// picker's type-to-filter query is empty, restoring the composer draft
/// that was present before `/` was originally typed.
#[test]
fn code_swarm_backspace_steps_back_to_palette_when_filter_is_empty() {
    // Same path the app takes: composer -> palette -> code-swarm picker, so
    // `saved_draft` threads through the picker back to the palette.
    let mut surface = BottomSurface::new(CommandContext::default());
    surface.edit_composer(|draft| draft.insert_text("draft before slash"));
    surface.open_palette();
    let saved = surface.composer().clone();
    surface.open_picker(PickerSpec::CodeSwarmModels {
        choices: vec![ModelChoice::new("fixture", "echo")],
        selected: Vec::new(),
        user_tier: false,
    });

    assert!(surface.picker_backspace_steps_back());
    assert!(matches!(surface.owner(), BottomOwner::Palette(_)));
    assert_eq!(surface.composer(), &saved);
}

#[test]
fn code_swarm_backspace_does_not_step_back_while_filter_has_text() {
    let mut surface = code_swarm_picker_surface(Vec::new());
    surface.palette_insert("gl");
    assert!(!surface.picker_backspace_steps_back());
    assert!(matches!(surface.owner(), BottomOwner::Picker(_)));
}

/// Issue #23: the typed `/` (and the rest of the query) stays green
/// throughout, independent of the selected row's styling below it.
#[test]
fn palette_query_row_keeps_the_slash_green() {
    let mut surface = BottomSurface::new(CommandContext::default());
    surface.open_palette();
    surface.palette_insert("mo");
    let theme = Theme::warm_ledger();

    let lines = surface
        .surface_canvas_lines(&theme, 40)
        .expect("palette lines");
    let query_line = &lines[0];
    let slash_span = query_line
        .spans
        .iter()
        .find(|span| span.text.as_str().contains('/'))
        .expect("query span carrying the slash");
    assert_eq!(slash_span.style.fg, Some(theme.palette.added));
}

#[test]
fn palette_line_count_matches_rendered_rows_at_boundaries() {
    let mut no_match = BottomSurface::new(CommandContext::default());
    no_match.open_palette();
    no_match.palette_insert("zz");
    let BottomOwner::Palette(palette) = no_match.owner() else {
        panic!("palette should own surface");
    };
    assert_eq!(
        usize::from(palette.line_count()),
        palette.render_lines(80).len()
    );

    let mut one_match = BottomSurface::new(CommandContext::default());
    one_match.open_palette();
    one_match.palette_insert("mo");
    let BottomOwner::Palette(palette) = one_match.owner() else {
        panic!("palette should own surface");
    };
    assert_eq!(
        usize::from(palette.line_count()),
        palette.render_lines(80).len()
    );

    let mut out_of_range = palette.clone();
    out_of_range.selected = 5;
    assert_eq!(
        usize::from(out_of_range.line_count()),
        out_of_range.render_lines(80).len()
    );
}

#[test]
fn palette_reports_cursor_inside_query_row() {
    let mut surface = BottomSurface::new(CommandContext::default());
    surface.open_palette();
    surface.palette_insert("model");

    assert_eq!(
        surface.surface_cursor(80),
        Some((0, u16::try_from(display_width("\u{258c} /model")).unwrap()))
    );
    assert_eq!(surface.surface_cursor(0), None);
    assert_eq!(surface.surface_cursor(1), Some((0, 0)));
    assert_eq!(surface.surface_cursor(4), Some((0, 3)));
}

#[test]
fn palette_cursor_uses_display_width_for_wide_input() {
    let mut surface = BottomSurface::new(CommandContext::default());
    surface.open_palette();
    surface.palette_insert("界");

    assert_eq!(
        surface.surface_cursor(80),
        Some((0, u16::try_from(display_width("\u{258c} /界")).unwrap()))
    );
    assert_eq!(surface.surface_cursor(5), Some((0, 4)));
    assert_eq!(surface.surface_cursor(4), Some((0, 3)));
    assert_eq!(surface.surface_cursor(3), Some((0, 2)));
}

#[test]
fn only_palette_reports_bottom_surface_cursor() {
    let mut surface = BottomSurface::new(CommandContext::default());
    assert_eq!(surface.surface_cursor(80), None);

    surface.open_palette();
    assert!(surface.surface_cursor(80).is_some());
    assert_eq!(surface.confirm(), SurfaceEvent::None);
    assert_eq!(surface.surface_cursor(80), None);
}
