// The demo library shown in the rebuilt app window: the same Kanye West albums as
// the screenshots in the book (docs/src/images). Names, track lists and lengths are
// the real ones from the iTunes catalogue; covers are in public/covers.

export type Album = {
  slug: string;
  name: string;
  artist: string;
};

/** The Library grid, in the order of the book's library screenshot. */
export const ALBUMS: readonly Album[] = [
  { slug: "808s-and-heartbreak", name: "808s & Heartbreak", artist: "Kanye West" },
  { slug: "graduation", name: "Graduation", artist: "Kanye West" },
  { slug: "jesus-is-king", name: "JESUS IS KING", artist: "Kanye West" },
  { slug: "late-registration", name: "Late Registration", artist: "Kanye West" },
  {
    slug: "my-beautiful-dark-twisted-fantasy",
    name: "My Beautiful Dark Twisted Fantasy",
    artist: "Kanye West",
  },
  { slug: "the-college-dropout", name: "The College Dropout", artist: "Kanye West" },
  { slug: "the-life-of-pablo", name: "The Life of Pablo", artist: "Kanye West" },
  { slug: "ye", name: "ye", artist: "Kanye West" },
];

/** "8 albums and playlists, 119 songs": the eight albums have 119 songs. */
export const LIBRARY_COUNT_LINE = "8 albums and playlists, 119 songs";

/** The album opened in the tour. */
export const OPEN_ALBUM = {
  slug: "graduation",
  name: "Graduation",
  subtitle: "Album by Kanye West",
  meta: "14 songs, 54 minutes",
} as const;

/** Lyrics timing as the app shows it: 3 word-synced, 2 line-synced, 1 plain. */
export type LyricsQuality = 1 | 2 | 3;

export type Track = {
  title: string;
  duration: string;
  lyrics: LyricsQuality;
};

export const OPEN_ALBUM_TRACKS: readonly Track[] = [
  { title: "Good Morning", duration: "3:15", lyrics: 3 },
  { title: "Champion", duration: "2:47", lyrics: 3 },
  { title: "Stronger", duration: "5:12", lyrics: 3 },
  { title: "I Wonder", duration: "4:03", lyrics: 2 },
  { title: "Good Life (feat. T-Pain)", duration: "3:26", lyrics: 3 },
  { title: "Can't Tell Me Nothing", duration: "4:31", lyrics: 3 },
  { title: "Barry Bonds (feat. Lil Wayne)", duration: "3:24", lyrics: 2 },
  { title: "Drunk and Hot Girls (feat. Mos Def)", duration: "5:13", lyrics: 3 },
  { title: "Flashing Lights", duration: "3:57", lyrics: 3 },
  { title: "Everything I Am (feat. DJ Premier)", duration: "3:47", lyrics: 2 },
  { title: "The Glory", duration: "3:32", lyrics: 3 },
  { title: "Homecoming (feat. Chris Martin)", duration: "3:24", lyrics: 3 },
  { title: "Big Brother", duration: "4:48", lyrics: 2 },
  { title: "Good Night", duration: "3:06", lyrics: 3 },
];
