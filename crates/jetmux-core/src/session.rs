//! The session model: tabs of pane layouts, plus one view per attached
//! client. Each client has its own active tab, focus and zoom; panes, tabs
//! and copy mode are shared.

use std::collections::{HashMap, HashSet};

use jetmux_proto::rpc::{PaneRef, TabRef};
use std::os::fd::OwnedFd;

use jetmux_proto::{Action, LayoutSpec, PaneSpec, PaneView, Rect, Size, TabSpec, TabView, View};

use crate::snapshot::{PaneSnapshot, SessionSnapshot, TabSnapshot};
use crate::{Error, Pane, Spawn};
use jet_core::{CopyOutcome, KeyInput, Mods, MouseOutcome, VtSettings};
use jetmux_proto::input::{KeyEvent, MouseEvent, Theme};
use jetmux_proto::layout::{Layout, PaneId, neighbor};

pub type ClientId = usize;
type TabId = u32;

/// Result of an action that the caller needs to act on.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    None,
    /// Text for the acting client's clipboard.
    Clipboard(String),
    /// A note for the acting client's status bar.
    Message(String),
    /// The acting client should paste its clipboard.
    WantPaste(PaneId),
}

struct Tab {
    id: TabId,
    /// Set by the user; otherwise the tab shows its focused pane's title.
    name: Option<String>,
    layout: Layout,
}

#[derive(Clone, Debug, Default)]
struct ClientView {
    /// The size the client is laid out for: its own, or for a follower the
    /// size owner's.
    size: Size,
    /// The client's actual terminal size.
    real_size: Size,
    tab: TabId,
    focus: HashMap<TabId, PaneId>,
    zoomed: HashSet<TabId>,
}

/// What an action run through [`Session::act_on`] left focused, so callers
/// can report the pane a split or new tab created.
#[derive(Debug, PartialEq, Eq)]
pub struct Acted {
    pub outcome: Outcome,
    pub focus: Option<PaneId>,
}

/// Scripted actions run as this pseudo-client.
const SCRIPT: ClientId = ClientId::MAX;

pub struct Session {
    name: String,
    spawn: Spawn,
    tabs: Vec<Tab>,
    panes: HashMap<PaneId, Pane>,
    views: HashMap<ClientId, ClientView>,
    /// Where new clients start: the view of the last client to leave, as
    /// moved by scripted actions since.
    home: ClientView,
    /// The client whose geometry the panes follow: the last one to act.
    size_owner: Option<ClientId>,
    /// Clients that follow the size owner's layout rather than their own.
    followers: HashSet<ClientId>,
    /// Terminal size used to lay out panes when no client owns the size.
    last_size: Size,
    /// Last text yanked in copy mode, for [`Action::Paste`].
    buffer: Option<String>,
    next_pane: PaneId,
    next_tab: TabId,
    /// The profile the session was started from, kept for restores.
    profile: Option<String>,
    /// Clients' colors, and the ones the panes answer with now: those of
    /// the last client to act, as with sizes.
    themes: HashMap<ClientId, VtSettings>,
    settings: VtSettings,
    /// The config's colors, for when no client sent any.
    base: VtSettings,
    /// Clients' cell sizes in pixels, and the one panes are laid out for,
    /// chosen the same way.
    cells: HashMap<ClientId, (u32, u32)>,
    cell: (u32, u32),
}

/// The pane area for a client of this size: everything above the status bar.
pub fn pane_area(size: Size) -> Rect {
    Rect {
        x: 0,
        y: 0,
        cols: size.cols.max(1),
        rows: size.rows.saturating_sub(1).max(1),
    }
}

impl Session {
    /// Start a session with `tabs`, or with one tab running `spawn` when
    /// there are none. `spawn.cwd` doubles as the cwd for later panes that
    /// can't inherit one.
    pub fn new(name: String, spawn: Spawn, size: Size, tabs: &[TabSpec]) -> Result<Self, Error> {
        let mut session = Self {
            name,
            spawn,
            tabs: Vec::new(),
            panes: HashMap::new(),
            views: HashMap::new(),
            home: ClientView {
                size,
                ..ClientView::default()
            },
            size_owner: None,
            followers: HashSet::new(),
            last_size: size,
            buffer: None,
            next_pane: 0,
            next_tab: 0,
            profile: None,
            themes: HashMap::new(),
            settings: VtSettings::default(),
            base: VtSettings::default(),
            cells: HashMap::new(),
            cell: (0, 0),
        };
        let command = std::mem::take(&mut session.spawn.command);
        if tabs.is_empty() {
            session.new_tab(0, PaneSpec { command, cwd: None })?;
        }
        for tab in tabs {
            session.build_tab(tab)?;
        }
        session.home.tab = session.tabs.first().map_or(0, |t| t.id);
        Ok(session)
    }

    /// Add a tab from a spec, spawning each pane at its laid-out size.
    fn build_tab(&mut self, spec: &TabSpec) -> Result<(), Error> {
        let mut specs = Vec::new();
        let first = self.next_pane;
        let layout = Layout::from_spec(&spec.layout, &mut |pane: &PaneSpec| {
            specs.push(pane.clone());
            first + PaneId::try_from(specs.len() - 1).unwrap_or(PaneId::MAX)
        })
        .ok_or_else(|| Error::Spec("a split needs at least one pane".into()))?;
        let rects: HashMap<PaneId, Rect> = layout
            .rects(pane_area(self.last_size))
            .into_iter()
            .collect();
        for (id, pane) in (first..).zip(specs) {
            let size = rects.get(&id).map_or(self.last_size, |r| r.size());
            self.spawn_pane(pane, size)?;
        }
        let id = self.next_tab;
        self.next_tab += 1;
        self.tabs.push(Tab {
            id,
            name: spec.name.clone(),
            layout,
        });
        Ok(())
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn set_profile(&mut self, profile: Option<String>) {
        self.profile = profile;
    }

    /// Bring a session back after a restart, from what
    /// [`Session::snapshot`] saved. Each pane gets a fresh process: its
    /// declared command if `rerun` allows it, otherwise a shell in its old
    /// cwd with that command (or the last one run there) typed but not run.
    /// The old screen and scrollback are painted above a marker.
    pub fn restore(
        snapshot: &SessionSnapshot,
        spawn: Spawn,
        rerun: impl Fn(&[String]) -> bool,
    ) -> Result<Self, Error> {
        let mut typed = Vec::new();
        let tabs: Vec<TabSpec> = snapshot
            .tabs
            .iter()
            .map(|tab| {
                let mut panes = tab.panes.iter();
                let layout = map_leaves(&tab.layout, &mut |spec: &PaneSpec| {
                    let saved = panes.next();
                    let run = !spec.command.is_empty() && rerun(&spec.command);
                    let retype = if spec.command.is_empty() {
                        saved.and_then(|p| p.last_command.clone())
                    } else if run {
                        None
                    } else {
                        Some(shell_words(&spec.command))
                    };
                    typed.push((saved.and_then(|p| p.history.clone()), retype));
                    PaneSpec {
                        command: if run {
                            spec.command.clone()
                        } else {
                            Vec::new()
                        },
                        cwd: spec.cwd.clone(),
                    }
                });
                TabSpec {
                    name: tab.name.clone(),
                    layout,
                }
            })
            .collect();
        let mut session = Self::new(snapshot.name.clone(), spawn, snapshot.size, &tabs)?;
        session.profile = snapshot.profile.clone();
        // Panes are numbered in leaf order from 0, the order `typed` is in.
        for (id, (history, retype)) in (0..).zip(typed) {
            let Some(pane) = session.panes.get_mut(&id) else {
                continue;
            };
            if let Some(history) = history {
                pane.paint(history.as_bytes())?;
                pane.paint(RESTORE_MARKER.as_bytes())?;
            }
            if let Some(command) = retype {
                pane.type_at_prompt(command);
            }
        }
        if let Some(tab) = session.tabs.get(snapshot.active_tab) {
            session.home.tab = tab.id;
        }
        Ok(session)
    }

    /// Take over a session a previous daemon froze. `adopt` turns each
    /// pane's saved state back into a running pane.
    pub fn resume(
        snapshot: &SessionSnapshot,
        spawn: Spawn,
        mut adopt: impl FnMut(&PaneSnapshot, Vec<String>) -> Result<Pane, Error>,
    ) -> Result<Self, Error> {
        let mut session = Self {
            name: snapshot.name.clone(),
            spawn,
            tabs: Vec::new(),
            panes: HashMap::new(),
            views: HashMap::new(),
            home: ClientView {
                size: snapshot.size,
                ..ClientView::default()
            },
            size_owner: None,
            followers: HashSet::new(),
            last_size: snapshot.size,
            buffer: None,
            next_pane: 0,
            next_tab: 0,
            profile: snapshot.profile.clone(),
            themes: HashMap::new(),
            settings: VtSettings::default(),
            base: VtSettings::default(),
            cells: HashMap::new(),
            cell: (0, 0),
        };
        for tab in &snapshot.tabs {
            let mut saved = tab.panes.iter();
            let mut adopted = Vec::new();
            let layout = Layout::from_spec(&tab.layout, &mut |spec: &PaneSpec| {
                let pane = saved.next();
                let id = pane.map_or(PaneId::MAX, |p| p.id);
                adopted.push((pane, spec.command.clone()));
                id
            })
            .ok_or_else(|| Error::Spec("a saved tab has no panes".into()))?;
            for (pane, command) in adopted {
                let pane_snapshot = pane.ok_or_else(|| {
                    Error::Spec("a saved layout has more panes than state".into())
                })?;
                session
                    .panes
                    .insert(pane_snapshot.id, adopt(pane_snapshot, command)?);
                session.next_pane = session.next_pane.max(pane_snapshot.id + 1);
            }
            let id = session.next_tab;
            session.next_tab += 1;
            if let Some(focus) = tab.focus {
                session.home.focus.insert(id, focus);
            }
            session.tabs.push(Tab {
                id,
                name: tab.name.clone(),
                layout,
            });
        }
        if let Some(tab) = session.tabs.get(snapshot.active_tab) {
            session.home.tab = tab.id;
        }
        Ok(session)
    }

    /// The session as a restart would need it.
    pub fn snapshot(&self) -> Result<SessionSnapshot, Error> {
        let specs = self.pane_specs();
        self.save(&specs, &mut |id| match self.panes.get(&id) {
            Some(pane) => pane.snapshot(id),
            None => Err(Error::Spec(format!("pane {id} is gone"))),
        })
    }

    /// Stop every pane's PTY reader, ahead of [`Session::freeze`].
    pub fn stop_readers(&mut self) {
        for pane in self.panes.values_mut() {
            pane.stop_reader();
        }
    }

    /// Hand the session to a new daemon: its exact state plus the PTY
    /// masters, which must stay open until the new daemon adopts them.
    pub fn freeze(mut self) -> Result<(SessionSnapshot, Vec<OwnedFd>), Error> {
        let specs = self.pane_specs();
        let mut frozen = HashMap::new();
        let mut masters = Vec::new();
        for (id, pane) in std::mem::take(&mut self.panes) {
            let (snapshot, master) = pane.freeze(id)?;
            frozen.insert(id, snapshot);
            masters.push(master);
        }
        let snapshot = self.save(&specs, &mut |id| {
            frozen
                .remove(&id)
                .ok_or_else(|| Error::Spec(format!("pane {id} is gone")))
        })?;
        Ok((snapshot, masters))
    }

    /// Each pane's command and cwd, for saved layouts.
    fn pane_specs(&self) -> HashMap<PaneId, PaneSpec> {
        self.panes
            .iter()
            .map(|(id, pane)| {
                let spec = PaneSpec {
                    command: pane.command().to_vec(),
                    cwd: pane.cwd(),
                };
                (*id, spec)
            })
            .collect()
    }

    fn save(
        &self,
        specs: &HashMap<PaneId, PaneSpec>,
        pane: &mut impl FnMut(PaneId) -> Result<PaneSnapshot, Error>,
    ) -> Result<SessionSnapshot, Error> {
        let spec = |id: PaneId| specs.get(&id).cloned().unwrap_or_default();
        let view = self.reference_view();
        let tabs = self
            .tabs
            .iter()
            .map(|t| {
                Ok(TabSnapshot {
                    name: t.name.clone(),
                    layout: t.layout.to_spec(&spec),
                    panes: t
                        .layout
                        .panes()
                        .into_iter()
                        .map(&mut *pane)
                        .collect::<Result<_, _>>()?,
                    focus: view.focus.get(&t.id).copied(),
                })
            })
            .collect::<Result<_, Error>>()?;
        Ok(SessionSnapshot {
            name: self.name.clone(),
            profile: self.profile.clone(),
            size: self.last_size,
            active_tab: self.tabs.iter().position(|t| t.id == view.tab).unwrap_or(0),
            tabs,
        })
    }

    pub fn is_empty(&self) -> bool {
        self.panes.is_empty()
    }

    pub fn pane_ids(&self) -> Vec<PaneId> {
        self.panes.keys().copied().collect()
    }

    pub fn pane(&self, id: PaneId) -> Option<&Pane> {
        self.panes.get(&id)
    }

    pub fn panes_mut(&mut self) -> impl Iterator<Item = &mut Pane> {
        self.panes.values_mut()
    }

    pub fn pane_mut(&mut self, id: PaneId) -> Option<&mut Pane> {
        self.panes.get_mut(&id)
    }

    /// Size of the pane in the first tab, for listings.
    pub fn size(&self) -> Size {
        self.tabs
            .first()
            .and_then(|t| t.layout.panes().first().copied())
            .and_then(|id| self.panes.get(&id))
            .and_then(|p| p.size().ok())
            .unwrap_or_default()
    }

    pub fn clients(&self) -> usize {
        self.views.len()
    }

    /// A new client starts where the last one left off and takes over the
    /// pane sizes.
    /// Add a client. A following client sees the size owner's layout and
    /// never resizes panes; others take the size over.
    pub fn add_client(&mut self, client: ClientId, size: Size, follow: bool) -> Result<(), Error> {
        let view = ClientView {
            size: if follow { self.last_size } else { size },
            real_size: size,
            ..self.home.clone()
        };
        self.views.insert(client, view);
        if follow {
            self.followers.insert(client);
            return Ok(());
        }
        self.size_owner = Some(client);
        self.fit()
    }

    /// Start or stop following. A client that stops takes the size over at
    /// its own terminal size.
    pub fn set_follow(&mut self, client: ClientId, follow: bool) -> Result<(), Error> {
        let Some(view) = self.views.get_mut(&client) else {
            return Ok(());
        };
        if follow {
            self.followers.insert(client);
            view.size = self.last_size;
            if self.size_owner == Some(client) {
                self.size_owner = None;
            }
            return Ok(());
        }
        self.followers.remove(&client);
        view.size = view.real_size;
        self.size_owner = Some(client);
        self.fit()
    }

    pub fn remove_client(&mut self, client: ClientId) {
        self.followers.remove(&client);
        self.themes.remove(&client);
        self.cells.remove(&client);
        if self.themes.is_empty() {
            // A pane that can't take the reply is going away anyway.
            let _ = self.apply_settings(self.base.clone());
        }
        if let Some(view) = self.views.remove(&client) {
            self.home = view;
        }
        if self.size_owner == Some(client) {
            self.size_owner = None;
        }
    }

    pub fn resize_client(&mut self, client: ClientId, size: Size) -> Result<(), Error> {
        if let Some(view) = self.views.get_mut(&client) {
            view.real_size = size;
            if self.followers.contains(&client) {
                return Ok(());
            }
            view.size = size;
        }
        if self.size_owner.is_none_or(|owner| owner == client) {
            self.size_owner = Some(client);
        }
        self.fit()
    }

    /// Send input to the client's focused pane. Panes in copy mode take
    /// copy actions instead, so raw input to them is dropped.
    pub fn input(&mut self, client: ClientId, data: &[u8]) -> Result<(), Error> {
        self.take_size(client)?;
        let Some(id) = self.focused(client) else {
            return Ok(());
        };
        if let Some(pane) = self.panes.get_mut(&id)
            && !pane.in_copy_mode()
        {
            pane.send(data)?;
        }
        Ok(())
    }

    pub fn action(&mut self, client: ClientId, action: Action) -> Result<Outcome, Error> {
        if !self.views.contains_key(&client) {
            return Ok(Outcome::None);
        }
        self.take_size(client)?;
        let outcome = self.apply(client, action)?;
        self.fit()?;
        Ok(outcome)
    }

    /// Run an action on `pane`'s behalf, as the socket API does. Tab
    /// switches, focus changes and zoom it causes apply to every client, the
    /// way they would if one client shared them.
    pub fn act_on(&mut self, pane: PaneId, action: Action) -> Result<Acted, Error> {
        let Some(tab) = self.tabs.iter().find(|t| t.layout.contains(pane)) else {
            return Err(Error::Spec(format!("no pane %{pane}")));
        };
        let tab = tab.id;
        let mut view = self.reference_view().clone();
        view.tab = tab;
        view.focus.insert(tab, pane);
        self.views.insert(SCRIPT, view);
        let shared = matches!(
            action,
            Action::NewTab(_)
                | Action::Split(..)
                | Action::FocusPane(_)
                | Action::Focus(_)
                | Action::FocusNext
                | Action::FocusPrev
                | Action::ToggleZoom
                | Action::SelectTab(_)
                | Action::NextTab
                | Action::PrevTab
        );
        let result = self.apply(SCRIPT, action);
        if self.size_owner.is_none() {
            // Nobody is attached, so lay panes out for the script's view.
            self.fit_for(SCRIPT)?;
        }
        let focus = self.focused(SCRIPT);
        let view = self.views.remove(&SCRIPT).unwrap_or_default();
        if shared {
            let tab = self
                .tabs
                .iter()
                .any(|t| t.id == view.tab)
                .then_some(view.tab);
            if let Some(tab) = tab {
                let zoomed = view.zoomed.contains(&tab);
                for v in self.views.values_mut().chain([&mut self.home]) {
                    v.tab = tab;
                    if let Some(focus) = focus {
                        v.focus.insert(tab, focus);
                    }
                    if zoomed {
                        v.zoomed.insert(tab);
                    } else {
                        v.zoomed.remove(&tab);
                    }
                }
            }
        }
        self.fit()?;
        Ok(Acted {
            outcome: result?,
            focus,
        })
    }

    /// The view scripted calls default to: the size owner's, or where the
    /// last client left off.
    fn reference_view(&self) -> &ClientView {
        self.size_owner
            .and_then(|c| self.views.get(&c))
            .unwrap_or(&self.home)
    }

    /// Find a pane by tab and pane reference. Missing parts default to the
    /// reference view's tab and that tab's focused pane.
    pub fn resolve(&self, tab: Option<&TabRef>, pane: Option<PaneRef>) -> Result<PaneId, Error> {
        if let Some(PaneRef::Id(id)) = pane {
            return if self.panes.contains_key(&id) {
                Ok(id)
            } else {
                Err(Error::Spec(format!("no pane %{id}")))
            };
        }
        let view = self.reference_view();
        let tab = match tab {
            None => self
                .tabs
                .iter()
                .find(|t| t.id == view.tab)
                .or(self.tabs.first()),
            Some(TabRef::Index(i)) => self.tabs.get(usize::from(*i).saturating_sub(1)),
            Some(TabRef::Name(name)) => self
                .tabs
                .iter()
                .enumerate()
                .find(|(i, _)| self.tab_name(*i, view) == *name)
                .map(|(_, t)| t),
        }
        .ok_or_else(|| Error::Spec("no such tab".into()))?;
        let panes = tab.layout.panes();
        match pane {
            Some(PaneRef::Index(i)) => panes
                .get(usize::from(i).saturating_sub(1))
                .copied()
                .ok_or_else(|| Error::Spec(format!("no pane {i} in that tab"))),
            _ => view
                .focus
                .get(&tab.id)
                .copied()
                .filter(|id| tab.layout.contains(*id))
                .or_else(|| panes.first().copied())
                .ok_or_else(|| Error::Spec("empty tab".into())),
        }
    }

    pub fn tab_count(&self) -> usize {
        self.tabs.len()
    }

    /// Each tab's name and panes in layout order.
    pub fn tab_list(&self) -> Vec<(String, Vec<PaneId>)> {
        let view = self.reference_view();
        (0..self.tabs.len())
            .map(|i| (self.tab_name(i, view), self.tabs[i].layout.panes()))
            .collect()
    }

    /// The pane focused in the reference view's current tab.
    pub fn reference_focus(&self) -> Option<PaneId> {
        self.resolve(None, None).ok()
    }

    /// The session's tabs as specs that rebuild it: each pane's command and
    /// current cwd.
    pub fn layout_spec(&self) -> Vec<TabSpec> {
        let specs = self.pane_specs();
        let pane = |id: PaneId| specs.get(&id).cloned().unwrap_or_default();
        self.tabs
            .iter()
            .map(|t| TabSpec {
                name: t.name.clone(),
                layout: t.layout.to_spec(&pane),
            })
            .collect()
    }

    fn apply(&mut self, client: ClientId, action: Action) -> Result<Outcome, Error> {
        let Some(tab_index) = self.current_tab_index(client) else {
            return Ok(Outcome::None);
        };
        let focus = self.focused(client);
        match action {
            Action::NewTab(mut spec) => {
                if spec.cwd.is_none() {
                    spec.cwd = focus.and_then(|id| self.panes.get(&id)?.cwd());
                }
                let id = self.new_tab(tab_index + 1, spec)?;
                self.view_mut(client).tab = id;
            }
            Action::CloseTab => {
                for pane in self.tabs[tab_index].layout.panes() {
                    self.remove_pane(pane);
                }
            }
            Action::NextTab | Action::PrevTab | Action::SelectTab(_) => {
                let count = self.tabs.len();
                let index = match action {
                    Action::NextTab => (tab_index + 1) % count,
                    Action::PrevTab => (tab_index + count - 1) % count,
                    Action::SelectTab(i) => usize::from(i),
                    _ => tab_index,
                };
                match self.tabs.get(index) {
                    Some(tab) => {
                        let id = tab.id;
                        self.view_mut(client).tab = id;
                    }
                    None => return Ok(Outcome::Message(format!("no tab {}", index + 1))),
                }
            }
            Action::MoveTab(delta) => {
                let max = self.tabs.len().saturating_sub(1);
                let to = tab_index.saturating_add_signed(isize::from(delta)).min(max);
                let tab = self.tabs.remove(tab_index);
                self.tabs.insert(to, tab);
            }
            Action::RenameTab(name) => {
                self.tabs[tab_index].name = Some(name).filter(|n| !n.is_empty());
            }
            Action::Split(dir, mut spec) => {
                let Some(target) = focus else {
                    return Ok(Outcome::None);
                };
                if spec.cwd.is_none() {
                    spec.cwd = self.panes.get(&target).and_then(Pane::cwd);
                }
                let id = self.next_pane;
                if !self.tabs[tab_index].layout.split(target, id, dir) {
                    return Ok(Outcome::None);
                }
                // Lay the new pane out before spawning it so it starts at its
                // final size.
                let tab = self.tabs[tab_index].id;
                self.view_mut(client).zoomed.remove(&tab);
                let size = self
                    .rects(client)
                    .into_iter()
                    .find(|(p, _)| *p == id)
                    .map_or(self.last_size, |(_, r)| r.size());
                match self.spawn_pane(spec, size) {
                    Ok(_) => {
                        self.view_mut(client).focus.insert(tab, id);
                    }
                    Err(e) => {
                        self.remove_pane(id);
                        return Err(e);
                    }
                }
            }
            Action::ClosePane => {
                if let Some(id) = focus {
                    self.remove_pane(id);
                }
            }
            Action::FocusPane(id) => {
                if let Some(tab) = self.tabs.iter().find(|t| t.layout.contains(id)) {
                    let tab = tab.id;
                    let view = self.view_mut(client);
                    view.tab = tab;
                    if view.focus.insert(tab, id) != Some(id) {
                        view.zoomed.remove(&tab);
                    }
                }
            }
            Action::Focus(dir) => {
                let tab = self.tabs[tab_index].id;
                self.view_mut(client).zoomed.remove(&tab);
                if let Some(from) = focus
                    && let Some(to) = neighbor(&self.rects(client), from, dir)
                {
                    self.view_mut(client).focus.insert(tab, to);
                }
            }
            Action::FocusNext | Action::FocusPrev => {
                let tab = self.tabs[tab_index].id;
                let panes = self.tabs[tab_index].layout.panes();
                if let Some(next) = cycle(&panes, focus, action == Action::FocusNext) {
                    let view = self.view_mut(client);
                    view.zoomed.remove(&tab);
                    view.focus.insert(tab, next);
                }
            }
            Action::Swap(dir) => {
                if let Some(from) = focus
                    && let Some(to) = neighbor(&self.rects(client), from, dir)
                {
                    self.tabs[tab_index].layout.swap(from, to);
                }
            }
            Action::Rotate(forward) => {
                let panes = self.tabs[tab_index].layout.panes();
                if let Some(from) = focus
                    && let Some(to) = cycle(&panes, focus, forward)
                {
                    self.tabs[tab_index].layout.swap(from, to);
                }
            }
            Action::Resize(dir, cells) => {
                let area = pane_area(self.views[&client].size);
                if let Some(id) = focus {
                    self.tabs[tab_index].layout.resize(id, dir, cells, area);
                }
            }
            Action::SetRatio { split, permille } => {
                self.tabs[tab_index]
                    .layout
                    .set_ratio(usize::from(split), f32::from(permille) / 1000.0);
            }
            Action::ToggleZoom => {
                let tab = self.tabs[tab_index].id;
                let zoomed = &mut self.view_mut(client).zoomed;
                if !zoomed.remove(&tab) {
                    zoomed.insert(tab);
                }
            }
            Action::EnterCopyMode => {
                if let Some(pane) = focus.and_then(|id| self.panes.get_mut(&id)) {
                    pane.enter_copy_mode()?;
                }
            }
            Action::Search { query, regex } => {
                if let Some(pane) = focus.and_then(|id| self.panes.get_mut(&id)) {
                    pane.search(&query, regex)?;
                }
            }
            Action::SearchStep(n) => {
                if let Some(pane) = focus.and_then(|id| self.panes.get_mut(&id)) {
                    pane.search_step(n)?;
                }
            }
            Action::Copy(copy) => {
                let Some(pane) = focus.and_then(|id| self.panes.get_mut(&id)) else {
                    return Ok(Outcome::None);
                };
                return Ok(match pane.copy_action(copy)? {
                    CopyOutcome::Yank(text) => {
                        self.buffer = Some(text.clone());
                        Outcome::Clipboard(text)
                    }
                    CopyOutcome::Message(msg) => Outcome::Message(msg),
                    CopyOutcome::Stay | CopyOutcome::Exit => Outcome::None,
                });
            }
            Action::Paste => {
                let Some(text) = self.buffer.clone() else {
                    return Ok(Outcome::Message("nothing copied yet".into()));
                };
                if let Some(pane) = focus.and_then(|id| self.panes.get_mut(&id)) {
                    pane.paste(&text)?;
                }
            }
        }
        Ok(Outcome::None)
    }

    /// Drop a pane whose child exited (or that was closed), collapsing its
    /// split and removing its tab if it was the last pane there.
    pub fn remove_pane(&mut self, id: PaneId) {
        self.panes.remove(&id);
        let Some(index) = self.tabs.iter().position(|t| t.layout.contains(id)) else {
            return;
        };
        let layout = std::mem::replace(&mut self.tabs[index].layout, Layout::Pane(id));
        match layout.remove(id) {
            Some(layout) => self.tabs[index].layout = layout,
            None => {
                let tab = self.tabs.remove(index);
                let fallback = self
                    .tabs
                    .get(index.min(self.tabs.len().saturating_sub(1)))
                    .map(|t| t.id);
                for view in self.views.values_mut() {
                    view.focus.remove(&tab.id);
                    view.zoomed.remove(&tab.id);
                    if view.tab == tab.id
                        && let Some(fallback) = fallback
                    {
                        view.tab = fallback;
                    }
                }
            }
        }
        for view in self.views.values_mut() {
            view.focus.retain(|_, p| *p != id);
        }
        let _ = self.fit();
    }

    /// The client's view, laid out for its size.
    pub fn view(&self, client: ClientId) -> Option<View> {
        let view = self.views.get(&client)?;
        let index = self.current_tab_index(client)?;
        let focus = self.focused(client);
        let title = |id: Option<PaneId>| {
            id.and_then(|id| self.panes.get(&id))
                .map(|p| p.title().to_owned())
                .unwrap_or_default()
        };
        let tabs = self
            .tabs
            .iter()
            .enumerate()
            .map(|(i, tab)| TabView {
                id: tab.id,
                name: self.tab_name(i, view),
                layout: tab.layout.clone(),
                focused: view
                    .focus
                    .get(&tab.id)
                    .copied()
                    .filter(|id| tab.layout.contains(*id))
                    .or_else(|| tab.layout.panes().first().copied())
                    .unwrap_or_default(),
            })
            .collect();
        let panes = self
            .rects(client)
            .into_iter()
            .map(|(id, rect)| PaneView {
                id,
                rect,
                focused: Some(id) == focus,
                copy_mode: self.panes.get(&id).is_some_and(Pane::in_copy_mode),
                title: title(Some(id)),
            })
            .collect();
        Some(View {
            session: self.name.clone(),
            tabs,
            active_tab: u16::try_from(index).unwrap_or(u16::MAX),
            panes,
            zoomed: view.zoomed.contains(&self.tabs[index].id),
            area: pane_area(view.size),
        })
    }

    /// A tab's name as `view` sees it: the user's name, or the title of the
    /// pane focused there.
    fn tab_name(&self, index: usize, view: &ClientView) -> String {
        let tab = &self.tabs[index];
        if let Some(name) = &tab.name {
            return name.clone();
        }
        let title = view
            .focus
            .get(&tab.id)
            .copied()
            .filter(|id| tab.layout.contains(*id))
            .or_else(|| tab.layout.panes().first().copied())
            .and_then(|id| self.panes.get(&id))
            .map(|p| short_title(p.title()).to_owned())
            .unwrap_or_default();
        if title.is_empty() {
            "shell".to_owned()
        } else {
            title
        }
    }

    pub fn focused(&self, client: ClientId) -> Option<PaneId> {
        let view = self.views.get(&client)?;
        let tab = self.tabs.get(self.current_tab_index(client)?)?;
        view.focus
            .get(&tab.id)
            .copied()
            .filter(|id| tab.layout.contains(*id))
            .or_else(|| tab.layout.panes().first().copied())
    }

    fn current_tab_index(&self, client: ClientId) -> Option<usize> {
        let view = self.views.get(&client)?;
        self.tabs
            .iter()
            .position(|t| t.id == view.tab)
            .or((!self.tabs.is_empty()).then_some(0))
    }

    /// Pane rectangles in the client's current tab.
    fn rects(&self, client: ClientId) -> Vec<(PaneId, Rect)> {
        let (Some(view), Some(index)) = (self.views.get(&client), self.current_tab_index(client))
        else {
            return Vec::new();
        };
        let tab = &self.tabs[index];
        let area = pane_area(view.size);
        match self.focused(client) {
            Some(focus) if view.zoomed.contains(&tab.id) => vec![(focus, area)],
            _ => tab.layout.rects(area),
        }
    }

    /// Callers have already checked the client exists; the default only
    /// keeps this infallible.
    fn view_mut(&mut self, client: ClientId) -> &mut ClientView {
        self.views.entry(client).or_default()
    }

    fn take_size(&mut self, client: ClientId) -> Result<(), Error> {
        self.take_theme(client)?;
        if let Some(&cell) = self.cells.get(&client)
            && cell != self.cell
        {
            self.cell = cell;
            self.fit()?;
        }
        if self.size_owner != Some(client) && !self.followers.contains(&client) {
            self.size_owner = Some(client);
            self.fit()?;
        }
        Ok(())
    }

    /// Size the panes the size owner can see to its layout. Panes it can't
    /// see keep their size until it looks at them.
    fn fit(&mut self) -> Result<(), Error> {
        match self.size_owner {
            Some(owner) => self.fit_for(owner),
            None => Ok(()),
        }
    }

    fn fit_for(&mut self, client: ClientId) -> Result<(), Error> {
        if let Some(view) = self.views.get(&client) {
            self.last_size = view.size;
        }
        for follower in &self.followers {
            if let Some(view) = self.views.get_mut(follower) {
                view.size = self.last_size;
            }
        }
        for (id, rect) in self.rects(client) {
            if let Some(pane) = self.panes.get_mut(&id)
                && rect.cols > 0
                && rect.rows > 0
            {
                pane.resize(rect.size(), self.cell)?;
            }
        }
        Ok(())
    }

    fn new_tab(&mut self, index: usize, spec: PaneSpec) -> Result<TabId, Error> {
        let size = pane_area(self.last_size).size();
        let pane = self.spawn_pane(spec, size)?;
        let id = self.next_tab;
        self.next_tab += 1;
        self.tabs.insert(
            index.min(self.tabs.len()),
            Tab {
                id,
                name: None,
                layout: Layout::Pane(pane),
            },
        );
        Ok(id)
    }

    /// Answer apps with `client`'s colors, if it sent any.
    fn take_theme(&mut self, client: ClientId) -> Result<(), Error> {
        match self.themes.get(&client) {
            Some(settings) => self.apply_settings(settings.clone()),
            None => Ok(()),
        }
    }

    fn apply_settings(&mut self, settings: VtSettings) -> Result<(), Error> {
        if settings == self.settings {
            return Ok(());
        }
        self.settings = settings;
        for pane in self.panes.values_mut() {
            pane.configure(&self.settings)?;
        }
        Ok(())
    }

    /// The config's colors and settings, which apps see while no client
    /// has sent its theme (and under every client's colors).
    pub fn set_base(&mut self, base: VtSettings) -> Result<(), Error> {
        self.base = base;
        if self.themes.is_empty() {
            self.apply_settings(self.base.clone())?;
        }
        Ok(())
    }

    /// A client's colors. They take over at once when no one else's do.
    pub fn set_theme(&mut self, client: ClientId, theme: &Theme) -> Result<(), Error> {
        let settings = self.base.clone().with_theme(theme);
        let first = self.themes.is_empty() || self.size_owner == Some(client);
        self.themes.insert(client, settings);
        if first {
            self.take_theme(client)?;
        }
        Ok(())
    }

    /// A client's cell size in pixels. It takes over at once when no one
    /// else's has.
    pub fn set_cell_size(&mut self, client: ClientId, cell: (u32, u32)) -> Result<(), Error> {
        let first = self.cells.is_empty() || self.size_owner == Some(client);
        self.cells.insert(client, cell);
        if first && cell != self.cell {
            self.cell = cell;
            self.fit()?;
        }
        Ok(())
    }

    /// A key for the client's focused pane, encoded for its modes.
    pub fn key(
        &mut self,
        client: ClientId,
        pane: PaneId,
        event: &KeyEvent,
    ) -> Result<Outcome, Error> {
        self.take_size(client)?;
        let Some(pane) = self.panes.get_mut(&pane) else {
            return Ok(Outcome::None);
        };
        Ok(match pane.key(&KeyInput::from_wire(event))? {
            Some(text) => {
                self.buffer = Some(text.clone());
                Outcome::Clipboard(text)
            }
            None => Outcome::None,
        })
    }

    pub fn mouse(&mut self, client: ClientId, event: &MouseEvent) -> Result<Outcome, Error> {
        if !matches!(event.action, jetmux_proto::input::MouseAction::Move) {
            self.take_size(client)?;
        }
        let Some(pane) = self.panes.get_mut(&event.pane) else {
            return Ok(Outcome::None);
        };
        Ok(match pane.mouse(event)? {
            MouseOutcome::Copy(text) => Outcome::Clipboard(text),
            MouseOutcome::Paste => Outcome::WantPaste(event.pane),
            MouseOutcome::Nothing | MouseOutcome::Pty(_) => Outcome::None,
        })
    }

    pub fn wheel(
        &mut self,
        pane: PaneId,
        notches: i32,
        cell: (u16, u16),
        mods: u16,
    ) -> Result<(), Error> {
        if let Some(pane) = self.panes.get_mut(&pane) {
            pane.wheel(notches, cell, Mods::from_bits_retain(mods))?;
        }
        Ok(())
    }

    pub fn paste(&mut self, client: ClientId, pane: PaneId, text: &str) -> Result<(), Error> {
        self.take_size(client)?;
        if let Some(pane) = self.panes.get_mut(&pane) {
            pane.paste(text)?;
        }
        Ok(())
    }

    /// The client's window gained or lost focus over `pane`.
    pub fn focus(&mut self, pane: PaneId, focused: bool) -> Result<(), Error> {
        if let Some(pane) = self.panes.get_mut(&pane) {
            pane.focus(focused)?;
        }
        Ok(())
    }

    fn spawn_pane(&mut self, spec: PaneSpec, size: Size) -> Result<PaneId, Error> {
        let id = self.next_pane;
        let mut spawn = Spawn {
            command: spec.command,
            cwd: spec.cwd.or_else(|| self.spawn.cwd.clone()),
            env: self.spawn.env.clone(),
        };
        spawn.env.push(("JETMUX_PANE".into(), id.to_string()));
        let mut pane = Pane::spawn(&spawn, size)?;
        pane.configure(&self.settings)?;
        pane.resize(size, self.cell)?;
        self.next_pane += 1;
        self.panes.insert(id, pane);
        Ok(id)
    }
}

/// Painted between a restored pane's old output and its new shell.
const RESTORE_MARKER: &str = "\r\n\x1b[0;2m--- restored ---\x1b[0m\r\n";

/// `spec` with each pane leaf replaced by `pane(leaf)`, in leaf order.
fn map_leaves(spec: &LayoutSpec, pane: &mut impl FnMut(&PaneSpec) -> PaneSpec) -> LayoutSpec {
    match spec {
        LayoutSpec::Pane(p) => LayoutSpec::Pane(pane(p)),
        LayoutSpec::Split { axis, children } => LayoutSpec::Split {
            axis: *axis,
            children: children
                .iter()
                .map(|(weight, child)| (*weight, map_leaves(child, pane)))
                .collect(),
        },
    }
}

/// A command line a shell reads back as `words`.
fn shell_words(words: &[String]) -> String {
    words
        .iter()
        .map(|w| {
            if !w.is_empty()
                && w.chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_./=:,+@%".contains(c))
            {
                w.clone()
            } else {
                format!("'{}'", w.replace('\'', "'\\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The pane after (or before) `from` in layout order, wrapping.
fn cycle(panes: &[PaneId], from: Option<PaneId>, forward: bool) -> Option<PaneId> {
    let at = panes.iter().position(|p| Some(*p) == from)?;
    let n = panes.len();
    let next = if forward {
        (at + 1) % n
    } else {
        (at + n - 1) % n
    };
    (next != at).then(|| panes[next])
}

/// A pane title trimmed for a tab: shells' `user@host: ~/src/jetmux`
/// becomes `jetmux`, trailing `|key:value` tags (`|git:main|rc:1`) are
/// dropped, and any other title stays as it is.
fn short_title(title: &str) -> &str {
    let title = match title.split_once('|') {
        Some((head, tags))
            if tags.split('|').all(|tag| {
                tag.split_once(':')
                    .is_some_and(|(key, _)| !key.is_empty() && !key.contains(' '))
            }) =>
        {
            head
        }
        _ => title,
    };
    let path = match title.split_once(": ") {
        Some((who, path)) if who.contains('@') && !who.contains(' ') => path,
        _ => title,
    };
    if path.starts_with(['/', '~']) && !path.contains(' ') {
        match path.trim_end_matches('/').rsplit('/').next() {
            Some("") | None => path,
            Some(last) => last,
        }
    } else if path.len() < title.len() {
        path
    } else {
        title
    }
}

#[cfg(test)]
mod tests {
    use jetmux_proto::rpc::{PaneRef, TabRef};
    use jetmux_proto::{Action, Axis, Direction, LayoutSpec, PaneSpec, Size, TabSpec};

    use crate::Spawn;
    use crate::session::{Session, short_title};

    #[test]
    fn tab_names_trim_shell_titles() {
        assert_eq!(short_title("me@box: ~/src/jetmux"), "jetmux");
        assert_eq!(short_title("me@box: ~"), "~");
        assert_eq!(short_title("me@box: /"), "/");
        assert_eq!(short_title("me@box: vim notes.md"), "vim notes.md");
        assert_eq!(short_title("nvim: main.rs"), "nvim: main.rs");
        assert_eq!(short_title("/usr/local/bin/htop"), "htop");
        assert_eq!(short_title("me@box: ~/src/jetmux|git:main|rc:1"), "jetmux");
        assert_eq!(short_title("me@box: ~|venv:x"), "~");
        assert_eq!(short_title("a | b"), "a | b");
    }

    fn cat() -> Spawn {
        Spawn {
            command: vec!["cat".into()],
            ..Spawn::default()
        }
    }

    fn session() -> Session {
        Session::new("t".into(), cat(), Size { cols: 81, rows: 25 }, &[]).unwrap()
    }

    fn split() -> Action {
        Action::Split(Direction::Right, PaneSpec::default())
    }

    #[test]
    fn colors_follow_the_client_that_typed_last() {
        use jetmux_proto::input::{Rgb, Theme};
        let mut s = session();
        let size = Size { cols: 81, rows: 25 };
        s.add_client(1, size, false).unwrap();
        s.add_client(2, size, false).unwrap();
        let theme = |r| Theme {
            background: Rgb { r, g: 0, b: 0 },
            ..Theme::default()
        };
        s.set_theme(1, &theme(1)).unwrap();
        assert_eq!(s.settings.background.r, 1, "the only theme takes over");
        s.set_theme(2, &theme(2)).unwrap();
        s.paste(1, 0, "x").unwrap();
        assert_eq!(s.settings.background.r, 1, "whoever acts last");
        s.paste(2, 0, "y").unwrap();
        assert_eq!(s.settings.background.r, 2);
        s.remove_client(2);
        s.paste(1, 0, "z").unwrap();
        assert_eq!(s.settings.background.r, 1);
        // The config's colors hold while no client sent any.
        s.remove_client(1);
        s.set_base(jet_core::VtSettings {
            background: jet_core::Rgb { r: 7, g: 0, b: 0 },
            ..jet_core::VtSettings::default()
        })
        .unwrap();
        assert_eq!(s.settings.background.r, 7);
    }

    #[test]
    fn panes_report_state_for_late_clients() {
        use jetmux_proto::pane::{PaneEvent, Progress};
        let mut s = session();
        let pane = s.pane_mut(0).unwrap();
        // Without OSC 7, where procfs says, once per move. Linux only: with
        // no procfs, only the shell's own report counts.
        if cfg!(target_os = "linux") {
            let polled = pane.poll_location();
            assert!(
                matches!(&polled, Some(PaneEvent::Location(Some(l))) if l.host.is_some() && l.path.starts_with('/')),
                "{polled:?}"
            );
            assert_eq!(pane.poll_location(), None);
            assert!(pane.state_reports().contains(&polled.unwrap()));
        }
        pane.feed(b"\x1b]7;file://box/tmp\x07\x1b]9;4;1;42\x07\x1b]1337;SetUserVar=k=dg==\x07")
            .unwrap();
        let location = PaneEvent::Location(Some(jet_core::Location {
            host: Some("box".into()),
            path: "/tmp".into(),
        }));
        let reports = pane.take_reports();
        assert!(reports.contains(&location), "{reports:?}");
        assert!(reports.contains(&PaneEvent::Progress(Progress::Normal(42))));
        assert!(reports.contains(&PaneEvent::Var {
            name: "k".into(),
            value: "v".into()
        }));
        assert!(pane.take_reports().is_empty());
        // The shell's own report wins from then on.
        assert_eq!(pane.poll_location(), None);
        let state = pane.state_reports();
        assert!(
            state.contains(&location) && state.contains(&PaneEvent::Progress(Progress::Normal(42)))
        );
        // A client attaching later still gets the shell's variables.
        assert!(state.contains(&PaneEvent::Var {
            name: "k".into(),
            value: "v".into()
        }));
    }

    #[test]
    fn clients_have_their_own_tab_and_focus() {
        let mut s = session();
        s.add_client(1, Size { cols: 81, rows: 25 }, false).unwrap();
        s.add_client(2, Size { cols: 81, rows: 25 }, false).unwrap();

        s.action(1, split()).unwrap();
        let v1 = s.view(1).unwrap();
        assert_eq!(v1.panes.len(), 2);
        assert!(v1.panes[1].focused);
        // Client 2 sees the split but keeps its own focus.
        let v2 = s.view(2).unwrap();
        assert_eq!(v2.panes.len(), 2);
        assert!(v2.panes[0].focused);

        s.action(2, Action::NewTab(PaneSpec::default())).unwrap();
        assert_eq!(s.view(2).unwrap().active_tab, 1);
        assert_eq!(s.view(1).unwrap().active_tab, 0);

        s.action(1, Action::ToggleZoom).unwrap();
        assert_eq!(s.view(1).unwrap().panes.len(), 1);
        assert_eq!(s.view(2).unwrap().tabs.len(), 2);
    }

    #[test]
    fn closing_the_last_pane_of_a_tab_moves_viewers() {
        let mut s = session();
        s.add_client(1, Size { cols: 81, rows: 25 }, false).unwrap();
        s.action(1, Action::NewTab(PaneSpec::default())).unwrap();
        s.action(1, Action::ClosePane).unwrap();
        let v = s.view(1).unwrap();
        assert_eq!(v.tabs.len(), 1);
        assert_eq!(v.active_tab, 0);
        s.action(1, Action::ClosePane).unwrap();
        assert!(s.is_empty());
    }

    #[test]
    fn panes_follow_the_size_owner() {
        let mut s = session();
        s.add_client(1, Size { cols: 81, rows: 25 }, false).unwrap();
        s.action(1, split()).unwrap();
        let pane = s.view(1).unwrap().panes[0].id;
        assert_eq!(
            s.pane_mut(pane).unwrap().size().unwrap(),
            Size { cols: 40, rows: 24 }
        );
        s.add_client(2, Size { cols: 41, rows: 11 }, false).unwrap();
        assert_eq!(
            s.pane_mut(pane).unwrap().size().unwrap(),
            Size { cols: 20, rows: 10 }
        );
        s.input(1, b"").unwrap();
        assert_eq!(
            s.pane_mut(pane).unwrap().size().unwrap(),
            Size { cols: 40, rows: 24 }
        );
    }

    /// A phone following a desktop session sees the desktop's layout and
    /// can type without resizing anything, until it asks to fit.
    #[test]
    fn followers_never_resize_until_they_fit() {
        let mut s = session();
        let desktop = Size { cols: 81, rows: 25 };
        s.add_client(1, desktop, false).unwrap();
        let pane = s.view(1).unwrap().panes[0].id;
        s.add_client(2, Size { cols: 41, rows: 11 }, true).unwrap();
        s.input(2, b"").unwrap();
        s.resize_client(2, Size { cols: 31, rows: 9 }).unwrap();
        assert_eq!(s.view(2).unwrap().panes[0].rect.cols, 81);
        assert_eq!(s.pane_mut(pane).unwrap().size().unwrap().cols, 81);
        s.set_follow(2, false).unwrap();
        assert_eq!(s.pane_mut(pane).unwrap().size().unwrap().cols, 31);
        // The desktop typing takes the size back, and a follower tracks it.
        s.set_follow(2, true).unwrap();
        s.input(1, b"").unwrap();
        assert_eq!(s.view(2).unwrap().panes[0].rect.cols, 81);
    }

    #[test]
    fn sessions_build_from_specs_and_save_back() {
        let pane = |cmd: &str| {
            LayoutSpec::Pane(PaneSpec {
                command: vec![cmd.into()],
                cwd: None,
            })
        };
        let tabs = [
            TabSpec {
                name: Some("edit".into()),
                layout: pane("cat"),
            },
            TabSpec {
                name: None,
                layout: LayoutSpec::Split {
                    axis: Axis::Columns,
                    children: vec![(1, pane("cat")), (1, pane("cat"))],
                },
            },
        ];
        let mut s = Session::new("t".into(), cat(), Size { cols: 81, rows: 25 }, &tabs).unwrap();
        assert_eq!(s.tab_count(), 2);
        let right = s
            .resolve(Some(&TabRef::Index(2)), Some(PaneRef::Index(2)))
            .unwrap();
        assert_eq!(
            s.pane_mut(right).unwrap().size().unwrap(),
            Size { cols: 40, rows: 24 }
        );
        assert_eq!(
            s.resolve(Some(&TabRef::Name("edit".into())), None).unwrap(),
            0
        );

        let saved = s.layout_spec();
        assert_eq!(saved[0].name.as_deref(), Some("edit"));
        assert!(
            matches!(&saved[1].layout, LayoutSpec::Split { children, .. } if children.len() == 2)
        );
    }

    #[test]
    fn scripted_actions_move_every_client() {
        let mut s = session();
        s.add_client(1, Size { cols: 81, rows: 25 }, false).unwrap();
        s.add_client(2, Size { cols: 81, rows: 25 }, false).unwrap();
        let first = s.focused(1).unwrap();
        let acted = s.act_on(first, split()).unwrap();
        let new = acted.focus.unwrap();
        assert_ne!(new, first);
        assert_eq!(s.focused(1), Some(new));
        assert_eq!(s.focused(2), Some(new));

        // Resizing doesn't steal focus.
        s.act_on(first, Action::Resize(Direction::Right, 5))
            .unwrap();
        assert_eq!(s.focused(2), Some(new));

        // Nobody attached: the next client starts where scripts left it.
        s.remove_client(1);
        s.remove_client(2);
        s.act_on(first, Action::FocusPane(first)).unwrap();
        s.add_client(3, Size { cols: 81, rows: 25 }, false).unwrap();
        assert_eq!(s.focused(3), Some(first));
    }

    /// A snapshot brings a session back two ways: restored after a reboot
    /// with fresh processes, or resumed by a new daemon with the old ones.
    #[test]
    fn snapshots_restore_and_resume() {
        let pane = |cmd: &str| {
            LayoutSpec::Pane(PaneSpec {
                command: vec![cmd.into()],
                cwd: Some("/".into()),
            })
        };
        let tabs = [TabSpec {
            name: Some("work".into()),
            layout: LayoutSpec::Split {
                axis: Axis::Columns,
                children: vec![(1, pane("cat")), (1, pane("sh"))],
            },
        }];
        let size = Size { cols: 81, rows: 25 };
        let mut s = Session::new("t".into(), cat(), size, &tabs).unwrap();
        s.pane_mut(0)
            .unwrap()
            .feed(b"\x1b[1mhello\x1b[0m\r\n")
            .unwrap();
        let saved = s.snapshot().unwrap();
        assert_eq!(saved.tabs[0].name.as_deref(), Some("work"));
        let history = saved.tabs[0].panes[0].history.as_deref().unwrap();
        assert!(history.contains("hello"), "{history:?}");

        // cat may run again; sh may not, so it comes back typed at a shell.
        let restored = Session::restore(&saved, cat(), |cmd| cmd[0] == "cat").unwrap();
        let text = restored.pane(0).unwrap().capture(true).unwrap();
        assert!(text.starts_with("hello\n--- restored ---"), "{text:?}");
        assert_eq!(restored.pane(0).unwrap().command(), ["cat"]);
        assert!(restored.pane(1).unwrap().command().is_empty());
        assert_eq!(restored.tab_list()[0].0, "work");

        let (frozen, masters) = s.freeze().unwrap();
        let mut masters: std::collections::HashMap<i32, std::os::fd::OwnedFd> = masters
            .into_iter()
            .map(|fd| (std::os::fd::AsRawFd::as_raw_fd(&fd), fd))
            .collect();
        let resumed = Session::resume(&frozen, cat(), |p, command| {
            let fd = masters.remove(&p.live.as_ref().unwrap().fd).unwrap();
            crate::Pane::adopt(fd, p, command)
        })
        .unwrap();
        let text = resumed.pane(0).unwrap().capture(true).unwrap();
        assert!(text.starts_with("hello"), "{text:?}");
        assert_eq!(resumed.pane(1).unwrap().command(), ["sh"]);
        assert_eq!(resumed.tab_list()[0].1, [0, 1]);
    }
}
