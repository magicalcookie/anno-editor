// The kinds of file 117 holds beyond core's generic ones: its islands and
// test-map templates, models and particles, and its compiled UI screens.

use anno_core::files::{CFG, FC, Group, IFO, Kind, RDM, View};

pub static KINDS: &[Kind] = &[
    Kind::ext(
        "a7m",
        "Island",
        Group::Islands,
        &["a7m"],
        Some(View::Island),
    ),
    Kind::ext(
        "a7t",
        "Map template",
        Group::Islands,
        &["a7t", "a7tinfo"],
        Some(View::Map),
    ),
    // The islands' and templates' editor sidecars: XML.
    Kind::ext(
        "a7me",
        "Island settings",
        Group::Islands,
        &["a7me", "a7te"],
        None,
    ),
    CFG,
    RDM,
    FC,
    IFO,
    Kind::ext("rdp", "Particles", Group::Models, &["rdp"], None),
    Kind {
        id: "screen",
        label: "UI screen",
        group: Group::Ui,
        exts: &[],
        folder: Some("/ui/studio/generated/"),
        view: Some(View::Screen),
        decode: None,
        deps: None,
    },
];
