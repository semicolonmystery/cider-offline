# CLAUDE.md

Guidance for AI coding assistants working in this repository. [`README.md`](README.md)
says what the app does; [`docs/src/how-it-works.md`](docs/src/how-it-works.md) explains
the architecture and [`docs/src/development.md`](docs/src/development.md) covers building,
testing and releasing. This file is about *how to work here*.

---

## 1. Rules

### 1.1 Ask when uncertain

If you are unsure about requirements, intent, scope or an unfamiliar API, ask. Don't
guess and don't quietly pick an interpretation. Small calls made while building are
fine, but list them in your report.

### 1.2 Verify against docs, not memory

Before writing code against a crate, read its docs for the exact version in
`Cargo.lock` (docs.rs, or context7 if you have it). Don't guess signatures, even for
crates you know well; this project tracks recent releases and APIs move. Add
dependencies at their latest stable version.

The stack: Rust 2024 edition (MSRV in the root `Cargo.toml`), Slint for the UI, tokio,
hudsucker for the MITM proxy, rusqlite for storage, reqwest for HTTP.

### 1.3 "Done" means verified

After a self-contained piece of work:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Until all three pass, the work is written, not verified. Never report a failure as
success; include its output. Code for another OS (`#[cfg(target_os = ...)]`) can't be
compiled locally, so say so instead of claiming it works.

### 1.4 No git operations unless told

Never branch, stage, commit, stash, push, tag or merge on your own initiative. When told
to commit, use Conventional Commits (`feat: ...`, `fix: ...`, `docs: ...`), with no
`Co-Authored-By` trailers and no Claude/AI name, e-mail or "Generated with" footer.

### 1.5 Follow the conventions

Write idiomatic Rust and use each library's documented patterns instead of hand-rolled
ones. `unsafe` is forbidden in the workspace; the app crate has two scoped exceptions
(Slint's generated code and one Windows call in `platform/windows.rs`). Don't add more.
If you find yourself fighting a convention, stop and ask.

---

## 2. Where code goes

| Crate | Folder | Holds |
|---|---|---|
| `cider-offline` | `crates/app` | The program: Slint UI (`ui/*.slint` + `src/ui/`), tray, single instance, start at login, Cider integration, cleanup, CLI and headless checks. The only crate that knows about windows and the desktop. |
| `cider-offline-core` | `crates/core` | The engine: proxy, offline responder, library sync, storage (SQLite), lyrics, download queue, config and folders. No UI. |
| `cider-offline-trust` | `crates/trust` | The local root CA and installing/removing it in each OS trust store. |
| `cider-offline-download` | `crates/download` | Downloading and decrypting Apple Music audio; implements core's `Downloader` trait. |
| icon generator | `packaging/icons/gen` | Build-time tool that renders the icons. Not part of the app. |
| website | `web` | The site at cider-offline.podik.cz (Next.js static export, npm); the book is built into it under `/docs/`. Not part of the app; see `web/CLAUDE.md`. |

Dependencies point toward core: app uses core, trust and download; download uses core;
core uses only trust. Keep it that way. Packaging lives in `packaging/` and
`crates/app/Packager.toml`; CI and releases in `.github/workflows/`.

---

## 3. Things that will surprise you

- A debug build uses the same data folders as an installed copy, so it runs on your real
  library. Create `target/debug/data` to run it in portable mode instead.
- Paths the app stores (audio files, artwork) are relative to the data folder, with `/`
  separators. Don't store absolute paths.
- Don't open a live `store.sqlite` with MSYS2/Git Bash `sqlite3.exe` on Windows; its
  locking doesn't match native SQLite and can corrupt the database. Use a copy, or
  Python's `sqlite3` with the app closed.
- After `cargo add` or `cargo remove`, check that the `[profile.dev.package.*]` blocks in
  the root `Cargo.toml` are still there.
- Icons in `packaging/icons/` are generated. Edit the SVGs in `packaging/icons/src/` and
  rerun the generator; never edit the PNGs or ICO by hand.
- Never log or print token values or cookies, including in tests and headless checks.

---

## 4. Commands

```sh
cargo run -p cider-offline                                    # run the app
RUST_LOG=info,cider_offline_core=debug cargo run -p cider-offline
cargo run -p cider-offline -- --offline-check                 # headless checks, see docs/src/development.md
mdbook serve docs --open                                      # preview the docs
cargo run --manifest-path packaging/icons/gen/Cargo.toml      # regenerate icons
```
