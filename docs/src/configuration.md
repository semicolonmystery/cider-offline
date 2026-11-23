# Configuration

The app keeps its settings in a TOML file, `config.toml`:

| Install | Location |
| --- | --- |
| Windows | `%APPDATA%\cider-offline\CiderOffline\config\config.toml` |
| macOS | `~/Library/Application Support/sh.cider-offline.CiderOffline/config.toml` |
| Linux | `~/.config/cideroffline/config.toml` (or `$XDG_CONFIG_HOME/cideroffline/config.toml`) |
| Portable | `data/config/config.toml` next to the program |

Most options are on the Settings page, and the app writes the file whenever you change one there. Without the file, every option has its default.

If you edit the file by hand, quit the app first, or it may overwrite your changes with its own copy. Missing keys take their default and unknown keys are ignored. If the file isn't valid TOML, or holds a value the app can't use (such as `port = 0`), the app logs `invalid config; using defaults` together with the reason and starts with the defaults. It leaves the broken file alone at startup, but the next change you make on the Settings page saves the defaults over it, so fix it first.

## Top-level options

| Key | Type | Default | Effect |
| --- | --- | --- | --- |
| `mode` | `"active"` or `"passthrough"` | `"active"` | The last mode you picked. The app writes it but no longer reads it at startup: it always starts in Passthrough and switches to Active once the certificate is trusted (see [Overview](using.md#overview)). |
| `capture` | bool | `false` | Write Apple Music requests and responses to the `captures` folder. |
| `auto_cache` | `"off"`, `"instant"` or `"deferred"` | `"off"` | Download songs as they play: never, right away, or once playback has been idle for 30 seconds. |
| `detail_level` | `"full"` or `"minimal"` | `"full"` | How much metadata is stored for each song. `minimal` keeps only id, title, artist, album, duration and artwork. |
| `download_concurrency` | integer | `0` | Downloads that run at once, 1 to 6. `0` means the default, 2. Larger values are capped at 6. |
| `download_format` | `"stereo"`, `"spatial_where_available"` or `"both"` | `"stereo"` | Which mix of a song a download saves (**Settings > Downloads > Download format**). `stereo` is the stereo mix Cider plays by default. The spatial mix is the Dolby Atmos mix rendered for headphones (binaural stereo), which only songs mastered in Atmos have. `spatial_where_available` saves the spatial mix when the song has one and the stereo mix otherwise; `both` saves the stereo mix plus the spatial one when it exists. Each extra mix is a download of its own (about the same size). |
| `play_other_downloaded_mix` | bool | `false` | **Play the other downloaded mix** (Settings > Downloads): when Cider asks for a mix you haven't downloaded (stereo or spatial), play the one you did download instead of streaming from Apple. Offline, this always happens. See [Stereo and spatial mixes](using.md#stereo-and-spatial-mixes). |
| `remove_lyrics_with_downloads` | bool | `false` | **Remove lyrics when removing downloads** (Settings > Downloads): **Remove download** on a song, album or playlist also deletes the lyrics the app fetched for the songs whose files are deleted. Lyrics you imported (custom lyrics) are never removed this way. See [Removing lyrics](using.md#removing-lyrics). |
| `unlinked_downloads` | `"manual"` or `"automatic"` | `"manual"` | **Unlinked downloads** (Settings > Downloads): downloaded songs no album, playlist or single download needs any more. `manual` keeps them until **Clean up now…**; `automatic` deletes them every `unlinked_cleanup_hours` hours. Custom audio and links to local files are never touched. See [Shared songs and unlinked downloads](using.md#shared-songs-and-unlinked-downloads). |
| `unlinked_cleanup_hours` | integer | `0` | Hours between automatic clean-ups, 1 to 168. `0` means the default, 6. Larger values are capped at 168 (a week). |
| `overwrite_audio` | bool | `false` | Replace audio a song already has when downloading or importing, instead of skipping it. |
| `overwrite_lyrics` | bool | `false` | When fetching lyrics for a collection or after a download, look up songs that already have lyrics again. Apple lyrics are always kept. |
| `start_with_os` | bool | `false` | Start the app when you sign in. |
| `start_minimized` | bool | `false` | Start hidden in the tray (minimized when there is no tray). |
| `cider_config_path` | path | not set | Cider's `client-options.yml`, when you picked it with **Choose config file…** (Windows). |
| `tray_hint_dismissed` | bool | `false` | The one-time "no system tray" hint on Linux was dismissed. |

## `[proxy]`

| Key | Type | Default | Effect |
| --- | --- | --- | --- |
| `bind_address` | IP address | `"127.0.0.1"` | Address the proxy listens on. The default keeps it reachable from this computer only. |
| `port` | integer | `8000` | TCP port, 1 to 65535, the same range the Settings page accepts. `0` is not allowed, since Cider has to be pointed at a fixed address: a file with `port = 0` counts as invalid (see above). |

## `[connectivity]`

The connectivity monitor decides whether Apple Music is reachable.

| Key | Type | Default | Effect |
| --- | --- | --- | --- |
| `enabled` | bool | `true` | Run the monitor. When off, the proxy assumes it is online (a failed request still gets an offline answer). |
| `probe_url` | string | `"https://amp-api.music.apple.com/"` | URL checked with a `HEAD` request. Any HTTP response counts as online. |
| `interval_secs` | integer | `15` | Seconds between checks (at least 1). |
| `timeout_secs` | integer | `5` | Timeout of one check in seconds (at least 1). |
| `force_offline` | bool | `false` | Treat Apple Music as unreachable, so every request is answered from your computer. Useful to see what works offline. |

## `[library]`

| Key | Type | Default | Effect |
| --- | --- | --- | --- |
| `songs_page` | `"apple"` or `"all"` | `"apple"` | What Cider's Library > Songs page lists. `apple`: only the songs you added to your library, like Apple Music. `all`: every song of your library, including those in library playlists and albums. |

## `[lyrics]`

| Key | Type | Default | Effect |
| --- | --- | --- | --- |
| `providers` | array of `{ id, enabled }` | all seven, enabled | Lyrics providers in the order they are asked. |
| `on_failure` | `"skip_and_retry_later"`, `"wait_and_retry"` or `"skip"` | `"skip_and_retry_later"` | What happens when a provider fails (rate limit, server error, timeout, network error). |
| `policy_providers` | array of provider ids | all seven | Providers `on_failure` applies to. The others always wait and retry. |
| `target_quality` | `"word"`, `"line"` or `"plain"` | `"word"` | Stop asking providers once lyrics this good are found; otherwise keep the best found. |
| `fetch_after_download` | bool | `true` | Look up lyrics for each song once its download finishes. |
| `lrclib_spacing_ms` | integer | `250` | Milliseconds between two LRCLIB lookups (at most 10000). |
| `retry_passes` | integer | `3` | Retry passes over skipped songs with `skip_and_retry_later` (1 to 10). |
| `always_use_downloaded` | bool | `false` | While online, answer Cider's lyrics requests with saved lyrics when a song has any, without asking Apple. |

The provider ids, in the default order, are `apple`, `community`, `amll`, `musixmatch`, `netease`, `kugou` and `lrclib`. A provider missing from `providers` is added at the end, enabled.

`wait_and_retry` waits 2, 5, 15 and then 30 seconds between attempts, or as long as the provider's `Retry-After` header asks. `skip_and_retry_later` waits longer before each pass (10 s, 30 s, 90 s and so on).

## Example

```toml
mode = "active"
capture = false
auto_cache = "deferred"
detail_level = "full"
download_concurrency = 2
download_format = "stereo"
play_other_downloaded_mix = false
remove_lyrics_with_downloads = false
unlinked_downloads = "manual"
unlinked_cleanup_hours = 6
overwrite_audio = false
overwrite_lyrics = false
start_with_os = true
start_minimized = true
tray_hint_dismissed = false

[proxy]
bind_address = "127.0.0.1"
port = 8000

[connectivity]
enabled = true
probe_url = "https://amp-api.music.apple.com/"
interval_secs = 15
timeout_secs = 5
force_offline = false

[library]
songs_page = "apple"

[lyrics]
on_failure = "skip_and_retry_later"
policy_providers = ["apple", "community", "amll", "musixmatch", "netease", "kugou", "lrclib"]
target_quality = "word"
fetch_after_download = true
lrclib_spacing_ms = 250
retry_passes = 3
always_use_downloaded = false

[[lyrics.providers]]
id = "apple"
enabled = true

[[lyrics.providers]]
id = "lrclib"
enabled = true

[[lyrics.providers]]
id = "musixmatch"
enabled = false
```

In this example LRCLIB is asked right after Apple, Musixmatch is off, and the providers not listed (`community`, `amll`, `netease`, `kugou`) are added at the end, enabled.

## Command-line options

| Option | Effect |
| --- | --- |
| `--version` | Print `cider-offline <version>` and exit. |
| `--minimized` | Start in the tray, like `start_minimized`. The login entry passes it. |
| `--remove-all-data` | Undo the system changes and delete the app's files, then exit. See [Uninstalling](uninstalling.md#command-line-cleanup). |
| `--uninstall-cleanup` | Undo the system changes only, then exit. |
| `--dry-run` | With one of the two above: print what would be done and change nothing. |

On Windows the release build has no console window, so `--version` prints nothing you can see there. The app also has a few checks meant for development; they are listed in [Development](development.md#headless-checks).

Logging is controlled with the `RUST_LOG` environment variable; see [Troubleshooting](troubleshooting.md#logs).
