//! Raw, read-only changes used to draft a commit message on any Host.
use std::fmt;
use std::io;
use std::path::Path;

use super::status::{HeadState, StatusProbe, probe_status};
use crate::host::Host;

pub const MAX_COMMIT_CONTEXT_BYTES: usize = 24 * 1024;
const TRUNCATION_NOTE: &str =
    "\n[… commit changes truncated by tty7; generate only from the supplied changes]";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitContext {
    pub diff: String,
    pub truncated: bool,
    pub head: HeadState,
}

/// Kept typed inside io::Error so desktop callers can localize the failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommitContextError {
    NoChanges,
    Conflicts,
    MissingHead,
    NotRepository,
    StatusUnavailable,
    GitFailed,
}

impl fmt::Display for CommitContextError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NoChanges => "No commit changes are available for generation",
            Self::Conflicts => "Resolve conflicts before generating a commit message",
            Self::MissingHead => "Cannot amend a repository without a HEAD commit",
            Self::NotRepository => "The directory is not a Git repository",
            Self::StatusUnavailable => "Could not read the complete Git status",
            Self::GitFailed => "Could not read the commit changes",
        })
    }
}
impl std::error::Error for CommitContextError {}

fn failure(kind: CommitContextError) -> io::Error {
    io::Error::new(io::ErrorKind::Other, kind)
}

/// Match the commit policy without changing the index. With no explicit staged
/// selection, include tracked edits and non-ignored new files, like Commit All.
pub fn read_commit_context(host: &dyn Host, root: &Path, amend: bool) -> io::Result<CommitContext> {
    let status = match probe_status(host, root) {
        StatusProbe::Status(status) => status,
        StatusProbe::NotARepo => return Err(failure(CommitContextError::NotRepository)),
        StatusProbe::Unreachable => return Err(failure(CommitContextError::StatusUnavailable)),
    };
    if status.conflicts().next().is_some() {
        return Err(failure(CommitContextError::Conflicts));
    }
    // A truncated status might hide conflicts or staged entries. Never send a
    // patch selected using an incomplete picture of what commit would do.
    if status.truncated {
        return Err(failure(CommitContextError::StatusUnavailable));
    }
    let staged = status.staged().next().is_some();
    let mut omitted = false;
    let mut diff = if amend {
        if !status.head.has_commits() {
            return Err(failure(CommitContextError::MissingHead));
        }
        let patch = patch(
            host,
            root,
            &[
                "show",
                "--format=",
                "--root",
                "--first-parent",
                "--no-ext-diff",
                "--no-textconv",
                "--no-color",
                "HEAD",
                "--",
            ],
        )?;
        let mut combined = String::from("=== Current HEAD commit patch ===\n");
        combined.push_str(&patch);
        if staged {
            let increment = patch_diff(host, root, true)?;
            combined.push_str("\n=== Staged increment on top of HEAD ===\n");
            combined.push_str(&increment);
            if patch.trim().is_empty() && increment.trim().is_empty() {
                return Err(failure(CommitContextError::NoChanges));
            }
        } else if patch.trim().is_empty() {
            return Err(failure(CommitContextError::NoChanges));
        }
        combined
    } else if staged {
        patch_diff(host, root, true)?
    } else {
        let mut combined = if status.unstaged().next().is_some() {
            patch_diff(host, root, false)?
        } else {
            String::new()
        };
        for entry in status.untracked() {
            if combined.len() >= MAX_COMMIT_CONTEXT_BYTES {
                omitted = true;
                break;
            }
            if entry.path.lossy {
                return Err(failure(CommitContextError::StatusUnavailable));
            }
            let patch = untracked_patch(host, root, entry.path.as_str())?;
            if !combined.is_empty() && !patch.is_empty() {
                combined.push('\n');
            }
            combined.push_str(&patch);
        }
        combined
    };
    if diff.trim().is_empty() {
        return Err(failure(CommitContextError::NoChanges));
    }
    let truncated = omitted || diff.len() > MAX_COMMIT_CONTEXT_BYTES;
    if truncated {
        let mut end = diff
            .len()
            .min(MAX_COMMIT_CONTEXT_BYTES - TRUNCATION_NOTE.len());
        while !diff.is_char_boundary(end) {
            end -= 1;
        }
        diff.truncate(end);
        diff.push_str(TRUNCATION_NOTE);
    }
    Ok(CommitContext {
        diff,
        truncated,
        head: status.head,
    })
}

fn patch_diff(host: &dyn Host, root: &Path, cached: bool) -> io::Result<String> {
    let mut args = vec!["diff", "--no-ext-diff", "--no-textconv", "--no-color"];
    if cached {
        args.push("--cached");
    }
    args.push("--");
    patch(host, root, &args)
}

fn untracked_patch(host: &dyn Host, root: &Path, path: &str) -> io::Result<String> {
    let out = host.git(
        root,
        &[
            "-c",
            "core.quotePath=false",
            "diff",
            "--no-index",
            "--no-ext-diff",
            "--no-textconv",
            "--no-color",
            "--",
            "/dev/null",
            path,
        ],
    )?;
    // --no-index uses 1 to report a difference, not a failed command.
    if !matches!(out.status, Some(0 | 1)) {
        return Err(failure(CommitContextError::GitFailed));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn patch(host: &dyn Host, root: &Path, args: &[&str]) -> io::Result<String> {
    let mut full = vec!["-c", "core.quotePath=false"];
    full.extend_from_slice(args);
    let out = host.git(root, &full)?;
    if !out.success() {
        return Err(failure(CommitContextError::GitFailed));
    }
    // Git permits non-UTF8 file contents. The context is text; unlike paths in
    // mutation APIs, decoding lossily here cannot act on the wrong file.
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::git::test_support::{PINS, pin_repo_config};
    use crate::host::{SharedHost, local::LocalHost};

    struct Repo {
        dir: tempfile::TempDir,
        host: SharedHost,
    }
    impl Repo {
        fn new() -> Self {
            let repo = Self {
                dir: tempfile::tempdir().unwrap(),
                host: LocalHost::new(),
            };
            repo.git(&["init", "--quiet"]);
            assert!(pin_repo_config(repo.dir.path()));
            repo.git(&["symbolic-ref", "HEAD", "refs/heads/main"]);
            repo
        }
        fn git(&self, args: &[&str]) -> String {
            let mut full = PINS.to_vec();
            full.extend_from_slice(args);
            let out = self.host.git(self.dir.path(), &full).unwrap();
            assert!(out.success(), "git {args:?}: {}", out.stderr_trimmed());
            out.stdout_trimmed()
        }
        fn write(&self, path: &str, text: &str) {
            std::fs::write(self.dir.path().join(path), text).unwrap();
        }
        fn base(&self) {
            self.write("a.txt", "base A\n");
            self.write("b.txt", "base B\n");
            self.git(&["add", "--", "a.txt", "b.txt"]);
            self.git(&["commit", "--quiet", "-m", "base"]);
        }
        fn read(&self, amend: bool) -> io::Result<CommitContext> {
            read_commit_context(&*self.host, self.dir.path(), amend)
        }
    }

    fn error_kind(error: &io::Error) -> CommitContextError {
        *error
            .get_ref()
            .unwrap()
            .downcast_ref::<CommitContextError>()
            .unwrap()
    }

    #[test]
    fn staged_changes_exclude_unstaged_and_leave_index_untouched() {
        let repo = Repo::new();
        repo.base();
        repo.write("a.txt", "staged A\n");
        repo.git(&["add", "a.txt"]);
        repo.write("b.txt", "unstaged B\n");
        repo.write("new.txt", "unselected new file\n");
        let before = repo.git(&["write-tree"]);
        let context = repo.read(false).unwrap();
        assert!(context.diff.contains("+staged A"));
        assert!(!context.diff.contains("b.txt"));
        assert!(!context.diff.contains("unselected new file"));
        assert_eq!(repo.git(&["write-tree"]), before);
    }

    #[test]
    fn commit_all_context_and_commit_cover_new_modified_deleted_but_not_ignored_files() {
        use super::super::ops::{GitOp, run_op};
        let repo = Repo::new();
        repo.base();
        repo.write(".git/info/exclude", "*.ignored\n");
        repo.write("a.txt", "updated A\n");
        std::fs::remove_file(repo.dir.path().join("b.txt")).unwrap();
        repo.write("-new file 中文.txt", "new contents\n");
        repo.write("secret.ignored", "must not be sent or committed\n");
        let before = repo.git(&["write-tree"]);
        let context = repo.read(false).unwrap();
        assert!(context.diff.contains("+updated A"));
        assert!(context.diff.contains("-base B"));
        assert!(context.diff.contains("+new contents"));
        assert!(!context.diff.contains("must not be sent"));
        assert_eq!(
            repo.git(&["write-tree"]),
            before,
            "generation must not stage"
        );
        run_op(
            &*repo.host,
            repo.dir.path(),
            &GitOp::Commit {
                message: "Include all changes".into(),
                amend: false,
                signoff: false,
                no_verify: false,
                all: true,
            },
            &context.head,
        )
        .unwrap();
        assert_eq!(repo.git(&["show", "HEAD:a.txt"]), "updated A");
        assert_eq!(
            repo.git(&["show", "HEAD:-new file 中文.txt"]),
            "new contents"
        );
        let files = repo.git(&["ls-tree", "-r", "--name-only", "HEAD"]);
        assert!(
            !files
                .lines()
                .any(|path| path == "b.txt" || path == "secret.ignored")
        );
        assert!(repo.git(&["status", "--porcelain"]).is_empty());
    }

    #[test]
    fn new_files_can_generate_and_create_the_first_commit_without_staging() {
        use super::super::ops::{GitOp, run_op};
        let repo = Repo::new();
        repo.write("first.txt", "first commit\n");
        let index = repo.dir.path().join(".git/index");
        let before = std::fs::read(&index).ok();
        let context = repo.read(false).unwrap();
        assert!(context.diff.contains("+first commit"));
        assert!(!context.head.has_commits());
        assert_eq!(std::fs::read(&index).ok(), before);
        run_op(
            &*repo.host,
            repo.dir.path(),
            &GitOp::Commit {
                message: "Create first file".into(),
                amend: false,
                signoff: false,
                no_verify: false,
                all: true,
            },
            &context.head,
        )
        .unwrap();
        assert_eq!(repo.git(&["show", "HEAD:first.txt"]), "first commit");
        assert_eq!(repo.git(&["rev-list", "--count", "HEAD"]), "1");
    }

    #[test]
    fn explicit_staged_selection_does_not_sweep_new_or_unstaged_files() {
        use super::super::ops::{GitOp, run_op};
        // A stale/capped GUI snapshot may ask for Commit All after the user
        // selected files. The executor must honor the current index either way.
        for all in [false, true] {
            let repo = Repo::new();
            repo.base();
            repo.write("a.txt", "selected change\n");
            repo.git(&["add", "a.txt"]);
            repo.write("b.txt", "unselected edit\n");
            repo.write("new.txt", "unselected new\n");
            let context = repo.read(false).unwrap();
            run_op(
                &*repo.host,
                repo.dir.path(),
                &GitOp::Commit {
                    message: "Commit selected file".into(),
                    amend: false,
                    signoff: false,
                    no_verify: false,
                    all,
                },
                &context.head,
            )
            .unwrap();
            assert_eq!(repo.git(&["show", "HEAD:a.txt"]), "selected change");
            assert_eq!(repo.git(&["show", "HEAD:b.txt"]), "base B");
            assert!(
                !repo
                    .git(&["ls-tree", "-r", "--name-only", "HEAD"])
                    .lines()
                    .any(|p| p == "new.txt")
            );
            assert!(repo.git(&["diff", "--cached"]).is_empty());
        }
    }

    #[test]
    fn failed_automatic_staging_does_not_run_commit() {
        use super::super::ops::{GitOp, GitOpErrorKind, run_op};
        let repo = Repo::new();
        repo.base();
        let head = repo.git(&["rev-parse", "HEAD"]);
        repo.write("new.txt", "not committed\n");
        let context = repo.read(false).unwrap();
        repo.write(".git/index.lock", "");
        let error = run_op(
            &*repo.host,
            repo.dir.path(),
            &GitOp::Commit {
                message: "Must not land".into(),
                amend: false,
                signoff: false,
                no_verify: false,
                all: true,
            },
            &context.head,
        )
        .unwrap_err();
        assert_eq!(error.kind, GitOpErrorKind::LockHeld);
        assert_eq!(repo.git(&["rev-parse", "HEAD"]), head);
    }

    #[test]
    fn amend_reads_root_head_and_staged_increment_but_not_worktree() {
        let repo = Repo::new();
        repo.base();
        let head = repo.git(&["rev-parse", "HEAD"]);
        let context = repo.read(true).unwrap();
        assert!(context.diff.contains("+base A"));
        assert!(context.diff.contains("+base B"));
        repo.write("a.txt", "staged increment\n");
        repo.git(&["add", "a.txt"]);
        repo.write("b.txt", "do not send unstaged\n");
        repo.write("new.txt", "do not send untracked\n");
        let context = repo.read(true).unwrap();
        assert!(
            context
                .diff
                .contains("=== Staged increment on top of HEAD ===")
        );
        assert!(context.diff.contains("+staged increment"));
        assert!(!context.diff.contains("do not send unstaged"));
        assert!(!context.diff.contains("do not send untracked"));
        assert_eq!(repo.git(&["rev-parse", "HEAD"]), head);
    }

    #[test]
    fn amend_of_nonroot_commit_uses_only_current_commit_patch() {
        let repo = Repo::new();
        repo.base();
        repo.write("a.txt", "second commit\n");
        repo.git(&["add", "a.txt"]);
        repo.git(&["commit", "--quiet", "-m", "second"]);
        let context = repo.read(true).unwrap();
        assert!(context.diff.contains("+second commit"));
        assert!(!context.diff.contains("b.txt"));
    }

    #[test]
    fn clean_changes_and_unborn_amend_are_rejected() {
        let repo = Repo::new();
        assert_eq!(
            error_kind(&repo.read(true).unwrap_err()),
            CommitContextError::MissingHead
        );
        repo.base();
        assert_eq!(
            error_kind(&repo.read(false).unwrap_err()),
            CommitContextError::NoChanges
        );
    }

    #[test]
    fn unresolved_conflicts_are_rejected() {
        let repo = Repo::new();
        repo.base();
        repo.git(&["checkout", "-b", "other"]);
        repo.write("a.txt", "other branch\n");
        repo.git(&["add", "a.txt"]);
        repo.git(&["commit", "--quiet", "-m", "other"]);
        repo.git(&["checkout", "main"]);
        repo.write("a.txt", "main branch\n");
        repo.git(&["add", "a.txt"]);
        repo.git(&["commit", "--quiet", "-m", "main"]);
        let out = repo
            .host
            .git(repo.dir.path(), &["merge", "--no-edit", "other"])
            .unwrap();
        assert!(!out.success());
        for amend in [false, true] {
            assert_eq!(
                error_kind(&repo.read(amend).unwrap_err()),
                CommitContextError::Conflicts
            );
        }
    }

    #[test]
    fn chinese_patch_is_capped_including_note_at_a_utf8_boundary() {
        let repo = Repo::new();
        repo.base();
        repo.write("a.txt", &"中文变更\n".repeat(6000));
        let context = repo.read(false).unwrap();
        assert!(context.truncated);
        assert!(context.diff.len() <= MAX_COMMIT_CONTEXT_BYTES);
        assert!(context.diff.ends_with(TRUNCATION_NOTE));
        assert!(!context.diff.contains('\u{fffd}'));
    }

    #[test]
    fn binary_patch_keeps_summary_without_sending_binary_payload() {
        let repo = Repo::new();
        repo.base();
        std::fs::write(repo.dir.path().join("a.txt"), b"\0binary payload").unwrap();
        let context = repo.read(false).unwrap();
        assert!(context.diff.contains("Binary files"));
        assert!(!context.diff.contains("binary payload"));
    }
}
