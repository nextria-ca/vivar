# Activate this environment with `. .venv\Scripts\Activate.ps1` (or bin/Activate.ps1).
function global:deactivate([switch]$NonDestructive) {
    if (Test-Path variable:global:_OLD_VIRTUAL_PATH) {
        $env:PATH = $global:_OLD_VIRTUAL_PATH
        Remove-Variable _OLD_VIRTUAL_PATH -Scope global
    }
    if (Test-Path function:global:_old_virtual_prompt) {
        Set-Item function:global:prompt -Value $function:global:_old_virtual_prompt
        Remove-Item function:global:_old_virtual_prompt
    }
    if (Test-Path env:VIRTUAL_ENV) { Remove-Item env:VIRTUAL_ENV }
    if (Test-Path env:VIRTUAL_ENV_PROMPT) { Remove-Item env:VIRTUAL_ENV_PROMPT }
    if (-not $NonDestructive) { Remove-Item function:deactivate }
}
deactivate -NonDestructive

$env:VIRTUAL_ENV = "__VENV_DIR__"
$global:_OLD_VIRTUAL_PATH = $env:PATH
$env:PATH = "$env:VIRTUAL_ENV" + [IO.Path]::DirectorySeparatorChar + "__VENV_BIN__" + [IO.Path]::PathSeparator + $env:PATH
if (-not $env:VIRTUAL_ENV_DISABLE_PROMPT) {
    function global:_old_virtual_prompt { "" }
    $function:global:_old_virtual_prompt = $function:prompt
    function global:prompt { "(__VENV_PROMPT__) " + (& $function:global:_old_virtual_prompt) }
    $env:VIRTUAL_ENV_PROMPT = "(__VENV_PROMPT__) "
}
