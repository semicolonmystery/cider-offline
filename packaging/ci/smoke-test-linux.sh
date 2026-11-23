#!/usr/bin/env bash
# Smoke-tests the Linux packages that release.yml builds, from the repository
# root, after `cargo packager` (AppImage, .deb, pacman) and `cargo generate-rpm`:
#
#   VERSION=0.1.1 bash packaging/ci/smoke-test-linux.sh
#
# For every package it checks that the shipped binary prints
# `cider-offline $VERSION` for `--version`, that `--version` does not create a
# portable `data` folder beside it, and that the CA removal scripts are shipped
# executable at /usr/lib/cider-offline/scripts. Each check says what it checks
# and every failure names what failed.
set -Eeuo pipefail
shopt -s nullglob
trap 'echo "::error::smoke test: line $LINENO exited with status $?: $BASH_COMMAND" >&2' ERR

: "${VERSION:?VERSION must be set (the app version, without a leading v)}"
expect="cider-offline ${VERSION}"
scripts_dir="usr/lib/cider-offline/scripts"

fail() {
  echo "::error::$*" >&2
  exit 1
}

# one VAR GLOB...: set VAR to the single path the glob matched.
one() {
  local var="$1"
  shift
  if [ $# -ne 1 ]; then
    echo "expected exactly one file, found $#: $*" >&2
    ls -la target/packages target/generate-rpm >&2 || true
    fail "expected exactly one file for $var, found $#"
  fi
  printf -v "$var" '%s' "$1"
  echo "found $var: $1"
}

# check_version WHAT CMD...: CMD must exit 0 and print exactly $expect.
check_version() {
  local what="$1" out status=0
  shift
  echo "checking $what: '$*' prints '$expect'"
  out="$("$@" 2>&1)" || status=$?
  if [ "$status" -ne 0 ]; then
    fail "$what: '$*' exited with status $status, output: $out"
  fi
  if [ "$out" != "$expect" ]; then
    fail "$what: '$*' printed '$out', expected '$expect'"
  fi
  echo "ok   $what: $out"
}

# check_tree WHAT ROOT: the extracted package under ROOT ships the binary and
# executable removal scripts, and `--version` left no `data` folder behind.
check_tree() {
  local what="$1" root="$2" f
  check_version "$what" "$root/usr/bin/cider-offline" --version
  echo "checking $what: --version did not create $root/usr/bin/data"
  if [ -e "$root/usr/bin/data" ]; then
    fail "$what: --version created $root/usr/bin/data (portable mode must stay off)"
  fi
  for f in remove-ca-linux.sh remove-ca-macos.sh; do
    echo "checking $what: /$scripts_dir/$f is an executable file"
    if [ ! -f "$root/$scripts_dir/$f" ]; then
      echo "contents of $root/usr/lib:" >&2
      find "$root/usr/lib" -maxdepth 3 >&2 || true
      fail "$what: /$scripts_dir/$f is missing"
    fi
    if [ ! -x "$root/$scripts_dir/$f" ]; then
      fail "$what: /$scripts_dir/$f is not executable (mode $(stat -c '%a' "$root/$scripts_dir/$f"))"
    fi
  done
  echo "checking $what: /$scripts_dir/remove-ca-windows.ps1 is shipped"
  [ -f "$root/$scripts_dir/remove-ca-windows.ps1" ] ||
    fail "$what: /$scripts_dir/remove-ca-windows.ps1 is missing"
  echo "ok   $what: binary, no data folder, removal scripts"
}

one appimage target/packages/*.AppImage
one deb target/packages/*.deb
one rpm target/generate-rpm/*.rpm
# The pacman package is the only .tar.gz cargo-packager writes.
one pacman target/packages/*.tar.gz

# bsdtar (libarchive) reads .rpm natively. Not rpm2cpio: Ubuntu's 4.18 build
# exits 1 on cargo-generate-rpm packages even though it writes the whole payload.
if ! command -v bsdtar >/dev/null; then
  echo "installing bsdtar (libarchive-tools)"
  sudo apt-get install -y --no-install-recommends libarchive-tools
fi

# AppImage: run it directly, then check its extracted tree. Extracting into an
# empty temp dir keeps `data/` away from the binary, so portable mode stays off.
chmod +x "$appimage"
appimage_abs="$(realpath "$appimage")"
check_version AppImage env APPIMAGE_EXTRACT_AND_RUN=1 "$appimage_abs" --version
appdir="$(mktemp -d)"
echo "extracting AppImage into $appdir"
(cd "$appdir" && "$appimage_abs" --appimage-extract >/dev/null) ||
  fail "AppImage: --appimage-extract failed"
check_tree AppImage "$appdir/squashfs-root"

debdir="$(mktemp -d)"
echo "extracting .deb into $debdir"
dpkg-deb -x "$deb" "$debdir" || fail ".deb: dpkg-deb -x $deb failed"
check_tree .deb "$debdir"

rpmdir="$(mktemp -d)"
echo "extracting .rpm into $rpmdir"
bsdtar -xf "$rpm" -C "$rpmdir" || fail ".rpm: bsdtar -xf $rpm failed"
check_tree .rpm "$rpmdir"

pacdir="$(mktemp -d)"
echo "extracting pacman package into $pacdir"
tar -xzf "$pacman" -C "$pacdir" || fail "pacman: tar -xzf $pacman failed"
check_tree pacman "$pacdir"

# The PKGBUILD must name the pacman archive it ships next to.
pacman_name="$(basename "$pacman")"
echo "checking PKGBUILD: source= names $pacman_name"
[ -f target/packages/PKGBUILD ] || fail "PKGBUILD: target/packages/PKGBUILD is missing"
grep -qF "source=(\"$pacman_name\")" target/packages/PKGBUILD ||
  fail "PKGBUILD: source= does not name $pacman_name: $(grep '^source=' target/packages/PKGBUILD || echo '(no source= line)')"

echo "smoke test passed: AppImage, .deb, .rpm and pacman all print '$expect'"
