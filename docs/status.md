# Status bar

Powerline-style segments about the focused pane: who and where you are, what's running, and anything your shell or scripts want to show. Each segment is a template; jet fills it in from the focused pane and redraws when that changes.

## Setting it up

In `config.toml` (`~/.jet/config.toml`, else `%APPDATA%\jet\config.toml` on Windows or `$XDG_CONFIG_HOME/jet/config.toml` on Linux); it hot-reloads, so save and look.

```toml
[window]
decorations = "custom"      # needed for position = "titlebar"

[status]
position = "titlebar"       # titlebar | tab-bar | top | bottom
style = "powerline"         # powerline | round | plain

[[status.segments]]
text = "{user}@{host}"
background = "#5de4c7"
foreground = "#1b1e28"

[[status.segments]]
text = " {var.git}"

[[status.segments]]
text = "⎈ {var.k8s}"
background = "#d0679d"
foreground = "#1b1e28"

[[status.segments]]
text = "{dir}"

[[status.segments]]
text = "{time}"
```

- **Order:** segments show in the order written.
- **Hiding:** a segment shows only when every field in it has a value. `⎈ {var.k8s}` disappears when no k8s context is set; `{ssh}` shows only while you're connected somewhere.
- **Colors:** `foreground` and `background` take `#rrggbb`. Unset ones alternate two shades of the bar, so a plain list of segments already looks right.
- **Braces:** `{{` and `}}` for literal ones. A misspelled field is a config error that names it, shown in the window like any other config error.

### Where it goes

| `position` | Where |
| --- | --- |
| `titlebar` | Right of the tabs, beside minimize/maximize/close. Needs `decorations = "custom"`; otherwise it falls back to `bottom` |
| `tab-bar` | The end of the tab bar: its right end, or the foot of a side (left/right) tab bar with one segment per row. Keeps the tab bar shown even with one tab |
| `top`, `bottom` | A row of its own above or below the panes. `align = "left"` on a segment puts it at the left end; segments default to the right |

`style`: `powerline` (arrows), `round` (pills), `plain` (blocks). jet draws the separators itself, so no Nerd Font is needed.

## Fields

| Field | Value |
| --- | --- |
| `{title}` | The title the shell or program set (OSC 0/2) |
| `{cwd}` | Where the shell is, as it reported it (OSC 7 or OSC 9;9), remote paths included; else the directory of the program in front (not while that's `ssh`) |
| `{dir}` | The last part of `{cwd}` |
| `{host}` | The host the shell reported in OSC 7, else the `ssh` destination, else this computer |
| `{user}` | The `ssh` login user, else you |
| `{ssh}` | `user@host` of an `ssh` running in the pane; empty otherwise |
| `{process}` | The program in front (`ssh`, `nvim`, `cargo`), without `.exe` |
| `{profile}` | The pane's profile name; `default` for the plain shell |
| `{tab}`, `{tabs}` | The tab's number and how many there are |
| `{time}`, `{date}` | Local `HH:MM` and `YYYY-MM-DD` |
| `{progress}` | `42%` (or `…`) while a program reports progress (OSC 9;4: winget, cargo, PowerShell) |
| `{var.NAME}` | A user variable your shell or a script set; see below |

- **Program in front:** `{process}`, `{ssh}`, and the fallbacks for `{cwd}`, `{host}` and `{user}` come from looking at the focused pane's newest process once a second. That's how `ssh me@box` shows up with nothing installed on the box. On Windows that's the newest process under the shell; on Linux, the terminal's foreground job.
- **Values are per pane:** each split keeps its own title, directory and variables, and the status follows the focus.

## Sending your own info: user variables

Anything you can compute in a prompt hook can be a segment. The shell sets a **user variable** with an escape sequence, and `{var.NAME}` shows it:

```
ESC ] 1337 ; SetUserVar=NAME=BASE64(VALUE) BEL
```

It's the convention WezTerm and iTerm2 use, so integrations written for them work unchanged. An empty value unsets the variable, which hides its segments. Variables pass through `ssh`, so a remote shell can set them too, and so can `jet-cli set-var NAME VALUE` from a script.

Also send OSC 7 with the host. Then `{host}`, `{cwd}` and `{dir}` are right on remote boxes, and new tabs open in the local shell's directory.

### bash

The `__wezterm_title` function in `~/cfg/hosts/constants.nix` packs everything into the title with `|` separators. With jet, send each piece as its own variable instead. The title stays free for the program's own, and each piece gets its own segment and color:

```bash
__jet_var() { printf '\e]1337;SetUserVar=%s=%s\a' "$1" "$(printf %s "$2" | base64 | tr -d '\n')"; }

__jet_status() {
  local last_rc="$1" git_branch="" venv="" k8s=""
  if [ -f .git/HEAD ]; then
    git_branch=$(sed 's|ref: refs/heads/||' .git/HEAD 2>/dev/null)
  elif [ -d .git ]; then
    git_branch=$(git rev-parse --short HEAD 2>/dev/null)
  fi
  [ -n "$VIRTUAL_ENV" ] && venv=$(basename "$VIRTUAL_ENV")
  if [ -n "$KUBECONFIG" ] && command -v kubectl &>/dev/null; then
    k8s=$(kubectl config current-context 2>/dev/null)
  fi
  [ "$last_rc" = "0" ] && last_rc=""

  # where we are, host included: {host}, {cwd}, {dir}
  printf '\e]7;file://%s%s\a' "${HOSTNAME%%.*}" "$PWD"
  __jet_var git "$git_branch"
  __jet_var venv "$venv"
  __jet_var k8s "$k8s"
  __jet_var rc "$last_rc"
}
```

Hook it the same way as `__wezterm_title`: through `starship_precmd_user_func`, or at the front of `PROMPT_COMMAND` so `$?` is intact (call it as `__jet_status $?`). Both functions can run side by side while you still use WezTerm.

Then use the variables as segments:

```toml
[[status.segments]]
text = " {var.git}"

[[status.segments]]
text = "🐍 {var.venv}"

[[status.segments]]
text = "⎈ {var.k8s}"

[[status.segments]]
text = "✘ {var.rc}"          # only after a failed command
background = "#d0679d"
```

### PowerShell

```powershell
function Set-JetVar($name, $value) {
  $b64 = [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes([string]$value))
  Write-Host -NoNewline "$([char]27)]1337;SetUserVar=$name=$b64$([char]7)"
}

function prompt {
  $ok = $?
  # PowerShell's cd doesn't move the process, so report where we are
  Write-Host -NoNewline "$([char]27)]9;9;$($PWD.ProviderPath)$([char]7)"
  Set-JetVar rc $(if ($ok) { "" } else { "failed" })
  "PS $PWD> "
}
```

### From scripts

```sh
jet-cli set-var deploy "staging 3/5"     # this pane ($JET_PANE)
jet-cli set-var --pane 4 deploy ""       # unset on pane 4
```
