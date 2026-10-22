use super::*;
use crate::core::ai::{
    CodexReasoningEffort, DeepSeekReasoningEffort, deepseek_key_account,
    normalize_deepseek_base_url,
};
use crate::core::keychain::CredentialStore;
use std::sync::Arc;

pub(crate) struct AiSettingsState {
    pub(crate) inputs: [Entity<InputState>; 4],
    key: Entity<InputState>,
    id: Uuid,
    configured: bool,
    busy: bool,
    error: Option<L10nKey>,
}

impl AiSettingsState {
    pub(crate) fn new(
        window: &mut Window,
        cx: &mut Context<Tty7App>,
        subs: &mut Vec<Subscription>,
    ) -> Self {
        let cfg = cx.global::<Config>();
        let values = [
            cfg.ai.codex.model.clone(),
            cfg.ai.deepseek.base_url.clone(),
            cfg.ai.deepseek.model.clone(),
            cfg.git_ai.style_rules.clone(),
        ];
        let inputs: [Entity<InputState>; 4] = std::array::from_fn(|i| {
            cx.new(|cx| {
                InputState::new(window, cx)
                    .multi_line(i == 3)
                    .placeholder(if i == 3 {
                        t(L10nKey::SettingsGitRulesPlaceholder)
                    } else {
                        ""
                    })
                    .default_value(values[i].clone())
            })
        });
        for (i, input) in inputs.iter().enumerate().take(4) {
            subs.push(
                cx.subscribe_in(input, window, move |this, input, ev, _, cx| {
                    if matches!(ev, InputEvent::Blur)
                        || (i != 3 && matches!(ev, InputEvent::PressEnter { .. }))
                    {
                        let value = input.read(cx).value().to_string();
                        this.commit_ai_text(i, value, cx);
                    }
                    if matches!(ev, InputEvent::Change) {
                        cx.notify();
                    }
                }),
            );
        }
        Self {
            inputs,
            key: cx.new(|cx| InputState::new(window, cx).masked(true)),
            id: Uuid::new_v4(),
            configured: false,
            busy: false,
            error: None,
        }
    }
}

// Never propagate keychain backend errors: implementations may include secrets.
fn key_operation(
    store: &dyn CredentialStore,
    dir: &std::path::Path,
    action: Option<Option<String>>,
) -> Result<bool, L10nKey> {
    let account = deepseek_key_account(dir).map_err(|_| L10nKey::SettingsAiConfigDirError)?;
    match action {
        None => store.get("tty7-ai", &account).map(|key| key.is_some()),
        Some(Some(key)) => store.set("tty7-ai", &account, &key).map(|_| true),
        Some(None) => store.delete("tty7-ai", &account).map(|_| false),
    }
    .map_err(|_| L10nKey::AiCredentialError)
}

impl Tty7App {
    pub(crate) fn commit_ai_text(&mut self, index: usize, value: String, cx: &mut Context<Self>) {
        let value = if index == 3 {
            value
        } else {
            value.trim().to_string()
        };
        let value = if index == 1 {
            match normalize_deepseek_base_url(&value) {
                Ok(value) => value,
                Err(_) => {
                    cx.notify();
                    return;
                }
            }
        } else {
            value
        };
        self.update_config(cx, |cfg| match index {
            0 => cfg.ai.codex.model = value,
            1 => cfg.ai.deepseek.base_url = value,
            2 => {
                cfg.ai.deepseek.model = if value.is_empty() {
                    "deepseek-flash".into()
                } else {
                    value
                }
            }
            3 => cfg.git_ai.style_rules = value,
            _ => unreachable!(),
        });
    }

    pub(crate) fn refresh_ai_controls(
        &mut self,
        indices: &[usize],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(s) = self.active_settings() {
            for &index in indices {
                let cfg = cx.global::<Config>();
                let value = match index {
                    0 => cfg.ai.codex.model.clone(),
                    1 => cfg.ai.deepseek.base_url.clone(),
                    2 => cfg.ai.deepseek.model.clone(),
                    3 => cfg.git_ai.style_rules.clone(),
                    _ => unreachable!(),
                };
                s.ai.inputs[index].update(cx, |input, cx| input.set_value(value, window, cx));
            }
        }
    }

    pub(crate) fn ai_key_action(
        &mut self,
        action: Option<bool>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        #[cfg(not(test))]
        let store: Arc<dyn CredentialStore> = Arc::new(OsCredentialStore);
        #[cfg(test)]
        let store: Arc<dyn CredentialStore> =
            Arc::new(crate::core::keychain::InMemoryCredentialStore::default());
        self.ai_key_action_with_store(action, store, window, cx);
    }

    fn ai_key_action_with_store(
        &mut self,
        action: Option<bool>,
        store: Arc<dyn CredentialStore>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(s) = self.active_settings() else {
            return;
        };
        if s.ai.busy {
            return;
        }
        let id = s.ai.id;
        let secret = action
            .filter(|save| *save)
            .map(|_| s.ai.key.read(cx).value().to_string());
        if secret.as_ref().is_some_and(|key| key.trim().is_empty()) {
            return;
        }
        // Use the normal persistence path first; it creates the config directory
        // and retains the existing retry/discard behavior on write failure.
        if action == Some(true) {
            self.persist_settings_config(cx);
            if self
                .active_settings()
                .is_some_and(|s| s.save_error.is_some())
            {
                return;
            }
        }
        let Some(dir) = crate::core::config::config_dir_path() else {
            if let Some(s) = self.active_settings_mut() {
                s.ai.error = Some(L10nKey::SettingsAiConfigDirError);
            }
            cx.notify();
            return;
        };
        if let Some(s) = self.active_settings_mut() {
            s.ai.busy = true;
            s.ai.error = None;
        }
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    key_operation(
                        store.as_ref(),
                        &dir,
                        action.map(|save| if save { secret } else { None }),
                    )
                })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                if let Some(s) = this.active_settings_mut().filter(|s| s.ai.id == id) {
                    s.ai.busy = false;
                    match result {
                        Ok(configured) => {
                            s.ai.configured = configured;
                            if action.is_some() {
                                s.ai.key.update(cx, |key, cx| key.set_value("", window, cx));
                            }
                        }
                        Err(error) => s.ai.error = Some(error),
                    }
                    cx.notify();
                }
            });
        })
        .detach();
    }

    pub(crate) fn render_settings_ai(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(s) = self.active_settings() else {
            return div().into_any_element();
        };
        let cfg = cx.global::<Config>().ai.clone();
        let codex_model = self
            .settings_text_input(&s.ai.inputs[0], PATH_FIELD_W, false, cx)
            .into_any_element();
        let url_error = normalize_deepseek_base_url(&s.ai.inputs[1].read(cx).value())
            .err()
            .map(|_| t(L10nKey::AiInvalidUrl).to_string());
        let url = self.settings_checked_input(&s.ai.inputs[1], PATH_FIELD_W, url_error, cx);
        let model = self
            .settings_text_input(&s.ai.inputs[2], PATH_FIELD_W, false, cx)
            .into_any_element();
        let key_input = self.settings_text_input(&s.ai.key, FIELD_W, false, cx);
        let status = t(if s.ai.configured {
            L10nKey::SettingsAiKeyConfigured
        } else {
            L10nKey::SettingsAiKeyNotConfigured
        });
        let error = s.ai.error;
        let busy = s.ai.busy;
        let key = v_flex()
            .gap(px(8.))
            .child(key_input)
            .child(div().child(status))
            .child(
                h_flex()
                    .gap(px(8.))
                    .child(
                        self.settings_button(
                            "ai-save-key",
                            t(L10nKey::SettingsAiSaveKey),
                            cx,
                            |this, w, cx| this.ai_key_action(Some(true), w, cx),
                        )
                        .disabled(busy),
                    )
                    .child(
                        self.settings_button(
                            "ai-remove-key",
                            t(L10nKey::SettingsAiRemoveKey),
                            cx,
                            |this, w, cx| this.ai_key_action(Some(false), w, cx),
                        )
                        .disabled(busy),
                    ),
            )
            .when_some(error, |col, error| {
                col.child(div().text_color(kit::Tk::of(cx).danger).child(t(error)))
            })
            .into_any_element();
        let mut codex_labels = vec![t(L10nKey::SettingsAiDefault)];
        codex_labels.extend(CodexReasoningEffort::ALL.iter().map(|v| v.as_str()));
        let effort = self.settings_choice(
            "ai-codex-effort",
            &codex_labels,
            cfg.codex
                .reasoning_effort
                .and_then(|v| CodexReasoningEffort::ALL.iter().position(|x| *x == v))
                .map_or(0, |i| i + 1),
            cx,
            |this, i, _, cx| {
                this.update_config(cx, |cfg| {
                    cfg.ai.codex.reasoning_effort =
                        i.checked_sub(1).map(|i| CodexReasoningEffort::ALL[i])
                })
            },
        );
        let mut deepseek_labels = vec![t(L10nKey::SettingsAiDefault)];
        deepseek_labels.extend(DeepSeekReasoningEffort::ALL.iter().map(|v| v.as_str()));
        let deepseek_effort = self.settings_choice(
            "ai-deepseek-effort",
            &deepseek_labels,
            cfg.deepseek
                .reasoning_effort
                .and_then(|v| DeepSeekReasoningEffort::ALL.iter().position(|x| *x == v))
                .map_or(0, |i| i + 1),
            cx,
            |this, i, _, cx| {
                this.update_config(cx, |cfg| {
                    cfg.ai.deepseek.reasoning_effort =
                        i.checked_sub(1).map(|i| DeepSeekReasoningEffort::ALL[i])
                })
            },
        );
        Self::settings_page([
            self.settings_group(
                Some("Codex CLI"),
                Some(t(L10nKey::SettingsCodexScope).into()),
                [
                    self.settings_row(
                        t(L10nKey::SettingsCodexModel),
                        t(L10nKey::SettingsCodexModelDesc),
                        codex_model,
                        cx,
                    )
                    .into_any_element(),
                    self.settings_row(
                        t(L10nKey::SettingsCodexEffort),
                        t(L10nKey::SettingsAiEffortDesc),
                        effort,
                        cx,
                    )
                    .into_any_element(),
                ],
                cx,
            ),
            self.settings_group(
                Some("DeepSeek API"),
                Some(t(L10nKey::SettingsDeepSeekDesc).into()),
                [
                    self.settings_row(
                        t(L10nKey::SettingsDeepSeekUrl),
                        t(L10nKey::SettingsDeepSeekUrlDesc),
                        url,
                        cx,
                    )
                    .into_any_element(),
                    self.settings_row(
                        t(L10nKey::SettingsDeepSeekKey),
                        t(L10nKey::SettingsDeepSeekKeyDesc),
                        key,
                        cx,
                    )
                    .into_any_element(),
                    self.settings_row(t(L10nKey::SettingsDeepSeekModel), "", model, cx)
                        .into_any_element(),
                    self.settings_row(
                        t(L10nKey::SettingsDeepSeekEffort),
                        t(L10nKey::SettingsAiEffortDesc),
                        deepseek_effort,
                        cx,
                    )
                    .into_any_element(),
                ],
                cx,
            ),
        ])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::keychain::{CredentialError, CredentialResult, InMemoryCredentialStore};
    use gpui::TestAppContext;

    struct FailingStore;
    impl CredentialStore for FailingStore {
        fn get(&self, _: &str, _: &str) -> CredentialResult<Option<String>> {
            Err(CredentialError::Backend("do not expose secret".into()))
        }
        fn set(&self, _: &str, _: &str, secret: &str) -> CredentialResult<()> {
            Err(CredentialError::Backend(secret.to_owned()))
        }
        fn delete(&self, _: &str, _: &str) -> CredentialResult<()> {
            Err(CredentialError::Backend("do not expose secret".into()))
        }
    }

    #[test]
    fn key_operations_are_explicit_isolated_and_errors_are_safe() {
        let dir = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let store = InMemoryCredentialStore::default();
        assert_eq!(key_operation(&store, dir.path(), None), Ok(false));
        assert_eq!(
            key_operation(&store, dir.path(), Some(Some("private".into()))),
            Ok(true)
        );
        assert_eq!(key_operation(&store, dir.path(), None), Ok(true));
        assert_eq!(key_operation(&store, other.path(), None), Ok(false));
        assert_eq!(key_operation(&store, dir.path(), Some(None)), Ok(false));
        for action in [None, Some(None), Some(Some("private".into()))] {
            assert_eq!(
                key_operation(&FailingStore, dir.path(), action),
                Err(L10nKey::AiCredentialError)
            );
        }
        assert_eq!(
            key_operation(
                &store,
                &dir.path().join("missing"),
                Some(Some("private".into()))
            ),
            Err(L10nKey::SettingsAiConfigDirError)
        );
        assert!(store.is_empty());
    }

    #[gpui::test]
    fn failed_key_save_keeps_replacement_and_success_controls_status(cx: &mut TestAppContext) {
        let (app, mut vcx) = super::super::gpui_tests::harness(cx);
        app.update_in(&mut vcx, |app, window, cx| {
            app.open_settings_section(SettingsSection::Ai, window, cx)
        });
        vcx.run_until_parked();
        app.update_in(&mut vcx, |app, window, cx| {
            let key = app.active_settings().unwrap().ai.key.clone();
            key.update(cx, |key, cx| {
                key.set_value("replacement-secret", window, cx)
            });
            app.ai_key_action_with_store(Some(true), Arc::new(FailingStore), window, cx);
        });
        vcx.run_until_parked();
        let store = Arc::new(InMemoryCredentialStore::default());
        app.update_in(&mut vcx, |app, window, cx| {
            let state = &app.active_settings().unwrap().ai;
            assert!(!state.busy);
            assert!(!state.configured);
            assert_eq!(state.error, Some(L10nKey::AiCredentialError));
            assert_eq!(state.key.read(cx).value().as_str(), "replacement-secret");
            app.ai_key_action_with_store(Some(true), store.clone(), window, cx);
        });
        vcx.run_until_parked();
        app.update_in(&mut vcx, |app, window, cx| {
            let state = &app.active_settings().unwrap().ai;
            assert!(state.configured);
            assert!(!state.busy);
            assert!(state.error.is_none());
            assert!(state.key.read(cx).value().is_empty());
            assert!(
                !serde_json::to_string(&cx.global::<Config>().0)
                    .unwrap()
                    .contains("replacement-secret")
            );
            app.ai_key_action_with_store(Some(false), store.clone(), window, cx);
        });
        vcx.run_until_parked();
        app.update_in(&mut vcx, |app, _, _| {
            assert!(!app.active_settings().unwrap().ai.configured);
            assert!(store.is_empty());
        });
    }

    #[gpui::test]
    fn multiline_rules_blur_save_search_and_targeted_reset(cx: &mut TestAppContext) {
        let (app, mut vcx) = super::super::gpui_tests::harness(cx);
        app.update_in(&mut vcx, |app, window, cx| {
            app.open_settings_section(SettingsSection::Git, window, cx)
        });
        vcx.run_until_parked();
        app.update_in(&mut vcx, |app, window, cx| {
            app.commit_ai_text(0, "  test-model  ".into(), cx);
            let input = app.active_settings().unwrap().ai.inputs[3].clone();
            input.update(cx, |input, cx| {
                input.set_value("Conventional Commits\nKeep a useful body", window, cx);
                cx.emit(InputEvent::Blur);
            });
        });
        vcx.run_until_parked();
        app.update_in(&mut vcx, |app, window, cx| {
            assert_eq!(
                cx.global::<Config>().git_ai.style_rules,
                "Conventional Commits\nKeep a useful body"
            );
            assert_eq!(cx.global::<Config>().ai.codex.model, "test-model");
            let search = app.active_settings().unwrap().search.clone();
            search.update(cx, |input, cx| {
                input.set_value("git_ai.style_rules", window, cx)
            });
            app.autoselect_settings_search(cx);
        });
        vcx.run_until_parked();
        app.update_in(&mut vcx, |app, window, cx| {
            assert!(app.active_settings().unwrap().search_active);
            let pending_model = app.active_settings().unwrap().ai.inputs[0].clone();
            pending_model.update(cx, |input, cx| {
                input.set_value("pending model edit", window, cx)
            });
            app.reset_settings_value(L10nKey::SettingsGitRules, window, cx);
            assert!(cx.global::<Config>().git_ai.style_rules.is_empty());
            assert_eq!(cx.global::<Config>().ai.codex.model, "test-model");
            assert_eq!(pending_model.read(cx).value(), "pending model edit");
            let original = cx.global::<Config>().ai.deepseek.base_url.clone();
            app.commit_ai_text(1, "https://user:secret@example.com?token=secret".into(), cx);
            assert_eq!(cx.global::<Config>().ai.deepseek.base_url, original);
        });
        vcx.run_until_parked();
    }

    #[test]
    fn settings_search_indexes_plain_fields_but_not_key_storage() {
        let entries = super::super::settings_search_entries();
        let cfg = Config::default();
        for (key, config_key) in [
            (L10nKey::SettingsCodexModel, "ai.codex.model"),
            (L10nKey::SettingsCodexEffort, "ai.codex.reasoning_effort"),
            (L10nKey::SettingsDeepSeekUrl, "ai.deepseek.base_url"),
            (L10nKey::SettingsDeepSeekModel, "ai.deepseek.model"),
            (
                L10nKey::SettingsDeepSeekEffort,
                "ai.deepseek.reasoning_effort",
            ),
            (L10nKey::SettingsGitProvider, "git_ai.provider"),
            (L10nKey::SettingsGitLanguage, "git_ai.language"),
            (L10nKey::SettingsGitRules, "git_ai.style_rules"),
        ] {
            let entry = entries.iter().find(|entry| entry.title == key).unwrap();
            assert_eq!(entry.config_key(), config_key);
            assert!(!entry.modified(&cfg));
            assert!(super::super::entry_matches(entry, config_key));
        }
        let key = entries
            .iter()
            .find(|entry| entry.title == L10nKey::SettingsDeepSeekKey)
            .unwrap();
        assert!(key.config_key().is_empty());
        assert!(!key.modified(&cfg));
        assert!(super::super::entry_matches(key, "API Key"));
        let rules = entries
            .iter()
            .find(|entry| entry.title == L10nKey::SettingsGitRules)
            .unwrap();
        let mut changed = cfg.clone();
        changed.git_ai.style_rules = "Use a body".into();
        assert!(rules.modified(&changed));
        assert!(!key.modified(&changed));
    }
}
