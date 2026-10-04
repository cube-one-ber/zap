_zap() {
    local current=${COMP_WORDS[COMP_CWORD]} command=${COMP_WORDS[1]}
    local commands='search select info localinfo install install-files build build-local install-local get pkgbuild upgrade updates list foreign local-search files owns check reason remove orphans autoremove clean stats news help version'
    local options='--dry-run --devel --recursive --nosave --aur --repo --needed --asdeps --asexplicit --sandbox --no-sandbox --rebuildtree --cleanafter --quiet --searchby --sortby --help --version'
    if (( COMP_CWORD == 1 )); then
        mapfile -t COMPREPLY < <(compgen -W "$commands --help --version" -- "$current")
    elif [[ ${COMP_WORDS[COMP_CWORD-1]} == --sortby ]]; then
        mapfile -t COMPREPLY < <(compgen -W 'votes popularity name modified' -- "$current")
    elif [[ ${COMP_WORDS[COMP_CWORD-1]} == --searchby ]]; then
        mapfile -t COMPREPLY < <(compgen -W 'name name-desc maintainer provides depends makedepends checkdepends optdepends' -- "$current")
    elif [[ $current == -* ]]; then
        mapfile -t COMPREPLY < <(compgen -W "$options" -- "$current")
    else
        case $command in
            remove|-R|-Rns|reason|-D|localinfo|-Qi|files|-Ql|check|-Qk|list|-Q|foreign|-Qm)
                mapfile -t COMPREPLY < <(compgen -W "$(zap list --quiet 2>/dev/null)" -- "$current") ;;
            build-local|install-local|-B|-Bi)
                mapfile -t COMPREPLY < <(compgen -d -- "$current") ;;
            install-files|-U|owns|-Qo)
                mapfile -t COMPREPLY < <(compgen -f -- "$current") ;;
            *) COMPREPLY=() ;;
        esac
    fi
}
complete -F _zap zap
