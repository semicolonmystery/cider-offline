//! User settings for lyrics lookups (the `[lyrics]` table of `config.toml`).

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::storage::LyricsQuality;

/// A source the app can actively ask for lyrics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LyricsProviderId {
    /// Apple Music's own TTML lyrics (`…/syllable-lyrics`, then `…/lyrics`),
    /// fetched with the captured Apple Music tokens.
    Apple,
    /// [LRCLIB](https://lrclib.net), community LRC lyrics.
    Lrclib,
    /// The [AMLL TTML DB](https://github.com/amll-dev/amll-ttml-db):
    /// community word-synced TTML, keyed by Apple Music catalog id.
    Amll,
    /// Netease Cloud Music (unofficial API): word-synced YRC or LRC.
    Netease,
    /// Kugou (unofficial API): word-synced KRC.
    Kugou,
    /// Musixmatch, through Cider's own lyrics backend: richsync (word),
    /// else line-synced.
    Musixmatch,
    /// Lyrics contributed by Cider users ("User Submitted" in Cider), keyed
    /// by Apple Music catalog id.
    Community,
}

impl LyricsProviderId {
    /// Every provider, in the default priority order: Apple, then the
    /// word-capable ones, then the line / plain ones.
    pub const ALL: [Self; 7] = [
        Self::Apple,
        Self::Community,
        Self::Amll,
        Self::Musixmatch,
        Self::Netease,
        Self::Kugou,
        Self::Lrclib,
    ];

    /// Name shown to the user.
    pub fn label(self) -> &'static str {
        match self {
            Self::Apple => "Apple Music",
            Self::Lrclib => "LRCLIB",
            Self::Amll => "AMLL TTML DB",
            Self::Netease => "NetEase Cloud Music",
            Self::Kugou => "Kugou",
            Self::Musixmatch => "Musixmatch",
            Self::Community => "Cider Community",
        }
    }

    /// Stable key (the TOML / UI value).
    pub fn key(self) -> &'static str {
        match self {
            Self::Apple => "apple",
            Self::Lrclib => "lrclib",
            Self::Amll => "amll",
            Self::Netease => "netease",
            Self::Kugou => "kugou",
            Self::Musixmatch => "musixmatch",
            Self::Community => "community",
        }
    }

    /// Position in [`Self::ALL`] (index of per-provider counters).
    pub fn index(self) -> usize {
        Self::ALL.iter().position(|id| *id == self).unwrap_or(0)
    }

    /// Default pause between two lookups (LRCLIB's is a setting, see
    /// [`LyricsSettings::lrclib_spacing_ms`]). The unofficial APIs get a
    /// gentle pace so a bulk fetch does not trip their anti-abuse limits.
    pub fn default_spacing(self) -> Duration {
        Duration::from_millis(match self {
            Self::Apple => 0,
            Self::Amll | Self::Community => 100,
            // Cider's backend allows ~30 requests per window; stay well under.
            Self::Musixmatch => 400,
            Self::Lrclib => 250,
            Self::Kugou => 500,
            Self::Netease => 600,
        })
    }

    /// Parses [`Self::key`].
    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|id| id.key() == key)
    }
}

impl std::fmt::Display for LyricsProviderId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

/// What a lookup does when a provider fails (rate limit, server error,
/// timeout, network error). "No lyrics" is never a failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailurePolicy {
    /// Move on at once, and retry the failed songs after the whole batch
    /// (up to [`LyricsSettings::retry_passes`] passes, waiting longer before
    /// each).
    #[default]
    SkipAndRetryLater,
    /// Wait (2 s, 5 s, 15 s, 30 s, or the provider's `Retry-After`) and retry
    /// the same song before moving on.
    WaitAndRetry,
    /// Move on; the song stays without lyrics until the next manual fetch.
    Skip,
}

impl FailurePolicy {
    /// Every policy, in the order the UI lists them.
    pub const ALL: [Self; 3] = [Self::SkipAndRetryLater, Self::WaitAndRetry, Self::Skip];

    /// Position in [`Self::ALL`] (the UI's index).
    pub fn index(self) -> usize {
        Self::ALL.iter().position(|p| *p == self).unwrap_or(0)
    }

    /// Policy at `index` of [`Self::ALL`]; out of range gives the default.
    pub fn from_index(index: usize) -> Self {
        Self::ALL.get(index).copied().unwrap_or_default()
    }
}

/// One entry of the provider priority list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderSetting {
    /// Which provider.
    pub id: LyricsProviderId,
    /// Whether it is asked at all.
    pub enabled: bool,
}

/// Lyrics lookup settings. Missing keys take their defaults.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LyricsSettings {
    /// Providers in priority order (first is asked first).
    pub providers: Vec<ProviderSetting>,
    /// What to do when a provider in [`Self::policy_providers`] fails.
    pub on_failure: FailurePolicy,
    /// Providers the failure policy applies to; the others always wait and
    /// retry.
    pub policy_providers: Vec<LyricsProviderId>,
    /// Providers are asked in order until lyrics at least this good are
    /// found (word-synced: always look for the best); otherwise the best
    /// lyrics seen are kept. [`LyricsQuality::None`] counts as `Plain`.
    pub target_quality: LyricsQuality,
    /// Fetch lyrics for every song once its download finished.
    pub fetch_after_download: bool,
    /// Pause between two LRCLIB lookups, in milliseconds.
    pub lrclib_spacing_ms: u32,
    /// Retry passes over the skipped songs with
    /// [`FailurePolicy::SkipAndRetryLater`].
    pub retry_passes: u8,
    /// "Always use downloaded lyrics": while online, answer Cider's lyrics
    /// requests from the local lyrics when a song has any, without asking
    /// Apple (as offline). Songs without local lyrics still go to Apple.
    /// Custom lyrics always win, whatever this says.
    pub always_use_downloaded: bool,
}

/// The choices of [`LyricsSettings::target_quality`], best first (the UI's order).
pub const TARGET_QUALITIES: [LyricsQuality; 3] = [
    LyricsQuality::Word,
    LyricsQuality::Line,
    LyricsQuality::Plain,
];

/// Largest accepted [`LyricsSettings::lrclib_spacing_ms`].
pub const MAX_SPACING_MS: u32 = 10_000;
/// Largest accepted [`LyricsSettings::retry_passes`].
pub const MAX_RETRY_PASSES: u8 = 10;

impl Default for LyricsSettings {
    fn default() -> Self {
        Self {
            providers: LyricsProviderId::ALL
                .into_iter()
                .map(|id| ProviderSetting { id, enabled: true })
                .collect(),
            on_failure: FailurePolicy::default(),
            policy_providers: LyricsProviderId::ALL.to_vec(),
            target_quality: LyricsQuality::Word,
            fetch_after_download: true,
            lrclib_spacing_ms: 250,
            retry_passes: 3,
            always_use_downloaded: false,
        }
    }
}

impl LyricsSettings {
    /// A copy where every known provider appears exactly once (first
    /// occurrence wins, missing ones are appended enabled) and the numbers
    /// are in range.
    #[must_use]
    pub fn normalized(&self) -> Self {
        let mut providers: Vec<ProviderSetting> = Vec::with_capacity(LyricsProviderId::ALL.len());
        for entry in &self.providers {
            if !providers.iter().any(|p| p.id == entry.id) {
                providers.push(*entry);
            }
        }
        let mut policy_providers = Vec::new();
        for id in &self.policy_providers {
            if !policy_providers.contains(id) {
                policy_providers.push(*id);
            }
        }
        // Providers this config has never seen (added in a later version)
        // join the end of the order, enabled and under the failure policy,
        // like the defaults.
        for id in LyricsProviderId::ALL {
            if !providers.iter().any(|p| p.id == id) {
                providers.push(ProviderSetting { id, enabled: true });
                if !policy_providers.contains(&id) {
                    policy_providers.push(id);
                }
            }
        }
        Self {
            providers,
            policy_providers,
            lrclib_spacing_ms: self.lrclib_spacing_ms.min(MAX_SPACING_MS),
            retry_passes: self.retry_passes.clamp(1, MAX_RETRY_PASSES),
            target_quality: self.target_quality.max(LyricsQuality::Plain),
            ..self.clone()
        }
    }

    /// Enabled providers in priority order.
    pub fn enabled_providers(&self) -> impl Iterator<Item = LyricsProviderId> + '_ {
        self.providers.iter().filter(|p| p.enabled).map(|p| p.id)
    }

    /// The failure policy for `id`: [`Self::on_failure`] when the policy
    /// applies to it, otherwise [`FailurePolicy::WaitAndRetry`].
    pub fn policy_for(&self, id: LyricsProviderId) -> FailurePolicy {
        if self.policy_providers.contains(&id) {
            self.on_failure
        } else {
            FailurePolicy::WaitAndRetry
        }
    }

    /// Pause between two lookups of `id`.
    pub fn spacing(&self, id: LyricsProviderId) -> Duration {
        match id {
            LyricsProviderId::Lrclib => Duration::from_millis(u64::from(self.lrclib_spacing_ms)),
            other => other.default_spacing(),
        }
    }

    /// Moves provider `id` one place up (`up`) or down in the priority list.
    pub fn move_provider(&mut self, id: LyricsProviderId, up: bool) {
        let Some(at) = self.providers.iter().position(|p| p.id == id) else {
            return;
        };
        let to = if up {
            at.checked_sub(1)
        } else {
            Some(at + 1).filter(|to| *to < self.providers.len())
        };
        if let Some(to) = to {
            self.providers.swap(at, to);
        }
    }

    /// Turns provider `id` on or off.
    pub fn set_enabled(&mut self, id: LyricsProviderId, enabled: bool) {
        if let Some(entry) = self.providers.iter_mut().find(|p| p.id == id) {
            entry.enabled = enabled;
        } else {
            self.providers.push(ProviderSetting { id, enabled });
        }
    }

    /// Includes or excludes provider `id` from the failure policy.
    pub fn set_policy_applies(&mut self, id: LyricsProviderId, applies: bool) {
        self.policy_providers.retain(|p| *p != id);
        if applies {
            self.policy_providers.push(id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalized_should_append_missing_and_drop_duplicates() {
        let settings = LyricsSettings {
            providers: vec![
                ProviderSetting {
                    id: LyricsProviderId::Lrclib,
                    enabled: false,
                },
                ProviderSetting {
                    id: LyricsProviderId::Lrclib,
                    enabled: true,
                },
            ],
            ..LyricsSettings::default()
        }
        .normalized();
        assert_eq!(
            settings.providers[..2],
            [
                ProviderSetting {
                    id: LyricsProviderId::Lrclib,
                    enabled: false
                },
                ProviderSetting {
                    id: LyricsProviderId::Apple,
                    enabled: true
                },
            ]
        );
    }

    const SAVED: &str = "policy_providers = [\"lrclib\"]\n\
        [[providers]]\nid = \"apple\"\nenabled = true\n\
        [[providers]]\nid = \"lrclib\"\nenabled = false\n";

    #[test]
    fn normalized_should_append_new_providers_after_saved_order() {
        let settings = toml::from_str::<LyricsSettings>(SAVED)
            .unwrap()
            .normalized();
        let order: Vec<_> = settings
            .providers
            .iter()
            .map(|p| (p.id, p.enabled))
            .collect();
        assert_eq!(
            order,
            [
                (LyricsProviderId::Apple, true),
                (LyricsProviderId::Lrclib, false),
                (LyricsProviderId::Community, true),
                (LyricsProviderId::Amll, true),
                (LyricsProviderId::Musixmatch, true),
                (LyricsProviderId::Netease, true),
                (LyricsProviderId::Kugou, true),
            ]
        );
    }

    #[test]
    fn new_providers_should_join_the_failure_policy() {
        let settings = toml::from_str::<LyricsSettings>(SAVED)
            .unwrap()
            .normalized();
        assert_eq!(
            settings.policy_providers,
            [
                LyricsProviderId::Lrclib,
                LyricsProviderId::Community,
                LyricsProviderId::Amll,
                LyricsProviderId::Musixmatch,
                LyricsProviderId::Netease,
                LyricsProviderId::Kugou,
            ]
        );
    }

    #[test]
    fn keys_should_roundtrip() {
        assert!(
            LyricsProviderId::ALL
                .into_iter()
                .all(|id| LyricsProviderId::from_key(id.key()) == Some(id))
        );
    }

    #[test]
    fn key_should_match_toml_name() {
        let text = toml::to_string(&ProviderSetting {
            id: LyricsProviderId::Netease,
            enabled: true,
        })
        .unwrap();
        assert!(text.contains("\"netease\""));
    }

    #[test]
    fn unselected_provider_should_wait_and_retry() {
        let settings = LyricsSettings {
            on_failure: FailurePolicy::Skip,
            policy_providers: vec![LyricsProviderId::Lrclib],
            ..LyricsSettings::default()
        };
        assert_eq!(
            settings.policy_for(LyricsProviderId::Apple),
            FailurePolicy::WaitAndRetry
        );
    }

    #[test]
    fn move_provider_should_swap_neighbours() {
        let mut settings = LyricsSettings::default();
        settings.move_provider(LyricsProviderId::Community, true);
        assert_eq!(
            settings.enabled_providers().take(2).collect::<Vec<_>>(),
            vec![LyricsProviderId::Community, LyricsProviderId::Apple]
        );
    }

    #[test]
    fn settings_should_roundtrip_through_toml() {
        let settings = LyricsSettings {
            on_failure: FailurePolicy::WaitAndRetry,
            ..LyricsSettings::default()
        };
        let text = toml::to_string(&settings).unwrap();
        assert_eq!(toml::from_str::<LyricsSettings>(&text).unwrap(), settings);
    }

    #[test]
    fn always_use_downloaded_should_default_off_when_missing() {
        let settings: LyricsSettings = toml::from_str("retry_passes = 2").unwrap();
        assert!(!settings.always_use_downloaded);
    }

    #[test]
    fn always_use_downloaded_should_survive_normalizing() {
        let settings = LyricsSettings {
            always_use_downloaded: true,
            ..LyricsSettings::default()
        };
        assert!(settings.normalized().always_use_downloaded);
    }
}
