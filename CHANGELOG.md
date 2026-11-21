# Changelog

What changed in each release of jet and jetmux, newest first. Link to a version by its heading, dots dropped: `CHANGELOG.md#011` is [0.1.1](#011). A release's GitHub notes start with its section here.

## 0.1.7

- Bell and notification sounds: `[terminal] bell-sound` plays a wav, mp3, ogg or flac file for the bell, `notification-sound` one for desktop notifications (OSC 9, OSC 777), at `sound-volume`. A profile can have its own of each, so an agent that rings for you (Claude Code's terminal bell, Codex's notifications) sounds like itself. A file that's missing or won't play is named in the window, and the bell falls back to the system alert. On Linux jet plays through ALSA, loaded when the first sound plays, so it still starts without it.

## 0.1.6

- Windows: several quake windows. Each `[quake.NAME]` is a window of its own, with its own place, profile and hotkeys, and `[quake]` under it; windows sharing a hotkey show and hide together, so one key can drop a terminal on each side of the screen. `jet quake` is now a small process holding the hotkeys and the one tray icon, which lists the windows, and starting a process for each.
- Windows: `[quake] hotkey-hook = true` catches the quake hotkey with a keyboard hook as well, so it works in games that turn hotkeys off while they're in front, such as League of Legends.
- Windows: the quake window no longer flashes and hides when it comes forward a moment after the hotkey (in a game, with `hotkey-hook`).

## 0.1.5

- macOS: jet no longer hangs on opening when no `jet quake` is running. Starting the quake window for `[quake] autostart`, it asked every running jet whether it was the quake window, itself included, and waited forever for its own answer.

## 0.1.4

- The palette, find bar and tab-name prompt edit anywhere: arrows, Home/End, Delete, and Ctrl (Alt on macOS) for a word at a time.
- The palette finds the version by typing `version`; Enter copies it.
- Status variables a remote shell set clear when `ssh` exits, even if the local shell doesn't reset them.
- jet attached to a jetmux session gets the panes' status variables right away, not at the next prompt.

## 0.1.3

- jetmux releases for aarch64 Linux (static, like x86_64's) and Apple silicon macOS. Intel Macs aren't supported.
- jetmux on macOS: the daemon answers requests. It failed every one, since macOS gives accepted sockets the listener's non-blocking mode.

## 0.1.2

- `jet --version | head -1`, `jet-cli list | head` and `jetmux ls | head` end quietly instead of panicking when the reader closes the pipe.
- `jetmux-web`, `jetmux-notify`, `jetmux-mcp` and `jetmux-rec` take `--version`.
- [docs/jet-status.bash](https://github.com/jpetrucciani/jet/blob/main/docs/jet-status.bash): a bash prompt hook that sends git branch and rebase/merge state, k8s context and namespace, nix shell, virtualenv, background jobs, command time and exit status as status variables, without starting a process. See [docs/status.md](https://github.com/jpetrucciani/jet/blob/main/docs/status.md#bash).

## 0.1.1

### Themes

- Follow the system: `[theme] light` and `dark` switch live when Windows or macOS changes mode (Ghostty's `theme = light:X,dark:Y` works too).
- New built-in themes: Grapebox, Dank Neon (and Light), Nu Disco Dark and Light.

### Profiles, hosts and tabs

- A profile can have its own theme, colors and tab color, shown as the whole tab, an edge line, a dot or the title (`color-style`).
- `[hosts]` rules give panes a look by host: ssh into `prod-*` and the pane turns red.
- Color any tab from the palette ("Set tab color"), a keybind (`set_tab_color`), or `jet-cli set-color`.

### Quake

- Fixed the window flashing and hiding on the first hotkey press.
- Drag it by the tab bar or size it from an edge; it stays there for the session ("Quake: reset position" puts it back).
- macOS: `autostart` adds a login item, and with `taskbar = true` clicking the Dock icon toggles it.
- Windows: upgrading with the MSI restarts quake instead of leaving it closed.

### Also

- "Open jet here" in Explorer's right-click menu (MSI).
- macOS: the system title bar is no longer see-through.

## 0.1.0

The first release.

### jet

- A native terminal for Windows, macOS and Linux on libghostty-vt: Ghostty's emulation (scrollback, reflow, the Kitty keyboard and graphics protocols, mouse reporting) with jet's own wgpu renderer (DirectX 12, Metal, Vulkan), ligatures, color emoji and fallback fonts.
- Ghostty's ~900 themes built in, previewed live from the palette.
- Tabs, splits and profiles (named shells, or `ssh` straight into a box), zoom, resize, and a command palette for every action.
- Asks before closing something running, and before pasting lines a prompt would run.
- Search (text or regex), copy mode with vi keys, and quick select for URLs, paths and hashes.
- Shell integration for bash, zsh and PowerShell without touching dotfiles: directory reports, prompt marks, jump to prompt, copy the last output.
- Status segments in the title bar, tab bar or a row (`{user}@{host}`, `{dir}`, git or k8s context from a script), following ssh and sudo.
- Plumbing: Ctrl+click text a regex matches to open it, run a program, or open a split on the box the pane is ssh'd into.
- Follow ssh: a new tab from a pane running `ssh` (or `wsl.exe`) lands on the same host in the same directory.
- A quake window on a global hotkey, started at sign-in on Windows.
- Session restore, desktop notifications, taskbar progress, clipboard writes (OSC 52, never reads) and hyperlinks.
- jet's own title bar with the tabs in it, opacity, and on Windows Mica/Acrylic, corners and border colors.
- `jet-cli` to drive running windows from scripts.
- An existing Ghostty config, its themes and `keybind` lines work as they are.
- Windows: an MSI (per user, no admin), Scoop and a zip, with a newer ConPTY bundled. macOS: `jet.app`. Linux: a tarball.

### jetmux

- A tmux-style daemon that owns the shells, so sessions outlive any client; jet shows its sessions as native tabs and splits.
- `jetmux attach` for any other terminal, with a prefix key, status bar, copy mode and palette.
- `jetmux upgrade` replaces the daemon without losing a process; `snapshot`/`restore` bring sessions back after a reboot.
- Profiles (named layouts) that `jetmux up NAME` creates or attaches to.
- `jetmux remote`: a mosh-style UDP link that survives sleep and network changes, with predictive echo.
- Scripting (`send-keys`, `capture`, `wait`, JSON events, a socket API), plus `jetmux-web`, `jetmux-notify`, `jetmux-mcp` and `jetmux-rec`.
- Static binaries for any x86_64 Linux.
