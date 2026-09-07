//! Synthetic, private audit probes against unmodified Euler source.
//! Assertions document observed defects, not desired regression behavior.

#[cfg(test)]
mod tests {
    use euler_core::{
        assemble_canvas, read_provenance, ApprovalMode, AutoCompactionPolicy, CanvasItem,
        DeciderVerdict, PermissionDecider, PermissionRequest, ProvenanceWriter, Session,
        SessionConfig,
    };
    use euler_provider::{FixtureResponse, ProviderSet, ScriptedProvider, ToolCall};
    use euler_sdk::Capability;
    use serde_json::json;
    use std::{
        fs,
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
        // Since #218 every turn ends with a `run.terminal` event; the defect is
        // that a partial MaxTokens stop still terminalizes as `completed`.
        assert!(session
            .events()
            .iter()
            .any(|event| event.kind.as_str() == "assistant.message"));
        let terminal = session.events().last().unwrap();
        assert_eq!(terminal.kind.as_str(), "run.terminal");
        assert_eq!(terminal.payload.get("status"), Some(&json!("completed")));
        assert!(!session
            .events()
            .iter()
            .any(|event| event.kind.as_str() == "error"));
        println!("CONFIRMED: MaxTokens with partial text returns Ok with an assistant.message and a completed run.terminal, no error event");
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
}
