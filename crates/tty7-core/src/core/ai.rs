//! Shared, framework-free AI settings and local Codex launch policy.
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AiSettings {
    pub codex: CodexSettings,
    pub deepseek: DeepSeekSettings,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CodexSettings {
    pub model: String,
    #[serde(deserialize_with = "crate::core::config::de_lenient")]
    pub reasoning_effort: Option<CodexReasoningEffort>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DeepSeekSettings {
    pub base_url: String,
    pub model: String,
    #[serde(deserialize_with = "crate::core::config::de_lenient")]
    pub reasoning_effort: Option<DeepSeekReasoningEffort>,
}

impl Default for DeepSeekSettings {
    fn default() -> Self {
        Self {
            base_url: "https://api.deepseek.com".into(),
            model: "deepseek-flash".into(),
            reasoning_effort: None,
        }
    }
}

macro_rules! effort {
    ($name:ident { $($variant:ident => $value:literal),+ $(,)? }) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
        pub enum $name { $(#[serde(rename = $value)] $variant),+ }
        impl $name {
            pub const ALL: &'static [Self] = &[$(Self::$variant),+];
            pub fn as_str(self) -> &'static str { match self { $(Self::$variant => $value),+ } }
        }
    };
}
effort!(CodexReasoningEffort {
    Minimal => "minimal", Low => "low", Medium => "medium", High => "high",
    Xhigh => "xhigh", Max => "max", Ultra => "ultra",
});
effort!(DeepSeekReasoningEffort { Off => "off", Low => "low", High => "high", Max => "max" });

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GitAiProvider {
    #[default]
    Off,
    Deepseek,
    CodexCli,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum GitCommitLanguage {
    #[default]
    #[serde(rename = "application")]
    Application,
    #[serde(rename = "en")]
    En,
    #[serde(rename = "zh")]
    Zh,
    #[serde(rename = "ja")]
    Ja,
    #[serde(rename = "ru")]
    Ru,
}

impl GitCommitLanguage {
    pub fn resolve(self, application: &str) -> &'static str {
        match self {
            Self::Application => match application.split(['-', '_']).next().unwrap_or("en") {
                "zh" => "Simplified Chinese",
                "ja" => "Japanese",
                "ru" => "Russian",
                _ => "English",
            },
            Self::En => "English",
            Self::Zh => "Simplified Chinese",
            Self::Ja => "Japanese",
            Self::Ru => "Russian",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GitAiSettings {
    #[serde(deserialize_with = "crate::core::config::de_lenient")]
    pub provider: GitAiProvider,
    #[serde(deserialize_with = "crate::core::config::de_lenient")]
    pub language: GitCommitLanguage,
    pub style_rules: String,
}

/// Validate without including a possibly credential-bearing URL in errors.
pub fn normalize_deepseek_base_url(input: &str) -> io::Result<String> {
    let input = input.trim().trim_end_matches('/');
    let url = url::Url::parse(input)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "Invalid API URL"))?;
    let has_userinfo = input.split_once("://").is_some_and(|(_, rest)| {
        rest.split(['/', '\\', '?', '#'])
            .next()
            .is_some_and(|authority| authority.contains('@'))
    });
    let loopback = match url.host() {
        Some(url::Host::Domain(name)) => name.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    };
    if url.host().is_none()
        || has_userinfo
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || !(url.scheme() == "https" || (url.scheme() == "http" && loopback))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Use HTTPS (or loopback HTTP), without credentials, query or fragment",
        ));
    }
    Ok(url.as_str().trim_end_matches('/').to_owned())
}

pub fn deepseek_key_account(config_dir: &Path) -> io::Result<String> {
    let path = config_dir.canonicalize()?;
    let path = path.to_str().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "Configuration directory is not UTF-8",
        )
    })?;
    use std::fmt::Write as _;
    let mut account = String::with_capacity(73);
    account.push_str("deepseek:");
    for byte in Sha256::digest(path.as_bytes()) {
        write!(&mut account, "{byte:02x}").expect("writing to String");
    }
    Ok(account)
}

static LOCAL_CODEX_LAUNCHER: OnceLock<PathBuf> = OnceLock::new();
pub fn register_local_codex_launcher(path: PathBuf) {
    let _ = LOCAL_CODEX_LAUNCHER.set(path);
}
pub fn local_codex_launcher() -> Option<&'static Path> {
    LOCAL_CODEX_LAUNCHER.get().map(PathBuf::as_path)
}

/// Read only the projection: never migrate, quarantine or rewrite configuration.
pub fn read_codex_defaults(path: &Path) -> io::Result<CodexSettings> {
    #[derive(Default, Deserialize)]
    #[serde(default)]
    struct Projection {
        ai: CodexProjection,
    }
    #[derive(Default, Deserialize)]
    #[serde(default)]
    struct CodexProjection {
        codex: CodexSettings,
    }
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(CodexSettings::default());
        }
        Err(error) => return Err(error),
    };
    let bytes = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(&bytes);
    serde_json::from_slice::<Projection>(bytes)
        .map(|p| p.ai.codex)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

/// Only missing defaults, to prepend to the *unchanged* original argv.
/// Unknown option arity disables injection rather than guessing at its value.
pub fn codex_default_args(args: &[OsString], defaults: &CodexSettings) -> Vec<OsString> {
    let mut model = false;
    let mut effort = false;
    let mut positional = false;
    let mut index = 0;
    while index < args.len() {
        let Some(arg) = args[index].to_str() else {
            positional = true;
            index += 1;
            continue;
        };
        if arg == "--" {
            break;
        }
        if matches!(arg, "--help" | "-h" | "--version" | "-V" | "--remote")
            || arg.starts_with("--remote=")
        {
            return Vec::new();
        }
        let mut config = None;
        if matches!(arg, "-m" | "--model" | "-c" | "--config") {
            index += 1;
            let value = args.get(index).and_then(|v| v.to_str());
            if matches!(arg, "-m" | "--model") {
                model = true;
            } else {
                config = value;
            }
        } else if arg.starts_with("--model=") || (arg.starts_with("-m") && arg.len() > 2) {
            model = true;
        } else if let Some(value) = arg
            .strip_prefix("--config=")
            .or_else(|| arg.strip_prefix("-c="))
            .or_else(|| arg.strip_prefix("-c").filter(|v| !v.is_empty()))
        {
            config = Some(value);
        } else if option_takes_value(arg) {
            index += 1;
        } else if arg == "-" {
            // Codex exec reads its prompt from stdin for this positional.
            positional = true;
        } else if arg.starts_with('-') {
            let attached_value = arg.len() > 2
                && ["-i", "-p", "-s", "-C", "-a", "-o"]
                    .iter()
                    .any(|prefix| arg.starts_with(prefix));
            if !attached_value
                && !flag_without_value(arg)
                && !(arg.starts_with("--") && arg.contains('='))
            {
                return Vec::new();
            }
        } else if !positional {
            positional = true;
            match arg {
                "exec" | "e" | "review" | "resume" | "fork" => {}
                "agents" | "login" | "logout" | "mcp" | "plugin" | "app-server"
                | "remote-control" | "app" | "completion" | "update" | "doctor" | "sandbox"
                | "debug" | "apply" | "a" | "queue" | "archive" | "delete" | "migrate-rollouts"
                | "unarchive" | "cloud" | "exec-server" | "features" | "help" => return Vec::new(),
                _ => {}
            }
        } else {
            // Prompt/session identifiers are positional; options after them are
            // still accepted by clap, but a single prompt token is never parsed.
        }
        if let Some((key, _)) = config.and_then(|v| v.split_once('=')) {
            match key.trim() {
                "model" => model = true,
                "model_reasoning_effort" => effort = true,
                _ => {}
            }
        }
        index += 1;
    }
    let mut result = Vec::new();
    if !model && !defaults.model.trim().is_empty() {
        result.extend([
            OsString::from("--model"),
            OsString::from(defaults.model.trim()),
        ]);
    }
    if !effort && let Some(value) = defaults.reasoning_effort {
        result.extend([
            OsString::from("-c"),
            OsString::from(format!("model_reasoning_effort={}", value.as_str())),
        ]);
    }
    result
}

fn option_takes_value(arg: &str) -> bool {
    matches!(
        arg,
        "-i" | "--image"
            | "-p"
            | "--profile"
            | "-s"
            | "--sandbox"
            | "-C"
            | "--cd"
            | "-a"
            | "--ask-for-approval"
            | "--enable"
            | "--disable"
            | "--local-provider"
            | "--add-dir"
            | "--remote-auth-token-env"
            | "-o"
            | "--output-last-message"
            | "--output-schema"
            | "--color"
            | "--config-profile"
            | "--base"
            | "--commit"
            | "--title"
    )
}
fn flag_without_value(arg: &str) -> bool {
    matches!(
        arg,
        "--oss"
            | "--approve-for-me"
            | "--dangerously-bypass-approvals-and-sandbox"
            | "--dangerously-bypass-hook-trust"
            | "--worktree"
            | "--search"
            | "--no-alt-screen"
            | "--no-daemon"
            | "--strict-config"
            | "--last"
            | "--all"
            | "--skip-git-repo-check"
            | "--ephemeral"
            | "--json"
            | "--uncommitted"
            | "--full-auto"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    fn args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }
    fn defaults() -> CodexSettings {
        CodexSettings {
            model: " smoke-model ".into(),
            reasoning_effort: Some(CodexReasoningEffort::High),
        }
    }

    #[test]
    fn exec_stdin_marker_preserves_model_and_effort_defaults() {
        assert_eq!(
            codex_default_args(&args(&["exec", "--sandbox", "read-only", "-"]), &defaults()),
            args(&[
                "--model",
                "smoke-model",
                "-c",
                "model_reasoning_effort=high"
            ]),
        );
    }

    #[test]
    fn missing_defaults_are_independent_and_preserve_explicit_forms() {
        let both = args(&[
            "--model",
            "smoke-model",
            "-c",
            "model_reasoning_effort=high",
        ]);
        assert_eq!(codex_default_args(&[], &defaults()), both);
        assert!(codex_default_args(&[], &CodexSettings::default()).is_empty());
        for model in [
            vec!["-m", "explicit"],
            vec!["-mexplicit"],
            vec!["--model", "explicit"],
            vec!["--model=explicit"],
            vec!["-c", "model='explicit'"],
            vec!["--config", "model=explicit"],
            vec!["--config=model=explicit"],
            vec!["-cmodel=explicit"],
            vec!["-c=model=explicit"],
        ] {
            assert_eq!(
                codex_default_args(&args(&model), &defaults()),
                args(&["-c", "model_reasoning_effort=high"]),
                "{model:?}"
            );
        }
        for config in [
            vec!["-c", "model_reasoning_effort=low"],
            vec!["--config", "model_reasoning_effort=low"],
            vec!["--config=model_reasoning_effort=low"],
            vec!["-cmodel_reasoning_effort=low"],
            vec!["-c=model_reasoning_effort=low"],
        ] {
            assert_eq!(
                codex_default_args(&args(&config), &defaults()),
                args(&["--model", "smoke-model"])
            );
        }
        assert_eq!(
            codex_default_args(
                &args(&["prompt with spaces", "--model", "explicit"]),
                &defaults()
            ),
            args(&["-c", "model_reasoning_effort=high"])
        );
        assert_eq!(
            codex_default_args(
                &args(&["-p--model=profile", "--model=explicit"]),
                &defaults()
            ),
            args(&["-c", "model_reasoning_effort=high"])
        );
        for argv in [
            vec!["--", "--model=prompt"],
            vec!["--image", "--model=filename"],
            vec!["prompt with --model=x and -c model_reasoning_effort=low"],
            vec!["resume", "session"],
            vec!["fork", "--last"],
            vec!["exec", "a prompt"],
            vec!["review", "--uncommitted"],
        ] {
            assert_eq!(
                codex_default_args(&args(&argv), &defaults()),
                both,
                "{argv:?}"
            );
        }
        for command in [
            "login",
            "logout",
            "completion",
            "mcp",
            "app-server",
            "features",
            "help",
            "doctor",
            "--help",
            "--version",
        ] {
            assert!(codex_default_args(&args(&[command]), &defaults()).is_empty());
        }
    }
    #[test]
    fn lenient_enums_and_old_configuration() {
        let config: crate::core::config::Config = serde_json::from_str("{}").unwrap();
        assert_eq!(config.ai, AiSettings::default());
        let ai: AiSettings =
            serde_json::from_str(r#"{"codex":{"reasoning_effort":"future"}}"#).unwrap();
        assert_eq!(ai.codex.reasoning_effort, None);
        let git: GitAiSettings =
            serde_json::from_str(r#"{"provider":"future","language":"future"}"#).unwrap();
        assert_eq!(git, GitAiSettings::default());
    }
    #[test]
    fn projection_never_rewrites_bad_json_and_accepts_bom() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        assert_eq!(
            read_codex_defaults(&path).unwrap(),
            CodexSettings::default()
        );
        std::fs::write(&path, b"{bad").unwrap();
        assert_eq!(
            read_codex_defaults(&path).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"{bad");
        std::fs::write(
            &path,
            b"\xef\xbb\xbf{\"ai\":{\"codex\":{\"model\":\"local\"}}}",
        )
        .unwrap();
        assert_eq!(read_codex_defaults(&path).unwrap().model, "local");
    }
    #[test]
    fn base_url_rejects_credential_leaks_and_allows_local_gateway() {
        for value in [
            "http://example.com",
            "https://user@example.com",
            "https://@example.com",
            "https://example.com?key=x",
            "https://example.com/#x",
        ] {
            assert!(normalize_deepseek_base_url(value).is_err());
        }
        for value in [
            "https://api.deepseek.com/v1",
            "http://127.0.0.1:1234",
            "http://[::1]:1234",
            "http://localhost:1234",
        ] {
            assert_eq!(
                normalize_deepseek_base_url(&format!(" {value}/ ")).unwrap(),
                value
            );
        }
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            deepseek_key_account(dir.path()).unwrap(),
            deepseek_key_account(&dir.path().join(".")).unwrap()
        );
    }
}
