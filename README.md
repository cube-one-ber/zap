<img src="assets/icon.svg" width="64" height="64" alt="">

# zap

A terminal AUR helper written in **Zig 0.15.2**. zap imports `alpm.h` and `curl/curl.h` directly, queries package databases through **libalpm**, and commits native ALPM transactions. Privilege escalation uses **systemd run0 and polkit**. Package queries and transactions do not parse pacman output.

## Build

Use Arch Linux or an Arch derivative with `pacman` (libalpm headers/library), `curl`, `pkgconf`, `git`, `bubblewrap`, `systemd >= 256`, and `base-devel`. Install **Zig 0.15.2**; newer Zig versions have different standard library APIs.

```sh
zig build -Doptimize=ReleaseSafe
zig build test
./zig-out/bin/zap --help
./zig-out/bin/zap search ripgrep
./zig-out/bin/zap install google-chrome --dry-run
```

The native Arch build links `/usr/lib/libalpm` and `/usr/lib/libcurl`. Zig supplies libc startup objects targeting glibc 2.38 to avoid linker incompatibilities with newer host GCC startup objects; runtime dependencies still come from the host. Cross compilation requires matching target headers and libraries and is not configured here.

The test suite includes AUR API fixtures, name/manifest validation, signature policy parsing, terminal escape filtering, versioned dependencies, split-package providers, archive hashes and symlinks, exact artifact selection and duplicate/base validation, optional debug outputs, package destinations with spaces, native ALPM preparation/conflict tests in temporary databases, and an actual unprivileged makepkg build of a local fixture. Tests never install packages on the host or invoke run0. They also cover CLI scope/option validation, numeric selections, local source snapshots, RSS validation, installation reasons, and real sandboxed makepkg phases that cannot read a private file or change Git metadata. Tests require `bsdtar`, `git`, `makepkg`, `bubblewrap`, working unprivileged user namespaces, and the build tools in `base-devel`.

## Install

Read-only commands and `build` work directly from `zig-out/bin/zap`. System transactions require a **root-owned `/usr/bin/zap`**, with root-owned parent directories and no group/other write access. This prevents elevating a binary from a user-writable checkout.

Build a local Arch package from the current sources:

```sh
./packaging/source.sh
cd packaging
makepkg
# Or, if the matching Zig compiler is outside PATH:
# ZIG=/absolute/path/to/zig-0.15.2/zig makepkg
```

`source.sh` generates a reproducible source archive and a PKGBUILD with its SHA-256 checksum. Read `packaging/PKGBUILD.in` before building. Install the resulting package through systemd:

```sh
run0 /usr/bin/pacman -U ./zap-0.1.0-1-x86_64.pkg.tar.zst
```

Use the filename reported by makepkg for your architecture and compression setting. This bootstrap installation uses pacman; subsequent zap transactions use libalpm directly. Do not make zap setuid, and do not grant blanket passwordless polkit permissions. run0 uses systemd's normal authorization policy and authentication agent. A running system systemd manager is required for system changes.

The package includes a manual and Bash, Zsh, and Fish completions.

## Commands

| Command | Alias | Behavior |
| --- | --- | --- |
| `search TERM` | `-Ss` | Search repository databases and the AUR |
| `select TERM` | | Search, choose numbered results/ranges, then review and install |
| `local-search TERM` | `-Qs` | Search installed names and descriptions |
| `localinfo PACKAGES` | `-Qi` | Installed metadata, reasons and reverse dependencies |
| `info PACKAGES` | `-Si` | Versions, dependencies, maintainer, URLs, update dates |
| `install PACKAGES` | `-S` | Resolve dependencies, review sources, build and install |
| `build PACKAGES` | | Build AUR packages without installation; dependencies must already be installed |
| `build-local DIRS` | `-B` | Build local PKGBUILDs from fresh review snapshots |
| `install-local DIRS` | `-Bi` | Resolve dependencies, review and install local PKGBUILDs |
| `install-files FILES` | `-U` | Install local archives through verified root staging |
| `pkgbuild PACKAGES` | `-Gp` | Print AUR PKGBUILDs without sourcing them |
| `get PACKAGES` | `-G` | Fetch AUR sources into the user cache without executing them |
| `upgrade` | `-Syu` | Refresh and upgrade repositories, then update AUR packages |
| `updates` | `-Qu` | Compare installed versions against cached repositories and live AUR metadata |
| `list [PACKAGES]` | `-Q` | Installed versions, installation dates and reasons |
| `foreign` | `-Qm` | Installed packages absent from configured repositories |
| `files PACKAGES` | `-Ql` | List installed package files |
| `owns FILES` | `-Qo` | Find installed file owners, including directory aliases |
| `check [PACKAGES]` | `-Qk` | Detect missing installed files; no checksum/content verification |
| `reason PACKAGES --asdeps/--asexplicit` | `-D` | Review and change installation reasons under the ALPM lock |
| `stats` | `-Ps` | Counts, orphan count, installed size and largest packages |
| `news` | `-Pw` | Latest Arch news titles, dates and links for intervention notices |
| `remove PACKAGES` | `-R` | Remove packages with ALPM dependency checks |
| `orphans` | `-Qdt` | List dependencies with no required or optional users |
| `autoremove` | | Review and remove orphan dependencies |
| `clean [BASES]` | `-Sc` | Clear all or selected cached package-base directories |
| `version` | `--version` | Show zap version and libalpm ABI |

`--dry-run` skips builds, privilege escalation, and system changes for install, select, install-files, build, build-local, install-local, upgrade, remove, reason, autoremove, and clean. Installation/build plans may fetch or update the user cache and ask for a provider choice. Repository dependencies and conflicts are finalized by libalpm in the privileged transaction; dry-run is a build/dependency plan, not a simulation of committing the transaction. `get` and `pkgbuild` fetch sources and reject `--dry-run`.

`updates --devel` and `upgrade --devel` include installed `-git`, `-svn`, `-hg`, `-bzr`, `-cvs`, `-darcs`, and `-fossil` packages for rebuild. Ordinary upgrades preserve existing installation reasons. `remove --recursive` also removes unneeded dependencies. `remove --nosave` discards backup configuration files; `-Rns` combines both options. Plain removal retains the usual ALPM backup behavior. There is no unattended transaction confirmation flag.

```sh
zap info google-chrome
zap install google-chrome
zap upgrade --devel
zap build an-aur-package
zap remove an-installed-package --recursive
zap autoremove --dry-run
```

## Useful paru/yay workflows

The command selection follows the [paru manual](https://github.com/Morganamilo/paru/blob/master/man/paru.8) and [yay manual](https://jguer.github.io/yay/man.html). zap implements the everyday workflows with its own native ALPM worker and a smaller option surface; it does not claim complete pacman/paru/yay command compatibility.

| Workflow | zap behavior |
| --- | --- |
| Combined repository/AUR search and provider selection | `search` and `select`; numbered selections support `1 3-5` and commas |
| AUR-only or repository-only operations | `--aur` / `--repo` on search, select, info, install, updates and upgrade; `-Sua` / `-Qua` are AUR-only aliases |
| Needed-only installs and installation reasons | `install --needed`, `--asdeps`, `--asexplicit`, and `reason`; reason overrides affect requested packages, not every dependency |
| Rebuild installed AUR dependency trees | `--rebuildtree` on builds/installs/upgrades; rejects combination with `--needed`; cycles fail explicitly |
| Ranked AUR search | `--sortby votes` (default), `popularity`, `name`, or `modified`; deterministic name tie breaks |
| Search AUR fields | `--searchby name`, `name-desc`, `maintainer`, `provides`, `depends`, `makedepends`, `checkdepends`, or `optdepends`; repository search retains its own name/description semantics |
| Source retrieval and PKGBUILD display | `get` / `-G`, `pkgbuild` / `-Gp`; no build code execution |
| Local PKGBUILD builds and local archive installs | `build-local` / `-B`, `install-local` / `-Bi`, `install-files` / `-U` |
| Package inspection and scripts | Installed search/info, file listings/ownership/presence checks, statistics, and `--quiet` name output; `-Qq`, `-Qmq`, `-Qdtq` aliases |
| Arch intervention notices | `news` shows the ten latest feed entries; read linked instructions before upgrades |
| Cache maintenance | `clean [BASES]` confirms deletion; `--cleanafter` removes untracked and ignored files after successful installation, retaining tracked review sources and nested VCS repositories |
| Safer build isolation | Bubblewrap is the default for **every** makepkg invocation; failure never falls back automatically |

Repository-only upgrades refresh and perform a complete repository upgrade. AUR-only upgrades do not refresh repositories, and dependencies can still come from either source. There is no package exclusion menu that encourages partial repository upgrades. Ordinary installs rebuild/reinstall requested same-version packages; `--needed` skips current or newer installed versions and still applies requested reason changes. `--devel` requests suffix-based rebuilds rather than checking upstream VCS commits.

Local build directories must include a static `.SRCINFO` matching their PKGBUILD. zap copies regular files into a fresh cache snapshot and reviews **all** copied files before any execution. It excludes `.git`, top-level `src`/`pkg` trees and package archives, preserves executable bits, rejects symlinks/special files, and caps snapshots at 1,024 entries, 32 directory levels and 64 MiB. It builds every package in a local split base, orders dependencies among supplied local bases, and leaves the original directory unchanged. `clean` clears these snapshots too; named cleanup takes cache directory/base names, not arbitrary paths.

```sh
zap select browser --aur --sortby popularity --dry-run
zap upgrade --repo
zap -Sua --devel --dry-run
zap install ripgrep --needed
zap reason ripgrep --asexplicit --dry-run
zap -Bi ./my-package --dry-run
zap build-local ./my-package
zap install-files ./package.pkg.tar.zst --dry-run
zap owns /usr/bin/bash
zap files bash --quiet
zap news
```

No arguments display help; unknown commands/options fail. `--` ends option parsing for filenames. Flags that do not apply to a command fail explicitly. zap deliberately omits arbitrary makepkg/privilege command flags, checksum/PGP bypasses, unattended confirmation, automatic PGP trust, executable user hooks/configuration, broad overwrite switches, and automatic cycle bootstrapping. Custom PKGBUILD repositories, repository source downloads, AUR comment scraping, persistent VCS commit tracking, and local-repository/chroot management are outside this implementation.

## Build isolation

Bubblewrap gives each makepkg invocation a fresh temporary home, isolated process/IPC namespaces, no capabilities, and an empty environment with fixed tool paths. The host system and package database are read-only. The package checkout is the only writable host directory, with its `.git` metadata mounted read-only to prevent build code from planting Git configuration/hooks for later helper operations. User credentials, desktop/session sockets, SSH agents, home configuration and private keyrings are not mounted. A sandbox probe must succeed before dependency installation or build execution.

Network access remains available for source downloads. This is filesystem/process isolation using the host toolchain, not a reproducible clean chroot or protection against kernel vulnerabilities. A fresh home means user makepkg settings, custom package destinations, and source-signing keys are unavailable by default. A source requiring an unavailable PGP key fails verification. `--no-sandbox` explicitly opts into the previous user-permission build behavior when user configuration/keys are required; source review, hash checks, and signature policy still apply. Do not bypass signature verification to get a build through.

## Dates in the UI

AUR results show RPC `LastModified` as **last modified**. Repository results show the package **build date**, the available repository metadata timestamp. Local listings show the **installation date**. These are labeled separately and rendered in UTC. Missing timestamps display `unknown`. Results also flag out-of-date and unmaintained AUR packages. `NO_COLOR=1` disables styling.

## Build and transaction flow

1. Prefer an installed satisfier for dependencies and a configured repository provider before consulting AUR.
2. Fetch AUR Git repositories over HTTPS. Resolve versioned runtime, make and check dependencies from static `.SRCINFO`, detect cycles, group split packages by base, and order builds by their dependencies. Split-package virtual providers remain attached to the package that declares them. For safety, the dependency plan includes dependencies declared by all split packages in a base.
3. Show changes to cached sources and every tracked source file. Require explicit review of every base before sourcing any PKGBUILD, including `makepkg --printsrcinfo`. Compare generated metadata with `.SRCINFO`; reject discrepancies.
4. Install missing repository build dependencies through a reviewed native transaction. Run every makepkg phase in the default sandbox as the original user, with no `--syncdeps` or `--install`. Verify reviewed sources around each build. Literal VCS `pkgver` rewrites are allowed and restored after a successful build; executable changes are rejected.
5. Read built archive metadata through libalpm. Validate every required package by name, reject duplicate identities and mismatched package bases, and allow absent automatic debug archives when no symbols were produced. Display validated outputs in the planned package order with their SHA-256 digests. Invoke the fixed installed worker through run0 using a bounded, validated request.
6. The worker opens regular archives with `O_NOFOLLOW`, copies them to a private root-owned staging directory, checks the expected hashes and package names, and applies the configured local signature policy. Commit uses those private copies.
7. Acquire ALPM's database lock, prepare dependencies/conflicts, prompt for providers/replacements/key imports when necessary, honor HoldPkg, and display all final additions/removals and the installed-size change. Require a final confirmation before committing. ALPM runs package scripts and system hooks, reports `.pacnew`/`.pacsave` files, and maintains its normal log.

Each package base is installed before building dependants. A complete AUR upgrade spans several transactions: an earlier successful install remains installed if a later build fails. zap reports failures instead of silently skipping unresolved targets or overwriting file conflicts. SIGINT/SIGTERM request cancellation; transaction resources are released when control returns. ALPM transactions are not a filesystem rollback system, and force-killing a worker can leave its normal database lock behind.

## Configuration and security boundaries

zap reads `/etc/pacman.conf` and recursive/glob `Include` files. It supports repository priority, mirrors and `$repo`/`$arch` substitution, repository Usage, signature levels, architecture, root/database/log/GPG/cache/hook paths, IgnorePkg, IgnoreGroup, HoldPkg, NoUpgrade, NoExtract, CheckSpace, DownloadUser and ParallelDownloads. Local signature policy inherits the global policy when not explicitly set. Configured `XferCommand` and `AssumeInstalled` are rejected rather than silently bypassed. zap is not a drop-in implementation of every pacman option.

The cache is `$XDG_CACHE_HOME/zap` or `$HOME/.cache/zap`, restricted to its owner and locked to prevent simultaneous helper builds. Cache/build metadata symlinks, special-file archives, invalid names, oversized metadata, duplicate targets and unsafe worker paths are rejected. Network requests require HTTPS with certificate validation, have timeouts and response limits, and escape RPC arguments. Child processes receive an allowlisted environment. No shell command interpolation is used. Remote text is filtered for terminal control characters. The only C shim adapts libalpm's variadic log callback; Zig compiles it as part of the build.

PKGBUILDs are programs. Default build isolation limits their host access; with `--no-sandbox`, approved code has the permissions of your user. Source review and hash checks alone do not sandbox code. Installed packages, package scriptlets, and hooks can change the system as root after you approve the final transaction. The implementation has not received an independent security audit. Signed-package requirements are never disabled to make an AUR installation succeed; if your LocalFileSigLevel requires signatures, provide appropriately signed build artifacts.

Deliberate limits include no automatic cycle bootstrapping, no edits to tracked source files, no symlinked tracked source files, no automatic PGP trust for makepkg sources, no broad file-overwrite switch, and no unattended privileged transactions. Unsupported or inconsistent build metadata fails explicitly.

GPL-3.0-or-later. See [LICENSE](LICENSE).
