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

Cider is configured to use the proxy for HTTP and HTTPS. For most hosts the proxy only opens a tunnel and passes the encrypted bytes through. For Apple Music hosts (`music.apple.com`, `itunes.apple.com` and `mzstatic.com`, plus `blobstore.apple.com` while offline) it terminates TLS with a certificate signed by its local certificate authority, which is why Cider has to trust that authority. In Passthrough mode it doesn't do even that: everything is a tunnel.

Each intercepted request runs through a fixed chain of steps:

1. Token capture: the Apple Music developer token and user token Cider sends are saved, so the app can sync the library and download songs on its own later. Token values are never logged.
2. Library injection: songs, albums and playlists you created in the app are merged into Apple's answers (library lists, lookups, search), so Cider shows them like any other item.
3. Response cache: successful `GET` answers of the Apple Music API and artwork are stored. Auth, token, license and playback endpoints are never cached. The song metadata in those answers also feeds the local library.
4. Lyrics: Apple's lyrics are saved as they pass. Your own lyrics, or saved lyrics with "Always use downloaded lyrics" on, are answered without asking Apple. Songs that have local lyrics are marked as having lyrics even where Apple says they have none.
5. Local playback: when Cider asks to play a song that has a local file, the answer points Cider at a media URL that the proxy serves itself, with seeking support. It sits on Apple's playback host, so it needs no extra port or certificate, and the request never leaves your computer.
6. Offline responder: when Apple Music is unreachable, it answers from the local data.

### Online

Online, requests go to Apple, and the steps above watch and adjust the answers on the way back. A separate library sync uses the captured tokens to fetch your complete library from the Apple Music API, because what Cider happens to request is never the whole library (first pages, partial lists). The sync replaces the stored library in one transaction, and only if every request succeeded.

### Offline

A connectivity monitor checks Apple Music every 15 seconds. When it can't be reached, or a forwarded request fails, the offline responder answers instead:

1. artwork from the stored images, in whatever size and format Cider asks for;
2. the API endpoints the local library can answer (library lists and lookups, collection tracks, search, catalog lookups for local songs), built from the synced library and honouring Cider's paging and `include` parameters;
3. other requests from an exact match in the response cache;
4. anything else with an empty but valid document, so Cider's pages still render.

Songs without local audio keep their place in lists but lose their play parameters, so Cider shows them as unavailable instead of failing in the middle of a queue.

## Downloads

Downloading is separate from the proxy. A queue in the core crate decides what to fetch, how many downloads run at once, when to retry and how long to wait; it is the only place that tracks download state. It hands each song to a downloader.

The downloader in this project does what Cider's web player does when it plays a song: it asks Apple's playback service for the song's stream, gets a Widevine license for it using a Widevine device that ships with the app (or your own `device.wvd`), downloads the encrypted audio, decrypts it on your computer and writes a plain AAC `.m4a` into the `media` folder. Each song needs its own license, and Apple refuses licenses when too many are requested at once, which is why the queue spaces downloads out and retries temporary failures.

After a download, the queue records the file in the library and, if enabled, fetches lyrics.

## The crates

The code is a Cargo workspace:

| Crate | Folder | What it does |
| --- | --- | --- |
| `cider-offline` | `crates/app` | The desktop app: Slint window, tray icon, settings, Cider integration, cleanup, and the command-line checks. Owns the tokio runtime. |
| `cider-offline-core` | `crates/core` | The proxy engine: interception, the interceptor chain, offline responder, storage (SQLite), library sync, lyrics providers, local media, and the download queue with its `Downloader` trait. |
| `cider-offline-trust` | `crates/trust` | Creates the local certificate authority and installs it into or removes it from the system trust stores. |
| `cider-offline-download` | `crates/download` | The Widevine downloader that plugs into the core's `Downloader` trait. |
| `icon-gen` | `packaging/icons/gen` | A small tool that renders the SVG icon sources into the PNG and ICO files the build uses. Not a workspace member. |

The dependencies run in one direction: the app uses core, trust and download; download uses core; core uses trust. None of the libraries knows about the UI.

The split keeps each piece testable on its own. The core runs without a window, so it can be tested headless and driven by the command-line checks. The trust crate holds all the platform-specific certificate handling (Windows certificate store, the macOS `security` tool, `certutil` and `pkexec` on Linux) behind one interface. The downloader is kept out of the core on purpose: the core contains no license or decryption code, only the trait any downloader implements, plus a built-in one that imports a local file. The icon generator pulls in an SVG renderer, which normal builds don't need, so it lives outside the workspace and the generated icons are committed.

## The app

The main thread runs the Slint event loop (macOS requires the UI on the main thread), and a multi-threaded tokio runtime on background threads runs the proxy and all storage, network and file work. The two sides talk through a command handle and a stream of status events, without shared mutable state.

A lock file in the data folder (`instance.lock`) makes sure only one copy runs per data folder. A second launch connects to the running copy over a local socket, asks it to show its window, and exits.
