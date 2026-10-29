# Activate this environment with `source bin/activate.fish`.
function deactivate -d "Exit the virtual environment"
    if test -n "$_OLD_VIRTUAL_PATH"
        set -gx PATH $_OLD_VIRTUAL_PATH
        set -e _OLD_VIRTUAL_PATH
    end
    if test -n "$_OLD_VIRTUAL_PYTHONHOME"
        set -gx PYTHONHOME $_OLD_VIRTUAL_PYTHONHOME
        set -e _OLD_VIRTUAL_PYTHONHOME
    end
    if functions -q _old_fish_prompt
        functions -e fish_prompt
        functions -c _old_fish_prompt fish_prompt
        functions -e _old_fish_prompt
    end
    set -e VIRTUAL_ENV
    set -e VIRTUAL_ENV_PROMPT
    if test "$argv[1]" != "nondestructive"
        functions -e deactivate
    end
end
deactivate nondestructive

set -gx VIRTUAL_ENV '__VENV_DIR__'
set -gx _OLD_VIRTUAL_PATH $PATH
set -gx PATH "$VIRTUAL_ENV/__VENV_BIN__" $PATH
if set -q PYTHONHOME
    set -gx _OLD_VIRTUAL_PYTHONHOME $PYTHONHOME
    set -e PYTHONHOME
end
if test -z "$VIRTUAL_ENV_DISABLE_PROMPT"
    functions -c fish_prompt _old_fish_prompt
    function fish_prompt
        printf '(%s) ' '__VENV_PROMPT__'
        _old_fish_prompt
    end
    set -gx VIRTUAL_ENV_PROMPT '(__VENV_PROMPT__) '
end
