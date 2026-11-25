// Owns the Model workspace's asset picker: a search over the install's
// files of one kind (prop cfgs, particle effects, animations, meshes,
// textures) or the game's sounds, listed with folder and texture
// thumbnails, picked by mouse or keys, and what a pick does: add an entry,
// an animation or a sound clip, or set a field. Drops from the Asset
// browser end in the same place.

use super::ModelEditor;
use crate::editor::Editor;
use anno_formats::model::{Play, Sound};
use anno_formats::vfs::Install;
use anno_formats::xml::Id;
use anyhow::Context as _;
use gpui_editor_kit::{
    field::{EditEvent, Filter, TextEdit, text_field},
    gpui::{prelude::*, *},
    theme,
    tree::{self, Item, Tree},
    widgets::{self, Tone},
};

/// How many hits the picker lists: a search of "a" over 430k files would
/// otherwise lay out a list nobody scrolls.
const HITS: usize = 400;

/// What a picked file is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Prop,
    Effect,
    Animation,
    Mesh,
    Texture,
    /// An `Audio` asset, by name: not a file.
    Sound,
}

impl Kind {
    pub fn noun(self) -> &'static str {
        match self {
            Kind::Prop => "a prop",
            Kind::Effect => "a particle effect",
            Kind::Animation => "an animation",
            Kind::Mesh => "a mesh",
            Kind::Texture => "a texture",
            Kind::Sound => "a sound",
        }
    }

    /// Whether the shipped file at `path` is one.
    pub fn takes(self, path: &str) -> bool {
        match self {
            Kind::Prop => path.ends_with(".cfg") || path.ends_with(".prp"),
            // 117's and 1800's baked effects, 2070's emitter files.
            Kind::Effect => path.ends_with(".rdp") || path.ends_with(".efc"),
            // Animation rdms live in `anim`/`animations` folders; meshes in
            // `rdm` ones.
            Kind::Animation => path.ends_with(".rdm") && path.contains("anim"),
            Kind::Mesh => path.ends_with(".rdm") && !path.contains("/anim"),
            Kind::Texture => path.ends_with(".dds"),
            Kind::Sound => false,
        }
    }

    /// `path` as a cfg names it: a texture by its source (`maps/x.psd`
    /// ships as `maps/x_0.dds`), the rest as shipped.
    pub fn cfg_path(self, path: &str) -> String {
        match (self, path.strip_suffix("_0.dds")) {
            (Kind::Texture, Some(stem)) => format!("{stem}.psd"),
            _ => path.to_owned(),
        }
    }

    /// Of a file dropped from the Asset browser on the model: what it
    /// adds, if anything. A texture drops on a field, not as a decal.
    pub fn of_drop(path: &str) -> Option<Kind> {
        [Kind::Prop, Kind::Effect, Kind::Animation, Kind::Mesh]
            .into_iter()
            .find(|k| k.takes(path))
    }
}

/// What a pick does.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Want {
    /// Adds an entry of this kind: a prop, a particle effect, a mesh, or a
    /// decal of a texture.
    Add(Kind),
    /// Adds an animation to this part entry.
    Animation(Id),
    /// Sets this leaf to the file.
    Field(Id, Kind),
    /// Adds a clip to this `Track` that plays the sound from `at` seconds.
    Sound { track: Id, at: f32 },
    /// Opens the cfg or prp in the Model workspace in place of this one.
    Open,
}

impl Want {
    fn kind(self) -> Kind {
        match self {
            Want::Add(k) | Want::Field(_, k) => k,
            Want::Animation(_) => Kind::Animation,
            Want::Open => Kind::Prop,
            Want::Sound { .. } => Kind::Sound,
        }
    }
}

pub struct Picker {
    pub want: Want,
    pub query: TextEdit<()>,
    /// Shipped paths; a sound's name.
    pub hits: Vec<String>,
    /// The highlighted hit.
    pub at: usize,
    rows: Tree<()>,
}

/// Files of `kind` whose path holds `query`; with no query, the ones in
/// and under `near`'s folder and its parent's, nearest first.
fn search_files(install: &Install, near: &str, kind: Kind, query: &str) -> Vec<String> {
    let mut hits: Vec<String> = match query.trim().is_empty() {
        false => (install.search(query.trim(), HITS * 20).into_iter())
            .map(|f| install.file_path(f))
            .filter(|p| kind.takes(p))
            .take(HITS)
            .collect(),
        true => {
            // The model's folder and its parent's: a building's props
            // and maps sit beside it.
            let dir = near.rsplit_once('/').map_or("", |(d, _)| d);
            let up = dir.rsplit_once('/').map_or(dir, |(d, _)| d);
            (install.search(&format!("{up}/"), HITS * 20).into_iter())
                .map(|f| install.file_path(f))
                .filter(|p| kind.takes(p))
                .take(HITS)
                .collect()
        }
    };
    hits.sort_by_key(|p| (p.matches('/').count(), p.clone()));
    hits
}

impl ModelEditor {
    /// Hits for `query`: files of `kind` whose path holds it; with no
    /// query, the ones in and under the cfg's own folder, nearest first.
    fn search(&self, kind: Kind, query: &str) -> Vec<String> {
        match kind {
            Kind::Sound => self.search_sounds(query),
            _ => search_files(&self.install, &self.path, kind, query),
        }
    }

    /// Sounds whose name holds every word of `query`, by name; with no
    /// query, the ones this model's sequences play.
    fn search_sounds(&self, query: &str) -> Vec<String> {
        let Some(sounds) = &self.sounds else {
            return Vec::new();
        };
        let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
        let mut hits: Vec<&str> = match words.is_empty() {
            true => (self.model.sequences.iter().flat_map(|s| &s.clips))
                .filter_map(|c| match c.what {
                    Play::Sound(s) => sounds.name(s.guid),
                    _ => None,
                })
                .collect(),
            false => (sounds.sounds())
                .map(|(_, name)| name)
                .filter(|name| {
                    let name = name.to_lowercase();
                    words.iter().all(|w| name.contains(w.as_str()))
                })
                .collect(),
        };
        // Sound events first: 2,966 of 117's 2,977 sound clips play an
        // `evt_` one, 4,840 of 1800's 5,342 a `wwe_` or `wwc_` one; most
        // other sounds are voice lines (13.7k in 117, 19.6k `Play_` in 1800).
        let event = |n: &str| ["evt_", "wwe_", "wwc_"].iter().any(|p| n.starts_with(p));
        hits.sort_by_cached_key(|n| (!event(n), n.to_lowercase()));
        hits.dedup();
        hits.into_iter().take(HITS).map(str::to_owned).collect()
    }

    /// Does what `want` asks with `path`, a shipped file or a sound's name.
    pub fn pick(&mut self, want: Want, path: &str) -> anyhow::Result<()> {
        match want {
            Want::Sound { track, at } => {
                let guid = (self.sounds.as_ref())
                    .and_then(|s| s.guid_of(path))
                    .with_context(|| format!("no sound named {path}"))?;
                self.add_clip(track, Play::Sound(Sound::of(guid)), at)
                    .map(|_| ())
            }
            Want::Add(Kind::Prop) => self.add("Files", path),
            // 2070 places its `.efc` as `m_Effects` entries.
            Want::Add(Kind::Effect) => match self.doc.prefix() {
                "m_" => self.add("Effects", path),
                _ => self.add("Particles", path),
            },
            Want::Add(Kind::Mesh) => self.add_mesh(path).map(|_| ()),
            Want::Add(Kind::Texture) => self.add_decal(path).map(|_| ()),
            Want::Add(k) => anyhow::bail!("{} is not added as an entry", k.noun()),
            Want::Animation(part) => self.add_animation(part, path),
            Want::Field(leaf, kind) => {
                self.set_value(leaf, &kind.cfg_path(path));
                Ok(())
            }
            // The editor opens it: it replaces this model.
            Want::Open => anyhow::bail!("opening a model is the editor's"),
        }
    }
}

impl Editor {
    /// A file from the Asset browser dropped on the model: a prop, an
    /// effect or a mesh places at the origin, an animation goes to the
    /// selected part.
    pub(crate) fn model_drop(&mut self, path: &str, cx: &mut Context<Self>) {
        let Some(m) = &mut self.model else { return };
        let part = (m.selected).filter(|&s| m.model.parts.iter().any(|p| p.element == s));
        let done = match (Kind::of_drop(path), part) {
            (Some(Kind::Animation), Some(part)) => m.pick(Want::Animation(part), path),
            (Some(Kind::Animation), None) => {
                Err(anyhow::anyhow!("select the mesh it animates first"))
            }
            (Some(kind), _) => m.pick(Want::Add(kind), path),
            (None, _) => Err(anyhow::anyhow!(
                "drop a prop's .cfg, a particle effect, a mesh or an animation; a texture drops on a field"
            )),
        };
        if let Err(e) = done {
            m.message = Some((format!("Not added: {e:#}"), true));
        }
        self.model_edited(cx);
    }

    /// The picker's hits for `query`. Open lists the models opened this
    /// session first, then the open model's neighbours; it needs no model.
    fn picker_hits(&self, want: Want, query: &str) -> Vec<String> {
        if want != Want::Open {
            return (self.model.as_ref()).map_or_else(Vec::new, |m| m.search(want.kind(), query));
        }
        let Some(install) = &self.install else {
            return Vec::new();
        };
        let current = self.model.as_ref().map(|m| m.path.as_str());
        let mut hits: Vec<String> = match (query.trim().is_empty(), current) {
            (true, near) => (self.model_recent.iter().cloned())
                .chain(near.map_or_else(Vec::new, |n| search_files(install, n, Kind::Prop, "")))
                .collect(),
            (false, near) => search_files(install, near.unwrap_or(""), Kind::Prop, query),
        };
        let mut seen = std::collections::HashSet::new();
        hits.retain(|p| Some(p.as_str()) != current && seen.insert(p.clone()));
        hits
    }

    pub(crate) fn open_model_picker(&mut self, want: Want, cx: &mut Context<Self>) {
        if want != Want::Open && self.model.is_none() {
            return;
        }
        if let Some(m) = &mut self.model {
            m.typing = None;
        }
        let hits = self.picker_hits(want, "");
        self.model_picker = Some(Picker {
            want,
            query: TextEdit::new((), "", Filter::Text),
            hits,
            at: 0,
            rows: Tree::default(),
        });
        cx.notify();
    }

    /// Plays the highlighted sound, to hear it before picking.
    fn model_picker_listen(&mut self) {
        let guid = (self.model_picker.as_ref())
            .and_then(|p| p.hits.get(p.at))
            .zip(self.model.as_ref().and_then(|m| m.sounds.clone()))
            .and_then(|(name, sounds)| sounds.guid_of(name));
        if let Some(guid) = guid {
            self.listen_model_sound(guid);
        }
    }

    fn model_picker_choose(&mut self, i: usize, cx: &mut Context<Self>) {
        let Some(p) = self.model_picker.take() else {
            return;
        };
        if p.want == Want::Open {
            if let Some(path) = p.hits.get(i) {
                self.open_model(path, cx);
            }
            return cx.notify();
        }
        let (Some(m), Some(path)) = (&mut self.model, p.hits.get(i)) else {
            return;
        };
        if let Err(e) = m.pick(p.want, path) {
            m.message = Some((format!("Not added: {e:#}"), true));
        }
        self.model_edited(cx);
    }

    /// Keys while the picker is open: typing searches, arrows move, Enter
    /// picks, Esc closes. Takes every key, as a dialog does.
    pub(crate) fn model_picker_key(
        &mut self,
        event: &KeyDownEvent,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(p) = &mut self.model_picker else {
            return false;
        };
        match event.keystroke.key.as_str() {
            "down" => p.at = (p.at + 1).min(p.hits.len().saturating_sub(1)),
            "up" => p.at = p.at.saturating_sub(1),
            "enter" => {
                let at = p.at;
                self.model_picker_choose(at, cx);
                return true;
            }
            "escape" => self.model_picker = None,
            _ => {
                if p.query.key_down_in(event, cx) == EditEvent::Changed {
                    let (want, query) = (p.want, p.query.text.clone());
                    let hits = self.picker_hits(want, &query);
                    let p = self.model_picker.as_mut().expect("open");
                    (p.hits, p.at) = (hits, 0);
                }
            }
        }
        if let Some(p) = &self.model_picker {
            p.rows.scroll_to(p.at);
        }
        cx.notify();
        true
    }

    /// The picker over the window, when open.
    pub(crate) fn render_model_picker(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let p = self.model_picker.as_ref()?;
        let kind = p.want.kind();
        let items: Vec<(String, String)> = (p.hits.iter())
            .map(|h| match h.rsplit_once('/') {
                Some((dir, name)) => (name.to_owned(), dir.to_owned()),
                None => (h.clone(), String::new()),
            })
            .collect();
        let at = p.at;
        // Texture hits show their picture, as the Asset browser's rows do.
        let thumbs: Vec<Option<std::sync::Arc<RenderImage>>> = match kind {
            Kind::Texture => (p.hits.iter())
                .take(60)
                .map(|h| self.texture_thumb(h, cx))
                .collect(),
            _ => Vec::new(),
        };
        let p = self.model_picker.as_ref()?;
        let list = tree::List::new(
            "model-picker-list",
            items.len(),
            move |i| Item {
                depth: 0,
                disclosure: widgets::Disclosure::Leaf,
                label: items[i].0.clone().into(),
                detail: items[i].1.clone().into(),
                selected: i == at,
            },
            |this: &mut Editor, i, _, cx| this.model_picker_choose(i, cx),
        )
        .icon(move |i| thumbs.get(i).cloned().flatten())
        .render(&p.rows, window, cx);
        let opening = p.want == Want::Open;
        let found = match (p.hits.len(), p.query.text.is_empty()) {
            (0, true) if opening => "Type to search the game's models".to_owned(),
            (n, true) if opening => {
                format!("{n} opened before or near this one; type to search the game")
            }
            (0, true) if kind == Kind::Sound => {
                "This model plays no sounds yet; type to search the game's".to_owned()
            }
            (0, _) => "Nothing found".to_owned(),
            (n, true) if kind == Kind::Sound => {
                format!("{n} this model plays; type to search the game's")
            }
            (n, true) => format!("{n} near this model; type to search the game"),
            (n, false) if n >= HITS => format!("The first {HITS}; type more to narrow"),
            (n, false) => format!("{n} found"),
        };
        let title = match opening {
            true => "Open a model".to_owned(),
            false => format!("Pick {}", kind.noun()),
        };
        let card = widgets::dialog(title)
            .w(px(720.))
            .child(text_field(
                "model-picker-query",
                &p.query.text,
                Some(p.query.display()),
                "Search by name or folder",
            ))
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(theme::get().muted)
                    .child(found),
            )
            .child(div().h(px(420.)).child(list.size_full()))
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .children((kind == Kind::Sound).then(|| {
                        widgets::action("Listen", Tone::Secondary)
                            .on_click(cx.listener(|this, _, _, _| this.model_picker_listen()))
                    }))
                    .child(
                        widgets::action("Cancel", Tone::Secondary).on_click(cx.listener(
                            |this, _, _, cx| {
                                this.model_picker = None;
                                cx.notify();
                            },
                        )),
                    )
                    .child(
                        widgets::action(if opening { "Open" } else { "Pick" }, Tone::Primary)
                            .on_click(
                                cx.listener(move |this, _, _, cx| this.model_picker_choose(at, cx)),
                            ),
                    ),
            );
        Some(widgets::modal("model-picker", card).into_any_element())
    }
}

impl Picker {
    /// The query, for the platform's text input.
    pub(crate) fn typing(&mut self) -> &mut dyn gpui_editor_kit::field::InputBuffer {
        &mut self.query
    }
}

#[cfg(test)]
mod tests {
    use super::Kind;

    #[test]
    fn picked_files_read_as_a_cfg_names_them() {
        assert_eq!(
            Kind::Texture.cfg_path("a/maps/x_diff_0.dds"),
            "a/maps/x_diff.psd"
        );
        assert_eq!(Kind::Prop.cfg_path("a/b.cfg"), "a/b.cfg");
        assert!(Kind::Animation.takes("a/anim/walk.rdm") && !Kind::Mesh.takes("a/anim/walk.rdm"));
        assert_eq!(Kind::of_drop("a/rdp/fire.rdp"), Some(Kind::Effect));
        assert_eq!(Kind::of_drop("a/maps/x_0.dds"), None);
        assert_eq!(Kind::of_drop("a/rdm/x_lod0.rdm"), Some(Kind::Mesh));
        assert_eq!(Kind::of_drop("a/anim/walk.rdm"), Some(Kind::Animation));
    }
}
