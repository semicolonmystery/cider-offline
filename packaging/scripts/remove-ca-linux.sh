#!/bin/sh
# Removes the Cider Offline local CA from the user NSS databases (used by
# Cider / Chromium) and from the system trust anchors. Use after uninstalling if
# the CA was not removed from the app's Settings page.
set -eu

for db in "$HOME/.pki/nssdb" "$HOME"/.var/app/*[Cc]ider*/.pki/nssdb \
    "$HOME"/snap/*[Cc]ider*/current/.pki/nssdb; do
    [ -f "$db/cert9.db" ] || continue
    # The current nickname, then the one used before the rename to "Cider Offline".
    for name in "Cider Offline CA" "Cider Offline Proxy Local CA"; do
        while certutil -L -d "sql:$db" -n "$name" >/dev/null 2>&1; do
            certutil -D -d "sql:$db" -n "$name"
            echo "Removed CA \"$name\" from $db"
        done
    done
done

# Same layouts as the app (crates/trust/src/store/linux.rs), with the current
# and the pre-rename (cider-offline-proxy-ca) anchor file names.
for entry in \
    "/etc/pki/ca-trust/source/anchors/cider-offline-ca.pem|update-ca-trust extract" \
    "/usr/local/share/ca-certificates/cider-offline-ca.crt|update-ca-certificates" \
    "/etc/ca-certificates/trust-source/anchors/cider-offline-ca.crt|trust extract-compat" \
    "/usr/share/pki/trust/anchors/cider-offline-ca.pem|update-ca-certificates" \
    "/etc/pki/ca-trust/source/anchors/cider-offline-proxy-ca.pem|update-ca-trust extract" \
    "/usr/local/share/ca-certificates/cider-offline-proxy-ca.crt|update-ca-certificates" \
    "/etc/ca-certificates/trust-source/anchors/cider-offline-proxy-ca.crt|trust extract-compat" \
    "/usr/share/pki/trust/anchors/cider-offline-proxy-ca.pem|update-ca-certificates"; do
    file=${entry%%|*}
    refresh=${entry#*|}
    if [ -e "$file" ]; then
        # $refresh is a fixed command line from the list above.
        # shellcheck disable=SC2086
        pkexec /bin/sh -c 'rm -f -- "$1" && shift && exec "$@"' sh "$file" $refresh
        echo "Removed $file"
    fi
done
