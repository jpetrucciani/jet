# jet shell integration for zsh: prompt marks (OSC 133) and the working
# directory (OSC 7). The hooks go in at the first prompt, so they run
# after any a theme added in .zshrc.

builtin autoload -Uz add-zsh-hook
typeset -g _jet_ran=

_jet_precmd() {
    builtin local ret=$?
    if [[ -n "$_jet_ran" ]]; then
        builtin print -n "\e]133;D;$ret\a"
    fi
    _jet_ran=
    builtin print -n "\e]133;A\a\e]7;file://${HOST}${PWD}\a"
    if [[ "$PS1" != *'133;B'* ]]; then
        PS1="$PS1%{"$'\e]133;B\a'"%}"
    fi
}

_jet_preexec() {
    _jet_ran=1
    builtin print -n "\e]133;C\a"
}

_jet_install() {
    add-zsh-hook -d precmd _jet_install
    add-zsh-hook precmd _jet_precmd
    add-zsh-hook preexec _jet_preexec
    _jet_precmd
}

add-zsh-hook precmd _jet_install
