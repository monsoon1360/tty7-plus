use super::*;
use crate::core::ai::{GitAiProvider, GitCommitLanguage};

impl Tty7App {
    pub(crate) fn render_settings_git(&self, cx: &mut Context<Self>) -> AnyElement {
        if self.active_settings().is_none() {
            return div().into_any_element();
        }
        let cfg = cx.global::<Config>();
        let settings = cfg.git_ai.clone();
        let codex_model = if cfg.ai.codex.model.is_empty() {
            t(L10nKey::SettingsAiDefault)
        } else {
            &cfg.ai.codex.model
        };
        let labels = [
            t(L10nKey::SettingsAiOff).to_string(),
            format!("DeepSeek API · {}", cfg.ai.deepseek.model),
            format!("Codex CLI · {codex_model}"),
        ];
        let labels = labels.iter().map(String::as_str).collect::<Vec<_>>();
        let provider = self.settings_choice(
            "git-ai-provider",
            &labels,
            match settings.provider {
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
                .position(|v| *v == settings.language)
                .unwrap_or(0),
            cx,
            move |this, i, _, cx| this.update_config(cx, |cfg| cfg.git_ai.language = values[i]),
        );
        let rules = self.active_settings().unwrap().ai.inputs[3].clone();
        let rules = self
            .settings_multiline_text_input(&rules, PATH_FIELD_W, cx)
            .into_any_element();
        Self::settings_page([self.settings_group(
            Some(t(L10nKey::SettingsGitAiTitle)),
            Some(t(L10nKey::SettingsGitAiDesc).into()),
            [
                self.settings_row(t(L10nKey::SettingsGitProvider), "", provider, cx)
                    .into_any_element(),
                self.settings_row(
                    t(L10nKey::SettingsGitLanguage),
                    t(L10nKey::SettingsGitLanguageDesc),
                    language,
                    cx,
                )
                .into_any_element(),
                self.settings_row(
                    t(L10nKey::SettingsGitRules),
                    t(L10nKey::SettingsGitRulesDesc),
                    rules,
                    cx,
                )
                .into_any_element(),
            ],
            cx,
        )])
    }
}
