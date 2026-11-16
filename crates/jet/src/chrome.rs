//! Where the window's own parts go: jet's title bar, the tab bar on any
//! side, and the panes in what's left. Plain data and integer pixels, so it
//! tests on any platform.

use jet_config::TabBarPosition;

use crate::layout::Rect;

/// The window buttons, left to right.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Button {
    Minimize,
    Maximize,
    Close,
}

pub const BUTTONS: [Button; 3] = [Button::Minimize, Button::Maximize, Button::Close];

/// Where the status segments go, resolved: `titlebar` without a title
/// bar of jet's is `Bottom`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatusAt {
    Caption,
    TabBar,
    Top,
    Bottom,
}

/// What decides the layout.
#[derive(Clone, Copy, Debug)]
pub struct Input {
    pub width: i32,
    pub height: i32,
    /// jet draws the title bar.
    pub caption: bool,
    pub tabs_shown: bool,
    pub position: TabBarPosition,
    /// Height of a bar row (the title bar, a top or bottom tab bar).
    pub row: i32,
    /// Width of a side tab bar.
    pub side: i32,
    /// Width of each of jet's window buttons; 0 where the system draws its
    /// own over the title bar (macOS).
    pub button: i32,
    /// Room kept free at the title bar's start for the system's buttons.
    pub left: i32,
    /// Empty title bar kept free of tabs, to drag the window by.
    pub gap: i32,
    pub status: Option<StatusAt>,
    /// The status's width in the title bar or a top or bottom tab bar; its
    /// height at the bottom of a side one.
    pub status_size: i32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Chrome {
    /// jet's title bar; empty when Windows draws it (or nobody does).
    pub caption: Rect,
    /// Minimize, maximize, close, inside the caption.
    pub buttons: [Rect; 3],
    /// The caption between the system's buttons and the status or jet's
    /// buttons, for the title.
    pub title: Rect,
    /// The tab bar; inside the caption when both are on top.
    pub tabs: Rect,
    /// The status segments; empty without any.
    pub status: Rect,
    pub panes: Rect,
}

impl Chrome {
    pub fn new(i: Input) -> Self {
        let mut free = Rect {
            x: 0,
            y: 0,
            w: i.width,
            h: i.height,
        };
        let mut chrome = Self::default();
        if i.caption {
            chrome.caption = take_top(&mut free, i.row);
            let right = i.width;
            for (n, rect) in chrome.buttons.iter_mut().enumerate() {
                *rect = Rect {
                    x: right - (3 - n as i32) * i.button,
                    w: i.button,
                    ..chrome.caption
                };
            }
        }
        // Beside the buttons, taking room from the tabs before the gap.
        let left = i.left.min(chrome.caption.w);
        let end = (chrome.caption.w - 3 * i.button).max(left);
        let in_caption = if i.status == Some(StatusAt::Caption) && i.caption {
            let w = i.status_size.min(end - left);
            chrome.status = Rect {
                x: end - w,
                w,
                ..chrome.caption
            };
            w
        } else {
            0
        };
        if i.caption {
            chrome.title = Rect {
                x: left,
                w: end - in_caption - left,
                ..chrome.caption
            };
        }
        if i.tabs_shown || i.status == Some(StatusAt::TabBar) {
            chrome.tabs = match i.position {
                TabBarPosition::Top if i.caption => Rect {
                    x: left,
                    w: (end - i.gap - in_caption - left).max(0),
                    ..chrome.caption
                },
                TabBarPosition::Top => take_top(&mut free, i.row),
                TabBarPosition::Bottom => take_bottom(&mut free, i.row),
                TabBarPosition::Left => {
                    let w = i.side.min(free.w);
                    let bar = Rect { w, ..free };
                    free.x += w;
                    free.w -= w;
                    bar
                }
                TabBarPosition::Right => {
                    let w = i.side.min(free.w);
                    free.w -= w;
                    Rect {
                        x: free.x + free.w,
                        w,
                        ..free
                    }
                }
            };
        }
        match i.status {
            Some(StatusAt::TabBar) if chrome.tabs.h > 0 => {
                let t = &mut chrome.tabs;
                chrome.status =
                    if matches!(i.position, TabBarPosition::Left | TabBarPosition::Right) {
                        let h = i.status_size.min(t.h);
                        t.h -= h;
                        Rect {
                            y: t.y + t.h,
                            h,
                            ..*t
                        }
                    } else {
                        let w = i.status_size.min(t.w);
                        t.w -= w;
                        Rect {
                            x: t.x + t.w,
                            w,
                            ..*t
                        }
                    };
            }
            Some(StatusAt::Top) => chrome.status = take_top(&mut free, i.row),
            Some(StatusAt::Bottom) => chrome.status = take_bottom(&mut free, i.row),
            Some(StatusAt::Caption | StatusAt::TabBar) | None => {}
        }
        chrome.panes = free;
        chrome
    }

    pub fn button_at(&self, x: i32, y: i32) -> Option<Button> {
        let n = self.buttons.iter().position(|r| r.contains(x, y))?;
        BUTTONS.get(n).copied()
    }
}

fn take_bottom(free: &mut Rect, h: i32) -> Rect {
    let h = h.min(free.h);
    free.h -= h;
    Rect {
        y: free.y + free.h,
        h,
        ..*free
    }
}

fn take_top(free: &mut Rect, h: i32) -> Rect {
    let h = h.min(free.h);
    let top = Rect { h, ..*free };
    free.y += h;
    free.h -= h;
    top
}

#[cfg(test)]
mod tests {
    use jet_config::TabBarPosition;

    use crate::chrome::{Button, Chrome, Input, StatusAt};
    use crate::layout::Rect;

    fn input(position: TabBarPosition, caption: bool) -> Input {
        Input {
            width: 1000,
            height: 600,
            caption,
            tabs_shown: true,
            position,
            row: 30,
            side: 200,
            button: 46,
            left: 0,
            gap: 40,
            status: None,
            status_size: 0,
        }
    }

    #[test]
    fn top_tabs_share_jets_title_bar() {
        let c = Chrome::new(input(TabBarPosition::Top, true));
        assert_eq!(c.caption.h, 30);
        assert_eq!(c.tabs.y, 0);
        assert_eq!(c.tabs.w, 1000 - 3 * 46 - 40);
        assert_eq!(c.buttons[2].x + c.buttons[2].w, 1000);
        assert_eq!(c.button_at(999, 5), Some(Button::Close));
        assert_eq!(c.button_at(999, 40), None);
        assert_eq!(
            c.panes,
            Rect {
                x: 0,
                y: 30,
                w: 1000,
                h: 570
            }
        );
    }

    #[test]
    fn system_buttons_keep_the_title_bars_start() {
        let mut i = input(TabBarPosition::Top, true);
        (i.button, i.left) = (0, 80);
        let c = Chrome::new(i);
        assert_eq!((c.tabs.x, c.tabs.w), (80, 1000 - 80 - 40));
        assert_eq!((c.title.x, c.title.w), (80, 920));
        assert_eq!(c.button_at(999, 5), None);
    }

    #[test]
    fn side_tabs_sit_below_the_title_bar() {
        let c = Chrome::new(input(TabBarPosition::Right, true));
        assert_eq!(
            c.tabs,
            Rect {
                x: 800,
                y: 30,
                w: 200,
                h: 570
            }
        );
        assert_eq!(c.panes.w, 800);
        let c = Chrome::new(input(TabBarPosition::Left, false));
        assert_eq!((c.tabs.x, c.tabs.h), (0, 600));
        assert_eq!((c.panes.x, c.caption.h), (200, 0));
    }

    #[test]
    fn bottom_tabs_and_windows_title_bar() {
        let c = Chrome::new(input(TabBarPosition::Bottom, false));
        assert_eq!((c.tabs.y, c.tabs.h), (570, 30));
        assert_eq!((c.panes.y, c.panes.h), (0, 570));
        assert_eq!(c.button_at(999, 5), None);
    }

    #[test]
    fn status_beside_the_buttons_or_in_the_bars() {
        let mut i = input(TabBarPosition::Top, true);
        (i.status, i.status_size) = (Some(StatusAt::Caption), 100);
        let c = Chrome::new(i);
        assert_eq!((c.status.x, c.status.w), (1000 - 3 * 46 - 100, 100));
        assert_eq!(c.tabs.w, 1000 - 3 * 46 - 40 - 100);
        // A side bar keeps it at its foot.
        let mut i = input(TabBarPosition::Left, false);
        (i.status, i.status_size) = (Some(StatusAt::TabBar), 60);
        let c = Chrome::new(i);
        assert_eq!((c.status.y, c.status.h, c.tabs.h), (540, 60, 540));
        // A row of its own, under a bottom tab bar's.
        let mut i = input(TabBarPosition::Bottom, false);
        i.status = Some(StatusAt::Bottom);
        let c = Chrome::new(i);
        assert_eq!((c.tabs.y, c.status.y, c.panes.h), (570, 540, 540));
        let mut i = input(TabBarPosition::Right, true);
        i.status = Some(StatusAt::Top);
        let c = Chrome::new(i);
        assert_eq!((c.status.y, c.status.w, c.panes.y), (30, 800, 60));
    }
}
