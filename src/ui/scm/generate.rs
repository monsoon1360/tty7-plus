//! Draft-only AI generation. Repository IO stays on HostOps; providers run locally.
use std::io;
use std::path::Path;
use std::sync::Arc;

use gpui::{Context, PromptButton, PromptLevel, Window};
use gpui_component::WindowExt as _;
use gpui_component::notification::Notification;
use tty7_core::core::git::commit_context::{CommitContextError, read_commit_context};
use tty7_core::core::git::status::{HeadState, StatusProbe, WorkingTreeStatus, probe_status};
use tty7_core::host::Host;

use crate::core::ai::GitAiProvider;
use crate::core::ai_completion::{
    AiCompletionError, CommitMessageRequest, generate_commit_message,
};
use crate::core::cli_agent::program_on_path;
use crate::core::config::{Config, config_dir_path};
use crate::core::keychain::OsCredentialStore;
use crate::terminal::git_data::status_of;
use crate::ui::app::Tty7App;
use crate::ui::host_ops::HostOps;
use crate::ui::host_registry::HostRegistry;
use crate::ui::i18n::{L10nKey, t};
use crate::ui::scm::state::{GenerationFinish, RepoKey};
use crate::ui::settings::SettingsSection;

/// Shared by the actual button and non-pointer action entrypoint.
pub(super) fn unavailable(
    status: &WorkingTreeStatus,
    amend: bool,
    generating: bool,
    committing: bool,
) -> Option<L10nKey> {
    if generating {
        return Some(L10nKey::ScmAiBusy);
    }
    if committing {
        return Some(L10nKey::ScmAiCommitting);
    }
    if status.conflicts().next().is_some() {
        return Some(L10nKey::ScmAiConflicts);
    }
    if amend {
        return (!status.head.has_commits()).then_some(L10nKey::ScmAiMissingHead);
    }
    (status.staged().next().is_none() && status.unstaged().next().is_none())
        .then_some(L10nKey::ScmAiNoChanges)
}

fn read_head(host: &dyn Host, root: &Path) -> io::Result<HeadState> {
    match probe_status(host, root) {
        StatusProbe::Status(status) => Ok(status.head),
        _ => Err(io::Error::other(t(L10nKey::ScmAiReadFailed))),
    }
}

fn generation_error_text(error: &anyhow::Error) -> String {
    let kind = error
        .downcast_ref::<io::Error>()
        .and_then(io::Error::get_ref)
        .and_then(|error| error.downcast_ref::<CommitContextError>());
    let key = match kind {
        Some(CommitContextError::NoChanges) => L10nKey::ScmAiNoChanges,
        Some(CommitContextError::Conflicts) => L10nKey::ScmAiConflicts,
        Some(CommitContextError::MissingHead) => L10nKey::ScmAiMissingHead,
        Some(CommitContextError::NotRepository) => L10nKey::PanelNotAGitRepo,
        Some(CommitContextError::StatusUnavailable | CommitContextError::GitFailed) => {
            L10nKey::ScmAiReadFailed
        }
        None => return error.to_string(),
    };
    t(key).to_owned()
}

impl Tty7App {
    pub(crate) fn scm_generate_commit_message(
        &mut self,
        repo: RepoKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if cx.global::<Config>().git_ai.provider == GitAiProvider::Off {
            self.open_settings_section(SettingsSection::Git, window, cx);
            return;
        }
        let Some(status) = status_of(cx, repo.host, &repo.root) else {
            return;
        };
        let committing = self
            .scm
            .committing
            .as_ref()
            .is_some_and(|(key, _, _)| key == &repo);
        if let Some(reason) = unavailable(
            &status,
            self.scm.amend,
            self.scm.generating.contains_key(&repo),
            committing,
        ) {
            window.push_notification(t(reason).to_string(), cx);
            return;
        }
        if cx.global::<Config>().git_ai.provider == GitAiProvider::CodexCli
            && !program_on_path("codex", &std::env::var_os("PATH").unwrap_or_default())
        {
            window.push_notification(Notification::error(t(L10nKey::ScmAiMissingCodex)), cx);
            self.open_settings_section(SettingsSection::Ai, window, cx);
            return;
        }
        let original = self.scm_message(&repo, cx);
        let amend = self.scm.amend;
        if !original.is_empty() {
            let answer = window.prompt(
                PromptLevel::Warning,
                t(L10nKey::ScmAiReplaceQuestion),
                None,
                &[
                    PromptButton::cancel(t(L10nKey::Cancel)),
                    PromptButton::new(t(L10nKey::ScmAiReplace)),
                ],
                cx,
            );
            cx.spawn_in(window, async move |app, cx| {
                if !matches!(answer.await, Ok(1)) {
                    return;
                }
                let _ = app.update_in(cx, |app, window, cx| {
                    app.scm_start_generation(repo, original, amend, window, cx);
                });
            })
            .detach();
        } else {
            self.scm_start_generation(repo, original, amend, window, cx);
        }
    }

    fn scm_start_generation(
        &mut self,
        repo: RepoKey,
        original: String,
        amend: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A dialog can outlive edits, a mode change or disconnect. It grants
        // permission to replace only the text it originally showed.
        if self.scm_message(&repo, cx) != original || self.scm.amend != amend {
            window.push_notification(t(L10nKey::ScmAiChanged).to_string(), cx);
            return;
        }
        let Some(host) = HostRegistry::get(cx, repo.host) else {
            return;
        };
        let Some(status) = status_of(cx, repo.host, &repo.root) else {
            return;
        };
        let committing = self
            .scm
            .committing
            .as_ref()
            .is_some_and(|(key, _, _)| key == &repo);
        if let Some(reason) = unavailable(
            &status,
            amend,
            self.scm.generating.contains_key(&repo),
            committing,
        ) {
            window.push_notification(t(reason).to_string(), cx);
            return;
        }
        let config = cx.global::<Config>();
        if config.git_ai.provider == GitAiProvider::Off {
            return;
        }
        let ai = config.ai.clone();
        let settings = config.git_ai.clone();
        let language = settings.language.resolve(&config.gui_language).to_owned();
        let Some(config_dir) = config_dir_path() else {
            window.push_notification(Notification::error(t(L10nKey::ScmAiReadFailed)), cx);
            self.open_settings_section(SettingsSection::Ai, window, cx);
            return;
        };
        let head = status.head.clone();
        let Some(token) = self
            .scm
            .begin_generation(repo.clone(), head.clone(), original, amend)
        else {
            return;
        };
        let root = repo.root.clone();
        let http = cx.http_client();
        let provider_host = host.clone();
        cx.notify();
        HostOps::run_in(
            host,
            window,
            cx,
            move |host| {
                if read_head(host, &root)? != head {
                    return Err(io::Error::other(t(L10nKey::ScmAiChanged)));
                }
                read_commit_context(host, &root, amend)
            },
            move |app, context, window, cx| {
                if !app
                    .scm
                    .generating
                    .get(&repo)
                    .is_some_and(|state| state.token == token)
                {
                    return;
                }
                let context = match context {
                    Ok(context) => context,
                    Err(error) => {
                        app.scm_generation_failed(&repo, token, error.into(), window, cx);
                        return;
                    }
                };
                let truncated = context.truncated;
                let request = CommitMessageRequest {
                    ai,
                    settings,
                    language,
                    context,
                    credentials: Arc::new(OsCredentialStore),
                    config_dir,
                };
                cx.spawn_in(window, async move |app, cx| {
                    let result = generate_commit_message(request, http).await;
                    let _ = app.update_in(cx, |app, window, cx| {
                        if !app
                            .scm
                            .generating
                            .get(&repo)
                            .is_some_and(|state| state.token == token)
                        {
                            return;
                        }
                        match result {
                            Err(error) => {
                                app.scm_generation_failed(&repo, token, error, window, cx)
                            }
                            Ok(message) => {
                                let root = repo.root.clone();
                                HostOps::run_in(
                                    provider_host,
                                    window,
                                    cx,
                                    move |host| read_head(host, &root),
                                    move |app, head, window, cx| {
                                        let head = match head {
                                            Ok(head) => head,
                                            Err(error) => {
                                                app.scm_generation_failed(
                                                    &repo,
                                                    token,
                                                    error.into(),
                                                    window,
                                                    cx,
                                                );
                                                return;
                                            }
                                        };
                                        app.scm_land_generation(
                                            repo, token, head, message, truncated, window, cx,
                                        );
                                    },
                                );
                            }
                        }
                    });
                })
                .detach();
            },
        );
    }

    fn scm_generation_failed(
        &mut self,
        repo: &RepoKey,
        token: u64,
        error: anyhow::Error,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self
            .scm
            .generating
            .get(repo)
            .is_some_and(|state| state.token == token)
        {
            return;
        }
        self.scm.generating.remove(repo);
        let configure = matches!(
            error.downcast_ref::<AiCompletionError>(),
            Some(
                AiCompletionError::MissingKey
                    | AiCompletionError::CodexMissing
                    | AiCompletionError::CodexLoginRequired(_)
            )
        );
        window.push_notification(
            Notification::error(generation_error_text(&error)).title(t(L10nKey::ScmAiFailed)),
            cx,
        );
        if configure {
            self.open_settings_section(SettingsSection::Ai, window, cx);
        }
        cx.notify();
    }

    pub(super) fn scm_land_generation(
        &mut self,
        repo: RepoKey,
        token: u64,
        head: HeadState,
        message: String,
        truncated: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let current = self.scm_message(&repo, cx);
        match self.scm.finish_generation(
            &repo,
            token,
            &head,
            self.scm.amend,
            &current,
            Some(message),
        ) {
            GenerationFinish::Applied => {
                if self.scm.commit_repo.as_ref() == Some(&repo) {
                    if let Some(input) = &self.scm.commit_input {
                        let message = self.scm.draft(&repo).to_owned();
                        input.update(cx, |input, cx| input.set_value(message, window, cx));
                    }
                }
                if truncated {
                    window.push_notification(t(L10nKey::ScmAiTruncated).to_string(), cx);
                }
            }
            GenerationFinish::Changed => {
                window.push_notification(t(L10nKey::ScmAiChanged).to_string(), cx)
            }
            GenerationFinish::Invalidated | GenerationFinish::Failed => {}
        }
        cx.notify();
    }
}
