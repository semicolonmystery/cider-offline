# Packaging

Assets and helper scripts used by [`cargo-packager`](https://docs.rs/cargo-packager)
to build the Cider Offline installers. The packager configuration lives in
[`../crates/app/Packager.toml`](../crates/app/Packager.toml); the release
workflow ([`../.github/workflows/release.yml`](../.github/workflows/release.yml))
drives the whole process on tag pushes.

## Layout

```
packaging/
  icons/        app + installer icons in the sizes cargo-packager expects
  scripts/      stand-alone "remove the CA" scripts, shipped as resources
  nsis/         patched NSIS installer template
  macos/        Info.plist additions
  macos/liquid-glass-icon.sh
                builds the optional macOS 26 Liquid Glass Assets.car (release CI)
  desktop/      .desktop entry used by the rpm
  portable/     README.txt for the portable archives
  ci/install-linux-deps.sh
                Linux build/test packages, used by ci.yml and release.yml
  ci/smoke-test-linux.sh
                release.yml's check of the AppImage, .deb, .rpm and pacman package
  README.md     this file
```

## Icons

`cargo-packager` picks the right icon per format from the `icons` list in
`Packager.toml`: the `.ico` is used on Windows, the largest PNG is rasterised
into an `.icns` for macOS, and the PNGs are used directly on Linux.

The sources of truth are the SVGs in `icons/src/` (original artwork):

| Source | Use |
| --- | --- |
| `app.svg` | the app icon (all sizes above 16 px) |
| `app-16.svg` | hand-hinted 16 px master of `app.svg` |
| `tray-passthrough.svg` | tray icon in Passthrough mode (grey tile) |
| `tray-template.svg` | macOS menu-bar icon: the glyph only, black on transparent, no tile |

Every derived asset is generated from them and committed, so normal builds
never need an SVG renderer. After editing a source, run from the repository root:

```sh
cargo run --manifest-path packaging/icons/gen/Cargo.toml
```

`icons/gen/` is a stand-alone crate (not a workspace member; it uses `resvg`).
It writes:

- `icons/{16,32,128,256,512}x….png`: listed in `Packager.toml`. cargo-packager
  builds the macOS `.icns` from them and installs them into the Linux `hicolor`
  theme. Only add sizes that map to an `.icns` icon type (e.g. not 48 or 64),
  or macOS packaging fails.
- `icons/icon.ico`: 16–256 px. Used for the NSIS installer, and `crates/app/build.rs`
  embeds it into the Windows exe (`winresource`).
- `crates/app/assets/icon-256.png`: the window icon and sidebar logo (`app.slint`).
- `crates/app/assets/tray-active.png` / `tray-passthrough.png`: 32 px tray icons
  (Windows, Linux), converted to raw RGBA by `crates/app/build.rs`.
- `crates/app/assets/tray-template.png` / `tray-template-passthrough.png`: 44 px
  (22 pt @2x) macOS menu-bar icons, the second at 50 % opacity for Passthrough.
  Drawn as template images, so macOS tints them for light, dark and tinted menu
  bars. `crates/app/build.rs` embeds only these on macOS.

## Scripts (`scripts/`)

These are shipped inside every installer in a `scripts` folder (the
`resources` key in `Packager.toml`; for the rpm, the matching assets in
`crates/app/Cargo.toml`): `scripts\` in the Windows install folder,
`Contents/Resources/scripts/` in the `.app`, and
`/usr/lib/cider-offline/scripts/` in the `.deb`, `.rpm`, Arch package and
AppImage. They remove the proxy's local root CA from
the OS trust store for users who uninstall the app without first clicking
**Uninstall** on the **Uninstall certificate** row of the Settings page. Each mirrors the exact store locations and
nicknames used by `crates/trust/src/store/` so they stay in sync.

| Script | Store(s) cleaned | Notes |
| --- | --- | --- |
| `remove-ca-windows.ps1` | `Cert:\CurrentUser\Root` | `powershell -ExecutionPolicy Bypass -File remove-ca-windows.ps1`; Windows confirms each removal. |
| `remove-ca-macos.sh` | login keychain + user trust settings | asks for your password to change trust settings. |
| `remove-ca-linux.sh` | user NSS DBs (`~/.pki/nssdb`, Flatpak Cider) + system anchors | needs `certutil` (`libnss3-tools`); the system-anchor part prompts via `pkexec`. |

The CA subject / nickname is `Cider Offline CA`
(`CA_COMMON_NAME` in `crates/trust`). CAs created before the rename keep
`Cider Offline Proxy Local CA`; the app and the scripts handle both names.

## Linux `.desktop` entry

`cargo-packager` generates the `.desktop` file for the `.deb`, AppImage and
pacman packages from the `Packager.toml` metadata (name, identifier, category,
icon). The `.rpm` is built by `cargo generate-rpm` instead, which has no such
generator, so `desktop/cider-offline.desktop` is installed as-is by the
`[package.metadata.generate-rpm]` assets in `crates/app/Cargo.toml`. Edit that
file to change the rpm entry, and keep it in line with `Packager.toml`.

## Windows installer template (`nsis/`)

`nsis/installer.nsi` is cargo-packager 0.11.8's NSIS template with these edits
(listed in its header): an always-present, unticked "Also remove my downloaded
music and settings" checkbox on the uninstall page; an `ExecWait` of the app
with `--remove-all-data` (ticked) or `--uninstall-cleanup` (otherwise, including
silent `/S`), with a message box (not in silent mode) if it exits non-zero; and
an `/UPDATE` marker that the installer passes to the old uninstaller on an
upgrade, which skips that cleanup so an upgrade keeps the trusted CA, the
start-at-login entry and Cider's proxy setting. Re-diff against upstream when
upgrading cargo-packager.

## macOS `Info.plist` (`macos/`)

Merged into the generated plist (`info-plist-path`): `LSUIElement` (no Dock icon
by default) and `NSHighResolutionCapable`.

## Portable build (`portable/`)

`portable/README.txt` is shipped in the portable archives.
