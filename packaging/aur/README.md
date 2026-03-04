# Arch Packaging

This directory contains AUR-ready package recipes for `fish-session`.

## Packages

- `fish-session`: stable package from tagged GitHub release tarballs
- `fish-session-git`: development package from `main`

Both packages install:

- `/usr/bin/fish-session`
- `/usr/bin/fish-sessiond`
- Fish integration files under:
  - `/usr/share/fish/vendor_functions.d/fish_session.fish`
  - `/usr/share/fish/vendor_conf.d/fish-session.fish`

## Local build test

```bash
cd packaging/aur/fish-session
makepkg -si
```

or

```bash
cd packaging/aur/fish-session-git
makepkg -si
```

## AUR publish workflow

1. Create AUR repos (`fish-session` and optionally `fish-session-git`).
2. Copy matching `PKGBUILD` and generated `.SRCINFO` into each AUR git repo.
3. Commit and push to AUR.
4. Users can then install with:

```bash
paru -S fish-session
# or
paru -S fish-session-git
```
