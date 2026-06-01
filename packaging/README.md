# `packaging/`

OS-level distribution artefacts for zwhisper.

```
packaging/
├── arch/
│   ├── PKGBUILD               — Arch Linux package recipe (M8 DoD #1)
│   ├── zwhisper.install       — post-install / post-upgrade / post-remove
│   ├── namcap.expected        — allow-listed namcap WARNINGs
│   └── tests/                 — bash smoke tests for the PKGBUILD
│       ├── pkgbuild_metadata.sh
│       └── install_paths.sh
```

## Arch Linux

The PKGBUILD builds the CLI-only product binaries (`zwhisperd` and
`zwhisper`) and installs them under the standard system paths. It
does not package tray services, settings GUI launchers, or FLTK
runtime/build prerequisites. See `packaging/arch/PKGBUILD` for the
full file. The release process, including how to refresh `b2sums`
after a tag, is in
[`docs/RELEASE.md`](../docs/RELEASE.md).

### Local install

```sh
cd packaging/arch
makepkg -si
pacman -Q zwhisper           # → 0.1.0-1
```

### Dry-run without installing

```sh
cd packaging/arch
makepkg -s --noinstall
namcap PKGBUILD
namcap zwhisper-*.pkg.tar.zst
```

`namcap` is a separate package on Arch (`pacman -S namcap`); the
WARNINGs it emits are allow-listed in `namcap.expected`. New
WARNINGs require either a fix or an explicit entry there.

### CI smoke

CI runs `bash packaging/arch/tests/pkgbuild_metadata.sh` and
`bash packaging/arch/tests/install_paths.sh` as part of the
`packaging-shell` job. They parse the PKGBUILD statically — no
`makepkg`, no `pacman`, no chroot — so a maintainer can iterate
on the recipe without standing up an Arch container locally.

## Other distributions

Out of scope for the 0.1.0 release: Flatpak, AUR submission,
`.deb`, `.rpm`, NixOS module. See `docs/M8-plan.md` § "Out of
scope" for the full list and the rationale for deferring them.
