# Installation

Every release on the [Releases page](https://github.com/semicolonmystery/cider-offline/releases) has an installer and a portable archive for each system, plus a `SHA256SUMS.txt` with the checksum of every file.

Use the installer unless you have a reason not to. The portable build is for running the app from a folder of your choice (a USB stick, a synced folder) with all of its data next to it. The two behave the same once they are running. See [Files and folders](files.md) for where each keeps its data.

## Windows

| File | What it is |
| --- | --- |
| `cider-offline_<version>_x64-setup.exe` | Installer for 64-bit Intel and AMD PCs |
| `cider-offline_<version>_x86-setup.exe` | Installer for 32-bit Windows |
| `cider-offline_<version>_arm64-setup.exe` | Installer for ARM64 PCs |
| `cider-offline_<version>_<arch>-portable.zip` | Portable build for the same three architectures |

The installer installs for your user only, into `%LOCALAPPDATA%\Cider Offline`, and doesn't ask for administrator rights. Installing a newer version over an older one keeps your data and settings.

For the portable build, unzip the archive anywhere you can write to. You get `cider-offline.exe`, a `README.txt` and an empty `data` folder. Keep the `data` folder next to the exe: that folder is what makes the app run in portable mode.

### SmartScreen warning

The builds are not code-signed, so the first time you run the installer or the portable exe, Windows may show "Windows protected your PC". Click **More info**, check that the app name is right, then click **Run anyway**.

## macOS

| File | What it is |
| --- | --- |
| `cider-offline_<version>_universal.dmg` | Disk image with the app, for Intel and Apple silicon Macs |
| `cider-offline_<version>_universal-portable.zip` | Portable build: the app next to an empty `data` folder |

Open the `.dmg` and drag **Cider Offline** into **Applications**. macOS 11 or newer is required.

The app lives in the menu bar. It shows a Dock icon only while its window is open.

### Gatekeeper warning

The macOS builds are not notarized by Apple (they are ad-hoc signed), so the first launch is blocked with a message that Apple could not check the app. To open it anyway:

1. Try to open the app once and dismiss the message.
2. Open **System Settings > Privacy & Security**, scroll down to the message about Cider Offline and click **Open Anyway**.

You can also remove the quarantine flag in Terminal:

```sh
xattr -dr com.apple.quarantine "/Applications/Cider Offline.app"
```

### Portable build on macOS

macOS runs a quarantined app that came from a downloaded zip from a hidden, read-only copy (App Translocation). From there the app can't see the `data` folder next to it and falls back to your normal user folders. The Overview page shows a warning when this happens. Before the first start, remove the quarantine from the unzipped folder:

```sh
xattr -dr com.apple.quarantine "/path/to/unzipped folder"
```

## Linux

Builds exist for x86_64 and ARM64 (`aarch64`).

| File | What it is |
| --- | --- |
| `.deb` | Package for Debian, Ubuntu and their derivatives |
| `.rpm` | Package for Fedora, openSUSE and other rpm-based distributions |
| `cider-offline_<version>_<arch>.tar.gz` and `cider-offline_<version>_<arch>.PKGBUILD` | Package for Arch Linux and its derivatives |
| `.AppImage` | Single file that runs on most distributions |
| `cider-offline_<version>_<arch>-portable.tar.gz` | Portable build: the AppImage next to an empty `data` folder |

Install the `.deb` or `.rpm` with your package manager, for example:

```sh
sudo apt install ./cider-offline_<version>_amd64.deb
sudo dnf install ./cider-offline-<version>.x86_64.rpm
```

The exact file names depend on the architecture; use the ones you downloaded.

For Arch, download the `.tar.gz` and the `.PKGBUILD` into the same folder, rename the `.PKGBUILD` file to `PKGBUILD` and run `makepkg -si` there.

The packages pull in what the app needs at runtime: the NSS tools (`certutil`), polkit (`pkexec`), `xdg-utils` and the usual graphics, font and keyboard libraries. For the AppImage, install `certutil` yourself: `libnss3-tools` on Debian and Ubuntu, `nss-tools` on Fedora, `nss` on Arch.

Make the AppImage executable before you run it:

```sh
chmod +x cider-offline_<version>_<arch>.AppImage
```

To use the system tray on GNOME, install the "AppIndicator and KStatusNotifierItem Support" extension. Without a tray the app still works, but closing its window quits it.

## First run and the local certificate

When the app starts for the first time, the Overview page shows two steps: trust the local certificate, then connect Cider. Connecting Cider has [its own page](connecting-cider.md). This section is about the certificate.

### Why it's needed

Cider talks to Apple Music over HTTPS. To keep a copy of your library and answer Cider while you are offline, the proxy has to read those connections, and Cider only accepts that if the proxy can present a certificate it trusts. So on the first start the app creates its own certificate authority, called **Cider Offline CA**, on your computer. It is valid for about ten years. The certificate and its private key stay in the `ca` folder of the app's data folder; on macOS and Linux the key file is readable only by you.

Click **Install** on the **Certificate** row of the Overview page. Your system then asks you to confirm:

- Windows adds the certificate to your user's Root store and shows its own security warning. No administrator rights are needed.
- macOS adds it to your login keychain and asks for your password to change the trust settings.
- Linux adds it to your NSS database (`~/.pki/nssdb`, plus the databases of a Flatpak or Snap Cider). That is the store Cider uses, and it needs no password. The app then also tries to add it to the system's trust anchors, which asks for your password through polkit. That part is optional: if it is skipped, the row says the certificate is trusted for Cider and system-wide trust was not installed.

**Check again** re-reads the trust store, for example after you installed Cider as a Flatpak later. When the app can't read the trust store, the row shows the reason and the button says **Fix** instead of **Install**; it runs the same installation again.

### What the proxy can see

The proxy listens on `127.0.0.1` (your own computer only) on port 8000 by default. Only programs you point at it use it, which normally means just Cider.

In Active mode it decrypts and reads Cider's traffic to Apple Music: hosts under `music.apple.com`, `itunes.apple.com` and `mzstatic.com` (artwork), and while offline also `blobstore.apple.com` (custom playlist covers). This is where it caches responses, captures the Apple Music session tokens it needs for syncing and downloading, and serves your local files. Connections to any other host pass through as encrypted tunnels that the proxy does not open. In Passthrough mode nothing is decrypted at all.

A trusted root certificate comes with a risk: whoever has its private key could make your computer accept a fake certificate for any website. The key is created on your computer and never leaves it. If you stop using the app, remove the certificate with **Settings > Uninstall certificate**, or use **Remove all data**, which also removes it (see [Uninstalling](uninstalling.md)).
