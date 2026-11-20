// The Cider Offline window, rebuilt in HTML/CSS from the Slint UI so each part can
// move on its own. It renders a still of one state; the tour animates it from there.
// To assistive tech the whole window is one image with a label.

import type { CSSProperties } from "react";

import { AppIcon, type IconName } from "./app-icons";
import s from "./app-window.module.css";
import {
  ALBUMS,
  LIBRARY_COUNT_LINE,
  OPEN_ALBUM,
  OPEN_ALBUM_TRACKS,
  type LyricsQuality,
} from "./library-data";

export type WindowView = "library" | "album";
/** The album's download: not started, part-way (two songs running), or finished. */
export type WindowDownload = "idle" | "progress" | "done";
type ControlState = "none" | "idle" | "queued" | "downloading" | "done";

type Props = {
  view: WindowView;
  download?: WindowDownload;
  /** Play the CSS launch and library fill on load. */
  launch?: boolean;
  label: string;
  /** Load the covers eagerly (the first window on the page). */
  priority?: boolean;
};

const LYRICS_ICON: Record<LyricsQuality, IconName> = {
  3: "lyricsWord",
  2: "lyricsSynced",
  1: "lyrics",
};

// A still of the download part-way through: four songs done, two running.
function rowState(index: number, download: WindowDownload): { state: ControlState; p: number } {
  if (download === "done") return { state: "done", p: 1 };
  if (download === "idle") return { state: "none", p: 0 };
  if (index < 4) return { state: "done", p: 1 };
  if (index === 4) return { state: "downloading", p: 0.72 };
  if (index === 5) return { state: "downloading", p: 0.35 };
  return { state: "queued", p: 0 };
}

function albumState(download: WindowDownload): { state: ControlState; p: number } {
  if (download === "done") return { state: "done", p: 1 };
  if (download === "progress") return { state: "downloading", p: 0.36 };
  return { state: "idle", p: 0 };
}

function Control({
  ring,
  state,
  p,
  anim,
}: {
  ring: number;
  state: ControlState;
  p: number;
  anim?: string;
}) {
  return (
    <div
      className={s.control}
      data-state={state}
      data-anim={anim}
      style={{ "--ring": ring, "--p": p } as CSSProperties}
    >
      <AppIcon name="download" className={s.ctlIdle} />
      <svg className={s.ctlRing} viewBox="0 0 24 24" fill="none" aria-hidden="true">
        <circle className={s.ringTrack} cx="12" cy="12" r="10.5" strokeWidth="2" />
        <circle
          className={s.ringBar}
          cx="12"
          cy="12"
          r="10.5"
          strokeWidth="2"
          strokeLinecap="round"
          pathLength={100}
          transform="rotate(-90 12 12)"
        />
        <g transform="translate(6 6) scale(0.5)" className={s.ringGlyph}>
          <path
            d="M4 7C4 5.6 4 4.9 4.44 4.44C4.88 4 5.59 4 7 4C8.41 4 9.12 4 9.56 4.44C10 4.88 10 5.59 10 7V17C10 18.41 10 19.12 9.56 19.56C9.12 20 8.41 20 7 20C5.59 20 4.88 20 4.44 19.56C4 19.12 4 18.41 4 17V7ZM14 7C14 5.59 14 4.88 14.44 4.44C14.88 4 15.59 4 17 4C18.41 4 19.12 4 19.56 4.44C20 4.88 20 5.59 20 7V17C20 18.41 20 19.12 19.56 19.56C19.12 20 18.41 20 17 20C15.59 20 14.88 20 14.44 19.56C14 19.12 14 18.41 14 17V7Z"
            fill="currentColor"
          />
        </g>
      </svg>
      <AppIcon name="done" className={s.ctlDone} />
    </div>
  );
}

export function AppWindow({ view, download = "idle", launch = false, label, priority = false }: Props) {
  const album = albumState(download);

  return (
    <div
      className={`${s.window} ${launch ? s.launch : ""}`}
      data-view={view}
      data-anim="window"
      role="img"
      aria-label={label}
    >
      <div className={s.titlebar} aria-hidden="true">
        <span className={s.light} />
        <span className={s.light} />
        <span className={s.light} />
        <span className={s.titleText}>Cider Offline</span>
      </div>

      <div className={s.body} aria-hidden="true">
        <div className={s.sidebar}>
          <div className={s.brand}>
            {/* eslint-disable-next-line @next/next/no-img-element -- static export, unoptimized images */}
            <img className={s.brandIcon} src="/app-icon.svg" alt="" width={28} height={28} />
            <span className={s.label}>Cider Offline</span>
          </div>
          <div className={s.nav}>
            <AppIcon name="overview" />
            <span className={s.label}>Overview</span>
          </div>
          <div className={`${s.nav} ${s.navSelected}`}>
            <AppIcon name="library" />
            <span className={s.label}>Library</span>
          </div>
          <div className={s.nav}>
            <AppIcon name="settings" />
            <span className={s.label}>Settings</span>
          </div>
          <div className={s.status}>
            <span className={s.dot} />
            <span className={s.label}>Active, 127.0.0.1:8000</span>
          </div>
        </div>

        <div className={s.page}>
          {/* Library: header, toolbar and the album grid. */}
          <div className={`${s.layer} ${s.library}`} data-anim="library">
            <div className={s.libraryHeader}>
              <div>
                <div className={s.largeTitle}>Library</div>
                <div className={s.callout}>{LIBRARY_COUNT_LINE}</div>
              </div>
              <div className={s.headerActions}>
                <span className={s.iconButton}>
                  <AppIcon name="add" />
                </span>
                <span className={s.iconButton}>
                  <AppIcon name="more" />
                </span>
              </div>
            </div>
            <div className={s.toolbar}>
              <div className={s.search}>
                <AppIcon name="search" />
                Search albums, playlists and songs
              </div>
              <div className={s.segmented}>
                <span className={`${s.segment} ${s.segmentOn}`}>Library</span>
                <span className={s.segment}>Downloaded</span>
                <span className={s.segment}>All</span>
                <span className={s.segment}>Custom</span>
              </div>
              <div className={s.toggles}>
                <span className={`${s.toggle} ${s.toggleOn}`}>
                  <AppIcon name="grid" />
                </span>
                <span className={s.toggle}>
                  <AppIcon name="list" />
                </span>
              </div>
              <div className={`${s.toggles} ${s.filters}`}>
                <span className={s.toggle}>
                  <AppIcon name="missingAudio" />
                </span>
                <span className={s.toggle}>
                  <AppIcon name="lyrics" />
                </span>
              </div>
            </div>
            <div className={s.grid}>
              {ALBUMS.map((album, i) => (
                <div
                  key={album.slug}
                  className={s.card}
                  data-anim={album.slug === OPEN_ALBUM.slug ? "open-card" : undefined}
                  style={{ "--i": i } as CSSProperties}
                >
                  <div className={s.cover}>
                    {/* eslint-disable-next-line @next/next/no-img-element -- static export, unoptimized images */}
                    <img
                      src={`/covers/${album.slug}.webp`}
                      alt=""
                      width={400}
                      height={400}
                      loading={priority ? "eager" : "lazy"}
                      fetchPriority={priority && i < 4 ? "high" : undefined}
                      decoding="async"
                    />
                    <span className={s.coverDim} data-anim="cover-dim" />
                    <span className={s.moreChip} data-anim="more-chip">
                      <AppIcon name="more" />
                    </span>
                  </div>
                  <div className={s.cardTitle}>{album.name}</div>
                  <div className={s.cardSubtitle}>{album.artist}</div>
                </div>
              ))}
            </div>
          </div>

          {/* Album detail: back, hero, download control, songs. */}
          <div className={`${s.layer} ${s.detail}`} data-anim="detail">
            <span className={s.iconButton}>
              <AppIcon name="back" />
            </span>
            <div className={s.hero}>
              <div className={s.heroCover} data-anim="hero-cover">
                {/* eslint-disable-next-line @next/next/no-img-element -- static export, unoptimized images */}
                <img
                  src={`/covers/${OPEN_ALBUM.slug}.webp`}
                  alt=""
                  width={400}
                  height={400}
                  loading={priority ? "eager" : "lazy"}
                  decoding="async"
                />
              </div>
              <div className={s.heroText} data-anim="hero-text">
                <div className={s.title}>{OPEN_ALBUM.name}</div>
                <div className={s.headline}>{OPEN_ALBUM.subtitle}</div>
                <div className={s.meta}>{OPEN_ALBUM.meta}</div>
                <div className={s.heroActions}>
                  <Control ring={36} state={album.state} p={album.p} anim="album-control" />
                  <span className={s.iconButton}>
                    <AppIcon name="more" />
                  </span>
                </div>
              </div>
            </div>
            <div className={s.columns} data-anim="columns">
              <span className={s.number}>#</span>
              <span className={s.colTitle}>Title</span>
              <span className={s.colAlbum}>Album</span>
              <span className={s.colTime}>Time</span>
            </div>
            {OPEN_ALBUM_TRACKS.map((track, i) => {
              const row = rowState(i, download);
              return (
                <div key={track.title} className={s.row} data-anim="row">
                  <span className={s.number}>{i + 1}</span>
                  <div className={s.trackText}>
                    <div className={s.trackTitle}>{track.title}</div>
                    <div className={s.trackArtist}>Kanye West</div>
                  </div>
                  <div className={s.trackAlbum}>{OPEN_ALBUM.name}</div>
                  <span className={s.lyricsMark}>
                    <AppIcon name={LYRICS_ICON[track.lyrics]} />
                  </span>
                  <span className={s.statusCell}>
                    <Control ring={22} state={row.state} p={row.p} anim="row-control" />
                  </span>
                  <span className={s.duration}>{track.duration}</span>
                  <span />
                </div>
              );
            })}
          </div>
        </div>
      </div>

      <div className={s.cursor} data-anim="cursor" aria-hidden="true">
        <svg viewBox="0 0 20 20">
        <path
          d="M3 2.5v13.2l3.4-3.1 2.3 5 2.3-1-2.3-4.9h4.8L3 2.5Z"
          fill="#000"
          stroke="#fff"
          strokeWidth="1.2"
          strokeLinejoin="round"
        />
        </svg>
      </div>
    </div>
  );
}
