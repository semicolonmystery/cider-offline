//! Apple Music catalog API client and typed models.
//!
//! Two unrelated concerns live in this module:
//!
//! - [`extract`] parses JSON:API documents that flow through the proxy and
//!   feeds [`crate::storage`]. It is used by the cache interceptor on the
//!   hot path; no HTTP of its own.
//! - [`client::CatalogClient`] actively fetches catalog metadata from
//!   `amp-api.music.apple.com` using tokens captured into [`crate::storage`].
//!   It is driven by the UI / library layer, not the proxy hot path.

pub mod client;
pub mod error;
mod extract;
pub mod models;

pub(crate) use extract::{collection_from_resource, extract, playable_from_resource};

pub use client::CatalogClient;
pub use error::CatalogError;
pub use models::{Album, CatalogItem, Playlist, Song, SongCandidate};
