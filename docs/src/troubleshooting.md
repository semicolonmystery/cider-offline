# Troubleshooting

Most problems show up as a line at the bottom of the Overview page, which links to the Settings section where they are fixed: the proxy status, the certificate, the Cider setting or the Apple Music session. For anything else, look in the [logs](#logs).

## Cider shows network errors or loads nothing

In Active mode Cider only accepts the proxy's connections if the certificate is trusted. If the Certificate row says "Cider will reject the proxy's HTTPS traffic until the certificate is trusted", click **Install**, confirm the system prompt, and restart Cider.

On Linux:

- If the row reports that `certutil` was not found, install the NSS tools (`libnss3-tools` on Debian and Ubuntu, `nss-tools` on Fedora, `nss` on Arch) and click **Install** again.
- If you installed Cider as a Flatpak or Snap after trusting the certificate, its certificate database doesn't have it yet. Click **Check again**, then **Install**.

If the app stays in **Passthrough** after it starts, the certificate isn't trusted: the app only switches to Active on its own once it is. Install it as above and the app switches to Active.

To check whether the proxy is the cause at all, switch to **Passthrough**. If Cider works then, the problem is the certificate or the cache, not the connection.

## The proxy doesn't start

The Overview page says "Could not start the proxy: …" at the bottom, and Settings > Proxy says "Proxy stopped". The most common cause is "could not listen on 127.0.0.1:8000", which means another program already uses that port. Pick a different port under **Settings > Proxy > Port**, then point Cider at the new port (on Windows, turn **Route Cider through this proxy** off and on again).

## Requests seen stays at 0

Cider isn't sending its traffic through the proxy.

- Windows: check that **Route Cider through this proxy** is on. Cider reads the setting only when it starts, so restart Cider if you changed `client-options.yml` yourself.
- macOS and Linux: the `--proxy-server` option applies only to a Cider started with it. Quit Cider completely and start it with the command shown in Settings > Cider.
- If you changed the port, Cider must use the new one.

## Windows: Cider's settings file was not found

The app looks for `%APPDATA%\sh.cider.dotnet\client-options.yml`. If your Cider keeps it elsewhere, click **Choose config file…** and pick it. "Cider did not close in time" means Cider didn't exit when asked; close it yourself and turn the switch on again.

## The library is empty or out of date

The library sync needs the Apple Music session tokens that the proxy picks up from Cider. Use Cider online through the proxy (open your library or play a song), then choose **Refresh library** in the Library page's menu. The sync doesn't run while offline, and a sync that fails part way through changes nothing, so the old library stays until a full sync succeeds.

## Songs are greyed out offline

Offline, only songs with local audio can play: downloaded ones and ones linked to a local file. Use the Library page's "missing audio" filter to find the others, and download them while you're online.

## Downloads fail

- "Apple Music sign-in expired: play a song in Cider while online, then retry": Apple no longer accepts the session the proxy captured. Play something in Cider while online, then retry the download.
- "Can't download yet": Apple's answer for this song doesn't include audio the downloader can handle. It isn't retried automatically.
- Many downloads failing at once: lower **Settings > Downloads > Simultaneous downloads** to 2 or 1. Apple turns requests away when too many arrive together.
- Downloads are turned off and the message says no downloader is registered: a `device.wvd` file in the data folder can't be used. Remove it and restart the app.

Temporary failures (rate limits, server and network errors) are retried on their own; the row shows when the next attempt is.

## A downloaded song streams from Apple while online

If Cider's spatial audio (Dolby Atmos) setting is on, Cider asks for the spatial mix of an Atmos song. When you downloaded only the stereo mix, Cider streams the spatial mix from Apple while you're online; offline it plays your download. To play your download online too, turn on **Settings > Downloads > Play the other downloaded mix**, or download the spatial mix as well (**Download format**, then **Download missing songs** or **Re-download…**). See [Stereo and spatial mixes](using.md#stereo-and-spatial-mixes).

## Lyrics are missing

Fetch them with **Fetch lyrics** on the song, or **Fetch all missing lyrics** in the Library menu. If a provider keeps failing with rate limits, raise the LRCLIB spacing or choose **Wait and retry** under **Settings > Lyrics**. Lyrics you chose from a file always take priority over everything else.

## Starting the app again does nothing

Only one copy runs at a time, and starting it again shows the window of the running copy. If you don't see it, look for the tray icon. On macOS, opening the app again from Finder or Spotlight doesn't show the window; use **Open window** in the menu bar icon.

On Linux without a tray host (GNOME without the AppIndicator extension), the app shows a hint about it, and closing the window quits the app.

## macOS: the portable copy uses my normal folders

The Overview page says macOS is running the copy from a temporary read-only location. Quit the app, run `xattr -dr com.apple.quarantine` on the unzipped folder, and open it again. See [Installation](installation.md#portable-build-on-macos).

## Going back to an older version

0.1.1 upgrades the library database the first time it starts. 0.1.0 doesn't know the new layout and doesn't work with the upgraded database, so install 0.1.1 or newer again. From 0.1.1 on, an app that finds a database from a newer version refuses to open it and logs "the database has schema version …, but this version of the app supports up to …; update the app", without changing anything.

## My settings were reset

If `config.toml` has a syntax error, the app starts with the defaults and logs `invalid config; using defaults`. Fix the file (or delete it) with the app closed. See [Configuration](configuration.md).

## Checking offline behaviour

To see what works offline without disconnecting, quit the app, set `force_offline = true` under `[connectivity]` in `config.toml`, and start it again. Set it back to `false` when you're done.

## Logs

The app logs to a daily file in the `logs` folder of its cache folder, and keeps the last seven files:

- Windows: `%LOCALAPPDATA%\cider-offline\CiderOffline\cache\logs`
- macOS: `~/Library/Caches/sh.cider-offline.CiderOffline/logs`
- Linux: `~/.cache/cideroffline/logs`
- Portable: `data/cache/logs` next to the program

The default level is `info` (with the proxy library's own messages at `warn`). For more detail, set `RUST_LOG` before starting the app, for example `RUST_LOG=info,cider_offline_core=debug`. The targets are `cider_offline` (the app), `cider_offline_core` (proxy, library, lyrics, downloads queue), `cider_offline_trust` (certificate) and `cider_offline_download` (the downloader).

The app never logs token values, but logs can contain song titles, URLs and file paths. When you share logs in a bug report, paste only the relevant lines and remove anything that looks like a token, a cookie or an `Authorization` header. If the problem is in what Cider receives, a capture (Settings > Storage > **Capture Apple traffic to disk**) helps too; check it the same way before sharing.

Bugs go to the [issue tracker](https://github.com/semicolonmystery/cider-offline/issues). Report security problems privately through the repository's **Security** tab instead.
