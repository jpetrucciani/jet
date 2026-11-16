#!/usr/bin/env bash
# Builds the scene in docs/screenshots: run as the shell of a jet window
# started with docs/screenshots/config-*.toml. It lays out tabs and splits
# with jet-cli, prints sample output, and leaves each pane at a neutral
# prompt, so no real user, host or directory shows.
#
#   JET_DEMO       this script's path
#   JET_DEMO_BASH  the bash to run panes with (a Windows path on Windows)
#   JET_CLI        jet-cli
set -u
bash_=${JET_DEMO_BASH:-bash}
cli=${JET_CLI:-jet-cli}

e=$'\e'
osc() { printf '%s]%s\a' "$e" "$1"; }
c() { printf '%s[%sm' "$e" "$1"; }
prompt() { printf '%s~/src/jet%s %s❯%s %s\n' "$(c '38;5;80')" "$(c 0)" "$(c '38;5;212')" "$(c 0)" "$1"; }
var() { osc "1337;SetUserVar=$1=$(printf '%s' "$2" | base64)"; }

# Hand over to an interactive shell with the same prompt, marked for
# OSC 133 so jump_to_prompt works in the screenshot session too.
shell() {
  export PS1='\[\e]133;A\a\e[38;5;80m\]~/src/jet\[\e[0m\] \[\e[38;5;212m\]❯\[\e[0m\] '
  export PROMPT_COMMAND=
  exec "$bash_" --norc --noprofile -i
}

case ${1:-main} in
main)
  osc "0;jet"
  var k8s "prod-east"
  var branch "main"
  # Another tab, then this one again, then the splits.
  "$cli" spawn -- "$bash_" "$JET_DEMO" notes > /dev/null
  "$cli" activate --pane "$JET_PANE"
  right=$("$cli" split --direction right --ratio 0.42 -- "$bash_" "$JET_DEMO" colors)
  "$cli" split --pane "$right" --direction down --ratio 0.45 -- "$bash_" "$JET_DEMO" tests > /dev/null
  "$cli" activate --pane "$JET_PANE"
  osc "133;A"
  prompt "cargo build --release"
  for crate in libghostty-vt-sys libghostty-vt jet-core jet-font jet-render jet-config jet-cli jet; do
    printf '   %sCompiling%s %s v0.1.0\n' "$(c '1;32')" "$(c 0)" "$crate"
  done
  printf '    %sFinished%s `release` profile [optimized] target(s) in 41.20s\n' "$(c '1;32')" "$(c 0)"
  osc "133;A"
  prompt "git log --oneline --graph -6"
  while read -r sha msg; do
    printf '%s*%s %s%s%s %s\n' "$(c 31)" "$(c 0)" "$(c 33)" "$sha" "$(c 0)" "$msg"
  done <<'LOG'
3f9c2a1 osc 8 hyperlinks and prompt jumps
b81e0d4 synchronized output
9d2c7f3 osc 52 clipboard writes
5ac9e10 ime and url clicks
e47b2a8 macos port
0c3d9f5 status segments in the title bar
LOG
  osc "133;A"
  prompt "ls"
  printf '%sCargo.toml%s  %scorpus%s/  %scrates%s/  %sdefault.nix%s  %sdocs%s/  %sSPEC.md%s\n' \
    "" "" "$(c '1;34')" "$(c 0)" "$(c '1;34')" "$(c 0)" "" "" "$(c '1;34')" "$(c 0)" "" ""
  shell
  ;;
colors)
  osc "0;colors"
  printf '%s16 colors%s\n' "$(c 1)" "$(c 0)"
  for row in 0 8; do
    for i in 0 1 2 3 4 5 6 7; do printf '%s    ' "$(c "48;5;$((row + i))")"; done
    printf '%s\n' "$(c 0)"
  done
  printf '\n%s256 colors%s\n' "$(c 1)" "$(c 0)"
  for row in 0 1 2 3 4 5; do
    for col in $(seq 0 35); do printf '%s ' "$(c "48;5;$((16 + row * 36 + col))")"; done
    printf '%s\n' "$(c 0)"
  done
  for i in $(seq 232 255); do printf '%s  ' "$(c "48;5;$i")"; done
  printf '%s\n\n' "$(c 0)"
  printf '%sbold%s %sitalic%s %sfaint%s %sstrike%s %sinverse%s\n' \
    "$(c 1)" "$(c 0)" "$(c 3)" "$(c 0)" "$(c 2)" "$(c 0)" "$(c 9)" "$(c 0)" "$(c 7)" "$(c 0)"
  printf '%sunderline%s %sdouble%s %scurly%s %sdotted%s %sdashed%s\n' \
    "$(c 4)" "$(c 0)" "$(c '4:2')" "$(c 0)" "$(c '4:3;58;5;204')" "$(c 0)" "$(c '4:4')" "$(c 0)" "$(c '4:5')" "$(c 0)"
  printf 'ligatures: => -> != <= >= === |> ::\n'
  printf 'emoji and wide: 🚀 ✨ 日本語 한국어\n'
  shell
  ;;
tests)
  osc "0;tests"
  osc "133;A"
  prompt "cargo test -p jet-core"
  printf 'running 33 tests\n'
  for t in vt::snapshot_shares_unchanged_rows vt::jumps_between_shell_prompts \
    vt::queries_get_jets_answers pane::sync_holds_frames snapshot::urls_follow_wraps \
    snapshot::preedit_overlays; do
    printf 'test %s ... %sok%s\n' "$t" "$(c 32)" "$(c 0)"
  done
  printf '\ntest result: %sok%s. 33 passed; 0 failed\n' "$(c 32)" "$(c 0)"
  shell
  ;;
notes)
  osc "0;notes"
  shell
  ;;
esac
