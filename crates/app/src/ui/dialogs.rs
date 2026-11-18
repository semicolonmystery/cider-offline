//! Dialogs: in-app Slint confirmations plus `rfd` file pickers.
//!
//! Confirmations are drawn by the app (`ui/dialog.slint`): `rfd`'s message
//! dialogs shell out to zenity on Linux and silently answer "cancel" when it
//! is missing. File pickers still use `rfd`, parented to the main window and
//! spawned as a Slint local future so macOS can show them asynchronously; the
//! result is delivered to `done` on the UI thread.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use rfd::AsyncFileDialog;
use slint::ComponentHandle as _;

use super::{AppContext, DialogState};

/// Audio extensions offered in the file picker.
const AUDIO_EXTENSIONS: &[&str] = &["m4a", "mp3", "flac", "aac", "alac", "wav", "ogg", "opus"];
/// Lyrics extensions offered in the file picker (TTML or LRC).
const LYRICS_EXTENSIONS: &[&str] = &["ttml", "lrc", "xml", "txt"];
/// Image extensions offered in the artwork / cover picker.
const IMAGE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "webp", "gif", "bmp"];

/// Lets the user pick an audio file; `done` receives `None` on cancel.
pub fn pick_audio_file(
    ctx: &AppContext,
    title: &str,
    done: impl FnOnce(Option<PathBuf>) + 'static,
) {
    pick_file(ctx, title, "Audio", AUDIO_EXTENSIONS, done);
}

/// Lets the user pick a lyrics file (TTML / LRC); `None` on cancel.
pub fn pick_lyrics_file(
    ctx: &AppContext,
    title: &str,
    done: impl FnOnce(Option<PathBuf>) + 'static,
) {
    pick_file(ctx, title, "Lyrics", LYRICS_EXTENSIONS, done);
}

/// Lets the user pick an image file for artwork / covers; `None` on cancel.
pub fn pick_image_file(
    ctx: &AppContext,
    title: &str,
    done: impl FnOnce(Option<PathBuf>) + 'static,
) {
    pick_file(ctx, title, "Images", IMAGE_EXTENSIONS, done);
}

/// Shared single-file picker with a named extension filter plus "All files",
/// parented to the main window.
pub fn pick_file(
    ctx: &AppContext,
    title: &str,
    filter_name: &str,
    extensions: &[&str],
    done: impl FnOnce(Option<PathBuf>) + 'static,
) {
    let mut dialog = AsyncFileDialog::new()
        .set_title(title)
        .add_filter(filter_name, extensions)
        .add_filter("All files", &["*"]);
    if let Some(ui) = ctx.ui.upgrade() {
        dialog = dialog.set_parent(&ui.window().window_handle());
    }
    run(async move {
        let file = dialog.pick_file().await;
        done(file.map(|f| f.path().to_path_buf()));
    });
}

/// Text of a confirmation dialog.
#[derive(Clone, Copy)]
pub struct Prompt<'a> {
    /// Heading.
    pub title: &'a str,
    /// Explanation under the heading.
    pub message: &'a str,
    /// Label of the confirm button.
    pub confirm: &'a str,
    /// Styles the confirm button as destructive.
    pub destructive: bool,
}

/// Callback receiving the user's answer.
type Answer = Box<dyn FnOnce(bool)>;

/// The pending answer callback of the open dialog.
#[derive(Default)]
pub struct DialogSlot {
    pending: RefCell<Option<Answer>>,
}

impl DialogSlot {
    /// Registers `done` as the pending callback. A callback that is still
    /// pending is answered `false` first.
    pub fn open(&self, done: Answer) {
        let previous = self.pending.borrow_mut().replace(done);
        if let Some(previous) = previous {
            previous(false);
        }
    }

    /// Runs the pending callback with `yes`; does nothing when none is pending.
    pub fn answer(&self, yes: bool) {
        // Take it out first: the callback may open another dialog.
        let pending = self.pending.borrow_mut().take();
        if let Some(done) = pending {
            done(yes);
        }
    }
}

/// Wires the dialog's `answer` callback to the context's slot.
pub fn bind(ctx: &Rc<AppContext>) {
    let c = Rc::clone(ctx);
    ctx.with_ui(|ui| {
        ui.global::<DialogState>()
            .on_answer(move |yes| c.dialog.answer(yes));
    });
}

/// Asks a yes/no question in the app window; `done` receives the answer
/// (`true` for confirm). Always called, also on Esc or a click outside.
pub fn ask(ctx: &Rc<AppContext>, prompt: Prompt<'_>, done: impl FnOnce(bool) + 'static) {
    let ui = ctx.ui.clone();
    // Registering first answers a still-open dialog `false` (and closes it).
    ctx.dialog.open(Box::new(move |yes| {
        if let Some(ui) = ui.upgrade() {
            ui.global::<DialogState>().set_open(false);
        }
        done(yes);
    }));
    ctx.with_ui(|ui| {
        let state = ui.global::<DialogState>();
        state.set_title(prompt.title.into());
        state.set_message(prompt.message.into());
        state.set_confirm_label(prompt.confirm.into());
        state.set_destructive(prompt.destructive);
        state.set_open(true);
    });
}

/// Like [`ask`], but `done` runs only if the user confirms.
pub fn confirm(ctx: &Rc<AppContext>, prompt: Prompt<'_>, done: impl FnOnce() + 'static) {
    ask(ctx, prompt, move |yes| {
        if yes {
            done();
        }
    });
}

fn run(future: impl Future<Output = ()> + 'static) {
    if let Err(error) = slint::spawn_local(future) {
        tracing::error!(%error, "could not open the dialog");
    }
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    use super::DialogSlot;

    #[test]
    fn second_open_should_cancel_the_first() {
        let slot = DialogSlot::default();
        let first = Rc::new(RefCell::new(Vec::new()));
        let second = Rc::new(RefCell::new(Vec::new()));
        let a = Rc::clone(&first);
        slot.open(Box::new(move |yes| a.borrow_mut().push(yes)));
        let b = Rc::clone(&second);
        slot.open(Box::new(move |yes| b.borrow_mut().push(yes)));
        assert_eq!(*first.borrow(), vec![false]);
        slot.answer(true);
        assert_eq!(*second.borrow(), vec![true]);
    }

    #[test]
    fn answer_should_run_callback_once() {
        let slot = DialogSlot::default();
        let count = Rc::new(Cell::new(0));
        let c = Rc::clone(&count);
        slot.open(Box::new(move |_| c.set(c.get() + 1)));
        slot.answer(true);
        slot.answer(true);
        assert_eq!(count.get(), 1);
    }

    #[test]
    fn answer_without_pending_should_do_nothing() {
        let slot = DialogSlot::default();
        slot.answer(true);
    }
}
