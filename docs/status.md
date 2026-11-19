# Status bar

Powerline-style segments about the focused pane: who and where you are, what's running, and anything your shell or scripts want to show. Each segment is a template; jet fills it in from the focused pane and redraws when that changes.

## Setting it up

In `config.toml` (`~/.jet/config.toml`, else `%APPDATA%\jet\config.toml` on Windows or `$XDG_CONFIG_HOME/jet/config.toml` on Linux); it hot-reloads, so save and look.

```toml
[window]
decorations = "custom" # needed for position = "titlebar"

[status]
position = "titlebar" # titlebar | tab-bar | top | bottom
style = "powerline" # powerline | round | plain

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

| `position`      | Where                                                                                                                                                 |
| --------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------- |
| `titlebar`      | Right of the tabs, beside minimize/maximize/close. Needs `decorations = "custom"`; otherwise it falls back to `bottom`                                |
| `tab-bar`       | The end of the tab bar: its right end, or the foot of a side (left/right) tab bar with one segment per row. Keeps the tab bar shown even with one tab |
| `top`, `bottom` | A row of its own above or below the panes. `align = "left"` on a segment puts it at the left end; segments default to the right                       |

`style`: `powerline` (arrows), `round` (pills), `plain` (blocks). jet draws the separators itself, so no Nerd Font is needed.

## Fields

| Field              | Value                                                                                                                                                |
| ------------------ | ---------------------------------------------------------------------------------------------------------------------------------------------------- |
| `{title}`          | The title the shell or program set (OSC 0/2)                                                                                                         |
| `{cwd}`            | Where the shell is, as it reported it (OSC 7 or OSC 9;9), remote paths included; else the directory of the program in front (not while that's `ssh`) |
| `{dir}`            | The last part of `{cwd}`                                                                                                                             |
| `{host}`           | The host from the innermost OSC 3008 context or the shell's OSC 7, else the `ssh` destination, else this computer                                    |
| `{user}`           | Who the innermost OSC 3008 context runs as, else the `ssh` login user, else you                                                                      |
| `{context}`        | What the innermost OSC 3008 context changed: `root` under `run0`/`sudo`, the container or VM name, the remote host; empty otherwise                  |
| `{ssh}`            | `user@host` of an `ssh` running in the pane; empty otherwise                                                                                         |
| `{process}`        | The program in front (`ssh`, `nvim`, `cargo`), without `.exe`                                                                                        |
| `{profile}`        | The pane's profile name; `default` for the plain shell                                                                                               |
| `{tab}`, `{tabs}`  | The tab's number and how many there are                                                                                                              |
| `{time}`, `{date}` | Local `HH:MM` and `YYYY-MM-DD`                                                                                                                       |
| `{progress}`       | `42%` (or `…`) while a program reports progress (OSC 9;4: winget, cargo, PowerShell)                                                                 |
| `{var.NAME}`       | A user variable your shell or a script set; see below                                                                                                |

- **Program in front:** `{process}`, `{ssh}`, and the fallbacks for `{cwd}`, `{host}` and `{user}` come from looking at the focused pane's newest process once a second. That's how `ssh me@box` shows up with nothing installed on the box. On Windows that's the newest process under the shell; on Linux, the terminal's foreground job.
- **Values are per pane:** each split keeps its own title, directory and variables, and the status follows the focus.

## Sending your own info: user variables

Anything you can compute in a prompt hook can be a segment. The shell sets a **user variable** with an escape sequence, and `{var.NAME}` shows it:

```
ESC ] 1337 ; SetUserVar=NAME=BASE64(VALUE) BEL
```

It's the convention WezTerm and iTerm2 use, so integrations written for them work unchanged. An empty value unsets the variable, which hides its segments. Variables pass through `ssh`, so a remote shell can set them too, and so can `jet-cli set-var NAME VALUE` from a script.

A pane keeps its variables until they're set again, so send every one on every prompt, empty ones included. That's what clears the values a remote shell set once `ssh` exits.

Also send OSC 7 with the host. Then `{host}`, `{cwd}` and `{dir}` are right on remote boxes, and new tabs open in the local shell's directory.

### bash

[`jet-status.bash`](jet-status.bash) sends all of this before each prompt:

| Variable | Value                                                                                    |
| -------- | ---------------------------------------------------------------------------------------- |
| `git`    | The branch, or the short commit when detached                                            |
| `gitop`  | An operation in progress: `rebase 2/5`, `merge`, `cherry-pick`, `revert`, `bisect`, `am` |
| `k8s`    | The kubeconfig's current context, with `:namespace` unless it's `default`                |
| `nix`    | The `nix-shell` or `nix develop` shell's name                                            |
| `venv`   | The Python virtualenv (its project, for a `.venv`)                                       |
| `jobs`   | How many background jobs, when there are any                                             |
| `took`   | How long the last command ran, when 2 seconds or more                                    |
| `rc`     | The last command's exit status when it failed (`130 INT`)                                |

It also sends OSC 7, and inside tmux it wraps everything so it passes through (needs `set -g allow-passthrough on`).

It never starts a process, since it runs before every prompt: git's `HEAD` and the kubeconfig are read by bash itself, and base64 is done in bash too. That's about 0.2 ms a prompt, where calling `kubectl config current-context` alone takes 50. It needs bash 4.4 or newer, and does nothing on older ones (macOS's `/bin/bash`).

Copy it next to your `~/.bashrc`, on the machines you ssh into too, and hook it in:

```bash
source ~/.jet-status.bash
PROMPT_COMMAND="__jet_status${PROMPT_COMMAND:+;$PROMPT_COMMAND}"
```

It goes first so it sees the last command's `$?`. With starship, use its hook instead, after `eval "$(starship init bash)"`; the timing then comes from starship's own timer:

```bash
starship_precmd_user_func=__jet_status
```

The values also stay in the `JET_STATUS` array, for anything else in your prompt to use.

Then show the ones you want:

```toml
[[status.segments]]
text = " {var.git}"

[[status.segments]]
text = "{var.gitop}"
background = "#d0679d"

[[status.segments]]
text = "⎈ {var.k8s}"

[[status.segments]]
text = "❄ {var.nix}"

[[status.segments]]
text = "🐍 {var.venv}"

[[status.segments]]
text = "⚙ {var.jobs}"

[[status.segments]]
text = "⏱ {var.took}"

[[status.segments]]
text = "✘ {var.rc}" # only after a failed command
background = "#d0679d"
```

### PowerShell

PowerShell's prompt runs in the same process, so a status costs little here, and the same rule applies: no `git` or `kubectl`.

```powershell
function prompt {
  $ok = $?
  $code = $global:LASTEXITCODE
  $vars = [ordered]@{ git = ''; venv = ''; took = ''; rc = '' }

  # the branch, from .git/HEAD in this directory or above
  for ($d = $PWD.ProviderPath; $d; $d = Split-Path $d) {
    $git = Join-Path $d .git
    if (Test-Path $git -PathType Leaf) { # a worktree: "gitdir: path"
      $git = (Get-Content $git -TotalCount 1) -replace '^gitdir: '
      if (-not [IO.Path]::IsPathRooted($git)) { $git = Join-Path $d $git }
    }
    if (Test-Path (Join-Path $git HEAD)) {
      $head = Get-Content (Join-Path $git HEAD) -TotalCount 1
      $vars.git = if ($head -like 'ref: refs/heads/*') { $head.Substring(16) } else { $head.Substring(0, 7) }
      break
    }
  }
  if ($env:VIRTUAL_ENV) { $vars.venv = Split-Path $env:VIRTUAL_ENV -Leaf }
  $last = Get-History -Count 1
  if ($last) {
    $s = [int]($last.EndExecutionTime - $last.StartExecutionTime).TotalSeconds
    if ($s -ge 3600) { $vars.took = '{0}h{1}m' -f [int][Math]::Floor($s / 3600), [int][Math]::Floor($s % 3600 / 60) }
    elseif ($s -ge 60) { $vars.took = '{0}m{1}s' -f [int][Math]::Floor($s / 60), ($s % 60) }
    elseif ($s -ge 2) { $vars.took = "${s}s" }
  }
  if (-not $ok) { $vars.rc = if ($code) { "$code" } else { 'failed' } }

  # where we are (PowerShell's cd doesn't move the process), then each
  # variable, empty ones too so they clear; one write for all of it
  $e = [char]27
  $out = "$e]9;9;$($PWD.ProviderPath)$e\"
  foreach ($k in $vars.Keys) {
    $b64 = [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($vars[$k]))
    $out += "$e]1337;SetUserVar=$k=$b64$([char]7)"
  }
  [Console]::Write($out)
  "PS $PWD> "
}
```

With a prompt module (oh-my-posh, starship), put everything above the last line in a function and call it first thing in your prompt instead.

### From scripts

```sh
jet-cli set-var deploy "staging 3/5"     # this pane ($JET_PANE)
jet-cli set-var --pane 4 deploy ""       # unset on pane 4
```
