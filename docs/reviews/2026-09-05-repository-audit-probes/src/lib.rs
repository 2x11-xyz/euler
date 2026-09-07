//! Synthetic, private audit probes against unmodified Euler source.
//! Assertions document observed defects, not desired regression behavior.

#[cfg(test)]
mod tests {
    use euler_core::command_safety::is_statically_safe_command;
    use euler_core::{
        assemble_canvas, read_provenance, ApprovalMode, AutoCompactionPolicy, CanvasItem,
        DeciderVerdict, PermissionDecider, PermissionRequest, ProvenanceWriter, Session,
        SessionConfig, ToolRegistry,
    };
    use euler_provider::{FixtureResponse, ProviderSet, ScriptedProvider, ToolCall};
    use euler_sdk::Capability;
    use serde_json::json;
    use std::{
        fs,
        process::Command,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        },
    };

    #[derive(Default)]
    struct Deny(Arc<AtomicUsize>);
    impl PermissionDecider for Deny {
        fn decide(&mut self, _: &PermissionRequest) -> DeciderVerdict {
            self.0.fetch_add(1, Ordering::Relaxed);
            DeciderVerdict::Deny
        }
    }

    fn call(id: &str, name: &str, input: serde_json::Value) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: name.into(),
            input,
        }
    }

    #[test]
    fn reproduces_uniq_write_without_approval() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("input.txt"), "a\na\nb\n").unwrap();
        fs::write(tmp.path().join("output.txt"), "user-owned original\n").unwrap();
        let decider = Deny::default();
        let asks = decider.0.clone();
        let provider = ScriptedProvider::new(vec![
            FixtureResponse::ToolCalls(vec![call(
                "c1",
                "run_shell",
                json!({"command":"uniq input.txt output.txt"}),
            )]),
            FixtureResponse::Assistant("finished".into()),
        ]);
        let mut session = Session::new(SessionConfig::new(tmp.path()), provider, decider);
        session.set_permission_mode(Capability::FsWrite, ApprovalMode::AlwaysDeny);
        session.set_permission_mode(Capability::ShellExec, ApprovalMode::Ask);
        session.run_turn("audit fixture").unwrap();
        assert_eq!(asks.load(Ordering::Relaxed), 0);
        assert_eq!(
            fs::read_to_string(tmp.path().join("output.txt")).unwrap(),
            "a\nb\n"
        );
        assert!(session
            .events()
            .iter()
            .any(|e| e.payload.get("mode") == Some(&json!("static-safe"))));
        println!("CONFIRMED: uniq overwrote output.txt with FsWrite=AlwaysDeny and ShellExec=Ask; decider calls=0");
    }

    #[test]
    fn reproduces_glob_and_cd_read_scope_bypass() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("workspace");
        fs::create_dir_all(root.join("nested")).unwrap();
        let outside = tmp.path().join("outside.txt");
        fs::write(&outside, "SYNTHETIC_OUTSIDE_MARKER\n").unwrap();
        std::os::unix::fs::symlink(&outside, root.join("public.txt")).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("nested/view.txt")).unwrap();
        fs::write(root.join("view.txt"), "inside\n").unwrap();
        assert!(!is_statically_safe_command("cat public.txt", &root));
        for command in [
            "cat *.txt",
            "cd nested && cat view.txt",
            "rg --follow SYNTHETIC .",
        ] {
            assert!(is_statically_safe_command(command, &root), "{command}");
            let output = Command::new("sh")
                .args(["-c", command])
                .current_dir(&root)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{command}: status {:?}, stderr {}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(String::from_utf8_lossy(&output.stdout).contains("SYNTHETIC_OUTSIDE_MARKER"));
            println!(
                "CONFIRMED: statically approved {command:?} read synthetic file outside workspace"
            );
        }
    }

    #[test]
    fn reproduces_prepared_create_overwriting_intervening_file() {
        let tmp = tempfile::tempdir().unwrap();
        let tools = ToolRegistry::new(tmp.path());
        let prepared = tools
            .execute(
                "write_file",
                &json!({"path":"new.txt", "content":"agent\n"}),
            )
            .unwrap();
        fs::write(tmp.path().join("new.txt"), "intervening user creation\n").unwrap();
        tools.apply_patch(prepared.patch.as_ref().unwrap()).unwrap();
        assert_eq!(
            fs::read_to_string(tmp.path().join("new.txt")).unwrap(),
            "agent\n"
        );
        println!(
            "CONFIRMED: create-only write overwrites a file created between prepare and apply"
        );
    }

    #[test]
    fn reproduces_prepared_edit_losing_intervening_change() {
        let tmp = tempfile::tempdir().unwrap();
        let tools = ToolRegistry::new(tmp.path());
        fs::write(tmp.path().join("existing.txt"), "before\n").unwrap();
        let prepared = tools
            .execute(
                "edit_file",
                &json!({"path":"existing.txt", "old":"before", "new":"after"}),
            )
            .unwrap();
        fs::write(tmp.path().join("existing.txt"), "intervening user edit\n").unwrap();
        tools.apply_patch(prepared.patch.as_ref().unwrap()).unwrap();
        assert_eq!(
            fs::read_to_string(tmp.path().join("existing.txt")).unwrap(),
            "after\n"
        );
        println!("CONFIRMED: prepared edit silently overwrites an intervening user edit");
    }

    #[test]
    fn reproduces_reused_provider_tool_id_disappearing_from_canvas() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("first.txt"), "first-output").unwrap();
        fs::write(tmp.path().join("second.txt"), "second-output").unwrap();
        let provider = ScriptedProvider::new(vec![
            FixtureResponse::ToolCalls(vec![call(
                "reused",
                "read_file",
                json!({"path":"first.txt"}),
            )]),
            FixtureResponse::ToolCalls(vec![call(
                "reused",
                "read_file",
                json!({"path":"second.txt"}),
            )]),
            FixtureResponse::Assistant("done".into()),
        ]);
        let mut session = Session::new(SessionConfig::new(tmp.path()), provider, Deny::default());
        session.run_turn("read both").unwrap();
        let results: Vec<_> = session
            .events()
            .iter()
            .filter(|e| e.kind.as_str() == "tool.result")
            .collect();
        assert_eq!(results.len(), 2);
        let canvas = assemble_canvas(session.events(), &AutoCompactionPolicy::default());
        let outputs: Vec<_> = canvas
            .iter()
            .filter_map(|item| match item {
                CanvasItem::ToolOutput { output, .. } => Some(output.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(outputs, vec!["first-output"]);
        println!("CONFIRMED: provenance contains 2 tool results; canvas retains only first when provider call ID repeats");
    }

    struct AllowSession;
    impl PermissionDecider for AllowSession {
        fn decide(&mut self, _: &PermissionRequest) -> DeciderVerdict {
            DeciderVerdict::AllowSession
        }
    }

    #[test]
    fn reproduces_revoked_session_grant_restored_on_resume() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("events.jsonl");
        let provider = ScriptedProvider::new(vec![
            FixtureResponse::ToolCalls(vec![call(
                "w1",
                "write_file",
                json!({"path":"first.txt","content":"first"}),
            )]),
            FixtureResponse::Assistant("done".into()),
        ]);
        let mut session = Session::new(SessionConfig::new(tmp.path()), provider, AllowSession)
            .with_provenance(ProvenanceWriter::new(&log).unwrap());
        session.run_turn("write fixture").unwrap();
        assert_eq!(
            session
                .revoke_grant(
                    Capability::FsWrite,
                    &euler_core::ScopePattern::unscoped(),
                    euler_core::GrantSource::Session
                )
                .unwrap(),
            1
        );
        assert_eq!(
            session.configured_mode(Capability::FsWrite),
            Some(ApprovalMode::Ask)
        );
        drop(session);
        let decider = Deny::default();
        let asks = decider.0.clone();
        let provider = ScriptedProvider::new(vec![
            FixtureResponse::ToolCalls(vec![call(
                "w2",
                "write_file",
                json!({"path":"second.txt","content":"second"}),
            )]),
            FixtureResponse::Assistant("done".into()),
        ]);
        let mut resumed = euler_core::resume_session(
            SessionConfig::new(tmp.path()),
            ProviderSet::single(provider),
            decider,
            &log,
        )
        .unwrap();
        assert_eq!(
            resumed.configured_mode(Capability::FsWrite),
            Some(ApprovalMode::SessionAllow)
        );
        resumed.run_turn("second fixture").unwrap();
        assert_eq!(asks.load(Ordering::Relaxed), 0);
        assert_eq!(
            fs::read_to_string(tmp.path().join("second.txt")).unwrap(),
            "second"
        );
        println!("CONFIRMED: revoked fs-write session grant silently restored on resume; subsequent write bypasses denying decider");
    }

    #[test]
    fn reproduces_utf8_torn_tail_rejecting_valid_prefix() {
        use std::io::Write;
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("events.jsonl");
        let event = euler_event::EventEnvelope::new(
            "session",
            "root",
            None,
            "user.message",
            euler_event::object([("content", json!("valid prefix"))]),
        );
        let writer = ProvenanceWriter::new(&log).unwrap();
        writer.append(&[event]).unwrap();
        drop(writer);
        fs::OpenOptions::new()
            .append(true)
            .open(&log)
            .unwrap()
            .write_all(b"{\"content\":\"\xf0\x9f")
            .unwrap();
        assert!(read_provenance(&log).is_err());
        assert!(euler_core::resume::read_resume_prefix(&log).is_err());
        let page =
            euler_core::query_provenance(&log, euler_core::ProvenanceQuery::new(10)).unwrap();
        assert_eq!(page.events.len(), 1);
        println!("CONFIRMED: replay/resume cannot read a valid prefix before torn UTF-8 tail, while query_provenance can");
    }

    struct StopProvider;
    impl euler_provider::ModelProvider for StopProvider {
        fn name(&self) -> &'static str {
            "fixture"
        }
        fn invoke(
            &self,
            _: euler_provider::ModelRequest,
        ) -> Result<euler_provider::ProviderStream, euler_provider::ProviderError> {
            Ok(Box::new(
                vec![
                    Ok(euler_provider::ModelStreamEvent::TextDelta(
                        "incomplete answer".into(),
                    )),
                    Ok(euler_provider::ModelStreamEvent::Finished {
                        stop_reason: euler_provider::StopReason::MaxTokens,
                        usage: None,
                    }),
                ]
                .into_iter(),
            ))
        }
    }

    #[test]
    fn reproduces_partial_max_tokens_as_normal_completion() {
        let tmp = tempfile::tempdir().unwrap();
        let mut session = Session::new(
            SessionConfig::new(tmp.path()),
            StopProvider,
            Deny::default(),
        );
        assert!(session.run_turn("finish the task").is_ok());
        assert_eq!(
            session.events().last().unwrap().kind.as_str(),
            "assistant.message"
        );
        assert!(!session
            .events()
            .iter()
            .any(|event| event.kind.as_str() == "error"));
        println!("CONFIRMED: MaxTokens with partial text returns Ok with final assistant.message and no error event");
    }

    struct EmptyStopProvider;
    impl euler_provider::ModelProvider for EmptyStopProvider {
        fn name(&self) -> &'static str {
            "fixture"
        }
        fn invoke(
            &self,
            _: euler_provider::ModelRequest,
        ) -> Result<euler_provider::ProviderStream, euler_provider::ProviderError> {
            Ok(Box::new(
                vec![Ok(euler_provider::ModelStreamEvent::Finished {
                    stop_reason: euler_provider::StopReason::MaxTokens,
                    usage: None,
                })]
                .into_iter(),
            ))
        }
    }

    #[test]
    fn checks_resume_after_empty_non_success_stop() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("events.jsonl");
        let mut session = Session::new(
            SessionConfig::new(tmp.path()),
            EmptyStopProvider,
            Deny::default(),
        )
        .with_provenance(ProvenanceWriter::new(&log).unwrap());
        assert!(session.run_turn("finish").is_err());
        drop(session);
        let outcome = euler_core::resume_session(
            SessionConfig::new(tmp.path()),
            ProviderSet::single(ScriptedProvider::new(vec![])),
            Deny::default(),
            &log,
        );
        let Err(error) = outcome else {
            panic!("expected observed resume defect")
        };
        assert!(matches!(
            error,
            euler_core::ResumeError::DuplicateModelTerminal { .. }
        ));
        println!("CONFIRMED RESUME FAILURE after empty non-success stop: {error}");
    }

    struct PartialFailureProvider;
    impl euler_provider::ModelProvider for PartialFailureProvider {
        fn name(&self) -> &'static str {
            "fixture"
        }
        fn invoke(
            &self,
            _: euler_provider::ModelRequest,
        ) -> Result<euler_provider::ProviderStream, euler_provider::ProviderError> {
            Ok(Box::new(
                vec![
                    Ok(euler_provider::ModelStreamEvent::TextDelta(
                        "PARTIAL_RESEARCH_EVIDENCE_CANARY".into(),
                    )),
                    Err(euler_provider::ProviderError::transport(
                        "synthetic interrupted connection",
                    )),
                ]
                .into_iter(),
            ))
        }
    }

    #[test]
    fn reproduces_partial_stream_content_missing_from_durable_record() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("events.jsonl");
        let mut session = Session::new(
            SessionConfig::new(tmp.path()),
            PartialFailureProvider,
            Deny::default(),
        )
        .with_provenance(ProvenanceWriter::new(&log).unwrap());
        assert!(session.run_turn("research fixture").is_err());
        assert!(session
            .events()
            .iter()
            .any(|event| event.payload.get("delta")
                == Some(&json!("PARTIAL_RESEARCH_EVIDENCE_CANARY"))));
        let durable = read_provenance(&log).unwrap();
        assert!(!serde_json::to_string(&durable)
            .unwrap()
            .contains("PARTIAL_RESEARCH_EVIDENCE_CANARY"));
        println!("CONFIRMED: visible partial model output exists in memory but disappears from durable provenance after transport failure");
    }
}
