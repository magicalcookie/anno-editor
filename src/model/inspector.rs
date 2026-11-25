// Owns the Model workspace's inspector: the selected entry's placement,
// joint, effect, animations and fields, the model's summary and adding
// entries, the picked joint's pose, and typing into all of them. Shared
// with the other panes: angles as the inspector shows them, and its
// section and hint styles.

use super::{
    Field, LISTS, ModelEditor,
    effect::{Change, Emitter, Knob},
    hierarchy::{Row, row_label},
    picker::{Kind, Want},
    playback::Source,
};
use crate::editor::Editor;
use anno_formats::efc;
use anno_formats::model::{Sound as Clip, Transform};
use anno_formats::xml::Id;
use gpui_editor_kit::{
    field::{EditEvent, Filter, NumberAction, TextEdit, number_field, text_field},
    gpui::{prelude::*, *},
    theme,
    widgets::{self, Tone, Tooltip},
};

/// Degrees about X, Y and Z of a unit quaternion, turned Y, then X, then Z
/// (yaw, pitch, roll for Y up).
pub fn euler(q: [f32; 4]) -> [f32; 3] {
    let [x, y, z, w] = q;
    let m13 = 2. * (x * z + w * y);
    let m23 = 2. * (y * z - w * x);
    let m33 = 1. - 2. * (x * x + y * y);
    let m21 = 2. * (x * y + w * z);
    let m22 = 1. - 2. * (x * x + z * z);
    let m31 = 2. * (x * z - w * y);
    let m11 = 1. - 2. * (y * y + z * z);
    let ax = (-m23).clamp(-1., 1.).asin();
    let (ay, az) = if m23.abs() < 0.999_999 {
        (m13.atan2(m33), m21.atan2(m22))
    } else {
        ((-m31).atan2(m11), 0.)
    };
    [ax, ay, az].map(f32::to_degrees)
}

/// [`euler`] back to a quaternion.
pub fn quaternion(degrees: [f32; 3]) -> [f32; 4] {
    let [x, y, z] = degrees.map(|d| d.to_radians() / 2.);
    let (s1, c1) = x.sin_cos();
    let (s2, c2) = y.sin_cos();
    let (s3, c3) = z.sin_cos();
    [
        s1 * c2 * c3 + c1 * s2 * s3,
        c1 * s2 * c3 - s1 * c2 * s3,
        c1 * c2 * s3 - s1 * s2 * c3,
        c1 * c2 * c3 + s1 * s2 * s3,
    ]
}

/// A placement with one field changed as the inspector names it.
fn with_field(t: Transform, field: Field, v: f32) -> Transform {
    let mut t = t;
    match field {
        Field::Position(a) => t.position[a] = v,
        Field::Rotation(a) => {
            let mut e = euler(t.rotation);
            e[a] = v;
            t.rotation = quaternion(e);
        }
        Field::Scale(a) => t.scale[a] = v,
        _ => {}
    }
    t
}

fn field_value(t: &Transform, field: Field) -> f32 {
    match field {
        Field::Position(a) => t.position[a],
        Field::Rotation(a) => euler(t.rotation)[a],
        Field::Scale(a) => t.scale[a],
        _ => 0.,
    }
}

/// A plain round sprite every game here ships, for new effects.
const NEW_EFFECT_TEXTURE: &str =
    "data/base/graphics/library/effects/shared/maps/circle_shape_03_diff.psd";

/// The particle file a particle entry plays: an `.rdp`, or 2070's `.efc`.
fn particle_file(m: &ModelEditor, entry: Id, ext: &str) -> Option<String> {
    (m.model.particles.iter())
        .find(|p| p.element == entry)
        .map(|p| p.file.clone())
        .filter(|f| f.ends_with(ext))
}

/// The baked effect a particle entry plays.
fn effect_path(m: &ModelEditor, entry: Id) -> Option<String> {
    particle_file(m, entry, ".rdp")
}

/// The 2070 emitter file a particle entry plays.
fn efc_path(m: &ModelEditor, entry: Id) -> Option<String> {
    particle_file(m, entry, ".efc")
}

/// [`Field::Effect`] numbers from here on are [`Knob::ALL`]'s.
const KNOBS: u8 = 32;

/// Sets the effect's number `k` (see [`Field::Effect`]): 0 the frame
/// time, 1 to 4 a material's boost, [`KNOBS`] on the emitter's.
fn effect_number(m: &mut ModelEditor, entry: Id, k: u8, v: f32) -> anyhow::Result<()> {
    if let Some(&knob) = Knob::ALL.get(k.wrapping_sub(KNOBS) as usize) {
        return edit_emitter(m, entry, |e| knob.set(e, v));
    }
    let path = effect_path(m, entry).ok_or_else(|| anyhow::anyhow!("no effect"))?;
    match k {
        0 => m.change_effect(&path, Change::FrameTime(v)),
        _ => m.change_effect(
            &path,
            Change::Boost(k as usize - 1, v.clamp(0., 255.) as u8),
        ),
    }
}

/// Changes the selected entry's emitter, one undo step: an effect made in
/// the editor bakes again, a 2070 `.efc` writes the inspector's emitter of
/// it into its file. 2070 takes no launch or flipbook, so those are dropped.
fn edit_emitter(
    m: &mut ModelEditor,
    entry: Id,
    edit: impl FnOnce(&mut Emitter),
) -> anyhow::Result<()> {
    if let Some(path) = effect_path(m, entry) {
        let mut e = (m.emitter(&path)).ok_or_else(|| anyhow::anyhow!("no emitter"))?;
        edit(&mut e);
        return m.rebake(&path, e);
    }
    let path = efc_path(m, entry).ok_or_else(|| anyhow::anyhow!("no effect"))?;
    let k = m.emitter_slot;
    let efc = m
        .efc(&path)
        .ok_or_else(|| anyhow::anyhow!("{path} does not read"))?;
    let effect =
        (efc.effects().into_iter().nth(k)).ok_or_else(|| anyhow::anyhow!("no emitter {k}"))?;
    let mut e = Emitter {
        effect,
        launch: Default::default(),
        flipbook: None,
    };
    edit(&mut e);
    m.set_efc_effect(&path, k, &e.effect)
}

/// A number field's action for the effect's number `k`, now at `value`.
fn effect_number_action(
    k: u8,
    value: f32,
) -> impl Fn(&mut Editor, NumberAction, &mut Window, &mut Context<Editor>) + 'static {
    move |this, action, _, cx| {
        let Some(m) = &mut this.model else { return };
        let Some(entry) = m.selected else { return };
        match action {
            NumberAction::Step(by) => {
                if let Err(e) = effect_number(m, entry, k, value + by) {
                    m.message = Some((format!("Effect not changed: {e:#}"), true));
                }
            }
            NumberAction::Edit => {
                m.typing = Some(TextEdit::new(
                    Field::Effect(k),
                    value.to_string(),
                    Filter::Number,
                ));
            }
        }
        this.model_edited(cx);
    }
}

pub(super) fn section(text: impl Into<SharedString>) -> Div {
    div()
        .pt_3()
        .pb_1()
        .text_size(px(11.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(theme::get().muted)
        .child(text.into())
}

pub(super) fn hint(text: impl Into<SharedString>) -> Div {
    div()
        .text_size(px(11.))
        .text_color(theme::get().muted)
        .child(text.into())
}

/// What the inspector shows a field being typed into, when it is.
fn typing(m: &ModelEditor, f: Field) -> Option<String> {
    (m.typing.as_ref())
        .filter(|e| e.field == f)
        .map(|e| e.display())
}

/// Whether an effect is the game's, which edits change everywhere it
/// plays, and Make a copy for this model.
fn copy_rows(m: &ModelEditor, entry: Id, path: &str, cx: &mut Context<Editor>) -> Div {
    let dir = m.path.rsplit_once('/').map_or("", |(d, _)| d);
    let mut out = div().flex().flex_col().gap_1();
    if !path.starts_with(dir) {
        out = out.child(hint(
            "Edits change this effect wherever the game uses it; a copy changes it here only",
        ));
    }
    out.child(
        widgets::action("Make a copy for this model", Tone::Secondary).on_click(cx.listener(
            move |this, _, _, cx| {
                if let Some(m) = &mut this.model {
                    match m.copy_effect(entry) {
                        Ok(to) => m.message = Some((format!("This model now plays {to}"), false)),
                        Err(e) => m.message = Some((format!("Not copied: {e:#}"), true)),
                    }
                }
                this.model_edited(cx);
            },
        )),
    )
}

/// What an effect kind draws, as the inspector says it.
fn kind_text(kind: u32) -> String {
    match kind {
        efc::PARTICLE => "Particles: an emitter of sprites".into(),
        efc::BILLBOARD => "Billboard: one sprite a cycle".into(),
        efc::FLAME => "Flame: one sprite; the flame shader's noise is not drawn".into(),
        efc::MODEL => "Mesh: not drawn here".into(),
        efc::LINE => "Line: not drawn here".into(),
        efc::ARROW => "Arrow: not drawn here".into(),
        k => format!("Kind {k}: not drawn here"),
    }
}

/// An emitter's switches, texture and numbers. `authored` is an effect
/// made for 117 or 1800, which also has a launch and a flipbook.
fn emitter_rows(m: &ModelEditor, e: &Emitter, authored: bool, cx: &mut Context<Editor>) -> Div {
    let fx = &e.effect;
    let mut out = (div().flex().flex_col().gap_1())
        .child(section("Emitter"))
        .child(hint(kind_text(fx.kind)));
    if !fx.drawn() {
        return out;
    }
    out = out.child(widgets::segmented(
        "emitter-loop",
        [(true, "Loop".into()), (false, "Once".into())],
        &fx.looped,
        cx,
        |this, on, _, cx| this.model_emitter(|e| e.effect.looped = on, cx),
    ));
    out = match fx.technique {
        0 | 1 => out.child(widgets::segmented(
            "emitter-blend",
            [(0u32, "Blend".into()), (1, "Add".into())],
            &fx.technique,
            cx,
            |this, t, _, cx| this.model_emitter(|e| e.effect.technique = t, cx),
        )),
        2 => out.child(hint("Refracts what is behind it: not drawn here")),
        3 => out.child(hint("Blends, writing depth")),
        4 => out.child(hint("Blends over everything, ignoring depth")),
        5 => out.child(hint("Draws into the water's waves: not drawn here")),
        t => out.child(hint(format!("Technique {t}"))),
    };
    if fx.kind == efc::PARTICLE {
        out = out.child(widgets::segmented(
            "emitter-bound",
            [
                (true, "Follows the emitter".into()),
                (false, "Stays where born".into()),
            ],
            &fx.bound,
            cx,
            |this, on, _, cx| this.model_emitter(|e| e.effect.bound = on, cx),
        ));
    }
    let texture = fx.texture.clone();
    out = out.child(
        text_field(
            "emitter-texture",
            &texture,
            typing(m, Field::EmitterTexture),
            "texture path",
        )
        .on_click(cx.listener(move |this, _, _, cx| {
            if let Some(m) = &mut this.model {
                let edit = TextEdit::new(Field::EmitterTexture, texture.clone(), Filter::Text);
                m.typing = Some(edit);
            }
            cx.notify();
        })),
    );
    let knobs = Knob::ALL.iter().enumerate();
    for (i, knob) in knobs.filter(|(_, k)| k.applies(fx.kind, authored)) {
        let code = KNOBS + i as u8;
        let value = knob.get(e);
        out = out.child(number_field(
            knob.label(),
            value,
            knob.step(),
            typing(m, Field::Effect(code)),
            cx,
            effect_number_action(code, value),
        ));
    }
    out
}

impl Editor {
    pub fn render_model_inspector(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let pose = self.pose_section(cx);
        let Some(m) = &self.model else {
            return self.render_model_peek(cx);
        };
        let mut body = div().flex().flex_col().p_2().gap_1().text_size(px(12.));
        if let Some(pose) = pose {
            body = body.child(pose);
        }
        let Some(entry) = m.selected else {
            return self.model_summary(body, window, cx);
        };
        let list = m.list_of(entry);
        let title = match list.and_then(|l| LISTS.iter().find(|(n, _)| *n == l)) {
            Some((_, kind)) => format!("{kind} › {}", m.label(entry)),
            None => m.label(entry),
        };
        body = body.child(
            div()
                .text_size(px(13.))
                .font_weight(FontWeight::SEMIBOLD)
                .child(title),
        );
        let siblings = m
            .doc
            .parent(entry)
            .map(|l| m.doc.children(l).collect::<Vec<_>>());
        let at = (siblings.as_ref()).and_then(|s| s.iter().position(|&c| c == entry));
        let (up, down) = match (&siblings, at) {
            (Some(s), Some(i)) => (
                (i > 0).then(|| s[i - 1]),
                s.get(i + 1).map(|_| s.get(i + 2).copied()),
            ),
            _ => (None, None),
        };
        let mut actions = div()
            .flex()
            .flex_wrap()
            .gap_1()
            .child(
                widgets::action("Duplicate", Tone::Secondary)
                    .tooltip(Tooltip::text(
                        "Duplicate",
                        "Ctrl D",
                        "A copy right after it",
                    ))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(m) = &mut this.model {
                            m.duplicate(entry);
                        }
                        this.model_edited(cx);
                    })),
            )
            .child(
                widgets::action("Delete", Tone::Secondary)
                    .tooltip(Tooltip::text("Delete", "Del", "Undo brings it back"))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(m) = &mut this.model {
                            m.remove(entry);
                        }
                        this.model_edited(cx);
                    })),
            );
        if let Some(before) = up {
            actions = actions.child(widgets::action("Move up", Tone::Secondary).on_click(
                cx.listener(move |this, _, _, cx| {
                    if let Some(m) = &mut this.model {
                        m.reorder(entry, Some(before));
                    }
                    this.model_edited(cx);
                }),
            ));
        }
        if let Some(after) = down {
            actions = actions.child(widgets::action("Move down", Tone::Secondary).on_click(
                cx.listener(move |this, _, _, cx| {
                    if let Some(m) = &mut this.model {
                        m.reorder(entry, after);
                    }
                    this.model_edited(cx);
                }),
            ));
        }
        if list == Some("Files")
            && let Some(prop) = m.model.props.iter().find(|p| p.element == entry)
        {
            let cfg = prop.cfg.clone();
            let tip = cfg.clone();
            actions = actions.child(
                widgets::action("Open prop", Tone::Secondary)
                    .tooltip(Tooltip::text("Open prop", "Double-click", tip))
                    .on_click(cx.listener(move |this, _, _, cx| this.open_model(&cfg, cx))),
            );
        }
        body = body.child(actions);
        if let Some(t) = m.placement(entry) {
            body = body.child(section("Placement"));
            let rows: [(&'static str, Field, f32); 9] = [
                ("Position X", Field::Position(0), 0.1),
                ("Position Y", Field::Position(1), 0.1),
                ("Position Z", Field::Position(2), 0.1),
                ("Turn X°", Field::Rotation(0), 15.),
                ("Turn Y°", Field::Rotation(1), 15.),
                ("Turn Z°", Field::Rotation(2), 15.),
                ("Scale X", Field::Scale(0), 0.1),
                ("Scale Y", Field::Scale(1), 0.1),
                ("Scale Z", Field::Scale(2), 0.1),
            ];
            for (label, field, step) in rows {
                let editing = (m.typing.as_ref())
                    .filter(|e| e.field == field)
                    .map(|e| e.display());
                let value = field_value(&t, field);
                body = body.child(number_field(
                    label,
                    value,
                    step,
                    editing,
                    cx,
                    move |this, action, _, cx| {
                        let Some(m) = &mut this.model else { return };
                        let Some(t) = m.placement(entry) else { return };
                        match action {
                            NumberAction::Step(by) => {
                                let v = field_value(&t, field) + by;
                                m.set_placement(entry, with_field(t, field, v), true);
                            }
                            NumberAction::Edit => {
                                let text = format!("{}", field_value(&t, field));
                                m.typing = Some(TextEdit::new(field, text, Filter::Number));
                            }
                        }
                        this.model_edited(cx);
                    },
                ));
            }
        }
        if m.placement(entry).is_some() {
            body = body.child(section("Joint"));
            let link = m.rig(entry).and_then(|r| r.link);
            let on = link.and_then(|l| {
                let host = m.model.parts.get(l.model as usize)?;
                let s = host.mesh.as_ref()?.skeleton.as_ref()?;
                Some(format!("{} › {}", host.name, s.name(s.find_hash(l.bone)?)))
            });
            body = body.child(hint(match (&on, link) {
                (Some(on), _) => format!("Hangs on {on}; its placement is in that joint's space"),
                (None, Some(l)) => format!(
                    "Hangs on joint {:#x} of mesh {}, which does not load",
                    l.bone, l.model
                ),
                (None, None) => {
                    "On no joint. Pick a joint in the hierarchy to hang it there".to_owned()
                }
            }));
            let picked = (m.joint)
                .filter(|(part, _)| *part != entry)
                .and_then(|(part, j)| {
                    Some((m.link_to(part, j)?, row_label(m, Row::Joint(part, j)).0))
                });
            let mut row = div().flex().flex_wrap().gap_1();
            if let Some((target, name)) = picked {
                row = row.child(
                    widgets::action(format!("Attach to {name}"), Tone::Secondary)
                        .tooltip(Tooltip::text(
                            "Attach to joint",
                            "",
                            "It keeps its place in the bind pose and moves with the joint",
                        ))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(m) = &mut this.model {
                                m.attach(entry, Some(target));
                            }
                            this.model_edited(cx);
                        })),
                );
            }
            if link.is_some() {
                row = row.child(
                    widgets::action("Detach", Tone::Secondary).on_click(cx.listener(
                        move |this, _, _, cx| {
                            if let Some(m) = &mut this.model {
                                m.attach(entry, None);
                            }
                            this.model_edited(cx);
                        },
                    )),
                );
            }
            body = body.child(row);
        }
        if let Some(effect) = self.effect_section(entry, cx) {
            body = body.child(effect);
        }
        if let Some(effect) = self.efc_section(entry, cx) {
            body = body.child(effect);
        }
        if let Some(sound) = self.sound_section(entry, cx) {
            body = body.child(sound);
        }
        if let Some(animations) = self.animations_section(entry, cx) {
            body = body.child(animations);
        }
        body = body.child(section("Fields"));
        let fields = self.render_fields(entry, window, cx);
        body = body.child(fields);
        self.model_scroll(body)
    }

    /// The inspector with nothing selected, as the Island one heads an
    /// island: the model's name, where it is filed, whose it is, its
    /// file and counts folded under File & stats, problems, and adding
    /// entries.
    fn model_summary(&self, body: Div, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let m = self.model.as_ref().expect("checked by the caller");
        let name = crate::island::pretty(
            self.asset_words(),
            crate::project::models::model_name(&m.path),
        );
        let mut body = body
            .child(
                div()
                    .text_size(px(13.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(name),
            )
            .child(hint(super::list::filed(&m.path)))
            .child(hint(match (m.dirty(), m.in_project) {
                (true, _) => {
                    "Edited: Ctrl S saves it into the project, where it replaces the game's"
                }
                (false, true) => "Saved in the project",
                (false, false) => "The game's own: an edit saves into the project and replaces it",
            }));
        let counts = [
            ("mesh", "meshes", m.model.parts.len()),
            ("prop", "props", m.model.props.len()),
            ("decal", "decals", m.model.decals.len()),
            (
                "particle effect",
                "particle effects",
                m.model.particles.len(),
            ),
            ("light", "lights", m.model.lights.len()),
        ];
        let said: Vec<String> = (counts.iter())
            .filter(|(_, _, n)| *n > 0)
            .map(|&(one, many, n)| crate::project::plural(n, one, many))
            .collect();
        // Folded, with what it holds on its heading, as the Island
        // inspector folds an island's file and numbers.
        let (heading, open) = widgets::collapsible("model-file", "File & stats", false, window, cx);
        body = body.child(crate::island::fold_summary(heading, open, said.join(" · ")));
        if open {
            body = body
                .child(hint(m.path.clone()))
                .child(hint(said.join(" · ")));
            if !m.model.sequences.is_empty() {
                body = body.child(hint(crate::project::plural(
                    m.model.sequences.len(),
                    "sequence",
                    "sequences",
                )));
            }
        }
        if !m.model.problems.is_empty() {
            body = body.child(section("Problems"));
            for p in m.model.problems.iter().take(20) {
                body = body.child(
                    div()
                        .text_size(px(11.))
                        .text_color(theme::get().error)
                        .child(p.clone()),
                );
            }
        }
        body = body.child(section("Add"));
        // 2070's effects are `.efc` emitter files, which add as its own;
        // its lights are not read and its meshes and decals are written
        // another way, so those stay 117's and 1800's. A prp is one mesh.
        let baked = m.doc.prefix().is_empty();
        let cfg = baked && m.doc.child(Id::ROOT, "Prop").is_none();
        let picks = [
            ("Add prop…", Kind::Prop, "a prop's cfg", true),
            (
                "Add particle effect…",
                Kind::Effect,
                if baked { "a baked effect" } else { "an effect" },
                true,
            ),
            (
                "Add mesh…",
                Kind::Mesh,
                "an .rdm; it keeps the materials a shipped cfg beside it gives it, else a plain white one",
                cfg,
            ),
            (
                "Add decal…",
                Kind::Texture,
                "the texture a 2 m terrain decal shows",
                cfg,
            ),
        ];
        let mut row = div().flex().flex_wrap().gap_1();
        for (label, kind, what, _) in picks.into_iter().filter(|p| p.3) {
            row = row.child(
                widgets::action(label, Tone::Secondary)
                    .tooltip(Tooltip::text(
                        label,
                        "",
                        format!("Pick {what} from the game; it lands at the origin"),
                    ))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.open_model_picker(Want::Add(kind), cx);
                    })),
            );
        }
        type Make = fn(&mut ModelEditor) -> anyhow::Result<Id>;
        let made: [(&str, &str, Make); 3] = [
            (
                "Add point light",
                "A warm glow a metre across, copied from a shipped one",
                |m| m.add_light(false),
            ),
            (
                "Add spot light",
                "A spot shining down its own -Y, copied from a shipped one",
                |m| m.add_light(true),
            ),
            (
                "Add cloth",
                "A shipped cloak on a line, mesh and maps included, to change in the inspector",
                ModelEditor::add_cloth,
            ),
        ];
        for (label, tip, add) in made.into_iter().filter(|_| cfg) {
            row = row.child(
                widgets::action(label, Tone::Secondary)
                    .tooltip(Tooltip::text(label, "", tip))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(m) = &mut this.model
                            && let Err(e) = add(m)
                        {
                            m.message = Some((format!("Not added: {e:#}"), true));
                        }
                        this.model_edited(cx);
                    })),
            );
        }
        row = row.when(baked, |row| row.child(
            widgets::action("New particle effect", Tone::Secondary)
                .tooltip(Tooltip::text(
                    "New particle effect",
                    "",
                    "A small plume baked from an emitter you shape in the inspector, saved beside this cfg",
                ))
                .on_click(cx.listener(|this, _, _, cx| {
                    let mut emitter = Emitter::default();
                    emitter.effect.texture = NEW_EFFECT_TEXTURE.to_owned();
                    if let Some(m) = &mut this.model
                        && let Err(e) = m.new_effect(emitter)
                    {
                        m.message = Some((format!("No effect made: {e:#}"), true));
                    }
                    this.model_edited(cx);
                })),
        ));
        body = body.child(row);
        self.model_scroll(body)
    }

    /// The selected sound clip: what it plays, Looped, and the joint it
    /// plays at, which the joint picked in the hierarchy can replace.
    fn sound_section(&mut self, entry: Id, cx: &mut Context<Self>) -> Option<Div> {
        let m = self.model.as_ref()?;
        let s = m.sound_clip(entry)?;
        let name = (m.sounds.as_ref())
            .and_then(|l| l.name(s.guid))
            .map_or_else(
                || format!("Sound {} (not one of the game's)", s.guid),
                str::to_owned,
            );
        let looping = (m.sounds.as_ref())
            .and_then(|l| l.event(s.guid))
            .is_some_and(|e| e.looping);
        let mut out = div()
            .flex()
            .flex_col()
            .gap_1()
            .child(section("Sound"))
            .child(hint(name))
            .child(
                widgets::checkbox(
                    "Looped",
                    s.looped,
                    if looping { "the sound loops itself" } else { "" },
                )
                .tooltip(Tooltip::text(
                    "Looped",
                    "",
                    "Runs from its start until the sequence stops, instead of once each time the playhead crosses it",
                ))
                .on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(m) = &mut this.model {
                        m.set_sound(entry, Clip { looped: !s.looped, ..s });
                    }
                    this.model_edited(cx);
                })),
            )
            .child(hint(m.sound_place(s)));
        let mut row = div().flex().flex_wrap().gap_1();
        let picked = m.picked_sound_joint().filter(|&p| p != (s.model, s.bone));
        if let (Some((model, bone)), Some((part, j))) = (picked, m.joint) {
            let joint = row_label(m, Row::Joint(part, j)).0;
            row = row.child(
                widgets::action(format!("Attach to {joint}"), Tone::Secondary)
                    .tooltip(Tooltip::text(
                        "Attach to joint",
                        "",
                        "The sound plays from the joint as it moves",
                    ))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(m) = &mut this.model {
                            m.set_sound(entry, Clip { model, bone, ..s });
                        }
                        this.model_edited(cx);
                    })),
            );
        } else if m.joint.is_none() {
            out = out.child(hint("Pick a joint in the hierarchy to play it there"));
        }
        if (s.model, s.bone) != (0, 0) {
            row = row.child(
                widgets::action("Detach", Tone::Secondary)
                    .tooltip(Tooltip::text(
                        "Detach",
                        "",
                        "Back to the first mesh's first joint, where most shipped sound clips play",
                    ))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(m) = &mut this.model {
                            m.set_sound(
                                entry,
                                Clip {
                                    model: 0,
                                    bone: 0,
                                    ..s
                                },
                            );
                        }
                        this.model_edited(cx);
                    })),
            );
        }
        Some(out.child(row))
    }

    /// The selected particle entry's effect: what it is, whole-run changes,
    /// each material's drawing, and for an effect made this session its
    /// emitter.
    fn effect_section(&mut self, entry: Id, cx: &mut Context<Self>) -> Option<Div> {
        let m = self.model.as_mut()?;
        let path = effect_path(m, entry)?;
        let mut out = div().flex().flex_col().gap_1().child(section("Effect"));
        let Some(rdp) = m.effect(&path) else {
            return Some(out.child(hint(format!("{path} does not read"))));
        };
        let edited = m.effects.iter().any(|(p, _)| *p == path);
        let emitter = m.emitter(&path);
        out = out.child(hint(path.clone())).child(hint(format!(
            "{} particles · {} frames · {:.2} s · {}{}",
            rdp.starts.len(),
            rdp.frame_count,
            rdp.duration(),
            if rdp.looped { "looped" } else { "once" },
            if edited { " · edited" } else { "" },
        )));
        // Made in the editor: its emitter, baked again on each change.
        if let Some(e) = emitter {
            return Some(out.child(emitter_rows(m, &e, true, cx)));
        }
        out = out.child(copy_rows(m, entry, &path, cx));
        out = out.child(number_field(
            "Frame ms",
            rdp.frame_time,
            5.,
            typing(m, Field::Effect(0)),
            cx,
            effect_number_action(0, rdp.frame_time),
        ));
        out = out.child(widgets::segmented(
            "effect-loop",
            [(true, "Loop".into()), (false, "Once".into())],
            &rdp.looped,
            cx,
            move |this, on, _, cx| this.model_effect_change(entry, Change::Looped(on), cx),
        ));
        let change = |label: &'static str, change: Change, cx: &mut Context<Self>| {
            widgets::action(label, Tone::Secondary).on_click(
                cx.listener(move |this, _, _, cx| this.model_effect_change(entry, change, cx)),
            )
        };
        out = out.child(
            div()
                .flex()
                .flex_wrap()
                .gap_1()
                .child(change("Smaller", Change::Size(0.8), cx))
                .child(change("Larger", Change::Size(1.25), cx))
                .child(change("Darker", Change::Color([0.8, 0.8, 0.8, 1.]), cx))
                .child(change(
                    "Brighter",
                    Change::Color([1.25, 1.25, 1.25, 1.]),
                    cx,
                ))
                .child(change("Clearer", Change::Color([1., 1., 1., 0.8]), cx))
                .child(change("Denser", Change::Color([1., 1., 1., 1.25]), cx)),
        );
        // At most four materials get controls: shipped effects have one to
        // three, and a field's id is its label.
        const BOOST: [&str; 4] = ["Boost 1", "Boost 2", "Boost 3", "Boost 4"];
        for (k, mat) in rdp.materials.iter().enumerate().take(4) {
            out = out
                .child(hint(format!("Material {}", k + 1)).pt_1())
                .child(widgets::segmented(
                    ("effect-blend", k),
                    [(false, "Blend".into()), (true, "Add".into())],
                    &mat.additive,
                    cx,
                    move |this, on, _, cx| {
                        this.model_effect_change(entry, Change::Additive(k, on), cx)
                    },
                ))
                .child(widgets::segmented(
                    ("effect-facing", k),
                    [
                        (1u8, "Faces camera".into()),
                        (2, "Upright".into()),
                        (0, "Own turn".into()),
                    ],
                    &mat.view_aligned,
                    cx,
                    move |this, v, _, cx| this.model_effect_change(entry, Change::Facing(k, v), cx),
                ))
                .child(number_field(
                    BOOST[k],
                    f32::from(mat.color_boost),
                    1.,
                    typing(m, Field::Effect(1 + k as u8)),
                    cx,
                    effect_number_action(1 + k as u8, f32::from(mat.color_boost)),
                ));
            let texture = match mat.movie_tex_path.is_empty() {
                true => rdp.texture_path.clone(),
                false => mat.movie_tex_path.clone(),
            };
            let editing = typing(m, Field::EffectTexture(k));
            out = out.child(
                text_field(("effect-texture", k), &texture, editing, "texture path").on_click(
                    cx.listener(move |this, _, _, cx| {
                        if let Some(m) = &mut this.model {
                            let edit = TextEdit::new(
                                Field::EffectTexture(k),
                                texture.clone(),
                                Filter::Text,
                            );
                            m.typing = Some(edit);
                        }
                        cx.notify();
                    }),
                ),
            );
        }
        Some(out)
    }

    /// The selected entry's 2070 `.efc`: its emitters to pick from, and the
    /// picked one's numbers, written into the file and played baked again.
    fn efc_section(&mut self, entry: Id, cx: &mut Context<Self>) -> Option<Div> {
        let m = self.model.as_mut()?;
        let path = efc_path(m, entry)?;
        let mut out = div().flex().flex_col().gap_1().child(section("Effect"));
        let Some(baked) = m.efc_emitters(&path) else {
            return Some(out.child(hint(format!("{path} does not read"))));
        };
        let (effects, runs) = (&baked.effects, &baked.runs);
        let edited = m.efcs.iter().any(|(p, _)| *p == path);
        out = out.child(hint(path.clone())).child(hint(format!(
            "{} emitters, {} drawn{}",
            effects.len(),
            runs.iter().flatten().count(),
            if edited { " · edited" } else { "" },
        )));
        out = out.child(copy_rows(m, entry, &path, cx));
        let slot = m.emitter_slot.min(effects.len().saturating_sub(1));
        for (k, e) in effects.iter().enumerate() {
            let what = match runs.get(k).and_then(Option::as_ref) {
                Some(run) => format!("{} particles", run.starts.len()),
                None => "not drawn".to_owned(),
            };
            let label = format!("{k}: {} · {}", e.name, what);
            let tone = if k == slot {
                Tone::Primary
            } else {
                Tone::Secondary
            };
            out = out.child(widgets::action(label, tone).on_click(cx.listener(
                move |this, _, _, cx| {
                    if let Some(m) = &mut this.model {
                        (m.emitter_slot, m.typing) = (k, None);
                    }
                    cx.notify();
                },
            )));
        }
        let e = Emitter {
            effect: effects.get(slot)?.clone(),
            launch: Default::default(),
            flipbook: None,
        };
        Some(out.child(emitter_rows(m, &e, false, cx)))
    }

    /// Changes the selected entry's emitter; see [`edit_emitter`].
    fn model_emitter(&mut self, edit: impl FnOnce(&mut Emitter), cx: &mut Context<Self>) {
        if let Some(m) = &mut self.model
            && let Some(entry) = m.selected
            && let Err(e) = edit_emitter(m, entry, edit)
        {
            m.message = Some((format!("Effect not changed: {e:#}"), true));
        }
        self.model_edited(cx);
    }

    /// A part's animations: play one, remove one, add an rdm by path. A
    /// sequence names them by position, so removing one shifts the ids of
    /// those after it.
    fn animations_section(&mut self, entry: Id, cx: &mut Context<Self>) -> Option<Div> {
        let m = self.model.as_ref()?;
        let part = m.model.parts.iter().position(|p| p.element == entry)?;
        let p = &m.model.parts[part];
        if p.mesh.as_ref().and_then(|r| r.skeleton.as_ref()).is_none() && p.animations.is_empty() {
            return None;
        }
        let list = m.doc.child(entry, "Animations");
        let configs: Vec<Id> = (list.into_iter())
            .flat_map(|l| m.doc.children(l))
            .filter(|&c| m.doc.name(c) == "Config")
            .collect();
        let mut out = div().flex().flex_col().gap_1().child(section("Animations"));
        for (k, path) in p.animations.iter().enumerate() {
            let file = path.rsplit('/').next().unwrap_or(path).to_owned();
            let config = configs.get(k).copied();
            let source = Source::Animation { part, animation: k };
            out = out.child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(div().flex_1().truncate().child(format!("{k} · {file}")))
                    .child(widgets::action("Play", Tone::Secondary).on_click(cx.listener(
                        move |this, _, _, cx| {
                            if let Some(m) = &mut this.model {
                                m.play(source);
                                m.playback.playing = true;
                            }
                            this.model_tick(cx);
                            cx.notify();
                        },
                    )))
                    .when_some(config, |d, config| {
                        d.child(
                            widgets::action("Remove", Tone::Secondary)
                                .tooltip(Tooltip::text(
                                    "Remove animation",
                                    "",
                                    "Sequences name animations by position: those after it move up one",
                                ))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    if let Some(m) = &mut this.model {
                                        m.remove(config);
                                        m.select(Some(entry));
                                    }
                                    this.model_edited(cx);
                                })),
                        )
                    }),
            );
        }
        out = out.child(
            widgets::action("Add animation…", Tone::Secondary)
                .tooltip(Tooltip::text(
                    "Add animation",
                    "",
                    "Pick one from the game, or drag one from the Asset browser onto the model",
                ))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.open_model_picker(Want::Animation(entry), cx);
                })),
        );
        if p.mesh.as_ref().and_then(|r| r.skeleton.as_ref()).is_none() {
            return Some(out);
        }
        let length = m.new_animation_length;
        let typing = (m.typing.as_ref())
            .filter(|e| e.field == Field::NewAnimationLength)
            .map(|e| e.display());
        out = out
            .child(number_field(
                "New length s",
                length,
                0.5,
                typing,
                cx,
                move |this, action, _, cx| {
                    let Some(m) = &mut this.model else { return };
                    match action {
                        NumberAction::Step(by) => {
                            m.new_animation_length = (length + by).max(0.1);
                        }
                        NumberAction::Edit => {
                            m.typing = Some(TextEdit::new(
                                Field::NewAnimationLength,
                                length.to_string(),
                                Filter::Number,
                            ));
                        }
                    }
                    cx.notify();
                },
            ))
            .child(
                widgets::action("New animation", Tone::Secondary)
                    .tooltip(Tooltip::text(
                        "New animation",
                        "",
                        "A new animation in the bind pose, saved beside this cfg: pick a joint and key it",
                    ))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(m) = &mut this.model {
                            let seconds = m.new_animation_length;
                            match m.new_animation(part, seconds) {
                                Ok(path) => {
                                    m.message = Some((format!("This model now plays {path}"), false))
                                }
                                Err(e) => {
                                    m.message = Some((format!("No new animation: {e:#}"), true))
                                }
                            }
                        }
                        this.model_edited(cx);
                    })),
            );
        Some(out)
    }

    /// With a joint picked while its part plays: the joint's pose at the
    /// playhead in its parent's space, keyed by typing or by the gizmo, and
    /// keying, removing a key and copying the animation for this model.
    fn pose_section(&mut self, cx: &mut Context<Self>) -> Option<Div> {
        let m = self.model.as_mut()?;
        let k = m.keying()?;
        let local = m.joint_local()?;
        let keyed = m.joint_key().is_some();
        let joint = row_label(m, Row::Joint(m.model.parts[k.part].element, k.joint as u16)).0;
        let file = k.path.rsplit('/').next().unwrap_or(&k.path).to_owned();
        let edited = m.animations.iter().any(|(p, _)| *p == k.path);
        let mut out = div()
            .flex()
            .flex_col()
            .gap_1()
            .child(section("Pose"))
            .child(hint(format!(
                "{joint} in {file} at {:.2} s{}{}",
                k.time,
                if keyed { " · keyed here" } else { "" },
                if edited { " · edited" } else { "" },
            )))
            .child(hint(
                "Move or turn it with the gizmo, or type its pose: either keys it here",
            ));
        let typing = |m: &ModelEditor, f: Field| {
            (m.typing.as_ref())
                .filter(|e| e.field == f)
                .map(|e| e.display())
        };
        let turn = euler(local.rotation);
        let rows: [(&'static str, Field, f32, f32); 6] = [
            ("Joint turn X°", Field::JointTurn(0), turn[0], 5.),
            ("Joint turn Y°", Field::JointTurn(1), turn[1], 5.),
            ("Joint turn Z°", Field::JointTurn(2), turn[2], 5.),
            (
                "Joint move X",
                Field::JointMove(0),
                local.translation[0],
                0.01,
            ),
            (
                "Joint move Y",
                Field::JointMove(1),
                local.translation[1],
                0.01,
            ),
            (
                "Joint move Z",
                Field::JointMove(2),
                local.translation[2],
                0.01,
            ),
        ];
        for (label, field, value, step) in rows {
            out = out.child(number_field(
                label,
                value,
                step,
                typing(m, field),
                cx,
                move |this, action, _, cx| {
                    let Some(m) = &mut this.model else { return };
                    let Some(mut local) = m.joint_local() else {
                        return;
                    };
                    match action {
                        NumberAction::Step(by) => {
                            match field {
                                Field::JointTurn(a) => {
                                    let mut e = euler(local.rotation);
                                    e[a] += by;
                                    local.rotation = quaternion(e);
                                }
                                Field::JointMove(a) => local.translation[a] += by,
                                _ => {}
                            }
                            if let Err(e) = m.key_joint(local, true) {
                                m.message = Some((format!("Not keyed: {e:#}"), true));
                            }
                        }
                        NumberAction::Edit => {
                            m.typing =
                                Some(TextEdit::new(field, value.to_string(), Filter::Number));
                        }
                    }
                    this.model_edited(cx);
                },
            ));
        }
        let (part, animation) = (k.part, k.animation);
        let mut row = div().flex().flex_wrap().gap_1().child(
            widgets::action("Key here", Tone::Secondary)
                .tooltip(Tooltip::text(
                    "Key here",
                    "",
                    "Holds the joint's pose at the playhead",
                ))
                .on_click(cx.listener(|this, _, _, cx| {
                    if let Some(m) = &mut this.model
                        && let Some(local) = m.joint_local()
                        && let Err(e) = m.key_joint(local, true)
                    {
                        m.message = Some((format!("Not keyed: {e:#}"), true));
                    }
                    this.model_edited(cx);
                })),
        );
        if keyed {
            row = row.child(
                widgets::action("Delete key", Tone::Secondary).on_click(cx.listener(
                    |this, _, _, cx| {
                        if let Some(m) = &mut this.model
                            && let Err(e) = m.remove_joint_key()
                        {
                            m.message = Some((format!("Key not removed: {e:#}"), true));
                        }
                        this.model_edited(cx);
                    },
                )),
            );
        }
        row = row.child(
            widgets::action("Make a copy for this model", Tone::Secondary)
                .tooltip(Tooltip::text(
                    "Copy the animation",
                    "",
                    "Edits change the animation wherever the game plays it; a copy beside this cfg changes it here only",
                ))
                .on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(m) = &mut this.model {
                        match m.copy_animation(part, animation) {
                            Ok(to) => m.message = Some((format!("This model now plays {to}"), false)),
                            Err(e) => m.message = Some((format!("Not copied: {e:#}"), true)),
                        }
                    }
                    this.model_edited(cx);
                })),
        );
        Some(out.child(row))
    }

    fn model_effect_change(&mut self, entry: Id, change: Change, cx: &mut Context<Self>) {
        if let Some(m) = &mut self.model
            && let Some(path) = effect_path(m, entry)
            && let Err(e) = m.change_effect(&path, change)
        {
            m.message = Some((format!("Effect not changed: {e:#}"), true));
        }
        self.model_edited(cx);
    }

    fn model_scroll(&self, body: Div) -> AnyElement {
        div()
            .size_full()
            .bg(theme::get().panel)
            .child(
                widgets::vscroll(
                    "model-inspector",
                    &self.scrolls.get("model-inspector"),
                    body,
                )
                .size_full(),
            )
            .into_any_element()
    }

    /// Typing into the inspector: Enter applies, Esc drops.
    pub(crate) fn model_typing_key(
        &mut self,
        event: &KeyDownEvent,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(m) = &mut self.model else {
            return false;
        };
        let Some(t) = &mut m.typing else { return false };
        match t.key_down_in(event, cx) {
            EditEvent::Commit => {
                let (field, text) = (t.field, t.text.clone());
                m.typing = None;
                match field {
                    Field::Value(leaf) => m.set_value(leaf, &text),
                    Field::Effect(k) => {
                        if let (Some(entry), Ok(v)) = (m.selected, text.trim().parse::<f32>())
                            && let Err(e) = effect_number(m, entry, k, v)
                        {
                            m.message = Some((format!("Effect not changed: {e:#}"), true));
                        }
                    }
                    Field::JointTurn(_) | Field::JointMove(_) => {
                        let local = m.joint_local();
                        if let (Some(mut local), Ok(v)) = (local, text.trim().parse::<f32>()) {
                            match field {
                                Field::JointTurn(a) => {
                                    let mut e = euler(local.rotation);
                                    e[a] = v;
                                    local.rotation = quaternion(e);
                                }
                                Field::JointMove(a) => local.translation[a] = v,
                                _ => {}
                            }
                            if let Err(e) = m.key_joint(local, true) {
                                m.message = Some((format!("Not keyed: {e:#}"), true));
                            }
                        }
                    }
                    Field::InspectorFilter => m.field_filter = text,
                    Field::NewAnimationLength => {
                        if let Ok(v) = text.trim().parse::<f32>() {
                            m.new_animation_length = v.max(0.1);
                        }
                    }
                    Field::EmitterTexture => {
                        if let Some(entry) = m.selected
                            && let Err(e) = edit_emitter(m, entry, |e| {
                                e.effect.texture = text.trim().to_owned()
                            })
                        {
                            m.message = Some((format!("Effect not changed: {e:#}"), true));
                        }
                    }
                    Field::EffectTexture(k) => {
                        let path = m.selected.and_then(|e| effect_path(m, e));
                        if let Some(path) = path
                            && let Err(e) = m.set_effect_texture(&path, k, &text)
                        {
                            m.message = Some((format!("Effect not changed: {e:#}"), true));
                        }
                    }
                    _ => {
                        let entry = m.selected;
                        let v = text.trim().parse::<f32>();
                        if let (Some(entry), Ok(v)) = (entry, v)
                            && let Some(t) = m.placement(entry)
                        {
                            m.set_placement(entry, with_field(t, field, v), true);
                        }
                    }
                }
                self.model_edited(cx);
                true
            }
            EditEvent::Cancel => {
                m.typing = None;
                cx.notify();
                true
            }
            EditEvent::Changed => {
                // The filter narrows the fields as it is typed.
                if t.field == Field::InspectorFilter {
                    m.field_filter = t.text.clone();
                }
                cx.notify();
                true
            }
            EditEvent::Ignored => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{euler, quaternion};

    #[test]
    fn euler_angles_round_trip() {
        for d in [
            [0., 0., 0.],
            [10., 20., 30.],
            [-45., 170., 5.],
            [89., -30., 60.],
        ] {
            let back = euler(quaternion(d));
            for (a, b) in back.iter().zip(d) {
                assert!((a - b).abs() < 1e-3, "{d:?} came back as {back:?}");
            }
        }
    }
}
