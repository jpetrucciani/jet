//! jetmux sessions in a jet window: each of a session's tabs is a jet tab,
//! laid out by the daemon and drawn with jet's own splits.
//!
//! jet decides which tab and pane are in front, and tells the daemon; it
//! follows the daemon's choice only after asking for something new (a tab,
//! a split) or when its own choice is gone.

use std::collections::HashMap;

use jet_cli::What;

use jet_config::keybinds::Direction;
use jetmux_proto::layout::Layout;
use jetmux_proto::{Action as Wire, Axis, ClientMsg, PaneSpec, Size, View};

use crate::app::{App, Host, Tab, Wake};
use crate::layout::{Divider, Node, PaneId, Rect};
use crate::pane::Pane;
use crate::remote::{Remote, SessionEvent};

/// Which session's tab, and which tab there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RemoteTab {
    session: u64,
    id: u32,
}

/// A session shown here.
pub struct Attached {
    remote: Remote,
    /// The jet profile it came from: opening that again adds a tab.
    profile: String,
    /// The daemon's panes as jet's.
    panes: HashMap<u32, PaneId>,
    view: Option<View>,
    /// Take the daemon's tab and focus from the next view: jet asked for
    /// something that moves them.
    follow: bool,
    /// What jet last told the daemon, so each goes once.
    sent_tab: Option<u32>,
    sent_focus: Option<u32>,
    sent_size: Option<Size>,
}

impl<H: Host> App<H> {
    /// Show jet profile `name`'s session, or add a tab to it when it's
    /// already showing.
    pub(super) fn open_jetmux(&mut self, name: &str, spec: &jet_config::Jetmux) {
        if let Some(attached) = self.remotes.values_mut().find(|a| a.profile == name) {
            attached.remote.act(Wire::NewTab(PaneSpec::default()));
            attached.follow = true;
            return;
        }
        let wake = self.host.waker(Wake::Panes);
        match Remote::connect(&spec.command(), &spec.session, move || wake()) {
            Ok(remote) => {
                let key = self.next_remote;
                self.next_remote += 1;
                self.remotes.insert(
                    key,
                    Attached {
                        remote,
                        profile: name.to_owned(),
                        panes: HashMap::new(),
                        view: None,
                        follow: true,
                        sent_tab: None,
                        sent_focus: None,
                        sent_size: None,
                    },
                );
                self.sync_remotes();
            }
            Err(err) => self.remote_problem(format!("jetmux session {}: {err}", spec.session)),
        }
    }

    fn remote_problem(&mut self, problem: String) {
        eprintln!("jet: {problem}");
        self.remote_problem = Some(problem);
        self.banner_dismissed = false;
        self.refresh_banner();
        self.render();
    }

    /// Everything the sessions said since last time.
    pub(super) fn pump_remotes(&mut self) {
        let keys: Vec<u64> = self.remotes.keys().copied().collect();
        for key in keys {
            while let Some(event) = self.remotes.get(&key).and_then(|a| a.remote.try_event()) {
                match event {
                    SessionEvent::View(view) => self.on_view(key, view),
                    SessionEvent::Clipboard { program: true, .. }
                        if !self.settings.config.terminal.clipboard_write => {}
                    SessionEvent::Clipboard { text, .. } => {
                        if let Err(err) = self.host.set_clipboard(&text) {
                            eprintln!("jet: copying to the clipboard: {err}");
                        }
                    }
                    SessionEvent::Bell => self.bell(),
                    SessionEvent::Message(text) => eprintln!("jet: jetmux: {text}"),
                    SessionEvent::Ended(why) => {
                        self.end_remote(key, why);
                        break;
                    }
                }
            }
        }
    }

    fn end_remote(&mut self, key: u64, why: Option<String>) {
        let Some(attached) = self.remotes.remove(&key) else {
            return;
        };
        for id in attached.panes.values() {
            self.emit(What::PaneClosed, Some(*id));
            self.panes.remove(id);
        }
        while let Some(index) = self.tabs.iter().position(|t| in_session(t, key)) {
            self.remove_tab(index);
        }
        if let Some(why) = why {
            self.remote_problem(format!("jetmux session: {why}"));
        }
    }

    /// Make the session's tabs and panes match `view`.
    fn on_view(&mut self, key: u64, view: View) {
        let Some(attached) = self.remotes.get_mut(&key) else {
            return;
        };
        let live: Vec<u32> = view.tabs.iter().flat_map(|t| t.layout.panes()).collect();
        let gone: Vec<PaneId> = attached
            .panes
            .iter()
            .filter(|(id, _)| !live.contains(id))
            .map(|(_, pane)| *pane)
            .collect();
        attached.panes.retain(|id, _| live.contains(id));
        let new: Vec<(u32, Pane)> = live
            .iter()
            .filter(|id| !attached.panes.contains_key(id))
            .map(|&id| (id, Pane::remote(attached.remote.link(id))))
            .collect();
        let profile = attached.profile.clone();
        let follow = std::mem::take(&mut attached.follow);
        let old_focus = self.focused_id();

        for id in gone {
            self.emit(What::PaneClosed, Some(id));
            self.panes.remove(&id);
        }
        let grid = self.current_grid();
        let m = self.renderer.cell_metrics();
        let settings = self.settings.config.profile_vt_settings(Some(&profile));
        let mut adopted = HashMap::new();
        for (id, pane) in new {
            pane.configure(settings.clone());
            pane.resize(jet_pty::Size {
                cols: grid.0,
                rows: grid.1,
                cell_width: u16::try_from(m.width).unwrap_or(u16::MAX),
                cell_height: u16::try_from(m.height).unwrap_or(u16::MAX),
            });
            let jet = self.adopt(pane, grid, Some(&profile));
            adopted.insert(id, jet);
            self.emit(What::PaneOpened, Some(jet));
        }
        let Some(attached) = self.remotes.get_mut(&key) else {
            return;
        };
        attached.panes.extend(adopted);
        let panes = attached.panes.clone();

        // Tabs: kept, made, then dropped.
        let mut last = self.tabs.iter().rposition(|t| in_session(t, key));
        let daemon_tab = view.tabs.get(usize::from(view.active_tab)).map(|t| t.id);
        for shown in &view.tabs {
            let (Some(root), Some(&focused)) =
                (to_node(&shown.layout, &panes), panes.get(&shown.focused))
            else {
                continue;
            };
            let remote = RemoteTab {
                session: key,
                id: shown.id,
            };
            let index = match self.tabs.iter().position(|t| t.remote == Some(remote)) {
                Some(index) => index,
                None => {
                    let mut tab = Tab::new(self.take_tab_id(), focused);
                    tab.remote = Some(remote);
                    let at = last.map_or(self.tabs.len(), |i| i + 1);
                    self.tabs.insert(at, tab);
                    if at <= self.active && self.active != usize::MAX {
                        self.active += 1;
                    }
                    at
                }
            };
            last = Some(index);
            let tab = &mut self.tabs[index];
            tab.root = root;
            tab.title = Some(shown.name.clone());
            if follow || !tab.root.panes().contains(&tab.focused) {
                tab.focused = focused;
            }
            if daemon_tab == Some(shown.id) {
                tab.zoomed = view.zoomed;
            }
        }
        let ids: Vec<u32> = view.tabs.iter().map(|t| t.id).collect();
        while let Some(index) = self.tabs.iter().position(|t| {
            t.remote
                .is_some_and(|r| r.session == key && !ids.contains(&r.id))
        }) {
            self.remove_tab(index);
        }

        if let Some(attached) = self.remotes.get_mut(&key) {
            if follow {
                attached.sent_tab = daemon_tab;
                attached.sent_focus = view
                    .tabs
                    .iter()
                    .find(|t| Some(t.id) == daemon_tab)
                    .map(|t| t.focused);
            }
            attached.view = Some(view);
        }
        if follow
            && let Some(id) = daemon_tab
            && let Some(index) = self
                .tabs
                .iter()
                .position(|t| t.remote == Some(RemoteTab { session: key, id }))
        {
            self.activate_tab(index);
        } else if self.focused_id() != old_focus {
            self.after_focus_change();
        }
        self.relayout(false);
        self.render();
    }

    /// Tell the daemons what jet shows: the active tab and focused pane of
    /// a session, and every session the window's size.
    pub(super) fn sync_remotes(&mut self) {
        if self.remotes.is_empty() {
            return;
        }
        let area = self.chrome().panes;
        let (cols, rows) = self
            .renderer
            .grid_size(area.w.max(1) as u32, area.h.max(1) as u32);
        // The daemon keeps a row of that for its status bar, which jet
        // doesn't draw.
        let size = Size {
            cols,
            rows: rows.saturating_add(1),
        };
        let front = self.tab().and_then(|t| Some((t.remote?, t.focused)));
        for (key, attached) in &mut self.remotes {
            if attached.sent_size != Some(size) {
                attached.remote.send(ClientMsg::Resize(size));
                attached.sent_size = Some(size);
            }
            let Some((tab, focused)) = front.filter(|(t, _)| t.session == *key) else {
                continue;
            };
            let Some(view) = &attached.view else {
                continue;
            };
            if attached.sent_tab != Some(tab.id)
                && let Some(index) = view.tabs.iter().position(|t| t.id == tab.id)
            {
                attached
                    .remote
                    .act(Wire::SelectTab(u16::try_from(index).unwrap_or(u16::MAX)));
                attached.sent_tab = Some(tab.id);
            }
            let pane = attached
                .panes
                .iter()
                .find(|(_, jet)| **jet == focused)
                .map(|(id, _)| *id);
            if let Some(pane) = pane
                && attached.sent_focus != Some(pane)
            {
                attached.remote.act(Wire::FocusPane(pane));
                attached.sent_focus = Some(pane);
            }
        }
    }

    /// The active tab's session, when it's a jetmux tab.
    fn front_session(&mut self) -> Option<&mut Attached> {
        let remote = self.tab()?.remote?;
        self.remotes.get_mut(&remote.session)
    }

    /// Take the active tab's session out of jet, still running there.
    pub(super) fn detach(&mut self) {
        if let Some(key) = self.tab().and_then(|t| t.remote).map(|r| r.session) {
            // Dropping its connection tells the daemon.
            self.end_remote(key, None);
        }
    }

    /// Send `action` when the active tab is a jetmux tab; true when it was.
    /// The daemon then picks the tab and pane in front.
    pub(super) fn remote_act(&mut self, action: Wire) -> bool {
        self.sync_remotes();
        let Some(attached) = self.front_session() else {
            return false;
        };
        let moves = matches!(
            action,
            Wire::Split(..) | Wire::ClosePane | Wire::CloseTab | Wire::NewTab(_)
        );
        attached.remote.act(action);
        if moves {
            attached.follow = true;
        }
        true
    }

    /// Ask the daemon to close jetmux pane `id`; false for jet's own.
    pub(super) fn close_remote_pane(&mut self, id: PaneId) -> bool {
        for attached in self.remotes.values_mut() {
            if let Some((&pane, _)) = attached.panes.iter().find(|(_, jet)| **jet == id) {
                attached.remote.act(Wire::FocusPane(pane));
                attached.remote.act(Wire::ClosePane);
                attached.sent_focus = None;
                attached.follow = true;
                return true;
            }
        }
        false
    }

    /// Ask the daemon to close the tab at `index`; false for jet's own.
    pub(super) fn close_remote_tab(&mut self, index: usize) -> bool {
        let Some(tab) = self.tabs.get(index) else {
            return false;
        };
        let (Some(remote), focused) = (tab.remote, tab.focused) else {
            return false;
        };
        let Some(attached) = self.remotes.get_mut(&remote.session) else {
            return true;
        };
        if let Some((&pane, _)) = attached.panes.iter().find(|(_, jet)| **jet == focused) {
            attached.remote.act(Wire::FocusPane(pane));
            attached.remote.act(Wire::CloseTab);
            attached.sent_tab = None;
            attached.sent_focus = None;
        }
        true
    }

    /// The daemon's current view of `remote`'s session, if that's the tab
    /// it shows this client.
    fn shown_view(&self, remote: RemoteTab) -> Option<(&Attached, &View)> {
        let attached = self.remotes.get(&remote.session)?;
        let view = attached.view.as_ref()?;
        let shown = view.tabs.get(usize::from(view.active_tab))?;
        (shown.id == remote.id).then_some((attached, view))
    }

    /// `cells` of the daemon's in client pixels within `area`, padding
    /// included as in jet's own layout.
    fn cells_to_px(&self, area: Rect, cells: jetmux_proto::Rect) -> Rect {
        let m = self.renderer.cell_metrics();
        let pad = self.renderer.padding() as i32;
        let (cw, ch) = (m.width as i32, m.height as i32);
        Rect {
            x: area.x + i32::from(cells.x) * cw,
            y: area.y + i32::from(cells.y) * ch,
            w: i32::from(cells.cols) * cw + 2 * pad,
            h: i32::from(cells.rows) * ch + 2 * pad,
        }
    }

    /// Where the daemon put `remote`'s panes, when it's the tab shown.
    pub(super) fn remote_rects(&self, remote: RemoteTab, area: Rect) -> Vec<(PaneId, Rect)> {
        let Some((attached, view)) = self.shown_view(remote) else {
            return Vec::new();
        };
        view.panes
            .iter()
            .filter_map(|p| Some((*attached.panes.get(&p.id)?, self.cells_to_px(area, p.rect))))
            .collect()
    }

    /// `remote`'s borders as dividers, in the order the daemon counts them.
    pub(super) fn remote_dividers(&self, remote: RemoteTab, area: Rect) -> Vec<Divider> {
        let Some((_, view)) = self.shown_view(remote) else {
            return Vec::new();
        };
        let Some(shown) = view.tabs.get(usize::from(view.active_tab)) else {
            return Vec::new();
        };
        let m = self.renderer.cell_metrics();
        let pad = self.renderer.padding() as i32;
        let (cw, ch) = (m.width as i32, m.height as i32);
        let gap = self.gap();
        // A border cell's content area; a split's area takes its padding.
        let inner = |cells: jetmux_proto::Rect| {
            let px = self.cells_to_px(area, cells);
            Rect {
                x: px.x + pad,
                y: px.y + pad,
                w: px.w - 2 * pad,
                h: px.h - 2 * pad,
            }
        };
        shown
            .layout
            .borders(view.area)
            .into_iter()
            .map(|(border, axis, split)| {
                let cell = inner(border);
                let horizontal = axis == Axis::Columns;
                let rect = if horizontal {
                    Rect {
                        x: cell.x + (cw - gap) / 2,
                        w: gap,
                        ..cell
                    }
                } else {
                    Rect {
                        y: cell.y + (ch - gap) / 2,
                        h: gap,
                        ..cell
                    }
                };
                Divider {
                    rect,
                    horizontal,
                    area: self.cells_to_px(area, split),
                }
            })
            .collect()
    }

    /// Splits move by cells in jetmux: `percent` of the tab's size.
    pub(super) fn remote_resize(&mut self, dir: Direction, percent: u8) -> bool {
        let span = match self.tab().and_then(|t| self.shown_view(t.remote?)) {
            Some((_, view)) => match dir {
                Direction::Left | Direction::Right => view.area.cols,
                Direction::Up | Direction::Down => view.area.rows,
            },
            None => return false,
        };
        let cells = (u32::from(span) * u32::from(percent) / 100).max(1);
        self.remote_act(Wire::Resize(
            wire_direction(dir),
            u16::try_from(cells).unwrap_or(u16::MAX),
        ))
    }
}

pub(super) fn wire_direction(dir: Direction) -> jetmux_proto::Direction {
    match dir {
        Direction::Left => jetmux_proto::Direction::Left,
        Direction::Right => jetmux_proto::Direction::Right,
        Direction::Up => jetmux_proto::Direction::Up,
        Direction::Down => jetmux_proto::Direction::Down,
    }
}

fn in_session(tab: &Tab, key: u64) -> bool {
    tab.remote.is_some_and(|r| r.session == key)
}

/// The daemon's split tree with jet's pane ids.
fn to_node(layout: &Layout, panes: &HashMap<u32, PaneId>) -> Option<Node> {
    Some(match layout {
        Layout::Pane(id) => Node::Leaf(*panes.get(id)?),
        Layout::Split {
            axis,
            ratio,
            first,
            second,
        } => Node::Split {
            horizontal: *axis == Axis::Columns,
            ratio: *ratio,
            first: Box::new(to_node(first, panes)?),
            second: Box::new(to_node(second, panes)?),
        },
    })
}
