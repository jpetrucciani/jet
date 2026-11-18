# jet: zsh found this through ZDOTDIR. Put the user's ZDOTDIR back, run
# their .zshenv, and in interactive shells load jet's integration.

if [[ -n "${JET_ZSH_ZDOTDIR+x}" ]]; then
    builtin export ZDOTDIR="$JET_ZSH_ZDOTDIR"
    builtin unset JET_ZSH_ZDOTDIR
else
    builtin unset ZDOTDIR
fi

{
    builtin typeset _jet_file="${ZDOTDIR-$HOME}/.zshenv"
    [[ ! -r "$_jet_file" ]] || builtin source -- "$_jet_file"
} always {
    if [[ -o interactive ]]; then
        builtin source -- "${${(%):-%x}:A:h}/jet.zsh"
    fi
    builtin unset _jet_file
}
