#![cfg(unix)]

use std::sync::mpsc;
use std::time::{Duration, Instant};

use jet_core::{Pane, PaneEvent, PaneOptions, VtSettings};
use jet_pty::{Command, Size};

const TIMEOUT: Duration = Duration::from_secs(10);

/// Waits on pane wakeups until `done` holds for the latest screen text and
/// the events seen so far.
fn wait_for(
    pane: &Pane,
    wakes: &mpsc::Receiver<()>,
    text: &mut String,
    events: &mut Vec<PaneEvent>,
    done: impl Fn(&str, &[PaneEvent]) -> bool,
) -> bool {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        if let Some(snapshot) = pane.take_snapshot() {
            *text = snapshot.text();
        }
        events.extend(std::iter::from_fn(|| pane.try_event()));
        if done(text, events) {
            return true;
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() || wakes.recv_timeout(left).is_err() {
            return false;
        }
    }
}

#[test]
fn bash_round_trips_through_the_pty() -> Result<(), jet_core::Error> {
    let command = Command::new("bash").arg("-c").arg(
        r#"printf '\033]2;jet-test\007ready\n'; read -r line; printf 'got:%s\n' "$line"; exit 3"#,
    );
    let (wake_tx, wakes) = mpsc::channel();
    let opts = PaneOptions {
        command,
        size: Size {
            cols: 40,
            rows: 10,
            cell_width: 8,
            cell_height: 16,
        },
        max_scrollback: 100,
        settings: VtSettings::default(),
    };
    let pane = Pane::spawn(opts, move || {
        let _ = wake_tx.send(());
    })?;

    let (mut text, mut events) = (String::new(), Vec::new());
    assert!(
        wait_for(&pane, &wakes, &mut text, &mut events, |t, _| t
            .contains("ready")),
        "never saw ready; screen:\n{text}"
    );

    pane.input("ping\r");
    let exited = |t: &str, e: &[PaneEvent]| {
        t.contains("got:ping") && e.contains(&PaneEvent::Exited(Some(3)))
    };
    assert!(
        wait_for(&pane, &wakes, &mut text, &mut events, exited),
        "no round trip; screen:\n{text}\nevents: {events:?}"
    );
    assert!(events.contains(&PaneEvent::Title("jet-test".into())));
    Ok(())
}

/// A flood larger than the reader's in-flight window comes through whole and
/// the pane still sees the child exit.
#[test]
fn flood_completes() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::temp_dir().join(format!("jet-flood-{}.txt", std::process::id()));
    let mut flood = String::new();
    for i in 0..20_000u32 {
        flood.push_str(&format!("word{i} "));
        if i % 8 == 7 {
            flood.push('\n');
        }
    }
    flood.push_str("\nflood-end\n");
    std::fs::write(&path, flood)?;

    let command = Command::new("cat").arg(&path);
    let (wake_tx, wakes) = mpsc::channel();
    let opts = PaneOptions {
        command,
        size: Size {
            cols: 100,
            rows: 30,
            cell_width: 8,
            cell_height: 16,
        },
        max_scrollback: 1000,
        settings: VtSettings::default(),
    };
    let pane = Pane::spawn(opts, move || {
        let _ = wake_tx.send(());
    })?;
    let (mut text, mut events) = (String::new(), Vec::new());
    let done = wait_for(&pane, &wakes, &mut text, &mut events, |t, e| {
        t.contains("flood-end") && e.contains(&PaneEvent::Exited(Some(0)))
    });
    let _ = std::fs::remove_file(&path);
    assert!(done, "flood stalled; screen:\n{text}\nevents: {events:?}");
    Ok(())
}

/// Mouse positions map to cells using the cell size the pane was spawned
/// with, before any resize: a double click on the top row picks its word.
#[test]
fn clicks_use_the_spawn_cell_size() -> Result<(), jet_core::Error> {
    use jet_core::{Mods, MouseAction, MouseButton, MouseInput};

    let command = Command::new("bash")
        .arg("-c")
        .arg("printf 'top\\n\\nbottom'; sleep 10");
    let (wake_tx, wakes) = mpsc::channel();
    let opts = PaneOptions {
        command,
        size: Size {
            cols: 20,
            rows: 5,
            cell_width: 10,
            cell_height: 20,
        },
        max_scrollback: 0,
        settings: VtSettings::default(),
    };
    let pane = Pane::spawn(opts, move || {
        let _ = wake_tx.send(());
    })?;
    let (mut text, mut events) = (String::new(), Vec::new());
    assert!(
        wait_for(&pane, &wakes, &mut text, &mut events, |t, _| t
            .contains("bottom")),
        "never saw output; screen:\n{text}"
    );
    // Row 0, column 1, in pixels.
    let click = |action| MouseInput {
        action,
        x: 15.0,
        y: 10.0,
        mods: Mods::empty(),
    };
    for _ in 0..2 {
        pane.mouse(click(MouseAction::Press(MouseButton::Left)));
        pane.mouse(click(MouseAction::Release(MouseButton::Left)));
    }
    pane.copy();
    let copied = |_: &str, e: &[PaneEvent]| e.iter().any(|e| matches!(e, PaneEvent::Copy(_)));
    assert!(
        wait_for(&pane, &wakes, &mut text, &mut events, copied),
        "nothing copied; events: {events:?}"
    );
    assert!(
        events.contains(&PaneEvent::Copy("top".into())),
        "{events:?}"
    );
    Ok(())
}
