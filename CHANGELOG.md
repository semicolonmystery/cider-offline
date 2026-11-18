# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0] - 2026-11-17

### Added

- Local HTTPS proxy between Cider and Apple Music, with its own certificate authority that the app installs into the system trust store after you confirm.
- Offline mode: when Apple Music is unreachable, Cider is answered from the local library, artwork, audio and lyrics.
- Library sync of your Apple Music playlists, albums and songs.
- Song and music video audio downloads, decrypted locally, with a download queue and up to six downloads at once.
- Automatic download of played songs, right away or once playback has been idle for a while.
- Lyrics from Apple Music, Cider Community, AMLL TTML DB, Musixmatch, NetEase Cloud Music, Kugou and LRCLIB, with a configurable provider order.
- Custom audio and custom lyrics (TTML or LRC) per song.
- Automatic setup of Cider's proxy setting on Windows, and setup instructions for macOS and Linux.
- Start at login, start minimized, and a system tray icon.
- Portable mode: a `data` folder next to the program keeps everything inside it.
- Remove all data from the app or with `--remove-all-data`, including the certificate and the start-at-login entry.
- Installers and portable builds for Windows (x64, x86, ARM64), macOS (universal) and Linux (x86_64, ARM64; AppImage, .deb, .rpm and an Arch package).

[Unreleased]: https://github.com/semicolonmystery/cider-offline/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/semicolonmystery/cider-offline/releases/tag/v0.1.0
