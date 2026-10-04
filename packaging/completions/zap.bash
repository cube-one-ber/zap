_zap() {
    local current=${COMP_WORDS[COMP_CWORD]}
    local commands='search info install build get upgrade updates list foreign remove orphans autoremove clean help version'
    local options='--dry-run --devel --recursive --nosave --help --version'
    if (( COMP_CWORD == 1 )); then
        mapfile -t COMPREPLY < <(compgen -W "$commands $options" -- "$current")
    elif [[ $current == -* ]]; then
        mapfile -t COMPREPLY < <(compgen -W "$options" -- "$current")
    else
        COMPREPLY=()
    fi
}
complete -F _zap zap
