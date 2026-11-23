# How it works

Cider Offline is an HTTPS proxy that sits between Cider and Apple Music, with a local database behind it and a desktop window in front.

```text
                          your computer
 +-----------------------------------------------------------+
 |                                                           |
 |  +-------+      +-----------------------+                 |       +-------------+
 |  | Cider | ---> |  proxy on 127.0.0.1   | --- online ---------->  | Apple Music |
 |  +-------+      |                       |                 |       +-------------+
 |                 |  Apple hosts:         |                 |
 |                 |    online: forward,   |                 |
 |                 |    cache, rewrite     |                 |
 |                 |    offline: answer    |                 |
 |                 |    from local data    |                 |
 |                 |  other hosts:         | --- tunnel ---------->  rest of the
 |                 |    tunnel untouched   |                 |       internet
 |                 +-----------------------+                 |
 |                        |          ^                       |
 |                        v          |                       |
 |       +------------------+   +-----------------------+    |
 |       | store.sqlite,    |   | window and tray icon  |    |
 |       | media/, lyrics/, |   | (settings, library,   |    |
 |       | artwork/         |   |  downloads)           |    |
 |       +------------------+   +-----------------------+    |
 +-----------------------------------------------------------+
```

## The proxy

Cider is configured to use the proxy for HTTP and HTTPS. For most hosts the proxy only opens a tunnel and passes the connection through untouched. For Apple Music hosts (`music.apple.com`, `itunes.apple.com` and `mzstatic.com`, plus `blobstore.apple.com` while offline) it terminates TLS with a certificate signed by its local certificate authority, which is why Cider has to trust that authority. In Passthrough mode it doesn't do even that: everything is a tunnel.

Each intercepted request runs through a fixed chain of steps:

1. Token capture: the Apple Music developer token and user token Cider sends are saved, so the app can sync the library and download songs on its own later. Token values are never logged.
2. Library injection: songs, albums and playlists you created in the app are merged into Apple's answers (library lists, lookups, search), so Cider shows them like any other item.
3. Response cache: successful `GET` answers of the Apple Music API and artwork are stored. Sign-in, token and playback endpoints are never cached. The song metadata in those answers also feeds the local library.
4. Lyrics: Apple's lyrics are saved as they pass. Your own lyrics, or saved lyrics with "Always use downloaded lyrics" on, are answered without asking Apple. Songs that have local lyrics are marked as having lyrics even where Apple says they have none.
5. Local playback: when Cider asks to play a song that has a local file, the answer points Cider at a media URL that the proxy serves itself, with seeking support. It sits on Apple's playback host, so it needs no extra port or certificate, and the request never leaves your computer. Which mix plays follows what Cider asks for: the `webPlayback` request is the stereo mix, and the song's enhanced HLS stream (which Cider uses for Atmos songs when its spatial audio is on) is the spatial mix. A downloaded spatial mix is offered to Cider as a local HLS playlist in place of Apple's enhanced HLS URL. When Cider will take the spatial path but only the stereo mix is downloaded, the proxy hands Cider Apple's real `webPlayback` answer, so Cider streams that mix from Apple, unless "Play the other downloaded mix" is on or Apple is unreachable (then Cider is kept on the local stereo file).
6. Offline responder: when Apple Music is unreachable, it answers from the local data.

### Online

Online, requests go to Apple, and the steps above watch and adjust the answers on the way back. A separate library sync uses the captured tokens to fetch your complete library from the Apple Music API, because what Cider happens to request is never the whole library (first pages, partial lists). The sync replaces the stored library in one transaction, and only if every request succeeded.

Apple names one song by several ids: its catalog id (`songs`), the id of your library copy (`i.…`) and, inside a library playlist, another library id (`a.…`). The database keeps one `tracks` row per song, keyed on its catalog id when one is known, and every other id in `track_aliases` together with the resource Apple returned under that id (the offline responder needs the library resources to answer library requests). Triggers keep song ids and aliases apart, so an id always names exactly one song. Audio, lyrics state, download pins and links to local files belong to the song. Collections keep their members as Apple listed them, and each member id is resolved to its song when read. When a library id later learns its catalog id, its song is re-keyed to that id, or merged into the catalog song if that is stored already.

### Offline

A connectivity monitor checks Apple Music every 15 seconds. When it can't be reached, or a forwarded request fails, the offline responder answers instead:

1. artwork from the stored images, in whatever size and format Cider asks for;
2. the API endpoints the local library can answer (library lists and lookups, collection tracks, search, catalog lookups for local songs), built from the synced library and honouring Cider's paging and `include` parameters;
3. other requests from an exact match in the response cache;
4. anything else with an empty but valid document, so Cider's pages still render.

Songs without local audio keep their place in lists but lose their play parameters, so Cider shows them as unavailable instead of failing in the middle of a queue.

## Downloads

Downloading is separate from the proxy. A queue in the core crate decides what to fetch, how many downloads run at once, when to retry and how long to wait; it is the only place that tracks download state. It hands each song to a downloader.

The downloader in this project fetches a song the way Cider's web player does when it plays it: it asks Apple's playback service for the song's stream with your Apple Music session and saves the audio as an AAC `.m4a` file in the `media` folder. Apple turns playback requests away when too many arrive at once, which is why the queue spaces downloads out and retries temporary failures.

Songs mastered in Dolby Atmos also have a spatial mix: the Atmos mix rendered to headphone stereo, which Cider plays when its spatial audio setting is on. Depending on the download format setting, the queue also (or instead) fetches that mix. It looks the song up in the Apple Music catalog for its enhanced HLS playlist, and the downloader takes the binaural stream from it. That stream takes two playback requests, which are sent a moment apart. The file is saved as `<id>.spatial.m4a` next to the stereo one. Every file is written to a temporary file first and moved into place once complete, so a re-download replaces an existing file only when the new one is finished.

Apple's streams declare no length in their headers, because a streaming player learns it from the playlist. The downloader writes the real duration into the saved file, so players show the full length without reading the whole file first.

After a download, the queue records each file (with its size and duration) in the library and, if enabled, fetches lyrics.

The queue holds one entry per song, covering every mix the format wants, and runs at most one worker per entry. A new request for a song that is already queued, running or paused merges into that entry (a re-download marks it to replace the existing files). When a download is paused or cancelled, the next one for that song waits until the old one has fully stopped, and a worker that was stopped can no longer change the entry's state. A plain download skips the mixes already on disk.

Every download is pinned to whoever asked for it: an album or playlist, or the song itself. A file is kept while any pin wants it, so removing one collection's downloads deletes only the songs no other download needs. Collections downloaded from the library are remembered with the view they came from, so a pin whose song has since left the collection (or whose collection was deleted) stops counting; files left without a pin are the "unlinked downloads" the app can clean up. Pins decide which files are kept. Whether an album or playlist shows as complete (or partly downloaded) comes from its songs' files on disk under the current download format, never from queue entries; its own pins only tell a collection you downloaded (it pins every song) from one whose songs are all on disk through other downloads. Downloading or linking such a collection pins every one of its songs to it, including those already on disk, without fetching them again. "Keep downloaded" collections get their newly added songs queued after each library sync.

## The crates

The code is a Cargo workspace:

| Crate | Folder | What it does |
| --- | --- | --- |
| `cider-offline` | `crates/app` | The desktop app: Slint window, tray icon, settings, Cider integration, cleanup, and the command-line checks. Owns the tokio runtime. |
| `cider-offline-core` | `crates/core` | The proxy engine: interception, the interceptor chain, offline responder, storage (SQLite), library sync, lyrics providers, local media, and the download queue with its `Downloader` trait. |
| `cider-offline-trust` | `crates/trust` | Creates the local certificate authority and installs it into or removes it from the system trust stores. |
| `cider-offline-download` | `crates/download` | The Apple Music downloader that plugs into the core's `Downloader` trait. |
| `icon-gen` | `packaging/icons/gen` | A small tool that renders the SVG icon sources into the PNG and ICO files the build uses. Not a workspace member. |

The dependencies run in one direction: the app uses core, trust and download; download uses core; core uses trust. None of the libraries knows about the UI.

The split keeps each piece testable on its own. The core runs without a window, so it can be tested headless and driven by the command-line checks. The trust crate holds all the platform-specific certificate handling (Windows certificate store, the macOS `security` tool, `certutil` and `pkexec` on Linux) behind one interface. The downloader is kept out of the core on purpose: the core holds only the trait any downloader implements, plus a built-in one that imports a local file. The icon generator pulls in an SVG renderer, which normal builds don't need, so it lives outside the workspace and the generated icons are committed.

## The app

The main thread runs the Slint event loop (macOS requires the UI on the main thread), and a multi-threaded tokio runtime on background threads runs the proxy and all storage, network and file work. The two sides talk through a command handle and a stream of status events, without shared mutable state.

A lock file in the data folder (`instance.lock`) makes sure only one copy runs per data folder. A second launch connects to the running copy over a local socket, asks it to show its window, and exits.
