# AUR package for zap

[`zap` is published on the AUR](https://aur.archlinux.org/packages/zap), with
Cube1ber as its submitter and maintainer.

The `PKGBUILD` and `.SRCINFO` build a fixed, tested source release from GitHub.
The package name is `zap`; unrelated `zap-git` and `zap-bin` packages conflict
because they also install `/usr/bin/zap`.

The default compiler is `/usr/bin/zig-0.15` from the AUR `zig0.15` package.
`zig0.15-bin` also provides this dependency. An already verified Zig 0.15.2
compiler may be used locally without installing another toolchain:

```sh
ZIG=/absolute/path/to/zig-0.15.2/zig makepkg --cleanbuild
```

Regenerate the public metadata with the default dependencies:

```sh
env -u ZIG makepkg --printsrcinfo > .SRCINFO
```

Publishing requires an AUR account named `Cube1ber` with a registered SSH public
key. Verify the account with `ssh -T aur@aur.archlinux.org` before pushing. A
GitHub login does not grant AUR access.

Copy only `PKGBUILD`, `.SRCINFO`, `LICENSE` and `.gitignore` to the AUR Git
checkout, commit as Cube1ber, and push its `master` branch to
`ssh://aur@aur.archlinux.org/zap.git`. The packaging files use the 0BSD license;
the application remains GPL-3.0-or-later.

For subsequent releases, update `pkgver`, the source SHA-256 digest and `.SRCINFO`,
then test the new recipe before pushing. The main repository's automatic
pre-release workflow does not publish AUR updates.
