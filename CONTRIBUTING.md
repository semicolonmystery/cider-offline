# Contributing

Contributions are welcome, on these terms. They are not negotiable in a pull request, but every one of them is negotiable in an issue.

**Open an issue first for anything bigger than a fix.** A new feature, a new dependency, a change to how the proxy answers Cider or to the database schema: describe it in an issue before you write it. I would rather say no or suggest a different approach before you spend a weekend on it than after.

**Small changes, one concern each.** One fix or one feature per pull request. A pull request that touches half the workspace gets closed unmerged. The work may well be good, but I can't review it honestly. If your change is big, open an issue and we split it up.

**Explain it properly.** Say what the problem was, why you solved it this way, and what you tried or ruled out. "Fix lyrics" tells me nothing. "Lyrics lookups for songs with a featured artist found nothing because the search still included the feat. part" tells me what broke and what to test.

**Match the surrounding code.** Use the patterns that are already there, the same error handling, the same naming, the same structure. Comments explain why, and only where the code can't. If you find yourself fighting a convention, ask in an issue instead of working around it.

**The gates must pass.** Before you open a pull request, run these and fix what they report:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

CI runs them too, on Windows, macOS and Linux. A pull request with a red CI won't be looked at until it's green.

**Use Conventional Commits.** `fix: retry a lyrics provider after a timeout`, `feat: show download speed`, `docs: ...`, `refactor: ...`, `test: ...`, `ci: ...`. The subject says what changed in plain words; the body says why. It keeps the history readable and the changelog easy to write.

**AI is fine.** I use it here, and you may too. What is not fine is code that doesn't fit the codebase: invented APIs, patterns nobody else here uses, comments that repeat the line below them. Whoever or whatever wrote it, you are the one submitting it, so you are responsible for it. Read it, understand it and test it yourself before you open the pull request.

**Read `CLAUDE.md` first if you use an AI tool.** It is how work is done in this repository: check crate APIs against the docs for the version in `Cargo.lock`, use current stable crates, and don't call anything finished until the gates pass. Point your tool at it.

**Don't change `CLAUDE.md` or `AGENTS.md` in a pull request.** If something in them is wrong, open an issue and I'll update them myself.

## Reporting bugs

Bug reports are worth as much as patches. Use the bug report template and include the app version, your OS, whether you installed the app or use the portable build, your Cider version, and what you did right before it went wrong.

Logs help most. They are daily `cider-offline.*.log` files in the `logs` folder of the app's cache directory:

- Windows: `%LOCALAPPDATA%\cider-offline\CiderOffline\cache\logs`
- macOS: `~/Library/Caches/sh.cider-offline.CiderOffline/logs`
- Linux: `~/.cache/cideroffline/logs`
- Portable: `data/cache/logs` next to the program

Paste the relevant lines, not the whole file, and read them first. Remove anything that looks like a token, a cookie or an `Authorization` header.

## Security issues

Don't open a public issue for a security problem. The app installs a root certificate and sits between Cider and Apple, so a bug there can matter. Report it privately through GitHub's private vulnerability reporting: the **Report a vulnerability** button on the [Security tab](https://github.com/semicolonmystery/cider-offline/security). I'll answer there.
