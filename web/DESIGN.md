# DESIGN.md (cider-offline.podik.cz)

The design direction for the website. Everything on the page follows it; a change
of direction starts here.

## 1. Brand

Cider Offline is a small, honest desktop tool: a proxy that keeps your Apple Music
library in Cider working when the internet is gone. Its face is the app itself, an
Apple-style window of greys with one red accent, and an icon where an eighth note
turns into a download arrow. The site should feel like opening the app, not like
reading an advert for it.

- Adjectives: plain, exact, calm, quietly confident, music-first.
- It must never look like: a SaaS landing kit (feature-card grid, stat row, logo
  bar, testimonials), a "hacker tool" (dark terminal, neon, monospace), a piracy
  site (skulls, "free music", anything that hints at getting music without paying), or a copy of Apple's own pages.

Design read: product landing page for Cider users who want offline listening,
in the app's own visual language, dial **ENERGY 2 / RHYTHM 2 / MOTION 3**. Motion
is high because the page is one scroll-driven tour of the app; everything around
the tour stays quiet (energy 2, rhythm 2).

## 2. The one visual idea

The app window is the page. It opens like an app launched from the Dock, then
the reader's scroll drives it: the window steps aside, the caption slides in from
the free side, and the UI inside the window does the thing the caption says. The
app is rebuilt in HTML/CSS from the Slint sources (`../crates/app/ui`), so each
part (a card, a row, a progress ring) moves on its own. No screenshots, no
decoration around it: no gradients, glows, grids or blobs.

Identity motif: the app's progress ring (accent stroke on a faint track) and the
alternating window/caption sides.

## 3. Colour

The site uses the app's palette (`Theme` in `components.slint`) so the page and
the rebuilt window are one surface. Light and dark follow `prefers-color-scheme`,
for the site and for the window.

| Token (globals.css) | Light | Dark | Role |
|---|---|---|---|
| `--background` | `#FFFFFF` | `#1C1C1E` | Page, app content area (`bg-window`) |
| `--sidebar` | `#F5F5F7` | `#161618` | App sidebar, quiet bands |
| `--card` | `#FFFFFF` | `#2C2C2E` | Raised surfaces (`bg-raised`) |
| `--foreground` | `#1D1D1F` | `#F5F5F7` | Text (`text-primary`) |
| `--muted-foreground` | `#6E6E73` | `#98989D` | Secondary text (`text-secondary`) |
| `--primary` | `#FA243C` | `#FA2D48` | Accent: progress, selection, icon tint |
| `--accent-strong` | `#D61F35` | `#D9213B` | Fill of the one primary button (`accent-pressed`) |
| `--border` | `rgb(0 0 0 / 10%)` | `rgb(255 255 255 / 8%)` | Hairlines (`separator`) |

Measured contrast (WCAG 2.x, rendered solid colours):

| Pair | Light | Dark |
|---|---|---|
| Text on background | 16.83 | 15.63 |
| Secondary text on background | 5.07 | 5.93 |
| Secondary text on sidebar | 4.66 | 6.29 |
| White on `--accent-strong` (Download button) | 5.11 | 4.96 |
| White on `--primary` | 3.90 (fails 4.5, so never used for button text) | 3.79 |

The app's tertiary grey (`#AEAEB2` / `#636366`, 2.21 / 2.84) appears only inside
the rebuilt window, which is an image to assistive tech (`role="img"` with a
label); site text never uses it. One accent: red marks the Download button and
live state inside the window (progress, the selected nav item). Links are
foreground with an underline, not red.

## 4. Type

- **Inter** (SIL OFL 1.1, rsms/inter), variable with the `opsz` axis, subsets
  `latin` and `latin-ext`, self-hosted through `next/font/google`.
- Reason: the app draws its UI in the platform's interface face; Inter is the open
  face closest to it, so the rebuilt window looks like the app and the page
  speaks with the same voice. At display sizes `opsz` switches it to its display
  cut, which gives the headline its own character without a second family.
- One family, hierarchy by size and weight:

| Role | Size | Weight | Line height | Tracking |
|---|---|---|---|---|
| Display (h1) | `clamp(2.75rem, 6vw + 1rem, 5.5rem)` | 700 | 1.0 | -0.035em |
| Caption title (h2) | `clamp(1.5rem, 1.6vw + 1rem, 2.25rem)` | 650 | 1.15 | -0.02em |
| Lead | `clamp(1.125rem, 0.6vw + 1rem, 1.375rem)` | 400 | 1.45 | 0 |
| Body | 1rem | 400 | 1.55 | 0 |
| Small | 0.875rem | 400 | 1.5 | 0 |

No all-caps labels, no eyebrows, no monospace, no single-word colour accents in a
headline. Inside the window the app's own scale applies (26/24/15/13/12/11 px,
scaled with the window).

## 5. Space and shape

- Spacing scale (rem): 0.25, 0.5, 0.75, 1, 1.5, 2, 3, 4, 6, 8. Page gutter 1rem on
  phones, 2rem from 640px, 3rem from 1024px.
- Radii come from the app: 4 / 6 / 10 px inside the window; the window itself 12px
  (macOS window corner); site buttons 10px (`radius-md`), not pills.
- One shadow: the window, because it is the only thing that floats above the page.

## 6. Motion

| What | How | Duration / ease |
|---|---|---|
| App launch (on load) | Window grows from a small, low point (the Dock) to full size, opacity 0 to 1 in the first third | 700ms, `cubic-bezier(0.22, 1, 0.36, 1)`; CSS animation, runs before any script |
| Library fill (on load, after launch) | Covers fade and rise 8px in reading order | 400ms each, 50ms stagger, same ease |
| Scroll scenes | One GSAP timeline, ScrollTrigger `scrub: 0.6` over a tall section with a CSS `position: sticky` stage. Native scrolling, no pin spacer, no smooth-scroll library | Scroll-driven; tweens `power2.inOut` |
| Caption change | Old caption leaves toward its side (x 40px, fade); new one enters from the other side | Part of the scrub |
| Press | Target scales to 0.96 and back, cursor dips | Part of the scrub |
| Interactive states (site buttons) | Colour transitions only | 150ms ease-out |

Only `transform` and `opacity` move (progress rings tween a CSS custom property
that drives `stroke-dashoffset`). Nothing loops.

Reduced motion (`prefers-reduced-motion: reduce`): no launch, no scrubbing. The tour
is shown as static scenes, each a caption beside a still of the window in that
state. The same static scenes show when JavaScript is off.

## 7. Storyboard

Scene 0 (hero, first screen). Header: icon + name, links Docs and GitHub. Centered
headline "Cider Offline", the promise line, Download button (label names the
visitor's OS; links to the latest release) and Read the docs. Below, the window
launches and the Library page fills: 8 Kanye West albums (demo library from the docs
screenshots), "8 albums and playlists, 119 songs", status "Active, 127.0.0.1:8000".
Works as a still.

Scene 1, library. Window moves right; caption from the left: "Your library, kept
on your computer". The window holds the filled Library grid.

Scene 2, open an album. Window moves left; caption from the right: "Open an
album". A cursor moves to Graduation, the cover dims (hover), the card is pressed,
the grid slides out left and the album page slides in: cover, "Album by Kanye
West", "14 songs, 54 minutes", the download control, then the 14 real track rows
with their lyrics marks.

Scene 3, download. Window moves right; caption from the left: "Download what you
want offline". The cursor presses the album's download control; it becomes a
ring. Rows turn queued, two download at a time (the default concurrency), rings
fill in red, finished rows turn into checks, the album ring fills.

The scenes below are planned, not built yet:

Scene 4, lyrics. The lyrics column marks resolve one by one (word-synced, line-
synced) with the provider order beside the window.

Scene 5, offline. The window switches to Overview. Online dot turns from green to
amber, the Activity list gets "Offline: serving from the local cache", the Active
switch stays on, a now-playing line keeps going. Caption: Cider keeps playing.

Scene 6, features moments. Short beats, each a single window change: library sync
(count line ticking), downloads panel, lyrics providers (Settings list), your own
audio and lyrics (row "Custom" badge), tray (status line), start at login and
portable (Settings startup group).

Scene 7, how it works. The window steps back and becomes one node of a three-box
line: Cider → Cider Offline → Apple Music, with the offline branch.

Then open source (MIT, GitHub link), the disclaimer (existing wording, ending
"Use it at your own risk.") and the footer.
