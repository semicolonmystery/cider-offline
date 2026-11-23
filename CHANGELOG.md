# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.1] - 2026-11-23

### Fixed

- Downloaded songs no longer restart at 0:00 when Cider picks Apple's spatial stream for them.
- Downloading or re-downloading a song while it was paused could start a second download of the same song. The queue now holds each song once.
- Downloaded files now store their real length, so players show the full duration right away. Re-download older files to get this.
- Songs are no longer listed twice (or three times) in the song lists. Apple names one song by its catalog id and by library ids, and 0.1.0 stored a copy of the song under each. The library now keeps each song once, and the upgrade merges the copies with their audio, lyrics, downloads and playlist membership.
- An album or playlist no longer shows as downloaded after its downloads were cancelled or removed, or when only a few of its songs are on your computer. Whether it is downloaded now comes from the songs' files under the current download format.
- The Settings page no longer gets wider than the window. The lyrics providers the failure rule applies to are listed one per line, and long folder paths wrap.
- The line at the bottom of the Overview updates right away when an Apple Music session is captured, and the certificate and Cider's settings are checked again when you come back to the window.
- **Remove downloads** on an album or playlist releases every download it held, also one kept under another id of a song or for a song that left the playlist, and the count of removed songs only includes songs whose files were deleted.

### Added

- Spatial mix downloads: the Dolby Atmos mix rendered for headphones, for songs mastered in Atmos.
- A Download format setting: Stereo only, Spatial where available, or Stereo + Spatial.
- A Play the other downloaded mix setting. When Cider asks for a mix you haven't downloaded, it plays the one you have instead of streaming from Apple. Offline this always happens.
- A headphones badge on songs that have a downloaded spatial mix.
- Partly downloaded albums and playlists show how many of their songs are downloaded ("12 of 20"), and **Download missing songs** fetches the rest.
- An album or playlist whose songs are all on your computer only through other downloads shows a link icon (**Saved through other downloads**) instead of the downloaded check. Clicking it, or **Link downloads** in its menu, links those songs to it too without downloading them again, so removing the other download keeps them.
- Re-download for songs, albums and playlists, in the current download format. The old file keeps playing until the new one is complete.
- Cancel for a song, an album or playlist, or the whole queue. Files that already finished are kept.
- More album and playlist actions: download missing songs, retry failed downloads, remove downloads and keep downloaded (songs added later are downloaded after the next library sync). Removing downloads keeps songs another album, playlist or single download still needs.
- Unlinked downloads: songs nothing needs any more, for example after they left a playlist. Clean them up by hand in Settings, or let the app do it every few hours.
- **Remove lyrics** for a song, and **Remove lyrics…** for an album or playlist. A collection's removal keeps lyrics you imported; a song's removes them too after asking.
- A Remove lyrics when removing downloads setting (off by default): removing a download also deletes the lyrics the app fetched for that song.
- A redesigned Overview page: one big switch for Active and Passthrough, and a single line at the bottom when something needs fixing.

### Changed

- The app starts in Active whenever the certificate is trusted, instead of in the mode it was last left in. Until the certificate is trusted it stays in Passthrough.
- Settings are reorganised into Proxy, Certificate, Cider, Downloads, Startup, Library, Lyrics and Storage sections. The proxy status, the certificate and the Cider setting moved there from the Overview.
- Capture files hide every session token and cookie, not only the authorization header.
- The library database is upgraded on the first start. 0.1.0 can't use the upgraded database, so don't go back to it.

## [0.1.0] - 2026-11-17

### Added

- Local HTTPS proxy between Cider and Apple Music, with its own certificate authority that the app installs into the system trust store after you confirm.
- Offline mode: when Apple Music is unreachable, Cider is answered from the local library, artwork, audio and lyrics.
- Library sync of your Apple Music playlists, albums and songs.
- Song and music video audio downloads for offline playback in Cider, with a download queue and up to six downloads at once.
- Automatic download of played songs, right away or once playback has been idle for a while.
- Lyrics from Apple Music, Cider Community, AMLL TTML DB, Musixmatch, NetEase Cloud Music, Kugou and LRCLIB, with a configurable provider order.
- Custom audio and custom lyrics (TTML or LRC) per song.
- Automatic setup of Cider's proxy setting on Windows, and setup instructions for macOS and Linux.
- Start at login, start minimized, and a system tray icon.
- Portable mode: a `data` folder next to the program keeps everything inside it.
- Remove all data from the app or with `--remove-all-data`, including the certificate and the start-at-login entry.
- Installers and portable builds for Windows (x64, x86, ARM64), macOS (universal) and Linux (x86_64, ARM64; AppImage, .deb, .rpm and an Arch package).

[Unreleased]: https://github.com/semicolonmystery/cider-offline/compare/v0.1.1...HEAD
[0.1.1]: https://github.com/semicolonmystery/cider-offline/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/semicolonmystery/cider-offline/releases/tag/v0.1.0
