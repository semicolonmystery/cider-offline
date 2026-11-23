import type { WindowDownload, WindowView } from "./app-window";

export type TourScene = {
  id: string;
  /** Where the caption sits on wide screens (the window takes the other side). */
  side: "left" | "right";
  title: string;
  body: string;
  /** The still of the window for this scene (reduced motion, no script). */
  view: WindowView;
  download: WindowDownload;
  /** Text alternative for the window in this state. */
  windowLabel: string;
};

export const TOUR_SCENES: readonly TourScene[] = [
  {
    id: "library",
    side: "left",
    title: "Your library, kept on your computer",
    body: "While you're online, Cider Offline syncs every playlist, album and song in your Apple Music library, with artwork, and keeps the copy up to date.",
    view: "library",
    download: "idle",
    windowLabel:
      "The Library page of Cider Offline showing eight Kanye West albums, 8 albums and playlists, 119 songs.",
  },
  {
    id: "album",
    side: "right",
    title: "Open an album",
    body: "Open any album or playlist to see its songs, which of them are on this computer and which have lyrics.",
    view: "album",
    download: "idle",
    windowLabel:
      "The album page for Graduation by Kanye West: 14 songs, 54 minutes, with a download button and the track list.",
  },
  {
    id: "download",
    side: "left",
    title: "Download what you want offline",
    body: "Download a song, an album or a playlist. The app asks Apple Music for each song the way Cider does and saves it on your computer for offline playback in Cider. Two songs download at a time, and lyrics are fetched when each one finishes.",
    view: "album",
    download: "progress",
    windowLabel:
      "Graduation downloading: the first four songs are done, two are in progress and the rest are waiting.",
  },
];
