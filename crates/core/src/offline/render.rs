//! Renders indexed resources in Apple's JSON:API shape for one request:
//! `fields` / `extend` handling, relationships for `include` / `relate`,
//! paginated relationship and listing pages with `next` links, hrefs with the
//! request locale, and the `format[resources]=map` normalized form.

use serde_json::{Map, Value, json};

use super::index::{LocalArtist, LocalCollection, LocalIndex, LocalTrack};
use super::query::Query;
use crate::storage::CollectionKind;

/// Deepest relationship nesting rendered (resource → relationship → nested).
const MAX_DEPTH: u8 = 2;
/// Default size of a collection's embedded `tracks` relationship.
const DEFAULT_TRACKS_LIMIT: usize = 100;
/// Default size of an album's embedded `tracks` relationship.
const DEFAULT_ALBUM_TRACKS_LIMIT: usize = 300;
/// Largest relationship page.
const MAX_TRACKS_LIMIT: usize = 300;

/// Rendering context of one request.
pub(crate) struct Render<'a> {
    pub(crate) index: &'a LocalIndex,
    pub(crate) query: &'a Query,
    /// Storefront of the request path (or the account's).
    pub(crate) storefront: &'a str,
}

/// The path part of a stored href (query dropped).
fn href_path(href: &str) -> &str {
    href.split('?').next().unwrap_or(href)
}

/// A JSON:API page: `data`, `meta.total` and a `next` link when more follow.
pub(crate) fn page_document(
    render: &Render<'_>,
    path: &str,
    data: Vec<Value>,
    total: usize,
    offset: usize,
) -> Value {
    let mut document = json!({ "meta": { "total": total } });
    document["data"] = Value::Array(data);
    let shown = document["data"].as_array().map_or(0, Vec::len);
    if offset + shown < total
        && shown > 0
        && let Some(object) = document.as_object_mut()
    {
        object.insert(
            "next".to_owned(),
            Value::from(render.next_link(path, offset + shown)),
        );
    }
    document
}

impl Render<'_> {
    /// `path?l=…` with the request locale.
    pub(crate) fn href(&self, path: &str) -> String {
        match self.query.locale() {
            Some(l) => format!("{}?l={l}", href_path(path)),
            None => href_path(path).to_owned(),
        }
    }

    /// Apple's next-page link: `path?l=…&offset=N`.
    pub(crate) fn next_link(&self, path: &str, offset: usize) -> String {
        match self.query.locale() {
            Some(l) => format!("{}?l={l}&offset={offset}", href_path(path)),
            None => format!("{}?offset={offset}", href_path(path)),
        }
    }

    fn set_href(&self, object: &mut Map<String, Value>) -> String {
        let path = object
            .get("href")
            .and_then(Value::as_str)
            .map(|h| href_path(h).to_owned())
            .unwrap_or_default();
        if !path.is_empty() {
            object.insert("href".to_owned(), Value::from(self.href(&path)));
        }
        path
    }

    /// Applies `extend` (known extended attributes get their local value) and
    /// the `fields` whitelist to `object.attributes`.
    fn shape_attributes(
        &self,
        object: &mut Map<String, Value>,
        ty: &str,
        extra: &dyn Fn(&str) -> Option<Value>,
    ) {
        let Some(attributes) = object.get_mut("attributes").and_then(Value::as_object_mut) else {
            return;
        };
        for name in self.query.extends(ty) {
            if !attributes.contains_key(name)
                && let Some(value) = extra(name)
            {
                attributes.insert(name.to_owned(), value);
            }
        }
        if let Some(fields) = self.query.fields(ty) {
            attributes.retain(|key, _| fields.contains(&key.as_str()));
            for name in fields {
                if !attributes.contains_key(name)
                    && let Some(value) = extra(name)
                {
                    attributes.insert(name.to_owned(), value);
                }
            }
        }
    }

    fn relationship(&self, owner_path: &str, name: &str, data: Vec<Value>) -> Value {
        let mut relationship = json!({ "href": self.href(&format!("{owner_path}/{name}")) });
        relationship["data"] = Value::Array(data);
        relationship
    }

    fn insert_relationships(object: &mut Map<String, Value>, relationships: Map<String, Value>) {
        if !relationships.is_empty() {
            object.insert("relationships".to_owned(), Value::Object(relationships));
        }
    }

    // ----- Songs -----------------------------------------------------------

    /// Removes `playParams` from a song that has no local audio, so Cider
    /// shows it as unavailable and leaves it out of the queue instead of
    /// failing playback.
    ///
    /// Conversely a downloaded song Apple lists without `playParams` (not
    /// streamable in the storefront) gets them, since its local file plays.
    fn mark_playability(object: &mut Map<String, Value>, track: &LocalTrack) {
        let id = object.get("id").cloned();
        let library = object
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(|t| t.starts_with("library-"));
        let Some(attributes) = object.get_mut("attributes").and_then(Value::as_object_mut) else {
            return;
        };
        if !track.downloaded {
            attributes.remove("playParams");
        } else if !attributes.contains_key("playParams")
            && let Some(id) = id
        {
            let kind = if track.kind().ends_with("music-videos") {
                "musicVideo"
            } else {
                "song"
            };
            let mut params = json!({ "id": id, "kind": kind });
            if library {
                params["isLibrary"] = Value::Bool(true);
                if let Some(catalog) = &track.catalog_id {
                    params["catalogId"] = Value::from(catalog.clone());
                }
            }
            attributes.insert("playParams".to_owned(), params);
        }
    }

    fn song_extra(track: &LocalTrack) -> impl Fn(&str) -> Option<Value> + '_ {
        move |name| match name {
            "inFavorites" => Some(Value::Bool(false)),
            "inLibrary" => Some(Value::Bool(track.in_library)),
            "artistUrl" => track
                .catalog
                .as_ref()
                .and_then(|c| c.get("attributes"))
                .and_then(|a| a.get("artistUrl"))
                .cloned(),
            _ => None,
        }
    }

    /// The track in the flavour a path asks for: the stored library resource
    /// for library paths, its catalog twin for catalog paths.
    pub(crate) fn song(
        &self,
        track: &LocalTrack,
        catalog: bool,
        depth: u8,
        primary: bool,
    ) -> Value {
        if catalog || !track.is_library_resource() {
            self.catalog_song(track, depth, primary)
        } else {
            self.library_song(track, depth)
        }
    }

    /// A `library-songs` resource.
    pub(crate) fn library_song(&self, track: &LocalTrack, depth: u8) -> Value {
        let mut object = track.resource.clone();
        let path = self.set_href(&mut object);
        let ty = track.kind().to_owned();
        self.shape_attributes(&mut object, &ty, &Self::song_extra(track));
        Self::mark_playability(&mut object, track);
        if depth < MAX_DEPTH {
            let mut relationships = Map::new();
            for name in self.query.includes(&ty) {
                match name {
                    "catalog" => {
                        if let Some(catalog) = track.catalog.as_ref() {
                            let mut nested = catalog.clone();
                            self.set_href(&mut nested);
                            self.shape_attributes(&mut nested, "songs", &Self::song_extra(track));
                            Self::mark_playability(&mut nested, track);
                            relationships.insert(
                                "catalog".to_owned(),
                                self.relationship(&path, "catalog", vec![Value::Object(nested)]),
                            );
                        }
                    }
                    "albums" => {
                        let data = track
                            .library_album
                            .map(|a| &self.index.collections[a])
                            .map(|album| self.collection_reference(album))
                            .into_iter()
                            .collect();
                        relationships.insert(
                            "albums".to_owned(),
                            self.relationship(&path, "albums", data),
                        );
                    }
                    _ => {}
                }
            }
            Self::insert_relationships(&mut object, relationships);
        }
        Value::Object(object)
    }

    /// A catalog `songs` resource (the catalog twin, or the stored catalog row).
    pub(crate) fn catalog_song(&self, track: &LocalTrack, depth: u8, primary: bool) -> Value {
        let Some(catalog) = track.catalog.as_ref() else {
            return self.library_song(track, depth);
        };
        let mut object = catalog.clone();
        let path = self.set_href(&mut object);
        let ty = object
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("songs")
            .to_owned();
        self.shape_attributes(&mut object, &ty, &Self::song_extra(track));
        Self::mark_playability(&mut object, track);
        if depth < MAX_DEPTH {
            let mut relationships = Map::new();
            if self.query.wants(&ty, "library") {
                let data = if track.in_library && track.is_library_resource() {
                    // Apple applies a plain `fields` list to the included
                    // library twin as well (hence `primary`).
                    vec![self.library_song(track, depth + 1)]
                } else {
                    Vec::new()
                };
                relationships.insert(
                    "library".to_owned(),
                    self.relationship(&path, "library", data),
                );
            }
            // Apple's catalog songs carry their album by default (as a bare
            // reference unless `include` asks for it).
            let wanted = self.query.wants(&ty, "albums");
            if wanted || (primary && depth == 0) {
                let data = track
                    .catalog_album_id
                    .as_deref()
                    .map(|album_id| {
                        let album = self.catalog_album_reference(album_id, track);
                        if wanted {
                            album
                        } else {
                            json!({"id": album["id"], "type": album["type"], "href": album["href"]})
                        }
                    })
                    .into_iter()
                    .collect();
                relationships.insert(
                    "albums".to_owned(),
                    self.relationship(&path, "albums", data),
                );
            }
            Self::insert_relationships(&mut object, relationships);
        }
        Value::Object(object)
    }

    /// A catalog album as a relationship entry (`fields[albums]` honoured).
    fn catalog_album_reference(&self, album_id: &str, track: &LocalTrack) -> Value {
        let mut object = self.catalog_album_object(album_id, Some(track));
        object.remove("relationships");
        self.shape_attributes(&mut object, "albums", &Self::album_extra);
        Value::Object(object)
    }

    // ----- Collections -----------------------------------------------------

    /// `{id, type, href, attributes}` of a library collection (no relationships).
    fn collection_reference(&self, collection: &LocalCollection) -> Value {
        let mut object = collection.resource.clone();
        self.set_href(&mut object);
        self.shape_attributes(
            &mut object,
            collection.library_type(),
            &Self::collection_extra(collection),
        );
        Value::Object(object)
    }

    fn collection_extra(collection: &LocalCollection) -> impl Fn(&str) -> Option<Value> + '_ {
        move |name| match name {
            "trackCount"
            | "songCount"
            | "numberOfTracks"
            | "approximateNumberOfTracks"
            | "totalTracks" => Some(Value::from(collection.tracks.len())),
            "inFavorites" | "hasCollaboration" => Some(Value::Bool(false)),
            "inLibrary" => Some(Value::Bool(true)),
            "tags" => Some(json!([])),
            _ => None,
        }
    }

    /// A paginated `tracks` relationship of `members` rendered with `song`.
    fn tracks_relationship(
        &self,
        owner_path: &str,
        members: &[usize],
        default_limit: usize,
        render: &dyn Fn(&LocalTrack) -> Value,
    ) -> Value {
        let limit = self
            .query
            .number("limit[tracks]")
            .unwrap_or(default_limit)
            .clamp(1, MAX_TRACKS_LIMIT);
        let data: Vec<Value> = members
            .iter()
            .take(limit)
            .map(|&i| render(&self.index.tracks[i]))
            .collect();
        let path = format!("{owner_path}/tracks");
        let mut relationship = json!({
            "href": self.href(&path),
            "data": data,
            "meta": { "total": members.len() },
        });
        if members.len() > limit
            && let Some(object) = relationship.as_object_mut()
        {
            object.insert("next".to_owned(), Value::from(self.next_link(&path, limit)));
        }
        relationship
    }

    /// A library playlist / album resource. Albums always carry their
    /// `tracks` (Apple's default); playlists when `include` asks for them.
    pub(crate) fn library_collection(&self, collection: &LocalCollection, primary: bool) -> Value {
        let mut object = collection.resource.clone();
        let path = self.set_href(&mut object);
        let ty = collection.library_type();
        self.shape_attributes(&mut object, ty, &Self::collection_extra(collection));
        let includes = self.query.includes(ty);
        let mut relationships = Map::new();
        if includes.contains(&"catalog") {
            if let Some(catalog) = &collection.catalog_relationship {
                relationships.insert("catalog".to_owned(), catalog.clone());
            } else if let Some(global) = &collection.global_id {
                let reference = json!({
                    "id": global,
                    "type": "playlists",
                    "href": self.href(&format!("/v1/catalog/{}/playlists/{global}", self.storefront)),
                });
                relationships.insert(
                    "catalog".to_owned(),
                    self.relationship(&path, "catalog", vec![reference]),
                );
            } else if let Some(album) = &collection.catalog_album_id {
                let reference = json!({
                    "id": album,
                    "type": "albums",
                    "href": self.href(&format!("/v1/catalog/{}/albums/{album}", self.storefront)),
                });
                relationships.insert(
                    "catalog".to_owned(),
                    self.relationship(&path, "catalog", vec![reference]),
                );
            }
        }
        let fields_exclude_relationships =
            self.query.fields(ty).is_some() && !includes.contains(&"tracks");
        let wants_tracks = includes.contains(&"tracks")
            || (collection.kind == CollectionKind::Album && !fields_exclude_relationships);
        if wants_tracks && primary {
            let default_limit = match collection.kind {
                CollectionKind::Album => DEFAULT_ALBUM_TRACKS_LIMIT,
                CollectionKind::Playlist => DEFAULT_TRACKS_LIMIT,
            };
            relationships.insert(
                "tracks".to_owned(),
                self.tracks_relationship(&path, &collection.tracks, default_limit, &|track| {
                    self.library_song(track, 1)
                }),
            );
        }
        Self::insert_relationships(&mut object, relationships);
        Value::Object(object)
    }

    /// The catalog `playlists` resource of a synced library playlist.
    pub(crate) fn catalog_playlist(&self, collection: &LocalCollection, primary: bool) -> Value {
        let id = collection
            .global_id
            .clone()
            .unwrap_or_else(|| collection.id.clone());
        let path = format!("/v1/catalog/{}/playlists/{id}", self.storefront);
        let mut attributes = collection
            .resource
            .get("attributes")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        attributes.insert(
            "playParams".to_owned(),
            json!({ "id": id, "kind": "playlist" }),
        );
        attributes
            .entry("playlistType".to_owned())
            .or_insert_with(|| Value::from("user-shared"));
        attributes
            .entry("isChart".to_owned())
            .or_insert(Value::Bool(false));
        attributes.entry("url".to_owned()).or_insert_with(|| {
            Value::from(format!(
                "https://music.apple.com/{}/playlist/{}/{id}",
                self.storefront,
                url_slug(collection.name_fold.as_str())
            ))
        });
        let mut object = Map::new();
        object.insert("id".to_owned(), Value::from(id));
        object.insert("type".to_owned(), Value::from("playlists"));
        object.insert("href".to_owned(), Value::from(self.href(&path)));
        object.insert("attributes".to_owned(), Value::Object(attributes));
        self.shape_attributes(
            &mut object,
            "playlists",
            &Self::collection_extra(collection),
        );
        // Apple's catalog playlists carry their tracks by default.
        if primary {
            let mut relationships = Map::new();
            relationships.insert(
                "tracks".to_owned(),
                self.tracks_relationship(&path, &collection.tracks, DEFAULT_TRACKS_LIMIT, &|t| {
                    self.catalog_song(t, 1, false)
                }),
            );
            Self::insert_relationships(&mut object, relationships);
        }
        Value::Object(object)
    }

    /// A catalog `albums` object built from the local tracks of `album_id`
    /// (and the synced library album, when there is one).
    fn catalog_album_object(
        &self,
        album_id: &str,
        hint: Option<&LocalTrack>,
    ) -> Map<String, Value> {
        let members = self
            .index
            .catalog_album_tracks(album_id)
            .unwrap_or_default();
        let first = hint.or_else(|| members.first().map(|&i| &self.index.tracks[i]));
        let library = self
            .index
            .collection(album_id)
            .filter(|c| c.kind == CollectionKind::Album);
        let song_attr = |key: &str| -> Option<Value> {
            first
                .and_then(|t| t.catalog.as_ref())
                .and_then(|c| c.get("attributes"))
                .and_then(|a| a.get(key))
                .cloned()
        };
        let mut attributes = Map::new();
        if let Some(library) = library
            && let Some(stored) = library
                .resource
                .get("attributes")
                .and_then(Value::as_object)
        {
            attributes.extend(stored.clone());
        }
        let name =
            song_attr("albumName").or_else(|| first.and_then(|t| t.album.clone()).map(Value::from));
        for (key, value) in [
            ("name", name),
            (
                "artistName",
                song_attr("albumArtistName").or_else(|| song_attr("artistName")),
            ),
            ("artwork", song_attr("artwork")),
            ("genreNames", song_attr("genreNames")),
            ("releaseDate", song_attr("releaseDate")),
            ("contentRating", album_rating(&self.index.tracks, members)),
        ] {
            if let Some(value) = value {
                attributes.entry(key.to_owned()).or_insert(value);
            }
        }
        let track_count = library.map_or(members.len(), |l| l.tracks.len());
        attributes.insert("trackCount".to_owned(), Value::from(track_count));
        attributes.insert(
            "playParams".to_owned(),
            json!({ "id": album_id, "kind": "album" }),
        );
        if let Some(url) = song_attr("url").as_ref().and_then(Value::as_str) {
            let album_url = url.split('?').next().unwrap_or(url);
            attributes.insert("url".to_owned(), Value::from(album_url));
        }
        let path = format!("/v1/catalog/{}/albums/{album_id}", self.storefront);
        let mut object = Map::new();
        object.insert("id".to_owned(), Value::from(album_id));
        object.insert("type".to_owned(), Value::from("albums"));
        object.insert("href".to_owned(), Value::from(self.href(&path)));
        object.insert("attributes".to_owned(), Value::Object(attributes));
        object
    }

    /// A catalog `albums` resource with its local `tracks`.
    pub(crate) fn catalog_album(&self, album_id: &str, primary: bool) -> Value {
        let mut object = self.catalog_album_object(album_id, None);
        self.shape_attributes(&mut object, "albums", &Self::album_extra);
        let members: Vec<usize> = match self.index.collection(album_id) {
            Some(library) if library.kind == CollectionKind::Album => library.tracks.clone(),
            _ => self
                .index
                .catalog_album_tracks(album_id)
                .map(<[usize]>::to_vec)
                .unwrap_or_default(),
        };
        // Apple's catalog albums carry their tracks by default.
        if primary {
            let path = format!("/v1/catalog/{}/albums/{album_id}", self.storefront);
            let mut relationships = Map::new();
            relationships.insert(
                "tracks".to_owned(),
                self.tracks_relationship(&path, &members, DEFAULT_ALBUM_TRACKS_LIMIT, &|t| {
                    self.catalog_song(t, 1, false)
                }),
            );
            Self::insert_relationships(&mut object, relationships);
        }
        Value::Object(object)
    }

    fn album_extra(name: &str) -> Option<Value> {
        match name {
            "inFavorites" => Some(Value::Bool(false)),
            _ => None,
        }
    }

    // ----- Artists ---------------------------------------------------------

    /// A `library-artists` resource.
    pub(crate) fn library_artist(&self, artist: &LocalArtist, primary: bool) -> Value {
        let path = format!("/v1/me/library/artists/{}", artist.id);
        let mut object = Map::new();
        object.insert("id".to_owned(), Value::from(artist.id.clone()));
        object.insert("type".to_owned(), Value::from("library-artists"));
        object.insert("href".to_owned(), Value::from(self.href(&path)));
        object.insert("attributes".to_owned(), json!({ "name": artist.name }));
        self.shape_attributes(&mut object, "library-artists", &|name| match name {
            "inFavorites" => Some(Value::Bool(false)),
            _ => None,
        });
        if primary && self.query.includes("library-artists").contains(&"albums") {
            let data = artist
                .albums
                .iter()
                .map(|&a| self.collection_reference(&self.index.collections[a]))
                .collect();
            let mut relationships = Map::new();
            relationships.insert(
                "albums".to_owned(),
                self.relationship(&path, "albums", data),
            );
            Self::insert_relationships(&mut object, relationships);
        }
        Value::Object(object)
    }
}

/// An album is explicit when any of its tracks is.
fn album_rating(tracks: &[LocalTrack], members: &[usize]) -> Option<Value> {
    members
        .iter()
        .flat_map(|&i| {
            let track = &tracks[i];
            [track.catalog.as_ref(), Some(&track.resource)]
        })
        .flatten()
        .filter_map(|object| object.get("attributes")?.get("contentRating")?.as_str())
        .find(|rating| *rating == "explicit")
        .map(Value::from)
}

/// A music.apple.com URL slug of a (folded) name.
fn url_slug(name_fold: &str) -> String {
    let slug: String = name_fold
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect();
    if slug.is_empty() {
        "playlist".to_owned()
    } else {
        slug
    }
}

/// Converts a `data` document into Apple's `format[resources]=map` form:
/// `data` (and every nested relationship) holds `{id, type, href}` references
/// and the full objects live once under `resources[type][id]`.
pub(crate) fn to_resource_map(document: &mut Value) {
    let mut resources: Map<String, Value> = Map::new();
    if let Some(data) = document.get_mut("data").and_then(Value::as_array_mut) {
        for item in data.iter_mut() {
            *item = normalize(item.take(), &mut resources);
        }
    }
    if let Some(object) = document.as_object_mut() {
        object.insert("resources".to_owned(), Value::Object(resources));
    }
}

fn reference_of(object: &Map<String, Value>) -> Value {
    let mut reference = Map::new();
    for key in ["id", "type", "href"] {
        if let Some(value) = object.get(key) {
            reference.insert(key.to_owned(), value.clone());
        }
    }
    Value::Object(reference)
}

fn normalize(value: Value, resources: &mut Map<String, Value>) -> Value {
    let Value::Object(mut object) = value else {
        return value;
    };
    if let Some(relationships) = object
        .get_mut("relationships")
        .and_then(Value::as_object_mut)
    {
        for relationship in relationships.values_mut() {
            if let Some(data) = relationship.get_mut("data").and_then(Value::as_array_mut) {
                for item in data.iter_mut() {
                    *item = normalize(item.take(), resources);
                }
            }
        }
    }
    let reference = reference_of(&object);
    let ty = object
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let id = object
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    if !ty.is_empty() && !id.is_empty() {
        let bucket = resources
            .entry(ty)
            .or_insert_with(|| Value::Object(Map::new()));
        if let Some(bucket) = bucket.as_object_mut() {
            let entry = bucket.entry(id).or_insert_with(|| json!({}));
            if let Some(existing) = entry.as_object_mut() {
                // Keep the richest copy when one resource appears several times.
                if object.len() >= existing.len() {
                    object.remove("meta");
                    *existing = std::mem::take(&mut object);
                }
            }
        }
    }
    reference
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_map_should_hoist_nested_resources() {
        let mut document = json!({"data": [{
            "id": "p.1", "type": "playlists", "href": "/p", "attributes": {"name": "x"},
            "relationships": {"tracks": {"data": [{"id": "1", "type": "songs", "href": "/s", "attributes": {}}]}}
        }]});
        to_resource_map(&mut document);
        assert_eq!(
            (
                document.pointer("/data/0").cloned(),
                document.pointer("/resources/songs/1/id").cloned(),
                document
                    .pointer("/resources/playlists/p.1/relationships/tracks/data/0")
                    .cloned()
            ),
            (
                Some(json!({"id": "p.1", "type": "playlists", "href": "/p"})),
                Some(json!("1")),
                Some(json!({"id": "1", "type": "songs", "href": "/s"}))
            )
        );
    }
}
