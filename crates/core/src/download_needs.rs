//! What "missing" means for a download: a song is missing when it lacks a
//! mix the current [`DownloadFormat`] wants.
//!
//! - **Stereo only**: the stereo mix.
//! - **Stereo + Spatial**: the stereo mix, plus the spatial mix unless the
//!   song is known to have none.
//! - **Spatial where available**: the spatial mix, unless the song is known
//!   to have none; then the stereo mix.
//!
//! Whether a song has a spatial mix comes from what the download queue
//! learned from the catalog (`song_spatial`), else from the `audioTraits`
//! (`atmos`) in the song's stored metadata. When neither says, the song
//! counts as missing its spatial mix and the queue decides: it looks the
//! song up, fetches what exists, and records the answer, so the song is
//! complete afterwards. "Download missing songs", the collection's
//! downloaded state and its counts all use [`needs_download`].

use serde_json::Value;

use crate::DownloadFormat;
use crate::storage::{AudioVariant, Track};

/// Whether a song with the mixes `present` still needs a download under
/// `format`; `has_spatial` is whether it has a spatial mix (`None`: unknown).
#[must_use]
pub fn needs_download(
    format: DownloadFormat,
    present: &[AudioVariant],
    has_spatial: Option<bool>,
) -> bool {
    let stereo = present.contains(&AudioVariant::Stereo);
    let spatial = present.contains(&AudioVariant::Spatial);
    match format {
        DownloadFormat::Stereo => !stereo,
        DownloadFormat::Both => !stereo || (!spatial && has_spatial != Some(false)),
        DownloadFormat::SpatialWhereAvailable => {
            !spatial && (has_spatial != Some(false) || !stereo)
        }
    }
}

/// Whether a stored track's metadata says the song has a spatial mix (its
/// `audioTraits` list `atmos`); `None` when it lists no traits.
#[must_use]
pub fn spatial_of_metadata(metadata: Option<&str>) -> Option<bool> {
    let resource: Value = serde_json::from_str(metadata?).ok()?;
    let traits = resource
        .pointer("/attributes/audioTraits")
        .or_else(|| resource.get("audioTraits"))?
        .as_array()?;
    Some(traits.iter().any(|t| t.as_str() == Some("atmos")))
}

/// [`needs_download`] for a stored track. `known` is what the queue learned
/// (see the module docs); custom songs never need a download.
#[must_use]
pub fn track_needs_download(track: &Track, format: DownloadFormat, known: Option<bool>) -> bool {
    if track.is_custom {
        return false;
    }
    let present: Vec<AudioVariant> = track.audio.iter().map(|(variant, _)| variant).collect();
    let spatial = known.or_else(|| spatial_of_metadata(track.metadata.as_deref()));
    needs_download(format, &present, spatial)
}

/// How much of an album or playlist is downloaded, counted from the audio
/// files its songs have (see [`track_needs_download`]). Queue rows and
/// lyrics play no part. Whether the collection itself pins its songs only
/// tells a collection the user downloaded apart from one whose songs are all
/// on disk through other downloads
/// ([`CollectionDownloadState::SavedElsewhere`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CollectionDownload {
    /// Apple Music songs of the collection. Custom songs are not downloads
    /// and are not counted.
    pub songs: usize,
    /// Songs that have every mix the format wants.
    pub complete: usize,
    /// Songs that have at least one downloaded mix, complete or not.
    pub with_audio: usize,
    /// Songs the collection itself pins (it is a download link for them).
    pub linked: usize,
}

/// The states a collection's download can be in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CollectionDownloadState {
    /// No song has a downloaded file (or the collection has no Apple Music
    /// songs).
    NotDownloaded,
    /// Some songs have files, but not every song has every wanted mix.
    Partial,
    /// Every song has every mix the format wants, but the collection does
    /// not pin all of them: some are on disk only through other albums,
    /// playlists or single downloads.
    SavedElsewhere,
    /// Every song has every mix the format wants, and the collection pins
    /// every one of them.
    Downloaded,
}

impl CollectionDownload {
    /// Counts one member song; `known` is whether the song has a spatial mix,
    /// as the queue learned it (see the module docs), and `linked` whether
    /// the collection pins it.
    pub fn add(
        &mut self,
        track: &Track,
        format: DownloadFormat,
        known: Option<bool>,
        linked: bool,
    ) {
        if track.is_custom {
            return;
        }
        self.songs += 1;
        if track.has_audio() {
            self.with_audio += 1;
        }
        if !track_needs_download(track, format, known) {
            self.complete += 1;
        }
        if linked {
            self.linked += 1;
        }
    }

    /// The summary of `members` under `format`, each with what the queue
    /// learned about its spatial mix and whether the collection pins it.
    #[must_use]
    pub fn of<'a>(
        members: impl IntoIterator<Item = (&'a Track, Option<bool>, bool)>,
        format: DownloadFormat,
    ) -> Self {
        let mut summary = Self::default();
        for (track, known, linked) in members {
            summary.add(track, format, known, linked);
        }
        summary
    }

    /// Which of the states this is.
    #[must_use]
    pub fn state(self) -> CollectionDownloadState {
        if self.songs > 0 && self.complete >= self.songs {
            if self.linked >= self.songs {
                CollectionDownloadState::Downloaded
            } else {
                CollectionDownloadState::SavedElsewhere
            }
        } else if self.complete > 0 || self.with_audio > 0 {
            CollectionDownloadState::Partial
        } else {
            CollectionDownloadState::NotDownloaded
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STEREO: &[AudioVariant] = &[AudioVariant::Stereo];
    const SPATIAL: &[AudioVariant] = &[AudioVariant::Spatial];
    const BOTH: &[AudioVariant] = &[AudioVariant::Stereo, AudioVariant::Spatial];

    /// `needs_download` for `[nothing, stereo, spatial, both]` on disk.
    fn row(format: DownloadFormat, has_spatial: Option<bool>) -> [bool; 4] {
        [&[][..], STEREO, SPATIAL, BOTH].map(|present| needs_download(format, present, has_spatial))
    }

    #[test]
    fn stereo_only_should_want_just_the_stereo_mix() {
        for spatial in [None, Some(true), Some(false)] {
            assert_eq!(
                row(DownloadFormat::Stereo, spatial),
                [true, false, true, false]
            );
        }
    }

    #[test]
    fn both_should_want_the_spatial_mix_of_an_atmos_song() {
        assert_eq!(
            row(DownloadFormat::Both, Some(true)),
            [true, true, true, false]
        );
    }

    #[test]
    fn both_should_be_complete_with_stereo_for_a_song_without_spatial() {
        assert_eq!(
            row(DownloadFormat::Both, Some(false)),
            [true, false, true, false]
        );
    }

    #[test]
    fn both_should_let_the_queue_decide_when_spatial_is_unknown() {
        assert_eq!(row(DownloadFormat::Both, None), [true, true, true, false]);
    }

    #[test]
    fn spatial_where_available_should_want_the_spatial_mix_of_an_atmos_song() {
        assert_eq!(
            row(DownloadFormat::SpatialWhereAvailable, Some(true)),
            [true, true, false, false]
        );
    }

    #[test]
    fn spatial_where_available_should_be_complete_with_stereo_without_spatial() {
        assert_eq!(
            row(DownloadFormat::SpatialWhereAvailable, Some(false)),
            [true, false, false, false]
        );
    }

    #[test]
    fn spatial_where_available_should_let_the_queue_decide_when_unknown() {
        assert_eq!(
            row(DownloadFormat::SpatialWhereAvailable, None),
            [true, true, false, false]
        );
    }

    #[test]
    fn metadata_traits_should_tell_atmos_songs() {
        assert_eq!(
            [
                spatial_of_metadata(Some(
                    r#"{"attributes":{"audioTraits":["atmos","lossless"]}}"#
                )),
                spatial_of_metadata(Some(r#"{"attributes":{"audioTraits":["lossy-stereo"]}}"#)),
                spatial_of_metadata(Some(r#"{"attributes":{}}"#)),
                spatial_of_metadata(None),
            ],
            [Some(true), Some(false), None, None]
        );
    }

    #[test]
    fn learned_spatial_should_win_over_metadata() {
        let mut track = Track::minimal("1", "Song", "Artist");
        track.metadata = Some(r#"{"attributes":{"audioTraits":["atmos"]}}"#.to_owned());
        track.audio = crate::storage::TrackAudio::normal("media/1.m4a");
        assert!(!track_needs_download(
            &track,
            DownloadFormat::Both,
            Some(false)
        ));
    }

    #[test]
    fn custom_songs_should_never_need_a_download() {
        let mut track = Track::minimal("local-1", "Mine", "Me");
        track.is_custom = true;
        assert!(!track_needs_download(&track, DownloadFormat::Stereo, None));
    }

    fn song(id: &str, audio: &[AudioVariant]) -> Track {
        let mut track = Track::minimal(id, "Song", "Artist");
        for variant in audio {
            let path = match variant {
                AudioVariant::Stereo => format!("media/{id}.m4a"),
                AudioVariant::Spatial => format!("media/{id}.spatial.m4a"),
            };
            track.audio.set(*variant, Some(path.into()));
        }
        track
    }

    /// The summary of `tracks`, every one pinned by the collection.
    fn summary(
        tracks: &[Track],
        format: DownloadFormat,
        known: Option<bool>,
    ) -> CollectionDownload {
        CollectionDownload::of(tracks.iter().map(|t| (t, known, true)), format)
    }

    /// The Stereo summary of `tracks` where the collection pins the first
    /// `linked`.
    fn linked_summary(tracks: &[Track], linked: usize) -> CollectionDownload {
        CollectionDownload::of(
            tracks
                .iter()
                .enumerate()
                .map(|(n, t)| (t, None, n < linked)),
            DownloadFormat::Stereo,
        )
    }

    #[test]
    fn collection_without_files_should_not_be_downloaded() {
        let tracks = [song("1", &[]), song("2", &[])];
        let summary = summary(&tracks, DownloadFormat::Stereo, None);
        assert_eq!(
            (summary.state(), summary.complete, summary.songs),
            (CollectionDownloadState::NotDownloaded, 0, 2)
        );
    }

    #[test]
    fn collection_with_some_files_should_be_partial() {
        let tracks = [song("1", STEREO), song("2", &[]), song("3", &[])];
        let summary = summary(&tracks, DownloadFormat::Stereo, None);
        assert_eq!(
            (summary.state(), summary.complete, summary.songs),
            (CollectionDownloadState::Partial, 1, 3)
        );
    }

    #[test]
    fn collection_with_every_file_should_be_downloaded() {
        let tracks = [song("1", STEREO), song("2", BOTH)];
        assert_eq!(
            summary(&tracks, DownloadFormat::Stereo, None).state(),
            CollectionDownloadState::Downloaded
        );
    }

    #[test]
    fn collection_missing_a_wanted_mix_should_be_partial_with_no_complete_song() {
        // Stereo + Spatial, Atmos songs with only their stereo file: files
        // exist, but no song is complete.
        let tracks = [song("1", STEREO), song("2", STEREO)];
        let summary = summary(&tracks, DownloadFormat::Both, Some(true));
        assert_eq!(
            (summary.state(), summary.complete),
            (CollectionDownloadState::Partial, 0)
        );
    }

    #[test]
    fn custom_songs_should_not_count_towards_a_collection() {
        let mut custom = song("local-1", STEREO);
        custom.is_custom = true;
        let tracks = [custom, song("2", &[])];
        let summary = summary(&tracks, DownloadFormat::Stereo, None);
        assert_eq!(
            (summary.state(), summary.songs),
            (CollectionDownloadState::NotDownloaded, 1)
        );
    }

    #[test]
    fn collection_of_only_custom_songs_should_not_be_downloaded() {
        let mut custom = song("local-1", STEREO);
        custom.is_custom = true;
        assert_eq!(
            summary(&[custom], DownloadFormat::Stereo, None).state(),
            CollectionDownloadState::NotDownloaded
        );
    }

    #[test]
    fn collection_with_every_file_pinned_by_others_should_be_saved_elsewhere() {
        let tracks = [song("1", STEREO), song("2", STEREO)];
        assert_eq!(
            linked_summary(&tracks, 0).state(),
            CollectionDownloadState::SavedElsewhere
        );
    }

    #[test]
    fn collection_with_every_file_partly_linked_should_be_saved_elsewhere() {
        let tracks = [song("1", STEREO), song("2", STEREO), song("3", STEREO)];
        assert_eq!(
            linked_summary(&tracks, 2).state(),
            CollectionDownloadState::SavedElsewhere
        );
    }

    #[test]
    fn collection_with_every_file_linked_should_be_downloaded() {
        let tracks = [song("1", STEREO), song("2", STEREO)];
        assert_eq!(
            linked_summary(&tracks, 2).state(),
            CollectionDownloadState::Downloaded
        );
    }

    #[test]
    fn collection_missing_files_should_stay_partial_whatever_it_pins() {
        let tracks = [song("1", STEREO), song("2", &[])];
        assert_eq!(
            [
                linked_summary(&tracks, 0).state(),
                linked_summary(&tracks, 2).state()
            ],
            [CollectionDownloadState::Partial; 2]
        );
    }

    #[test]
    fn custom_songs_should_not_need_a_link() {
        let mut custom = song("local-1", STEREO);
        custom.is_custom = true;
        let tracks = [song("2", STEREO), custom];
        assert_eq!(
            linked_summary(&tracks, 1).state(),
            CollectionDownloadState::Downloaded
        );
    }
}
