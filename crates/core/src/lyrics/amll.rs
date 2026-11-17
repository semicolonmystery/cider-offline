//! The [AMLL TTML DB](https://github.com/amll-dev/amll-ttml-db): community
//! word-synced lyrics in Apple's TTML format, stored by Apple Music catalog
//! id (CC0 / the original providers' terms). Read straight from the
//! repository's raw files, no account needed.
//!
//! - `am-lyrics/<catalog id>.ttml` is the lyrics of that song (`404`: none).
//! - `am-lyrics/index.jsonl` lists every entry with its metadata (`isrc`,
//!   `appleMusicId`, …). It is read (once per run) only to find a song by
//!   ISRC when its own catalog id has no file, e.g. another release.

use std::collections::HashMap;

use async_trait::async_trait;
use serde::Deserialize;
use tokio::sync::OnceCell;

use super::ttml::normalize_clock_attributes;
use super::{
    FetchedLyrics, LyricsError, LyricsKind, LyricsProvider, LyricsProviderId, LyricsQuality,
    LyricsRequest, ProviderError, ttml_quality, web,
};

/// Raw files of the database's main branch.
const BASE_URL: &str = "https://raw.githubusercontent.com/amll-dev/amll-ttml-db/refs/heads/main";
/// Namespace of Apple's `itunes:` attributes.
const ITUNES_NS: &str = "http://music.apple.com/lyric-ttml-internal";

#[derive(Debug, Deserialize)]
struct IndexEntry {
    id: String,
    #[serde(default)]
    metadata: Vec<(String, Vec<String>)>,
}

/// ISRC → catalog id of every entry in an `index.jsonl` (bad lines skipped).
fn parse_index(jsonl: &str) -> HashMap<String, String> {
    let mut by_isrc = HashMap::new();
    for line in jsonl.lines().filter(|l| !l.trim().is_empty()) {
        let Ok(entry) = serde_json::from_str::<IndexEntry>(line) else {
            continue;
        };
        for (key, values) in &entry.metadata {
            if key == "isrc" {
                for isrc in values {
                    by_isrc
                        .entry(isrc.trim().to_ascii_uppercase())
                        .or_insert_with(|| entry.id.clone());
                }
            }
        }
    }
    by_isrc
}

/// Makes an AMLL document look like Apple's: no `xmlns=""` resets (they
/// move `<div>`/`<p>` out of the TTML namespace), Apple-style clock values,
/// and an `itunes:timing` marker.
pub(crate) fn clean_ttml(ttml: &str) -> String {
    let mut out = normalize_clock_attributes(&ttml.trim().replace(" xmlns=\"\"", ""));
    if !out.contains("itunes:timing=") && out.starts_with("<tt") {
        let timing = if out.contains("<span begin=\"") {
            "Word"
        } else if out.contains("<p begin=\"") {
            "Line"
        } else {
            "None"
        };
        let namespace = if out.contains("xmlns:itunes=") {
            String::new()
        } else {
            format!(" xmlns:itunes=\"{ITUNES_NS}\"")
        };
        out.insert_str(3, &format!("{namespace} itunes:timing=\"{timing}\""));
    }
    out
}

/// AMLL TTML DB provider (see the module docs).
#[derive(Debug)]
pub struct AmllProvider {
    http: reqwest::Client,
    base_url: String,
    index: OnceCell<HashMap<String, String>>,
}

impl AmllProvider {
    /// Provider for the public repository.
    ///
    /// # Errors
    ///
    /// Returns an error if the HTTP client cannot be built.
    pub fn new() -> Result<Self, LyricsError> {
        Self::with_base_url(BASE_URL)
    }

    /// Provider for another host (tests).
    ///
    /// # Errors
    ///
    /// Returns an error if the HTTP client cannot be built.
    pub fn with_base_url(base_url: &str) -> Result<Self, LyricsError> {
        Ok(Self {
            http: web::client()?,
            base_url: base_url.trim_end_matches('/').to_owned(),
            index: OnceCell::new(),
        })
    }

    async fn file(&self, id: &str) -> Result<Option<String>, ProviderError> {
        web::text(
            self.http
                .get(format!("{}/am-lyrics/{id}.ttml", self.base_url)),
        )
        .await
    }

    /// The catalog id filed under `isrc`, loading the index on first use
    /// (a failed load is retried next time).
    async fn id_for_isrc(&self, isrc: &str) -> Result<Option<String>, ProviderError> {
        let index = self
            .index
            .get_or_try_init(|| async {
                let url = format!("{}/am-lyrics/index.jsonl", self.base_url);
                let text = web::text(self.http.get(url)).await?;
                Ok::<_, ProviderError>(parse_index(text.as_deref().unwrap_or_default()))
            })
            .await?;
        Ok(index.get(&isrc.trim().to_ascii_uppercase()).cloned())
    }
}

#[async_trait]
impl LyricsProvider for AmllProvider {
    fn id(&self) -> LyricsProviderId {
        LyricsProviderId::Amll
    }

    fn best_quality(&self) -> LyricsQuality {
        LyricsQuality::Word
    }

    async fn lookup(
        &self,
        request: &LyricsRequest<'_>,
    ) -> Result<Option<FetchedLyrics>, ProviderError> {
        let catalog_id = request
            .catalog_id
            .filter(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()));
        let mut found = None;
        if let Some(id) = catalog_id {
            found = self.file(id).await?.map(|ttml| (id.to_owned(), ttml));
        }
        if found.is_none()
            && let Some(isrc) = request
                .query
                .isrc
                .as_deref()
                .filter(|i| !i.trim().is_empty())
            && let Some(id) = self.id_for_isrc(isrc).await?
            && Some(id.as_str()) != catalog_id
        {
            found = self.file(&id).await?.map(|ttml| (id, ttml));
        }
        let Some((id, raw)) = found.filter(|(_, ttml)| ttml.trim_start().starts_with("<tt")) else {
            return Ok(None);
        };
        let ttml = clean_ttml(&raw);
        let quality = ttml_quality(&ttml);
        Ok(Some(FetchedLyrics {
            ttml,
            kind: LyricsKind::Line,
            quality,
            matched: Some(format!("AMLL am-lyrics/{id}.ttml")),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lyrics::LyricsQuery;
    use crate::lyrics::web::test_server::serve;

    const AMLL: &str = r#"<tt xmlns="http://www.w3.org/ns/ttml" xmlns:ttm="http://www.w3.org/ns/ttml#metadata" xmlns:itunes="http://music.apple.com/lyric-ttml-internal"><head><metadata xmlns=""><ttm:agent type="person" xml:id="v1"/></metadata></head><body dur="03:01.128"><div xmlns="" begin="00:00.917" end="03:01.128"><p begin="00:00.917" end="00:03.669" ttm:agent="v1" itunes:key="L1"><span begin="00:00.917" end="00:01.169">遥</span><span begin="00:01.169" end="00:01.385">か</span></p></div></body></tt>"#;

    fn query(isrc: Option<&str>) -> LyricsQuery {
        LyricsQuery {
            title: "祝福".to_owned(),
            artist: "YOASOBI".to_owned(),
            album: None,
            duration_ms: None,
            isrc: isrc.map(str::to_owned),
        }
    }

    #[test]
    fn cleaned_ttml_should_be_apple_shaped_word_lyrics() {
        let ttml = clean_ttml(AMLL);
        assert!(
            ttml.starts_with("<tt itunes:timing=\"Word\" xmlns=")
                && !ttml.contains("xmlns=\"\"")
                && ttml.contains("<div begin=\"0.917\" end=\"3:01.128\">")
                && ttml_quality(&ttml) == LyricsQuality::Word
        );
    }

    #[test]
    fn index_should_map_isrc_to_id() {
        let index = parse_index(
            "{\"id\":\"1645308331\",\"metadata\":[[\"isrc\",[\"jpp302200613\"]],[\"artists\",[\"YOASOBI\"]]]}\nnot json\n",
        );
        assert_eq!(
            index.get("JPP302200613").map(String::as_str),
            Some("1645308331")
        );
    }

    #[tokio::test]
    async fn file_for_catalog_id_should_be_word_lyrics() {
        let base = serve(vec![(200, "", AMLL.to_owned())]).await;
        let provider = AmllProvider::with_base_url(&base).unwrap();
        let q = query(None);
        let found = provider
            .lookup(&LyricsRequest {
                catalog_id: Some("1645308331"),
                query: &q,
            })
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found.quality, LyricsQuality::Word);
    }

    #[tokio::test]
    async fn missing_file_should_fall_back_to_isrc() {
        let index =
            "{\"id\":\"1707581520\",\"metadata\":[[\"isrc\",[\"JPP302200613\"]]]}".to_owned();
        let base = serve(vec![
            (404, "", String::new()),
            (200, "", index),
            (200, "", AMLL.to_owned()),
        ])
        .await;
        let provider = AmllProvider::with_base_url(&base).unwrap();
        let q = query(Some("JPP302200613"));
        let found = provider
            .lookup(&LyricsRequest {
                catalog_id: Some("1"),
                query: &q,
            })
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            found.matched.as_deref(),
            Some("AMLL am-lyrics/1707581520.ttml")
        );
    }

    #[tokio::test]
    async fn missing_file_without_isrc_should_be_none() {
        let base = serve(vec![(404, "", String::new())]).await;
        let provider = AmllProvider::with_base_url(&base).unwrap();
        let q = query(None);
        let found = provider
            .lookup(&LyricsRequest {
                catalog_id: Some("1"),
                query: &q,
            })
            .await;
        assert_eq!(found, Ok(None));
    }
}
