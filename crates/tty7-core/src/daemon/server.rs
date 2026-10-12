use std::collections::HashMap;
use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};

use crate::daemon::pane::DaemonPane;
use crate::daemon::protocol::{ClientMsg, DaemonMsg, DaemonVersion, LeaseRequest, RemoteKind};
use crate::daemon::ssh::SshConnection;
use crate::daemon::transport::{self, Stream};

struct Registry {
    panes: Mutex<HashMap<u64, Arc<DaemonPane>>>,
    next_id: AtomicU64,
}

impl Registry {
    fn new() -> Self {
        Self {
            panes: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
        }
    }

    fn alloc_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Resume naming panes where the previous image left off.
    ///
    /// Reusing a number would be worse than skipping one: a client that
    /// reconnects after a handoff is still holding the old ids, and an `Attach`
    /// for one of them has to find the pane it means or nothing at all.
    fn claim_ids_past(&self, next: u64) {
        self.next_id.fetch_max(next, Ordering::Relaxed);
    }

    fn seed_ids_past(&self, machine: &crate::core::machine::Machine) {
        let max = machine
            .panes
            .iter()
            .map(|p| p.id)
            .chain(
                machine
                    .workspaces
                    .iter()
                    .flat_map(|w| w.tabs.iter())
                    .flat_map(|t| t.root.pane_ids()),
            )
            .max()
            .unwrap_or(0);
        let next = max.saturating_add(1);
        let before = self.next_id.fetch_max(next, Ordering::Relaxed);
        if next > before {
            log::info!("pane ids start at {next} (the tree names panes up to {max})");
        }
    }

    fn insert(&self, pane: Arc<DaemonPane>) {
        self.panes.lock().unwrap().insert(pane.id, pane);
    }

    fn get(&self, id: u64) -> Option<Arc<DaemonPane>> {
        self.panes.lock().unwrap().get(&id).cloned()
    }

    fn remove(&self, id: u64) -> Option<Arc<DaemonPane>> {
        self.panes.lock().unwrap().remove(&id)
    }

    fn drain_and_kill(&self) {
        let panes: Vec<Arc<DaemonPane>> = {
            let mut guard = self.panes.lock().unwrap();
            guard.drain().map(|(_, p)| p).collect()
        };
        for pane in panes {
            pane.kill();
        }
    }

    fn all(&self) -> Vec<Arc<DaemonPane>> {
        self.panes.lock().unwrap().values().cloned().collect()
    }

    fn list(&self) -> Vec<crate::daemon::protocol::PaneInfo> {
        self.panes
            .lock()
            .unwrap()
            .values()
            .map(|p| p.info())
            .collect()
    }
}

impl crate::host::server::PaneDirectory for Registry {
    fn pane_count(&self) -> u64 {
        self.panes.lock().unwrap().len() as u64
    }

    fn panes(&self) -> Vec<crate::daemon::protocol::PaneInfo> {
        self.list()
    }

    fn pane_procs(&self, pane_id: u64) -> crate::daemon::protocol::PaneProcs {
        self.get(pane_id).map(|p| p.procs()).unwrap_or_default()
    }

    fn hibernate_pane(&self, pane_id: u64) {
        hibernate_pane(self, pane_id);
    }

    fn close_pane(&self, pane_id: u64) {
        kill_pane(self, pane_id);
    }

    fn agent_states(&self) -> Vec<crate::daemon::control::PaneAgentState> {
        let panes: Vec<Arc<DaemonPane>> = self.panes.lock().unwrap().values().cloned().collect();
        let mut states: Vec<_> = panes.iter().filter_map(|p| p.agent_state()).collect();
        states.sort_by_key(|s| s.pane_id);
        states
    }
}

struct MobileBackend {
    registry: Arc<Registry>,
    machine: Arc<crate::core::machine::MachineStore>,
}

impl MobileBackend {
    fn list(&self, auto_grouping: bool) -> serde_json::Value {
        use crate::core::group_key::{auto_key, place};
        use serde_json::json;
        let machine = self.machine.machine();
        let mut repositories = HashMap::new();
        let workspaces: Vec<_> = machine
            .workspaces
            .iter()
            .map(|workspace| {
                let tabs: Vec<_> = workspace
                    .tabs
                    .iter()
                    .filter(|tab| !tab.hibernated)
                    .collect();
                let panes: Vec<Vec<_>> = tabs
                    .iter()
                    .map(|tab| {
                        tab.root
                            .pane_ids()
                            .into_iter()
                            .filter_map(|id| {
                                let pane = self.registry.get(id)?;
                                let info = pane.info();
                                if !info.alive {
                                    return None;
                                }
                                let remote = pane.remote_context();
                                // Remote paths are not paths on this daemon's host.
                                let repo = if remote.is_none() {
                                    info.cwd
                                        .as_ref()
                                        .map(|cwd| {
                                            repositories
                                                .entry(cwd.clone())
                                                .or_insert_with(|| mobile_repo(cwd))
                                                .clone()
                                        })
                                        .unwrap_or_default()
                                } else {
                                    MobileRepo::default()
                                };
                                Some((info, pane.mobile_size(), pane.mobile_agent(), remote, repo))
                            })
                            .collect()
                    })
                    .collect();
                let keys: Vec<_> = tabs
                    .iter()
                    .zip(&panes)
                    .map(|(tab, panes)| {
                        let first = tab.root.pane_ids().first().copied();
                        let resolved = panes
                            .iter()
                            .find(|(info, _, _, _, _)| Some(info.pane_id) == first)
                            .and_then(|(_, _, _, remote, repo)| {
                                let ssh = remote
                                    .as_ref()
                                    .filter(|r| {
                                        matches!(r.kind, RemoteKind::Ssh | RemoteKind::NativeSsh)
                                    })
                                    .map(|r| r.target.as_str());
                                auto_key(ssh, repo.home.clone())
                            });
                        place(
                            tab.group,
                            &workspace.groups,
                            auto_grouping,
                            resolved.unwrap_or_else(|| tab.last_auto.clone()),
                        )
                    })
                    .collect();
                let sections = mobile_sections(&keys, &workspace.groups);
                let terminals: Vec<_> = sections
                    .into_iter()
                    .flat_map(|(group, indices)| {
                        let tabs = &tabs;
                        let panes = &panes;
                        let keys = &keys;
                        indices.into_iter().flat_map(move |index| {
                            let tab = tabs[index];
                            let group = group.clone();
                            let group_id = mobile_group_id(keys[index].as_ref());
                            panes[index]
                                .iter()
                                .map(move |(info, size, agent, _, repo)| {
                                    mobile_terminal(
                                        tab,
                                        info,
                                        *size,
                                        *agent,
                                        group.as_deref().zip(group_id.as_deref()),
                                        repo.branch.as_deref(),
                                    )
                                })
                        })
                    })
                    .collect();
                json!({"id":workspace.id.to_string(),
                "name":workspace.name.as_deref().unwrap_or("Workspace"), "terminals":terminals})
            })
            .collect();
        json!({"workspaces":workspaces})
    }

    fn workspace(
        &self,
        request: &serde_json::Value,
    ) -> std::io::Result<crate::core::session::WorkspaceId> {
        let text = request
            .get("workspace_id")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::InvalidInput, "workspace_id is required")
            })?;
        let id = text.parse().map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid workspace_id")
        })?;
        self.machine.workspace(id)?;
        Ok(id)
    }

    fn pane_id(request: &serde_json::Value) -> std::io::Result<u64> {
        request
            .get("pane_id")
            .and_then(serde_json::Value::as_u64)
            .filter(|id| *id > 0)
            .ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::InvalidInput, "pane_id is required")
            })
    }

    fn member(
        &self,
        workspace: crate::core::session::WorkspaceId,
        pane: u64,
    ) -> std::io::Result<Arc<DaemonPane>> {
        let tree = self.machine.workspace(workspace)?;
        if !tree
            .tabs
            .iter()
            .any(|tab| !tab.hibernated && tab.root.contains(pane))
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "terminal does not belong to this workspace",
            ));
        }
        self.registry
            .get(pane)
            .filter(|pane| pane.alive())
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "terminal is no longer running",
                )
            })
    }

    fn optional_string(
        request: &serde_json::Value,
        key: &str,
        limit: usize,
    ) -> std::io::Result<Option<String>> {
        match request.get(key) {
            None | Some(serde_json::Value::Null) => Ok(None),
            Some(serde_json::Value::String(text))
                if text.len() <= limit && !text.chars().any(char::is_control) =>
            {
                Ok(Some(text.clone()))
            }
            _ => Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("invalid {key}"),
            )),
        }
    }
}

#[derive(Clone, Default)]
struct MobileRepo {
    // Unknown preserves the GUI's last resolved group; known non-repo clears it.
    home: Option<Option<std::path::PathBuf>>,
    branch: Option<String>,
}

fn mobile_repo(cwd: &std::path::Path) -> MobileRepo {
    let host = crate::host::local::LocalHost::shared();
    let Ok(output) = host.git(
        cwd,
        &[
            "rev-parse",
            "--path-format=absolute",
            "--show-toplevel",
            "--git-dir",
            "--git-common-dir",
        ],
    ) else {
        return MobileRepo::default();
    };
    if !output.success() {
        return MobileRepo {
            home: String::from_utf8_lossy(&output.stderr)
                .to_ascii_lowercase()
                .contains("not a git repository")
                .then_some(None),
            branch: None,
        };
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut lines = text.lines();
    let Some(root) = lines.next() else {
        return MobileRepo::default();
    };
    let root = crate::core::git::git_path(&*host, root);
    MobileRepo {
        home: Some(Some(crate::core::git::repo_home(
            &*host,
            &root,
            lines.next(),
            lines.next(),
        ))),
        branch: crate::core::git::branch_name(&*host, cwd),
    }
}

fn mobile_terminal(
    tab: &crate::core::machine::Tab,
    info: &crate::daemon::protocol::PaneInfo,
    size: crate::daemon::protocol::WinSize,
    agent: crate::daemon::pane::MobileAgentState,
    group: Option<(&str, &str)>,
    branch: Option<&str>,
) -> serde_json::Value {
    use serde_json::json;
    json!({"pane_id":info.pane_id, "tab_id":tab.id.to_string(),
        "title":tab.name.as_deref().unwrap_or(info.osc_title.as_deref().unwrap_or(&info.title)),
        "cwd":info.cwd.as_ref().map(|p| p.to_string_lossy()),
        "cols":size.cols, "rows":size.rows, "group":group.map(|(name, _)| name),
        "group_id":group.map(|(_, id)| id), "git_branch":branch,
        "agent_revision":agent.revision,
        "agent":agent.agent.map(|(name, status)| json!({"name":name.display_name(), "status":status}))})
}

fn mobile_group_id(key: Option<&crate::core::group_key::GroupKey>) -> Option<String> {
    use crate::core::group_key::GroupKey;
    key.map(|key| match key {
        GroupKey::Pinned(id) => format!("pinned:{id}"),
        GroupKey::Auto(key) => format!(
            "auto:{}",
            serde_json::to_string(key).expect("auto group keys serialize as strings")
        ),
    })
}

fn mobile_sections(
    keys: &[Option<crate::core::group_key::GroupKey>],
    groups: &crate::core::group_key::WorkspaceGroups,
) -> Vec<(Option<String>, Vec<usize>)> {
    crate::core::group_key::sections(keys, groups, None)
        .into_iter()
        .map(|section| (section.name, section.tabs))
        .collect()
}

impl crate::mobile::Backend for MobileBackend {
    fn request(&self, request: &serde_json::Value) -> std::io::Result<serde_json::Value> {
        use base64::Engine;
        use serde_json::{Value, json};
        let op = request.get("op").and_then(Value::as_str).unwrap_or("");
        match op {
            "list" => Ok(self.list(crate::core::config::Config::load().sidebar_auto_grouping)),
            "create_workspace" => {
                let name = Self::optional_string(request, "name", 256)?;
                let workspace = self.machine.workspace_create(None, name, None)?;
                Ok(json!({"workspace_id":workspace.id.to_string()}))
            }
            "create_terminal" => {
                let workspace = self.workspace(request)?;
                let cwd =
                    Self::optional_string(request, "cwd", 4096)?.map(std::path::PathBuf::from);
                if cwd.as_ref().is_some_and(|cwd| !cwd.is_dir()) {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "cwd must be an existing directory",
                    ));
                }
                let id = self.registry.alloc_id();
                let pane = DaemonPane::spawn(
                    id,
                    cwd,
                    crate::daemon::protocol::WinSize {
                        cols: 80,
                        rows: 24,
                        cell_w: 0,
                        cell_h: 0,
                    },
                    None,
                    None,
                    Some(workspace.to_string()),
                    None,
                    false,
                    reaper(self.registry.clone(), id),
                )
                .map_err(std::io::Error::other)?;
                self.registry.insert(pane.clone());
                if let Err(error) =
                    self.machine
                        .tab_create(workspace, None, pane.mobile_seed(), None, None)
                {
                    kill_pane(&self.registry, id);
                    return Err(error);
                }
                Ok(json!({"pane_id":id}))
            }
            "close_terminal" => {
                let workspace = self.workspace(request)?;
                let id = Self::pane_id(request)?;
                self.member(workspace, id)?;
                let removed = self.machine.pane_close(workspace, id, None)?;
                for pane in removed {
                    kill_pane(&self.registry, pane);
                }
                Ok(Value::Null)
            }
            "input" => {
                let workspace = self.workspace(request)?;
                let pane = self.member(workspace, Self::pane_id(request)?)?;
                let text = request.get("data").and_then(Value::as_str).ok_or_else(|| {
                    std::io::Error::new(std::io::ErrorKind::InvalidInput, "input data is required")
                })?;
                if text.len() > 88 * 1024 {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "input is too large",
                    ));
                }
                let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
                    .decode(text)
                    .map_err(|_| {
                        std::io::Error::new(
                            std::io::ErrorKind::InvalidInput,
                            "invalid input encoding",
                        )
                    })?;
                // The same non-taking path as ClientMsg::SendInput: never attach or resize.
                pane.write_input(&bytes);
                Ok(Value::Null)
            }
            _ => Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "operation is not permitted",
            )),
        }
    }

    fn observe(
        &self,
        workspace: &str,
        pane_id: u64,
    ) -> std::io::Result<crate::mobile::Observation> {
        let workspace = workspace.parse().map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid workspace_id")
        })?;
        let pane = self.member(workspace, pane_id)?;
        let (send, receiver) = mpsc::sync_channel(1);
        let observer_id = pane.mobile_screen_observe(send)?;
        Ok(crate::mobile::Observation {
            pane,
            observer_id,
            receiver,
        })
    }
}

fn start_mobile(registry: &Arc<Registry>, services: &crate::host::server::Services) {
    if let Some(machine) = &services.machine {
        registry.seed_ids_past(&machine.machine());
        if crate::mobile::start(Arc::new(MobileBackend {
            registry: registry.clone(),
            machine: machine.clone(),
        }))
        .is_err()
        {
            log::error!("mobile access could not start; remote access unavailable");
        }
    }
}

/// How often panes are checked for an agent turn that has gone silent past
/// [`crate::daemon::pane::AGENT_STALE_AFTER`]. The threshold is half an hour,
/// so being up to a minute late to notice costs nothing.
const AGENT_STALE_SWEEP_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

/// The pane a Codex or jcode hook's report is about, found from the report
/// itself — where a hook runs says nothing for an agent that runs its hooks in
/// a shared server (see [`crate::daemon::pane::codex_report_target`]). `None`
/// for any other agent's report, which goes to the pane the hook named.
fn codex_report_pane(
    registry: &Registry,
    named: u64,
    body: &str,
) -> Option<(Arc<DaemonPane>, crate::core::cli_agent::AgentEvent)> {
    let event = crate::core::cli_agent::parse_agent_event_body(body.as_bytes())?;
    use crate::core::cli_agent::CLIAgent;
    let agent = event
        .agent
        .filter(|a| matches!(a, CLIAgent::Codex | CLIAgent::Jcode))?;
    let panes = registry.all();
    let candidates: Vec<_> = panes.iter().map(|p| p.codex_candidate(agent)).collect();
    let target = crate::daemon::pane::codex_report_target(
        &candidates,
        named,
        event.session_id.as_deref(),
        event.cwd.as_deref(),
        event.source.as_deref() == Some("clear"),
    )?;
    let pane = panes.into_iter().find(|p| p.id == target)?;
    Some((pane, event))
}

fn spawn_agent_stale_sweep(registry: Arc<Registry>) {
    let spawned = std::thread::Builder::new()
        .name("tty7-agent-stale".into())
        .spawn(move || {
            loop {
                std::thread::sleep(AGENT_STALE_SWEEP_INTERVAL);
                for pane in registry.all() {
                    pane.expire_stale_agent();
                }
            }
        });
    if let Err(e) = spawned {
        log::warn!("could not start the agent status sweep: {e}");
    }
}

const ORPHAN_SWEEP_INTERVAL: std::time::Duration = std::time::Duration::from_secs(600);

fn spawn_orphan_sweep(registry: Arc<Registry>) {
    let spawned = std::thread::Builder::new()
        .name("tty7-orphan-sweep".into())
        .spawn(move || {
            let mut previous: std::collections::HashSet<u64> = std::collections::HashSet::new();
            loop {
                std::thread::sleep(ORPHAN_SWEEP_INTERVAL);
                let Some(store) = crate::core::machine::observed_store() else {
                    continue;
                };
                let machine = store.machine();
                let referenced: std::collections::HashSet<u64> = machine
                    .workspaces
                    .iter()
                    .flat_map(|w| w.tabs.iter())
                    .flat_map(|t| t.root.pane_ids())
                    .collect();
                let orphans: std::collections::HashSet<u64> = registry
                    .list()
                    .into_iter()
                    .filter(|p| p.alive && !referenced.contains(&p.pane_id))
                    .map(|p| p.pane_id)
                    .collect();
                for id in orphans.intersection(&previous) {
                    log::info!(
                        "pane {id} is running but no workspace tree references it \
                         (kept; the sweep only reports — see spawn_orphan_sweep)"
                    );
                }
                previous = orphans;
            }
        });
    if let Err(e) = spawned {
        log::warn!("could not start the orphan-pane sweep: {e}");
    }
}

/// Pane ids that something can still ask to see again: every pane a workspace
/// tree names, plus every pane this daemon is running.
///
/// The registry half matters for a pane spawned since the tree was last
/// written — without it a sweep landing in that window would delete the
/// snapshot of a pane that is very much alive, and the writer would put it
/// back seconds later.
fn restorable_pane_ids(registry: &Registry) -> std::collections::HashSet<u64> {
    let mut ids: std::collections::HashSet<u64> =
        registry.list().into_iter().map(|p| p.pane_id).collect();
    if let Some(store) = crate::core::machine::observed_store() {
        let machine = store.machine();
        ids.extend(
            machine
                .workspaces
                .iter()
                .flat_map(|w| w.tabs.iter())
                .flat_map(|t| t.root.pane_ids()),
        );
        // And the pane list, not only the panes some tab currently stands on.
        // The two disagree while a window is between layouts — a pane whose
        // tab has been taken down and not yet put back is still a pane the
        // tree knows about — and the cost of being wrong is asymmetric: an
        // extra file is swept a tick later, a missing one is somebody's
        // terminal.
        ids.extend(machine.panes.iter().map(|p| p.id));
    }
    ids
}

/// Keep each pane's stored screen roughly current, and collect what no pane
/// can be asked about any more.
///
/// Only panes whose ring has moved are written, so an idle machine does no IO
/// at all, and the busy pane that most needs a fresh copy is the one that gets
/// it.
///
/// The two sweeps ride along here rather than at startup because this is where
/// the question they ask can be answered: by now the registry holds this
/// daemon's panes and the windows have had time to put their trees back. Both
/// take the same set, because a pane whose screen is still worth restoring is
/// exactly a pane whose commands are still worth carrying.
fn spawn_snapshot_keeper(registry: Arc<Registry>) {
    let spawned = std::thread::Builder::new()
        .name("tty7-scrollback".into())
        .spawn(move || {
            let mut marks: HashMap<u64, u64> = HashMap::new();
            loop {
                std::thread::sleep(crate::daemon::scrollback::SNAPSHOT_INTERVAL);
                for pane in registry.all() {
                    if marks.get(&pane.id) == Some(&pane.scrollback_mark()) {
                        continue;
                    }
                    let (segments, title, mark) = pane.scrollback_snapshot();
                    crate::daemon::scrollback::save(pane.id, &segments, title.as_deref());
                    marks.insert(pane.id, mark);
                }
                // Before the sweep, so a closed tab that has aged out takes its
                // screens with it on this pass rather than the next.
                if let Some(store) = crate::core::machine::observed_store() {
                    let now = crate::core::machine::unix_now();
                    for pane in store.expire_closed_tabs(now) {
                        kill_pane(&registry, pane);
                    }
                }
                let restorable = restorable_pane_ids(&registry);
                crate::daemon::scrollback::sweep(&restorable);
                crate::daemon::history::sweep(&restorable);
                marks.retain(|id, _| registry.get(*id).is_some());
            }
        });
    if let Err(e) = spawned {
        log::warn!("could not start the scrollback writer: {e}");
    }
}

/// Write every pane's screen one last time, on the way out of a shutdown this
/// process was told about.
///
/// The periodic writer covers the deaths nobody gets to prepare for; this
/// covers the ones we do, and makes the copy exact rather than up to
/// [`SNAPSHOT_INTERVAL`](crate::daemon::scrollback::SNAPSHOT_INTERVAL) stale.
fn store_scrollback_now(registry: &Registry) {
    for pane in registry.all() {
        let (segments, title, _) = pane.scrollback_snapshot();
        crate::daemon::scrollback::save(pane.id, &segments, title.as_deref());
    }
}

/// Turn a client's restore request into the screen its new pane opens with.
///
/// The snapshot is dropped as it is handed out. It has been folded into a live
/// pane's ring, which is where it will be persisted from now on — under that
/// pane's own id — and a copy left behind could only ever be used to restore
/// the same screen into a second pane.
fn restored_screen(
    request: crate::daemon::protocol::RestoreFrom,
) -> Option<crate::daemon::pane::Restore> {
    let (segments, title) = crate::daemon::scrollback::load(request.pane_id)?;
    // Dropped either way — this is the one request that will ever be made about
    // this pane, so nothing is served by keeping the file past it. What the
    // emptiness check decides is whether a *restore* happened, not whether the
    // file stays: a snapshot holding nothing is not a screen to hand over, but
    // it is still a file nobody will read again.
    crate::daemon::scrollback::forget(request.pane_id);
    if segments.is_empty() {
        return None;
    }
    log::info!(
        "pane {} is gone; its last screen is restored into a fresh pane",
        request.pane_id
    );
    Some(crate::daemon::pane::Restore {
        segments,
        banner: request.banner,
        title,
    })
}

/// Close a pane for good: stop it, drop it from the registry, and drop the copy
/// of its screen.
///
/// A stored screen exists so a pane can survive a death nobody chose. A pane
/// the user closed is not that; keeping its output on disk afterwards would
/// only be a way for it to turn up in some later restore.
fn kill_pane(registry: &Registry, pane_id: u64) {
    if let Some(pane) = registry.remove(pane_id) {
        pane.kill();
    }
    crate::daemon::scrollback::forget(pane_id);
}

/// Stop a pane whose tab is going to sleep, keeping its screen.
///
/// [`kill_pane`] with the other half of the bargain: the screen is written one
/// last time, exactly, and left on disk rather than dropped, so the pane that
/// wakes in this one's place opens showing it — the same restore a daemon that
/// died unannounced gets, asked for the same way. The tree still names this
/// pane, which is what keeps the sweeps off the file until then.
fn hibernate_pane(registry: &Registry, pane_id: u64) {
    let Some(pane) = registry.remove(pane_id) else {
        return;
    };
    let (segments, title, _) = pane.scrollback_snapshot();
    crate::daemon::scrollback::save(pane_id, &segments, title.as_deref());
    pane.kill();
}

fn ssh_connection_for(
    registry: &Registry,
    pane_id: u64,
) -> Result<Arc<crate::daemon::ssh::SshConnection>, String> {
    let pane = registry
        .get(pane_id)
        .ok_or_else(|| format!("no such pane {pane_id}"))?;
    pane.ssh_connection().ok_or_else(|| {
        "pane has no native SSH connection (SFTP needs a native-SSH pane)".to_string()
    })
}

/// `eprintln!` for a process that may have no standard error.
///
/// A daemon started through the Windows clean-parent path (see
/// `daemon::spawn::windows`) has NULL standard handles: naming a logical parent
/// makes handle inheritance follow *that* process, so tty7 cannot hand the
/// child a `NUL` handle the way the ordinary `Stdio::null()` path does. On
/// Windows a failed write to stderr makes `eprintln!` panic, which would kill
/// the daemon before it serves anything. These notes are diagnostics for
/// someone running the daemon in a terminal; dropping them is the right
/// outcome when nobody is there to read them.
macro_rules! startup_note {
    ($($arg:tt)*) => {{
        use std::io::Write as _;
        let _ = writeln!(std::io::stderr(), $($arg)*);
    }};
}

pub fn run_daemon() -> anyhow::Result<()> {
    #[cfg(unix)]
    raise_open_file_limit();
    let result = run_daemon_inner();
    if let Err(e) = &result {
        log::error!("daemon exited with error: {e:#}");
    }
    result
}

/// A GUI-launched macOS process commonly inherits a 256-descriptor soft
/// limit. Each pane and desktop/phone connection owns several descriptors;
/// around fifty tabs can leave accept() enough room for a socket but not
/// try_clone(), so existing screens work while new input connections get EOF.
/// Do this before opening services, including when adopting a live handoff.
#[cfg(unix)]
fn raise_open_file_limit() {
    const TARGET: libc::rlim_t = 4096;
    let mut limits = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limits) } != 0 {
        log::warn!(
            "could not read the open-file limit: {}",
            std::io::Error::last_os_error()
        );
        return;
    }
    let before = limits.rlim_cur;
    let target = TARGET.min(limits.rlim_max);
    if before < target {
        // Preserve the hard limit and never lower an already larger soft
        // limit. A finite target also avoids macOS rejecting RLIM_INFINITY.
        limits.rlim_cur = target;
        if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limits) } != 0 {
            log::warn!(
                "could not raise the open-file limit from {before} to {target}: {}",
                std::io::Error::last_os_error()
            );
            return;
        }
        log::info!(
            "open-file soft limit raised from {before} to {target}; hard limit={}",
            limits.rlim_max
        );
    }
    if limits.rlim_cur < TARGET {
        log::warn!(
            "open-file limit is only {}; many panes may exhaust it (hard limit={})",
            limits.rlim_cur,
            limits.rlim_max
        );
    }
}

fn run_daemon_inner() -> anyhow::Result<()> {
    // First, before a handoff is adopted: the re-exec keeps every inherited
    // descriptor open, and adopting one marks it close-on-exec again.
    #[cfg(target_os = "macos")]
    crate::daemon::responsibility::disclaim_inherited();

    if let Err(e) = crate::core::logfile::capture_stderr() {
        log::warn!("could not capture daemon stderr: {e}");
    }
    log::info!("daemon starting; version={}", env!("CARGO_PKG_VERSION"));

    #[cfg(unix)]
    if let Some(inheritance) = crate::daemon::handoff::requested() {
        return run_adopting(inheritance);
    }

    // Before either endpoint, and before the machine tree is opened. Whoever
    // holds this is the server; everyone else stands down while it lives.
    //
    // The endpoints cannot decide this between themselves. They are bound in
    // sequence, so a second server can take one of them in the window before
    // the first takes the other, and the two then serve half a machine each —
    // panes on one socket, an empty pane registry answering the machine tree on
    // the other. That is how a workspace switch came to rebuild live sessions:
    // no error anywhere, just every pane reading as dead. See `singleton`.
    let _seat = match crate::daemon::singleton::claim() {
        crate::daemon::singleton::Claim::Held(seat) => Some(seat),
        crate::daemon::singleton::Claim::Taken => {
            log::info!("another server already serves this config dir; exiting");
            startup_note!("tty7-server: another server already serves this config dir; exiting");
            return Ok(());
        }
        crate::daemon::singleton::Claim::Unavailable(why) => {
            log::warn!("starting without the single-server lock: {why}");
            startup_note!("tty7-server: starting without the single-server lock ({why})");
            None
        }
    };

    let registry = Arc::new(Registry::new());

    #[cfg(any(unix, windows))]
    {
        let mut services = control_services();
        services.panes = Some(registry.clone());
        start_mobile(&registry, &services);
        match crate::host::server::spawn_control_listener_with(
            crate::host::local::LocalHost::shared(),
            services,
        ) {
            Ok(path) => startup_note!("tty7-server: control socket at {}", path.display()),
            // Someone else already answers there. That is the ordinary local
            // shape: the GUI hosts the control listener and this process was
            // started only to own the panes. Carry on.
            //
            // Ask rather than read the errno. `bind_control_socket` clears an
            // ordinary leftover socket itself, but a path it cannot clear — a
            // directory in the way, a file owned by someone else — comes back
            // `AddrInUse` in the same words a live server does.
            Err(e) if crate::host::server::control_endpoint_answers() => {
                startup_note!(
                    "tty7-server: control listener unavailable ({e}); \
                     another server is answering there, so serving panes only"
                );
            }
            // Nothing is answering, and this process cannot answer either. Do
            // not stay alive: a running daemon holds the single-server lock, so
            // every later `--daemon` stands down at once and every client probe
            // of the control socket fails, forever. That pair is exactly the
            // "started but nothing was answering after 15s" a remote install
            // reports — with the reason, right here, thrown away. Exiting
            // releases the lock and hands the reason to whoever launched us.
            Err(e) => {
                startup_note!("tty7-server: control listener unavailable: {e}");
                return Err(e.into());
            }
        }
    }
    #[cfg(not(any(unix, windows)))]
    log::info!("no control listener on this platform; serving panes only");

    run_with(registry, _seat.is_some())
}

/// Come up as the far side of a handoff: the panes are already running, and
/// this image only has to recognise them.
///
/// The seat is adopted rather than claimed — the lock is held by this process,
/// which is the process that held it before the exec. Claiming would ask the
/// kernel for a lock our own descriptor already has, be refused, and take the
/// "another server is already serving" exit, which would leave the machine with
/// no daemon and a set of orphaned shells nobody can reach.
#[cfg(unix)]
fn run_adopting(inheritance: crate::daemon::handoff::Inheritance) -> anyhow::Result<()> {
    let _seat = inheritance
        .seat_fd
        .map(|fd| unsafe { crate::daemon::singleton::adopt(fd) });
    if _seat.is_none() {
        startup_note!("tty7-server: handed over without a seat descriptor; serving unprotected");
    }

    let registry = Arc::new(Registry::new());

    match crate::daemon::handoff::adopt(inheritance.blob_fd) {
        Some(adopted) => {
            registry.claim_ids_past(adopted.next_pane_id);
            let mut kept = 0usize;
            for carried in adopted.panes {
                let id = carried.id;
                let on_dead = reaper(registry.clone(), id);
                match crate::daemon::pane::DaemonPane::adopt(carried, on_dead) {
                    Ok(pane) => {
                        registry.insert(pane);
                        kept += 1;
                    }
                    // The shell is alive and this image cannot speak to its pty.
                    // Saying so is all that can be done: the descriptor closes
                    // when this process eventually exits, and the shell gets the
                    // hangup it would have got from an ordinary restart.
                    Err(e) => log::error!("pane {id} could not be adopted: {e}"),
                }
            }
            startup_note!("tty7-server: adopted {kept} pane(s) from the previous build");
        }
        // The blob is a file this process wrote and unlinked moments ago, so
        // this is a bug rather than an accident. The shells are the cost: their
        // ptys are still open here, held by descriptors nothing can now name,
        // which leaves them running with no reader — #42's stranded sessions,
        // arrived at from the other direction. They are released when this
        // daemon exits, so a restart clears it; there is nothing safe to close
        // in the meantime, because which descriptors they were is exactly what
        // was lost.
        None => startup_note!(
            "tty7-server: the handoff blob was unreadable; the previous build's panes are lost"
        ),
    }

    {
        let mut services = control_services();
        services.panes = Some(registry.clone());
        start_mobile(&registry, &services);
        match crate::host::server::spawn_control_listener_with(
            crate::host::local::LocalHost::shared(),
            services,
        ) {
            Ok(path) => startup_note!("tty7-server: control socket at {}", path.display()),
            Err(e) => startup_note!("tty7-server: control listener unavailable: {e}"),
        }
    }

    run_with(registry, _seat.is_some())
}

/// Become `exe` in place, keeping every pane that can survive the crossing.
///
/// Returns the reason it did not happen; on success there is no return, because
/// by then this program has been replaced by the one it was asked to become.
#[cfg(unix)]
fn hand_over(registry: &Registry, exe: &std::path::Path) -> anyhow::Error {
    // Before anything is given up: the native-SSH panes below are hung up on
    // the promise that this process is about to stop existing, and an exec
    // that was never going to work must not collect on it. `execve` can still
    // fail after this — a wrong architecture, a permission the metadata does
    // not show — but the everyday failures, a path that is not there or not a
    // program, are caught while everything is still intact.
    match std::fs::metadata(exe) {
        Err(e) => return anyhow::anyhow!("cannot become {}: {e}", exe.display()),
        Ok(meta) => {
            use std::os::unix::fs::PermissionsExt as _;
            if !meta.is_file() || meta.permissions().mode() & 0o111 == 0 {
                return anyhow::anyhow!("cannot become {}: not an executable file", exe.display());
            }
        }
    }

    let panes = registry.all();
    let mut prepared = Vec::new();
    let mut uncarried = Vec::new();
    for pane in &panes {
        match pane.prepare_handoff() {
            Ok(Some(guard)) => prepared.push(guard),
            Ok(None) => uncarried.push(pane.id),
            Err(error) => {
                return anyhow::anyhow!("cannot checkpoint pane {}: {error}", pane.id);
            }
        }
    }
    // Retain the guards through exec: no reader may consume bytes or resize
    // the mirrored state after its checkpoint. A failed exec drops the guards
    // and resumes every pane without discarding pending kernel PTY output.
    let carried = prepared.iter_mut().map(|guard| guard.take_carried()).collect();
    for id in uncarried {
        // Native SSH cipher state cannot cross exec; preserve its existing
        // close-on-handoff behavior only after all PTYs were checkpointed.
        log::info!("pane {id} cannot cross a handoff; closing it");
        kill_pane(registry, id);
    }

    // The tree is what the window rebuilds itself from when it reconnects, and
    // the reconnect happens milliseconds from now.
    if let Some(store) = crate::core::machine::observed_store() {
        store.flush();
    }

    // The new image cannot reap a child it did not start; it starts its own.
    crate::daemon::mobile::stop_for_handoff();

    let failed = crate::daemon::handoff::take_over(
        exe,
        carried,
        registry.alloc_id(),
        crate::daemon::singleton::held_fd(),
    );
    // Still this program: the exec did not happen, so this daemon goes on
    // serving, and so does its gateway.
    crate::daemon::mobile::handoff_failed();
    failed
}

#[cfg(not(unix))]
fn hand_over(_registry: &Registry, _exe: &std::path::Path) -> anyhow::Error {
    anyhow::anyhow!(
        "this platform has no way to replace a running program while keeping its \
         open consoles, so the daemon has to be stopped and started"
    )
}

/// What a pane does to the registry when its shell dies.
fn reaper(registry: Arc<Registry>, id: u64) -> impl FnOnce() + Send + 'static {
    move || {
        std::thread::Builder::new()
            .name("tty7-daemon-pane-reap".to_string())
            .spawn(move || {
                registry.remove(id);
            })
            .ok();
    }
}

pub fn control_services() -> crate::host::server::Services {
    use crate::core::machine::MachineStore;
    crate::core::machine::adopt_legacy_data_dir();
    match MachineStore::shared() {
        Ok(machine) => {
            startup_note!("machine tree at {}", machine.path().display());
            crate::core::machine::publish_observations(&machine);
            crate::host::server::Services::with_machine(machine)
        }
        Err(e) => {
            startup_note!("no machine tree ({e}); serving files and panes only");
            crate::host::server::Services::none()
        }
    }
}

/// Say which pseudoconsole this daemon's panes will run on.
///
/// `portable-pty` loads a sideloaded `conpty.dll` if one sits beside the
/// running executable and silently uses `kernel32`'s otherwise. The difference
/// is invisible until someone asks why a pane swallowed an OSC 11 query (#345),
/// so the choice belongs in the log rather than in a bug report.
#[cfg(windows)]
fn report_conpty_host() {
    let bundled = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("conpty.dll")));
    match bundled {
        Some(path) if path.is_file() => {
            log::info!("panes use the bundled ConPTY at {}", path.display())
        }
        _ => log::warn!(
            "no conpty.dll beside this executable; panes fall back to the in-box conhost, \
             which does not answer terminal color queries"
        ),
    }
}

/// `alone` says this process holds the single-server seat. It decides how the
/// endpoint left by whoever was here before may be dealt with — see
/// [`transport::clear_endpoint_before_bind`].
fn run_with(registry: Arc<Registry>, alone: bool) -> anyhow::Result<()> {
    crate::daemon::control::server_started();

    transport::clear_endpoint_before_bind(alone)?;

    let listener = transport::bind()?;
    log::info!("daemon listening on {}", transport::endpoint_display());

    #[cfg(windows)]
    report_conpty_host();

    crate::daemon::pidfile::write_current();

    // Phone access, when it is switched on: see `daemon::mobile`.
    crate::daemon::mobile::supervise();

    #[cfg(unix)]
    serve_sigterm(registry.clone());

    if let Some(store) = crate::core::machine::observed_store() {
        registry.seed_ids_past(&store.machine());
        let probe = registry.clone();
        store.set_liveness_probe(Arc::new(move |id| {
            probe.get(id).is_some_and(|pane| pane.info().alive)
        }));
    }

    spawn_orphan_sweep(registry.clone());
    spawn_agent_stale_sweep(registry.clone());
    // No sweeping here, deliberately — of either kind. Startup is the one
    // moment this process knows least: it owns no panes yet, and the windows
    // that know which of a dead daemon's files are still wanted cannot say so
    // until the endpoint below is listening. Answering "is anyone going to ask
    // for this?" here answers it when nobody can — and the answer deletes. A
    // tree that failed to parse makes it worse, because `read_machine`
    // quarantines it and hands back an empty `Machine`, so one bad file would
    // take every pane's screen and every pane's history with it.
    //
    // History was swept here until it was noticed that a restore carries the
    // dead pane's commands to its successor (`history::carry`, at the top of
    // the `Spawn` handler): sweeping before the window can ask deletes the very
    // file the request is about. Same shape as the scrollback bug, one file
    // over.
    //
    // Both sweeps run on the writer's tick instead, with the registry filled in
    // and the tree caught up. That is soon enough: nothing here is serving a
    // request in the meantime, and a daemon killed outright left its files for
    // exactly this pass to collect.
    spawn_snapshot_keeper(registry.clone());

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                transport::tune(&stream);
                let registry = registry.clone();
                std::thread::Builder::new()
                    .name("tty7-daemon-conn".to_string())
                    .spawn(move || {
                        crate::core::threads::promote_to_user_interactive();
                        if let Err(e) = handle_conn(stream, registry) {
                            log::debug!("connection ended: {e}");
                        }
                    })
                    .ok();
            }
            Err(e) => log::warn!("accept failed: {e}"),
        }
    }

    Ok(())
}

#[cfg(unix)]
fn serve_sigterm(registry: Arc<Registry>) {
    let set = unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGTERM);
        if libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut()) != 0 {
            log::warn!("could not block SIGTERM; keeping default termination");
            return;
        }
        set
    };
    std::thread::Builder::new()
        .name("tty7-daemon-sigterm".to_string())
        .spawn(move || {
            let mut sig: libc::c_int = 0;
            if unsafe { libc::sigwait(&set, &mut sig) } == 0 {
                log::info!("daemon shutting down on SIGTERM");
                // Ahead of the kill: `drain_and_kill` hangs up every pty, and a
                // pane whose shell has already been reaped has nothing left to
                // photograph.
                store_scrollback_now(&registry);
                registry.drain_and_kill();
                on_shutdown();
                exit_now();
            }
        })
        .ok();
}

fn on_shutdown() {
    if let Some(store) = crate::core::machine::observed_store() {
        store.flush();
    }
    transport::remove_stale_endpoint();
    #[cfg(windows)]
    crate::host::server::remove_control_endpoint();
    // The pidfile stays. Once the endpoint above is unlinked it is the only
    // name anything has for this process, and the process is not gone yet —
    // a stalled exit after this point used to leave a daemon holding the
    // singleton lock with no pidfile, which `spawn::stop` and
    // `ensure_running` could then never find or reap: every later launch
    // stood down against a server serving nobody (#653). A pidfile that
    // outlives a clean exit is already handled — `recorded_daemon_is_dead`
    // reads a dead pid as stale, and `reap_recorded_daemon` deletes the file
    // once the process is confirmed gone.
}

/// Ends the daemon immediately, skipping libc `exit`'s atexit handlers and
/// static destructors. Those run while this process's other threads — the
/// accept loop, the control listeners — are still live, and a finalizer that
/// blocks on anything one of them holds never returns: the daemon then
/// lingers with its endpoint already unlinked but the singleton lock still
/// held (#653). Everything that must reach disk is flushed explicitly in
/// `on_shutdown`, so nothing is owed to the handlers being skipped.
fn exit_now() -> ! {
    log::info!("daemon exiting; code=0");
    log::logger().flush();
    #[cfg(unix)]
    unsafe {
        libc::_exit(0)
    }
    #[cfg(not(unix))]
    std::process::exit(0)
}

fn handle_conn(stream: Stream, registry: Arc<Registry>) -> anyhow::Result<()> {
    let mut read_stream = stream;

    transport::authenticate(&mut read_stream)?;

    let write_stream = read_stream.try_clone()?;

    let (first_kind, first_payload) = crate::daemon::protocol::read_frame(&mut read_stream)?;
    if first_kind == crate::daemon::router::ROUTE_KIND {
        drop(write_stream);
        let header = crate::daemon::router::RouteHeader::decode(&first_payload)?;
        return Ok(crate::daemon::router::RemoteRouter::route(
            read_stream,
            &header,
        )?);
    }

    let first = ClientMsg::from_frame(first_kind, first_payload)?;
    match first {
        ClientMsg::Spawn {
            cwd,
            size,
            shell,
            owner,
            workspace,
            restore,
            allow_remote_clipboard_write,
        } => {
            let id = registry.alloc_id();
            if let Some(dead) = restore.as_ref().map(|r| r.pane_id) {
                // Before the spawn, because the spawn is what hands the shell
                // the name of its history file — and this is what puts the
                // predecessor's commands behind that name.
                crate::daemon::history::carry(dead, id);
            }
            let predecessor = restore.as_ref().map(|r| r.pane_id);
            let restore = restore.and_then(restored_screen);
            log::info!(
                "pane {id} spawn requested; restore_from={predecessor:?} screen_restored={}",
                restore.is_some()
            );
            let on_dead = {
                let registry = registry.clone();
                move || {
                    std::thread::Builder::new()
                        .name("tty7-daemon-pane-reap".to_string())
                        .spawn(move || {
                            log::info!("pane {id} ended; removing from registry");
                            registry.remove(id);
                        })
                        .ok();
                }
            };
            let pane = match DaemonPane::spawn(
                id,
                cwd,
                size,
                shell,
                owner,
                workspace,
                restore,
                allow_remote_clipboard_write,
                on_dead,
            ) {
                Ok(p) => p,
                Err(e) => {
                    let mut w = write_stream;
                    log::error!("pane {id} spawn failed: {e}");
                    // The daemon's own error is already a sentence; a second
                    // "spawn failed:" in front of it only pads the one the
                    // window ends up showing.
                    let _ = DaemonMsg::Error(format!("{e}")).encode(&mut w);
                    return Err(e);
                }
            };
            log::info!("pane {id} spawned");
            registry.insert(pane.clone());

            {
                let mut w = &write_stream;
                DaemonMsg::Spawned { pane_id: id }.encode(&mut w)?;
            }
            stream_pane(
                pane,
                id,
                read_stream,
                write_stream,
                registry,
                allow_remote_clipboard_write,
            )
        }

        ClientMsg::SpawnNativeSsh { cwd, size, spec } => {
            let allow_remote_clipboard_write = spec.remote_clipboard_write;
            // A far-host path that only travels as a `PathBuf`: taken as the
            // text it was sent as, never resolved against this machine. One
            // that is not UTF-8 could only reach the far shell mangled, so it
            // is dropped and the shell starts where it would have anyway.
            let remote_start_dir = cwd.and_then(|p| p.into_os_string().into_string().ok());
            let id = registry.alloc_id();
            let on_dead = {
                let registry = registry.clone();
                move || {
                    std::thread::Builder::new()
                        .name("tty7-daemon-pane-reap".to_string())
                        .spawn(move || {
                            log::info!("pane {id} ended; removing from registry");
                            registry.remove(id);
                        })
                        .ok();
                }
            };
            let pane = match DaemonPane::spawn_native_ssh(id, size, spec, remote_start_dir, on_dead)
            {
                Ok(p) => p,
                Err(e) => {
                    let mut w = write_stream;
                    log::error!("pane {id} native SSH spawn failed: {e}");
                    let _ =
                        DaemonMsg::Error(format!("native ssh spawn failed: {e}")).encode(&mut w);
                    return Err(e);
                }
            };
            log::info!("pane {id} spawned");
            registry.insert(pane.clone());
            {
                let mut w = &write_stream;
                DaemonMsg::Spawned { pane_id: id }.encode(&mut w)?;
            }
            stream_pane(
                pane,
                id,
                read_stream,
                write_stream,
                registry,
                allow_remote_clipboard_write,
            )
        }

        ClientMsg::Attach {
            pane_id,
            size: _,
            allow_remote_clipboard_write,
        } => match registry.get(pane_id) {
            Some(pane) => stream_pane_with_attach(
                pane,
                pane_id,
                read_stream,
                write_stream,
                registry,
                allow_remote_clipboard_write,
            ),
            None => {
                let mut w = write_stream;
                DaemonMsg::Error(format!("no such pane {pane_id}")).encode(&mut w)?;
                Ok(())
            }
        },

        // The size is accepted but deliberately not applied: an observer is
        // read-only, and resizing the pty for it would reflow the grid under
        // the controller. The replay tells the observer the pane's real size;
        // the field stays on the wire so a future observer-side viewport does
        // not need another message kind.
        ClientMsg::Observe { pane_id, size: _ } => match registry.get(pane_id) {
            Some(pane) => stream_observer(pane, read_stream, write_stream),
            None => {
                let mut w = write_stream;
                DaemonMsg::Error(format!("no such pane {pane_id}")).encode(&mut w)?;
                Ok(())
            }
        },

        ClientMsg::List => {
            let mut w = write_stream;
            DaemonMsg::PaneList(registry.list()).encode(&mut w)?;
            Ok(())
        }

        ClientMsg::Version => {
            let mut w = write_stream;
            DaemonMsg::Version(DaemonVersion::current()).encode(&mut w)?;
            Ok(())
        }

        ClientMsg::Shutdown => {
            log::info!("daemon shutting down on client request");
            // This is the path a restart takes, so it is the one that decides
            // whether panes come back showing anything. Ahead of the kill:
            // `drain_and_kill` hangs up every pty, and a pane whose shell has
            // been reaped has nothing left to photograph.
            store_scrollback_now(&registry);
            registry.drain_and_kill();
            // The ConPTY hosts (OpenConsole.exe) are this process's children,
            // not the shells', so the per-pane kill never reaches them — and
            // exiting right away would leave them holding the installed
            // OpenConsole.exe image open while an updater tries to replace it.
            // Reap everything still below us and wait for the images to be
            // released before the endpoint disappears, because the endpoint
            // going away is what tells `spawn::stop` the shutdown is complete.
            #[cfg(windows)]
            crate::daemon::winproc::reap_descendants_of(
                std::process::id(),
                std::time::Duration::from_secs(3),
            );
            on_shutdown();
            exit_now();
        }

        ClientMsg::Kill { pane_id } => {
            kill_pane(&registry, pane_id);
            Ok(())
        }

        ClientMsg::Handoff { exe } => {
            let mut w = write_stream;
            // Only ever returns having failed: a handoff that works replaces
            // this program mid-call, and there is nobody left to write a reply.
            let failure = hand_over(&registry, &exe);
            log::error!("handoff to {} did not happen: {failure}", exe.display());
            DaemonMsg::Error(failure.to_string()).encode(&mut w)?;
            Ok(())
        }

        ClientMsg::EnsureLoopbackForward(req) => {
            let mut w = write_stream;
            let Some(pane) = registry.get(req.pane_id) else {
                DaemonMsg::Error(format!("no such pane {}", req.pane_id)).encode(&mut w)?;
                return Ok(());
            };
            let Some(remote) = pane.remote_context() else {
                DaemonMsg::Error("pane has no ssh remote context".to_string()).encode(&mut w)?;
                return Ok(());
            };
            let result = if remote.kind == RemoteKind::NativeSsh {
                match pane.ssh_connection() {
                    Some(conn) => crate::daemon::ssh::SshManager::global()
                        .ensure_loopback_forward(
                            req.pane_id,
                            conn,
                            &remote.target,
                            &req.remote_host,
                            req.remote_port,
                        )
                        .map_err(|e| e.to_string()),
                    None => Err("native ssh connection is not ready".to_string()),
                }
            } else {
                Err("pane is not a native ssh session".to_string())
            };
            match result {
                Ok(forward) => DaemonMsg::LoopbackForward(forward).encode(&mut w)?,
                Err(e) => DaemonMsg::Error(format!("forward failed: {e}")).encode(&mut w)?,
            }
            Ok(())
        }

        ClientMsg::ListKnownHosts => {
            let mut w = write_stream;
            let list = crate::daemon::ssh::known_hosts::list();
            DaemonMsg::KnownHostsList(list).encode(&mut w)?;
            Ok(())
        }

        ClientMsg::TestSsh { spec } => {
            let mut w = write_stream;
            let report = crate::daemon::ssh::SshManager::global().test_connection(&spec);
            DaemonMsg::SshTestResult(report).encode(&mut w)?;
            Ok(())
        }

        ClientMsg::DeleteKnownHost(id) => {
            let mut w = write_stream;
            let _ = crate::daemon::ssh::known_hosts::delete(&id);
            let list = crate::daemon::ssh::known_hosts::list();
            DaemonMsg::KnownHostsList(list).encode(&mut w)?;
            Ok(())
        }

        ClientMsg::SftpList { pane_id, path } => {
            let mut w = write_stream;
            match ssh_connection_for(&registry, pane_id) {
                Ok(conn) => {
                    match crate::daemon::ssh::sftp::SftpManager::global().list(&conn, &path) {
                        Ok(entries) => DaemonMsg::SftpEntries(entries).encode(&mut w)?,
                        Err(e) => DaemonMsg::Error(e).encode(&mut w)?,
                    }
                }
                Err(e) => DaemonMsg::Error(e).encode(&mut w)?,
            }
            Ok(())
        }

        ClientMsg::AddForward { pane_id, rule } => {
            let mut w = write_stream;
            match forward_pane_connection(&registry, pane_id) {
                Ok(conn) => {
                    let list =
                        crate::daemon::ssh::SshManager::global().add_forward(pane_id, conn, &rule);
                    DaemonMsg::ForwardList(list).encode(&mut w)?;
                }
                Err(e) => DaemonMsg::Error(e).encode(&mut w)?,
            }
            Ok(())
        }

        ClientMsg::SftpOp { pane_id, op } => {
            let mut w = write_stream;
            match ssh_connection_for(&registry, pane_id) {
                Ok(conn) => {
                    let result = crate::daemon::ssh::sftp::SftpManager::global().op(&conn, &op);
                    DaemonMsg::SftpOpResult(result).encode(&mut w)?;
                }
                Err(e) => DaemonMsg::Error(e).encode(&mut w)?,
            }
            Ok(())
        }

        ClientMsg::SftpTransferStart(spec) => {
            let mut w = write_stream;
            match ssh_connection_for(&registry, spec.pane_id) {
                Ok(conn) => {
                    match crate::daemon::ssh::sftp::SftpManager::global()
                        .start_transfer(&conn, spec)
                    {
                        Ok(job_id) => DaemonMsg::SftpTransferStarted { job_id }.encode(&mut w)?,
                        Err(e) => DaemonMsg::Error(e).encode(&mut w)?,
                    }
                }
                Err(e) => DaemonMsg::Error(e).encode(&mut w)?,
            }
            Ok(())
        }

        ClientMsg::SftpTransferCancel { job_id } => {
            let mut w = write_stream;
            let jobs = crate::daemon::ssh::sftp::SftpManager::global().cancel(job_id);
            DaemonMsg::SftpTransferProgress(jobs).encode(&mut w)?;
            Ok(())
        }

        ClientMsg::SftpTransferList { pane_id } => {
            let mut w = write_stream;
            let jobs = crate::daemon::ssh::sftp::SftpManager::global().list_jobs(pane_id);
            DaemonMsg::SftpTransferProgress(jobs).encode(&mut w)?;
            Ok(())
        }

        ClientMsg::RemoveForward {
            pane_id,
            forward_id,
        } => {
            let mut w = write_stream;
            let list = crate::daemon::ssh::SshManager::global().remove_forward(pane_id, forward_id);
            DaemonMsg::ForwardList(list).encode(&mut w)?;
            Ok(())
        }

        ClientMsg::SetForwardEnabled {
            pane_id,
            forward_id,
            enabled,
        } => {
            let mut w = write_stream;
            // Only switching on needs the connection; switching off has to
            // work on a pane whose connection is already gone.
            let conn = match forward_pane_connection(&registry, pane_id) {
                Ok(conn) => Some(conn),
                Err(e) if enabled => {
                    DaemonMsg::Error(e).encode(&mut w)?;
                    return Ok(());
                }
                Err(_) => None,
            };
            match crate::daemon::ssh::SshManager::global()
                .set_forward_enabled(pane_id, conn, forward_id, enabled)
            {
                Ok(list) => DaemonMsg::ForwardList(list).encode(&mut w)?,
                Err(e) => DaemonMsg::Error(e).encode(&mut w)?,
            }
            Ok(())
        }

        ClientMsg::QueryProcs { pane_id } => {
            let mut w = write_stream;
            let procs = registry.get(pane_id).map(|p| p.procs()).unwrap_or_default();
            DaemonMsg::Procs(procs).encode(&mut w)?;
            Ok(())
        }

        ClientMsg::SendInput { pane_id, bytes } => {
            let mut w = write_stream;
            match registry.get(pane_id) {
                Some(pane) if pane.alive() => {
                    pane.write_input(&bytes);
                    DaemonMsg::InputAck { pane_id }.encode(&mut w)?;
                }
                Some(_) => {
                    DaemonMsg::Error(format!("pane {pane_id} is not running")).encode(&mut w)?
                }
                None => DaemonMsg::Error(format!("no such pane {pane_id}")).encode(&mut w)?,
            }
            Ok(())
        }

        ClientMsg::AgentEvent {
            pane_id,
            pid,
            event,
        } => {
            let mut w = write_stream;
            let applied = match codex_report_pane(&registry, pane_id, &event) {
                Some((pane, parsed)) => pane.apply_reported_agent_event(parsed),
                None => match registry.get(pane_id) {
                    Some(pane) => pane.report_agent_event(pid, &event),
                    None => Err(format!("no such pane {pane_id}")),
                },
            };
            match applied {
                Ok(()) => DaemonMsg::InputAck { pane_id }.encode(&mut w)?,
                Err(message) => DaemonMsg::Error(message).encode(&mut w)?,
            }
            Ok(())
        }

        ClientMsg::ListForwards { pane_id } => {
            let mut w = write_stream;
            let list = crate::daemon::ssh::SshManager::global().list_forwards(pane_id);
            DaemonMsg::ForwardList(list).encode(&mut w)?;
            Ok(())
        }

        ClientMsg::OnWorkspace(req) => {
            let mut w = write_stream;
            crate::daemon::ssh::workspace::handle(&req).encode(&mut w)?;
            Ok(())
        }

        other => {
            log::debug!("unexpected opening message: {other:?}");
            Ok(())
        }
    }
}

fn forward_pane_connection(
    registry: &Registry,
    pane_id: u64,
) -> Result<Arc<SshConnection>, String> {
    let pane = registry
        .get(pane_id)
        .ok_or_else(|| format!("no such pane {pane_id}"))?;
    pane.ssh_connection()
        .ok_or_else(|| "pane is not a ready native-ssh session".to_string())
}

fn stream_pane_with_attach(
    pane: Arc<DaemonPane>,
    id: u64,
    read_stream: Stream,
    write_stream: Stream,
    registry: Arc<Registry>,
    allow_remote_clipboard_write: bool,
) -> anyhow::Result<()> {
    let (tx, rx) = mpsc::channel::<DaemonMsg>();
    let epoch = pane.attach_with_permissions(tx, allow_remote_clipboard_write);
    run_stream(pane, id, epoch, rx, read_stream, write_stream, registry)
}

fn stream_pane(
    pane: Arc<DaemonPane>,
    id: u64,
    read_stream: Stream,
    write_stream: Stream,
    registry: Arc<Registry>,
    allow_remote_clipboard_write: bool,
) -> anyhow::Result<()> {
    let (tx, rx) = mpsc::channel::<DaemonMsg>();
    let epoch = pane.attach_with_permissions(tx, allow_remote_clipboard_write);
    run_stream(pane, id, epoch, rx, read_stream, write_stream, registry)
}

fn stream_observer(
    pane: Arc<DaemonPane>,
    mut read_stream: Stream,
    write_stream: Stream,
) -> anyhow::Result<()> {
    // The pane holds the channel's only sender, and refusals go through it,
    // so an observer the pane drops at its budget loses its stream: the writer
    // runs dry and shuts the read below down, rather than the client sitting
    // on a connection nothing will ever be sent down again.
    let (tx, rx) = mpsc::channel::<DaemonMsg>();
    let gate = Arc::new(crate::daemon::pane::OutputGate::new());
    let (observer_id, initial) = pane.observe(tx, gate.clone());
    let writer = spawn_writer(rx, write_stream, gate, initial);

    observe_loop(
        &mut read_stream,
        |msg| pane.tell_observer(observer_id, msg),
        |request| pane.observer_lease(observer_id, request),
    );

    pane.unobserve(observer_id);
    let _ = writer.join();
    Ok(())
}

fn observe_loop<R: std::io::Read>(
    read_stream: &mut R,
    mut refuse: impl FnMut(DaemonMsg) -> bool,
    mut lease: impl FnMut(LeaseRequest),
) {
    loop {
        match ClientMsg::read(read_stream) {
            Ok(ClientMsg::Lease(request)) => lease(request),
            Ok(ClientMsg::Input(_)) | Ok(ClientMsg::Resize(_)) => {
                if !refuse(DaemonMsg::Error(
                    "this connection is a read-only observer; attach to write".to_string(),
                )) {
                    break;
                }
            }
            Ok(ClientMsg::Detach) => break,
            Ok(_) => {}
            Err(_) => break,
        }
    }
}

fn run_stream(
    pane: Arc<DaemonPane>,
    id: u64,
    epoch: u64,
    rx: Receiver<DaemonMsg>,
    mut read_stream: Stream,
    write_stream: Stream,
    registry: Arc<Registry>,
) -> anyhow::Result<()> {
    use std::io::Read as _;

    // Blocking reads: when this controller is displaced its writer's channel
    // closes, and the writer shuts our read side down on its way out (see
    // spawn_writer), so the read below returns instead of hanging.
    let writer = spawn_writer(rx, write_stream, pane.gate(), Vec::new());

    let mut killed = false;
    let mut pending: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 65536];
    'conn: loop {
        loop {
            let (kind, payload) = match crate::daemon::protocol::take_frame(&mut pending) {
                Ok(Some(frame)) => frame,
                Ok(None) => break,
                Err(_) => break 'conn,
            };
            let Ok(msg) = ClientMsg::from_frame(kind, payload) else {
                break 'conn;
            };
            match msg {
                ClientMsg::Input(bytes) => {
                    if !pane.controls(epoch) {
                        break 'conn;
                    }
                    pane.write_input(&bytes);
                }
                ClientMsg::Resize(size) => {
                    if !pane.controls(epoch) {
                        break 'conn;
                    }
                    pane.resize(size);
                }
                ClientMsg::Lease(request) => {
                    if !pane.controls(epoch) {
                        break 'conn;
                    }
                    pane.controller_lease(request);
                }
                ClientMsg::AuthResponse {
                    request_id,
                    response,
                } => pane.deliver_auth_response(request_id, response),
                ClientMsg::Detach => break 'conn,
                ClientMsg::Kill { pane_id } => {
                    if pane_id == id {
                        killed = true;
                        break 'conn;
                    }
                    kill_pane(&registry, pane_id);
                }
                _ => {}
            }
        }
        match read_stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => pending.extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }

    // Asked after the loop, before detach clears the seat: whoever no longer
    // holds the epoch was preempted rather than having hung up on their own.
    let displaced = !pane.controls(epoch);
    let reclaimable = pane.detach(epoch);
    if displaced {
        let _ = read_stream.shutdown(std::net::Shutdown::Both);
    }
    let _ = writer.join();

    if killed {
        kill_pane(&registry, id);
    } else if reclaimable {
        registry.remove(id);
    }
    Ok(())
}

const OUTPUT_COALESCE_CAP: usize = 256 * 1024;

pub(crate) fn spawn_writer(
    rx: Receiver<DaemonMsg>,
    mut write_stream: Stream,
    gate: Arc<crate::daemon::pane::OutputGate>,
    initial: Vec<DaemonMsg>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("tty7-daemon-writer".to_string())
        .spawn(move || {
            crate::core::threads::promote_to_user_interactive();
            let mut carried: Option<DaemonMsg> = None;
            let mut initial = initial.into_iter();
            loop {
                let replaying = initial.len() > 0;
                let msg = match initial.next().or_else(|| carried.take()) {
                    Some(m) => m,
                    None => match rx.recv() {
                        Ok(m) => m,
                        Err(_) => break,
                    },
                };
                let msg = if let DaemonMsg::Output(mut buf) = msg {
                    while !replaying && buf.len() < OUTPUT_COALESCE_CAP {
                        match rx.try_recv() {
                            Ok(DaemonMsg::Output(more)) => buf.extend_from_slice(&more),
                            Ok(other) => {
                                carried = Some(other);
                                break;
                            }
                            Err(_) => break,
                        }
                    }
                    DaemonMsg::Output(buf)
                } else {
                    msg
                };
                let drained = if replaying {
                    0
                } else {
                    crate::daemon::pane::gated_len(&msg)
                };
                let write_ok = msg.encode(&mut write_stream).is_ok();
                if drained > 0 {
                    gate.sub(drained);
                }
                if !write_ok {
                    break;
                }
                let _ = write_stream.flush();
            }
            // This half and the connection's read half are try_clone'd views of
            // one socket, so shutting the read side down here unblocks whoever
            // is parked in read() on the other handle. The writer's channel
            // closing IS the handover signal — a new controller replaces the
            // subscriber, the old Sender drops, rx.recv() fails, and we land
            // here — so the displaced reader wakes at once and nobody has to
            // poll for it.
            let _ = write_stream.shutdown(std::net::Shutdown::Read);
        })
        .expect("spawn daemon writer thread")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mobile_agent_list_serializes_the_live_badge_and_its_revision() {
        use crate::core::cli_agent::{AgentStatus, CLIAgent};
        use crate::daemon::pane::MobileAgentState;
        let tab = crate::core::machine::Tab::leaf(7);
        let info = crate::daemon::protocol::PaneInfo {
            pane_id: 7,
            cwd: None,
            title: "omp".into(),
            osc_title: None,
            alive: true,
            owner: None,
        };
        let size = crate::daemon::protocol::WinSize {
            cols: 80,
            rows: 24,
            cell_w: 0,
            cell_h: 0,
        };
        for (revision, status) in [
            (1, AgentStatus::Working),
            (2, AgentStatus::Waiting),
            (3, AgentStatus::Done),
        ] {
            let terminal = mobile_terminal(
                &tab, &info, size,
                MobileAgentState { revision, agent: Some((CLIAgent::OhMyPi, status)) },
                None, None,
            );
            assert_eq!(terminal["agent"]["status"], serde_json::to_value(status).unwrap());
            assert_eq!(terminal["agent_revision"], revision);
            assert_eq!(terminal["agent"]["name"], CLIAgent::OhMyPi.display_name());
        }
        let terminal = mobile_terminal(
            &tab, &info, size, MobileAgentState { revision: 4, agent: None }, None, None,
        );
        assert!(terminal["agent"].is_null());
        assert_eq!(terminal["agent_revision"], 4);
    }

    #[test]
    fn mobile_repository_metadata_uses_worktree_branch_and_shared_repo_home() {
        let directory = tempfile::tempdir().unwrap();
        let repo = directory.path().join("repo");
        let worktree = directory.path().join("topic");
        std::fs::create_dir(&repo).unwrap();
        let git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(&["init", "-b", "main"]);
        git(&[
            "-c",
            "user.name=tty7 test",
            "-c",
            "user.email=tty7@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--allow-empty",
            "-m",
            "initial",
        ]);
        git(&[
            "worktree",
            "add",
            "-b",
            "feature/mobile",
            worktree.to_str().unwrap(),
        ]);
        let main = mobile_repo(&repo);
        let topic = mobile_repo(&worktree);
        assert_eq!(main.branch.as_deref(), Some("main"));
        assert_eq!(topic.branch.as_deref(), Some("feature/mobile"));
        assert_eq!(main.home, Some(Some(std::fs::canonicalize(&repo).unwrap())));
        assert_eq!(topic.home, main.home);
        assert_eq!(mobile_repo(directory.path()).home, Some(None));
    }

    #[test]
    fn mobile_sections_follow_desktop_order_and_disambiguate_paths() {
        use crate::core::group_key::{AutoKey, GroupKey, PinnedGroup, WorkspaceGroups};
        let first = PinnedGroup::folder(std::path::Path::new("/kept/a/project"));
        let second = PinnedGroup::folder(std::path::Path::new("/kept/b/project"));
        let groups = WorkspaceGroups {
            pinned: vec![first.clone(), second.clone()],
            ..Default::default()
        };
        let repo_a = GroupKey::Auto(AutoKey::Repo("/repos/a/project".into()));
        let repo_b = GroupKey::Auto(AutoKey::Repo("/repos/b/project".into()));
        let host = GroupKey::Auto(AutoKey::SshHost("dev@host".into()));
        let keys = vec![
            Some(repo_b.clone()),
            None,
            Some(GroupKey::Pinned(second.id)),
            Some(repo_a),
            Some(GroupKey::Pinned(first.id)),
            Some(repo_b),
            Some(host),
        ];
        assert_eq!(
            mobile_sections(&keys, &groups),
            vec![
                (Some("a/project".into()), vec![4]),
                (Some("b/project".into()), vec![2]),
                (Some("b/project".into()), vec![0, 5]),
                (Some("a/project".into()), vec![3]),
                (Some("dev@host".into()), vec![6]),
                (None, vec![1]),
            ]
        );
    }

    #[test]
    fn mobile_groups_respect_explicit_membership_and_disabled_auto_grouping() {
        use crate::core::group_key::{AutoKey, GroupKey, PinnedGroup, WorkspaceGroups, place};
        let kept = PinnedGroup::label("My agents");
        let removed = PinnedGroup::label("Deleted");
        let groups = WorkspaceGroups {
            pinned: vec![kept.clone()],
            ..Default::default()
        };
        let auto = Some(AutoKey::Repo("/repo".into()));
        assert_eq!(
            place(Some(kept.id), &groups, false, auto.clone()),
            Some(GroupKey::Pinned(kept.id))
        );
        assert_eq!(
            place(Some(removed.id), &groups, true, auto.clone()),
            auto.clone().map(GroupKey::Auto)
        );
        assert_eq!(place(None, &groups, false, auto), None);
        assert_eq!(
            mobile_sections(&[None], &WorkspaceGroups::default()),
            vec![(None, vec![0])]
        );
        // No containment rule reassigns a tab the user removed from a folder group.
        let folder = PinnedGroup::folder(std::path::Path::new("/repo"));
        let groups = WorkspaceGroups {
            pinned: vec![folder],
            ..Default::default()
        };
        let key = place(None, &groups, true, Some(AutoKey::Repo("/repo".into())));
        assert!(matches!(key, Some(GroupKey::Auto(_))));
    }

    #[test]
    fn equal_named_adjacent_groups_keep_distinct_stable_identities() {
        use crate::core::group_key::{AutoKey, GroupKey, PinnedGroup, WorkspaceGroups};
        let first = PinnedGroup::label("project");
        let second = PinnedGroup::label("project");
        let groups = WorkspaceGroups {
            pinned: vec![first.clone(), second.clone()],
            ..Default::default()
        };
        let keys = vec![
            Some(GroupKey::Pinned(first.id)),
            Some(GroupKey::Pinned(second.id)),
            Some(GroupKey::Auto(AutoKey::Repo("/repos/project".into()))),
            Some(GroupKey::Auto(AutoKey::SshHost("/repos/project".into()))),
        ];
        let sections = mobile_sections(&keys, &groups);
        assert_eq!(sections[0], (Some("project".into()), vec![0]));
        assert_eq!(sections[1], (Some("project".into()), vec![1]));
        assert_eq!(sections[2], (Some("project".into()), vec![2]));
        let ids: Vec<_> = keys
            .iter()
            .map(|key| mobile_group_id(key.as_ref()).unwrap())
            .collect();
        for i in 0..ids.len() {
            assert!(!ids[..i].contains(&ids[i]));
            assert_eq!(Some(ids[i].clone()), mobile_group_id(keys[i].as_ref()));
        }
        assert_eq!(ids[0], format!("pinned:{}", first.id));
        assert_eq!(ids[2], r#"auto:{"repo":"/repos/project"}"#);
        assert_eq!(ids[3], r#"auto:{"ssh_host":"/repos/project"}"#);
        assert_eq!(mobile_group_id(None), None);
    }

    #[test]
    fn mobile_input_and_observe_reject_cross_workspace_panes() {
        use crate::core::machine::{MachineStore, PaneSeed};
        use crate::mobile::Backend as _;
        let directory = tempfile::tempdir().unwrap();
        let machine = MachineStore::open(directory.path().join("machine.json"));
        let owner = machine.workspace_create(None, None, None).unwrap();
        let unrelated = machine.workspace_create(None, None, None).unwrap();
        machine
            .tab_create(owner.id, None, PaneSeed::bare(7), None, None)
            .unwrap();
        let backend = MobileBackend {
            registry: Arc::new(Registry::new()),
            machine,
        };
        let error = backend
            .request(&serde_json::json!({
                "op":"input", "workspace_id":unrelated.id.to_string(), "pane_id":7, "data":"YQ"
            }))
            .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        let error = backend
            .observe(&unrelated.id.to_string(), 7)
            .err()
            .expect("must refuse observation");
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn alloc_id_is_monotonic_from_one() {
        let reg = Registry::new();
        assert_eq!(reg.alloc_id(), 1);
        assert_eq!(reg.alloc_id(), 2);
        assert_eq!(reg.alloc_id(), 3);
    }

    #[test]
    fn pane_ids_never_alias_what_the_persisted_tree_references() {
        use crate::core::machine::{MachineStore, PaneSeed};
        let dir = tempfile::TempDir::new().unwrap();
        let store = MachineStore::open(dir.path().join("machine.json"));
        let ws = store.workspace_create(None, None, None).unwrap();
        store
            .tab_create(ws.id, None, PaneSeed::bare(7), None, None)
            .unwrap();

        let reg = Registry::new();
        reg.seed_ids_past(&store.machine());
        assert_eq!(reg.alloc_id(), 8, "past the highest id the tree names");

        reg.seed_ids_past(&store.machine());
        assert_eq!(reg.alloc_id(), 9);
    }

    #[test]
    fn a_tree_naming_the_maximum_pane_id_does_not_panic_the_seed() {
        use crate::core::machine::{Machine, PaneRecord};
        let reg = Registry::new();
        reg.seed_ids_past(&Machine {
            workspaces: Vec::new(),
            panes: vec![PaneRecord::new(u64::MAX)],
        });
        assert_eq!(reg.alloc_id(), u64::MAX);
    }

    #[test]
    fn empty_registry_get_remove_list_are_empty() {
        let reg = Registry::new();
        assert!(reg.get(1).is_none());
        assert!(reg.remove(1).is_none());
        assert!(reg.list().is_empty());
    }

    #[test]
    fn an_empty_registry_serves_empty_aggregates() {
        use crate::host::server::PaneDirectory as _;
        let reg = Registry::new();
        assert_eq!(reg.pane_count(), 0);
        assert!(reg.agent_states().is_empty());
    }

    #[test]
    fn the_observer_loop_refuses_writes_and_honors_detach() {
        let size = crate::daemon::protocol::WinSize {
            cols: 80,
            rows: 24,
            cell_w: 8,
            cell_h: 17,
        };
        let mut wire = Vec::new();
        ClientMsg::Input(b"echo hijack\r".to_vec())
            .encode(&mut wire)
            .unwrap();
        ClientMsg::Resize(size).encode(&mut wire).unwrap();
        ClientMsg::QueryProcs { pane_id: 1 }
            .encode(&mut wire)
            .unwrap();
        ClientMsg::Detach.encode(&mut wire).unwrap();
        ClientMsg::Input(b"too late".to_vec())
            .encode(&mut wire)
            .unwrap();

        let (tx, rx) = std::sync::mpsc::channel();
        observe_loop(
            &mut std::io::Cursor::new(wire),
            |m| tx.send(m).is_ok(),
            |_| {},
        );
        drop(tx);

        assert!(
            matches!(rx.try_recv(), Ok(DaemonMsg::Error(m)) if m.contains("read-only")),
            "an observer's Input must be answered with an Error, not forwarded"
        );
        assert!(
            matches!(rx.try_recv(), Ok(DaemonMsg::Error(m)) if m.contains("read-only")),
            "an observer's Resize must be answered with an Error, not applied"
        );
        assert!(
            rx.try_recv().is_err(),
            "Detach ends the loop; frames after it are never read"
        );
    }

    #[test]
    fn the_observer_loop_ends_at_stream_eof() {
        let (tx, rx) = std::sync::mpsc::channel();
        observe_loop(
            &mut std::io::Cursor::new(Vec::<u8>::new()),
            |m| tx.send(m).is_ok(),
            |_| {},
        );
        drop(tx);
        assert!(rx.try_recv().is_err());
    }

    #[cfg(unix)]
    mod conn {
        use super::super::{OUTPUT_COALESCE_CAP, Registry, handle_conn, spawn_writer};
        use crate::daemon::protocol::{ClientMsg, DaemonMsg, WinSize};
        use std::os::unix::net::UnixStream;
        use std::sync::{Arc, mpsc};
        use std::thread;

        const SIZE: WinSize = WinSize {
            cols: 80,
            rows: 24,
            cell_w: 8,
            cell_h: 17,
        };

        fn serve() -> (UnixStream, thread::JoinHandle<()>) {
            let (client, server) = UnixStream::pair().unwrap();
            let reg = Arc::new(Registry::new());
            let h = thread::spawn(move || {
                let _ = handle_conn(server, reg);
            });
            (client, h)
        }

        #[test]
        fn list_on_empty_registry_replies_with_empty_pane_list() {
            let (mut client, h) = serve();
            ClientMsg::List.encode(&mut client).unwrap();
            assert_eq!(
                DaemonMsg::read(&mut client).unwrap(),
                DaemonMsg::PaneList(vec![])
            );
            h.join().unwrap();
        }

        #[test]
        fn attach_to_missing_pane_reports_error() {
            let (mut client, h) = serve();
            ClientMsg::Attach {
                pane_id: 999,
                size: SIZE,
                allow_remote_clipboard_write: false,
            }
            .encode(&mut client)
            .unwrap();
            match DaemonMsg::read(&mut client).unwrap() {
                DaemonMsg::Error(msg) => assert!(msg.contains("999"), "error names the id"),
                other => panic!("expected Error, got {other:?}"),
            }
            h.join().unwrap();
        }

        #[test]
        fn observe_of_a_missing_pane_reports_error() {
            let (mut client, h) = serve();
            ClientMsg::Observe {
                pane_id: 999,
                size: SIZE,
            }
            .encode(&mut client)
            .unwrap();
            match DaemonMsg::read(&mut client).unwrap() {
                DaemonMsg::Error(msg) => assert!(msg.contains("999"), "error names the id"),
                other => panic!("expected Error, got {other:?}"),
            }
            h.join().unwrap();
        }

        #[test]
        fn kill_unknown_pane_closes_without_reply() {
            let (mut client, h) = serve();
            ClientMsg::Kill { pane_id: 123 }
                .encode(&mut client)
                .unwrap();
            assert!(DaemonMsg::read(&mut client).is_err());
            h.join().unwrap();
        }

        #[test]
        fn unexpected_opening_message_is_ignored_and_closed() {
            let (mut client, h) = serve();
            ClientMsg::Resize(SIZE).encode(&mut client).unwrap();
            assert!(DaemonMsg::read(&mut client).is_err());
            h.join().unwrap();
        }

        #[test]
        fn spawn_writer_frames_messages_then_exits_on_channel_close() {
            let (tx, rx) = mpsc::channel::<DaemonMsg>();
            let (mut client, server) = UnixStream::pair().unwrap();
            let writer = spawn_writer(
                rx,
                server,
                Arc::new(crate::daemon::pane::OutputGate::new()),
                Vec::new(),
            );

            tx.send(DaemonMsg::Output(b"hi".to_vec())).unwrap();
            tx.send(DaemonMsg::Exited { code: Some(0) }).unwrap();
            assert_eq!(
                DaemonMsg::read(&mut client).unwrap(),
                DaemonMsg::Output(b"hi".to_vec())
            );
            assert_eq!(
                DaemonMsg::read(&mut client).unwrap(),
                DaemonMsg::Exited { code: Some(0) }
            );

            drop(tx);
            writer.join().unwrap();
        }

        #[test]
        fn observer_writer_sends_owned_replay_before_live_and_debits_only_live() {
            let (tx, rx) = mpsc::channel();
            let gate = Arc::new(crate::daemon::pane::OutputGate::new());
            let initial = vec![
                DaemonMsg::Snapshot(b"\x1b[?1049h".to_vec()),
                DaemonMsg::Size(SIZE),
                DaemonMsg::Snapshot(vec![b'x'; crate::daemon::pane::OBSERVER_BUDGET as usize]),
                DaemonMsg::Cwd("/work".into()),
            ];
            tx.send(DaemonMsg::Size(WinSize { cols: 100, ..SIZE }))
                .unwrap();
            gate.add(b"first live".len());
            tx.send(DaemonMsg::Output(b"first live".to_vec())).unwrap();
            gate.add(b"queued snapshot".len());
            tx.send(DaemonMsg::Snapshot(b"queued snapshot".to_vec()))
                .unwrap();
            drop(tx);
            let (mut client, server) = UnixStream::pair().unwrap();
            let writer = spawn_writer(rx, server, gate.clone(), initial);
            assert_eq!(
                DaemonMsg::read(&mut client).unwrap(),
                DaemonMsg::Snapshot(b"\x1b[?1049h".to_vec())
            );
            assert_eq!(DaemonMsg::read(&mut client).unwrap(), DaemonMsg::Size(SIZE));
            let DaemonMsg::Snapshot(bytes) = DaemonMsg::read(&mut client).unwrap() else {
                panic!("missing owned snapshot");
            };
            assert_eq!(bytes.len(), crate::daemon::pane::OBSERVER_BUDGET as usize);
            assert!(bytes.iter().all(|byte| *byte == b'x'));
            assert_eq!(
                DaemonMsg::read(&mut client).unwrap(),
                DaemonMsg::Cwd("/work".into())
            );
            assert_eq!(
                DaemonMsg::read(&mut client).unwrap(),
                DaemonMsg::Size(WinSize { cols: 100, ..SIZE })
            );
            assert_eq!(
                DaemonMsg::read(&mut client).unwrap(),
                DaemonMsg::Output(b"first live".to_vec())
            );
            assert_eq!(
                DaemonMsg::read(&mut client).unwrap(),
                DaemonMsg::Snapshot(b"queued snapshot".to_vec())
            );
            writer.join().unwrap();
            assert_eq!(
                gate.queued_bytes(),
                0,
                "owned replay must not debit the live gate"
            );
        }

        #[test]
        fn spawn_writer_coalesces_queued_outputs_into_one_frame() {
            let (tx, rx) = mpsc::channel::<DaemonMsg>();
            tx.send(DaemonMsg::Output(b"one".to_vec())).unwrap();
            tx.send(DaemonMsg::Output(b"two".to_vec())).unwrap();
            tx.send(DaemonMsg::Output(b"three".to_vec())).unwrap();
            drop(tx);

            let (mut client, server) = UnixStream::pair().unwrap();
            let writer = spawn_writer(
                rx,
                server,
                Arc::new(crate::daemon::pane::OutputGate::new()),
                Vec::new(),
            );

            assert_eq!(
                DaemonMsg::read(&mut client).unwrap(),
                DaemonMsg::Output(b"onetwothree".to_vec()),
                "queued Outputs must merge into one frame, bytes in send order"
            );
            assert!(DaemonMsg::read(&mut client).is_err());
            writer.join().unwrap();
        }

        #[test]
        fn spawn_writer_splits_output_backlog_at_the_coalesce_cap() {
            const CHUNK: usize = 64 * 1024;
            let chunks: Vec<Vec<u8>> = (0u8..6).map(|i| vec![i; CHUNK]).collect();
            let expected: Vec<u8> = chunks.concat();

            let (tx, rx) = mpsc::channel::<DaemonMsg>();
            for chunk in &chunks {
                tx.send(DaemonMsg::Output(chunk.clone())).unwrap();
            }
            drop(tx);

            let (mut client, server) = UnixStream::pair().unwrap();
            let writer = spawn_writer(
                rx,
                server,
                Arc::new(crate::daemon::pane::OutputGate::new()),
                Vec::new(),
            );

            let mut frames: Vec<Vec<u8>> = Vec::new();
            loop {
                match DaemonMsg::read(&mut client) {
                    Ok(DaemonMsg::Output(bytes)) => frames.push(bytes),
                    Ok(other) => panic!("expected only Output frames, got {other:?}"),
                    Err(_) => break,
                }
            }
            writer.join().unwrap();

            assert!(
                frames.len() >= 2,
                "a backlog over the cap must be split into multiple frames, got {}",
                frames.len()
            );
            for frame in &frames {
                assert!(
                    frame.len() <= OUTPUT_COALESCE_CAP + CHUNK,
                    "frame of {} bytes exceeds the cap by more than one message",
                    frame.len()
                );
            }
            assert_eq!(frames.concat(), expected, "no bytes lost or reordered");
        }

        #[test]
        fn spawn_writer_does_not_coalesce_outputs_across_a_non_output_message() {
            let (tx, rx) = mpsc::channel::<DaemonMsg>();
            tx.send(DaemonMsg::Output(b"before".to_vec())).unwrap();
            tx.send(DaemonMsg::Exited { code: Some(0) }).unwrap();
            tx.send(DaemonMsg::Output(b"after".to_vec())).unwrap();
            drop(tx);

            let (mut client, server) = UnixStream::pair().unwrap();
            let writer = spawn_writer(
                rx,
                server,
                Arc::new(crate::daemon::pane::OutputGate::new()),
                Vec::new(),
            );

            assert_eq!(
                DaemonMsg::read(&mut client).unwrap(),
                DaemonMsg::Output(b"before".to_vec()),
                "the first Output must not absorb bytes from beyond the Exited"
            );
            assert_eq!(
                DaemonMsg::read(&mut client).unwrap(),
                DaemonMsg::Exited { code: Some(0) },
                "the interrupting message keeps its place in the sequence"
            );
            assert_eq!(
                DaemonMsg::read(&mut client).unwrap(),
                DaemonMsg::Output(b"after".to_vec())
            );
            assert!(DaemonMsg::read(&mut client).is_err());
            writer.join().unwrap();
        }

        #[test]
        fn spawn_writer_exits_on_write_failure_while_sender_is_alive() {
            let (tx, rx) = mpsc::channel::<DaemonMsg>();
            let (client, server) = UnixStream::pair().unwrap();
            let writer = spawn_writer(
                rx,
                server,
                Arc::new(crate::daemon::pane::OutputGate::new()),
                Vec::new(),
            );

            drop(client);

            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
            while !writer.is_finished() && std::time::Instant::now() < deadline {
                let _ = tx.send(DaemonMsg::Output(b"into the void".to_vec()));
                thread::sleep(std::time::Duration::from_millis(5));
            }
            assert!(
                writer.is_finished(),
                "writer must exit once the socket write fails, without waiting for channel close"
            );
            writer.join().unwrap();
            drop(tx);
        }

        #[test]
        fn a_closed_writer_channel_wakes_the_controller_parked_in_read() {
            let (client, server) = UnixStream::pair().unwrap();
            let mut reader = server.try_clone().expect("clone the read half");
            let (tx, rx) = mpsc::channel::<DaemonMsg>();
            let writer = spawn_writer(
                rx,
                server,
                Arc::new(crate::daemon::pane::OutputGate::new()),
                Vec::new(),
            );

            // Exactly where run_stream sits between frames.
            let parked = thread::spawn(move || {
                let mut buf = [0u8; 64];
                std::io::Read::read(&mut reader, &mut buf).ok()
            });

            // A handover: the pane seats a new controller and drops this one's
            // Sender. Nothing else happens — no poll, no timeout.
            drop(tx);

            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
            while !parked.is_finished() && std::time::Instant::now() < deadline {
                thread::sleep(std::time::Duration::from_millis(5));
            }
            assert!(
                parked.is_finished(),
                "a displaced controller must be woken by the writer's shutdown; \
                 if this hangs, run_stream is back to polling for the handover"
            );
            assert_eq!(
                parked.join().unwrap(),
                Some(0),
                "the woken read must report EOF, not a partial frame"
            );
            writer.join().unwrap();
            drop(client);
        }
    }
}
