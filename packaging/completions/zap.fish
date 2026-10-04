complete -c zap -f
for cmd in search select info localinfo install install-files build build-local install-local get pkgbuild upgrade updates list foreign local-search files owns check reason remove orphans autoremove clean stats news help version
    complete -c zap -n '__fish_use_subcommand' -a $cmd
end
complete -c zap -l dry-run -d 'Plan without builds or system changes'
complete -c zap -l devel -d 'Include VCS rebuilds during updates or upgrade'
complete -c zap -l recursive -d 'Remove unneeded dependencies'
complete -c zap -l nosave -d 'Discard backup configuration during removal'
complete -c zap -l aur -d 'Scope requested packages to AUR'
complete -c zap -l repo -d 'Scope requested packages to repositories'
complete -c zap -l needed -d 'Skip current installed packages'
complete -c zap -l asdeps -d 'Mark requested packages as dependencies'
complete -c zap -l asexplicit -d 'Mark requested packages as explicit'
complete -c zap -l sandbox -d 'Isolate every makepkg phase (default)'
complete -c zap -l no-sandbox -d 'Build with the invoking user permissions'
complete -c zap -l rebuildtree -d 'Rebuild installed foreign dependencies'
complete -c zap -l cleanafter -d 'Remove untracked outputs after installation'
complete -c zap -l quiet -s q -d 'Print names only'
complete -c zap -l searchby -r -a 'name name-desc maintainer provides depends makedepends checkdepends optdepends'
complete -c zap -l sortby -r -a 'votes popularity name modified'
complete -c zap -l help -d 'Show help'
complete -c zap -l version -d 'Show version'
complete -c zap -n '__fish_seen_subcommand_from remove reason localinfo files check list foreign -R -Rns -D -Qi -Ql -Qk -Q -Qm' -a '(zap list --quiet 2>/dev/null)'
complete -c zap -n '__fish_seen_subcommand_from build-local install-local -B -Bi' -a '(__fish_complete_directories)'
complete -c zap -n '__fish_seen_subcommand_from install-files owns -U -Qo' -F
