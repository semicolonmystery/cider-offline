//! Offline answers for the Apple Music JSON API, synthesized from the
//! [`LocalIndex`]: the library (playlists, albums, songs, artists, recently
//! added, library search), the catalog lookups Cider makes from library views
//! (songs, albums, playlists, multi-type `ids[...]` lookups), catalog search
//! and suggestions, the storefront, and harmless empty answers for the
//! personalised endpoints Cider calls while rendering a page.

use hudsucker::hyper::{Method, StatusCode};
use serde_json::{Value, json};

use super::index::{LocalCollection, LocalIndex};
use super::query::{DEFAULT_LIMIT, MAX_LIMIT, Query};
use super::render::{Render, page_document, to_resource_map};
use super::search;
use crate::apple::segments;
use crate::config::SongsPage;
use crate::storage::CollectionKind;

/// Default page size of the playlist-folder listing Cider pages through.
const FOLDER_LIMIT: usize = 100;
/// Default page size of `recent/played`.
const RECENT_LIMIT: usize = 10;
/// Largest page of a track listing.
const MAX_TRACKS_PAGE: usize = 300;

/// An offline API answer.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Answer {
    /// `200` with this JSON document.
    Json(Value),
    /// The requested resource is not known locally (`404`, Apple error shape).
    NotFound,
    /// Not an endpoint the index answers; the caller may replay a cached
    /// response or fall back to an empty document.
    Unowned,
}

impl Answer {
    pub(crate) fn status(&self) -> StatusCode {
        match self {
            Self::Json(_) | Self::Unowned => StatusCode::OK,
            Self::NotFound => StatusCode::NOT_FOUND,
        }
    }
}

/// Apple's `404` body.
pub(crate) fn not_found_document() -> Value {
    json!({"errors": [{
        "id": "offline",
        "title": "Resource Not Found",
        "detail": "Resource with requested id was not found (offline)",
        "status": "404",
        "code": "40400",
    }]})
}

/// Minimal storefront document for `id` (used when none was saved).
pub(crate) fn storefront_document(id: &str, language: &str) -> Value {
    json!({"data": [{
        "id": id,
        "type": "storefronts",
        "href": format!("/v1/storefronts/{id}"),
        "attributes": {
            "defaultLanguageTag": language,
            "explicitContentPolicy": "allowed",
            "name": id.to_uppercase(),
            "supportedLanguageTags": [language],
        },
    }]})
}

/// Whether `path?query` is the library Songs page listing (not an id lookup),
/// the one endpoint [`SongsPage`] changes.
pub(crate) fn is_songs_listing(path: &str, query: &Query) -> bool {
    segments(path).eq(["v1", "me", "library", "songs"]) && ids_param(query).is_none()
}

/// Answers `method path?query` from `index`; the library Songs listing shows
/// `songs_page`.
pub(crate) fn answer(
    index: &LocalIndex,
    method: &Method,
    path: &str,
    query: &Query,
    songs_page: SongsPage,
) -> Answer {
    let parts: Vec<&str> = segments(path).collect();
    if *method == Method::POST {
        return match parts.as_slice() {
            ["v1", "me", "recommendations", "suggested"] => {
                Answer::Json(json!({"results": {"suggested": []}}))
            }
            _ => Answer::Unowned,
        };
    }
    if *method != Method::GET && *method != Method::HEAD {
        return Answer::Unowned;
    }
    let account = index.storefront.as_str();
    match parts.as_slice() {
        ["v1", "me", "storefront"] => Answer::Json(my_storefront(index, query)),
        ["v1", "storefronts"] => {
            let mut document = my_storefront(index, query);
            if query.wants_resource_map() {
                to_resource_map(&mut document);
            }
            Answer::Json(document)
        }
        ["v1", "storefronts", id] if *id == account => Answer::Json(my_storefront(index, query)),
        ["v1", "me", "library", rest @ ..] => {
            let render = Render {
                index,
                query,
                storefront: account,
            };
            library(&render, rest, songs_page)
        }
        ["v1", "catalog", storefront] => {
            let render = Render {
                index,
                query,
                storefront,
            };
            Answer::Json(typed_lookup(&render))
        }
        ["v1", "catalog", storefront, rest @ ..] => {
            let render = Render {
                index,
                query,
                storefront,
            };
            catalog(&render, rest)
        }
        ["v1", "me", "recent", "played"] => {
            let render = Render {
                index,
                query,
                storefront: account,
            };
            let mut document = recent(&render, "/v1/me/recent/played", RECENT_LIMIT, |c| {
                c.last_modified.clone()
            });
            // Recently played lists catalog resources.
            catalog_flavour(&render, &mut document);
            Answer::Json(document)
        }
        ["v1", "social", "badging-map"] => Answer::Json(json!({"results": {"badgingMap": {}}})),
        ["v1", "social", _, "search"] => Answer::Json(json!({
            "results": {},
            "meta": {"results": {"order": [], "rawOrder": []}},
        })),
        _ => Answer::Unowned,
    }
}

fn my_storefront(index: &LocalIndex, query: &Query) -> Value {
    index.storefront_document.clone().unwrap_or_else(|| {
        storefront_document(&index.storefront, query.locale().unwrap_or("en-US"))
    })
}

// ----- Library ---------------------------------------------------------------

#[expect(clippy::too_many_lines, reason = "one flat route table")]
fn library(render: &Render<'_>, rest: &[&str], songs_page: SongsPage) -> Answer {
    let index = render.index;
    let query = render.query;
    match rest {
        ["playlist-folders", folder, "children"] => {
            if *folder == "p.playlistsroot" {
                Answer::Json(collection_page(
                    render,
                    &format!("/v1/me/library/playlist-folders/{folder}/children"),
                    index.playlists().collect::<Vec<_>>().as_slice(),
                    FOLDER_LIMIT,
                ))
            } else {
                Answer::Json(json!({"data": [], "meta": {"total": 0}}))
            }
        }
        ["playlists"] => {
            if let Some(ids) = ids_param(query) {
                return Answer::Json(
                    json!({"data": collections_by_ids(render, &ids, CollectionKind::Playlist)}),
                );
            }
            if query.get("filter[featured]").is_some() {
                return Answer::Json(suggested_playlists(render));
            }
            Answer::Json(collection_page(
                render,
                "/v1/me/library/playlists",
                index.playlists().collect::<Vec<_>>().as_slice(),
                DEFAULT_LIMIT,
            ))
        }
        ["albums"] => {
            if let Some(ids) = ids_param(query) {
                return Answer::Json(
                    json!({"data": collections_by_ids(render, &ids, CollectionKind::Album)}),
                );
            }
            let mut albums: Vec<&LocalCollection> = index.albums().collect();
            sort_collections(&mut albums, query.get("sort"));
            Answer::Json(collection_page(
                render,
                "/v1/me/library/albums",
                &albums,
                DEFAULT_LIMIT,
            ))
        }
        ["playlists" | "albums", id] => match library_collection(index, id) {
            Some(collection) => {
                Answer::Json(json!({"data": [render.library_collection(collection, true)]}))
            }
            None => Answer::NotFound,
        },
        ["playlists" | "albums", id, "tracks"] => match library_collection(index, id) {
            Some(collection) => Answer::Json(tracks_page(
                render,
                &format!("/v1/me/library/{}/{id}/tracks", rest[0]),
                &collection.tracks,
                false,
            )),
            None => Answer::NotFound,
        },
        ["playlists", id, "catalog"] => match library_collection(index, id) {
            Some(collection) if collection.global_id.is_some() => {
                Answer::Json(json!({"data": [render.catalog_playlist(collection, true)]}))
            }
            Some(_) => Answer::Json(json!({"data": []})),
            None => Answer::NotFound,
        },
        ["albums", id, "catalog"] => match library_collection(index, id) {
            Some(collection) => match &collection.catalog_album_id {
                Some(album) => Answer::Json(json!({"data": [render.catalog_album(album, true)]})),
                None => Answer::Json(json!({"data": []})),
            },
            None => Answer::NotFound,
        },
        ["songs"] => {
            if let Some(ids) = ids_param(query) {
                let data: Vec<Value> = ids
                    .iter()
                    .filter_map(|id| index.track(id))
                    .map(|t| render.library_song(t, 0))
                    .collect();
                return Answer::Json(json!({"data": data}));
            }
            let mut songs = index.songs_page(songs_page);
            let sort = query.get("sort").map(|s| s.trim_start_matches('-'));
            // Apple lists the Songs tab by name unless `sort` asks otherwise;
            // its `dateAdded` order (either sign) is the synced order.
            let by_name = match songs_page {
                SongsPage::Apple => sort.is_none_or(|s| s == "name"),
                SongsPage::All => sort == Some("name"),
            };
            if by_name {
                songs.sort_by(|&a, &b| index.tracks[a].title_fold.cmp(&index.tracks[b].title_fold));
            }
            Answer::Json(tracks_page(render, "/v1/me/library/songs", &songs, false))
        }
        ["songs" | "music-videos", id] => match index.track(id) {
            Some(track) => Answer::Json(json!({"data": [render.library_song(track, 0)]})),
            None => Answer::NotFound,
        },
        ["songs" | "music-videos", id, "catalog"] => match index.track(id) {
            Some(track) if track.catalog.is_some() => {
                Answer::Json(json!({"data": [render.catalog_song(track, 0, true)]}))
            }
            Some(_) => Answer::Json(json!({"data": []})),
            None => Answer::NotFound,
        },
        ["songs", id, "albums"] => match index.track(id) {
            Some(track) => {
                let data: Vec<Value> = track
                    .library_album
                    .map(|a| render.library_collection(&index.collections[a], false))
                    .into_iter()
                    .collect();
                Answer::Json(json!({"data": data}))
            }
            None => Answer::NotFound,
        },
        ["artists"] => {
            let (offset, limit) = query.page(DEFAULT_LIMIT, MAX_LIMIT);
            let data = index
                .artists
                .iter()
                .skip(offset)
                .take(limit)
                .map(|a| render.library_artist(a, true))
                .collect();
            Answer::Json(page_document(
                render,
                "/v1/me/library/artists",
                data,
                index.artists.len(),
                offset,
            ))
        }
        ["artists", id] => match index.artist(id) {
            Some(artist) => Answer::Json(json!({"data": [render.library_artist(artist, true)]})),
            None => Answer::NotFound,
        },
        ["artists", id, "albums"] => match index.artist(id) {
            Some(artist) => Answer::Json(collection_page(
                render,
                &format!("/v1/me/library/artists/{id}/albums"),
                artist
                    .albums
                    .iter()
                    .map(|&a| &index.collections[a])
                    .collect::<Vec<_>>()
                    .as_slice(),
                DEFAULT_LIMIT,
            )),
            None => Answer::NotFound,
        },
        ["artists", id, "songs"] => match index.artist(id) {
            Some(artist) => Answer::Json(tracks_page(
                render,
                &format!("/v1/me/library/artists/{id}/songs"),
                &artist.songs,
                false,
            )),
            None => Answer::NotFound,
        },
        ["recently-added"] => Answer::Json(recent(
            render,
            "/v1/me/library/recently-added",
            DEFAULT_LIMIT,
            |c| c.date_added.clone(),
        )),
        ["search"] => Answer::Json(search::library_search(render)),
        // Pins, playlist folders below the root, music videos and the like:
        // nothing saved locally, an empty list renders fine.
        _ => Answer::Json(json!({"data": []})),
    }
}

fn ids_param(query: &Query) -> Option<Vec<String>> {
    let ids: Vec<String> = query.list("ids").into_iter().map(str::to_owned).collect();
    (!ids.is_empty()).then_some(ids)
}

fn library_collection<'a>(index: &'a LocalIndex, id: &str) -> Option<&'a LocalCollection> {
    index.collection(id)
}

fn collections_by_ids(render: &Render<'_>, ids: &[String], kind: CollectionKind) -> Vec<Value> {
    ids.iter()
        .filter_map(|id| render.index.collection(id))
        .filter(|c| c.kind == kind)
        .map(|c| render.library_collection(c, true))
        .collect()
}

/// A paginated page of library collections.
fn collection_page(
    render: &Render<'_>,
    path: &str,
    collections: &[&LocalCollection],
    default_limit: usize,
) -> Value {
    let (offset, limit) = render.query.page(default_limit, MAX_LIMIT);
    let data = collections
        .iter()
        .skip(offset)
        .take(limit)
        .map(|c| render.library_collection(c, true))
        .collect();
    page_document(render, path, data, collections.len(), offset)
}

/// A paginated page of tracks (library or catalog flavour).
fn tracks_page(render: &Render<'_>, path: &str, members: &[usize], catalog: bool) -> Value {
    let (offset, limit) = render.query.page(MAX_LIMIT, MAX_TRACKS_PAGE);
    let data = members
        .iter()
        .skip(offset)
        .take(limit)
        .map(|&i| render.song(&render.index.tracks[i], catalog, 0, true))
        .collect();
    page_document(render, path, data, members.len(), offset)
}

fn sort_collections(collections: &mut [&LocalCollection], sort: Option<&str>) {
    let Some(sort) = sort else {
        return;
    };
    let descending = sort.starts_with('-');
    match sort.trim_start_matches('-') {
        "name" => collections.sort_by(|a, b| a.name_fold.cmp(&b.name_fold)),
        "artistName" => collections.sort_by(|a, b| {
            (a.artist_fold.as_str(), a.name_fold.as_str())
                .cmp(&(b.artist_fold.as_str(), b.name_fold.as_str()))
        }),
        "dateAdded" => collections.sort_by(|a, b| a.date_added.cmp(&b.date_added)),
        _ => return,
    }
    if descending {
        collections.reverse();
    }
}

/// Library collections newest first (by `key`), paginated.
fn recent(
    render: &Render<'_>,
    path: &str,
    default_limit: usize,
    key: impl Fn(&LocalCollection) -> String,
) -> Value {
    let mut collections: Vec<&LocalCollection> = render.index.collections.iter().collect();
    collections.sort_by_key(|c| std::cmp::Reverse(key(c)));
    let (offset, limit) = render.query.page(default_limit, MAX_LIMIT);
    let data = collections
        .iter()
        .skip(offset)
        .take(limit)
        .map(|c| render.library_collection(c, true))
        .collect();
    let mut document = page_document(render, path, data, collections.len(), offset);
    // Apple's recent/recently-added listings carry no `meta.total`.
    if let Some(object) = document.as_object_mut() {
        object.remove("meta");
    }
    document
}

/// Replaces library collections in `document.data` by their catalog twins
/// (catalog playlist by global id, catalog album), where one is known.
fn catalog_flavour(render: &Render<'_>, document: &mut Value) {
    let Some(data) = document.get_mut("data").and_then(Value::as_array_mut) else {
        return;
    };
    for item in data.iter_mut() {
        let Some(collection) = item
            .get("id")
            .and_then(Value::as_str)
            .and_then(|id| render.index.collection(id))
        else {
            continue;
        };
        if collection.global_id.is_some() {
            *item = render.catalog_playlist(collection, false);
        } else if let Some(album) = &collection.catalog_album_id {
            *item = render.catalog_album(album, false);
        }
    }
}

/// `/v1/me/library/playlists?filter[featured]=suggested`: the editable
/// playlists Cider offers in "Add to playlist".
fn suggested_playlists(render: &Render<'_>) -> Value {
    let editable: Vec<&LocalCollection> = render
        .index
        .playlists()
        .filter(|c| {
            c.resource
                .get("attributes")
                .and_then(|a| a.get("canEdit"))
                .and_then(Value::as_bool)
                .unwrap_or(false)
        })
        .collect();
    let data: Vec<Value> = editable
        .iter()
        .map(|c| render.library_collection(c, true))
        .collect();
    let references: Vec<Value> = data
        .iter()
        .map(|d| json!({"id": d["id"], "type": d["type"], "href": d["href"]}))
        .collect();
    json!({
        "data": data,
        "meta": {"filters": {"featured": {"suggested": references}}},
    })
}

// ----- Catalog ---------------------------------------------------------------

#[expect(clippy::too_many_lines, reason = "one flat route table")]
fn catalog(render: &Render<'_>, rest: &[&str]) -> Answer {
    let index = render.index;
    let query = render.query;
    let mut answer = match rest {
        ["songs" | "music-videos"] => match ids_param(query) {
            Some(ids) => Answer::Json(json!({"data": ids
                .iter()
                .filter_map(|id| index.track(id))
                .map(|t| render.catalog_song(t, 0, true))
                .collect::<Vec<_>>()})),
            None => Answer::Json(json!({"data": []})),
        },
        ["songs" | "music-videos", id] => match index.track(id) {
            Some(track) => {
                let mut song = render.catalog_song(track, 0, true);
                // Apple answers a music video asked for by the songs path as
                // `songs` (seen for library music videos' linked twins).
                if rest[0] == "songs" && song["type"] == "music-videos" {
                    song["type"] = Value::from("songs");
                    if let Some(attributes) = song["attributes"].as_object_mut() {
                        for key in ["hasLyrics", "hasTimeSyncedLyrics"] {
                            attributes.entry(key).or_insert(Value::Bool(false));
                        }
                    }
                }
                Answer::Json(json!({"data": [song]}))
            }
            None => Answer::NotFound,
        },
        ["songs" | "music-videos", id, relationship] => match (index.track(id), *relationship) {
            // Unknown song; or lyrics, which the lyrics interceptor answers
            // (reaching here means there are none locally).
            (None, _) | (Some(_), "lyrics" | "syllable-lyrics") => Answer::NotFound,
            (Some(track), "albums") => Answer::Json(json!({"data": track
                .catalog_album_id
                .as_deref()
                .map(|album| render.catalog_album(album, false))
                .into_iter()
                .collect::<Vec<_>>()})),
            (Some(track), "library") => Answer::Json(
                json!({"data": if track.in_library && track.is_library_resource() {
                    vec![render.library_song(track, 1)]
                } else {
                    Vec::new()
                }}),
            ),
            // audio-analysis, artists, credits …: nothing local.
            (Some(_), _) => Answer::Json(json!({"data": []})),
        },
        ["albums"] => match ids_param(query) {
            Some(ids) => Answer::Json(json!({"data": ids
                .iter()
                .filter_map(|id| catalog_album_id(index, id))
                .map(|album| render.catalog_album(&album, true))
                .collect::<Vec<_>>()})),
            None => Answer::Json(json!({"data": []})),
        },
        ["albums", id] => album_or_library(
            render,
            id,
            |album| json!({"data": [render.catalog_album(album, true)]}),
        ),
        ["albums", id, "tracks"] => match catalog_album_id(index, id) {
            Some(album) => {
                let members = album_members(index, &album);
                Answer::Json(tracks_page(
                    render,
                    &format!("/v1/catalog/{}/albums/{album}/tracks", render.storefront),
                    &members,
                    true,
                ))
            }
            None => Answer::NotFound,
        },
        ["playlists"] => match ids_param(query) {
            Some(ids) => Answer::Json(json!({"data": ids
                .iter()
                .filter_map(|id| index.collection(id))
                .filter(|c| c.kind == CollectionKind::Playlist)
                .map(|c| render.catalog_playlist(c, true))
                .collect::<Vec<_>>()})),
            None => Answer::Json(json!({"data": []})),
        },
        ["playlists", id] => match index.collection(id) {
            Some(c) if c.kind == CollectionKind::Playlist => {
                Answer::Json(json!({"data": [render.catalog_playlist(c, true)]}))
            }
            _ => Answer::NotFound,
        },
        ["playlists", id, "tracks"] => match index.collection(id) {
            Some(c) if c.kind == CollectionKind::Playlist => Answer::Json(tracks_page(
                render,
                &format!("/v1/catalog/{}/playlists/{id}/tracks", render.storefront),
                &c.tracks,
                !id.starts_with("p."),
            )),
            _ => Answer::NotFound,
        },
        ["search"] => Answer::Json(search::catalog_search(render)),
        ["search", "suggestions"] => Answer::Json(search::suggestions(render)),
        ["search", "hints"] => Answer::Json(search::hints(render)),
        // Artist pages, curators, stations, charts …: nothing local.
        [
            "artists" | "curators" | "apple-curators" | "record-labels" | "stations",
            _,
            ..,
        ] => Answer::NotFound,
        _ => Answer::Json(json!({"data": []})),
    };
    if query.wants_resource_map()
        && let Answer::Json(document) = &mut answer
        && document.get("data").is_some()
    {
        to_resource_map(document);
    }
    answer
}

/// The catalog album id a catalog-path album id stands for (a catalog id
/// with local tracks, or a library album id with a known catalog album).
fn catalog_album_id(index: &LocalIndex, id: &str) -> Option<String> {
    if index.catalog_album_tracks(id).is_some() {
        return Some(id.to_owned());
    }
    index
        .collection(id)
        .filter(|c| c.kind == CollectionKind::Album)
        .and_then(|c| c.catalog_album_id.clone())
}

fn album_members(index: &LocalIndex, album: &str) -> Vec<usize> {
    match index.collection(album) {
        Some(library) if library.kind == CollectionKind::Album => library.tracks.clone(),
        _ => index
            .catalog_album_tracks(album)
            .map(<[usize]>::to_vec)
            .unwrap_or_default(),
    }
}

/// A catalog album by catalog id; a library album id (Cider asks the catalog
/// path with `l.…` ids too) without a catalog twin answers its library resource.
fn album_or_library(render: &Render<'_>, id: &str, catalog: impl Fn(&str) -> Value) -> Answer {
    if let Some(album) = catalog_album_id(render.index, id) {
        return Answer::Json(catalog(&album));
    }
    match render.index.collection(id) {
        Some(c) if c.kind == CollectionKind::Album => {
            Answer::Json(json!({"data": [render.library_collection(c, true)]}))
        }
        _ => Answer::NotFound,
    }
}

/// `/v1/catalog/{sf}?ids[songs]=…&ids[albums]=…` (usually with
/// `format[resources]=map`).
fn typed_lookup(render: &Render<'_>) -> Value {
    let index = render.index;
    let mut data = Vec::new();
    for (ty, ids) in render.query.typed_ids() {
        for id in &ids {
            let resource = match ty.as_str() {
                "songs" | "music-videos" => {
                    index.track(id).map(|t| render.catalog_song(t, 0, true))
                }
                "library-songs" => index.track(id).map(|t| render.library_song(t, 0)),
                "albums" => catalog_album_id(index, id).map(|a| render.catalog_album(&a, true)),
                "playlists" => index
                    .collection(id)
                    .filter(|c| c.kind == CollectionKind::Playlist)
                    .map(|c| render.catalog_playlist(c, true)),
                "library-playlists" | "library-albums" => index
                    .collection(id)
                    .map(|c| render.library_collection(c, true)),
                _ => None,
            };
            data.extend(resource);
        }
    }
    let mut document = json!({ "data": data });
    if render.query.wants_resource_map() {
        to_resource_map(&mut document);
    }
    document
}
