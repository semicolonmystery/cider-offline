# Using the app

The window has three pages: **Overview**, **Library** and **Settings**. Closing the window doesn't stop the proxy; the app keeps running in the system tray.

## Overview

![The Overview page](images/overview.png)

The large switch at the top picks the mode:

- **Active** (the default): Apple Music traffic is read, cached and, when Apple Music is unreachable, answered from your computer.
- **Passthrough**: every connection is forwarded untouched. Nothing is decrypted or cached, and offline mode does nothing.

Below it you see whether the proxy is online or offline, its address (with a **Copy** button), how many requests it has seen and how many it answered itself, the certificate and Cider rows from [setup](connecting-cider.md), and a short activity list.

**Capture Apple traffic to disk** writes every Apple Music request and response to the capture folder as JSON. It is meant for troubleshooting. The authorization, cookie and Apple Music user token headers are replaced with `<redacted>`, but the bodies are stored as they are, so look through the files before you share them.

## Offline mode

While you are online, the proxy checks every 15 seconds whether Apple Music answers (a `HEAD` request to `https://amp-api.music.apple.com/`; any HTTP answer counts as online). When the check fails, or a request to Apple fails, the proxy switches to offline mode and answers Cider itself:

- your library playlists, albums and songs, in the same order as in Apple Music;
- search across your local songs, albums and playlists, accent-insensitive;
- artwork it has stored, in whatever size Cider asks for;
- lyrics it has saved;
- playback of every song you have downloaded or linked to a local file.

Songs without local audio keep their place in the lists, but Cider shows them as unavailable and skips them when it builds a queue, so playback doesn't stop halfway through an album. Requests the proxy can't answer get an empty but valid response, so Cider's pages still load.

When Apple Music is reachable again, the proxy goes back to forwarding and the activity list says so. To try offline mode without disconnecting, set `force_offline = true` in the `[connectivity]` section of the [config](configuration.md).

## Library sync

The proxy needs your Apple Music session to sync anything. It picks up the tokens from Cider's own requests, so use Cider once while online with the proxy connected.

After that it fetches your whole library from Apple Music: every library playlist and album with its complete track list, and your Songs tab. It syncs when the app starts, every 30 minutes, when it sees new tokens, when Apple Music becomes reachable again, and when you choose **Refresh library** in the Library page's menu. It doesn't sync while offline. A sync is saved only when every request succeeded, so a failed or interrupted sync never leaves you with half a library.

### Songs page

**Settings > Library > Songs page** decides what Cider's **Library > Songs** page lists, online and offline:

- **Added to library** (the default): only the songs you added to your library yourself, the same list Apple Music shows.
- **Every song**: every song in your library, including every song of your library playlists and albums. The proxy answers this list itself, online too.

Only that one listing changes. Playlists, albums, search and playback are the same either way.

## The Library page

![The Library page](images/library.png)

The Library page shows your synced albums and playlists, and every song in them. Use the search button to search it, and the filters to show only songs missing audio or missing lyrics. Open an album or playlist to see its songs.

Each song row has a download control and a menu with:

- **Download**, **Pause download**, **Resume download** or **Retry download**, depending on its state;
- **Link to local file…** to play your own audio file for this song, or **Unlink local file** to stop;
- **Fetch lyrics** and **Choose lyrics file…**;
- **Remove download…** to delete the downloaded audio.

An album or playlist has **Download**, **Fetch lyrics for all songs** and **Remove download…** for all of its songs. The Library menu has **Refresh library**, **Download all missing audio** and **Fetch all missing lyrics**.

## Downloads

A download asks Apple Music for the song the same way Cider does when it plays it, gets a Widevine license, downloads the encrypted audio and decrypts it on your computer, using a Widevine device that ships with the app. The result is a plain AAC `.m4a` file (256 kbps when Apple offers it, otherwise 64 kbps) in the `media` folder of the app's data folder. After a download finishes, the app looks up lyrics for the song (you can turn that off).

The download needs the Apple Music session the proxy captured from Cider. If Apple says the session ended, the song fails with "Apple Music sign-in expired: play a song in Cider while online, then retry". Do what it says.

The **Downloads** panel lists the queue. You can pause and resume everything, retry the failed downloads, clear the finished ones, and add any song, album or playlist by pasting its Apple Music link or ID. Failures that are likely temporary (Apple rate limits, server errors, network errors) are retried automatically with growing pauses. A song marked "Can't download yet" can't be downloaded this way and isn't retried until you ask.

For music videos, the app downloads the audio. A music video also offers to download the song from the album instead; its edit can differ from the video's.

Settings that affect downloads, under **Settings > Library**:

- **Download played songs**: **Off** (the default), **Right away** (download every song as you play it), or **When idle** (queue them, and download once nothing has played for 30 seconds).
- **Simultaneous downloads**: 1 to 6, default 2. Apple refuses licenses when too many are requested at once, so 2 is the most reliable.
- **Overwrite audio**: download or import again over audio a song already has, instead of skipping it.
- **Detail level**: **Full** keeps every attribute Apple sends for a song, which makes the library, search and lyrics richer offline. **Minimal** keeps only the id, title, artist, album, duration and artwork.

### Using your own Widevine device

To use a different Widevine device, put it in the app's data folder as `device.wvd` (see [Files and folders](files.md#custom-widevine-device)) and restart the app. If the file can't be read, the app falls back to the built-in device. If it can be read but isn't a valid device file, downloads are turned off until you fix or remove it.

## Lyrics

When Cider fetches lyrics from Apple while online, the proxy saves them. Offline it serves the saved lyrics. For songs Apple has no lyrics for, you can fetch them from other providers. These are asked in this order by default:

1. Apple Music
2. Cider Community (lyrics submitted by Cider users)
3. AMLL TTML DB
4. Musixmatch (through Cider's lyrics service)
5. NetEase Cloud Music
6. Kugou
7. LRCLIB

Under **Settings > Lyrics** you can turn providers off and change the order. The other options there:

- **Search until**: providers are asked in order until lyrics of this quality turn up: **Word-synced (best)**, **Line-synced** or **Any lyrics**. If none do, the best lyrics found are kept.
- **When a lookup fails** (rate limit, server error, timeout): **Skip, retry at the end** asks again for the skipped songs once the rest are done; **Wait and retry** waits and asks the same provider again before moving on; **Skip** leaves the song without lyrics until you fetch again. "No lyrics found" is not a failure.
- **Always use downloaded lyrics**: while online, answer Cider's lyrics requests with the saved lyrics instead of asking Apple. Songs without saved lyrics still get Apple's.
- **Fetch after downloading**: look up lyrics for every song once its download finishes.
- **Overwrite lyrics**: when fetching for a whole album or playlist, or after a download, look up songs that already have lyrics again. Apple lyrics are always kept. Fetching lyrics for a single song always looks them up.

**More options** has the providers the failure rule applies to (the others always wait and retry), the pause between two LRCLIB lookups, and how many retry passes run at the end.

## Your own audio and lyrics

**Link to local file…** in a song's menu plays your own file for that song instead of Apple's stream. Supported types are `m4a`, `mp3`, `flac`, `aac`, `alac`, `wav`, `ogg` and `opus`. The file stays where it is; the app only remembers its path.

**Choose lyrics file…** sets lyrics from a TTML or LRC file. Lyrics you choose this way always win: Cider gets them online and offline, whatever the other lyrics settings say.

The add button on the Library page (**Add a song, album or playlist**) adds a song, album or playlist of your own. A song can have a title, artist, album, artwork, a release date, a description and more, an audio file or the audio of another Apple Music song (by its ID), and a lyrics file. It can go into one of your albums or playlists, or into a new album. These items show up in Cider like the rest of your library: in the library lists, in search and in playback, online and offline. Your own items have **Edit details…** and **Delete…** in their menu, and songs in your own playlists can be moved up and down.

## Start at login and the tray

**Settings > Startup** has **Start with the system**, which launches the app when you sign in, and **Start minimized**, which starts it hidden in the tray. The login entry always starts the app minimized.

The tray menu (the menu bar on macOS) shows the proxy's status and has **Passthrough** and **Capture** switches, **Open window** and **Quit**. On Windows and Linux, clicking the icon opens the window. The icon changes with the mode.

Closing the window hides it to the tray. **Quit** stops the proxy and exits.

On Linux the tray needs a StatusNotifierItem host. KDE has one; on GNOME install the "AppIndicator and KStatusNotifierItem Support" extension. Without one, the app shows a one-time hint, the minimize button minimizes, the close button quits, and "start minimized" starts minimized instead of hidden. If the tray goes away while the window is hidden (for example when the panel restarts), the window comes back.

Only one copy of the app runs at a time. Starting it again brings up the window of the one that is running. On macOS, opening the app again from Finder, Launchpad or Spotlight doesn't do this; use **Open window** in the menu bar icon. An installed copy and a portable copy have separate data, so they can run side by side.

## Settings

![The Settings page](images/settings.png)

Most of the Settings page is described above. The rest:

- **Proxy > Port**: the port the proxy listens on. Changing it restarts the proxy; point Cider at the new port too.
- **Storage**: open the data folder or the capture folder; **Clear cache** deletes cached Apple Music responses and artwork, and keeps your downloaded audio, linked files and your own songs; **Uninstall certificate** removes the local certificate from your system (Cider then stops working through the proxy in Active mode); **Remove all data…** is described in [Uninstalling](uninstalling.md#remove-all-data).

Settings are saved to `config.toml`. [Configuration](configuration.md) lists every option, including a few that are only in the file.
