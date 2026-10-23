use super::*;
use crate::core::ai::{
    CodexReasoningEffort, DeepSeekReasoningEffort, DeepSeekSettings, deepseek_key_account,
    normalize_deepseek_base_url,
};
use crate::core::ai_completion::{AiCompletionError, test_deepseek_connection};
use crate::core::keychain::CredentialStore;
use gpui_component::ActiveTheme as _;
use gpui_component::group_box::{GroupBox, GroupBoxVariants as _};
use std::sync::Arc;
use std::time::Duration;

struct ConnectionTest {
    epoch: u64,
    settings: DeepSeekSettings,
    result: Option<Result<Duration, String>>,
}

pub(crate) struct AiSettingsState {
    pub(crate) inputs: [Entity<InputState>; 4],
    key: Entity<InputState>,
    id: Uuid,
    configured: bool,
    busy: bool,
    error: Option<L10nKey>,
    test_epoch: u64,
    connection_test: Option<ConnectionTest>,
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
                    if matches!(ev, InputEvent::Change) && matches!(i, 1 | 2) {
                        this.invalidate_ai_connection_test();
                    }
                    if matches!(ev, InputEvent::Change) {
                        cx.notify();
                    }
                }),
            );
        }
        let key = cx.new(|cx| InputState::new(window, cx).masked(true));
        subs.push(cx.subscribe_in(&key, window, |this, _, ev, _, cx| {
            if matches!(ev, InputEvent::Change) {
                this.invalidate_ai_connection_test();
                cx.notify();
            }
        }));
        Self {
            inputs,
            key,
            id: Uuid::new_v4(),
            configured: false,
            busy: false,
            error: None,
            test_epoch: 0,
            connection_test: None,
        }
    }

    fn invalidate_connection_test(&mut self) {
        self.test_epoch = self.test_epoch.wrapping_add(1);
        self.connection_test = None;
    }

    fn form_matches(&self, settings: &DeepSeekSettings, cx: &App) -> bool {
        let url = self.inputs[1].read(cx).value();
        let model = self.inputs[2].read(cx).value();
        let model = model.trim();
        normalize_deepseek_base_url(&url).is_ok_and(|url| url == settings.base_url)
            && (if model.is_empty() {
                "deepseek-flash"
            } else {
                model
            }) == settings.model
            && self.key.read(cx).value().is_empty()
    }
}

// Never propagate keychain backend errors: implementations may include secrets.
fn key_operation(
    store: &dyn CredentialStore,
    dir: &std::path::Path,
    action: Option<Option<&str>>,
) -> Result<bool, L10nKey> {
    let account = deepseek_key_account(dir).map_err(|_| L10nKey::SettingsAiConfigDirError)?;
    match action {
        None => store.get("tty7-ai", &account).map(|key| key.is_some()),
        Some(Some(key)) => store.set("tty7-ai", &account, key).map(|_| true),
        Some(None) => store.delete("tty7-ai", &account).map(|_| false),
    }
    .map_err(|_| L10nKey::AiCredentialError)
}

/// AI/Git opt into cards without changing the layout of other settings pages.
pub(super) fn settings_ai_card(
    id: &'static str,
    title: &str,
    desc: &str,
    body: impl IntoIterator<Item = AnyElement>,
    cx: &App,
) -> AnyElement {
    GroupBox::new()
        .id(id)
        .fill()
        .child(
            v_flex()
                .gap(px(4.))
                .min_w_0()
                .child(
                    div()
                        .text_size(kit::fs(13.5))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(cx.theme().foreground)
                        .child(title.to_string()),
                )
                .when(!desc.is_empty(), |col| {
                    col.child(
                        div()
                            .text_size(kit::fs(12.))
                            .line_height(kit::fs(17.))
                            .text_color(cx.theme().muted_foreground)
                            .child(desc.to_string()),
                    )
                }),
        )
        .child(v_flex().w_full().min_w_0().children(body))
        .into_any_element()
}

impl Tty7App {
    pub(crate) fn invalidate_ai_connection_test(&mut self) {
        if let Some(s) = self.active_settings_mut() {
            s.ai.invalidate_connection_test();
        }
    }

    pub(crate) fn invalidate_changed_ai_connection_test(&mut self, cx: &App) {
        let changed = self.active_settings().is_some_and(|s| {
            s.ai.connection_test.as_ref().is_some_and(|test| {
                test.settings != cx.global::<Config>().ai.deepseek
                    || !s.ai.form_matches(&test.settings, cx)
            })
        });
        if changed {
            self.invalidate_ai_connection_test();
        }
    }

    pub(crate) fn set_deepseek_enabled(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.update_config(cx, |cfg| cfg.ai.deepseek.enabled = enabled);
    }

    pub(crate) fn ai_test_connection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        #[cfg(not(test))]
        let credentials: Arc<dyn CredentialStore> = Arc::new(OsCredentialStore);
        #[cfg(test)]
        let credentials: Arc<dyn CredentialStore> =
            Arc::new(crate::core::keychain::InMemoryCredentialStore::default());
        let Some((id, epoch, settings)) = self.begin_ai_connection_test(cx) else {
            return;
        };
        let Some(config_dir) = crate::core::config::config_dir_path() else {
            self.finish_ai_connection_test(
                id,
                epoch,
                Err(t(L10nKey::SettingsAiConfigDirError).into()),
                cx,
            );
            return;
        };
        let http = cx.http_client();
        cx.spawn_in(window, async move |this, cx| {
            let result = test_deepseek_connection(settings, credentials, config_dir, http)
                .await
                .map_err(|error| {
                    error
                        .downcast_ref::<AiCompletionError>()
                        .map(ToString::to_string)
                        .unwrap_or_else(|| t(L10nKey::AiNetworkError).into())
                });
            let _ = this.update_in(cx, |this, _, cx| {
                this.finish_ai_connection_test(id, epoch, result, cx)
            });
        })
        .detach();
    }

    fn begin_ai_connection_test(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Option<(Uuid, u64, DeepSeekSettings)> {
        let s = self.active_settings()?;
        if s.ai.busy
            || s.ai
                .connection_test
                .as_ref()
                .is_some_and(|test| test.result.is_none())
        {
            return None;
        }
        let settings = cx.global::<Config>().ai.deepseek.clone();
        let problem = if !s.ai.key.read(cx).value().is_empty() {
            Some(L10nKey::SettingsAiSaveReplacementFirst)
        } else if s.save_error.is_some() || !s.ai.form_matches(&settings, cx) {
            Some(L10nKey::SettingsAiSaveSettingsFirst)
        } else if !s.ai.configured {
            Some(L10nKey::AiMissingKey)
        } else {
            None
        };
        let s = self.active_settings_mut()?;
        s.ai.invalidate_connection_test();
        let epoch = s.ai.test_epoch;
        s.ai.connection_test = Some(ConnectionTest {
            epoch,
            settings: settings.clone(),
            result: problem.map(|key| Err(t(key).into())),
        });
        let id = s.ai.id;
        cx.notify();
        problem.is_none().then_some((id, epoch, settings))
    }

    fn finish_ai_connection_test(
        &mut self,
        id: Uuid,
        epoch: u64,
        result: Result<Duration, String>,
        cx: &mut Context<Self>,
    ) {
        self.invalidate_changed_ai_connection_test(cx);
        if let Some(s) = self.active_settings_mut().filter(|s| s.ai.id == id)
            && let Some(test) =
                s.ai.connection_test
                    .as_mut()
                    .filter(|test| test.epoch == epoch)
        {
            test.result = Some(result);
            cx.notify();
        }
    }
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
        if action.is_some() {
            self.invalidate_ai_connection_test();
        }
        if let Some(s) = self.active_settings_mut() {
            s.ai.busy = true;
            s.ai.error = None;
        }
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let (result, secret) = cx
                .background_spawn(async move {
                    let result = key_operation(
                        store.as_ref(),
                        &dir,
                        action.map(|save| if save { secret.as_deref() } else { None }),
                    );
                    (result, secret)
                })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                if let Some(s) = this.active_settings_mut().filter(|s| s.ai.id == id) {
                    s.ai.busy = false;
                    match result {
                        Ok(configured) => {
                            s.ai.configured = configured;
                            if action == Some(true)
                                && secret.as_deref() == Some(s.ai.key.read(cx).value().as_str())
                            {
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
        let (codex_effort_value, deepseek_effort_value, deepseek_enabled) = {
            let cfg = &cx.global::<Config>().ai;
            (
                cfg.codex.reasoning_effort,
                cfg.deepseek.reasoning_effort,
                cfg.deepseek.enabled,
            )
        };
        let muted = cx.theme().muted_foreground;
        let border = cx.theme().border;
        let danger = cx.theme().danger;
        let success = cx.theme().success;
        let codex_model = self
            .settings_text_input(&s.ai.inputs[0], PATH_FIELD_W, false, cx)
            .id("ai-codex-model")
            .into_any_element();
        let url_error = normalize_deepseek_base_url(&s.ai.inputs[1].read(cx).value())
            .err()
            .map(|_| t(L10nKey::AiInvalidUrl).to_string());
        let url = div()
            .id("ai-deepseek-url")
            .child(self.settings_checked_input(&s.ai.inputs[1], PATH_FIELD_W, url_error, cx))
            .into_any_element();
        let model = self
            .settings_text_input(&s.ai.inputs[2], PATH_FIELD_W, false, cx)
            .id("ai-deepseek-model")
            .into_any_element();
        let key_input = self
            .settings_text_input(&s.ai.key, PATH_FIELD_W, false, cx)
            .id("ai-deepseek-key");
        let status = t(if s.ai.configured {
            L10nKey::SettingsAiKeyConfigured
        } else {
            L10nKey::SettingsAiKeyNotConfigured
        });
        let error = s.ai.error;
        let busy = s.ai.busy;
        let pending_key = !s.ai.key.read(cx).value().is_empty();
        let testing =
            s.ai.connection_test
                .as_ref()
                .is_some_and(|test| test.result.is_none());
        let test_result =
            s.ai.connection_test
                .as_ref()
                .and_then(|test| test.result.as_ref());
        let test_status = match test_result {
            Some(Ok(elapsed)) => Some((
                t_fmt(
                    L10nKey::SettingsAiTestSuccess,
                    &[("ms", &elapsed.as_millis().to_string())],
                ),
                success,
            )),
            Some(Err(error)) => Some((error.clone(), danger)),
            None => None,
        };
        let key = v_flex()
            .w_full()
            .gap(px(10.))
            .child(key_input)
            .child(
                v_flex()
                    .id("ai-deepseek-key-footer")
                    .w_full()
                    .gap(px(8.))
                    .pt(px(10.))
                    .border_t_1()
                    .border_color(border)
                    .child(
                        div()
                            .id("ai-deepseek-key-status")
                            .text_size(kit::fs(12.))
                            .text_color(muted)
                            .child(if busy {
                                t(L10nKey::SettingsAiKeyWorking)
                            } else {
                                status
                            }),
                    )
                    .child(
                        h_flex()
                            .flex_wrap()
                            .gap(px(8.))
                            .child(
                                self.settings_button(
                                    "ai-save-key",
                                    t(L10nKey::SettingsAiSaveKey),
                                    cx,
                                    |this, w, cx| this.ai_key_action(Some(true), w, cx),
                                )
                                .disabled(busy || !pending_key),
                            )
                            .child(
                                self.settings_button(
                                    "ai-remove-key",
                                    t(L10nKey::SettingsAiRemoveKey),
                                    cx,
                                    |this, w, cx| this.ai_key_action(Some(false), w, cx),
                                )
                                .disabled(busy || !s.ai.configured),
                            ),
                    )
                    .when(pending_key, |col| {
                        col.child(
                            div()
                                .text_size(kit::fs(12.))
                                .text_color(muted)
                                .child(t(L10nKey::SettingsAiSaveReplacementFirst)),
                        )
                    }),
            )
            .when_some(error, |col, error| {
                col.child(div().text_color(danger).child(t(error)))
            })
            .into_any_element();
        let mut codex_labels = vec![t(L10nKey::SettingsAiDefault)];
        codex_labels.extend(CodexReasoningEffort::ALL.iter().map(|v| v.as_str()));
        let effort = self.settings_choice(
            "ai-codex-effort",
            &codex_labels,
            codex_effort_value
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
            deepseek_effort_value
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
        let enabled = self.settings_switch(
            "ai-deepseek-enabled",
            deepseek_enabled,
            cx,
            |this, on, _, cx| this.set_deepseek_enabled(on, cx),
        );
        let probe = v_flex()
            .w_full()
            .items_start()
            .gap(px(8.))
            .child(
                self.settings_button(
                    "ai-deepseek-test",
                    t(if testing {
                        L10nKey::SettingsAiTesting
                    } else {
                        L10nKey::SettingsAiTestConnection
                    }),
                    cx,
                    |this, window, cx| this.ai_test_connection(window, cx),
                )
                .disabled(busy || testing),
            )
            .when_some(test_status, |col, (status, color)| {
                col.child(
                    div()
                        .id("ai-deepseek-test-status")
                        .text_size(kit::fs(12.))
                        .text_color(color)
                        .child(status),
                )
            })
            .into_any_element();
        let codex_rows = [
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
            div()
                .id("ai-codex-scope")
                .mt(px(8.))
                .pt(px(12.))
                .border_t_1()
                .border_color(border)
                .text_size(kit::fs(12.))
                .line_height(kit::fs(18.))
                .text_color(muted)
                .child(t(L10nKey::SettingsCodexScope))
                .into_any_element(),
        ];
        let deepseek_rows = [
            self.settings_row(
                t(L10nKey::SettingsDeepSeekEnabled),
                t(L10nKey::SettingsDeepSeekEnabledDesc),
                enabled,
                cx,
            )
            .into_any_element(),
            self.settings_row(
                t(L10nKey::SettingsDeepSeekUrl),
                t(L10nKey::SettingsDeepSeekUrlDesc),
                url,
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
            self.settings_row(
                t(L10nKey::SettingsDeepSeekKey),
                t(L10nKey::SettingsDeepSeekKeyDesc),
                key,
                cx,
            )
            .into_any_element(),
            self.settings_row(
                t(L10nKey::SettingsAiTestConnection),
                t(L10nKey::SettingsAiTestDesc),
                probe,
                cx,
            )
            .into_any_element(),
        ];
        v_flex()
            .id("settings-ai-page")
            .gap(px(24.))
            .child(self.settings_group(
                Some(t(L10nKey::SettingsAiLocalTools)),
                None,
                [settings_ai_card(
                    "ai-codex-card",
                    "Codex CLI",
                    t(L10nKey::SettingsCodexCardDesc),
                    codex_rows,
                    cx,
                )],
                cx,
            ))
            .child(self.settings_group(
                Some(t(L10nKey::SettingsAiProviders)),
                None,
                [settings_ai_card(
                    "ai-deepseek-card",
                    "DeepSeek API",
                    t(L10nKey::SettingsDeepSeekDesc),
                    deepseek_rows,
                    cx,
                )],
                cx,
            ))
            .into_any_element()
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
            (L10nKey::SettingsDeepSeekEnabled, "ai.deepseek.enabled"),
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

    #[gpui::test]
    fn deepseek_toggle_and_reset_preserve_key_and_pending_edits(cx: &mut TestAppContext) {
        let (app, mut vcx) = super::super::gpui_tests::harness(cx);
        let store = Arc::new(InMemoryCredentialStore::default());
        app.update_in(&mut vcx, |app, window, cx| {
            app.open_settings_section(SettingsSection::Ai, window, cx);
        });
        vcx.run_until_parked();
        app.update_in(&mut vcx, |app, window, cx| {
            let key = app.active_settings().unwrap().ai.key.clone();
            key.update(cx, |input, cx| input.set_value("saved-key", window, cx));
            app.ai_key_action_with_store(Some(true), store.clone(), window, cx);
        });
        vcx.run_until_parked();
        app.update_in(&mut vcx, |app, window, cx| {
            assert!(!cx.global::<Config>().ai.deepseek.enabled);
            let model = app.active_settings().unwrap().ai.inputs[2].clone();
            let rules = app.active_settings().unwrap().ai.inputs[3].clone();
            model.update(cx, |input, cx| input.set_value("unsaved model", window, cx));
            rules.update(cx, |input, cx| {
                input.set_value("unsaved\nrules", window, cx)
            });
            app.set_deepseek_enabled(true, cx);
            assert!(
                app.active_settings()
                    .unwrap()
                    .saved_config
                    .ai
                    .deepseek
                    .enabled
            );
            app.reset_settings_value(L10nKey::SettingsDeepSeekEnabled, window, cx);
            assert!(!cx.global::<Config>().ai.deepseek.enabled);
            assert!(app.active_settings().unwrap().ai.configured);
            assert!(!store.is_empty());
        });
        vcx.run_until_parked();
        app.update_in(&mut vcx, |app, _, cx| {
            let state = app.active_settings().unwrap();
            assert_eq!(state.ai.inputs[2].read(cx).value(), "unsaved model");
            assert_eq!(state.ai.inputs[3].read(cx).value(), "unsaved\nrules");
        });
    }

    #[gpui::test]
    fn connection_test_requires_saved_key_and_never_enables_provider(cx: &mut TestAppContext) {
        let (app, mut vcx) = super::super::gpui_tests::harness(cx);
        app.update_in(&mut vcx, |app, window, cx| {
            app.open_settings_section(SettingsSection::Ai, window, cx);
        });
        vcx.run_until_parked();
        app.update_in(&mut vcx, |app, window, cx| {
            assert!(app.begin_ai_connection_test(cx).is_none());
            let state = &app.active_settings().unwrap().ai;
            assert_eq!(
                state
                    .connection_test
                    .as_ref()
                    .unwrap()
                    .result
                    .as_ref()
                    .unwrap()
                    .as_ref()
                    .unwrap_err(),
                t(L10nKey::AiMissingKey),
            );
            let key = state.key.clone();
            key.update(cx, |input, cx| input.set_value("replacement", window, cx));
            assert!(app.begin_ai_connection_test(cx).is_none());
            assert_eq!(
                app.active_settings()
                    .unwrap()
                    .ai
                    .connection_test
                    .as_ref()
                    .unwrap()
                    .result
                    .as_ref()
                    .unwrap()
                    .as_ref()
                    .unwrap_err(),
                t(L10nKey::SettingsAiSaveReplacementFirst),
            );
            assert_eq!(key.read(cx).value(), "replacement");
            assert!(!cx.global::<Config>().ai.deepseek.enabled);
        });
    }

    #[gpui::test]
    fn connection_results_are_scoped_to_snapshot_request_and_mounted_page(cx: &mut TestAppContext) {
        let (app, mut vcx) = super::super::gpui_tests::harness(cx);
        let store = Arc::new(InMemoryCredentialStore::default());
        app.update_in(&mut vcx, |app, window, cx| {
            app.open_settings_section(SettingsSection::Ai, window, cx);
        });
        vcx.run_until_parked();
        app.update_in(&mut vcx, |app, window, cx| {
            let key = app.active_settings().unwrap().ai.key.clone();
            key.update(cx, |input, cx| input.set_value("saved-key", window, cx));
            app.ai_key_action_with_store(Some(true), store.clone(), window, cx);
        });
        vcx.run_until_parked();
        app.update_in(&mut vcx, |app, window, cx| {
            let (id, stale, _) = app.begin_ai_connection_test(cx).unwrap();
            assert!(!app.active_settings().unwrap().ai.busy);
            app.finish_ai_connection_test(id, stale, Ok(Duration::from_millis(8)), cx);
            assert!(!cx.global::<Config>().ai.deepseek.enabled);
            let (id, stale, _) = app.begin_ai_connection_test(cx).unwrap();
            app.set_deepseek_enabled(true, cx);
            app.finish_ai_connection_test(id, stale, Ok(Duration::from_millis(12)), cx);
            assert!(app.active_settings().unwrap().ai.connection_test.is_none());

            let (id, first, _) = app.begin_ai_connection_test(cx).unwrap();
            app.invalidate_ai_connection_test();
            let (_, second, _) = app.begin_ai_connection_test(cx).unwrap();
            app.finish_ai_connection_test(id, first, Ok(Duration::from_millis(12)), cx);
            assert!(
                app.active_settings()
                    .unwrap()
                    .ai
                    .connection_test
                    .as_ref()
                    .unwrap()
                    .result
                    .is_none()
            );
            app.finish_ai_connection_test(id, second, Ok(Duration::from_millis(24)), cx);
            assert_eq!(
                app.active_settings()
                    .unwrap()
                    .ai
                    .connection_test
                    .as_ref()
                    .unwrap()
                    .result,
                Some(Ok(Duration::from_millis(24))),
            );

            let (id, token, _) = app.begin_ai_connection_test(cx).unwrap();
            let model = app.active_settings().unwrap().ai.inputs[2].clone();
            model.update(cx, |input, cx| {
                input.set_value("new unsaved model", window, cx)
            });
            app.finish_ai_connection_test(id, token, Ok(Duration::from_millis(12)), cx);
            assert!(app.active_settings().unwrap().ai.connection_test.is_none());
            app.refresh_ai_controls(&[2], window, cx);

            let (id, token, _) = app.begin_ai_connection_test(cx).unwrap();
            app.navigate_settings(SettingsSection::Git, None, window, cx);
            app.finish_ai_connection_test(id, token, Ok(Duration::from_millis(12)), cx);
            assert!(app.active_settings().unwrap().ai.connection_test.is_none());
            app.navigate_settings(SettingsSection::Ai, None, window, cx);

            let (id, token, _) = app.begin_ai_connection_test(cx).unwrap();
            app.close_settings(window, cx);
            app.open_settings_section(SettingsSection::Ai, window, cx);
            app.finish_ai_connection_test(id, token, Ok(Duration::from_millis(12)), cx);
            assert!(app.active_settings().unwrap().ai.connection_test.is_none());
            assert!(!store.is_empty());
        });
    }

    #[gpui::test]
    fn rules_enter_inserts_newline_without_saving_or_losing_pending_text(cx: &mut TestAppContext) {
        let (app, mut vcx) = super::super::gpui_tests::harness(cx);
        app.update_in(&mut vcx, |app, window, cx| {
            app.open_settings_section(SettingsSection::Git, window, cx);
        });
        vcx.run_until_parked();
        app.update_in(&mut vcx, |app, window, cx| {
            let input = app.active_settings().unwrap().ai.inputs[3].clone();
            input.update(cx, |input, cx| {
                input.set_value("Keep titles short", window, cx)
            });
            let focus = input.read(cx).focus_handle(cx);
            window.focus(&focus, cx);
        });
        vcx.run_until_parked();
        vcx.simulate_keystrokes("enter");
        vcx.run_until_parked();
        app.update_in(&mut vcx, |app, _, cx| {
            assert!(
                app.active_settings().unwrap().ai.inputs[3]
                    .read(cx)
                    .value()
                    .contains('\n')
            );
            assert!(cx.global::<Config>().git_ai.style_rules.is_empty());
            app.set_deepseek_enabled(true, cx);
        });
        vcx.run_until_parked();
        app.update_in(&mut vcx, |app, _, cx| {
            let input = app.active_settings().unwrap().ai.inputs[3].clone();
            assert!(input.read(cx).value().contains("Keep titles short"));
            assert!(input.read(cx).value().contains('\n'));
            assert!(cx.global::<Config>().git_ai.style_rules.is_empty());
            input.update(cx, |_, cx| cx.emit(InputEvent::Blur));
        });
        vcx.run_until_parked();
        app.update_in(&mut vcx, |app, _, cx| {
            assert_eq!(
                cx.global::<Config>().git_ai.style_rules,
                app.active_settings().unwrap().ai.inputs[3]
                    .read(cx)
                    .value()
                    .as_str(),
            );
        });
    }
}
