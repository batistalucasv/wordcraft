//! Command search: a `CommandPalette` in its in-place face, over the registry's commands.

use std::sync::{Arc, Weak};

use craft::behavior::IconId;
use craft::i18n::Direction;
use craft::keymap::CommandRef;
use craft::ui::buttons::IconButton;
use craft::ui::contract::{Bind, Props as _, Show as _, TooltipSpec};
use craft::ui::menu::search::{HelpEntry, SearchIndex, SearchWords, Synonyms};
use craft::ui::patterns::panels::{CommandPalette, PaletteFace};
use craft::ui::runtime::env::{Env, Label, WordTemplate};
use craft::ui::runtime::inspect;
use egui::Ui;
use serde_json::json;

use crate::WordApp;

/// One help entry for each registry command Search offers: its label, with the English label so a search in
/// English finds it in any language. Paper Color opens the selector in Options instead of running with no
/// value, since Search is an interactive entry point.
pub(crate) fn help_entries(app: &WordApp) -> Vec<HelpEntry> {
    app.session
        .registry
        .all()
        .iter()
        .filter(|c| !crate::dialogs::NOT_IN_SEARCH.contains(&c.id))
        .map(|c| {
            let command =
                if c.id == "view.paperColor" { CommandRef::with_params("file.options", json!({"pane": "general"})) } else { CommandRef::new(c.id) };
            HelpEntry::new(command, tl!(c.label).to_string()).with_english(c.label.to_string())
        })
        .collect()
}

/// Opens the search where this window shows its field (the title bar, or Immersive's screen bar). `false`
/// when it shows none: Focus hides it, so the Search dialog takes over.
pub(crate) fn open(app: &mut WordApp) -> bool {
    if matches!(crate::chrome::Screen::of(&app.session.view), crate::chrome::Screen::Focus) {
        return false;
    }
    app.chrome.search = true;
    true
}

/// Whether the search is being typed in: Escape closes it, not Full Screen.
pub(crate) fn is_open(app: &WordApp) -> bool {
    app.chrome.search
}

/// Shows the search field, and its results while it is open, as the inspected region the keytips badge; returns the
/// rect it took. `compact` (a narrow window, #5886) shows the magnifier alone, and the search opens as a box over
/// the window, since the bar has no room for the field.
/// Cached help entries and command search index keyed by registry instance, search words, language, and direction.
pub(crate) struct SearchCache {
    registry: Weak<wordcraft_engine::Registry>,
    words: Weak<SearchWords>,
    lang: crate::i18n::Lang,
    dir: Direction,
    entries: Vec<HelpEntry>,
    index: SearchIndex,
    suggested: Vec<CommandRef>,
}

impl SearchCache {
    fn matches(&self, registry: &Arc<wordcraft_engine::Registry>, words: &Arc<SearchWords>, lang: crate::i18n::Lang, dir: Direction) -> bool {
        self.lang == lang
            && self.dir == dir
            && self.registry.upgrade().is_some_and(|r| Arc::ptr_eq(&r, registry))
            && self.words.upgrade().is_some_and(|w| Arc::ptr_eq(&w, words))
    }
}

/// Returns the cached command search entries and index, rebuilding only when the registry, interface language,
/// flow direction, or search synonym table changes.
fn cached_search(app: &mut WordApp, ui: &Ui) -> Arc<SearchCache> {
    let env = Env::of(ui);
    let view = env.view();
    let words = view.words::<SearchWords>();
    let lang = crate::i18n::current();
    let dir = view.direction();
    if let Some(cache) = app.chrome.search_cache.as_ref().filter(|c| c.matches(&app.session.registry, &words, lang, dir)) {
        return Arc::clone(cache);
    }
    let entries = help_entries(app);
    let index = SearchIndex::new(&view, &entries, &Synonyms::of(&words));
    let suggested = crate::dialogs::SEARCH_SUGGESTIONS.iter().map(|id| CommandRef::new(*id)).collect();
    let cache = Arc::new(SearchCache {
        registry: Arc::downgrade(&app.session.registry),
        words: Arc::downgrade(&words),
        lang,
        dir,
        entries,
        index,
        suggested,
    });
    app.chrome.search_cache = Some(Arc::clone(&cache));
    cache
}

/// Shows the title bar's command search (`Tell me what you want to do`) in its place, or the screen bar's compact icon
/// button opening the dialog (`compact`).
pub(super) fn show(app: &mut WordApp, ui: &mut Ui, compact: bool) -> egui::Rect {
    let cache = cached_search(app, ui);
    let mut open = app.chrome.search;
    let name = tl!("Search commands and help").to_string();
    let ((rect, out), _) = inspect::record(ui, crate::keytips::search_region(), |ui| {
        let mut rect = None;
        if compact {
            let button = IconButton::new(IconId::new("wordcraft.search")).label(name.clone()).tooltip(TooltipSpec::new(name.clone())).show(ui);
            if button.response.clicked() {
                open = true;
            }
            rect = Some(button.response.rect);
        }
        let out = CommandPalette::new(name.clone(), &cache.entries, &cache.index, Bind::bound(&mut open))
            .face(if compact { PaletteFace::Dialog } else { PaletteFace::InPlace })
            .placeholder(Label::from(name.clone()))
            .suggested((Label::from(tl!("Suggested").to_string()), cache.suggested.clone()))
            .fallback(WordTemplate::new(tl!("Find “{text}” in the document")))
            .show(ui);
        (rect.unwrap_or(out.response.rect), out)
    });
    app.chrome.search = open;
    if let Some(text) = out.fallback {
        crate::dialogs::find_in_navigation_pane(app, ui.ctx(), &text);
    }
    rect
}
