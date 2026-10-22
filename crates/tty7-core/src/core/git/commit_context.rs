//! Raw, read-only changes used to draft a commit message on any Host.
use std::fmt;
use std::io;
use std::path::Path;

use super::status::{StatusProbe, probe_status};
use crate::host::Host;

pub const MAX_COMMIT_CONTEXT_BYTES: usize = 24 * 1024;
const TRUNCATION_NOTE: &str =
    "\n[… commit changes truncated by tty7; generate only from the supplied changes]";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitContext {
    pub diff: String,
    pub truncated: bool,
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

/// Match the existing commit policy without staging, committing or reading
/// untracked files. Call this on HostOps, never on the GUI thread.
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
    } else if status.unstaged().next().is_some() {
        patch_diff(host, root, false)?
    } else {
        return Err(failure(CommitContextError::NoChanges));
    };
    if diff.trim().is_empty() {
        return Err(failure(CommitContextError::NoChanges));
    }
    let truncated = diff.len() > MAX_COMMIT_CONTEXT_BYTES;
    if truncated {
        let mut end = MAX_COMMIT_CONTEXT_BYTES - TRUNCATION_NOTE.len();
        while !diff.is_char_boundary(end) {
            end -= 1;
        }
        diff.truncate(end);
        diff.push_str(TRUNCATION_NOTE);
    }
    Ok(CommitContext { diff, truncated })
}

fn patch_diff(host: &dyn Host, root: &Path, cached: bool) -> io::Result<String> {
    let mut args = vec!["diff", "--no-ext-diff", "--no-textconv", "--no-color"];
    if cached {
        args.push("--cached");
    }
    args.push("--");
    patch(host, root, &args)
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
        let before = repo.git(&["write-tree"]);
        let context = repo.read(false).unwrap();
        assert!(context.diff.contains("+staged A"));
        assert!(!context.diff.contains("b.txt"));
        assert_eq!(repo.git(&["write-tree"]), before);
    }

    #[test]
    fn without_staged_changes_only_tracked_worktree_changes_are_sent() {
        let repo = Repo::new();
        repo.base();
        repo.write("b.txt", "tracked B\n");
        repo.write("secret.txt", "untracked secret\n");
        let context = repo.read(false).unwrap();
        assert!(context.diff.contains("+tracked B"));
        assert!(!context.diff.contains("secret"));
        assert!(repo.git(&["diff", "--cached"]).is_empty());
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
        let context = repo.read(true).unwrap();
        assert!(
            context
                .diff
                .contains("=== Staged increment on top of HEAD ===")
        );
        assert!(context.diff.contains("+staged increment"));
        assert!(!context.diff.contains("do not send unstaged"));
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
    fn clean_untracked_only_and_unborn_amend_are_rejected() {
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
        repo.write("untracked", "not sent");
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
