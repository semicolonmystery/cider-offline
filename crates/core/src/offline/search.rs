//! Local search over the [`LocalIndex`] in Apple's response shapes: catalog
//! search (grouped `with=serverBubbles` form and the plain form), library
//! search, search suggestions and hints.
//!
//! Matching is accent- and case-insensitive (see [`fold`]): every word of the
//! term must occur in the item's title, artist or album. Results with local
//! audio rank first, then exact / prefix title matches, then library order.

use std::collections::HashSet;
use std::fmt::Write as _;

use serde_json::{Map, Value, json};

use super::index::{LocalIndex, fold};
use super::render::Render;
use crate::storage::CollectionKind;

/// Default `limit` of a search group.
const DEFAULT_SEARCH_LIMIT: usize = 5;
/// Largest `limit` of a search group.
const MAX_SEARCH_LIMIT: usize = 50;
/// Items in the grouped form's `top` group.
const TOP_RESULTS: usize = 3;
/// Default number of suggestion `topResults`.
const DEFAULT_TOP_SUGGESTIONS: usize = 10;

/// How well an item's name matches the term (lower is better).
fn name_score(name_fold: &str, term_fold: &str) -> u8 {
    if name_fold == term_fold {
        0
    } else if name_fold.starts_with(term_fold) {
        1
    } else if name_fold.contains(term_fold) {
        2
    } else {
        3
    }
}

fn all_words_in(words: &[&str], haystack: &str) -> bool {
    !words.is_empty() && words.iter().all(|w| haystack.contains(w))
}

/// Local search hits, best first.
#[derive(Debug, Default)]
pub(crate) struct Hits {
    /// Track indexes.
    pub(crate) songs: Vec<usize>,
    /// Catalog album ids (library albums first).
    pub(crate) albums: Vec<String>,
    /// Library album collection indexes.
    pub(crate) library_albums: Vec<usize>,
    /// Playlist collection indexes.
    pub(crate) playlists: Vec<usize>,
    /// Library artist indexes.
    pub(crate) artists: Vec<usize>,
}

/// Searches the index for `term`.
pub(crate) fn search(index: &LocalIndex, term: &str) -> Hits {
    let term_fold = fold(term);
    let words: Vec<&str> = term_fold.split(' ').filter(|w| !w.is_empty()).collect();
    let mut hits = Hits::default();
    if words.is_empty() {
        return hits;
    }

    let mut songs: Vec<(bool, u8, usize)> = index
        .tracks
        .iter()
        .enumerate()
        .filter(|(_, t)| {
            all_words_in(
                &words,
                &format!("{} {} {}", t.title_fold, t.artist_fold, t.album_fold),
            )
        })
        .map(|(i, t)| (!t.downloaded, name_score(&t.title_fold, &term_fold), i))
        .collect();
    songs.sort_unstable();
    let mut seen_catalog = HashSet::new();
    hits.songs = songs
        .into_iter()
        .map(|(_, _, i)| i)
        // A catalog row and its library twin are one song.
        .filter(|&i| {
            let t = &index.tracks[i];
            seen_catalog.insert(t.catalog_id.clone().unwrap_or_else(|| t.id.clone()))
        })
        .collect();

    let mut collections: Vec<(bool, u8, usize)> = index
        .collections
        .iter()
        .enumerate()
        .filter(|(_, c)| all_words_in(&words, &format!("{} {}", c.name_fold, c.artist_fold)))
        .map(|(i, c)| {
            let downloaded =
                !c.tracks.is_empty() && c.tracks.iter().all(|&t| index.tracks[t].downloaded);
            (!downloaded, name_score(&c.name_fold, &term_fold), i)
        })
        .collect();
    collections.sort_unstable();
    for (_, _, i) in collections {
        match index.collections[i].kind {
            CollectionKind::Playlist => hits.playlists.push(i),
            CollectionKind::Album => hits.library_albums.push(i),
        }
    }

    // Catalog albums: library albums first, then albums of local songs.
    let mut albums: Vec<String> = hits
        .library_albums
        .iter()
        .filter_map(|&i| index.collections[i].catalog_album_id.clone())
        .collect();
    let mut other: Vec<(bool, u8, String)> = index
        .catalog_album_ids()
        .filter(|id| !albums.contains(id))
        .filter_map(|id| {
            let members = index.catalog_album_tracks(id)?;
            let first = &index.tracks[*members.first()?];
            all_words_in(
                &words,
                &format!("{} {}", first.album_fold, first.artist_fold),
            )
            .then(|| {
                let downloaded = members.iter().any(|&t| index.tracks[t].downloaded);
                (
                    !downloaded,
                    name_score(&first.album_fold, &term_fold),
                    id.clone(),
                )
            })
        })
        .collect();
    other.sort_unstable();
    albums.extend(other.into_iter().map(|(_, _, id)| id));
    hits.albums = albums;

    hits.artists = index
        .artists
        .iter()
        .enumerate()
        .filter(|(_, a)| all_words_in(&words, &a.name_fold))
        .map(|(i, _)| i)
        .collect();
    hits
}

fn term(render: &Render<'_>) -> String {
    render
        .query
        .get("term")
        .unwrap_or_default()
        .trim()
        .to_owned()
}

fn group_limit(render: &Render<'_>) -> usize {
    render
        .query
        .number("limit")
        .unwrap_or(DEFAULT_SEARCH_LIMIT)
        .clamp(1, MAX_SEARCH_LIMIT)
}

fn requested_types(render: &Render<'_>, default: &[&'static str]) -> Vec<String> {
    let types = render.query.list("types");
    if types.is_empty() {
        default.iter().map(|t| (*t).to_owned()).collect()
    } else {
        types.into_iter().map(str::to_owned).collect()
    }
}

/// Rendered catalog results of one search type.
fn catalog_items(render: &Render<'_>, hits: &Hits, ty: &str) -> Vec<Value> {
    let index = render.index;
    match ty {
        "songs" => hits
            .songs
            .iter()
            .map(|&i| render.catalog_song(&index.tracks[i], 0, true))
            .collect(),
        "albums" => hits
            .albums
            .iter()
            .map(|id| render.catalog_album(id, false))
            .collect(),
        "playlists" => hits
            .playlists
            .iter()
            .map(|&i| &index.collections[i])
            .filter(|c| c.global_id.is_some())
            .map(|c| render.catalog_playlist(c, false))
            .collect(),
        _ => Vec::new(),
    }
}

/// Singular group key of the grouped search form (`songs` → `song`).
fn group_key(ty: &str) -> Option<(&'static str, &'static str)> {
    match ty {
        "songs" => Some(("song", "Songs")),
        "albums" => Some(("album", "Albums")),
        "playlists" => Some(("playlist", "Playlists")),
        _ => None,
    }
}

fn search_href(render: &Render<'_>, path: &str, group: Option<&str>) -> String {
    let mut href = format!("{path}?");
    if let Some(group) = group {
        let _ = write!(href, "groups={group}&");
    }
    if let Some(l) = render.query.locale() {
        let _ = write!(href, "l={l}&");
    }
    let encoded: String = reqwest::Url::parse_with_params("http://q/", &[("term", term(render))])
        .ok()
        .and_then(|u| u.query().map(str::to_owned))
        .unwrap_or_default();
    href.push_str(&encoded);
    let _ = write!(href, "&types={}", render.query.list("types").join("%2C"));
    href
}

/// `/v1/catalog/{sf}/search`.
pub(crate) fn catalog_search(render: &Render<'_>) -> Value {
    let hits = search(render.index, &term(render));
    let limit = group_limit(render);
    let path = format!("/v1/catalog/{}/search", render.storefront);
    let types = requested_types(render, &["songs", "albums", "playlists"]);
    let grouped = render.query.list("with").contains(&"serverBubbles");

    let mut results = Map::new();
    let mut order = Vec::new();
    let mut top: Vec<Value> = Vec::new();
    for ty in &types {
        let items = catalog_items(render, &hits, ty);
        if items.is_empty() {
            continue;
        }
        let total = items.len();
        let page: Vec<Value> = items.into_iter().take(limit).collect();
        if grouped {
            let Some((key, name)) = group_key(ty) else {
                continue;
            };
            if top.len() < TOP_RESULTS {
                top.push(page[0].clone());
            }
            results.insert(
                key.to_owned(),
                json!({
                    "groupId": key,
                    "name": name,
                    "href": search_href(render, &path, Some(key)),
                    "data": page,
                }),
            );
            order.push(key.to_owned());
        } else {
            let mut group = json!({
                "href": search_href(render, &path, None),
                "data": page,
            });
            if total > limit
                && let Some(object) = group.as_object_mut()
            {
                object.insert(
                    "next".to_owned(),
                    Value::from(format!(
                        "{}&offset={limit}",
                        search_href(render, &path, None)
                    )),
                );
            }
            results.insert(ty.clone(), group);
            order.push(ty.clone());
        }
    }
    if grouped && !top.is_empty() {
        results.insert(
            "top".to_owned(),
            json!({"groupId": "top", "name": "Top Results", "data": top}),
        );
        order.insert(0, "top".to_owned());
    }
    json!({
        "results": results,
        "meta": {"results": {"order": order, "rawOrder": order}},
    })
}

/// `/v1/me/library/search`.
pub(crate) fn library_search(render: &Render<'_>) -> Value {
    let index = render.index;
    let hits = search(index, &term(render));
    let limit = group_limit(render);
    let types = requested_types(
        render,
        &["library-songs", "library-albums", "library-playlists"],
    );
    let mut results = Map::new();
    let mut order = Vec::new();
    for ty in &types {
        let items: Vec<Value> = match ty.as_str() {
            "library-songs" => hits
                .songs
                .iter()
                .map(|&i| &index.tracks[i])
                .filter(|t| t.is_library_resource())
                .map(|t| render.library_song(t, 0))
                .collect(),
            "library-albums" => hits
                .library_albums
                .iter()
                .map(|&i| render.library_collection(&index.collections[i], false))
                .collect(),
            "library-playlists" => hits
                .playlists
                .iter()
                .map(|&i| render.library_collection(&index.collections[i], false))
                .collect(),
            "library-artists" => hits
                .artists
                .iter()
                .map(|&i| render.library_artist(&index.artists[i], false))
                .collect(),
            _ => Vec::new(),
        };
        if items.is_empty() {
            continue;
        }
        let total = items.len();
        let path = "/v1/me/library/search";
        let mut group = json!({
            "href": search_href(render, path, None),
            "data": items.into_iter().take(limit).collect::<Vec<_>>(),
        });
        if total > limit
            && let Some(object) = group.as_object_mut()
        {
            object.insert(
                "next".to_owned(),
                Value::from(format!(
                    "{}&offset={limit}",
                    search_href(render, path, None)
                )),
            );
        }
        results.insert(ty.clone(), group);
        order.push(ty.clone());
    }
    json!({
        "results": results,
        "meta": {"results": {"order": order, "rawOrder": order}},
    })
}

/// `/v1/catalog/{sf}/search/suggestions` (`kinds=terms,topResults`).
pub(crate) fn suggestions(render: &Render<'_>) -> Value {
    let term = term(render);
    let hits = search(render.index, &term);
    let kinds = render.query.list("kinds");
    let wants = |kind: &str| kinds.is_empty() || kinds.contains(&kind);
    let mut suggestions = Vec::new();
    if wants("terms") {
        let limit = render
            .query
            .number("limit[results:terms]")
            .unwrap_or(5)
            .max(1);
        let mut terms: Vec<String> = vec![term.clone()];
        for &i in &hits.songs {
            let title = render.index.tracks[i]
                .resource
                .get("attributes")
                .and_then(|a| a.get("name"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_lowercase();
            if !title.is_empty() && !terms.contains(&title) {
                terms.push(title);
            }
        }
        for t in terms.into_iter().take(limit) {
            suggestions.push(json!({"kind": "terms", "searchTerm": t, "displayTerm": t}));
        }
    }
    if wants("topResults") {
        let limit = render
            .query
            .number("limit[results:topResults]")
            .unwrap_or(DEFAULT_TOP_SUGGESTIONS);
        let types = requested_types(render, &["songs", "albums", "playlists"]);
        let mut content: Vec<Value> = Vec::new();
        for ty in ["albums", "playlists", "songs"] {
            if types.iter().any(|t| t == ty) {
                content.extend(catalog_items(render, &hits, ty));
            }
        }
        for item in content.into_iter().take(limit) {
            suggestions.push(json!({"kind": "topResults", "content": item}));
        }
    }
    json!({"results": {"suggestions": suggestions}})
}

/// `/v1/catalog/{sf}/search/hints`.
pub(crate) fn hints(render: &Render<'_>) -> Value {
    let hits = search(render.index, &term(render));
    let terms: Vec<Value> = hits
        .songs
        .iter()
        .take(10)
        .filter_map(|&i| {
            render.index.tracks[i]
                .resource
                .get("attributes")
                .and_then(|a| a.get("name"))
                .and_then(Value::as_str)
                .map(|n| Value::from(n.to_lowercase()))
        })
        .collect();
    json!({"results": {"terms": terms}})
}
