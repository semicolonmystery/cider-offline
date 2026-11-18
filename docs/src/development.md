# Development

[How it works](how-it-works.md) explains the architecture and the crates. This page is about building, testing and releasing.

## Prerequisites

You need a stable Rust toolchain no older than the `rust-version` in the workspace `Cargo.toml` (currently 1.95). The workspace uses the 2024 edition.

- Windows: the MSVC toolchain (Visual Studio Build Tools with "Desktop development with C++"). The GNU toolchain is not supported. Building for 32-bit Windows (`i686-pc-windows-msvc`) also needs NASM on `PATH`, and building for ARM64 (`aarch64-pc-windows-msvc`) needs `clang-cl`, which comes with Visual Studio's LLVM component.
- macOS: the Xcode command-line tools (`xcode-select --install`).
- Linux: `pkg-config` and the development packages for xcb, xkbcommon and fontconfig, plus `libnss3-tools` for the trust store tests. On Debian and Ubuntu, `packaging/ci/install-linux-deps.sh` installs exactly what CI uses. The app needs no GTK.

## Build and run

```sh
cargo build -p cider-offline
cargo run -p cider-offline
```

A debug build on Windows keeps its console window, so log output is visible; release builds are GUI-only.

The app logs through `tracing`. The default filter is `info,hudsucker=warn`; set `RUST_LOG` to change it:

```sh
RUST_LOG=info,cider_offline_core=debug cargo run -p cider-offline
```

In PowerShell, set it first with `$env:RUST_LOG = "info,cider_offline_core=debug"`. The useful targets are `cider_offline` (the app), `cider_offline_core`, `cider_offline_trust`, `cider_offline_download` and `hudsucker` (the proxy library). Logs also go to the daily file described in [Files and folders](files.md#logs).

A debug build uses the same folders as an installed copy, so it works on your real library and settings. To keep it separate, create an empty `data` folder next to the built executable (`target/debug/data`); the build then runs in [portable mode](files.md#portable-mode).

## Checks before a change is done

These three must pass:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

CI (`.github/workflows/ci.yml`) runs exactly these checks, on the whole workspace, on every push and pull request, on Linux x86_64, Linux ARM64, Windows and macOS.

The workspace forbids `unsafe` code and warns on missing docs and on Clippy's pedantic lints, so `-D warnings` makes those hard errors. The app crate only denies `unsafe` instead of forbidding it, for two scoped exceptions: the code Slint generates from the `.slint` files, and one Windows call (`AllowSetForegroundWindow`, so a second launch can bring the running window to the front).

## Headless checks

The app binary has a few modes that run against your real data without opening a window, for checking library, offline and download behaviour against the live Apple Music API. They need tokens the proxy has already captured from Cider, and they print no token values. Run them with the app closed, from a debug or release build:

| Command | What it checks |
| --- | --- |
| `cider-offline --library-check [--fresh] [--import-tokens <store.sqlite>]` | Runs one library sync and compares the result with what the Library page reads. `--fresh` syncs into a new temporary database that has only the tokens, `--import-tokens` copies the tokens from another database first. |
| `cider-offline --offline-check [--captures <dir>] [--baseline] [--verbose]` | Starts the proxy forced offline on a free port, over a temporary copy of the database, and replays captured Cider requests against it through real TLS. Then checks the library contents, artwork, search, playback, the Songs page setting and lyrics, offline and online. `--captures` defaults to the app's captures folder. |
| `cider-offline --download-check <catalog id>... [--concurrency N]` | Runs the given songs through the real download queue and downloader, printing every queue event. |
| `cider-offline --lyrics-check [--collection NAME] [--default-settings] [--overwrite]` | Counts songs with lyrics, runs "Fetch all missing lyrics" (or one collection's fetch) and counts again. |
| `cider-offline --lyrics-check --provider <id\|all> [--collection NAME]... [--limit N] [--ids ID...] [--show-ttml]` | Asks one provider (or each in turn) about some songs and prints what it found. Stores nothing. |
| `cider-offline --perf-check [--collection NAME]` | Times the paged library queries, the cover prefetch and opening a large playlist. |
| `cider-offline --screenshot out.png [--page library\|songs\|detail\|overview\|settings] [--collection NAME] ...` | Renders one view with the real window and data and saves it as a PNG. See `crates/app/src/ui/shot.rs` for all options. |

`--library-check` doesn't need to know your library: it checks that the Library page shows exactly the playlists and albums Apple returned, each once and with its full track list in Apple's order, that albums come after playlists, and that every cover loads. `--perf-check` opens the largest collection of the Apple Music library unless `--collection` names one, and `--screenshot --page detail` opens the first one.

The `--offline-check` replay needs captures, so turn on **Capture Apple traffic to disk** and use Cider for a while first.

## Building packages

Packages are built with [cargo-packager](https://docs.rs/cargo-packager) 0.11.8 (the NSIS template in `packaging/nsis/` is a patched copy of that version's) and, for the `.rpm`, `cargo-generate-rpm`. Install them with:

```sh
cargo install cargo-packager --version 0.11.8 --locked
cargo install cargo-generate-rpm --locked
```

Then, from the repository root:

```sh
cargo build --release --locked -p cider-offline
cargo packager --config crates/app/Packager.toml --formats nsis          # Windows
cargo packager --config crates/app/Packager.toml --formats app,dmg       # macOS
cargo packager --config crates/app/Packager.toml --formats appimage,deb,pacman   # Linux
cargo generate-rpm -p crates/app                                         # Linux .rpm
```

The packages land in `target/packages` (the `.rpm` in `target/generate-rpm`). `Packager.toml` reads the binary from `target/release`. When you build for another target with `--target`, change `binaries-dir` in `Packager.toml` to that target's folder; the release workflow does the same.

What goes into the packages:

- `crates/app/Packager.toml`: name, version, icons, the bundled removal scripts, the per-user NSIS installer, the macOS minimum version (11.0) and the Linux package dependencies.
- `packaging/nsis/installer.nsi`: the Windows installer template, with the "Also remove my downloaded music and settings" checkbox and the cleanup calls described in [Uninstalling](uninstalling.md#windows-installer).
- `packaging/macos/Info.plist`: merged into the app's plist; makes it a menu bar app without a permanent Dock icon.
- `packaging/scripts/`: the certificate removal scripts.
- `packaging/portable/README.txt`: the readme of the portable archives.
- `packaging/icons/`: generated icons. After editing an SVG in `packaging/icons/src/`, regenerate them with `cargo run --manifest-path packaging/icons/gen/Cargo.toml`.

## Releases

Releases are built by `.github/workflows/release.yml` when a tag starting with `v` is pushed:

```sh
git tag v0.1.0
git push origin v0.1.0
```

The workflow first checks that the tag (without its `v`) equals both the workspace version in `Cargo.toml` and the version in `crates/app/Packager.toml`, and stops if they differ. Then it builds:

| Runner | Targets | Output |
| --- | --- | --- |
| `windows-latest` | x64, x86 | NSIS setup `.exe` and portable `.zip` |
| `windows-11-arm` | ARM64 | NSIS setup `.exe` and portable `.zip` |
| `macos-26` | arm64 and x86_64, joined into one universal binary | `.dmg` and portable `.zip` |
| `ubuntu-24.04` | x86_64 | AppImage, `.deb`, `.rpm`, Arch package with its PKGBUILD, portable `.tar.gz` |
| `ubuntu-24.04-arm` | aarch64 | the same as x86_64 |

Every job runs its build with `--version` as a smoke test before uploading. The last job collects everything, writes one `SHA256SUMS.txt`, and creates a draft GitHub Release with generated notes. Review the draft and publish it by hand.

The builds are unsigned unless signing secrets are configured. With `WINDOWS_CERTIFICATE_THUMBPRINT` set, the Windows installer is Authenticode-signed. With the `APPLE_*` secrets (certificate, signing identity, Apple ID, app-specific password, team ID), the macOS app is signed with a Developer ID and notarized; without them it is ad-hoc signed.

## This documentation

The documentation is an [mdBook](https://rust-lang.github.io/mdBook/) in `docs/`, with the pages in `docs/src/` and the table of contents in `docs/src/SUMMARY.md`. To preview it:

```sh
cargo install mdbook --locked
mdbook serve docs --open
```

`mdbook build docs` writes the site to `docs/book/`, which is not committed. Pushing changes under `docs/` to `main` runs `.github/workflows/docs.yml`, which builds the book and publishes it to <https://cider-offline.podik.cz/docs/> with GitHub Pages.
