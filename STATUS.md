# Rust rewrite — status

Goal: rewrite zap in Rust with identical behaviour and output, prioritising speed. Done; the old implementation is gone (commit 1062b6b is the last that has it). libalpm is **not** linked: the subset zap uses is reimplemented in Rust under `src/libalpm/` (ported from pacman 7.1.0 @ 54d94116).

## Layout
- `Cargo.toml`, `src/*.rs`: zap.
- `src/libalpm/`: the engine. It has no UI or CLI code.
  - read side: vercmp, deps, local/sync db, packages, snapshots, archives (tar with gz/zstd/xz/bz2)
  - write side: `trans`, `remove`, `add` (extraction, backup and .pacnew), `conflict`, `sync` (resolve, sysupgrade, download, validate, load, commit), `dload` (file:// and http(s), CacheServer, If-Modified-Since, parallel, `.part` resume, per-server error limits), `sandbox` (DownloadUser: Landlock, seccomp, setuid child), `diskspace` (CheckSpace), `signing` (`gpg --status-fd`, key import via WKD and keyserver, sync DB signatures), `compress` (.Z natively; lrzip and lzop through their programs; lz4 and lzip via crates), `hook`, scriptlets in `util`
- `src/pactest.rs` (`--features pactest`, bin `zap-pactest`): a pacman-compatible front end over the engine.
- `pactest/run.py`: runs pacman's pactest suite against the shim, in a user namespace, optionally beside `/usr/bin/pacman`. `pactest/expected.txt` lists the tests that pass.
- `pactest/plan_diff.py`: compares `-Sp`, `-Sup`, `-Rp`, `-Rsp` and `-Rcp` plans with `pacman --print`, using copies of this system's dbs.
- `bench/`: `compare.sh` checks two builds for identical output and `bench.py` compares their speed. CI no longer runs them; use them against a previous build by hand.
- Reference pacman checkout: scratchpad `ref/pacman`. If it's gone, re-clone gitlab.archlinux.org/pacman/pacman at 54d94116.

## Plan / progress
1. [x] scaffold, util, ui, errors, cli, help, config, signals
2. [x] alpm read side
3. [x] read-only commands
4. [x] net, aur, news, srcinfo, sandbox, artifacts, builder. Dry runs matched the original byte for byte.
5. [x] benchmarks and output comparison against the original (dropped from CI with it)
6. [x] equivalence harness
   - pactest: 285 pass, 25 fail, 31 unsupported; pacman passes the 25 except one.
   - The one test only pacman passes is `remove-print-empty-replacements` (`--print-format`, a frontend feature zap doesn't use).
   - plan diff: 789/789 plans equal on this machine (`--samples 150`).
7. [x] write side: remove, `-U`, `-S`, `-Su`, all with their pactests passing
8. [x] transaction worker (`transaction::worker`), packaging (the PKGBUILD builds with cargo), README
   - The worker was checked end to end against a throwaway root: `pactest/worker_e2e.sh` (user and mount namespace, temp pacman.conf bind-mounted over /etc/pacman.conf).
9. [x] engine gaps: parallel and sandboxed downloads, `.part` resume, CheckSpace, key import, sync DB signature checks, lz4/lzip/lrz/Z/lzo
10. [x] real system: `pactest/container_e2e.sh PKG` (2026-11-16) passed: a week-stale archlinux:base-devel container ran `zap upgrade` (31 packages, DownloadUser=alpm, ParallelDownloads=5), then install, remove and install-files for every compression, all clean under `pacman -Dk`/`-Qkk`.

## Next
- Before each release, run `pactest/container_e2e.sh` on the built package (needs root docker).
- First Rust release: v0.2.0.r14.g3f4f32a94680 (GitHub pre-release). Published to the AUR on 2026-11-16 (x86_64 only: the seccomp filter in `libalpm/sandbox.rs` is x86_64-only, and its tests don't compile elsewhere). Later releases: `packaging/aur/update.sh VERSION`.
- Done on master; branch `rust-rewrite` merged and deleted.
- Not implemented in the engine: XferCommand. Config rejects it on purpose, as the original did.

## Gotchas
- The Arch libalpm build uses `LDCONFIG=/usr/bin/ldconfig` and `SCRIPTLET_SHELL=/usr/bin/bash` (`libalpm/util.rs`). With other paths, pactest roots behave differently.
- pactest writes `SigLevel = Never` by default. Sync packages sit in the cachedir when `cachepkgs` is true.
- With `--print`, pacman shows errors only, no events, and sets NOCONFLICTS|NOLOCK. The shim does the same.
- The sig URL follows a redirect only when the effective file name contains `.db` or `.pkg` (dload.c).
- makepkg's LTO breaks linking the bundled zstd. The PKGBUILD sets `!lto`.
- `/etc/subuid` has a range, so `unshare --map-auto --map-root-user` works for commit tests in a temp root.
- Never run the worker or shim against the real root. Use pactest, `plan_diff.py` (temp dbpath and root) or `pactest/worker_e2e.sh`.
- AUR review diffs use the pre-fetch HEAD as their baseline. Reset the cached checkout before comparing two back-to-back runs.
- CI's root pactest matches `pactest/expected.txt` as is; nothing needed pruning.
- sudo's pty relay spins forever once the command exits if its terminal keeps receiving input (e.g. `yes |`). Feed scripted runs a finite set of answers.
- Real pacman 7.1.0 `-U` of a local file leaves an empty `download-XXXXXX` dir in the cache. zap's sandbox removes its own.
- This host's docker bridge may have no outbound network (daemon.json forces DNS to 172.17.0.1). Use `--network host` with the resolved stub, as `container_e2e.sh` does.
- Sync DB snapshots are kept in `~/.cache/zap/.sync/`, for unprivileged runs only. The root worker never reads them.
