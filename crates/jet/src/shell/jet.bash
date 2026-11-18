# jet shell integration for bash: prompt marks (OSC 133) and the working
# directory (OSC 7). jet starts bash in POSIX mode with ENV pointing here,
# so this runs first: it leaves POSIX mode, runs the startup files bash
# would have, then hooks the prompt.

if [[ -n ${JET_BASH_INJECT+x} ]]; then
  builtin set +o posix
  # POSIX mode picked ~/.sh_history; bash's own is ~/.bash_history. The
  # history loads after startup files, so theirs can still change it.
  if [[ ${HISTFILE-} == "$HOME/.sh_history" ]]; then
    HISTFILE="$HOME/.bash_history"
  fi
  _jet_inject="$JET_BASH_INJECT"
  builtin unset JET_BASH_INJECT ENV
  if [[ -n ${JET_BASH_ENV+x} ]]; then
    builtin export ENV="$JET_BASH_ENV"
    builtin unset JET_BASH_ENV
  fi
  if [[ $_jet_inject == login ]]; then
    [[ -r /etc/profile ]] && builtin source /etc/profile
    for _jet_file in ~/.bash_profile ~/.bash_login ~/.profile; do
      if [[ -r $_jet_file ]]; then
        builtin source "$_jet_file"
        break
      fi
    done
  else
    [[ -r /etc/bash.bashrc ]] && builtin source /etc/bash.bashrc
    [[ -r ~/.bashrc ]] && builtin source ~/.bashrc
  fi
  builtin unset _jet_inject _jet_file
fi

if [[ $- == *i* && -z ${_jet_hooked-} ]]; then
  _jet_hooked=1
  _jet_ran=
  _jet_status=0

  # First in PROMPT_COMMAND: the last command's end, and where we are.
  _jet_precmd() {
    _jet_status=$?
    if [[ -n $_jet_ran ]]; then
      builtin printf '\e]133;D;%s\a' "$_jet_status"
    fi
    _jet_ran=
    builtin printf '\e]7;file://%s%s\a' "${HOSTNAME-}" "$PWD"
    return "$_jet_status"
  }

  # Last, after prompt tools (starship and co.) have set PS1: mark it.
  _jet_prompt_marks() {
    if [[ $PS1 != *'133;A'* ]]; then
      PS1='\[\e]133;A\a\]'"$PS1"'\[\e]133;B\a\]'
    fi
    return "$_jet_status"
  }

  if ((BASH_VERSINFO[0] > 5 || (BASH_VERSINFO[0] == 5 && BASH_VERSINFO[1] >= 1))) &&
    [[ ${PROMPT_COMMAND@a} == *a* ]]; then
    PROMPT_COMMAND=(_jet_precmd "${PROMPT_COMMAND[@]}" _jet_prompt_marks)
  else
    # Newlines, not `;`: the user's may end in one.
    PROMPT_COMMAND="_jet_precmd"$'\n'"${PROMPT_COMMAND:+$PROMPT_COMMAND$'\n'}_jet_prompt_marks"
  fi

  # PS0 shows as a command starts (bash 4.4 and later); the subscript
  # notes that one ran, so an empty Enter doesn't report a finish.
  if ((BASH_VERSINFO[0] > 4 || (BASH_VERSINFO[0] == 4 && BASH_VERSINFO[1] >= 4))); then
    PS0='${_jet_none[_jet_ran=1]}\e]133;C\a'"${PS0-}"
  fi
fi
