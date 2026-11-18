#!/bin/sh
# System libraries needed to build the app on Debian/Ubuntu (CI and local dev):
# - Slint (winit backend): xcb, xkbcommon, fontconfig
# - tray (ksni) and file pickers (rfd, xdg-portal) are pure Rust: no GTK, appindicator or libxdo
# - trust store at runtime / tests: certutil (libnss3-tools)
set -eu
sudo apt-get update
sudo apt-get install -y --no-install-recommends \
    pkg-config \
    libxcb-shape0-dev \
    libxcb-xfixes0-dev \
    libxkbcommon-dev \
    libfontconfig1-dev \
    libnss3-tools
