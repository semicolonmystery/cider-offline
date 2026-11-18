#!/bin/sh
# Removes the Cider Offline local CA (and its user trust settings) from
# the login keychain. Use after deleting the app if the CA was not removed from
# its Settings page. macOS asks for your password to change trust settings.
set -eu
keychain="$HOME/Library/Keychains/login.keychain-db"
# The current name, then the one used before the rename to "Cider Offline".
for name in "Cider Offline CA" "Cider Offline Proxy Local CA"; do
    removed=0
    while security find-certificate -c "$name" "$keychain" >/dev/null 2>&1; do
        security delete-certificate -t -c "$name" "$keychain"
        removed=$((removed + 1))
    done
    echo "Removed $removed certificate(s) named \"$name\"."
done
