use super::*;
use crate::core::ai::{GitAiProvider, GitCommitLanguage};
use gpui_component::ActiveTheme as _;

impl Tty7App {
    pub(crate) fn render_settings_git(&self, cx: &mut Context<Self>) -> AnyElement {
        if self.active_settings().is_none() {
            return div().into_any_element();
        }
        let cfg = cx.global::<Config>();
        let provider_value = cfg.git_ai.provider;
        let language_value = cfg.git_ai.language;
        let deepseek_disabled = !cfg.ai.deepseek.enabled;
        let codex_model = if cfg.ai.codex.model.is_empty() {
            t(L10nKey::SettingsAiDefault)
        } else {
            &cfg.ai.codex.model
        };
        let labels = [
            t(L10nKey::SettingsAiOff).to_string(),
            if deepseek_disabled {
                format!(
                    "DeepSeek API · {} · {}",
                    cfg.ai.deepseek.model,
                    t(L10nKey::SettingsAiDisabled)
                )
            } else {
                format!("DeepSeek API · {}", cfg.ai.deepseek.model)
            },
            format!("Codex CLI · {codex_model}"),
        ];
        let labels = labels.iter().map(String::as_str).collect::<Vec<_>>();
        let provider = self.settings_choice(
            "git-ai-provider",
            &labels,
            match provider_value {
                GitAiProvider::Off => 0,
                GitAiProvider::Deepseek => 1,
                GitAiProvider::CodexCli => 2,
            },
            cx,
            |this, i, _, cx| {
                this.update_config(cx, |cfg| {
                    cfg.git_ai.provider = [
                        GitAiProvider::Off,
                        GitAiProvider::Deepseek,
                        GitAiProvider::CodexCli,
                    ][i]
                })
            },
        );
        let muted = cx.theme().muted_foreground;
        let border = cx.theme().border;
        let provider = v_flex()
            .w(px(FIELD_W))
            .max_w_full()
            .gap(px(8.))
            .child(provider)
            .when(
                deepseek_disabled && provider_value == GitAiProvider::Deepseek,
                |col| {
                    col.child(
                        div()
                            .id("git-ai-deepseek-disabled")
                            .text_size(kit::fs(12.))
                            .text_color(muted)
                            .child(t(L10nKey::SettingsAiDisabled)),
                    )
                    .child(self.settings_button(
                        "git-ai-enable-deepseek",
                        t(L10nKey::SettingsGitEnableDeepSeek),
                        cx,
                        |this, window, cx| {
                            this.navigate_settings(
                                SettingsSection::Ai,
                                Some(L10nKey::SettingsDeepSeekEnabled),
                                window,
                                cx,
                            )
                        },
                    ))
                },
            )
            .into_any_element();
        let languages = [
            t(L10nKey::SettingsGitLanguageApplication),
            "English",
            "简体中文",
            "日本語",
            "Русский",
        ];
        let values = [
            GitCommitLanguage::Application,
            GitCommitLanguage::En,
            GitCommitLanguage::Zh,
            GitCommitLanguage::Ja,
            GitCommitLanguage::Ru,
        ];
        let language = self.settings_choice(
            "git-ai-language",
            &languages,
            values
                .iter()
                .position(|v| *v == language_value)
                .unwrap_or(0),
            cx,
            move |this, i, _, cx| this.update_config(cx, |cfg| cfg.git_ai.language = values[i]),
        );
        let rules = self.active_settings().unwrap().ai.inputs[3].clone();
        let rules = self
            .settings_multiline_text_input(&rules, READING_COLUMN, cx)
            .id("git-ai-rules")
            .w_full()
            .into_any_element();
        super::ai::settings_ai_card(
            "settings-git-ai-card",
            t(L10nKey::SettingsGitAiTitle),
            t(L10nKey::SettingsGitAiSummary),
            [
                self.settings_row(t(L10nKey::SettingsGitProvider), "", provider, cx)
                    .into_any_element(),
                self.settings_row(t(L10nKey::SettingsGitLanguage), "", language, cx)
                    .into_any_element(),
                self.settings_row(
                    t(L10nKey::SettingsGitRules),
                    t(L10nKey::SettingsGitRulesDesc),
                    rules,
                    cx,
                )
                .into_any_element(),
                div()
                    .id("git-ai-scope")
                    .mt(px(8.))
                    .pt(px(12.))
                    .border_t_1()
                    .border_color(border)
                    .text_size(kit::fs(12.))
                    .line_height(kit::fs(18.))
                    .text_color(muted)
                    .child(t(L10nKey::SettingsGitAiDesc))
                    .into_any_element(),
            ],
            cx,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[gpui::test]
    fn provider_and_language_share_a_control_column(cx: &mut gpui::TestAppContext) {
        let (app, mut vcx) = super::super::gpui_tests::harness(cx);
        app.update_in(&mut vcx, |app, window, cx| {
            let cfg = cx.global_mut::<Config>();
            cfg.ai.deepseek.enabled = true;
            cfg.git_ai.provider = GitAiProvider::Deepseek;
            cfg.git_ai.language = GitCommitLanguage::Zh;
            app.open_settings_section(SettingsSection::Git, window, cx);
        });
        for width in [960., 1100., 720.] {
            vcx.simulate_resize(gpui::size(px(width), px(800.)));
            vcx.run_until_parked();
            let provider = vcx
                .debug_bounds("git-ai-provider")
                .expect("provider is visible");
            let language = vcx
                .debug_bounds("git-ai-language")
                .expect("language is visible");
            assert_eq!(provider.origin.x, language.origin.x);
            assert_eq!(provider.size.width, language.size.width);
            assert!(
                provider.bottom() <= language.top(),
                "the controls must not overlap"
            );
        }
    }
}
