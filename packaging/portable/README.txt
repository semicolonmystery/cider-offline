Cider Offline - portable build

This is the portable build. It runs in portable mode while a folder named "data" sits next to the program (create an empty one if it is missing): everything the app stores (settings, library database, downloaded music, lyrics, artwork, logs and its certificate) lives inside "data", so deleting this whole folder removes the app and all of its files. Before you delete it, open Settings and use "Remove all data" first: that also removes the trusted certificate, the start-at-login entry and Cider's proxy setting, which live in your operating system outside this folder and would otherwise stay behind.

"Remove all data" deletes only the app's own files and folders inside "data" (config, cache, store.sqlite and its -wal/-shm/-journal files, media, lyrics, ca, captures, instance.lock, device.wvd, .keep). Anything else you put in "data" stays.

macOS: a downloaded zip is quarantined, and macOS then runs the app from a hidden read-only copy ("App Translocation") where the "data" folder is not found, so the app would use your normal user folders instead (it shows a warning on the Overview page). Before the first start, remove the quarantine from the unzipped folder in Terminal:

    xattr -dr com.apple.quarantine "/path/to/unzipped folder"
