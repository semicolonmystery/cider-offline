//! Settings page: port (with proxy restart), autostart, folders, cache, CA.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;

use cider_offline_core::lyrics::TARGET_QUALITIES;
use cider_offline_core::{
    AutoCacheMode, Config, DetailLevel, FailurePolicy, LyricsProviderId, LyricsSettings, SongsPage,
    clamp_concurrency,
};
use slint::{ComponentHandle as _, ModelRc, VecModel};

use super::{AppContext, LyricsProviderRow, SettingsState, dialogs};
use crate::cleanup;

/// Settings state owned by the UI thread.
#[derive(Default)]
pub struct SettingsData {
    /// Port requested by "Apply" that the proxy has not confirmed yet.
    pending_port: Cell<Option<u16>>,
}

/// Wires the Settings callbacks.
pub fn bind(ctx: &Rc<AppContext>) {
    let Some(ui) = ctx.ui.upgrade() else { return };
    let state = ui.global::<SettingsState>();
    {
        let config = ctx.config();
        let port = i32::from(config.proxy.port);
        state.set_port(port);
        state.set_applied_port(port);
        state.set_start_minimized(config.start_minimized);
        state.set_auto_cache_mode(i32::from(config.auto_cache.as_u8()));
        state.set_detail_level(i32::from(config.detail_level.as_u8()));
        state.set_overwrite_audio(config.overwrite_audio);
        state.set_songs_page(i32::from(config.library.songs_page.as_u8()));
        state.set_overwrite_lyrics(config.overwrite_lyrics);
        state.set_download_concurrency(i32::try_from(config.download_concurrency()).unwrap_or(1));
        fill_lyrics(&state, &config);
    }
    state.set_autostart_supported(ctx.autostart.is_some());
    let start_with_os = ctx.autostart.as_ref().is_some_and(|autostart| {
        autostart.is_enabled().unwrap_or_else(|error| {
            tracing::warn!(%error, "could not read the start-at-login entry");
            false
        })
    });
    state.set_start_with_os(start_with_os);
    state.set_data_dir(ctx.paths.data_dir().display().to_string().into());
    state.set_captures_dir(ctx.paths.captures_dir().display().to_string().into());

    let c = Rc::clone(ctx);
    state.on_apply_port(move |port| apply_port(&c, port));
    let c = Rc::clone(ctx);
    state.on_set_start_with_os(move |enabled| set_start_with_os(&c, enabled));
    let c = Rc::clone(ctx);
    state.on_set_start_minimized(move |enabled| {
        c.update_config(|config| config.start_minimized = enabled);
    });
    let c = Rc::clone(ctx);
    state.on_set_download_concurrency(move |value| set_download_concurrency(&c, value));
    let c = Rc::clone(ctx);
    state.on_set_auto_cache_mode(move |index| set_auto_cache_mode(&c, index));
    let c = Rc::clone(ctx);
    state.on_set_detail_level(move |index| {
        let level = u8::try_from(index).map_or(DetailLevel::Full, DetailLevel::from_u8);
        c.update_config(|config| config.detail_level = level);
    });
    let c = Rc::clone(ctx);
    state.on_set_overwrite_audio(move |enabled| {
        c.update_config(|config| config.overwrite_audio = enabled);
    });
    let c = Rc::clone(ctx);
    state.on_set_songs_page(move |index| set_songs_page(&c, index));
    let c = Rc::clone(ctx);
    state.on_set_overwrite_lyrics(move |enabled| {
        update_lyrics(&c, |config| config.overwrite_lyrics = enabled);
    });
    bind_lyrics(ctx, &state);
    let c = Rc::clone(ctx);
    state.on_open_data_dir(move || c.open_folder(c.paths.data_dir().to_path_buf()));
    let c = Rc::clone(ctx);
    state.on_open_captures_dir(move || c.open_folder(c.paths.captures_dir()));
    let c = Rc::clone(ctx);
    state.on_clear_cache(move || clear_cache(&c));
    let c = Rc::clone(ctx);
    state.on_uninstall_ca(move || {
        let c = Rc::clone(&c);
        dialogs::confirm(
            &c.clone(),
            dialogs::Prompt {
                title: "Uninstall certificate",
                message: "Remove the proxy's certificate from the system trust store? Cider will reject the proxy in Active mode until you install it again.",
                confirm: "Uninstall",
                destructive: true,
            },
            move || super::overview::uninstall_ca(&c),
        );
    });
    let c = Rc::clone(ctx);
    state.on_remove_all_data(move || remove_all_data(&c));
}

/// The note under a provider's name.
fn provider_note(id: LyricsProviderId) -> &'static str {
    match id {
        LyricsProviderId::Apple => {
            "Apple Music's own lyrics, word-synced when available. Uses your Apple Music sign-in."
        }
        LyricsProviderId::Lrclib => "Community lyrics from lrclib.net, line-synced or plain.",
        LyricsProviderId::Amll => {
            "Community word-synced lyrics (AMLL TTML DB on GitHub), matched by Apple Music id."
        }
        LyricsProviderId::Netease => "Word- or line-synced. Unofficial API: may stop working.",
        LyricsProviderId::Kugou => "Word-synced. Unofficial API: may stop working.",
        LyricsProviderId::Musixmatch => {
            "Word- or line-synced, through Cider's lyrics service (as Cider does). Matched by Apple Music id."
        }
        LyricsProviderId::Community => {
            "Lyrics submitted by Cider users, through Cider's lyrics service. Matched by Apple Music id."
        }
    }
}

/// Shows the lyrics settings of `config` in the Settings state.
pub fn fill_lyrics(state: &SettingsState<'_>, config: &Config) {
    let lyrics = config.lyrics.normalized();
    let rows: Vec<LyricsProviderRow> = lyrics
        .providers
        .iter()
        .map(|p| LyricsProviderRow {
            id: p.id.key().into(),
            name: p.id.label().into(),
            note: provider_note(p.id).into(),
            enabled: p.enabled,
            policy: lyrics.policy_providers.contains(&p.id),
        })
        .collect();
    state.set_lyrics_providers(ModelRc::new(VecModel::from(rows)));
    state.set_lyrics_failure_policy(i32::try_from(lyrics.on_failure.index()).unwrap_or(0));
    state.set_lyrics_target_quality(
        TARGET_QUALITIES
            .iter()
            .position(|q| *q == lyrics.target_quality)
            .and_then(|i| i32::try_from(i).ok())
            .unwrap_or(0),
    );
    state.set_lyrics_after_download(lyrics.fetch_after_download);
    state.set_lyrics_always_local(lyrics.always_use_downloaded);
    state.set_lrclib_spacing(i32::try_from(lyrics.lrclib_spacing_ms).unwrap_or(250));
    state.set_lyrics_retry_passes(i32::from(lyrics.retry_passes));
    state.set_overwrite_lyrics(config.overwrite_lyrics);
}

/// Changes the lyrics settings, persists them, refreshes the page and
/// applies them to the running library at once.
fn update_lyrics(ctx: &Rc<AppContext>, change: impl FnOnce(&mut Config)) {
    ctx.update_config(|config| {
        change(config);
        config.lyrics = config.lyrics.normalized();
    });
    let config = ctx.config().clone();
    ctx.with_ui(|ui| fill_lyrics(&ui.global::<SettingsState>(), &config));
    ctx.with_backend(
        move |backend| async move { backend.library.apply_lyrics_config(&config) },
        |_, _| {},
    );
}

fn edit_lyrics(ctx: &Rc<AppContext>, change: impl FnOnce(&mut LyricsSettings)) {
    update_lyrics(ctx, |config| change(&mut config.lyrics));
}

fn bind_lyrics(ctx: &Rc<AppContext>, state: &SettingsState<'_>) {
    let c = Rc::clone(ctx);
    state.on_move_lyrics_provider(move |id, up| {
        if let Some(id) = LyricsProviderId::from_key(&id) {
            edit_lyrics(&c, |lyrics| lyrics.move_provider(id, up));
        }
    });
    let c = Rc::clone(ctx);
    state.on_set_lyrics_provider_enabled(move |id, enabled| {
        if let Some(id) = LyricsProviderId::from_key(&id) {
            edit_lyrics(&c, |lyrics| lyrics.set_enabled(id, enabled));
        }
    });
    let c = Rc::clone(ctx);
    state.on_set_lyrics_policy_provider(move |id, applies| {
        if let Some(id) = LyricsProviderId::from_key(&id) {
            edit_lyrics(&c, |lyrics| lyrics.set_policy_applies(id, applies));
        }
    });
    let c = Rc::clone(ctx);
    state.on_set_lyrics_failure_policy(move |index| {
        let policy = FailurePolicy::from_index(usize::try_from(index).unwrap_or(0));
        edit_lyrics(&c, |lyrics| lyrics.on_failure = policy);
    });
    let c = Rc::clone(ctx);
    state.on_set_lyrics_target_quality(move |index| {
        let quality = usize::try_from(index)
            .ok()
            .and_then(|i| TARGET_QUALITIES.get(i).copied())
            .unwrap_or(TARGET_QUALITIES[0]);
        edit_lyrics(&c, |lyrics| lyrics.target_quality = quality);
    });
    let c = Rc::clone(ctx);
    state.on_set_lyrics_after_download(move |enabled| {
        edit_lyrics(&c, |lyrics| lyrics.fetch_after_download = enabled);
    });
    let c = Rc::clone(ctx);
    state.on_set_lyrics_always_local(move |enabled| {
        edit_lyrics(&c, |lyrics| lyrics.always_use_downloaded = enabled);
        // The proxy reads this on every lyrics request: no restart.
        c.service.set_prefer_local_lyrics(enabled);
    });
    let c = Rc::clone(ctx);
    state.on_set_lrclib_spacing(move |ms| {
        let ms = u32::try_from(ms).unwrap_or(0);
        edit_lyrics(&c, |lyrics| lyrics.lrclib_spacing_ms = ms);
    });
    let c = Rc::clone(ctx);
    state.on_set_lyrics_retry_passes(move |passes| {
        let passes = u8::try_from(passes).unwrap_or(1);
        edit_lyrics(&c, |lyrics| lyrics.retry_passes = passes);
    });
}

fn apply_port(ctx: &Rc<AppContext>, port: i32) {
    let Ok(port) = u16::try_from(port) else {
        show_message(ctx, "Choose a port between 1 and 65535.");
        return;
    };
    if port == 0 {
        show_message(ctx, "Choose a port between 1 and 65535.");
        return;
    }
    ctx.settings.pending_port.set(Some(port));
    show_message(ctx, &format!("Restarting the proxy on port {port}…"));
    let mut config = ctx.config().clone();
    config.proxy.port = port;
    ctx.service.restart(config);
}

/// Called with the port the proxy actually listens on after (re)start.
pub fn on_listening(ctx: &Rc<AppContext>, port: u16) {
    ctx.with_ui(|ui| {
        ui.global::<SettingsState>()
            .set_applied_port(i32::from(port));
    });
    if ctx.settings.pending_port.get() == Some(port) {
        ctx.settings.pending_port.set(None);
        ctx.update_config(|config| config.proxy.port = port);
        show_message(
            ctx,
            &format!("Proxy now listens on port {port}. Update Cider's proxy settings to match."),
        );
    }
}

/// Called when (re)starting the proxy failed.
pub fn on_start_failed(ctx: &Rc<AppContext>, error: &str) {
    if let Some(port) = ctx.settings.pending_port.take() {
        let applied = i32::from(ctx.config().proxy.port);
        ctx.with_ui(|ui| ui.global::<SettingsState>().set_port(applied));
        show_message(ctx, &format!("Could not use port {port}: {error}"));
    }
}

fn set_start_with_os(ctx: &Rc<AppContext>, enabled: bool) {
    let Some(autostart) = ctx.autostart.as_ref().map(Arc::clone) else {
        return;
    };
    ctx.spawn(
        async move {
            tokio::task::spawn_blocking(move || {
                autostart.set_enabled(enabled)?;
                autostart.is_enabled()
            })
            .await
            .map_err(|e| e.to_string())?
        },
        move |ctx, result| match result {
            Ok(now) => {
                ctx.with_ui(|ui| ui.global::<SettingsState>().set_start_with_os(now));
                ctx.update_config(|config| config.start_with_os = now);
                show_message(ctx, "");
            }
            Err(error) => {
                ctx.with_ui(|ui| ui.global::<SettingsState>().set_start_with_os(!enabled));
                show_message(ctx, &format!("Could not change the login item: {error}"));
            }
        },
    );
}

fn set_auto_cache_mode(ctx: &Rc<AppContext>, index: i32) {
    let mode = u8::try_from(index).map_or(AutoCacheMode::Off, AutoCacheMode::from_u8);
    ctx.update_config(|config| config.auto_cache = mode);
    ctx.service.set_auto_cache_mode(mode);
}

/// Persists the "Songs page" choice and applies it to the running proxy at
/// once (the next Songs page request shows it).
fn set_songs_page(ctx: &Rc<AppContext>, index: i32) {
    let page = u8::try_from(index).map_or(SongsPage::Apple, SongsPage::from_u8);
    ctx.update_config(|config| config.library.songs_page = page);
    ctx.service.set_songs_page(page);
}

/// Persists the "Simultaneous downloads" choice and applies it to the
/// running queue at once (no restart).
fn set_download_concurrency(ctx: &Rc<AppContext>, value: i32) {
    let limit = clamp_concurrency(usize::try_from(value).unwrap_or(1));
    let stored = u8::try_from(limit).unwrap_or(u8::MAX);
    ctx.update_config(|config| config.download_concurrency = stored);
    ctx.with_ui(|ui| {
        ui.global::<SettingsState>()
            .set_download_concurrency(i32::from(stored));
    });
    ctx.with_backend(
        move |backend| async move {
            backend.downloads.set_concurrency(limit);
        },
        |_, _| {},
    );
}

fn clear_cache(ctx: &Rc<AppContext>) {
    let c = Rc::clone(ctx);
    dialogs::confirm(
        ctx,
        dialogs::Prompt {
            title: "Clear cache",
            message: "Delete all cached Apple Music responses and artwork? Cider will not work offline until they are cached again.",
            confirm: "Clear",
            destructive: true,
        },
        move || {
            show_message(&c, "Clearing the cache…");
            c.with_backend(
                |backend| async move { backend.library.clear_cache().await },
                |ctx, result| match result {
                    Some(Ok(())) => {
                        show_message(ctx, "Cache cleared.");
                        super::library::reload(ctx);
                    }
                    Some(Err(error)) => {
                        show_message(ctx, &format!("Could not clear the cache: {error}"));
                    }
                    None => show_message(ctx, "The proxy is not running."),
                },
            );
        },
    );
}

/// "Remove all data": confirms, undoes the system integration, then quits so
/// `main` deletes the files once the proxy has stopped.
fn remove_all_data(ctx: &Rc<AppContext>) {
    let c = Rc::clone(ctx);
    let message = format!(
        "This deletes your downloaded music, lyrics, artwork, settings and logs, removes the \
         proxy's certificate and the start-at-login entry, and turns off Cider's proxy \
         setting. This cannot be undone.\n\nFiles are deleted from:\n{}",
        cleanup::removal_locations(&ctx.paths)
    );
    dialogs::confirm(
        ctx,
        dialogs::Prompt {
            title: "Remove all data?",
            message: &message,
            confirm: "Remove everything",
            destructive: true,
        },
        move || {
            set_removing(&c, true);
            show_message(&c, "Removing all data…");
            let paths = c.paths.clone();
            let config = c.config().clone();
            c.spawn(
                async move {
                    tokio::task::spawn_blocking(move || {
                        cleanup::remove_integration(&paths, &config, false)
                    })
                    .await
                },
                |ctx, report| match report {
                    Ok(report) if report.is_ok() => {
                        report.log();
                        finish_data_removal(ctx, report.ca_folder());
                    }
                    Ok(report) => {
                        report.log();
                        on_integration_failed(
                            ctx,
                            &report.failure_summary(),
                            &report.leftovers(),
                            report.ca_folder(),
                        );
                    }
                    // Nothing is known about what ran: keep the CA folder so a
                    // later uninstall can still remove the certificate.
                    Err(error) => on_integration_failed(
                        ctx,
                        &error.to_string(),
                        &[UNKNOWN_LEFTOVERS],
                        cleanup::CaFolder::Keep,
                    ),
                },
            );
        },
    );
}

/// What may stay behind when the integration task itself failed.
const UNKNOWN_LEFTOVERS: &str = "The proxy's certificate, the start-at-login entry and Cider's \
     proxy setting may still be in place. The \"ca\" folder is kept.";

/// The "Some steps failed" dialog text: the failures, what stays behind if
/// the user goes on, then the question.
fn failure_prompt(failures: &str, leftovers: &[&str]) -> String {
    let mut message = format!("{failures}\n\n");
    if !leftovers.is_empty() {
        message.push_str("If you continue:\n");
        for leftover in leftovers {
            message.push_str("- ");
            message.push_str(leftover);
            message.push('\n');
        }
        message.push('\n');
    }
    message.push_str("Delete the app's files and quit anyway?");
    message
}

/// Shows what failed and what stays behind, and lets the user delete the
/// files anyway or stop.
fn on_integration_failed(
    ctx: &Rc<AppContext>,
    failures: &str,
    leftovers: &[&str],
    ca: cleanup::CaFolder,
) {
    show_message(
        ctx,
        &format!("Could not finish removing all data:\n{failures}"),
    );
    let c = Rc::clone(ctx);
    let message = failure_prompt(failures, leftovers);
    dialogs::ask(
        ctx,
        dialogs::Prompt {
            title: "Some steps failed",
            message: &message,
            confirm: "Delete files",
            destructive: true,
        },
        move |yes| {
            if yes {
                finish_data_removal(&c, ca);
            } else {
                set_removing(&c, false);
            }
        },
    );
}

/// Hands the file deletion to `main` and quits.
fn finish_data_removal(ctx: &AppContext, ca: cleanup::CaFolder) {
    ctx.request_data_removal(ca);
    ctx.quit();
}

fn set_removing(ctx: &AppContext, removing: bool) {
    ctx.with_ui(|ui| ui.global::<SettingsState>().set_removing(removing));
}

/// Shows a status line on the Settings page.
pub fn show_message(ctx: &AppContext, message: &str) {
    ctx.with_ui(|ui| ui.global::<SettingsState>().set_message(message.into()));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failure_prompt_should_say_what_stays_behind() {
        let message = failure_prompt(
            "Remove the certificate: denied",
            &[cleanup::StepKind::Certificate.leftover().unwrap()],
        );

        assert_eq!(
            message,
            "Remove the certificate: denied\n\nIf you continue:\n- The proxy's certificate \
             stays trusted by the system. Its \"ca\" folder is kept, so the uninstaller (or \
             running the app with --uninstall-cleanup) can still remove it.\n\nDelete the \
             app's files and quit anyway?"
        );
    }

    #[test]
    fn failure_prompt_should_skip_the_list_without_leftovers() {
        assert_eq!(
            failure_prompt("x", &[]),
            "x\n\nDelete the app's files and quit anyway?"
        );
    }
}
