# Files and folders

The app uses three folders: a config folder for settings, a data folder for your library and downloads, and a cache folder for things it can fetch again. Config and data are kept apart so settings can roam with your user profile while the large files stay on this computer. Everything in the cache folder can be deleted at any time.

## Where they are

| System | Config | Data | Cache |
| --- | --- | --- | --- |
| Windows | `%APPDATA%\cider-offline\CiderOffline\config` | `%LOCALAPPDATA%\cider-offline\CiderOffline\data` | `%LOCALAPPDATA%\cider-offline\CiderOffline\cache` |
| macOS | `~/Library/Application Support/sh.cider-offline.CiderOffline` | same folder as config | `~/Library/Caches/sh.cider-offline.CiderOffline` |
| Linux | `~/.config/cideroffline` | `~/.local/share/cideroffline` | `~/.cache/cideroffline` |

On Linux the `XDG_CONFIG_HOME`, `XDG_DATA_HOME` and `XDG_CACHE_HOME` variables move these folders if they are set. On macOS the config and data folders are the same, so `config.toml` sits next to `store.sqlite`.

**Settings > Storage > Data folder** opens the data folder.

## What's in them

| Folder | Contents |
| --- | --- |
| Config | `config.toml` (see [Configuration](configuration.md)) |
| Data | `store.sqlite` (the library database, with its `-wal` and `-shm` files while the app runs), `media/` (downloaded audio), `lyrics/` (saved lyrics), `ca/` (the local certificate and its private key), `captures/` (only if capture was turned on), `instance.lock` (makes sure only one copy runs), and `device.wvd` if you added one |
| Cache | `artwork/` (cover art), `logs/` (log files) |

The database stores the paths of files in the data folder relative to it (for example `media/123.m4a`), so you can move the data folder, or plug a portable drive in under a different drive letter, without breaking the library. Files you picked from elsewhere, such as a linked audio file, are stored with their full path and stay where they are.

## Portable mode

If a folder named `data` sits next to the program, the app runs in portable mode and keeps everything inside that folder:

| What | Where |
| --- | --- |
| Config | `data/config/config.toml` |
| Data | directly in `data/` |
| Cache | `data/cache/` (so logs are in `data/cache/logs/`) |

"Next to the program" means:

- Windows: the folder that contains `cider-offline.exe`;
- macOS: the folder that contains `Cider Offline.app`;
- Linux: the folder that contains the AppImage.

The portable archives come with an empty `data` folder (holding a `.keep` file so unzip tools don't drop it). The installers never create one. To switch a copy back to the normal folders, remove its `data` folder.

On macOS a quarantined app is run from a hidden read-only location where `data` can't be found; see [Installation](installation.md#portable-build-on-macos).

## Custom Widevine device

A file named `device.wvd` in the data folder replaces the Widevine device built into the app:

- Windows: `%LOCALAPPDATA%\cider-offline\CiderOffline\data\device.wvd`
- macOS: `~/Library/Application Support/sh.cider-offline.CiderOffline/device.wvd`
- Linux: `~/.local/share/cideroffline/device.wvd`
- Portable: `data/device.wvd` next to the program

The app reads it when it starts, so restart the app after adding or replacing it.

## Logs

The app writes one log file per day, `cider-offline.<date>.log`, to the `logs` folder in the cache folder, and keeps the last seven:

- Windows: `%LOCALAPPDATA%\cider-offline\CiderOffline\cache\logs`
- macOS: `~/Library/Caches/sh.cider-offline.CiderOffline/logs`
- Linux: `~/.cache/cideroffline/logs`
- Portable: `data/cache/logs`

## Outside these folders

A few things live elsewhere on your system:

- the trusted certificate, in your system's certificate store (see [Installation](installation.md#why-its-needed));
- the start-at-login entry, if you turned it on;
- on Windows, the two proxy lines in Cider's `client-options.yml`, if you used **Route Cider through this proxy**;
- text reports in your temporary folder: `cider-offline-cleanup-*.txt` from a cleanup run that was a dry run or had a failed step, and `cider-offline-removal-report-*.txt` when **Remove all data** couldn't delete something.

[Uninstalling](uninstalling.md) explains how to remove all of it.
