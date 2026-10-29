# Activate this environment with `source bin/activate.csh` (csh, tcsh).
alias deactivate 'test $?_OLD_VIRTUAL_PATH != 0 && setenv PATH "$_OLD_VIRTUAL_PATH" && unset _OLD_VIRTUAL_PATH; rehash; test $?_OLD_VIRTUAL_PROMPT != 0 && set prompt="$_OLD_VIRTUAL_PROMPT" && unset _OLD_VIRTUAL_PROMPT; unsetenv VIRTUAL_ENV; unsetenv VIRTUAL_ENV_PROMPT; test "\!:*" != "nondestructive" && unalias deactivate'
deactivate nondestructive

setenv VIRTUAL_ENV '__VENV_DIR__'
set _OLD_VIRTUAL_PATH="$PATH"
setenv PATH "$VIRTUAL_ENV/__VENV_BIN__:$PATH"
set _OLD_VIRTUAL_PROMPT="$prompt"
if (! "$?VIRTUAL_ENV_DISABLE_PROMPT") then
    set prompt = "(__VENV_PROMPT__) $prompt"
    setenv VIRTUAL_ENV_PROMPT "(__VENV_PROMPT__) "
endif
rehash
