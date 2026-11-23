//! Version-aware playback routing: which version of a song Cider gets, and
//! from where.
//!
//! # What Cider does (from the 2026-11-20 log and captures)
//!
//! For **every** song it plays, Cider (`MusicKit`) first POSTs `webPlayback`
//! (`{"universalLibraryId":"a.<id>","subscriptionAdamId":"<id>"}` or
//! `{"salableAdamId":"<id>"}`), then GETs
//! `/v1/catalog/<sf>/songs/<id>?extend=extendedAssetUrls`, and then takes one
//! of two paths:
//!
//! - **normal**: the `WebPlayback` asset (`mzaf_….rphq.aac.wa.m3u8` from
//!   Apple, or our local media URL);
//! - **enhanced / spatial**: the binaural variant of the catalog
//!   `extendedAssetUrls.enhancedHls` master
//!   (`…/P<n>_A<id>_audio_en_gr256_mp4a-40-2_bm.m3u8`), unlocked with the
//!   key-server URLs of the `WebPlayback` answer. Cider takes this path only
//!   for Dolby Atmos songs (`audioTraits` contains `atmos`) while its spatial
//!   setting (`audio.atmos.enabled` in Cider's `spa-config.yml`) is on. The
//!   master itself is usually served from Chromium's HTTP cache
//!   (`max-age=2592000`), so the proxy often sees only the variant.
//!
//! So `webPlayback` arrives before Cider shows which path it takes, and the
//! enhanced path still depends on it for the keys. Answering `webPlayback`
//! locally (no keys) while Cider goes on to the enhanced path is the restart
//! bug: the encrypted stream can't be unlocked and `MusicKit` retries forever.
//!
//! # The rule
//!
//! Cider *wants spatial* for a song when its spatial setting is on (unknown
//! counts as on) and the song is Atmos ([`Router::wants_spatial`]); otherwise
//! it wants normal. With that, [`source_for`] decides where the wanted
//! version comes from:
//!
//! 1. it is downloaded: the local file, always;
//! 2. only the other version is downloaded: the local other version when
//!    "Play the other downloaded mix" is on or Apple is unreachable, else
//!    Apple (Cider streams the version it asked for);
//! 3. nothing is downloaded: Apple online, unavailable offline.
//!
//! Each request type applies that rule its own way so the two paths stay
//! consistent:
//!
//! - [`web_playback_route`]: local answer (with the file it should play if
//!   Cider takes the normal path) or Apple's real answer. When the result
//!   depends on whether Cider wants spatial and that is unknown (the song's
//!   traits were never seen and the catalog lookup failed), Apple's answer
//!   is used: it carries the keys, so both paths work.
//! - [`asset_urls_route`], applied to every song resource in API answers:
//!   keep `extendedAssetUrls` (Cider may stream from Apple), strip it (Cider
//!   stays on the `WebPlayback` path, which points at the local file), or
//!   point `enhancedHls` at the local spatial playlist ([`crate::hls`]).
//!   What `webPlayback` was answered for the song overrides it: after a
//!   local answer Cider must never reach Apple's encrypted stream.
//! - [`enhanced_route`], for Apple's enhanced-HLS playlists that still reach
//!   the proxy (cached catalog answers, the browser's cached master): a
//!   local playlist when the rule picks a local file, or when `webPlayback`
//!   was answered locally; otherwise Apple.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use bytes::Bytes;
use hudsucker::Body;
use hudsucker::hyper::Method;
use serde_json::{Map, Value};

use crate::apple;
use crate::catalog::CatalogClient;
use crate::hls;
use crate::interceptor_prelude::*;
use crate::respond::{BufferOutcome, buffer_decoded, content_type_essence};
use crate::storage::{AudioVariant, Storage, StorageError};

/// The song attribute holding Apple's direct stream URLs.
const ASSET_URLS: &str = "extendedAssetUrls";
/// The `extendedAssetUrls` entry Cider's spatial path plays.
const ENHANCED_HLS: &str = "enhancedHls";
/// Entries kept in each of the router's memories before it is cleared.
const MEMORY_CAPACITY: usize = 4096;
/// How long a `webPlayback` answer is remembered for a song.
const ANSWER_TTL: Duration = Duration::from_hours(1);
/// Longest wait for the catalog lookup deciding a `webPlayback` answer.
const TRAITS_LOOKUP_TIMEOUT: Duration = Duration::from_secs(3);

/// Which local versions a song has.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct LocalVariants {
    /// A normal file (download, mapping or custom song file).
    pub(crate) normal: bool,
    /// A spatial download.
    pub(crate) spatial: bool,
}

impl LocalVariants {
    /// Whether `variant` is local.
    pub(crate) fn has(self, variant: AudioVariant) -> bool {
        match variant {
            AudioVariant::Stereo => self.normal,
            AudioVariant::Spatial => self.spatial,
        }
    }

    /// Whether any version is local.
    pub(crate) fn any(self) -> bool {
        self.normal || self.spatial
    }

    /// The variants in `list`.
    pub(crate) fn from_list(list: &[AudioVariant]) -> Self {
        Self {
            normal: list.contains(&AudioVariant::Stereo),
            spatial: list.contains(&AudioVariant::Spatial),
        }
    }
}

impl std::fmt::Display for LocalVariants {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match (self.normal, self.spatial) {
            (true, true) => "stereo+spatial",
            (true, false) => "stereo",
            (false, true) => "spatial",
            (false, false) => "none",
        })
    }
}

/// The conditions a decision is taken under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Conditions {
    /// Whether Apple Music is reachable.
    pub(crate) online: bool,
    /// "Play the other downloaded mix".
    pub(crate) play_other: bool,
}

/// Where the version Cider wants comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Source {
    /// This local file.
    Local(AudioVariant),
    /// Apple's stream.
    Apple,
    /// Nowhere (offline, nothing downloaded).
    Unavailable,
}

/// The variant other than `variant`.
fn other(variant: AudioVariant) -> AudioVariant {
    match variant {
        AudioVariant::Stereo => AudioVariant::Spatial,
        AudioVariant::Spatial => AudioVariant::Stereo,
    }
}

/// The routing rule (see the [module docs](self)).
pub(crate) fn source_for(wanted: AudioVariant, local: LocalVariants, when: Conditions) -> Source {
    if local.has(wanted) {
        Source::Local(wanted)
    } else if local.has(other(wanted)) && (when.play_other || !when.online) {
        Source::Local(other(wanted))
    } else if when.online {
        Source::Apple
    } else {
        Source::Unavailable
    }
}

/// How a `webPlayback` request is answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WebPlaybackRoute {
    /// Locally, with the media URL of this file.
    Local(AudioVariant),
    /// Apple's real answer.
    Apple,
    /// "Not available offline".
    Unavailable,
}

impl std::fmt::Display for WebPlaybackRoute {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Local(variant) => write!(f, "local {variant}"),
            Self::Apple => f.write_str("Apple"),
            Self::Unavailable => f.write_str("unavailable"),
        }
    }
}

/// The variant Cider wants: spatial when it wants spatial, else normal.
fn wanted(wants_spatial: bool) -> AudioVariant {
    if wants_spatial {
        AudioVariant::Spatial
    } else {
        AudioVariant::Stereo
    }
}

/// The `webPlayback` answer when it is known whether Cider wants spatial.
fn web_playback_route_for(
    local: LocalVariants,
    wants_spatial: bool,
    when: Conditions,
) -> WebPlaybackRoute {
    match source_for(wanted(wants_spatial), local, when) {
        // Cider plays the spatial file through the enhanced path (the local
        // playlist); should it take the normal path after all, it gets the
        // normal file it asks for there, if downloaded.
        Source::Local(AudioVariant::Spatial) if wants_spatial && local.normal => {
            WebPlaybackRoute::Local(AudioVariant::Stereo)
        }
        Source::Local(variant) => WebPlaybackRoute::Local(variant),
        Source::Apple => WebPlaybackRoute::Apple,
        Source::Unavailable => WebPlaybackRoute::Unavailable,
    }
}

/// Whether the `webPlayback` answer depends on whether Cider wants spatial.
pub(crate) fn web_playback_depends_on_wish(local: LocalVariants, when: Conditions) -> bool {
    web_playback_route_for(local, true, when) != web_playback_route_for(local, false, when)
}

/// How to answer `webPlayback` for a song with `local` versions;
/// `wants_spatial` is `None` when unknown.
pub(crate) fn web_playback_route(
    local: LocalVariants,
    wants_spatial: Option<bool>,
    when: Conditions,
) -> WebPlaybackRoute {
    match wants_spatial {
        Some(wish) => web_playback_route_for(local, wish, when),
        // Both wishes give the same answer (offline, the setting on, or both
        // versions local): no need to know.
        None if !web_playback_depends_on_wish(local, when) => {
            web_playback_route_for(local, false, when)
        }
        // Apple's answer carries the keys, so whichever path Cider takes
        // works (it streams instead of playing the local file).
        None => WebPlaybackRoute::Apple,
    }
}

/// What to do with a song resource's `extendedAssetUrls`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AssetUrlsRoute {
    /// Leave Apple's URLs.
    Keep,
    /// Remove them, so Cider plays through `webPlayback`.
    Strip,
    /// Point `enhancedHls` at the local spatial playlist.
    LocalSpatial,
}

impl std::fmt::Display for AssetUrlsRoute {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Keep => "keep Apple stream URLs",
            Self::Strip => "strip stream URLs",
            Self::LocalSpatial => "point enhancedHls at the local spatial playlist",
        })
    }
}

/// The `extendedAssetUrls` rewrite for a song with `local` versions, given
/// whether Cider wants spatial and how `webPlayback` was last answered.
pub(crate) fn asset_urls_route(
    local: LocalVariants,
    wants_spatial: bool,
    when: Conditions,
    answered: Option<WebPlaybackRoute>,
) -> AssetUrlsRoute {
    if !local.any() {
        return AssetUrlsRoute::Keep;
    }
    let route = match (
        wants_spatial,
        source_for(wanted(wants_spatial), local, when),
    ) {
        (true, Source::Local(AudioVariant::Spatial)) => AssetUrlsRoute::LocalSpatial,
        (_, Source::Local(_)) => AssetUrlsRoute::Strip,
        (_, Source::Apple | Source::Unavailable) => AssetUrlsRoute::Keep,
    };
    match (route, answered) {
        // Cider has Apple's keys: let it stream the version it wants.
        (AssetUrlsRoute::Strip, Some(WebPlaybackRoute::Apple)) => AssetUrlsRoute::Keep,
        // Answered locally, without keys: Apple's encrypted stream would fail
        // and restart, so Cider must stay on the local file.
        (AssetUrlsRoute::Keep, Some(WebPlaybackRoute::Local(_))) => AssetUrlsRoute::Strip,
        (route, _) => route,
    }
}

/// How to answer a request for one of Apple's enhanced-HLS playlists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EnhancedRoute {
    /// With a local playlist of this file.
    Local(AudioVariant),
    /// Pass it on (to Apple, or the offline responder).
    Apple,
}

impl std::fmt::Display for EnhancedRoute {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Local(variant) => write!(f, "local {variant} playlist"),
            Self::Apple => f.write_str("Apple"),
        }
    }
}

/// The answer to an enhanced-HLS playlist request (Cider wants spatial) for
/// a song with `local` versions.
pub(crate) fn enhanced_route(
    local: LocalVariants,
    when: Conditions,
    answered: Option<WebPlaybackRoute>,
) -> EnhancedRoute {
    match source_for(AudioVariant::Spatial, local, when) {
        Source::Local(variant) => EnhancedRoute::Local(variant),
        // `webPlayback` was answered locally, so Apple's stream has no keys:
        // play what is local rather than fail and restart.
        Source::Apple | Source::Unavailable => match answered {
            Some(WebPlaybackRoute::Local(_)) if local.spatial => {
                EnhancedRoute::Local(AudioVariant::Spatial)
            }
            Some(WebPlaybackRoute::Local(_)) if local.normal => {
                EnhancedRoute::Local(AudioVariant::Stereo)
            }
            _ => EnhancedRoute::Apple,
        },
    }
}

/// Cider's spatial audio setting, as last read from its config.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CiderSpatial {
    /// Not known (no config found); treated as on.
    Unknown,
    /// Spatial audio (Dolby Atmos) is off.
    Off,
    /// Spatial audio (Dolby Atmos) is on.
    On,
}

impl CiderSpatial {
    /// From an optional reading of Cider's setting.
    #[must_use]
    pub fn from_setting(enabled: Option<bool>) -> Self {
        match enabled {
            None => Self::Unknown,
            Some(false) => Self::Off,
            Some(true) => Self::On,
        }
    }

    fn as_u8(self) -> u8 {
        match self {
            Self::Unknown => 0,
            Self::Off => 1,
            Self::On => 2,
        }
    }

    fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::Off,
            2 => Self::On,
            _ => Self::Unknown,
        }
    }
}

/// A map that forgets everything once it holds [`MEMORY_CAPACITY`] entries.
#[derive(Debug)]
struct Memory<V> {
    entries: HashMap<String, V>,
}

impl<V> Default for Memory<V> {
    fn default() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }
}

impl<V> Memory<V> {
    fn insert(&mut self, key: String, value: V) {
        if self.entries.len() >= MEMORY_CAPACITY && !self.entries.contains_key(&key) {
            self.entries.clear();
        }
        self.entries.insert(key, value);
    }

    fn get(&self, key: &str) -> Option<&V> {
        self.entries.get(key)
    }
}

/// What the router has learned from the traffic.
#[derive(Debug, Default)]
struct Learned {
    /// Song id → whether it is a Dolby Atmos song.
    atmos: Memory<bool>,
    /// [`hls::url_key`] of an `enhancedHls` master → its song id.
    masters: Memory<String>,
    /// Song id → the last `webPlayback` answer and when it was given.
    answers: Memory<(WebPlaybackRoute, Instant)>,
}

/// Shared routing state: live settings and what was learned from responses.
/// Cheap to clone.
#[derive(Debug, Clone)]
pub(crate) struct Router {
    play_other: Arc<AtomicBool>,
    cider_spatial: Arc<AtomicU8>,
    catalog: Option<CatalogClient>,
    learned: Arc<Mutex<Learned>>,
}

impl Router {
    /// A router reading "Play the other downloaded mix" from `play_other`
    /// and Cider's setting from `cider_spatial` ([`CiderSpatial`]).
    pub(crate) fn new(play_other: Arc<AtomicBool>, cider_spatial: Arc<AtomicU8>) -> Self {
        Self {
            play_other,
            cider_spatial,
            catalog: None,
            learned: Arc::default(),
        }
    }

    /// Looks unknown songs' traits up in the catalog when a `webPlayback`
    /// answer depends on them.
    pub(crate) fn with_catalog(mut self, catalog: CatalogClient) -> Self {
        self.catalog = Some(catalog);
        self
    }

    /// The live [`CiderSpatial`] cell, as stored.
    pub(crate) fn cider_spatial_cell(value: CiderSpatial) -> Arc<AtomicU8> {
        Arc::new(AtomicU8::new(value.as_u8()))
    }

    /// Stores Cider's spatial setting into the live cell; returns whether it
    /// changed.
    pub(crate) fn store_cider_spatial(cell: &AtomicU8, value: CiderSpatial) -> bool {
        cell.swap(value.as_u8(), Ordering::Relaxed) != value.as_u8()
    }

    /// Cider's spatial setting.
    pub(crate) fn cider_spatial(&self) -> CiderSpatial {
        CiderSpatial::from_u8(self.cider_spatial.load(Ordering::Relaxed))
    }

    /// The current conditions.
    pub(crate) fn conditions(&self, online: bool) -> Conditions {
        Conditions {
            online,
            play_other: self.play_other.load(Ordering::Relaxed),
        }
    }

    fn learned(&self) -> std::sync::MutexGuard<'_, Learned> {
        // The maps stay consistent across a panic (single inserts).
        self.learned.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Whether Cider wants the spatial version of a song with the given
    /// Atmos-ness: its setting is on (or unknown) and the song is Atmos.
    /// `None` when the setting allows it but the song's traits are unknown.
    pub(crate) fn wants_spatial(&self, atmos: Option<bool>) -> Option<bool> {
        match self.cider_spatial() {
            CiderSpatial::Off => Some(false),
            CiderSpatial::On | CiderSpatial::Unknown => atmos,
        }
    }

    /// Whether any of `ids` is known to be an Atmos song.
    pub(crate) fn known_atmos<'a>(&self, ids: impl IntoIterator<Item = &'a str>) -> Option<bool> {
        let learned = self.learned();
        ids.into_iter()
            .find_map(|id| learned.atmos.get(id).copied())
    }

    /// Records the Atmos-ness of a song under each of `ids`.
    pub(crate) fn learn_atmos<'a>(&self, ids: impl IntoIterator<Item = &'a str>, atmos: bool) {
        let mut learned = self.learned();
        for id in ids {
            learned.atmos.insert(id.to_owned(), atmos);
        }
    }

    /// Looks the Atmos-ness of catalog song `catalog_id` up, bounded by
    /// [`TRAITS_LOOKUP_TIMEOUT`]; `None` when that fails or the catalog
    /// lists no traits.
    pub(crate) async fn lookup_atmos(&self, catalog_id: &str) -> Option<bool> {
        let catalog = self.catalog.as_ref()?;
        match tokio::time::timeout(TRAITS_LOOKUP_TIMEOUT, catalog.is_atmos(catalog_id)).await {
            Ok(Ok(atmos)) => atmos,
            Ok(Err(error)) => {
                tracing::debug!(catalog = %catalog_id, %error, "routing: traits lookup failed");
                None
            }
            Err(_) => {
                tracing::debug!(catalog = %catalog_id, "routing: traits lookup timed out");
                None
            }
        }
    }

    /// Remembers that `master_url` is the `enhancedHls` master of song `id`.
    pub(crate) fn learn_master(&self, master_url: &str, id: &str) {
        if let Some(key) = hls::url_key_of(master_url) {
            self.learned().masters.insert(key, id.to_owned());
        }
    }

    /// The song whose `enhancedHls` master has [`hls::url_key`] `key`.
    pub(crate) fn master_song(&self, key: &str) -> Option<String> {
        self.learned().masters.get(key).cloned()
    }

    /// Remembers how `webPlayback` was answered for the song named by `ids`.
    pub(crate) fn remember_answer<'a>(
        &self,
        ids: impl IntoIterator<Item = &'a str>,
        route: WebPlaybackRoute,
    ) {
        let now = Instant::now();
        let mut learned = self.learned();
        for id in ids {
            learned.answers.insert(id.to_owned(), (route, now));
        }
    }

    /// The last `webPlayback` answer (within [`ANSWER_TTL`]) for any of `ids`.
    pub(crate) fn answered<'a>(
        &self,
        ids: impl IntoIterator<Item = &'a str>,
    ) -> Option<WebPlaybackRoute> {
        let learned = self.learned();
        ids.into_iter()
            .filter_map(|id| learned.answers.get(id))
            .filter(|(_, at)| at.elapsed() < ANSWER_TTL)
            .max_by_key(|(_, at)| *at)
            .map(|(route, _)| *route)
    }
}

/// Whether `map` is a song resource.
fn is_song(map: &Map<String, Value>) -> bool {
    map.get("type")
        .and_then(Value::as_str)
        .is_some_and(|kind| kind == "songs" || kind == "library-songs")
}

/// The ids naming a song resource: its own id and its play params' catalog
/// and item ids (a library song's catalog twin).
fn resource_ids(map: &Map<String, Value>) -> Vec<&str> {
    let play_params = map.get("attributes").and_then(|a| a.get("playParams"));
    let mut ids: Vec<&str> = [
        map.get("id"),
        play_params.and_then(|p| p.get("catalogId")),
        play_params.and_then(|p| p.get("id")),
    ]
    .into_iter()
    .flatten()
    .filter_map(Value::as_str)
    .filter(|id| !id.is_empty())
    .collect();
    ids.dedup();
    ids
}

/// The Atmos-ness a song resource's `audioTraits` state, if listed.
fn atmos_of(map: &Map<String, Value>) -> Option<bool> {
    map.get("attributes")?
        .get("audioTraits")?
        .as_array()
        .map(|traits| traits.iter().any(|t| t.as_str() == Some("atmos")))
}

fn has_asset_urls(map: &Map<String, Value>) -> bool {
    map.get("attributes")
        .and_then(Value::as_object)
        .is_some_and(|attributes| attributes.contains_key(ASSET_URLS))
}

/// Visits every song resource in `document`.
fn for_each_song(document: &mut Value, visit: &mut impl FnMut(&mut Map<String, Value>)) {
    match document {
        Value::Object(map) => {
            if is_song(map) {
                visit(map);
            }
            for value in map.values_mut() {
                for_each_song(value, visit);
            }
        }
        Value::Array(items) => {
            for item in items {
                for_each_song(item, visit);
            }
        }
        _ => {}
    }
}

/// Learns the traits and `enhancedHls` masters of every song resource in
/// `document`, and returns the ids of those carrying `extendedAssetUrls`.
fn learn(router: &Router, document: &mut Value) -> Vec<String> {
    let mut candidates = Vec::new();
    for_each_song(document, &mut |map| {
        let ids = resource_ids(map);
        if let Some(atmos) = atmos_of(map) {
            router.learn_atmos(ids.iter().copied(), atmos);
        }
        if has_asset_urls(map) {
            if let (Some(master), Some(id)) = (
                map.get("attributes")
                    .and_then(|a| a.get(ASSET_URLS))
                    .and_then(|urls| urls.get(ENHANCED_HLS))
                    .and_then(Value::as_str),
                ids.first(),
            ) {
                router.learn_master(master, id);
            }
            candidates.extend(ids.iter().map(|id| (*id).to_owned()));
        }
    });
    candidates
}

/// One rewritten song resource, for logging.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Rewrite {
    /// The resource id.
    pub(crate) id: String,
    /// What was done.
    pub(crate) route: AssetUrlsRoute,
}

/// Learns from `document` and applies [`asset_urls_route`] to its song
/// resources. Returns what was changed.
///
/// # Errors
///
/// Returns database errors.
pub(crate) async fn rewrite_document(
    router: &Router,
    storage: &Storage,
    document: &mut Value,
    online: bool,
) -> Result<Vec<Rewrite>, StorageError> {
    let mut ids = learn(router, document);
    ids.sort_unstable();
    ids.dedup();
    let local = existing_variants(storage, storage.local_variants(ids).await?).await?;
    let mut changes = Vec::new();
    if local.is_empty() {
        return Ok(changes);
    }
    let when = router.conditions(online);
    for_each_song(document, &mut |map| {
        if !has_asset_urls(map) {
            return;
        }
        let ids = resource_ids(map);
        let Some((media_id, variants)) = ids
            .iter()
            .find_map(|id| local.get(*id).map(|v| (*id, LocalVariants::from_list(v))))
        else {
            return;
        };
        let atmos = atmos_of(map).or_else(|| router.known_atmos(ids.iter().copied()));
        // Unknown traits count as Atmos: Cider might take the enhanced path.
        let wants_spatial = router.wants_spatial(atmos).unwrap_or(true);
        let answered = router.answered(ids.iter().copied());
        let route = asset_urls_route(variants, wants_spatial, when, answered);
        tracing::debug!(
            track = %media_id,
            requested = %wanted(wants_spatial),
            available = %variants,
            play_other = when.play_other,
            online = when.online,
            web_playback = %answered.map_or_else(|| "-".to_owned(), |r| r.to_string()),
            decision = %route,
            "routing: catalog stream URLs"
        );
        let media_id = media_id.to_owned();
        let Some(attributes) = map.get_mut("attributes").and_then(Value::as_object_mut) else {
            return;
        };
        match route {
            AssetUrlsRoute::Keep => return,
            AssetUrlsRoute::Strip => {
                attributes.remove(ASSET_URLS);
            }
            AssetUrlsRoute::LocalSpatial => {
                let mut urls = Map::new();
                urls.insert(
                    ENHANCED_HLS.to_owned(),
                    Value::String(hls::local_master_url(&media_id, AudioVariant::Spatial)),
                );
                attributes.insert(ASSET_URLS.to_owned(), Value::Object(urls));
            }
        }
        changes.push(Rewrite {
            id: media_id,
            route,
        });
    });
    Ok(changes)
}

/// Keeps only the variants of `recorded` (from the database) whose file is on
/// disk, as the webPlayback answer and the media server see them: a recorded
/// spatial file that is gone must not send Cider to a local playlist that
/// can't be served.
async fn existing_variants(
    storage: &Storage,
    recorded: HashMap<String, Vec<AudioVariant>>,
) -> Result<HashMap<String, Vec<AudioVariant>>, StorageError> {
    let resolver = crate::media::MediaResolver::new(storage.clone());
    let mut existing = HashMap::with_capacity(recorded.len());
    for id in recorded.into_keys() {
        let files = resolver.files(&id).await?;
        let variants: Vec<AudioVariant> = [AudioVariant::Stereo, AudioVariant::Spatial]
            .into_iter()
            .filter(|variant| files.get(*variant).is_some())
            .collect();
        if !variants.is_empty() {
            existing.insert(id, variants);
        }
    }
    Ok(existing)
}

/// Rewrites `extendedAssetUrls` in every Apple API JSON answer, upstream or
/// answered locally (see the [module docs](self)), and learns song traits
/// and `enhancedHls` masters from them.
#[derive(Debug, Clone)]
pub(crate) struct AssetRoutingInterceptor {
    router: Router,
}

impl AssetRoutingInterceptor {
    pub(crate) fn new(router: Router) -> Self {
        Self { router }
    }
}

/// Whether `body` contains `needle`.
fn mentions(body: &[u8], needle: &str) -> bool {
    body.windows(needle.len()).any(|w| w == needle.as_bytes())
}

#[async_trait]
impl Interceptor for AssetRoutingInterceptor {
    fn name(&self) -> &'static str {
        "asset-routing"
    }

    async fn on_response(
        &self,
        exchange: &mut Exchange,
        response: Response<Body>,
    ) -> Response<Body> {
        if *exchange.method() != Method::GET
            || !apple::is_api_host(exchange.host())
            || !response.status().is_success()
        {
            return response;
        }
        let is_json = content_type_essence(response.headers()).is_some_and(|t| t.ends_with("json"));
        if !is_json {
            return response;
        }
        let buffered = match buffer_decoded(response).await {
            BufferOutcome::Buffered(buffered) => buffered,
            BufferOutcome::PassThrough(response) => return response,
        };
        if !mentions(&buffered.body, ASSET_URLS) && !mentions(&buffered.body, "audioTraits") {
            return buffered.into_response();
        }
        let Ok(mut document) = serde_json::from_slice::<Value>(&buffered.body) else {
            return buffered.into_response();
        };
        match rewrite_document(
            &self.router,
            exchange.storage(),
            &mut document,
            exchange.online(),
        )
        .await
        {
            Ok(changes) if !changes.is_empty() => {
                buffered.into_response_with(Bytes::from(document.to_string()))
            }
            Ok(_) => buffered.into_response(),
            Err(error) => {
                tracing::warn!(%error, "local audio lookup failed");
                buffered.into_response()
            }
        }
    }
}

/// Ids worth remembering for a `webPlayback` request: every id it names.
pub(crate) fn play_keys<'a>(ids: impl IntoIterator<Item = Option<&'a str>>) -> Vec<&'a str> {
    let mut seen = HashSet::new();
    ids.into_iter()
        .flatten()
        .filter(|id| !id.is_empty() && seen.insert(*id))
        .collect()
}

#[cfg(test)]
mod tests;
