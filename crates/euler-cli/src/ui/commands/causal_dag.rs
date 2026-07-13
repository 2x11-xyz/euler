use super::*;

const USAGE: &str = "usage: /causal-dag [view | export [--format html|json|svg|dot|markdown|summary] [--out path] | refresh [--operation incremental|reframe|final]]";

pub(super) fn effect(arg: Option<&str>, context: &CommandContext) -> CommandEffect {
    let enabled = context
        .extension_items
        .iter()
        .find(|item| item.id == "causal-dag")
        .is_some_and(|item| item.enabled);
    if !enabled {
        return CommandEffect::Notice(disabled_extension_teach("/causal-dag", "causal-dag"));
    }
    let stats = context.causal_dag_stats.clone().unwrap_or(CausalDagStats {
        session_id: "unknown".to_owned(),
        node_count: 0,
        cross_arc_count: 0,
    });
    let Some(arg) = arg.map(str::trim).filter(|arg| !arg.is_empty()) else {
        return CommandEffect::OpenPicker(PickerSpec::CausalDagActions(stats));
    };
    let (command, rest) =
        arg.split_once(char::is_whitespace)
            .map_or((arg, None), |(command, rest)| {
                (
                    command,
                    Some(rest.trim_start()).filter(|rest| !rest.is_empty()),
                )
            });
    match command {
        "view" if rest.is_none() => run("view", None),
        "export" if rest.is_none() => {
            CommandEffect::OpenPicker(PickerSpec::CausalDagFormats(stats))
        }
        "export" => run("export", rest),
        "refresh" => run("refresh", rest),
        _ => CommandEffect::Message(USAGE.to_owned()),
    }
}

fn run(command: &str, arg: Option<&str>) -> CommandEffect {
    match extension_argument_values(arg, "/causal-dag") {
        Ok((input, raw_args)) => CommandEffect::Action(CommandAction::ExtensionRun {
            id: "causal-dag".to_owned(),
            command: command.to_owned(),
            input,
            raw_args,
        }),
        Err(message) => CommandEffect::Message(message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn causal_dag_gets_one_extension_owned_surface() {
        let context = context(true);
        let commands = build_extension_slash_commands(&context.extension_items);
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].token, "/causal-dag");
        assert_eq!(commands[0].command, "surface");
        assert!(!commands.iter().any(|command| command.token == "/export"));
    }

    #[test]
    fn routes_picker_and_flagged_commands() {
        let context = context(true);
        assert!(matches!(
            dispatch_command("/causal-dag", &context),
            CommandEffect::OpenPicker(PickerSpec::CausalDagActions(CausalDagStats {
                node_count: 35,
                cross_arc_count: 7,
                ..
            }))
        ));
        assert!(matches!(
            dispatch_command("/causal-dag export", &context),
            CommandEffect::OpenPicker(PickerSpec::CausalDagFormats(_))
        ));
        assert_eq!(
            dispatch_command(
                "/causal-dag export --format html --out reports/dag.html",
                &context
            ),
            CommandEffect::Action(CommandAction::ExtensionRun {
                id: "causal-dag".to_owned(),
                command: "export".to_owned(),
                input: serde_json::json!({}),
                raw_args: Some("--format html --out reports/dag.html".to_owned()),
            })
        );
        assert!(matches!(
            dispatch_command("/causal-dag refresh --operation reframe", &context),
            CommandEffect::Action(CommandAction::ExtensionRun { command, .. })
                if command == "refresh"
        ));
    }

    #[test]
    fn disabled_surface_teaches_instead_of_opening() {
        assert_eq!(
            dispatch_command("/causal-dag", &context(false)),
            CommandEffect::Notice(disabled_extension_teach("/causal-dag", "causal-dag"))
        );
    }

    fn context(enabled: bool) -> CommandContext {
        let extension_items = vec![ExtensionManagerItem {
            id: "causal-dag".to_owned(),
            display_name: "Causal DAG".to_owned(),
            enabled,
            bundled: true,
            materialization: None,
            version: "0.2.0".to_owned(),
            commands: vec!["view".to_owned(), "export".to_owned(), "refresh".to_owned()],
            capabilities: vec![],
            audit_status: None,
        }];
        CommandContext {
            extension_slash_commands: build_extension_slash_commands(&extension_items),
            extension_items,
            causal_dag_stats: Some(CausalDagStats {
                session_id: "01KX8VEXAMPLE".to_owned(),
                node_count: 35,
                cross_arc_count: 7,
            }),
            ..CommandContext::default()
        }
    }
}
