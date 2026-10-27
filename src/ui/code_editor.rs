use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui::prelude::*;
use gpui::{
    AnyElement, Context, Entity, EntityInputHandler as _, Focusable as _, PromptLevel,
    SharedString, Subscription, Window, div, px, rems,
};
use gpui_component::button::Button;
use gpui_component::input::{Input, InputEvent, InputState, Position, RopeExt as _, TabSize};
use gpui_component::menu::{ContextMenuExt as _, PopupMenu, PopupMenuItem};
use gpui_component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, WindowExt as _, h_flex, v_flex,
};
use tty7_core::core::machine::TabId;

use crate::ui::app::{TITLE_BAR_BORDER, TitleStripRow as _, Tty7App};
use crate::ui::document_column::DocumentChrome;
use crate::ui::editor_session::{self, TabEditor};
use crate::ui::editor_text::{self, EditorConfig, Indent, LineEnding, TextFormat};
use crate::ui::host_ops::{HostId, HostOps, MTime, SharedHost, WatchSub};
use crate::ui::i18n::{L10nKey, t, t_fmt};

mod gutter;
mod nav;
pub(crate) mod outline;
mod problems;
mod strip;

pub(crate) use nav::KEY_CONTEXT as NAV_KEY_CONTEXT;

const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;

const RELOAD_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(200);

/// How far up the tree to look for `.editorconfig` files. Each level is one
/// round trip on a remote host, and a project nested deeper than this without
/// a `root = true` somewhere above it is not one worth stalling an open for.
const EDITORCONFIG_DEPTH: usize = 16;

/// An open buffer is named by its input's entity id: unique, stable for the
/// buffer's life, and already what every async landing looks it up by.
pub(crate) type BufferId = gpui::EntityId;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct ExternalEditorTab {
    pub path: PathBuf,
    pub tab: TabId,
    pub terminal: Option<TabId>,
    /// The editor this session runs — `hx`, `vim` — as it was launched. The
    /// record carries it so a later file-open asks the right editor without
    /// having to re-read the pane.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub program: String,
}

/// What a buffer's text looked like the last time it matched the disk.
///
/// Dirtiness is a comparison against this rather than a flag set by the first
/// keystroke, so typing a character and deleting it again — or undoing back to
/// the saved text — leaves the file clean, the way every other editor does.
/// The length is checked first: it is free, and it settles almost every
/// keystroke without hashing the whole file.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Fingerprint {
    len: usize,
    hash: u64,
}

impl Fingerprint {
    fn of_str(text: &str) -> Self {
        Self::of_chunks(text.len(), std::iter::once(text))
    }

    fn of_chunks<'a>(len: usize, chunks: impl Iterator<Item = &'a str>) -> Self {
        use std::hash::Hasher as _;
        // SipHash buffers its input, so the same bytes hash the same however
        // the rope happens to have split them into chunks.
        let mut hasher = std::hash::DefaultHasher::new();
        for chunk in chunks {
            hasher.write(chunk.as_bytes());
        }
        Self {
            len,
            hash: hasher.finish(),
        }
    }
}

/// Why the buffer and the file on disk no longer agree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DiskConflict {
    /// Something else wrote the file while the buffer had edits of its own.
    /// Carries the modification time that was seen, so "Keep mine" can accept
    /// exactly that version as the one a save may replace.
    Changed(Option<MTime>),
    /// The file is gone. The buffer is all that is left of it.
    Deleted,
}

/// One open file, shared by every tab that shows it.
///
/// Buffers used to belong to a tab, which meant the same file open in two tabs
/// was two independent copies: edits in one were invisible to the other, and
/// saving either silently threw the other's away. Now a tab only lists which
/// buffers it shows, and the text lives here once.
pub(crate) struct OpenFile {
    pub(crate) path: PathBuf,
    /// Set for a file that has never been saved; `path` is empty until then.
    untitled: Option<u32>,
    /// The machine `path` lives on, held rather than looked up. Saves,
    /// reloads and duplicate detection all key on its id — an SFTP file and a
    /// local file can share the string `/etc/hosts` without being the same
    /// file — and saving goes straight through this handle, so a buffer stays
    /// saveable however the window's own machine has changed underneath it.
    pub(crate) host: SharedHost,
    pub(crate) input: Entity<InputState>,
    pub(crate) dirty: bool,
    saved: Fingerprint,
    /// How the bytes on disk are encoded, so a save writes them back the same
    /// way — a CRLF file stays CRLF, a GB18030 file stays GB18030.
    format: TextFormat,
    saved_format: TextFormat,
    config: EditorConfig,
    indent: Indent,
    disk_mtime: Option<MTime>,
    saving: bool,
    save_pending: bool,
    save_then_close: bool,
    reload_seq: u64,
    pub(crate) conflict: Option<DiskConflict>,
    pub(crate) preview: bool,
    pub(crate) wrap: bool,
    /// The rendered-Markdown pane's own scroll. Per file, so switching away
    /// and back lands where you were reading — and so the pane can carry the
    /// scrollbar every other scrolling surface in tty7 has. The editor itself
    /// gets one from `Input`.
    pub(crate) preview_scroll: gpui::ScrollHandle,
    /// Git change markers — see `gutter`.
    gutter: gutter::BufferGutter,
    _sub: Subscription,
    _observe: Subscription,
}

impl OpenFile {
    fn id(&self) -> BufferId {
        self.input.entity_id()
    }

    fn label(&self) -> SharedString {
        if let Some(n) = self.untitled {
            return t_fmt(L10nKey::EditorUntitled, &[("n", &n.to_string())]).into();
        }
        self.path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| self.path.display().to_string())
            .into()
    }

    fn is_at(&self, host: HostId, path: &Path) -> bool {
        self.untitled.is_none() && self.host.id() == host && self.path == path
    }

    fn language(&self) -> &'static str {
        if self.untitled.is_some() {
            return "text";
        }
        language_for_path(&self.path)
    }

    fn local(&self) -> Option<LocalFile> {
        (self.untitled.is_none() && self.host.id().is_local()).then(|| LocalFile {
            id: self.id(),
            input: self.input.clone(),
            path: self.path.clone(),
            language: self.language(),
            indent: self.indent,
        })
    }
}

/// A saved file on this machine, as the language servers see it.
pub(crate) struct LocalFile {
    pub(crate) id: BufferId,
    pub(crate) input: Entity<InputState>,
    pub(crate) path: PathBuf,
    pub(crate) language: &'static str,
    pub(crate) indent: Indent,
}

/// One tab's view of the editor: the file it shows. A tab holds one file —
/// the editor's own UI names it, so the tab keeps no copy to draw.
pub(crate) struct TabCode {
    pub(crate) visible: bool,
    /// A dedicated terminal session rendered in this document slot.
    pub(crate) external: Option<TabId>,
    pub(crate) files: Vec<BufferId>,
    pub(crate) active: usize,
    pub(crate) roots: Vec<PathBuf>,
    pub(crate) expanded: std::collections::HashSet<PathBuf>,
    pub(crate) selected: Option<PathBuf>,
}

impl TabCode {
    pub(crate) fn new() -> Self {
        Self {
            visible: false,
            external: None,
            files: Vec::new(),
            active: 0,
            roots: Vec::new(),
            expanded: std::collections::HashSet::new(),
            selected: None,
        }
    }

    pub(crate) fn active_id(&self) -> Option<BufferId> {
        self.files.first().copied()
    }

    /// Puts a file in front. A tab shows one file, so whatever it showed
    /// before is replaced rather than listed beside it.
    fn show(&mut self, id: BufferId) {
        self.files.clear();
        self.files.push(id);
        self.active = 0;
    }

    /// Takes a file a caller loaded in the background, when the tab has room
    /// for it — that is, when it shows nothing.
    fn append(&mut self, id: BufferId) {
        if self.files.is_empty() {
            self.files.push(id);
            self.active = 0;
        }
    }

    /// Takes a file from a tab that is being merged into this one, when this
    /// tab has none of its own; a tab's file is not displaced by a merge.
    pub(crate) fn adopt(&mut self, ids: &[BufferId]) {
        if let Some(id) = ids.first() {
            self.append(*id);
        }
    }

    /// Takes a file out of the tab.
    fn forget(&mut self, id: BufferId) -> bool {
        let Some(pos) = self.files.iter().position(|f| *f == id) else {
            return false;
        };
        self.files.remove(pos);
        self.active = self.active.min(self.files.len().saturating_sub(1));
        true
    }

    /// Whether the tab shows this buffer. One file per tab, so this is the
    /// same question as "is it the file in front".
    pub(crate) fn shows(&self, id: BufferId) -> bool {
        self.files.contains(&id)
    }

    /// How many places in this tab show the buffer: 0 or 1.
    pub(crate) fn views_of(&self, id: BufferId) -> usize {
        usize::from(self.files.contains(&id))
    }

    /// Every buffer the tab shows, once each.
    pub(crate) fn all_files(&self) -> Vec<BufferId> {
        self.files.clone()
    }

    /// There is no second editor group any more.
    pub(crate) fn other_active_id(&self) -> Option<BufferId> {
        None
    }

    /// Closes a file in the tab's only group.
    pub(crate) fn close_in_focused(&mut self, id: BufferId) -> bool {
        self.forget(id)
    }

    /// Takes a buffer out of the tab, edits and all.
    pub(crate) fn forget_everywhere(&mut self, id: BufferId) -> bool {
        self.forget(id)
    }
}

/// What a question about unsaved files was standing in the way of.
#[derive(Clone, Debug)]
pub(crate) enum AfterUnsaved {
    CloseTab(TabId),
    /// Put this buffer in front of that tab's editor, replacing the file it
    /// showed, once the files leaving have been saved or discarded.
    ShowInTab(TabId, BufferId),
    ClosePane,
    CloseWindow,
    Quit,
}

/// Saves that something is waiting on: once every buffer in `ids` has landed
/// clean, `then` runs. Any one of them failing drops the wait, and with it
/// the close it would have carried out.
struct SaveWaiter {
    ids: Vec<BufferId>,
    then: AfterUnsaved,
}

enum BarKind {
    GoToLine,
    SaveAs {
        id: BufferId,
        then_close: bool,
    },
    /// A new name for the symbol at `offset` (`ui::lsp`).
    Rename {
        id: BufferId,
        offset: usize,
    },
}

/// The one-line prompt that sits above the text: go to line, or name a file
/// to save to on a machine the native save panel cannot browse.
struct EditorBar {
    kind: BarKind,
    input: Entity<InputState>,
    _sub: Subscription,
}

pub(crate) struct EditorPanelState {
    /// Every open buffer in this window. Tabs refer to these by id.
    buffers: Vec<OpenFile>,
    next_untitled: u32,
    /// Where to put the cursor once a particular file is on screen, for a
    /// `file.rs:120:3` that has to load first. Carried rather than applied at
    /// the call site because opening is asynchronous: the click is long over
    /// by the time there is a buffer to put a cursor in.
    pending_cursor: Option<(PathBuf, u32, u32)>,
    bar: Option<EditorBar>,
    waiters: Vec<SaveWaiter>,
    unsaved_prompt_open: bool,
    /// Tabs whose remembered files have been reopened (or found to have none).
    restored: HashSet<TabId>,
    /// What was last written to the session store for each tab, so an
    /// unchanged tab costs a comparison per frame and nothing more.
    recorded: HashMap<TabId, TabEditor>,
    /// A file loaded and waiting on an unsaved-changes answer before it can
    /// be shown. Orphan cleanup leaves it alone until the answer lands.
    pending_show: Option<BufferId>,
    watch: Option<Arc<WatchSub>>,
    watch_host: Option<SharedHost>,
    watch_opening: bool,
    watch_busy: bool,
    watch_dirty: bool,
    watched_dirs: HashSet<PathBuf>,
    watched_files: HashSet<PathBuf>,
    events_tx: smol::channel::Sender<Vec<PathBuf>>,
    /// Back/forward history, outlines and Go to Symbol's preview.
    nav: nav::EditorNav,
    /// The Problems list at the foot of the panel — see `problems`.
    problems: problems::ProblemsPane,
    /// The focus of a panel with no file in it. Without one there is nothing
    /// in the panel to hold the focus, so closing the last file handed it to
    /// the terminal and the next ⌘W closed the terminal instead of the panel.
    empty_focus: gpui::FocusHandle,
    /// The tab whose panel held the focus when it was last drawn, so the
    /// frame after its last file closes knows whether to take the focus over
    /// — and a tab switched to does not take it from its own terminal.
    had_focus: Option<TabId>,
}

impl EditorPanelState {
    pub(crate) fn new(window: &mut Window, cx: &mut Context<Tty7App>) -> Self {
        let (tx, rx) = smol::channel::unbounded::<Vec<PathBuf>>();
        cx.spawn_in(window, async move |app, cx| {
            while let Ok(first) = rx.recv().await {
                cx.background_executor().timer(RELOAD_DEBOUNCE).await;
                let mut changed: HashSet<PathBuf> = first.into_iter().collect();
                while let Ok(more) = rx.try_recv() {
                    changed.extend(more);
                }
                let ok = app.update_in(cx, |app, window, cx| {
                    for path in changed {
                        if app.editor.watched_files.contains(&path) {
                            app.editor_handle_external_change(&path, window, cx);
                        }
                    }
                });
                if ok.is_err() {
                    break;
                }
            }
        })
        .detach();
        Self {
            buffers: Vec::new(),
            next_untitled: 1,
            pending_cursor: None,
            bar: None,
            waiters: Vec::new(),
            unsaved_prompt_open: false,
            restored: HashSet::new(),
            recorded: HashMap::new(),
            pending_show: None,
            watch: None,
            watch_host: None,
            watch_opening: false,
            watch_busy: false,
            watch_dirty: false,
            watched_dirs: HashSet::new(),
            watched_files: HashSet::new(),
            events_tx: tx,
            nav: nav::EditorNav::new(cx),
            problems: Default::default(),
            empty_focus: cx.focus_handle(),
            had_focus: None,
        }
    }
}

pub(crate) fn language_for_path(path: &Path) -> &'static str {
    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
        let lowered = name.to_ascii_lowercase();
        match lowered.as_str() {
            "makefile" | "gnumakefile" => return "make",
            "cmakelists.txt" => return "cmake",
            _ => {}
        }
        if lowered.starts_with('.') && (lowered.contains("shrc") || lowered.ends_with("profile")) {
            return "bash";
        }
    }
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return "text";
    };
    match ext.to_ascii_lowercase().as_str() {
        "rs" => "rust",
        "go" => "go",
        "py" | "pyi" => "python",
        "js" | "mjs" | "cjs" | "jsx" => "javascript",
        "ts" | "mts" | "cts" => "typescript",
        "tsx" => "tsx",
        "json" | "jsonc" => "json",
        "toml" => "toml",
        "yaml" | "yml" => "yaml",
        "html" | "htm" => "html",
        "css" => "css",
        "md" | "markdown" => "markdown",
        "sh" | "bash" | "zsh" => "bash",
        "c" | "h" => "c",
        "cpp" | "cc" | "cxx" | "hpp" | "hh" => "cpp",
        "java" => "java",
        "kt" | "kts" => "kotlin",
        "lua" => "lua",
        "rb" => "ruby",
        "php" => "php",
        "sql" => "sql",
        "swift" => "swift",
        "scala" => "scala",
        "zig" => "zig",
        "proto" => "proto",
        "diff" | "patch" => "diff",
        "ex" | "exs" => "elixir",
        "erb" => "erb",
        "ejs" => "ejs",
        "svelte" => "svelte",
        "astro" => "astro",
        "graphql" | "gql" => "graphql",
        "cs" => "csharp",
        "cmake" | "mk" => "cmake",
        _ => "text",
    }
}

/// Dedicated editor sessions, not shells in which a user happens to run Vim.
pub(crate) fn is_external_editor_program(program: &str) -> bool {
    matches!(
        Path::new(program).file_name().and_then(|s| s.to_str()),
        Some("hx" | "helix" | "vim" | "nvim")
    )
}

/// A Vim expression yielding a literal filename for `:execute`. Code points
/// keep terminal control keys out of the input stream; Vim's own fnameescape
/// handles command separators, wildcard expansion and shell backticks.
fn vim_command_path(path: &str) -> String {
    let chars = path
        .chars()
        .map(|ch| (ch as u32).to_string())
        .collect::<Vec<_>>()
        .join(",");
    format!("fnameescape(list2str([{chars}]))")
}

fn external_editor_shell(
    mode: crate::core::config::FileOpenWith,
    path: &Path,
    line: Option<u32>,
    column: Option<u32>,
) -> crate::daemon::protocol::ShellSpec {
    use crate::core::config::FileOpenWith;
    let (program, args) = match mode {
        FileOpenWith::Helix => (
            "hx",
            vec![match line {
                Some(line) => format!("{}:{}:{}", path.display(), line, column.unwrap_or(1)),
                None => path.display().to_string(),
            }],
        ),
        FileOpenWith::Vim => (
            "vim",
            line.map(|line| format!("+{line}"))
                .into_iter()
                .chain(std::iter::once(path.display().to_string()))
                .collect(),
        ),
        FileOpenWith::BuiltIn => unreachable!("built-in files do not spawn a process"),
    };
    crate::daemon::protocol::ShellSpec {
        program: program.to_string(),
        args,
        args_are_tty7_defaults: false,
    }
}

/// What the status bar calls a language id from [`language_for_path`].
/// `None` for plain text, which the bar leaves unsaid.
fn language_label(id: &str) -> Option<&'static str> {
    Some(match id {
        "rust" => "Rust",
        "go" => "Go",
        "python" => "Python",
        "javascript" => "JavaScript",
        "typescript" => "TypeScript",
        "tsx" => "TypeScript JSX",
        "json" => "JSON",
        "toml" => "TOML",
        "yaml" => "YAML",
        "html" => "HTML",
        "css" => "CSS",
        "markdown" => "Markdown",
        "bash" => "Shell",
        "c" => "C",
        "cpp" => "C++",
        "java" => "Java",
        "kotlin" => "Kotlin",
        "lua" => "Lua",
        "ruby" => "Ruby",
        "php" => "PHP",
        "sql" => "SQL",
        "swift" => "Swift",
        "scala" => "Scala",
        "zig" => "Zig",
        "proto" => "Protocol Buffers",
        "diff" => "Diff",
        "elixir" => "Elixir",
        "erb" => "ERB",
        "ejs" => "EJS",
        "svelte" => "Svelte",
        "astro" => "Astro",
        "graphql" => "GraphQL",
        "csharp" => "C#",
        "cmake" => "CMake",
        "make" => "Makefile",
        _ => return None,
    })
}

/// How many frames a jump-to-line may wait for the editor to be laid out.
///
/// `InputState::scroll_to` gives up when the buffer has never been painted,
/// and that is exactly the state a file that just opened is in: the cursor
/// lands on the right line and the view stays at the top of the file, which
/// is the one thing a `:120` link exists to avoid. Three frames is the same
/// bounded-retry shape `prefill::select_all_when_drawn` uses against the same
/// class of problem.
const CURSOR_SCROLL_ATTEMPTS: u8 = 3;

/// Puts the cursor at `position`, re-trying on later frames until the scroll
/// that follows it can actually be computed.
///
/// Applied immediately as well as on the retry: the cursor itself lands with
/// no layout, so the position is right even for a pane that never paints.
fn place_cursor(
    input: Entity<InputState>,
    position: Position,
    left: u8,
    window: &mut Window,
    cx: &mut gpui::App,
) {
    input.update(cx, |state, cx| {
        state.set_cursor_position(position, window, cx);
    });
    // A target near the top of the file scrolls nowhere and is already done;
    // so is one that has landed. Either way this stops.
    if left <= 1 || input.read(cx).scroll_offset().y != px(0.) {
        return;
    }
    window.on_next_frame(move |window, cx| {
        place_cursor(input, position, left - 1, window, cx);
    });
    // Registering a callback does not by itself ask for a frame, and an
    // overlay that has finished drawing has no other reason to produce one.
    window.refresh();
}

/// Replaces a buffer's text with `new` as one ordinary edit.
///
/// `InputState::set_value` would be simpler, and it clears the undo history —
/// so a file reloaded because an agent or a `git checkout` touched it could no
/// longer be undone past that moment. Replacing only the span that differs is
/// an edit like any other: it goes on the undo stack, and the cursor and
/// scroll stay where they were for everything outside it.
fn replace_buffer_text(
    input: &Entity<InputState>,
    new: &str,
    window: &mut Window,
    cx: &mut gpui::App,
) {
    let old = input.read(cx).text().to_string();
    let Some((start, old_end, new_end)) = differing_span(&old, new) else {
        return;
    };
    let start16 = old[..start].encode_utf16().count();
    let end16 = start16 + old[start..old_end].encode_utf16().count();
    let replacement = &new[start..new_end];
    input.update(cx, |state, cx| {
        let cursor = state.cursor_position();
        state.replace_text_in_range(Some(start16..end16), replacement, window, cx);
        state.set_cursor_position(cursor, window, cx);
    });
}

/// The byte span where `old` and `new` differ: `(start, end in old, end in
/// new)`, on character boundaries in both. `None` when they are equal.
fn differing_span(old: &str, new: &str) -> Option<(usize, usize, usize)> {
    if old == new {
        return None;
    }
    let mut start = old
        .bytes()
        .zip(new.bytes())
        .take_while(|(a, b)| a == b)
        .count();
    while !old.is_char_boundary(start) || !new.is_char_boundary(start) {
        start -= 1;
    }
    let max_suffix = old.len().min(new.len()) - start;
    let mut suffix = old
        .bytes()
        .rev()
        .zip(new.bytes().rev())
        .take(max_suffix)
        .take_while(|(a, b)| a == b)
        .count();
    while !old.is_char_boundary(old.len() - suffix) || !new.is_char_boundary(new.len() - suffix) {
        suffix -= 1;
    }
    Some((start, old.len() - suffix, new.len() - suffix))
}

/// Parses what the go-to-line bar was given: `120`, `120:4` or `:120`.
/// One-based, as every compiler and the status bar count.
fn parse_line_target(text: &str) -> Option<(u32, u32)> {
    let text = text.trim().trim_start_matches(':');
    let mut parts = text.splitn(2, [':', ',']);
    let line: u32 = parts.next()?.trim().parse().ok()?;
    let column = match parts.next() {
        Some(c) if !c.trim().is_empty() => c.trim().parse().ok()?,
        _ => 1,
    };
    (line > 0).then_some((line, column.max(1)))
}

/// Why the built-in editor could not take a file.
enum EditorOpenError {
    /// Not text, so the editor was never the right place for it.
    NotText(PathBuf),
    /// Something already worded for the user.
    Message(String),
}

/// A file read off its host, decoded, with everything needed to edit it.
struct Loaded {
    path: PathBuf,
    text: String,
    format: TextFormat,
    mtime: Option<MTime>,
    config: EditorConfig,
}

/// Reads, sizes, decodes and configures a file, on the host's own thread.
fn load_file(h: &dyn tty7_core::host::Host, requested: PathBuf) -> Result<Loaded, EditorOpenError> {
    let path = h.canonicalize(&requested).unwrap_or(requested);
    let meta = h.stat(&path).map_err(|e| {
        EditorOpenError::Message(t_fmt(
            L10nKey::EditorCantOpen,
            &[("path", &path.display().to_string()), ("e", &e.to_string())],
        ))
    })?;
    if meta.len > MAX_FILE_BYTES {
        return Err(EditorOpenError::Message(t_fmt(
            L10nKey::EditorFileTooLarge,
            &[
                ("path", &path.display().to_string()),
                ("size", &(meta.len / (1024 * 1024)).to_string()),
            ],
        )));
    }
    let bytes = h.read_file(&path, MAX_FILE_BYTES).map_err(|e| {
        EditorOpenError::Message(t_fmt(
            L10nKey::EditorCantRead,
            &[("path", &path.display().to_string()), ("e", &e.to_string())],
        ))
    })?;
    let Some(decoded) = editor_text::decode(&bytes) else {
        return Err(EditorOpenError::NotText(path));
    };
    let config = read_editorconfig(h, &path);
    Ok(Loaded {
        path,
        text: decoded.text,
        format: decoded.format,
        mtime: meta.mtime,
        config,
    })
}

/// Collects the `.editorconfig` files that apply to `path`, nearest first,
/// stopping at the first one that declares itself the root.
fn read_editorconfig(h: &dyn tty7_core::host::Host, path: &Path) -> EditorConfig {
    let mut configs = Vec::new();
    for dir in path.ancestors().skip(1).take(EDITORCONFIG_DEPTH) {
        let file = h.join(dir, editor_text::EDITORCONFIG);
        let Ok(bytes) = h.read_file(&file, 64 * 1024) else {
            continue;
        };
        let contents = String::from_utf8_lossy(&bytes).into_owned();
        let root = editor_text::is_root(&contents);
        configs.push((dir.to_path_buf(), contents));
        if root {
            break;
        }
    }
    editor_text::editorconfig_for(path, &configs)
}

fn tab_size(indent: Indent) -> TabSize {
    TabSize {
        tab_size: indent.size.max(1),
        hard_tabs: indent.hard_tabs,
    }
}

/// Whether handing this path to the desktop would run it rather than show it.
///
/// The execute bit is what `open` reads to decide between displaying a file
/// and launching it; Windows has no such bit, so there the extension is the
/// only thing that says so.
fn is_program(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        return std::fs::metadata(path)
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0);
    }
    #[cfg(not(unix))]
    {
        let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
            return false;
        };
        matches!(
            ext.to_ascii_lowercase().as_str(),
            "exe"
                | "com"
                | "bat"
                | "cmd"
                | "scr"
                | "pif"
                | "msi"
                | "ps1"
                | "vbs"
                | "js"
                | "jse"
                | "wsf"
                | "wsh"
                | "cpl"
                | "msc"
                | "hta"
                | "reg"
                | "lnk"
        )
    }
}

/// A file a browser renders rather than showing its source.
fn opens_in_browser(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| {
        matches!(
            e.to_ascii_lowercase().as_str(),
            "html" | "htm" | "xhtml" | "svg"
        )
    })
}

/// Opens a local file in the default web browser. On macOS that is asked
/// for by name: what `.html` is associated with is as often an editor. The
/// other desktops go by the association.
fn open_in_browser(path: &Path) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    if let Some(bundle) = default_browser_bundle_id() {
        std::process::Command::new("open")
            .arg("-b")
            .arg(bundle)
            .arg(path)
            .spawn()?;
        return Ok(());
    }
    crate::terminal::view::open_file_path(path)
}

/// The app that handles `https:` links — the default browser.
#[cfg(target_os = "macos")]
fn default_browser_bundle_id() -> Option<String> {
    use core_foundation::base::TCFType as _;
    use core_foundation::string::{CFString, CFStringRef};

    #[link(name = "CoreServices", kind = "framework")]
    unsafe extern "C" {
        fn LSCopyDefaultHandlerForURLScheme(scheme: CFStringRef) -> CFStringRef;
    }
    let scheme = CFString::new("https");
    let handler = unsafe { LSCopyDefaultHandlerForURLScheme(scheme.as_concrete_TypeRef()) };
    // A Copy function: the string is ours to release, which the wrapper does.
    (!handler.is_null()).then(|| unsafe { CFString::wrap_under_create_rule(handler) }.to_string())
}

/// Modal editor commands must not enter an IME composition. Select the
/// user's last ASCII keyboard layout rather than hard-coding a US layout.
fn select_editor_english_input() {
    #[cfg(all(target_os = "macos", not(test)))]
    {
        use core_foundation::base::{CFRelease, CFTypeRef};
        #[link(name = "Carbon", kind = "framework")]
        unsafe extern "C" {
            fn TISCopyCurrentASCIICapableKeyboardLayoutInputSource() -> CFTypeRef;
            fn TISSelectInputSource(source: CFTypeRef) -> i32;
        }
        unsafe {
            let source = TISCopyCurrentASCIICapableKeyboardLayoutInputSource();
            if !source.is_null() {
                let status = TISSelectInputSource(source);
                CFRelease(source);
                if status != 0 {
                    log::warn!("could not select ASCII editor input source: {status}");
                }
            }
        }
    }
}

/// The lines a selection covers, 1-based and inclusive. One that ends at the
/// very start of a line — a whole-line selection — does not take that line.
fn selected_lines(
    text: &gpui_component::input::Rope,
    range: std::ops::Range<usize>,
) -> Option<(usize, usize)> {
    if range.is_empty() {
        return None;
    }
    let start = text.offset_to_point(range.start);
    let end = text.offset_to_point(range.end);
    let last = match end.column == 0 && end.row > start.row {
        true => end.row - 1,
        false => end.row,
    };
    Some((start.row + 1, last + 1))
}

/// What a watcher saw at a path: the file's modification time, or that it
/// is not there any more.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Observed {
    Present(Option<MTime>),
    Missing,
}

#[derive(Debug, PartialEq, Eq)]
enum ExternalChange {
    Ignore,
    Conflict,
    Deleted,
    Reload,
}

fn classify_external_change(
    saving: bool,
    dirty: bool,
    disk_mtime: Option<MTime>,
    observed: Observed,
) -> ExternalChange {
    if saving {
        return ExternalChange::Ignore;
    }
    let observed = match observed {
        Observed::Missing => return ExternalChange::Deleted,
        Observed::Present(m) => m,
    };
    if observed.is_some() && observed == disk_mtime {
        return ExternalChange::Ignore;
    }
    if dirty {
        ExternalChange::Conflict
    } else {
        ExternalChange::Reload
    }
}

/// How a save attempt ended, as reported back from the host's thread.
enum SaveOutcome {
    Saved(Option<MTime>),
    /// The file changed on disk since the buffer last matched it; nothing
    /// was written.
    Conflict(Option<MTime>),
    Failed(std::io::Error),
}

/// Writes `bytes` to `target`, first making sure nothing else wrote it since
/// the buffer last matched it. `expect` is `None` to write regardless — a
/// Save As, an Overwrite the user chose, or a file being recreated.
///
/// This is also the only external-change detection a host without a watcher
/// gets: an SFTP buffer never hears about someone else's edit until now.
fn write_checked(
    h: &dyn tty7_core::host::Host,
    target: &Path,
    bytes: &[u8],
    expect: Option<Option<MTime>>,
) -> SaveOutcome {
    if let Some(expected) = expect {
        match h.stat(target) {
            Ok(meta) if meta.mtime != expected => return SaveOutcome::Conflict(meta.mtime),
            Ok(_) => {}
            // Gone since it was opened: saving puts it back, which is what a
            // save of a deleted file means.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return SaveOutcome::Failed(e),
        }
    }
    match h.write_file(target, bytes) {
        Ok(meta) => SaveOutcome::Saved(meta.mtime),
        Err(e) => SaveOutcome::Failed(e),
    }
}

impl Tty7App {
    pub(crate) fn tab_code(&self) -> Option<&TabCode> {
        self.tabs.get(self.active)?.code.as_deref()
    }

    pub(crate) fn tab_code_mut(&mut self) -> Option<&mut TabCode> {
        self.tabs.get_mut(self.active)?.code.as_deref_mut()
    }

    pub(crate) fn tab_code_mut_or_init(&mut self) -> Option<&mut TabCode> {
        let tab = self.tabs.get_mut(self.active)?;
        Some(tab.code.get_or_insert_with(|| Box::new(TabCode::new())))
    }

    pub(crate) fn code_panel_visible(&self) -> bool {
        self.tab_code().is_some_and(|c| c.visible)
    }

    /// Whether the active tab's document is on its way out. The layout keeps
    /// drawing it at a shrinking width until the slide ends, so this is what
    /// tells the columns the close is in progress.
    pub(crate) fn document_closing_active(&self) -> bool {
        self.tabs
            .get(self.active)
            .zip(self.document_closing)
            .is_some_and(|(tab, (id, _))| tab.tree_id.get() == id)
    }

    /// The end of a close slide: hide the panel, but only if this is still the
    /// close that was started — a timer that outlived a reopen must not hide
    /// the panel the reader asked for.
    fn finish_document_close(
        &mut self,
        tab: tty7_core::core::machine::TabId,
        seq: u64,
        cx: &mut Context<Self>,
    ) {
        if self.document_closing != Some((tab, seq)) {
            return;
        }
        if let Some(ix) = self.tab_index_of(tab) {
            if let Some(code) = self.tabs[ix].code.as_mut() {
                code.visible = false;
            }
        }
        self.document_closing = None;
        cx.notify();
    }

    fn buffer(&self, id: BufferId) -> Option<&OpenFile> {
        self.editor.buffers.iter().find(|b| b.id() == id)
    }

    fn buffer_mut(&mut self, id: BufferId) -> Option<&mut OpenFile> {
        self.editor.buffers.iter_mut().find(|b| b.id() == id)
    }

    fn buffer_at(&self, host: HostId, path: &Path) -> Option<BufferId> {
        self.editor
            .buffers
            .iter()
            .find(|b| b.is_at(host, path))
            .map(OpenFile::id)
    }

    /// The buffer in front of the active tab's editor.
    fn active_buffer(&self) -> Option<&OpenFile> {
        self.buffer(self.tab_code()?.active_id()?)
    }

    /// Where the file in front of the active tab's editor lives, if it has
    /// been saved anywhere yet.
    pub(crate) fn editor_active_location(&self) -> Option<(HostId, &Path)> {
        let f = self.active_buffer()?;
        f.untitled
            .is_none()
            .then(|| (f.host.id(), f.path.as_path()))
    }

    fn tab_index_of(&self, tab: TabId) -> Option<usize> {
        self.tabs.iter().position(|t| t.tree_id.get() == tab)
    }

    /// How many places show this buffer: every tab showing it.
    fn buffer_refs(&self, id: BufferId) -> usize {
        self.tabs
            .iter()
            .filter_map(|t| t.code.as_deref())
            .map(|c| c.views_of(id))
            .sum()
    }

    /// How many tabs show this buffer, in either group.
    fn buffer_tabs(&self, id: BufferId) -> usize {
        self.tabs
            .iter()
            .filter_map(|t| t.code.as_deref())
            .filter(|c| c.shows(id))
            .count()
    }

    /// Whether the file tree should mark this path as having unsaved edits.
    pub(crate) fn editor_is_dirty(&self, host: HostId, path: &Path) -> bool {
        self.editor
            .buffers
            .iter()
            .any(|b| b.dirty && b.is_at(host, path))
    }

    /// Unsaved buffers that only tab `tab_ix` shows — the ones closing it
    /// would lose. A buffer another tab also shows survives the close.
    pub(crate) fn editor_unsaved_in_tab(&self, tab_ix: usize) -> Vec<BufferId> {
        let Some(code) = self.tabs.get(tab_ix).and_then(|t| t.code.as_deref()) else {
            return Vec::new();
        };
        code.all_files()
            .into_iter()
            .filter(|id| self.buffer(*id).is_some_and(|b| b.dirty) && self.buffer_tabs(*id) == 1)
            .collect()
    }

    /// Every unsaved buffer in the window.
    pub(crate) fn editor_unsaved(&self) -> Vec<BufferId> {
        self.editor
            .buffers
            .iter()
            .filter(|b| b.dirty)
            .map(OpenFile::id)
            .collect()
    }

    fn editor_rebuild_watcher(&mut self, cx: &mut Context<Self>) {
        // Only files on the host the watch itself runs on. A path from
        // another machine — an SFTP file, say — does not exist under that
        // watcher's feet, and would either miss or, worse, match a local file
        // that happens to share its name. Those are checked when they are
        // saved instead: see `write_checked`.
        let watch_host = self.spawn_host(cx);
        let files: HashSet<PathBuf> = self
            .editor
            .buffers
            .iter()
            .filter(|f| f.untitled.is_none() && f.host.id() == watch_host)
            .map(|f| f.path.clone())
            .collect();
        let dirs: HashSet<PathBuf> = files
            .iter()
            .filter_map(|p| p.parent().map(Path::to_path_buf))
            .collect();
        self.editor.watched_files = files;
        // Every change to which files are open, or where they live, passes
        // through here — which is exactly what the language servers follow.
        self.lsp_sync_buffers(cx);
        if dirs == self.editor.watched_dirs {
            return;
        }
        self.editor.watched_dirs = dirs;
        self.editor_watch_apply(cx);
    }

    fn editor_watch_apply(&mut self, cx: &mut Context<Self>) {
        let want: Vec<PathBuf> = self.editor.watched_dirs.iter().cloned().collect();
        let Some(host) = self.active_host(cx) else {
            return;
        };

        if !self
            .editor
            .watch_host
            .as_ref()
            .is_some_and(|opened_with| Arc::ptr_eq(opened_with, &host))
        {
            self.editor.watch = None;
            self.editor.watch_host = None;
            self.editor.watch_busy = false;
            self.editor.watch_dirty = false;
        }

        if let Some(sub) = self.editor.watch.clone() {
            if self.editor.watch_busy {
                self.editor.watch_dirty = true;
                return;
            }
            self.editor.watch_busy = true;
            HostOps::run(
                host,
                cx,
                move |_| sub.set_dirs(&want),
                |app: &mut Self, result: std::io::Result<()>, cx| {
                    app.editor.watch_busy = false;
                    if let Err(e) = result {
                        log::warn!("editor: could not update the watched set: {e}");
                    }
                    if std::mem::take(&mut app.editor.watch_dirty) {
                        app.editor_watch_apply(cx);
                    }
                },
            );
            return;
        }
        if self.editor.watch_opening {
            return;
        }
        self.editor.watch_opening = true;
        let opened_host = Arc::clone(&host);
        let opened_with = self.editor.watched_dirs.clone();
        HostOps::run(
            host,
            cx,
            {
                let want = want.clone();
                move |h| h.watch(&want).map(Arc::new)
            },
            move |app, result: std::io::Result<Arc<WatchSub>>, cx| {
                app.editor.watch_opening = false;
                let sub = match result {
                    Ok(sub) => sub,
                    Err(e) => {
                        log::warn!("editor: external-change watcher unavailable: {e}");
                        return;
                    }
                };
                let events = sub.events().clone();
                app.editor.watch = Some(sub);
                app.editor.watch_host = Some(opened_host);
                cx.spawn(async move |app, cx| {
                    while let Ok(batch) = events.recv().await {
                        let ok = app.update(cx, |app, _cx| {
                            let _ = app.editor.events_tx.try_send(batch);
                        });
                        if ok.is_err() {
                            break;
                        }
                    }
                })
                .detach();
                if app.editor.watched_dirs != opened_with {
                    app.editor_watch_apply(cx);
                }
            },
        );
    }

    /// Shows what a file link in the grid pointed at.
    ///
    /// A file opens in the editor, on the line the link named; the tree
    /// follows along so "what does it say?" and "where does it live?" are
    /// answered by the same click. A directory has no contents to show, so it
    /// is only ever the tree.
    pub(crate) fn open_linked_file(
        &mut self,
        path: &Path,
        line: Option<u32>,
        column: Option<u32>,
        is_dir: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if is_dir {
            self.file_tree_show(path, window, cx);
            return;
        }
        self.open_file_in_editor_at(path, line, column, window, cx);
        self.file_tree_reveal_path(path, cx);
    }

    /// [`Self::open_file_in_editor`], landing the cursor on a line the caller
    /// already knows — what a `src/main.rs:120:3` in the grid was pointing at.
    pub(crate) fn open_file_in_editor_at(
        &mut self,
        path: &Path,
        line: Option<u32>,
        column: Option<u32>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.editor.pending_cursor =
            line.map(|line| (path.to_path_buf(), line, column.unwrap_or(1)));
        self.open_file_in_editor(path, window, cx);
    }

    /// Moves the cursor to the position a link asked for, if the file it asked
    /// about is the one that just opened. Anything else — a different file
    /// opened in between, a file that never arrived — drops the request rather
    /// than throwing the cursor somewhere it was never meant to go.
    fn apply_pending_cursor(
        &mut self,
        id: BufferId,
        requested: &Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Peeked before it is taken: another file opening in the meantime must
        // not swallow a request that was never about it.
        if self
            .editor
            .pending_cursor
            .as_ref()
            .is_none_or(|(wanted, ..)| wanted != requested)
        {
            return;
        }
        let Some((_, line, column)) = self.editor.pending_cursor.take() else {
            return;
        };
        let Some(file) = self.buffer_mut(id) else {
            return;
        };
        // A line to land on is a place in the source. A Markdown file that
        // would otherwise open rendered (the remembered preview) opens as
        // text here, or the cursor would be placed somewhere nobody can see.
        file.preview = false;
        let input = file.input.clone();
        // The grid counts from one and `Position` counts from zero, and a
        // compiler that says "line 1" means the first line either way.
        let position = Position {
            line: line.saturating_sub(1),
            character: column.saturating_sub(1),
        };
        place_cursor(input, position, CURSOR_SCROLL_ATTEMPTS, window, cx);
    }

    pub(crate) fn open_file_in_editor(
        &mut self,
        path: &Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(host) = self.active_host(cx) else {
            return;
        };
        self.editor_open_on_host(host, path, window, cx);
    }

    pub(crate) fn is_external_editor_tab(&self, index: usize, cx: &gpui::App) -> bool {
        let Some(tab) = self.tabs.get(index) else {
            return false;
        };
        self.external_editor_tabs
            .iter()
            .any(|editor| editor.tab == tab.tree_id.get())
            || tab.pane.terminals().iter().any(|pane| {
                pane.read(cx)
                    .shell_spec()
                    .is_some_and(|shell| is_external_editor_program(&shell.program))
            })
    }

    pub(crate) fn external_editor_index(&self) -> Option<usize> {
        self.tab_index_of(self.tab_code()?.external?)
    }

    /// Spawning a pane can adopt the daemon's tab ID. Move document links
    /// before the next frame checks whether their backing session exited.
    pub(crate) fn editor_rekey_tab(&mut self, old: TabId, new: TabId, cx: &mut Context<Self>) {
        if old == new {
            return;
        }
        for tab in &mut self.tabs {
            if let Some(code) = tab.code.as_mut()
                && code.external == Some(old)
            {
                code.external = Some(new);
            }
        }
        if self.editor.restored.remove(&old) {
            self.editor.restored.insert(new);
        }
        self.editor.recorded.remove(&old);
        if let Some(state) = editor_session::get(cx, old) {
            editor_session::put(cx, new, state);
        }
        editor_session::remove(cx, old);
        for editor in &mut self.external_editor_tabs {
            let changed = editor.tab == old || editor.terminal == Some(old);
            if editor.tab == old {
                editor.tab = new;
            }
            if editor.terminal == Some(old) {
                editor.terminal = Some(new);
            }
            if changed {
                editor_session::put_external_editor(cx, editor.clone());
            }
        }
    }

    pub(crate) fn select_external_editor(
        &mut self,
        tab: TabId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.tab_index_of(tab).is_none() {
            return;
        }
        let Some(code) = self.tab_code_mut_or_init() else {
            return;
        };
        code.external = Some(tab);
        code.visible = true;
        self.raise_code_overlay();
        self.document_closing = None;
        self.editor.bar = None;
        self.focus_editor(window, cx);
        select_editor_english_input();
        // The file row's mouse-up and the newly mounted pane must settle
        // before the editor becomes the keyboard target.
        let owner = self.tabs[self.active].tree_id.get();
        let app = cx.entity().downgrade();
        window.on_next_frame(move |window, cx| {
            let _ = app.update(cx, |app, cx| {
                if app
                    .tabs
                    .get(app.active)
                    .is_some_and(|active| active.tree_id.get() == owner)
                    && app
                        .tab_code()
                        .is_some_and(|code| code.visible && code.external == Some(tab))
                    && !app.document_closing_active()
                {
                    app.focus_editor(window, cx);
                    select_editor_english_input();
                }
            });
        });
        window.refresh();
        cx.notify();
    }

    pub(crate) fn close_external_editor(
        &mut self,
        tab: TabId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(index) = self.tab_index_of(tab) {
            // Keep the existing running-process confirmation policy.
            self.close_tab(index, window, cx);
        }
    }

    /// Opens local modal editors in the same document slot as the built-in
    /// editor. One session per terminal: a file opened while one is already
    /// running is handed to that editor, which owns its buffers — and the
    /// name it shows — from then on.
    pub(crate) fn open_file_preferred(
        &mut self,
        path: &Path,
        line: Option<u32>,
        column: Option<u32>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mode = cx.global::<crate::core::config::Config>().file_open_with;
        let local = self
            .active_host(cx)
            .is_some_and(|host| host.id().is_local());
        if mode == crate::core::config::FileOpenWith::BuiltIn || !local {
            self.open_file_in_editor_at(path, line, column, window, cx);
            return;
        }
        let Some(owner) = self.tabs.get(self.active).map(|tab| tab.tree_id.get()) else {
            return;
        };
        // The editor already running for this terminal takes the file. It
        // decides whether this is a new buffer or one coming forward, and
        // its own status line shows which file is in front.
        if let Some(index) = self.external_editor_session(owner, cx) {
            self.external_editor_open(index, path, line, column, cx);
            let session = self.tabs[index].tree_id.get();
            self.select_external_editor(session, window, cx);
            return;
        }
        // A record whose session is gone — an older build, or a quit that
        // closed the tab — must not stand in for the one about to start.
        let live: HashSet<TabId> = self.tabs.iter().map(|tab| tab.tree_id.get()).collect();
        self.external_editor_tabs
            .retain(|editor| editor.terminal != Some(owner) || live.contains(&editor.tab));
        let cwd = self
            .tabs
            .get(self.active)
            .and_then(|tab| tab.pane.focused_or_first(window, cx))
            .and_then(|pane| pane.read(cx).spawnable_cwd());
        let terminal_visible = self.terminal_visible;
        let shell = external_editor_shell(mode, path, line, column);
        let program = shell.program.clone();
        let maximized = self.maximized.clone();
        let Some(_slot) = self.new_tab_slot(cwd, Some(shell), window, cx) else {
            log::warn!("no editor opened for {:?}", path);
            return;
        };
        let editor = ExternalEditorTab {
            path: path.to_path_buf(),
            tab: self.tabs[self.active].tree_id.get(),
            terminal: Some(owner),
            program,
        };
        editor_session::put_external_editor(cx, editor.clone());
        self.external_editor_tabs.push(editor.clone());
        // new_tab_slot seats the session in the daemon tree. Restore the
        // terminal selection before rendering either region.
        self.active = self
            .tab_index_of(owner)
            .expect("the originating terminal still exists");
        self.terminal_visible = terminal_visible;
        self.maximized = maximized;
        self.select_external_editor(editor.tab, window, cx);
        self.save_session(cx);
    }

    /// The live editor session opened from `owner`, if it is still running.
    fn external_editor_session(&self, owner: TabId, cx: &gpui::App) -> Option<usize> {
        let editor = self
            .external_editor_tabs
            .iter()
            .rev()
            .find(|editor| editor.terminal == Some(owner))?;
        let index = self.tab_index_of(editor.tab)?;
        let recorded = (!editor.program.is_empty()).then(|| editor.program.clone());
        let live = self.tabs[index].pane.terminals().iter().any(|terminal| {
            let view = terminal.read(cx);
            // A session record only ever names an editor tab. A pane whose
            // program is known to be something else is a stale record; one
            // with no program to go on (a restored quiet pane) is still the
            // session.
            !view.terminal.exited
                && match recorded.as_deref() {
                    Some(program) => is_external_editor_program(program),
                    None => view
                        .shell_spec()
                        .is_none_or(|shell| is_external_editor_program(&shell.program)),
                }
        });
        live.then_some(index)
    }

    /// Asks the running editor for `path`, in the words it understands. The
    /// editor keeps its buffers: there is no file tab strip to mirror them,
    /// and its own status line names whichever is in front.
    fn external_editor_open(
        &self,
        index: usize,
        path: &Path,
        line: Option<u32>,
        column: Option<u32>,
        cx: &gpui::App,
    ) {
        let Some(terminal) = self
            .tabs
            .get(index)
            .and_then(|tab| tab.pane.terminals().into_iter().next())
        else {
            return;
        };
        let session = self.tabs[index].tree_id.get();
        let program = self
            .external_editor_tabs
            .iter()
            .find(|editor| editor.tab == session)
            .map(|editor| editor.program.clone())
            .filter(|program| !program.is_empty())
            .or_else(|| terminal.read(cx).shell_spec().map(|shell| shell.program));
        let Some(program) = program else {
            return;
        };
        let path_text = path.to_string_lossy();
        // A newline would end the command line early and run whatever follows
        // as a second command; nothing legitimate needs one in a path.
        if path_text.contains(['\n', '\r']) {
            return;
        }
        let name = Path::new(&program)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        let mut commands = String::new();
        match name {
            "hx" | "helix" => {
                // A bare ESC immediately followed by `:` is parsed as Alt-:
                // by Helix. Encode Escape as CSI-u so the following `:open`
                // cannot turn into normal-mode `o` and insert the path.
                commands.push_str("\x1b[27u");
                // Single-quoted is Helix's literal token: spaces, `%`, and
                // backslashes all stay part of the path. A quote inside it is
                // escaped by doubling.
                commands.push_str(&format!(":open '{}'\r", path_text.replace('\'', "''")));
                if let Some(line) = line {
                    commands.push_str(&format!(":goto {line}\r"));
                }
            }
            "vim" | "nvim" => {
                commands.push('\x1b');
                commands.push_str(&format!(
                    ":execute 'hide edit ' . {}\r",
                    vim_command_path(&path_text)
                ));
                if let Some(line) = line {
                    commands.push_str(&format!(":call cursor({line}, {})\r", column.unwrap_or(1)));
                }
            }
            _ => return,
        }
        terminal.read(cx).run_editor_command(&commands);
    }

    /// [`Self::open_file_in_editor`] against an explicit host — the SFTP
    /// browser's files live on a host that is never the active one.
    pub(crate) fn editor_open_on_host(
        &mut self,
        host: SharedHost,
        path: &Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.tabs.get(self.active).map(|t| t.tree_id.get()) else {
            return;
        };
        self.raise_code_overlay();
        if let Some(id) = self.buffer_at(host.id(), path) {
            self.editor_show_in_tab(tab, id, true, window, cx);
            self.apply_pending_cursor(id, path, window, cx);
            return;
        }
        let p = path.to_path_buf();
        let requested = p.clone();
        let host_id = host.id();
        HostOps::run_in(
            host.clone(),
            window,
            cx,
            move |h| load_file(h, p),
            move |app, opened, window, cx| match opened {
                Ok(loaded) => {
                    let id = app.editor_install(host, loaded, tab, true, window, cx);
                    // Against `requested`, not the loaded path: the host
                    // canonicalised it on the way through, and a link that
                    // named a symlink would otherwise lose the line it asked
                    // for.
                    app.apply_pending_cursor(id, &requested, window, cx);
                }
                Err(EditorOpenError::NotText(path)) => {
                    app.open_outside_the_editor(host_id, &path, window, cx);
                }
                Err(EditorOpenError::Message(message)) => {
                    window.push_notification(message, cx);
                }
            },
        );
    }

    /// What to do with a file the built-in editor cannot show.
    ///
    /// A click on a PNG or a `.zip` meant "open this", not "tell me it is not
    /// text", and on this machine the desktop knows how. A file on another
    /// machine has nobody here to hand it to, so that one gets the words.
    ///
    /// A program does not: `open` on a Mach-O binary runs it, and a build's
    /// output is full of paths to programs. Clicking a word in a terminal
    /// must not be a way to execute one, so those keep the words too.
    fn open_outside_the_editor(
        &mut self,
        host_id: HostId,
        path: &Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !host_id.is_local() || !self.can_spawn_locally(cx) || is_program(path) {
            window.push_notification(
                t_fmt(
                    L10nKey::EditorBinaryFile,
                    &[("path", &path.display().to_string())],
                ),
                cx,
            );
            return;
        }
        Self::open_with(path, crate::terminal::view::open_file_path, window, cx);
    }

    /// Runs one of the desktop openers on a local file, and says so when it
    /// fails to spawn — as any opener can (#542).
    fn open_with(
        path: &Path,
        opener: fn(&Path) -> std::io::Result<()>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Err(e) = opener(path) {
            log::warn!("failed to open {}: {e}", path.display());
            window.push_notification(
                crate::ui::host_ops::failure(
                    t_fmt(
                        L10nKey::LinkFileOpenFailed,
                        &[
                            ("path", &path.display().to_string()),
                            ("error", &e.to_string()),
                        ],
                    ),
                    &e,
                ),
                cx,
            );
        }
    }

    /// Puts a buffer in front of a tab's editor. A tab shows one file: every
    /// other file it held leaves with this one — after asking about anything
    /// unsaved, so a click in the tree cannot lose edits.
    fn editor_show_in_tab(
        &mut self,
        tab: TabId,
        id: BufferId,
        front: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab_ix) = self.tab_index_of(tab).or(Some(self.active)) else {
            return;
        };
        // Showing something in a tab that is sliding closed reopens it: the
        // close timer must not hide what was just put in front of the reader.
        if self.document_closing.is_some_and(|(id, _)| id == tab) {
            self.document_closing = None;
        }
        if let Some(t) = self.tabs.get_mut(tab_ix)
            && t.code.is_none()
        {
            t.code = Some(Box::new(TabCode::new()));
        }
        if !front {
            // Listed only: a tab with nothing in it takes the file; one that
            // already shows something does not grow a second file.
            let mut took = false;
            if let Some(code) = self
                .tabs
                .get_mut(tab_ix)
                .and_then(|t| t.code.as_deref_mut())
            {
                if code.files.is_empty() && code.external.is_none() {
                    code.append(id);
                    took = true;
                }
            }
            if took {
                cx.notify();
            }
            return;
        }
        // One file per tab. The files it replaces are asked about first, so
        // an unsaved buffer is never dropped by a click in the tree.
        let others: Vec<BufferId> = self
            .tabs
            .get(tab_ix)
            .and_then(|t| t.code.as_deref())
            .map(|code| {
                code.all_files()
                    .into_iter()
                    .filter(|other| *other != id)
                    .collect()
            })
            .unwrap_or_default();
        let unsaved: Vec<BufferId> = others
            .iter()
            .copied()
            .filter(|other| {
                self.buffer(*other).is_some_and(|f| f.dirty) && self.buffer_refs(*other) == 1
            })
            .collect();
        if !unsaved.is_empty() {
            // One question at a time. The file is already loaded behind it;
            // cleanup keeps it until the answer comes back, or drops it when
            // the answer is cancel.
            if self.editor.unsaved_prompt_open {
                return;
            }
            self.editor.pending_show = Some(id);
            if self.editor_guard_unsaved(unsaved, AfterUnsaved::ShowInTab(tab, id), window, cx) {
                return;
            }
            self.editor.pending_show = None;
        }
        for other in others {
            self.editor_remove_from_tab(tab_ix, other, cx);
        }
        if let Some(code) = self
            .tabs
            .get_mut(tab_ix)
            .and_then(|t| t.code.as_deref_mut())
        {
            code.show(id);
            code.external = None;
            code.visible = true;
        }
        if tab_ix == self.active {
            self.editor.bar = None;
            self.focus_editor(window, cx);
        }
        cx.notify();
    }

    /// Makes a buffer out of a loaded file, or finds the one already open for
    /// it, and shows it in `tab`.
    fn editor_install(
        &mut self,
        host: SharedHost,
        loaded: Loaded,
        tab: TabId,
        front: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> BufferId {
        if let Some(id) = self.buffer_at(host.id(), &loaded.path) {
            self.editor_show_in_tab(tab, id, front, window, cx);
            return id;
        }
        let language = language_for_path(&loaded.path);
        let indent = loaded
            .config
            .indent(editor_text::detect_indent(&loaded.text, language));
        let id = self.editor_new_buffer(
            host,
            loaded.path,
            None,
            loaded.text,
            loaded.format,
            loaded.config,
            indent,
            loaded.mtime,
            window,
            cx,
        );
        self.editor_show_in_tab(tab, id, front, window, cx);
        self.editor_rebuild_watcher(cx);
        id
    }

    #[allow(clippy::too_many_arguments)]
    fn editor_new_buffer(
        &mut self,
        host: SharedHost,
        path: PathBuf,
        untitled: Option<u32>,
        text: String,
        format: TextFormat,
        config: EditorConfig,
        indent: Indent,
        mtime: Option<MTime>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> BufferId {
        let language = if untitled.is_some() {
            "text"
        } else {
            language_for_path(&path)
        };
        let (wrap, preview) = {
            let cfg = cx.global::<crate::core::config::Config>();
            (
                cfg.editor_soft_wrap,
                cfg.editor_markdown_preview && language == "markdown",
            )
        };
        let saved = Fingerprint::of_str(&text);
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .code_editor(language)
                .multi_line(true)
                .tab_size(tab_size(indent))
                .line_number(true)
                .searchable(true)
                .replaceable(true)
                .folding(true)
                .soft_wrap(wrap)
                // Ours is drawn by `editor_body_menu`, like every other menu
                // in the window; the built-in one is a native OS menu.
                .context_menu(false)
                .default_value(text)
        });
        let id = input.entity_id();
        let git_gutter = gutter::BufferGutter::attach(&input, id, cx);
        let sub = cx.subscribe_in(
            &input,
            window,
            move |this: &mut Tty7App, _input, ev, _window, cx| {
                if matches!(ev, InputEvent::Change) {
                    this.editor_note_edit(id, cx);
                }
            },
        );
        let observe = cx.observe(&input, |_, _, cx| cx.notify());
        self.editor.buffers.push(OpenFile {
            path,
            untitled,
            host,
            input,
            dirty: false,
            saved,
            format,
            saved_format: format,
            config,
            indent,
            disk_mtime: mtime,
            saving: false,
            save_pending: false,
            save_then_close: false,
            reload_seq: 0,
            conflict: None,
            preview,
            wrap,
            preview_scroll: gpui::ScrollHandle::new(),
            gutter: git_gutter,
            _sub: sub,
            _observe: observe,
        });
        id
    }

    /// Re-derives whether a buffer differs from what is on disk, after an
    /// edit of any kind — typing, undo, a reload, a save rule.
    fn editor_note_edit(&mut self, id: BufferId, cx: &mut Context<Self>) {
        let Some(f) = self.buffer(id) else {
            return;
        };
        let state = f.input.read(cx);
        let text = state.text();
        let len = text.len();
        let same_text = len == f.saved.len && Fingerprint::of_chunks(len, text.chunks()) == f.saved;
        let dirty = !same_text || f.format != f.saved_format;
        if let Some(f) = self.buffer_mut(id) {
            f.dirty = dirty;
        }
        self.lsp_buffer_edited(id, cx);
        self.editor_gutter_note_edit(id, cx);
        self.editor_nav_note_edit(id, cx);
        cx.notify();
    }

    /// Saved files on this machine — what a language server can read
    /// (`ui::lsp`). Untitled buffers and files on other hosts are not.
    pub(crate) fn editor_local_files(&self) -> Vec<LocalFile> {
        self.editor
            .buffers
            .iter()
            .filter_map(OpenFile::local)
            .collect()
    }

    pub(crate) fn editor_local_file(&self, id: BufferId) -> Option<LocalFile> {
        self.buffer(id).and_then(OpenFile::local)
    }

    pub(crate) fn editor_active_local_file(&self) -> Option<LocalFile> {
        self.active_buffer().and_then(OpenFile::local)
    }

    /// Asks for a new name for the symbol at `offset`, in the bar above the
    /// text; `ui::lsp` does the renaming once it is answered.
    pub(crate) fn editor_open_rename_bar(
        &mut self,
        id: BufferId,
        offset: usize,
        current: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.editor_open_bar(BarKind::Rename { id, offset }, current, window, cx);
    }

    /// A new, empty, never-saved buffer, on the window's own machine.
    pub(crate) fn editor_new_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(host) = self.active_host(cx) else {
            return;
        };
        let Some(tab) = self.tabs.get(self.active).map(|t| t.tree_id.get()) else {
            return;
        };
        self.raise_code_overlay();
        let n = self.editor.next_untitled;
        self.editor.next_untitled += 1;
        let indent = editor_text::detect_indent("", "text");
        let id = self.editor_new_buffer(
            host,
            PathBuf::new(),
            Some(n),
            String::new(),
            TextFormat::default(),
            EditorConfig::default(),
            indent,
            None,
            window,
            cx,
        );
        self.editor_show_in_tab(tab, id, true, window, cx);
    }

    pub(crate) fn toggle_code_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Ctrl+3 (or Esc) again while the panel is sliding away is the reader
        // asking for it back: the files are still there until the slide ends,
        // so this cancels the close rather than reopening over it.
        if self.document_closing_active() {
            self.document_closing = None;
            self.focus_editor(window, cx);
            cx.notify();
            return;
        }
        let Some(tab_id) = self.tabs.get(self.active).map(|t| t.tree_id.get()) else {
            return;
        };
        let buried = self.tabs.get(self.active).is_some_and(|tab| {
            tab.overlay_top == crate::ui::app::OverlayTop::Diff
                && tab.diff_overlay.is_some()
                && tab.code.as_ref().is_some_and(|c| c.visible)
        });
        if let Some(tab) = self.tabs.get_mut(self.active) {
            tab.overlay_top = crate::ui::app::OverlayTop::Code;
        }
        if buried {
            self.focus_editor(window, cx);
            cx.notify();
            return;
        }
        let visible = self.tab_code().is_some_and(|c| c.visible);
        if visible && crate::ui::panel_anim::PANEL_ANIMATIONS {
            // The panel keeps its files, and its last frame, while the column
            // clips it away: hiding it now would blank it mid-slide.
            self.document_close_seq += 1;
            let seq = self.document_close_seq;
            self.document_closing = Some((tab_id, seq));
            self.editor.had_focus = None;
            self.editor.bar = None;
            self.file_tree.editing = None;
            // Only where there is something on screen to type into: with the
            // terminal area hidden the panes are not drawn, and focus put on
            // one would swallow every keystroke.
            if self.terminal_visible {
                self.focus_active(window, cx);
            } else {
                self.workspace_focus.focus(window, cx);
            }
            cx.spawn(async move |this, cx| {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(
                        crate::ui::panel_anim::PANEL_ANIM_MS,
                    ))
                    .await;
                let _ = this.update(cx, |this, cx| this.finish_document_close(tab_id, seq, cx));
            })
            .detach();
            cx.notify();
            return;
        }
        let Some(tab) = self.tabs.get_mut(self.active) else {
            return;
        };
        let code = tab.code.get_or_insert_with(|| Box::new(TabCode::new()));
        if code.visible {
            code.visible = false;
            self.editor.had_focus = None;
            self.editor.bar = None;
            self.file_tree.editing = None;
            if self.terminal_visible {
                self.focus_active(window, cx);
            } else {
                self.workspace_focus.focus(window, cx);
            }
            cx.notify();
            return;
        }
        code.visible = true;
        self.document_closing = None;
        self.file_tree_refresh_roots(window, cx);
        if self.external_editor_index().is_some() || self.active_buffer().is_some() {
            self.focus_editor(window, cx);
        } else {
            // With no file to show, the panel says "Open a file from the file
            // tree" and hands the tree the focus — but nothing was putting the
            // tree on screen, so Ctrl+3 on a fresh tab opened an empty editor
            // pointing at a panel the reader could not see or reach from
            // there. Reveal it, then focus it.
            if !self.file_tree_on_screen(cx) {
                self.set_right_panel_tab(crate::core::config::RightPanelTab::Files, cx);
            }
            self.file_tree.focus_handle.focus(window, cx);
        }
        cx.notify();
    }

    fn raise_code_overlay(&mut self) {
        if let Some(tab) = self.tabs.get_mut(self.active) {
            tab.overlay_top = crate::ui::app::OverlayTop::Code;
        }
    }

    pub(crate) fn focus_editor(&self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(index) = self.external_editor_index() {
            if let Some(target) = self.tabs[index].pane.focused_or_first_slot(window, cx) {
                target.focus_handle(cx).focus(window, cx);
            }
            return;
        }
        if let Some(f) = self.active_buffer() {
            f.input.update(cx, |input, cx| input.focus(window, cx));
        }
    }

    /// The status bar's Preview / Edit button, and `ToggleDocumentPreview`.
    /// Acts on the active tab's open file, and only when that file is
    /// Markdown and the code panel is on screen — anything else has no
    /// rendered form to switch to. The new state is remembered for the next
    /// Markdown file opened.
    pub(crate) fn toggle_document_preview(&mut self, cx: &mut Context<Self>) {
        if !self.code_panel_visible() {
            return;
        }
        let Some(id) = self.tab_code().and_then(TabCode::active_id) else {
            return;
        };
        let Some(f) = self.buffer_mut(id) else {
            return;
        };
        if f.language() != "markdown" {
            return;
        }
        f.preview = !f.preview;
        let preview = f.preview;
        self.update_config(cx, |cfg| cfg.editor_markdown_preview = preview);
        cx.notify();
    }

    /// The status bar's Wrap button, and `ToggleDocumentWrap`. Same reach as
    /// [`Self::toggle_document_preview`], for any file; the new state becomes
    /// what the next file opens with.
    pub(crate) fn toggle_document_wrap(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.code_panel_visible() {
            return;
        }
        let Some(id) = self.tab_code().and_then(TabCode::active_id) else {
            return;
        };
        let Some(f) = self.buffer_mut(id) else {
            return;
        };
        f.wrap = !f.wrap;
        let wrap = f.wrap;
        f.input.clone().update(cx, |st, cx| {
            st.set_soft_wrap(wrap, window, cx);
        });
        self.update_config(cx, |cfg| cfg.editor_soft_wrap = wrap);
        cx.notify();
    }

    /// The status bar's line-ending button: flips the file between LF and
    /// CRLF. The buffer itself always holds `\n`; this only changes what a
    /// save writes, so it marks the file unsaved without touching its text.
    fn toggle_line_ending(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.tab_code().and_then(TabCode::active_id) else {
            return;
        };
        let Some(f) = self.buffer_mut(id) else {
            return;
        };
        f.format.line_ending = match f.format.line_ending {
            LineEnding::Lf => LineEnding::CrLf,
            LineEnding::CrLf => LineEnding::Lf,
        };
        self.editor_note_edit(id, cx);
    }

    /// Run one of gpui-component's editing actions on the active buffer, as
    /// if it had been pressed there. Returns false with no buffer to run it
    /// on. The palette closes before its row runs, so this focuses the
    /// buffer first rather than relying on where the focus happens to be.
    pub(crate) fn editor_dispatch(
        &mut self,
        action: &dyn gpui::Action,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.code_panel_visible() {
            return false;
        }
        let Some(input) = self.active_buffer().map(|f| f.input.clone()) else {
            return false;
        };
        let handle = input.read(cx).focus_handle(cx);
        window.focus(&handle, cx);
        handle.dispatch_action(action, window, cx);
        true
    }

    /// The window's listeners for the editor's text commands, when the user
    /// has bound one: each runs on the editor only while it has the focus.
    pub(crate) fn with_editor_text_commands(
        root: gpui::Stateful<gpui::Div>,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        use crate::core::actions::{
            EditorJoinLines, EditorRemoveSurroundingBrackets, EditorTransformLowercase,
            EditorTransformTitleCase, EditorTransformUppercase, EditorTrimTrailingWhitespace,
        };
        root.on_action(
            cx.listener(|this, _: &EditorTransformUppercase, window, cx| {
                if !this.editor_has_focus(window, cx) {
                    cx.propagate();
                    return;
                }
                this.editor_dispatch(&gpui_component::input::TransformToUppercase, window, cx);
            }),
        )
        .on_action(
            cx.listener(|this, _: &EditorTransformLowercase, window, cx| {
                if !this.editor_has_focus(window, cx) {
                    cx.propagate();
                    return;
                }
                this.editor_dispatch(&gpui_component::input::TransformToLowercase, window, cx);
            }),
        )
        .on_action(
            cx.listener(|this, _: &EditorTransformTitleCase, window, cx| {
                if !this.editor_has_focus(window, cx) {
                    cx.propagate();
                    return;
                }
                this.editor_dispatch(&gpui_component::input::TransformToTitleCase, window, cx);
            }),
        )
        .on_action(
            cx.listener(|this, _: &EditorTrimTrailingWhitespace, window, cx| {
                if !this.editor_has_focus(window, cx) {
                    cx.propagate();
                    return;
                }
                this.editor_dispatch(&gpui_component::input::TrimTrailingWhitespace, window, cx);
            }),
        )
        .on_action(cx.listener(|this, _: &EditorJoinLines, window, cx| {
            if !this.editor_has_focus(window, cx) {
                cx.propagate();
                return;
            }
            this.editor_dispatch(&gpui_component::input::JoinLines, window, cx);
        }))
        .on_action(
            cx.listener(|this, _: &EditorRemoveSurroundingBrackets, window, cx| {
                if !this.editor_has_focus(window, cx) {
                    cx.propagate();
                    return;
                }
                this.editor_dispatch(
                    &gpui_component::input::RemoveSurroundingBrackets,
                    window,
                    cx,
                );
            }),
        )
    }

    /// Whether an editor exists to run [`Self::editor_dispatch`] on.
    pub(crate) fn editor_can_dispatch(&self) -> bool {
        self.code_panel_visible()
            && self.external_editor_index().is_none()
            && self.active_buffer().is_some()
    }

    pub(crate) fn editor_has_focus(&self, window: &Window, cx: &Context<Self>) -> bool {
        self.code_panel_visible()
            && self.external_editor_index().is_none()
            && self.active_buffer().is_some_and(|f| {
                f.input
                    .read(cx)
                    .focus_handle(cx)
                    .contains_focused(window, cx)
            })
    }

    /// Whether anything in the editor panel — the text, its search bar, the
    /// go-to-line prompt — has the focus. Wider than
    /// [`Self::editor_has_focus`], for keys that belong to the panel as a
    /// whole: ⌘S from the find box should still save.
    pub(crate) fn editor_panel_has_focus(&self, window: &Window, cx: &Context<Self>) -> bool {
        self.editor_has_focus(window, cx)
            || (self.code_panel_visible() && self.editor.empty_focus.is_focused(window))
            || self.editor.bar.as_ref().is_some_and(|b| {
                b.input
                    .read(cx)
                    .focus_handle(cx)
                    .contains_focused(window, cx)
            })
    }

    pub(crate) fn editor_save_active(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.tab_code().and_then(TabCode::active_id) else {
            return;
        };
        self.editor_save_file(id, false, false, window, cx);
    }

    pub(crate) fn editor_save_as_active(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.tab_code().and_then(TabCode::active_id) else {
            return;
        };
        self.editor_save_as(id, false, window, cx);
    }

    /// Applies the file's `.editorconfig` save rules — trailing whitespace,
    /// final newline — to the buffer itself, before it is written, so what is
    /// on screen is what is on disk.
    fn apply_save_rules(&mut self, id: BufferId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(f) = self.buffer(id) else {
            return;
        };
        let text = f.input.read(cx).text().to_string();
        if let Some(fixed) = editor_text::apply_save_rules(&text, &f.config) {
            let input = f.input.clone();
            replace_buffer_text(&input, &fixed, window, cx);
        }
    }

    fn editor_save_file(
        &mut self,
        id: BufferId,
        then_close: bool,
        force: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(f) = self.buffer_mut(id) else {
            return;
        };
        if f.untitled.is_some() {
            self.editor_save_as(id, then_close, window, cx);
            return;
        }
        f.save_then_close |= then_close;
        if f.saving {
            f.save_pending = true;
            return;
        }
        self.apply_save_rules(id, window, cx);
        let Some(f) = self.buffer_mut(id) else {
            return;
        };
        let text = f.input.read(cx).text().to_string();
        // Pasted text can carry its own `\r\n`; the buffer's line ending is
        // the file's, and encoding adds it back uniformly.
        let text = if text.contains("\r\n") {
            text.replace("\r\n", "\n")
        } else {
            text
        };
        let bytes = match editor_text::encode(&text, &f.format) {
            Ok(bytes) => bytes,
            Err(e) => {
                self.editor_offer_utf8(id, then_close, e.unmappable, window, cx);
                return;
            }
        };
        let Some(f) = self.buffer_mut(id) else {
            return;
        };
        f.saving = true;
        let expect = match (force, f.conflict) {
            (true, _) | (_, Some(DiskConflict::Deleted)) => None,
            _ => Some(f.disk_mtime),
        };
        let written = Fingerprint::of_str(&text);
        let format = f.format;
        let host = f.host.clone();
        let target = f.path.clone();
        let host_id = host.id();
        let saved_in = target.parent().map(Path::to_path_buf);
        HostOps::run_in(
            host,
            window,
            cx,
            move |h| write_checked(h, &target, &bytes, expect),
            move |app, outcome: SaveOutcome, window, cx| {
                let Some(f) = app.buffer_mut(id) else {
                    return;
                };
                f.saving = false;
                let pending = std::mem::take(&mut f.save_pending);
                match outcome {
                    SaveOutcome::Saved(mtime) => {
                        f.disk_mtime = mtime;
                        f.saved = written;
                        f.saved_format = format;
                        f.conflict = None;
                        app.editor_note_edit(id, cx);
                        app.lsp_buffer_saved(id, cx);
                        app.editor_gutter_refetch(id);
                        // A save is a working-tree edit the `.git` watch cannot
                        // see, and the file tree only sees it while it happens
                        // to be showing that directory.
                        if let Some(dir) = &saved_in {
                            app.scm_invalidate_cwd(host_id, dir, cx);
                        }
                        if pending {
                            app.editor_save_file(id, false, false, window, cx);
                            return;
                        }
                        let close = app
                            .buffer_mut(id)
                            .is_some_and(|f| std::mem::take(&mut f.save_then_close) && !f.dirty);
                        if close {
                            app.editor_drop_buffer(id, cx);
                        }
                        app.editor_saves_landed(window, cx);
                    }
                    SaveOutcome::Conflict(observed) => {
                        f.save_then_close = false;
                        f.conflict = Some(DiskConflict::Changed(observed));
                        app.editor_saves_failed(id);
                        app.editor_ask_overwrite(id, window, cx);
                    }
                    SaveOutcome::Failed(e) => {
                        f.save_then_close = false;
                        // "Save failed" did not say which file, and with more
                        // than one editor tab open that is the first thing you
                        // need to know.
                        let context = t_fmt(L10nKey::EditorSaveFailed, &[("name", &f.label())]);
                        HostOps::notify_err(window, cx, &context, &e);
                        app.editor_saves_failed(id);
                    }
                }
                cx.notify();
            },
        );
        cx.notify();
    }

    /// Asked when a save finds the file changed underneath it.
    fn editor_ask_overwrite(&mut self, id: BufferId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(f) = self.buffer(id) else {
            return;
        };
        let name = f.label();
        let answer = window.prompt(
            PromptLevel::Warning,
            &t_fmt(L10nKey::EditorSaveConflictTitle, &[("name", &name)]),
            Some(t(L10nKey::EditorSaveConflictBody)),
            &crate::ui::confirm_answers(t(L10nKey::EditorOverwrite), t(L10nKey::Cancel)),
            cx,
        );
        cx.spawn_in(window, async move |app, cx| {
            if !matches!(answer.await, Ok(0)) {
                return;
            }
            let _ = app.update_in(cx, |app, window, cx| {
                app.editor_save_file(id, false, true, window, cx);
            });
        })
        .detach();
    }

    /// Asked when the text holds a character the file's encoding cannot.
    fn editor_offer_utf8(
        &mut self,
        id: BufferId,
        then_close: bool,
        unmappable: char,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(f) = self.buffer_mut(id) else {
            return;
        };
        f.save_then_close = false;
        let name = f.label();
        let encoding = f.format.encoding_label();
        self.editor_saves_failed(id);
        let answer = window.prompt(
            PromptLevel::Warning,
            &t_fmt(
                L10nKey::EditorEncodeFailedTitle,
                &[("name", &name), ("encoding", encoding)],
            ),
            Some(&t_fmt(
                L10nKey::EditorEncodeFailedBody,
                &[("ch", &unmappable.to_string()), ("encoding", encoding)],
            )),
            &crate::ui::confirm_answers(t(L10nKey::EditorSaveAsUtf8), t(L10nKey::Cancel)),
            cx,
        );
        cx.spawn_in(window, async move |app, cx| {
            if !matches!(answer.await, Ok(0)) {
                return;
            }
            let _ = app.update_in(cx, |app, window, cx| {
                if let Some(f) = app.buffer_mut(id) {
                    f.format = TextFormat {
                        line_ending: f.format.line_ending,
                        ..TextFormat::default()
                    };
                }
                app.editor_save_file(id, then_close, false, window, cx);
            });
        })
        .detach();
    }

    /// Save As: the native panel for a file on this machine, the path bar for
    /// one on a machine the panel cannot browse.
    fn editor_save_as(
        &mut self,
        id: BufferId,
        then_close: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(f) = self.buffer(id) else {
            return;
        };
        let suggested = if f.untitled.is_some() {
            PathBuf::from(format!("{}.txt", f.label()))
        } else {
            PathBuf::from(f.label().to_string())
        };
        let dir = match f.path.parent() {
            Some(dir) if f.untitled.is_none() => dir.to_path_buf(),
            _ => self
                .tab_code()
                .and_then(|c| c.roots.first().cloned())
                .unwrap_or_default(),
        };
        if !f.host.id().is_local() {
            let start = if dir.as_os_str().is_empty() {
                suggested
            } else {
                f.host.join(&dir, &suggested.to_string_lossy())
            };
            self.editor_open_bar(
                BarKind::SaveAs { id, then_close },
                start.display().to_string(),
                window,
                cx,
            );
            return;
        }
        let dir = if dir.as_os_str().is_empty() {
            dirs_home()
        } else {
            dir
        };
        let rx = cx.prompt_for_new_path(&dir, suggested.to_str());
        cx.spawn_in(window, async move |app, cx| {
            let Ok(Ok(Some(path))) = rx.await else {
                // Cancelled: a close that was waiting for this save must not
                // go ahead on some later, unrelated one.
                let _ = app.update(cx, |app, _cx| app.editor_saves_failed(id));
                return;
            };
            let _ = app.update_in(cx, |app, window, cx| {
                app.editor_save_to(id, path, then_close, window, cx);
            });
        })
        .detach();
    }

    /// Writes a buffer to a new path and points it there from then on.
    fn editor_save_to(
        &mut self,
        id: BufferId,
        path: PathBuf,
        then_close: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(f) = self.buffer(id) else {
            return;
        };
        let host = f.host.clone();
        if let Some(other) = self.buffer_at(host.id(), &path)
            && other != id
        {
            window.push_notification(
                t_fmt(
                    L10nKey::EditorAlreadyOpen,
                    &[("path", &path.display().to_string())],
                ),
                cx,
            );
            return;
        }
        if let Some(f) = self.buffer_mut(id) {
            f.path = path.clone();
            f.untitled = None;
            f.conflict = None;
            // A new file keeps the format the buffer had; its own
            // `.editorconfig` is read the next time it opens.
            let language = language_for_path(&path);
            f.input
                .update(cx, |st, cx| st.set_highlighter(language, cx));
        }
        self.editor_rebuild_watcher(cx);
        // Written as a save to a file already known to be the one intended:
        // the native panel has asked about replacing an existing file, and
        // the path bar asks before it gets here.
        self.editor_save_file(id, then_close, true, window, cx);
    }

    fn editor_open_bar(
        &mut self,
        kind: BarKind,
        initial: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let placeholder = match kind {
            BarKind::GoToLine => {
                let total = self
                    .active_buffer()
                    .map(|f| f.input.read(cx).text().lines_len())
                    .unwrap_or(1);
                t_fmt(
                    L10nKey::EditorGoToLinePlaceholder,
                    &[("total", &total.to_string())],
                )
            }
            BarKind::SaveAs { .. } => t(L10nKey::EditorSaveAsPlaceholder).to_string(),
            BarKind::Rename { .. } => t_fmt(L10nKey::LspRenamePlaceholder, &[("name", &initial)]),
        };
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(placeholder)
                .default_value(initial)
        });
        let sub = cx.subscribe_in(
            &input,
            window,
            |this: &mut Tty7App, _input, ev, window, cx| {
                match ev {
                    InputEvent::PressEnter { .. } => this.editor_submit_bar(window, cx),
                    // Clicking away from a go-to-line box is how you say never
                    // mind. A Save As is left up: it may be half-typed.
                    InputEvent::Blur => {
                        if matches!(
                            this.editor.bar.as_ref().map(|b| &b.kind),
                            Some(BarKind::GoToLine | BarKind::Rename { .. })
                        ) {
                            this.editor.bar = None;
                            cx.notify();
                        }
                    }
                    _ => {}
                }
            },
        );
        input.update(cx, |st, cx| st.focus(window, cx));
        self.editor.bar = Some(EditorBar {
            kind,
            input,
            _sub: sub,
        });
        cx.notify();
    }

    pub(crate) fn editor_go_to_line(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.code_panel_visible() || self.active_buffer().is_none() {
            return;
        }
        if let Some(f) = self.tab_code().and_then(TabCode::active_id)
            && let Some(f) = self.buffer_mut(f)
        {
            // There is no line to land on in the rendered view.
            f.preview = false;
        }
        self.editor_open_bar(BarKind::GoToLine, String::new(), window, cx);
    }

    fn editor_close_bar(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(BarKind::SaveAs { id, .. }) = self.editor.bar.as_ref().map(|b| &b.kind) {
            let id = *id;
            self.editor_saves_failed(id);
        }
        self.editor.bar = None;
        self.focus_editor(window, cx);
        cx.notify();
    }

    fn editor_submit_bar(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(bar) = self.editor.bar.as_ref() else {
            return;
        };
        let text = bar.input.read(cx).value().to_string();
        match bar.kind {
            BarKind::GoToLine => {
                let Some((line, column)) = parse_line_target(&text) else {
                    return;
                };
                self.editor_close_bar(window, cx);
                let Some(f) = self.active_buffer() else {
                    return;
                };
                let input = f.input.clone();
                let lines = input.read(cx).text().lines_len().max(1) as u32;
                let position = Position {
                    line: line.min(lines) - 1,
                    character: column - 1,
                };
                place_cursor(input, position, CURSOR_SCROLL_ATTEMPTS, window, cx);
            }
            BarKind::Rename { id, offset } => {
                let name = text.trim().to_string();
                if name.is_empty() {
                    return;
                }
                self.editor_close_bar(window, cx);
                self.lsp_rename(id, offset, name, window, cx);
            }
            BarKind::SaveAs { id, then_close } => {
                let path = PathBuf::from(text.trim());
                if path.as_os_str().is_empty() {
                    return;
                }
                let Some(host) = self.buffer(id).map(|f| f.host.clone()) else {
                    return;
                };
                if !host.is_absolute(&path) {
                    return;
                }
                self.editor.bar = None;
                cx.notify();
                let check = path.clone();
                HostOps::run_in(
                    host,
                    window,
                    cx,
                    move |h| h.exists(&check),
                    move |app, exists: bool, window, cx| {
                        if !exists {
                            app.editor_save_to(id, path, then_close, window, cx);
                            return;
                        }
                        let answer = window.prompt(
                            PromptLevel::Warning,
                            &t_fmt(
                                L10nKey::EditorReplaceExisting,
                                &[("path", &path.display().to_string())],
                            ),
                            None,
                            &crate::ui::confirm_answers(
                                t(L10nKey::EditorReplace),
                                t(L10nKey::Cancel),
                            ),
                            cx,
                        );
                        cx.spawn_in(window, async move |app, cx| {
                            if !matches!(answer.await, Ok(0)) {
                                return;
                            }
                            let _ = app.update_in(cx, |app, window, cx| {
                                app.editor_save_to(id, path, then_close, window, cx);
                            });
                        })
                        .detach();
                    },
                );
            }
        }
    }

    /// Closes the file at `pos` in the active tab's editor. Asks first only
    /// when this is the last tab showing a buffer with unsaved changes.
    pub(crate) fn editor_close_file(
        &mut self,
        pos: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let tab_ix = self.active;
        let Some(id) = self.tab_code().and_then(|c| c.files.get(pos).copied()) else {
            return;
        };
        let Some(f) = self.buffer(id) else {
            return;
        };
        if !f.dirty || self.buffer_refs(id) > 1 {
            self.editor_remove_from_tab(tab_ix, id, cx);
            return;
        }
        let name = f.label();
        let answer = window.prompt(
            PromptLevel::Warning,
            &t_fmt(L10nKey::EditorUnsavedChanges, &[("name", &name)]),
            None,
            // Cancel sits between Save and Discard on purpose. The platform
            // renders the first button as the default and lays the rest out
            // beside it, so Discard was landing directly next to the key that
            // Return presses. Apple separates them for exactly this reason.
            // Three answers, so the shared helper does not fit: Save keeps
            // index 0 (rightmost, Return), Cancel takes Escape, and Discard
            // sits on the far left where nothing lands by reflex.
            &[
                gpui::PromptButton::ok(t(L10nKey::Save)),
                gpui::PromptButton::cancel(t(L10nKey::Cancel)),
                gpui::PromptButton::ok(t(L10nKey::EditorDiscard)),
            ],
            cx,
        );
        cx.spawn_in(window, async move |app, cx| {
            let Ok(choice) = answer.await else { return };
            let _ = app.update_in(cx, |app, window, cx| match choice {
                0 => app.editor_save_file(id, true, false, window, cx),
                2 => app.editor_drop_buffer(id, cx),
                _ => {}
            });
        })
        .detach();
    }

    /// Types `@path` into the running agent's prompt, with the selected
    /// lines after it when there are any.
    fn editor_attach_to_agent(&mut self, id: BufferId, cx: &mut Context<Self>) {
        let Some(f) = self.buffer(id).filter(|f| f.untitled.is_none()) else {
            return;
        };
        let state = f.input.read(cx);
        let suffix = match selected_lines(state.text(), state.selected_range()) {
            Some((a, b)) if a == b => format!("#L{a}"),
            Some((a, b)) => format!("#L{a}-{b}"),
            None => String::new(),
        };
        let path = f.path.clone();
        self.attach_path_to_agent(&path, &suffix, cx);
    }

    /// Right-click in the text. Edits dispatch to the editor itself, so each
    /// row shows the chord that does the same thing.
    fn editor_body_menu(
        menu: PopupMenu,
        app: &gpui::WeakEntity<Self>,
        id: BufferId,
        cx: &gpui::App,
    ) -> PopupMenu {
        use gpui_component::input as edit;
        let Some(this) = app.upgrade() else {
            return menu;
        };
        let this = this.read(cx);
        let Some(f) = this.buffer(id) else {
            return menu;
        };
        let state = f.input.read(cx);
        let selected = !state.selected_range().is_empty();
        let mut menu = menu.min_w(px(220.)).action_context(state.focus_handle(cx));
        if f.untitled.is_none() {
            menu = menu
                .item(
                    PopupMenuItem::new(t(L10nKey::FileTreeContextAttachAgent)).on_click({
                        let app = app.clone();
                        move |_, _window, cx| {
                            let _ = app.update(cx, |this, cx| this.editor_attach_to_agent(id, cx));
                        }
                    }),
                )
                .separator();
        }
        let menu = menu
            .menu(t(L10nKey::AppMenuUndo), Box::new(edit::Undo))
            .menu(t(L10nKey::AppMenuRedo), Box::new(edit::Redo))
            .separator()
            .menu_with_disabled(t(L10nKey::AppMenuCut), Box::new(edit::Cut), !selected)
            .menu_with_disabled(t(L10nKey::AppMenuCopy), Box::new(edit::Copy), !selected)
            .menu_with_disabled(
                t(L10nKey::AppMenuPaste),
                Box::new(edit::Paste),
                cx.read_from_clipboard().is_none(),
            )
            .menu(t(L10nKey::AppMenuSelectAll), Box::new(edit::SelectAll))
            .separator()
            // The line commands act on every line a cursor or selection
            // touches. Their chords live on gpui-component's `CodeEditor` key
            // context and are re-added in `keymap::fixed_bindings`, so they
            // beat the app's own keys while the editor has focus.
            .menu(
                t(L10nKey::EditorToggleComment),
                Box::new(edit::ToggleLineComment),
            )
            .menu(t(L10nKey::EditorMoveLineUp), Box::new(edit::MoveLineUp))
            .menu(t(L10nKey::EditorMoveLineDown), Box::new(edit::MoveLineDown))
            .menu(
                t(L10nKey::EditorDuplicateLine),
                Box::new(edit::CopyLineDown),
            )
            .menu(t(L10nKey::EditorDeleteLine), Box::new(edit::DeleteLine))
            .separator()
            .menu(t(L10nKey::AppMenuFind), Box::new(edit::Search))
            .menu(
                t(L10nKey::EditorGoToLineAction),
                Box::new(crate::core::actions::EditorGoToLine),
            )
            .menu_with_disabled(
                t(L10nKey::EditorGitRevertChange),
                Box::new(crate::core::actions::EditorRevertChange),
                !this.editor_gutter_can_revert(id, cx),
            )
            .menu(
                t(L10nKey::EditorGoToMatchingBracket),
                Box::new(edit::MoveToMatchingBracket),
            );
        let menu = this.lsp_menu_items(menu, id, cx);
        this.editor_file_menu_items(menu, id, app, cx)
    }

    /// What both menus offer for the file as a whole: open it outside tty7,
    /// show it in its folder, copy where it is. Nothing for a file that has
    /// never been saved, since it is not anywhere yet.
    fn editor_file_menu_items(
        &self,
        menu: PopupMenu,
        id: BufferId,
        app: &gpui::WeakEntity<Self>,
        cx: &gpui::App,
    ) -> PopupMenu {
        let Some(f) = self.buffer(id).filter(|f| f.untitled.is_none()) else {
            return menu;
        };
        let path = f.path.clone();
        // Only this machine's desktop can open or show a file, and only a
        // file that is on this machine — the same rule as the file tree.
        let local = f.host.id().is_local();
        let mut menu = menu.separator();
        if local && self.can_spawn_locally(cx) && !is_program(&path) {
            let browser = opens_in_browser(&path);
            let label = match browser {
                true => t(L10nKey::PanelOpenInBrowser),
                false => t(L10nKey::AppMenuOpenLinkWithDefaultApp),
            };
            let opener: fn(&Path) -> std::io::Result<()> = match browser {
                true => open_in_browser,
                false => crate::terminal::view::open_file_path,
            };
            menu = menu.item(PopupMenuItem::new(label).on_click({
                let app = app.clone();
                let path = path.clone();
                move |_, window, cx| {
                    let _ = app.update(cx, |_, cx| Self::open_with(&path, opener, window, cx));
                }
            }));
        }
        if local {
            menu = menu.item(
                PopupMenuItem::new(crate::ui::right_panel::reveal_label()).on_click({
                    let path = path.clone();
                    move |_, _window, cx| {
                        cx.reveal_path(&crate::ui::path_display::native_separators(&path));
                    }
                }),
            );
        }
        let copy = |label: L10nKey, text: String| {
            PopupMenuItem::new(t(label)).on_click(move |_, _window, cx| {
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(text.clone()));
            })
        };
        // A remote host's paths are already spelled its own way; only one on
        // this machine is re-spelled with its separators.
        let full = match local {
            true => crate::ui::path_display::native_separators(&path)
                .display()
                .to_string(),
            false => path.display().to_string(),
        };
        menu = menu.item(copy(L10nKey::FileTreeContextCopyPath, full));
        if let Some(rel) = self.path_under_tree_root(&path) {
            let rel = match local {
                true => crate::ui::path_display::native_separators(&rel)
                    .display()
                    .to_string(),
                false => rel.display().to_string(),
            };
            menu = menu.item(copy(L10nKey::EditorCopyRelativePath, rel));
        }
        menu
    }

    pub(crate) fn editor_close_active_if_focused(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.code_panel_visible()
            && let Some(index) = self.external_editor_index()
            && self.tabs[index].pane.focused_leaf(window, cx).is_some()
        {
            self.close_external_editor(self.tabs[index].tree_id.get(), window, cx);
            return true;
        }
        let empty = self
            .tab_code()
            .is_some_and(|c| c.visible && c.external.is_none() && c.files.is_empty());
        // Ctrl+3 on an empty panel hands the focus to the file tree, so from
        // there ⌘W is the empty panel's too.
        let tree = empty && self.file_tree.focus_handle.contains_focused(window, cx);
        if !(tree || self.editor_panel_has_focus(window, cx)) {
            return false;
        }
        let Some(code) = self.tab_code_mut() else {
            return false;
        };
        if code.files.is_empty() {
            code.visible = false;
            self.document_closing = None;
            self.editor.had_focus = None;
            self.editor.bar = None;
            self.file_tree.editing = None;
            if self.terminal_visible {
                self.focus_active(window, cx);
            }
            cx.notify();
            return true;
        }
        let active = code.active;
        self.editor_close_file(active, window, cx);
        true
    }

    /// Takes a buffer out of one tab's editor, and drops it once no tab shows
    /// it any more.
    fn editor_remove_from_tab(&mut self, tab_ix: usize, id: BufferId, cx: &mut Context<Self>) {
        if let Some(code) = self
            .tabs
            .get_mut(tab_ix)
            .and_then(|t| t.code.as_deref_mut())
        {
            code.close_in_focused(id);
        }
        if self.buffer_refs(id) == 0 {
            self.editor_drop_buffer(id, cx);
        }
        cx.notify();
    }

    /// Forgets a buffer everywhere, edits and all.
    fn editor_drop_buffer(&mut self, id: BufferId, cx: &mut Context<Self>) {
        // Whatever was waiting on this buffer to be saved is not going to
        // see that happen now.
        self.editor_saves_failed(id);
        for code in self.tabs.iter_mut().filter_map(|t| t.code.as_deref_mut()) {
            code.forget_everywhere(id);
        }
        self.editor.buffers.retain(|b| b.id() != id);
        self.editor.nav.forget_buffer(id);
        if matches!(
            self.editor.bar.as_ref().map(|b| &b.kind),
            Some(BarKind::SaveAs { id: bar_id, .. }) if *bar_id == id
        ) {
            self.editor.bar = None;
        }
        self.editor_rebuild_watcher(cx);
        cx.notify();
    }

    // ---- Unsaved changes standing in the way of a close ----

    /// Asks about unsaved buffers before `then` goes ahead. Returns `true`
    /// when there was something to ask about: the caller stops there, and the
    /// answer carries the close on — after saving, after discarding, or not
    /// at all.
    pub(crate) fn editor_guard_unsaved(
        &mut self,
        ids: Vec<BufferId>,
        then: AfterUnsaved,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if ids.is_empty() {
            return false;
        }
        if self.editor.unsaved_prompt_open {
            return true;
        }
        self.editor.unsaved_prompt_open = true;
        let names: Vec<SharedString> = ids
            .iter()
            .filter_map(|id| self.buffer(*id).map(OpenFile::label))
            .collect();
        let title = match names.as_slice() {
            [one] => t_fmt(L10nKey::EditorUnsavedChanges, &[("name", one)]),
            _ => t_fmt(
                L10nKey::EditorUnsavedChangesMany,
                &[("count", &names.len().to_string())],
            ),
        };
        let mut body: Vec<String> = names.iter().take(8).map(|n| n.to_string()).collect();
        if names.len() > 8 {
            body.push("…".into());
        }
        let body = (names.len() > 1).then(|| body.join("\n"));
        let answer = window.prompt(
            PromptLevel::Warning,
            &title,
            body.as_deref(),
            // The same arrangement as closing a single file, and for the same
            // reason: Discard as far from Return as the dialog allows.
            &[
                gpui::PromptButton::ok(if names.len() > 1 {
                    t(L10nKey::EditorSaveAll)
                } else {
                    t(L10nKey::Save)
                }),
                gpui::PromptButton::cancel(t(L10nKey::Cancel)),
                gpui::PromptButton::ok(t(L10nKey::EditorDiscard)),
            ],
            cx,
        );
        // Showing it is part of the question: the file being asked about may
        // be behind the terminal, in a tab that is not the one in front.
        if let Some(tab_ix) = ids.first().and_then(|id| {
            self.tabs
                .iter()
                .position(|t| t.code.as_deref().is_some_and(|c| c.shows(*id)))
        }) && tab_ix == self.active
            && let Some(code) = self.tab_code_mut()
            && let Some(pos) = code.files.iter().position(|f| *f == ids[0])
        {
            code.active = pos;
            code.visible = true;
        }
        cx.spawn_in(window, async move |app, cx| {
            let choice = answer.await;
            let _ = app.update_in(cx, |app, window, cx| {
                app.editor.unsaved_prompt_open = false;
                match choice {
                    Ok(0) => {
                        app.editor.waiters.push(SaveWaiter {
                            ids: ids.clone(),
                            then,
                        });
                        for id in ids {
                            app.editor_save_file(id, false, false, window, cx);
                        }
                    }
                    Ok(2) => {
                        for id in ids {
                            app.editor_drop_buffer(id, cx);
                        }
                        app.editor_continue(then, window, cx);
                    }
                    _ => {
                        // The answer that backs out leaves a file that was
                        // only open to be shown with nowhere to go; cleanup
                        // takes it from here.
                        app.editor.pending_show = None;
                        cx.notify();
                    }
                }
            });
        })
        .detach();
        true
    }

    /// Runs whatever close was waiting on saves that have now all landed.
    fn editor_saves_landed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (ready, waiting): (Vec<SaveWaiter>, Vec<SaveWaiter>) =
            std::mem::take(&mut self.editor.waiters)
                .into_iter()
                .partition(|w| {
                    w.ids.iter().all(|id| {
                        self.buffer(*id)
                            .is_none_or(|f| !f.dirty && !f.saving && f.untitled.is_none())
                    })
                });
        self.editor.waiters = waiting;
        for w in ready {
            self.editor_continue(w.then, window, cx);
        }
    }

    /// A save that was part of a close did not happen, so neither does the
    /// close: whatever it would have thrown away is still unsaved.
    fn editor_saves_failed(&mut self, id: BufferId) {
        self.editor.waiters.retain(|w| !w.ids.contains(&id));
    }

    fn editor_continue(&mut self, then: AfterUnsaved, window: &mut Window, cx: &mut Context<Self>) {
        match then {
            AfterUnsaved::CloseTab(tab) => {
                if let Some(ix) = self.tab_index_of(tab) {
                    self.close_tab(ix, window, cx);
                }
            }
            AfterUnsaved::ShowInTab(tab, id) => {
                self.editor.pending_show = None;
                self.editor_show_in_tab(tab, id, true, window, cx);
            }
            AfterUnsaved::ClosePane => self.close_pane_after_unsaved(window, cx),
            AfterUnsaved::CloseWindow => self.close_window_after_unsaved(window, cx),
            AfterUnsaved::Quit => self.quit_after_unsaved(window, cx),
        }
    }

    // ---- The file tree moving files out from under their buffers ----

    /// A file or folder was renamed through tty7: buffers at or under the old
    /// path follow it. Without this the buffer kept the old name, and the next
    /// save wrote a second copy there.
    pub(crate) fn editor_path_moved(
        &mut self,
        host: HostId,
        from: &Path,
        to: &Path,
        cx: &mut Context<Self>,
    ) {
        let mut moved = false;
        for f in self.editor.buffers.iter_mut() {
            if f.untitled.is_some() || f.host.id() != host {
                continue;
            }
            let Ok(rest) = f.path.strip_prefix(from) else {
                continue;
            };
            f.path = if rest.as_os_str().is_empty() {
                to.to_path_buf()
            } else {
                to.join(rest)
            };
            let language = language_for_path(&f.path);
            f.input
                .update(cx, |st, cx| st.set_highlighter(language, cx));
            moved = true;
        }
        if moved {
            self.editor_rebuild_watcher(cx);
            cx.notify();
        }
    }

    /// A file or folder was deleted through tty7: its buffers say so rather
    /// than waiting for the watcher, which a remote host may not have.
    pub(crate) fn editor_path_removed(
        &mut self,
        host: HostId,
        path: &Path,
        cx: &mut Context<Self>,
    ) {
        for f in self.editor.buffers.iter_mut() {
            if f.untitled.is_none() && f.host.id() == host && f.path.starts_with(path) {
                f.conflict = Some(DiskConflict::Deleted);
                f.disk_mtime = None;
            }
        }
        cx.notify();
    }

    // ---- Changes made by something else ----

    pub(crate) fn editor_handle_external_change(
        &mut self,
        path: &Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The watcher's own host: the events came from its machine, whatever
        // the window has moved on to since.
        let Some(host) = self.editor.watch_host.clone() else {
            return;
        };
        let host_id = host.id();
        let p = path.to_path_buf();
        let landed = p.clone();
        HostOps::run_in(
            host,
            window,
            cx,
            move |h| match h.stat(&p) {
                Ok(m) => Observed::Present(m.mtime),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Observed::Missing,
                // Unreadable for some other reason: say nothing rather than
                // call a file deleted that may well still be there.
                Err(_) => Observed::Present(None),
            },
            move |app, observed, window, cx| {
                app.editor_apply_external_change(host_id, &landed, observed, window, cx)
            },
        );
    }

    fn editor_apply_external_change(
        &mut self,
        host: HostId,
        path: &Path,
        observed: Observed,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(id) = self.buffer_at(host, path) else {
            return;
        };
        let Some(f) = self.buffer_mut(id) else {
            return;
        };
        match classify_external_change(f.saving, f.dirty, f.disk_mtime, observed) {
            ExternalChange::Ignore => {}
            ExternalChange::Conflict => {
                let Observed::Present(mtime) = observed else {
                    return;
                };
                f.conflict = Some(DiskConflict::Changed(mtime));
                cx.notify();
            }
            ExternalChange::Deleted => {
                f.conflict = Some(DiskConflict::Deleted);
                f.disk_mtime = None;
                cx.notify();
            }
            ExternalChange::Reload => self.editor_reload_from_disk(id, window, cx),
        }
    }

    pub(crate) fn editor_reload_from_disk(
        &mut self,
        id: BufferId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(f) = self.buffer_mut(id) else {
            return;
        };
        if f.untitled.is_some() {
            return;
        }
        let target = f.path.clone();
        let host = f.host.clone();
        f.reload_seq = f.reload_seq.wrapping_add(1);
        let seq = f.reload_seq;
        HostOps::run_in(
            host,
            window,
            cx,
            move |h| {
                let bytes = h.read_file(&target, MAX_FILE_BYTES)?;
                let decoded = editor_text::decode(&bytes).ok_or_else(|| {
                    std::io::Error::new(std::io::ErrorKind::InvalidData, "no longer text")
                })?;
                let mtime = h.stat(&target).ok().and_then(|m| m.mtime);
                Ok((decoded, mtime))
            },
            move |app,
                  result: std::io::Result<(editor_text::Decoded, Option<MTime>)>,
                  window,
                  cx| {
                let Some(f) = app.buffer_mut(id) else {
                    return;
                };
                if f.reload_seq != seq {
                    return;
                }
                let (decoded, mtime) = match result {
                    Ok(ok) => ok,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        f.conflict = Some(DiskConflict::Deleted);
                        f.disk_mtime = None;
                        cx.notify();
                        return;
                    }
                    // Unreadable for a moment — mid-write, say — is not a
                    // reason to touch the buffer; the next change event tries
                    // again.
                    Err(e) => {
                        log::warn!("editor: reload of {} failed: {e}", f.path.display());
                        return;
                    }
                };
                f.disk_mtime = mtime;
                f.conflict = None;
                f.saved = Fingerprint::of_str(&decoded.text);
                f.format = decoded.format;
                f.saved_format = decoded.format;
                let input = f.input.clone();
                replace_buffer_text(&input, &decoded.text, window, cx);
                // The replace reports its own change, but only once effects
                // flush; settle it now so nothing reads a stale flag first.
                app.editor_note_edit(id, cx);
            },
        );
    }

    // ---- Keeping the registry, the tabs and the session store in step ----

    /// Once per frame: buffers no tab shows any more are dropped — or, if
    /// they hold unsaved work, handed to the tab in front rather than lost —
    /// the active tab gets back the files it had open last time, and any
    /// tab whose files changed is recorded for next time.
    ///
    /// Done here rather than at each place a tab can go — a close, a shell
    /// that exits, a tab dragged into another, a workspace switch, a server
    /// restart — because this sees all of them, including ones added later.
    pub(crate) fn editor_sync(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Older sessions may restore with the editor's backing tab selected.
        // Seat it in the document slot before anything is drawn.
        if self.is_external_editor_tab(self.active, cx) {
            let editor = self.tabs[self.active].tree_id.get();
            let origin = self
                .external_editor_tabs
                .iter()
                .find(|entry| entry.tab == editor)
                .and_then(|entry| entry.terminal)
                .and_then(|id| self.tab_index_of(id))
                .filter(|&index| !self.is_external_editor_tab(index, cx))
                .or_else(|| {
                    (0..self.tabs.len()).find(|&index| !self.is_external_editor_tab(index, cx))
                });
            if let Some(index) = origin {
                self.active = index;
            } else if self.new_tab_slot(None, None, window, cx).is_none() {
                return;
            }
            self.select_external_editor(editor, window, cx);
        }
        // Exiting :q or closing a backing session must not leave a blank
        // document selected, nor change the terminal the user is working in.
        for index in 0..self.tabs.len() {
            let Some(selected) = self.tabs[index]
                .code
                .as_ref()
                .and_then(|code| code.external)
            else {
                continue;
            };
            if self.tab_index_of(selected).is_some() {
                continue;
            }
            let owner = self.tabs[index].tree_id.get();
            let next = self
                .external_editor_tabs
                .iter()
                .rev()
                .find(|entry| {
                    entry.terminal == Some(owner) && self.tab_index_of(entry.tab).is_some()
                })
                .map(|entry| entry.tab);
            let code = self.tabs[index].code.as_mut().unwrap();
            code.external = next;
            if next.is_none() && code.files.is_empty() {
                code.visible = false;
            }
            if index == self.active {
                if self.code_panel_visible() {
                    self.focus_editor(window, cx);
                } else {
                    self.terminal_visible = true;
                    self.focus_active(window, cx);
                }
            }
        }
        // A tab that left and came back under the same id — a server restart
        // rebuilds every tab that way — gets its files back like a relaunch.
        if self.editor.restored.len() > self.tabs.len() {
            let live: HashSet<TabId> = self.tabs.iter().map(|t| t.tree_id.get()).collect();
            self.editor.restored.retain(|id| live.contains(id));
        }
        self.editor_adopt_orphans(window, cx);
        self.editor_restore_active(window, cx);
        self.editor_record_sessions(cx);
    }

    fn editor_adopt_orphans(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // No tabs at all is the gap in the middle of a server restart or a
        // workspace switch, not a verdict on the buffers: wait for the tabs.
        if self.editor.buffers.is_empty() || self.tabs.is_empty() {
            return;
        }
        let shown: HashSet<BufferId> = self
            .tabs
            .iter()
            .filter_map(|t| t.code.as_deref())
            .flat_map(TabCode::all_files)
            .collect();
        let orphans: Vec<(BufferId, bool)> = self
            .editor
            .buffers
            .iter()
            .filter(|b| !shown.contains(&b.id()))
            .map(|b| (b.id(), b.dirty))
            .collect();
        if orphans.is_empty() {
            return;
        }
        // A tab shows one file, so an orphan can only be rescued into a tab
        // that has room: the one in front when it is empty, or when the file
        // it shows is clean. A tab whose own file is unsaved keeps it; the
        // orphan waits for the next opportunity rather than losing either.
        let current = self
            .tabs
            .get(self.active)
            .and_then(|t| t.code.as_deref())
            .and_then(TabCode::active_id);
        let current_dirty = current
            .and_then(|id| self.buffer(id))
            .is_some_and(|f| f.dirty);
        let Some(tab) = self.tabs.get(self.active).map(|t| t.tree_id.get()) else {
            return;
        };
        let mut changed = false;
        for (id, dirty) in orphans {
            // Loaded for a tab that is waiting on an unsaved-changes answer:
            // not an orphan, just not shown yet.
            if self.editor.pending_show == Some(id) {
                continue;
            }
            if !dirty {
                self.editor.buffers.retain(|b| b.id() != id);
                self.editor.nav.forget_buffer(id);
                changed = true;
                continue;
            }
            if current.is_some() && current_dirty {
                continue;
            }
            let name = self.buffer(id).map(OpenFile::label).unwrap_or_default();
            // An empty tab takes it in the background; a tab with a clean
            // file swaps it in front, since only one file can be shown.
            self.editor_show_in_tab(tab, id, current.is_some(), window, cx);
            window.push_notification(t_fmt(L10nKey::EditorOrphanAdopted, &[("name", &name)]), cx);
            changed = true;
        }
        if changed {
            self.editor_rebuild_watcher(cx);
            cx.notify();
        }
    }

    fn editor_restore_active(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(self.active) else {
            return;
        };
        let tab_id = tab.tree_id.get();
        if !self.editor.restored.insert(tab_id) {
            return;
        }
        if tab
            .code
            .as_deref()
            .is_some_and(|c| !c.files.is_empty() || c.external.is_some())
        {
            return;
        }
        let Some(state) = editor_session::get(cx, tab_id) else {
            return;
        };
        if let Some(external) = state.external.filter(|id| self.tab_index_of(*id).is_some()) {
            let code = self.tab_code_mut_or_init().unwrap();
            code.external = Some(external);
            code.visible = state.visible;
            return;
        }
        // One file: the one that was in front when it was recorded.
        let Some(path) = state
            .files
            .get(state.active.min(state.files.len().saturating_sub(1)))
            .or_else(|| state.files.first())
            .cloned()
        else {
            return;
        };
        let Some(host) = self.active_host(cx) else {
            return;
        };
        // What was recorded is what is being put back; recording it again
        // before the file has loaded would write down an empty tab.
        let visible = state.visible;
        self.editor.recorded.insert(tab_id, state);
        HostOps::run_in(
            host.clone(),
            window,
            cx,
            move |h| load_file(h, path).ok(),
            move |app, loaded, window, cx| {
                let Some(loaded) = loaded else {
                    return;
                };
                app.editor_finish_restore(host, loaded, tab_id, visible, window, cx);
            },
        );
    }

    /// A restore may finish after another file was opened or the tab closed.
    /// Only an empty, still-live tab takes it, without forcing the panel open.
    fn editor_finish_restore(
        &mut self,
        host: SharedHost,
        loaded: Loaded,
        tab: TabId,
        visible: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab_ix) = self.tab_index_of(tab) else {
            return;
        };
        if self.tabs[tab_ix]
            .code
            .as_deref()
            .is_some_and(|code| !code.files.is_empty() || code.external.is_some())
        {
            return;
        }
        self.editor_install(host, loaded, tab, false, window, cx);
        let code = self.tabs[tab_ix].code.as_deref_mut().unwrap();
        code.visible |= visible;
        if code.visible && tab_ix == self.active {
            self.focus_editor(window, cx);
        }
        cx.notify();
    }

    fn editor_record_sessions(&mut self, cx: &mut Context<Self>) {
        let spawn_host = self.spawn_host(cx);
        let mut changed: Vec<(TabId, TabEditor)> = Vec::new();
        for tab in &self.tabs {
            let Some(code) = tab.code.as_deref() else {
                continue;
            };
            let tab_id = tab.tree_id.get();
            // A tab still waiting for its file to come back has nothing to
            // say about it yet.
            if !self.editor.restored.contains(&tab_id) {
                continue;
            }
            // One file per tab: the one in front, when it is a local file
            // this window can open again next launch. An SFTP buffer's host
            // is a connection that will not exist then; an untitled buffer
            // has no path to come back to.
            let files: Vec<PathBuf> = code
                .active_id()
                .and_then(|id| self.buffer(id))
                .filter(|f| f.untitled.is_none() && f.host.id() == spawn_host)
                .map(|f| f.path.clone())
                .into_iter()
                .collect();
            let state = TabEditor {
                files,
                active: 0,
                visible: code.visible,
                external: code.external,
            };
            if self.editor.recorded.get(&tab_id) != Some(&state) {
                changed.push((tab_id, state));
            }
        }
        for (tab_id, state) in changed {
            self.editor.recorded.insert(tab_id, state.clone());
            editor_session::put(cx, tab_id, state);
        }
    }

    /// A tab is being closed: the files only it showed go with it. Unsaved
    /// ones were asked about before the close got here — except when a shell
    /// exiting took the tab, which cannot ask; those stay, and are handed to
    /// the tab in front.
    pub(crate) fn editor_close_tab_files(&mut self, tab_ix: usize, cx: &mut Context<Self>) {
        let Some(ids) = self
            .tabs
            .get(tab_ix)
            .and_then(|t| t.code.as_deref())
            .map(TabCode::all_files)
        else {
            return;
        };
        for id in ids {
            if self.buffer_tabs(id) == 1 && self.buffer(id).is_some_and(|b| !b.dirty) {
                self.editor_drop_buffer(id, cx);
            }
        }
    }

    /// A tab was closed for good: forget what it had open.
    pub(crate) fn editor_forget_tab(&mut self, tab: TabId, cx: &mut Context<Self>) {
        self.external_editor_tabs.retain(|editor| editor.tab != tab);
        self.editor.recorded.remove(&tab);
        self.editor.nav.forget_tab(tab);
        editor_session::remove(cx, tab);
    }
}

fn dirs_home() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

impl Tty7App {
    pub(crate) fn render_code_overlay(
        &mut self,
        chrome: DocumentChrome,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !self.code_panel_visible() {
            return None;
        }
        if self.external_editor_index().is_some() {
            return self.render_external_editor(chrome, window, cx);
        }
        // The last file closed with the focus in it: keep the focus in the
        // panel, or gpui reports it lost and it falls back to the terminal.
        let tab = self.tabs.get(self.active).map(|t| t.tree_id.get());
        if tab.is_some() && self.editor.had_focus == tab && self.active_buffer().is_none() {
            self.editor.empty_focus.focus(window, cx);
        }
        self.editor.had_focus = tab.filter(|_| self.editor_panel_has_focus(window, cx));
        self.editor_gutter_sync(cx);
        self.editor_nav_tick(cx);
        let breadcrumbs = self.render_editor_breadcrumbs(window, cx);
        let body = self.render_editor_body(self.tab_code().and_then(TabCode::active_id), 0, cx);
        let conflict_banner = self
            .active_buffer()
            .and_then(|f| f.conflict.map(|c| (f.id(), c)))
            .map(|(id, c)| self.render_editor_conflict_banner(id, c, cx));
        let bar = self.render_editor_bar(cx);

        let header = chrome
            .renders_own_header()
            .then(|| self.render_editor_header(chrome, window, cx));
        let editor_col = v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .children(header)
            .when_some(conflict_banner, |this, b| this.child(b))
            .children(bar)
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .min_h_0()
                    .h_full()
                    .children(breadcrumbs)
                    .child(div().flex_1().min_h_0().child(body)),
            )
            .children(self.render_editor_problems(window, cx));

        // The panel's own paint is the same either way; only the box is not.
        // Filling the workspace means stopping the window's translucency and
        // repainting the theme image the root's copy now sits under; docking
        // means sitting in the same plane as the right panel, which the column
        // wrapper has already painted.
        let shell = v_flex().id("code-panel");
        let shell = match chrome {
            DocumentChrome::Fill => shell
                .absolute()
                .inset_0()
                .occlude()
                // Opaque on purpose: this overlay covers the whole workspace
                // (everything but the detail panel) and an open file must never
                // let the window translucency / backdrop material show through
                // it. The preset's gradient fill is preserved, just with
                // alpha 1 — the same paint the settings overlay uses. The
                // theme background image is repainted on top of it, since the
                // root's copy now sits below this fill.
                .bg(crate::ui::theme::overlay_background(cx))
                .children(crate::ui::app::overlay_surface_layers(cx)),
            DocumentChrome::Dock | DocumentChrome::DockHoisted => shell.size_full().min_w_0(),
        };
        let shell = self.editor_nav_actions(shell, cx);
        Some(
            shell
                .on_key_down(cx.listener(|this, ev: &gpui::KeyDownEvent, window, cx| {
                    if ev.keystroke.key != "escape" {
                        return;
                    }
                    if this.editor_gutter_close_peek(cx) {
                        cx.stop_propagation();
                        return;
                    }
                    // Escape in the go-to-line or Save As box dismisses the
                    // box, not the whole editor.
                    if this.editor.bar.is_some() {
                        this.editor_close_bar(window, cx);
                        cx.stop_propagation();
                        return;
                    }
                    this.toggle_code_panel(window, cx);
                }))
                .child(h_flex().flex_1().min_h_0().w_full().child(editor_col))
                .child(self.render_code_status_bar(window, cx))
                .into_any_element(),
        )
    }

    /// One group's text: the file `id`, rendered or as source, or the empty
    /// panel. `slot` tells the two groups' elements apart.
    pub(crate) fn render_editor_body(
        &self,
        id: Option<BufferId>,
        slot: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match id.and_then(|id| self.buffer(id)) {
            None => self.render_editor_empty(cx).into_any_element(),
            Some(f) if f.preview => {
                let markdown = f.input.read(cx).text().to_string();
                let scroll = f.preview_scroll.clone();
                // The bar's wrapper takes its height from `flex_1`, so it needs
                // a column with a definite height to grow inside — hand it one
                // rather than dropping it straight into the overlay, or the
                // pane sizes to its content and there is nothing left to
                // scroll.
                v_flex()
                    .size_full()
                    .child(crate::ui::scrollbar::with_vertical_scrollbar(
                        ("editor-md-preview-scrollbar", slot),
                        div()
                            .id(("editor-md-preview", slot))
                            .size_full()
                            .overflow_y_scroll()
                            .track_scroll(&scroll)
                            .px_4()
                            .py_3()
                            .child(
                                gpui_component::text::TextView::markdown(
                                    ("editor-md-preview-body", slot),
                                    markdown,
                                )
                                .style(crate::ui::theme::markdown_style(cx)),
                            ),
                        &scroll,
                    ))
                    .into_any_element()
            }
            Some(f) => {
                let input = f.input.clone();
                let id = f.id();
                let app = cx.entity().downgrade();
                div()
                    .id(("editor-body", slot))
                    .size_full()
                    .child(
                        Input::new(&input)
                            .appearance(false)
                            .font_family(cx.theme().mono_font_family.clone())
                            .text_size(cx.theme().mono_font_size)
                            .size_full(),
                    )
                    .children(self.render_editor_gutter_peek(id, cx))
                    .context_menu(move |menu, _window, cx| {
                        Self::editor_body_menu(menu, &app, id, cx)
                    })
                    .into_any_element()
            }
        }
    }

    /// The editor header alone, for the strip above a docked column.
    pub(crate) fn render_editor_header_only(
        &self,
        chrome: DocumentChrome,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        self.render_editor_header(chrome, window, cx)
            .into_any_element()
    }

    fn render_editor_header(
        &self,
        chrome: DocumentChrome,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        // `TITLE_BAR_LEAD` is the room macOS's traffic lights need. Only a
        // header that starts at the left edge of the window has them to clear,
        // and a docked column never does.
        let lead = if self.left_panel_open(cx) || chrome.is_dock() {
            crate::ui::app::CONTENT_INSET
        } else {
            crate::ui::app::TITLE_BAR_LEAD
        };
        let row = h_flex().id("editor-header");
        let row = if chrome.header_is_title_strip() {
            crate::ui::app::title_bar_drag(row, "editor-header", window, cx)
        } else {
            row
        };
        // The header carries no file names: the editor itself shows what it
        // has open (Helix's status line, the built-in editor's own bar at the
        // foot), and a second, stale copy here would only disagree with it.
        let (tile, glyph) = (
            crate::ui::tab_strip::RAIL_TILE,
            crate::ui::tab_strip::RAIL_TILE_GLYPH,
        );
        let external = self.external_editor_index().is_some();
        let back =
            (chrome == DocumentChrome::Fill).then(|| self.render_back_to_terminal(window, cx));
        row.flex_none()
            // The border is spent on the box, not on the paint: this header
            // shows no rule, but its tiles sit in the band the traffic lights
            // hang over and have to keep the title bar's line.
            .title_strip_row(px(TITLE_BAR_BORDER), cx.theme().transparent)
            .gap(px(2.))
            // Keep the whole leading reserve outside the first control's own
            // padding, for both the return pill and the file controls.
            .pl(px(lead))
            // The glyph, not the tile, lands on `CONTENT_INSET`, the column
            // the file name starts on at the other end of the bar.
            .pr(px(crate::ui::app::CONTENT_INSET - (tile - glyph) / 2.))
            .children(back)
            // A new file belongs to the built-in editor: Helix and Vim own
            // their own buffers and their own way of opening one.
            .when(!external, |row| {
                row.child(
                    div().occlude().flex_none().child(
                        crate::ui::tab_strip::chrome_tile_sized(
                            Button::new("editor-new-file").icon(Icon::new(IconName::Plus)),
                            tile,
                            glyph,
                            false,
                            cx,
                        )
                        .rounded(px(crate::ui::tab_strip::RAIL_TILE_RADIUS))
                        .tooltip(t(L10nKey::EditorNewFile))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.editor_new_file(window, cx);
                        })),
                    ),
                )
            })
            // Whatever is left of the bar stays a place to drag the window by.
            .child(div().flex_1().h_full())
            .child(
                div().occlude().flex_none().ml(px(4.)).child(
                    crate::ui::tab_strip::chrome_tile_sized(
                        Button::new("editor-panel-close").icon(Icon::new(IconName::Close)),
                        tile,
                        glyph,
                        false,
                        cx,
                    )
                    .rounded(px(crate::ui::tab_strip::RAIL_TILE_RADIUS))
                    .tooltip(t(L10nKey::EditorBackToTerminal))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.toggle_code_panel(window, cx);
                    })),
                ),
            )
    }

    fn render_editor_bar(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let bar = self.editor.bar.as_ref()?;
        let label = match bar.kind {
            BarKind::GoToLine => t(L10nKey::EditorGoToLine),
            BarKind::SaveAs { .. } => t(L10nKey::EditorSaveAs),
            BarKind::Rename { .. } => t(L10nKey::LspRenameSymbol),
        };
        Some(
            h_flex()
                .flex_none()
                .w_full()
                .items_center()
                .gap_2()
                .px(px(crate::ui::app::CONTENT_INSET))
                .py_1()
                .border_b_1()
                .border_color(cx.theme().sidebar_border)
                .text_sm()
                .child(
                    div()
                        .flex_none()
                        .text_color(cx.theme().muted_foreground)
                        .child(label),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(Input::new(&bar.input).small()),
                )
                .into_any_element(),
        )
    }

    fn render_code_status_bar(&self, window: &Window, cx: &mut Context<Self>) -> gpui::Div {
        // No path here: the breadcrumbs over the text show where the file is.
        let muted = cx.theme().muted_foreground;
        let active = self.active_buffer();
        let cursor: Option<SharedString> = active.map(|f| {
            let input = f.input.read(cx);
            let pos = input.cursor_position();
            let ln_col = t_fmt(
                L10nKey::EditorLnCol,
                &[
                    ("line", &(pos.line + 1).to_string()),
                    ("column", &(pos.character + 1).to_string()),
                ],
            );
            // Multi-cursor: the position is the primary caret's, the count
            // says there are more.
            match input.selection_count() {
                1 => ln_col.into(),
                n => {
                    let n = n.to_string();
                    format!(
                        "{ln_col} {}",
                        t_fmt(L10nKey::EditorSelections, &[("n", &n)])
                    )
                    .into()
                }
            }
        });
        let wrap: Option<bool> = active.map(|f| f.wrap);
        let is_markdown = active.is_some_and(|f| f.language() == "markdown");
        let preview = active.is_some_and(|f| f.preview);
        let indent: Option<SharedString> = active.map(|f| {
            let key = if f.indent.hard_tabs {
                L10nKey::EditorIndentTabs
            } else {
                L10nKey::EditorIndentSpaces
            };
            t_fmt(key, &[("n", &f.indent.size.to_string())]).into()
        });
        let line_ending: Option<&'static str> = active.map(|f| f.format.line_ending.label());
        let encoding: Option<SharedString> = active.map(|f| f.format.encoding_label().into());
        let language: Option<&'static str> = active.and_then(|f| language_label(f.language()));
        // The header carries no file name any more; this bar is where the
        // file says what it is, the way Helix's status line does.
        let file: Option<(SharedString, String)> = active.map(|f| {
            let tip = match f.untitled {
                Some(_) => f.label().to_string(),
                None => f.path.display().to_string(),
            };
            (f.label(), tip)
        });
        let (lsp_lead, lsp_tail) = match self.render_lsp_status(cx) {
            Some((el, true)) => (Some(el), None),
            Some((el, false)) => (None, Some(el)),
            None => (None, None),
        };

        // Metadata, not a toolbar: caption ink on the plane's own fill, set
        // off by a hairline in the divider tone rather than a control border.
        // The readouts that do something answer the pointer with a soft fill
        // and body ink; the rest are plain text in the same row.
        let fg = cx.theme().foreground;
        let hover = cx.theme().muted;
        let item = |id: &'static str, label: SharedString| {
            div()
                .id(id)
                .flex_none()
                .h(px(20.))
                .px(px(4.))
                .mx(px(-4.))
                .flex()
                .items_center()
                .rounded(px(4.))
                .cursor_pointer()
                .hover(move |s| s.bg(hover).text_color(fg))
                .child(label)
        };
        h_flex()
            .flex_none()
            .w_full()
            .h(px(28.))
            .items_center()
            .gap(px(14.))
            .px(px(crate::ui::app::CONTENT_INSET))
            .border_t(crate::ui::theme::hairline(window))
            .border_color(cx.theme().sidebar_border)
            .text_size(gpui::rems(11.5 / 16.))
            .text_color(muted)
            .whitespace_nowrap()
            .children(file.map(|(name, tip)| {
                div()
                    .id("status-file")
                    .flex_none()
                    .max_w(px(360.))
                    .overflow_hidden()
                    .text_ellipsis()
                    .text_color(fg)
                    .child(name)
                    .tooltip(move |window, cx| {
                        gpui_component::tooltip::Tooltip::new(tip.clone()).build(window, cx)
                    })
            }))
            .children(lsp_lead)
            .child(div().flex_1())
            // The counts open the Problems list.
            .children(lsp_tail.map(|status| {
                div()
                    .id("status-problems")
                    .flex_none()
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_editor_problems(cx)))
                    .child(status)
            }))
            .when(is_markdown, |this| {
                let label = if preview {
                    t(L10nKey::EditorEdit)
                } else {
                    t(L10nKey::EditorPreview)
                };
                this.child(
                    item("status-md-preview", label.into())
                        .on_click(cx.listener(|this, _, _w, cx| this.toggle_document_preview(cx))),
                )
            })
            .when_some(wrap, |this, wrap| {
                let label = if wrap {
                    t(L10nKey::EditorWrapOn)
                } else {
                    t(L10nKey::EditorWrapOff)
                };
                this.child(item("status-wrap", label.into()).on_click(
                    cx.listener(|this, _, window, cx| this.toggle_document_wrap(window, cx)),
                ))
            })
            // Tabular figures, so the position does not jitter sideways as
            // the caret walks from line 9 to line 10. A click asks for a line
            // to go to.
            .when_some(cursor, |this, t| {
                this.child(
                    item("status-cursor", t)
                        .font_features(crate::ui::theme::tabular_figures())
                        .on_click(
                            cx.listener(|this, _, window, cx| this.editor_go_to_line(window, cx)),
                        ),
                )
            })
            .when_some(indent, |this, t| this.child(div().flex_none().child(t)))
            .when_some(encoding, |this, t| this.child(div().flex_none().child(t)))
            .when_some(line_ending, |this, eol| {
                this.child(
                    item("status-eol", eol.into())
                        .on_click(cx.listener(|this, _, _w, cx| this.toggle_line_ending(cx))),
                )
            })
            .when_some(language, |this, l| this.child(div().flex_none().child(l)))
    }

    fn render_editor_empty(&self, cx: &Context<Self>) -> gpui::Div {
        v_flex()
            .track_focus(&self.editor.empty_focus)
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _, window, cx| this.editor.empty_focus.focus(window, cx)),
            )
            .size_full()
            .items_center()
            .justify_center()
            .gap_2()
            .child(
                Icon::new(IconName::File)
                    .large()
                    .text_color(cx.theme().muted_foreground),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(crate::ui::i18n::t(
                        crate::ui::i18n::L10nKey::OpenFileFromTree,
                    )),
            )
    }

    fn render_editor_conflict_banner(
        &self,
        id: BufferId,
        conflict: DiskConflict,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        use crate::ui::dialog::{self, Tone};
        let theme = cx.theme();
        let rungs = cx.global::<crate::ui::presets::Surfaces>().window;
        // The floating notices' grammar, laid flat: a neutral strip over a
        // hairline, and the state carried by one amber dot. A strip tinted
        // amber end to end, with a white outlined button from the component
        // library beside a bare-text one, was three visual languages in one
        // 32px row.
        //
        // The filled answer is always the safe one: Keep mine / Save hold on
        // to the unsaved edits, Reload / Close throw them away, so those are
        // offered, not pressed on the reader.
        let row = h_flex()
            .flex_none()
            .w_full()
            .items_center()
            .gap(px(8.))
            .pl(px(dialog::INSET))
            .pr(px(6.))
            .h(px(dialog::FOOTER_H))
            .border_b_1()
            .border_color(theme.border)
            .text_size(rems(crate::ui::right_panel::TAB_TEXT))
            .child(
                div()
                    .flex_none()
                    .size(px(6.))
                    .rounded_full()
                    .bg(theme.warning),
            );
        let message = |key| div().flex_1().min_w_0().truncate().child(t(key));
        match conflict {
            DiskConflict::Changed(observed) => row
                .child(message(L10nKey::FileChangedOnDisk))
                .child(dialog::button(
                    "editor-conflict-reload",
                    t(L10nKey::Reload),
                    Tone::Secondary,
                    true,
                    rungs,
                    cx,
                    cx.listener(move |this, _, window, cx| {
                        this.editor_reload_from_disk(id, window, cx);
                    }),
                ))
                .child(dialog::button(
                    "editor-conflict-keep",
                    t(L10nKey::KeepMine),
                    Tone::Primary,
                    true,
                    rungs,
                    cx,
                    cx.listener(move |this, _, _w, cx| {
                        if let Some(f) = this.buffer_mut(id) {
                            // The version seen on disk is now the one a
                            // save is allowed to replace.
                            f.disk_mtime = observed;
                            f.conflict = None;
                            cx.notify();
                        }
                    }),
                ))
                .into_any_element(),
            DiskConflict::Deleted => row
                .child(message(L10nKey::EditorFileDeletedOnDisk))
                .child(dialog::button(
                    "editor-deleted-close",
                    t(L10nKey::Close),
                    Tone::Secondary,
                    true,
                    rungs,
                    cx,
                    cx.listener(move |this, _, _w, cx| {
                        this.editor_drop_buffer(id, cx);
                    }),
                ))
                .child(dialog::button(
                    "editor-deleted-save",
                    t(L10nKey::Save),
                    Tone::Primary,
                    true,
                    rungs,
                    cx,
                    cx.listener(move |this, _, window, cx| {
                        this.editor_save_file(id, false, true, window, cx);
                    }),
                ))
                .into_any_element(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    #[test]
    fn vim_opens_literal_paths_and_keeps_unsaved_buffers() {
        let dir = tempfile::tempdir().unwrap();
        let root = test_real_dir(dir.path());
        let first = root.join("first.rs");
        let second = root.join("`touch\tEXECUTED` ' | $ % # * ? [ { < \\ \" 中文.rs");
        std::fs::write(&first, "first\n").unwrap();
        std::fs::write(&second, "second\n").unwrap();
        let result = root.join("result");
        let script = root.join("test.vim");
        std::fs::write(
            &script,
            format!(
                "execute 'edit ' . {}\n\
                 let first = bufnr('%')\n\
                 call setline(1, 'unsaved edit')\n\
                 execute 'hide edit ' . {}\n\
                 call writefile([expand('%:p'), getline(1), getbufline(first, 1)[0], string(getbufvar(first, '&modified'))], '{}')\n\
                 qa!\n",
                vim_command_path(first.to_str().unwrap()),
                vim_command_path(second.to_str().unwrap()),
                result.display(),
            ),
        )
        .unwrap();
        let output = std::process::Command::new("/usr/bin/vim")
            .args(["-Nu", "NONE", "-i", "NONE", "-n", "-es", "-S"])
            .arg(script)
            .current_dir(&root)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        assert_eq!(
            std::fs::read_to_string(result).unwrap(),
            format!("{}\nsecond\nunsaved edit\n1\n", second.display())
        );
        assert!(!root.join("EXECUTED").exists());
    }

    #[test]
    fn external_editors_spawn_as_the_recorded_program_with_literal_arguments() {
        use crate::core::config::FileOpenWith;
        let file = Path::new("/project/a b'$(echo no).rs");
        let helix = external_editor_shell(FileOpenWith::Helix, file, Some(12), Some(3));
        assert_eq!(helix.program, "hx");
        assert_eq!(helix.args, ["/project/a b'$(echo no).rs:12:3"]);
        let vim = external_editor_shell(FileOpenWith::Vim, file, Some(12), None);
        assert_eq!(vim.program, "vim");
        assert_eq!(vim.args, ["+12", "/project/a b'$(echo no).rs"]);
        let restored: crate::daemon::protocol::ShellSpec =
            serde_json::from_str(&serde_json::to_string(&helix).unwrap()).unwrap();
        assert_eq!(restored, helix);
        assert!(is_external_editor_program(&restored.program));
    }

    /// A temp dir as the editor will see paths under it: through the local
    /// host's `canonicalize`, the call `load_file` makes. That resolves
    /// macOS's `/var` → `/private/var` and Windows' 8.3 short names
    /// (`RUNNER~1`), and never answers in Windows' `\\?\` form.
    pub(crate) fn test_real_dir(dir: &Path) -> PathBuf {
        tty7_core::host::local::LocalHost::new()
            .canonicalize(dir)
            .unwrap()
    }

    /// Handing a file the editor cannot read to the desktop is how a click
    /// opens a PNG. It must not be how a click runs a build's output.
    #[cfg(unix)]
    #[test]
    fn a_file_the_desktop_would_run_is_not_handed_to_it() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("tty7-program-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create dir");
        let image = dir.join("shot.png");
        let program = dir.join("built");
        std::fs::write(&image, b"\x89PNG\0\0").expect("write image");
        std::fs::write(&program, b"\x7fELF\0\0").expect("write program");
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755))
            .expect("mark executable");

        assert!(!is_program(&image), "a picture is only ever shown");
        assert!(is_program(&program), "a binary would be launched");
        assert!(
            !is_program(&dir),
            "a directory is not a program, whatever its mode says"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn language_map_covers_common_extensions() {
        for (path, lang) in [
            ("a/b/main.rs", "rust"),
            ("x.tsx", "tsx"),
            ("x.jsx", "javascript"),
            ("x.yml", "yaml"),
            ("Makefile", "make"),
            ("CMakeLists.txt", "cmake"),
            (".zshrc", "bash"),
            ("notes.md", "markdown"),
            ("query.SQL", "sql"),
            ("unknown.xyz", "text"),
            ("no_ext", "text"),
        ] {
            assert_eq!(language_for_path(Path::new(path)), lang, "path {path}");
        }
    }

    fn mt(secs: i64, nanos: u32) -> Option<MTime> {
        Some(MTime { secs, nanos })
    }

    #[test]
    fn external_changes_are_told_apart_from_our_own_saves() {
        let ours = mt(100, 0);
        let seen = |m| Observed::Present(m);

        assert_eq!(
            classify_external_change(false, false, ours, seen(ours)),
            ExternalChange::Ignore
        );
        assert_eq!(
            classify_external_change(false, false, ours, seen(mt(101, 0))),
            ExternalChange::Reload
        );
        assert_eq!(
            classify_external_change(false, true, ours, seen(mt(101, 0))),
            ExternalChange::Conflict
        );
        assert_eq!(
            classify_external_change(false, false, mt(100, 0), seen(mt(100, 1))),
            ExternalChange::Reload
        );
        assert_eq!(
            classify_external_change(true, false, ours, seen(mt(101, 0))),
            ExternalChange::Ignore
        );
        assert_eq!(
            classify_external_change(false, false, None, seen(None)),
            ExternalChange::Reload
        );
    }

    #[test]
    fn a_file_that_vanished_is_reported_deleted_whether_or_not_it_was_edited() {
        assert_eq!(
            classify_external_change(false, false, mt(1, 0), Observed::Missing),
            ExternalChange::Deleted
        );
        assert_eq!(
            classify_external_change(false, true, mt(1, 0), Observed::Missing),
            ExternalChange::Deleted
        );
        // Our own save replaces the file by rename, and a watcher can catch
        // the instant between.
        assert_eq!(
            classify_external_change(true, false, mt(1, 0), Observed::Missing),
            ExternalChange::Ignore
        );
    }

    #[test]
    fn the_fingerprint_does_not_depend_on_how_the_text_is_chunked() {
        let text = "fn main() {\n    println!(\"中文\");\n}\n";
        let whole = Fingerprint::of_str(text);
        let (a, b) = text.split_at(13);
        assert_eq!(
            Fingerprint::of_chunks(text.len(), [a, b].into_iter()),
            whole
        );
        assert_ne!(Fingerprint::of_str("fn main() {}\n"), whole);
    }

    #[test]
    fn only_the_span_that_changed_is_replaced() {
        assert_eq!(differing_span("abc", "abc"), None);
        assert_eq!(
            differing_span("hello world", "hello there world"),
            Some((6, 6, 12))
        );
        assert_eq!(differing_span("abc", "abXc"), Some((2, 2, 3)));
        assert_eq!(differing_span("abc", ""), Some((0, 3, 0)));
        assert_eq!(differing_span("", "new"), Some((0, 0, 3)));
        // "aaa" → "aa": the overlap of prefix and suffix must not double-count.
        assert_eq!(differing_span("aaa", "aa"), Some((2, 3, 2)));
    }

    #[test]
    fn a_changed_span_never_splits_a_character() {
        // 中 and 丰 share their first two bytes in UTF-8.
        let (s, oe, ne) = differing_span("x中y", "x丰y").unwrap();
        let (old, new) = ("x中y", "x丰y");
        assert!(old.is_char_boundary(s) && new.is_char_boundary(s));
        assert_eq!(&old[s..oe], "中");
        assert_eq!(&new[s..ne], "丰");
    }

    #[test]
    fn an_agent_is_told_the_lines_a_selection_covers() {
        let text = gpui_component::input::Rope::from("one\ntwo\nthree\n");
        assert_eq!(selected_lines(&text, 0..0), None);
        assert_eq!(selected_lines(&text, 1..2), Some((1, 1)));
        assert_eq!(selected_lines(&text, 2..9), Some((1, 3)));
        // Whole lines picked by dragging to the start of the next one.
        assert_eq!(selected_lines(&text, 4..14), Some((2, 3)));
        assert_eq!(selected_lines(&text, 0..4), Some((1, 1)));
    }

    #[test]
    fn go_to_line_reads_the_forms_people_type() {
        assert_eq!(parse_line_target("120"), Some((120, 1)));
        assert_eq!(parse_line_target(" 120:4 "), Some((120, 4)));
        assert_eq!(parse_line_target(":7"), Some((7, 1)));
        assert_eq!(parse_line_target("7,3"), Some((7, 3)));
        assert_eq!(parse_line_target("7:"), Some((7, 1)));
        assert_eq!(parse_line_target("0"), None);
        assert_eq!(parse_line_target("abc"), None);
        assert_eq!(parse_line_target(""), None);
    }

    #[test]
    fn showing_a_file_replaces_the_one_the_tab_showed() {
        let ids: Vec<BufferId> = (1..=3u64).map(gpui::EntityId::from).collect();
        let mut code = TabCode::new();
        code.show(ids[0]);
        assert_eq!(code.files, vec![ids[0]]);
        code.show(ids[1]);
        assert_eq!(code.files, vec![ids[1]], "one file per tab");
        assert_eq!(code.active_id(), Some(ids[1]));
        code.show(ids[0]);
        assert_eq!(code.files, vec![ids[0]]);
        assert!(code.forget(ids[0]));
        assert!(code.files.is_empty());
        assert!(!code.forget(ids[0]));
    }

    #[test]
    fn a_file_loaded_in_the_background_only_fills_an_empty_tab() {
        let ids: Vec<BufferId> = (1..=2u64).map(gpui::EntityId::from).collect();
        let mut code = TabCode::new();
        code.append(ids[0]);
        assert_eq!(code.files, vec![ids[0]]);
        code.append(ids[1]);
        assert_eq!(code.files, vec![ids[0]], "no second file");
        code.adopt(&ids[1..]);
        assert_eq!(code.files, vec![ids[0]], "a merge does not displace it");

        let mut empty = TabCode::new();
        empty.adopt(&ids[1..]);
        assert_eq!(empty.files, vec![ids[1]]);
    }
}

#[cfg(test)]
mod close_gpui_tests {
    use gpui::{Entity, TestAppContext, VisualTestContext};

    use super::nav::gpui_tests::{open_rs, source};
    use super::tests::test_real_dir;
    use super::*;
    use crate::core::actions::CloseActiveTab;
    use crate::ui::app::test_window::harness_with_tabs;

    fn panel(app: &Entity<Tty7App>, vcx: &mut VisualTestContext) -> (bool, usize) {
        app.read_with(vcx, |app, _| {
            let code = app.tab_code().expect("the tab is still there");
            (code.visible, code.files.len())
        })
    }

    /// Runs what is pending and paints a frame: the panel notes whether it
    /// holds the focus as it is drawn.
    fn settle(vcx: &mut VisualTestContext) {
        vcx.run_until_parked();
        vcx.update(|window, cx| window.draw(cx));
        vcx.run_until_parked();
    }

    fn terminal_focused(app: &Entity<Tty7App>, vcx: &mut VisualTestContext) -> bool {
        app.update_in(vcx, |app, window, cx| {
            app.tabs[app.active]
                .pane
                .leaves()
                .iter()
                .any(|l| l.contains_focused(window, cx))
        })
    }

    /// ⌘W, ⌘W on an editor with one file closes the file and then the empty
    /// editor. The second one used to close the terminal: closing the last
    /// file left nothing in the panel to hold the focus, so it fell back to
    /// the terminal pane.
    #[gpui::test]
    fn closing_the_last_file_keeps_close_on_the_editor(cx: &mut TestAppContext) {
        let (app, mut vcx, _streams) = harness_with_tabs(cx, 1);
        crate::ui::i18n::set_locale("en");
        open_rs(&app, &mut vcx, "/close/a.rs");
        settle(&mut vcx);
        assert_eq!(panel(&app, &mut vcx), (true, 1));

        vcx.dispatch_action(CloseActiveTab);
        settle(&mut vcx);
        assert_eq!(panel(&app, &mut vcx), (true, 0), "the file closes first");
        assert!(!terminal_focused(&app, &mut vcx));

        vcx.dispatch_action(CloseActiveTab);
        settle(&mut vcx);
        assert_eq!(panel(&app, &mut vcx), (false, 0), "then the empty editor");
        assert!(
            terminal_focused(&app, &mut vcx),
            "and the terminal has it back"
        );

        let tab = app.read_with(&vcx, |app, _| app.tabs[0].tree_id.get());
        vcx.dispatch_action(CloseActiveTab);
        settle(&mut vcx);
        assert!(
            app.read_with(&vcx, |app, _| app
                .tabs
                .iter()
                .all(|t| t.tree_id.get() != tab)),
            "only now is ⌘W the terminal's"
        );
    }

    /// A tab switched to keeps its focus in its terminal, even with an empty
    /// editor showing and the focus in the editor of the tab left behind.
    #[gpui::test]
    fn switching_to_an_empty_editor_leaves_the_focus_alone(cx: &mut TestAppContext) {
        let (app, mut vcx, _streams) = harness_with_tabs(cx, 2);
        crate::ui::i18n::set_locale("en");
        app.update_in(&mut vcx, |app, _, _| {
            app.active = 1;
            app.tab_code_mut_or_init().unwrap().visible = true;
            app.active = 0;
        });
        open_rs(&app, &mut vcx, "/close/a.rs");
        settle(&mut vcx);
        app.update_in(&mut vcx, |app, window, cx| {
            app.active = 1;
            app.focus_active(window, cx);
        });
        settle(&mut vcx);
        assert_eq!(panel(&app, &mut vcx), (true, 0));
        assert!(terminal_focused(&app, &mut vcx));
    }

    /// One file per tab: opening another file replaces the one in front, and
    /// the file it replaced goes with it.
    #[gpui::test]
    fn opening_another_file_replaces_the_file_in_the_editor(cx: &mut TestAppContext) {
        let (app, mut vcx, _streams) = harness_with_tabs(cx, 1);
        crate::ui::i18n::set_locale("en");
        let first = open_rs(&app, &mut vcx, "/single/a.rs");
        let second = open_rs(&app, &mut vcx, "/single/b.rs");
        app.read_with(&vcx, |app, _| {
            let code = app.tab_code().expect("the tab is still there");
            assert_eq!(code.files, vec![second]);
            assert_eq!(app.active_buffer().map(|f| f.id()), Some(second));
            assert!(
                app.editor.buffers.iter().all(|f| f.id() != first),
                "the file that left is closed, not kept behind"
            );
        });
    }

    #[gpui::test]
    fn replacing_a_shared_dirty_file_preserves_its_other_view(cx: &mut TestAppContext) {
        let (app, mut vcx, _streams) = harness_with_tabs(cx, 2);
        let shared = open_rs(&app, &mut vcx, "/single/shared.rs");
        app.update_in(&mut vcx, |app, window, cx| {
            app.active = 1;
            let tab = app.tabs[1].tree_id.get();
            app.editor_show_in_tab(tab, shared, true, window, cx);
            let input = app.buffer(shared).unwrap().input.clone();
            replace_buffer_text(&input, "unsaved edit", window, cx);
            app.editor_note_edit(shared, cx);
            assert_eq!(app.buffer_refs(shared), 2);
            assert!(app.buffer(shared).unwrap().dirty);
        });
        let replacement = open_rs(&app, &mut vcx, "/single/replacement.rs");
        app.read_with(&vcx, |app, cx| {
            assert_eq!(app.active_buffer().unwrap().id(), replacement);
            assert!(!app.editor.unsaved_prompt_open);
            assert_eq!(app.tabs[0].code.as_ref().unwrap().active_id(), Some(shared));
            let kept = app.buffer(shared).unwrap();
            assert!(kept.dirty);
            assert_eq!(kept.input.read(cx).text().to_string(), "unsaved edit");
        });
    }

    #[gpui::test]
    fn restoring_a_file_preserves_the_panels_recorded_visibility(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let path = test_real_dir(dir.path()).join("restored.rs");
        std::fs::write(&path, source()).unwrap();
        for visible in [false, true] {
            let (app, mut vcx, _streams) = harness_with_tabs(cx, 1);
            app.update_in(&mut vcx, |app, window, cx| {
                let tab = app.tabs[0].tree_id.get();
                editor_session::put(
                    cx,
                    tab,
                    TabEditor {
                        files: vec![path.clone()],
                        visible,
                        ..Default::default()
                    },
                );
                app.tabs[0].code = None;
                app.editor.restored.remove(&tab);
                app.focus_active(window, cx);
                app.editor_restore_active(window, cx);
            });
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
            while app.read_with(&vcx, |app, _| app.active_buffer().is_none()) {
                assert!(
                    std::time::Instant::now() < deadline,
                    "restore did not finish"
                );
                std::thread::sleep(std::time::Duration::from_millis(10));
                vcx.run_until_parked();
            }
            assert_eq!(panel(&app, &mut vcx), (visible, 1));
            assert_eq!(terminal_focused(&app, &mut vcx), !visible);
        }
    }

    #[gpui::test]
    fn a_late_restore_does_not_replace_a_file_opened_since(cx: &mut TestAppContext) {
        let (app, mut vcx, _streams) = harness_with_tabs(cx, 1);
        let current = open_rs(&app, &mut vcx, "/single/current.rs");
        app.update_in(&mut vcx, |app, window, cx| {
            let host = app.active_host(cx).unwrap();
            let tab = app.tabs[0].tree_id.get();
            let loaded = Loaded {
                path: "/single/restored.rs".into(),
                text: source(),
                format: TextFormat::default(),
                config: EditorConfig::default(),
                mtime: None,
            };
            app.editor_finish_restore(host, loaded, tab, true, window, cx);
            assert_eq!(app.active_buffer().unwrap().id(), current);
            assert_eq!(app.editor.buffers.len(), 1);
        });
    }

    /// A file loaded behind an unsaved-changes question is not a stray
    /// buffer: cleanup must leave it alone until the answer lands, then take
    /// it when the answer is to back out.
    #[gpui::test]
    fn a_file_waiting_on_a_question_survives_orphan_cleanup(cx: &mut TestAppContext) {
        let (app, mut vcx, _streams) = harness_with_tabs(cx, 1);
        crate::ui::i18n::set_locale("en");
        let opener = open_rs(&app, &mut vcx, "/single/shown.rs");
        let waiting = app.update_in(&mut vcx, |app, window, cx| {
            let host = app.active_host(cx).expect("a host");
            let indent = crate::ui::editor_text::detect_indent("", "rust");
            app.editor_new_buffer(
                host,
                PathBuf::from("/single/waiting.rs"),
                None,
                source(),
                TextFormat::default(),
                EditorConfig::default(),
                indent,
                None,
                window,
                cx,
            )
        });
        app.update_in(&mut vcx, |app, window, cx| {
            app.editor.pending_show = Some(waiting);
            app.editor_adopt_orphans(window, cx);
        });
        app.read_with(&vcx, |app, _| {
            assert_eq!(app.active_buffer().map(|f| f.id()), Some(opener));
            assert!(
                app.editor.buffers.iter().any(|f| f.id() == waiting),
                "the file behind the question is kept"
            );
        });
        // The answer backing out drops the file that was only waiting.
        app.update_in(&mut vcx, |app, window, cx| {
            app.editor.pending_show = None;
            app.editor_adopt_orphans(window, cx);
        });
        app.read_with(&vcx, |app, _| {
            assert!(
                app.editor.buffers.iter().all(|f| f.id() != waiting),
                "nothing is left pointing at it"
            );
        });
    }

    /// In Helix mode a file opened while the editor runs is handed to that
    /// editor: no second process, and no tty7 file tab to go stale.
    #[gpui::test]
    fn a_file_opened_while_helix_runs_is_handed_to_the_running_editor(cx: &mut TestAppContext) {
        let (app, mut vcx, mut streams) = harness_with_tabs(cx, 2);
        crate::ui::i18n::set_locale("en");
        let (owner, session) = app.read_with(&vcx, |app, _| {
            (app.tabs[0].tree_id.get(), app.tabs[1].tree_id.get())
        });
        app.update_in(&mut vcx, |app, _, cx| {
            let mut cfg = cx.global::<crate::core::config::Config>().clone();
            cfg.file_open_with = crate::core::config::FileOpenWith::Helix;
            cx.set_global(cfg);
            app.external_editor_tabs.push(ExternalEditorTab {
                path: "/project/a.rs".into(),
                tab: session,
                terminal: Some(owner),
                program: "hx".into(),
            });
        });
        let before = app.read_with(&vcx, |app, _| app.tabs.len());
        app.update_in(&mut vcx, |app, window, cx| {
            app.open_file_preferred(
                std::path::Path::new("/project/b.rs"),
                Some(12),
                Some(3),
                window,
                cx,
            );
            app.open_file_preferred(
                std::path::Path::new("/project/c.rs"),
                None,
                None,
                window,
                cx,
            );
        });
        vcx.run_until_parked();
        assert_eq!(
            app.read_with(&vcx, |app, _| app.tabs.len()),
            before,
            "the running editor takes the file; no second process"
        );
        assert_eq!(
            app.read_with(&vcx, |app, _| app.external_editor_index()),
            Some(1),
            "the session is the document in front"
        );
        assert_eq!(
            drain(&mut streams[1]),
            b"\x1b[27u:open '/project/b.rs'\r:goto 12\r\x1b[27u:open '/project/c.rs'\r",
        );
        assert!(drain(&mut streams[0]).is_empty());
    }

    /// Everything the pane wrote to the PTY, in order.
    fn drain(daemon: &mut crate::daemon::transport::Stream) -> Vec<u8> {
        use crate::daemon::protocol::ClientMsg;
        daemon
            .set_read_timeout(Some(std::time::Duration::from_millis(150)))
            .unwrap();
        let mut out = Vec::new();
        loop {
            match ClientMsg::read(daemon) {
                Ok(ClientMsg::Input(bytes)) => out.extend_from_slice(&bytes),
                Ok(_) => continue,
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    break;
                }
                Err(e) => panic!("pane link failed: {e}"),
            }
        }
        out
    }

    /// With the focus in the terminal beside an empty editor, ⌘W is still
    /// the terminal's.
    #[gpui::test]
    fn close_from_the_terminal_is_still_the_terminals(cx: &mut TestAppContext) {
        let (app, mut vcx, _streams) = harness_with_tabs(cx, 1);
        crate::ui::i18n::set_locale("en");
        app.update_in(&mut vcx, |app, window, cx| {
            app.tab_code_mut_or_init().unwrap().visible = true;
            app.focus_active(window, cx);
        });
        vcx.run_until_parked();
        assert!(terminal_focused(&app, &mut vcx));
        assert!(!app.update_in(&mut vcx, |app, window, cx| {
            app.editor_close_active_if_focused(window, cx)
        }));
        assert_eq!(panel(&app, &mut vcx), (true, 0));
    }
}
