# Uninstalling

Besides its own folders, the app may have changed three things on your system: it trusted its certificate, it may have added a start-at-login entry, and on Windows it may have turned on Cider's proxy setting. Deleting the program alone leaves those behind. The ways to remove the app below undo them as well.

## Windows installer

Uninstall the app from **Settings > Apps > Installed apps** (or **Control Panel > Programs**). The uninstaller has a checkbox, unticked by default: **Also remove my downloaded music and settings**.

- Unticked, the uninstaller removes the certificate, turns off start at login, turns off Cider's proxy setting if it points at this proxy, and removes the program. Your library, downloads and settings stay, so a later install picks them up again.
- Ticked, it also deletes the config, data and cache folders.

If the app is still running, the uninstaller offers to close it first. If the cleanup fails, it shows a message with the path of the removal script you can run instead (see [Removal scripts](#removal-scripts)).

A silent uninstall (`/S`) leaves the box unticked. Installing a new version over an old one runs none of this cleanup, so the certificate, the login entry and Cider's proxy setting stay in place.

## macOS and Linux

There is no uninstaller that runs the cleanup. Before you delete the app or remove the package, open **Settings** and use **Remove all data…**, or run the cleanup from a terminal (see [Command-line cleanup](#command-line-cleanup)). Then delete `Cider Offline.app` from Applications, or remove the package with your package manager (`sudo apt remove cider-offline`, `sudo dnf remove cider-offline`, `sudo pacman -R cider-offline`), or delete the AppImage.

## Portable build

Everything the app stores is inside its `data` folder, but the certificate, the login entry and Cider's proxy setting are not. Use **Settings > Remove all data…** first, then delete the whole folder.

## Remove all data

**Settings > Storage > Remove all data…** removes everything the app created:

1. It removes the certificate from the trust store, turns off start at login, and turns off Cider's proxy setting when it points at this proxy (Windows only; on macOS and Linux the app never changed Cider's settings).
2. The app quits. After the proxy has stopped, it deletes the config, data and cache folders. On Windows it also removes the empty `cider-offline\CiderOffline` folders above them.

The confirmation dialog names the folders it will delete. In a portable copy it deletes only the app's own entries inside `data` (`config`, `cache`, `store.sqlite` and its `-wal`, `-shm` and `-journal` files, `media`, `lyrics`, `ca`, `captures`, `instance.lock`, `.keep` and the optional `device.wvd`). The `data` folder itself and anything else you keep in it stay.

Files you picked from elsewhere, such as linked audio files or lyrics files, are never touched.

If removing the certificate fails, the `ca` folder is kept so the certificate can still be removed later with `--uninstall-cleanup`. If a step fails after the window has closed, the app writes a `cider-offline-removal-report-*.txt` file to your temporary folder and opens it.

## Command-line cleanup

The same cleanup can run from a terminal:

| Command | Effect |
| --- | --- |
| `cider-offline --uninstall-cleanup` | Remove the certificate, turn off start at login, turn off Cider's proxy setting (Windows). Keeps all files. This is what the Windows uninstaller always runs. |
| `cider-offline --remove-all-data` | The above, then delete the app's files, like **Remove all data**. |
| add `--dry-run` | Print the steps it would take and change nothing. |

Where the program is:

- Windows: `"%LOCALAPPDATA%\Cider Offline\cider-offline.exe"`
- macOS: `"/Applications/Cider Offline.app/Contents/MacOS/cider-offline"`
- Linux packages: `cider-offline` (in `/usr/bin`)
- AppImage: the `.AppImage` file itself

The app must not be running. A cleanup run waits up to 5 seconds for a quitting app to let go, and otherwise stops with "Cider Offline is running; quit it first". It prints every step and exits with an error if any step failed.

The Windows release build has no console, so its output can't be seen. That's why a dry run, or a run with a failed step, also writes the steps to a new `cider-offline-cleanup-*.txt` file in the temporary folder (on every system) and prints its path.

## Removal scripts

If the app is already gone and its certificate is still trusted, the installers ship scripts that remove it:

| System | Script | Notes |
| --- | --- | --- |
| Windows | `%LOCALAPPDATA%\Cider Offline\scripts\remove-ca-windows.ps1` | Run with `powershell -ExecutionPolicy Bypass -File remove-ca-windows.ps1`. Windows asks you to confirm each removal. |
| macOS | `Cider Offline.app/Contents/Resources/scripts/remove-ca-macos.sh` | Asks for your password to change trust settings. |
| Linux | `/usr/lib/cider-offline/scripts/remove-ca-linux.sh` | Needs `certutil`. Removing the system-wide copy asks for your password through polkit. |

The scripts remove only the certificate, not the login entry or Cider's proxy setting. Copy the script somewhere else before you uninstall if you want to run it afterwards.

You can also remove the certificate by hand. It is called **Cider Offline CA**. On Windows it is in the current user's **Trusted Root Certification Authorities** (`certmgr.msc`), on macOS in the login keychain (Keychain Access), and on Linux in `~/.pki/nssdb` and the system trust anchors.
