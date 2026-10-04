complete -c zap -f
for cmd in search info install build get upgrade updates list foreign remove orphans autoremove clean help version
    complete -c zap -n '__fish_use_subcommand' -a $cmd
end
complete -c zap -l dry-run -d 'Plan without builds or system changes'
complete -c zap -l devel -d 'Include VCS rebuilds during updates or upgrade'
complete -c zap -l recursive -d 'Remove unneeded dependencies'
complete -c zap -l nosave -d 'Discard backup configuration during removal'
complete -c zap -l help -d 'Show help'
complete -c zap -l version -d 'Show version'
