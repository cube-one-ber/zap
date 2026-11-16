# AUR package for zap

[`zap` is published on the AUR](https://aur.archlinux.org/packages/zap), with
Cube1ber as its submitter and maintainer.

The recipe builds a fixed, tested source release from GitHub. The package name
is `zap`; unrelated `zap-git` and `zap-bin` packages conflict because they also
install `/usr/bin/zap`.

`PKGBUILD` and `.SRCINFO` are generated from the main recipe,
`packaging/PKGBUILD.in`, for one published release:

```sh
packaging/aur/update.sh 0.2.0.r5.g0123456789ab
```

The script downloads that release's `zap-source.tar.gz`, pins its SHA-256 digest
and regenerates `.SRCINFO`. Test the recipe with `makepkg --cleanbuild` before
pushing.

Publishing requires an AUR account named `Cube1ber` with a registered SSH public
key. Verify the account with `ssh -T aur@aur.archlinux.org` before pushing. A
GitHub login does not grant AUR access.

Copy only `PKGBUILD`, `.SRCINFO`, `LICENSE` and `.gitignore` to the AUR Git
checkout, commit as Cube1ber, and push its `master` branch to
`ssh://aur@aur.archlinux.org/zap.git`. The packaging files use the 0BSD license;
the application remains GPL-3.0-or-later.

The main repository's automatic pre-release workflow does not publish AUR
updates.
