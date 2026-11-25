// Behavioural tests of the PHX reader and editor over synthetic data, the
// extracted samples and the installed game; each skips what is absent.
use super::binding::{Bindings, NewBinding, NewSlot, Slot};
use super::*;
use crate::vfs::Install;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

/// The built-in property ids these tests write; the codec treats them as
/// any other id (the shell's `Builtin` names them).
mod builtin {
    pub const ALPHA: u32 = 0x06;
    pub const VISIBLE: u32 = 0x07;
    pub const START: u32 = 0x1f;
    pub const SHOWN: u32 = 0x3c;
}

// Extracted samples and reference tools from a local checkout.
const GENERATED: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../extract/studio/data/ui/studio/generated"
);
const UI_JSON: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../extract/ui_json/data/ui/pwl"
);
const PHX_PY: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../scripts/phx.py");

/// The quest tracker template: clips with keyed tracks.
const QUEST_TRACKER: &str = "005e6a6d-92f9-41f4-9951-029bbe1d6f9a";

fn sample_dir() -> Option<PathBuf> {
    let dir = PathBuf::from(GENERATED);
    if dir.is_dir() {
        Some(dir)
    } else {
        eprintln!("skipping: {GENERATED} missing");
        None
    }
}

fn sample_paths() -> Vec<PathBuf> {
    let Some(dir) = sample_dir() else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    paths.sort();
    paths
}

/// Every sample parsed, with its file name (its guid), in name order.
fn samples() -> impl Iterator<Item = (String, Asset)> {
    sample_paths().into_iter().map(|p| {
        let name = p.file_name().unwrap().to_str().unwrap().to_owned();
        let a =
            Asset::parse(std::fs::read(&p).unwrap()).unwrap_or_else(|e| panic!("{name}: {e:#}"));
        (name, a)
    })
}

// By guid prefix, so the tests can quote the short ids the docs use.
fn sample_path(prefix: &str) -> Option<PathBuf> {
    sample_paths()
        .into_iter()
        .find(|p| p.file_name().unwrap().to_str().unwrap().starts_with(prefix))
}

fn load(prefix: &str) -> Option<Asset> {
    Some(Asset::parse(std::fs::read(sample_path(prefix)?).unwrap()).unwrap())
}

/// The installed Anno 117, or None (the test skips) without ANNO117_DIR.
fn install() -> Option<Install> {
    let Some(dir) = crate::vfs::install_dir("ANNO117_DIR") else {
        eprintln!("skipping: ANNO117_DIR unset");
        return None;
    };
    Some(Install::open(dir, crate::vfs::Install::DATA_FIRST).unwrap())
}

fn install_1800() -> Option<Install> {
    Some(
        Install::open(
            crate::vfs::install_dir("ANNO1800_DIR")?,
            crate::vfs::Install::DATA_FIRST,
        )
        .unwrap(),
    )
}

/// Generated UI file `file` (a scene name or a guid) as the install ships it.
fn open(install: &Install, file: &str) -> Asset {
    Asset::parse(
        install
            .read(format!("data/ui/studio/generated/{file}"))
            .unwrap(),
    )
    .unwrap()
}

/// Each binding as the slots it reads and writes and its converter: what
/// renumbered indices must leave alone.
fn resolved(t: &Bindings) -> Vec<(Vec<Slot>, Vec<Slot>, u32)> {
    let slots = |i: &[usize]| i.iter().map(|&i| t.slots[i].clone()).collect();
    t.bindings
        .iter()
        .map(|b| (slots(&b.sources), slots(&b.targets), b.converter))
        .collect()
}

/// A binding that copies `from` into `to`.
fn assign(from: NewSlot, to: NewSlot) -> NewBinding {
    NewBinding {
        converter: name_crc(b"AssignConverterInstance"),
        sources: vec![from],
        targets: vec![to],
    }
}

/// Built-in property `id` (Visible, Alpha) of `widget`.
fn builtin_prop(widget: usize, id: u32) -> NewSlot {
    NewSlot::Widget {
        widget,
        path: vec![(1, Slot::BUILTIN | id)],
    }
}

/// The spans holding a Vec3 value, with the value.
fn vec3s(a: &Asset) -> impl Iterator<Item = (usize, [f32; 3])> + '_ {
    a.spans()
        .iter()
        .enumerate()
        .filter_map(|(i, span)| match span.record {
            Record::Value(Value::Vec3(v)) => Some((i, v)),
            _ => None,
        })
}

/// `after` is `before` with only the bytes in `range` changed.
fn same_outside(before: &[u8], after: &[u8], range: Range<usize>) {
    assert_eq!(before.len(), after.len());
    for (k, (b, a)) in before.iter().zip(after).enumerate() {
        assert!(range.contains(&k) || b == a, "byte {k:#x}");
    }
}

fn check_cover(a: &Asset) {
    let mut at = 0;
    for s in a.spans() {
        assert_eq!(s.range.start, at, "gap or overlap at {at:#x}");
        assert!(s.range.end > s.range.start, "empty span at {at:#x}");
        at = s.range.end;
    }
    assert_eq!(at, a.bytes().len());
}

/// Every object id of `a` is used once.
fn ids_unique(a: &Asset) {
    let ids: Vec<Guid> = a
        .objects()
        .filter_map(|(_, o)| match o.id {
            ObjectId::Id(g) => Some(g),
            _ => None,
        })
        .collect();
    assert_eq!(
        ids.iter().collect::<HashSet<_>>().len(),
        ids.len(),
        "every id unique"
    );
}

#[test]
fn installed_1800_widget_edits_preserve_other_bytes() {
    let Some(install) = install_1800() else {
        return;
    };
    let path = "data/ui/studio/generated/006956b4-ff57-46d4-b278-0c4dbe18b722";
    let bytes = install.read(path).unwrap();
    let asset = Asset::parse(bytes.clone()).unwrap();
    assert!(
        asset.typed(),
        "1800 widget must use its exact class versions"
    );
    assert_eq!(asset.encode(), bytes);
    assert!(!asset.widgets().is_empty());
    let (span, mut changed) = vec3s(&asset).next().expect("an editable vector");
    changed[0] += 3.;
    let edited = asset
        .apply(&Edit::Value {
            span,
            value: Value::Vec3(changed),
        })
        .unwrap();
    same_outside(
        &bytes,
        edited.asset.bytes(),
        asset.spans()[span].range.clone(),
    );
    assert!(edited.asset.typed());
    assert!(
        matches!(edited.asset.spans()[span].record, Record::Value(Value::Vec3(v)) if v == changed)
    );
}

#[test]
fn installed_1800_phoenix_assets_roundtrip() {
    let Some(install) = install_1800() else {
        return;
    };
    let mut checked = 0;
    let mut typed = 0;
    for id in install.search("/ui/studio/generated/", 100_000) {
        let bytes = install.read_file(id).unwrap();
        if !bytes.starts_with(b"PHX") {
            continue;
        }
        let path = install.file_path(id);
        let asset = Asset::parse(bytes.clone()).unwrap_or_else(|e| panic!("{path}: {e:#}"));
        assert_eq!(asset.encode(), bytes, "{path}");
        check_cover(&asset);
        checked += 1;
        typed += usize::from(asset.typed());
    }
    eprintln!("1800 Phoenix: {checked} exact roundtrips, {typed} fully decoded");
    assert!(checked > 6_000);
    assert!(typed > 6_000);
}

#[test]
fn text_exports_accept_1800_guids() {
    let texts = parse_texts(
        "<TextExport><Texts><Text><GUID>42</GUID><Text>Harbour &amp; trade</Text></Text></Texts></TextExport>",
    );
    assert_eq!(texts.get(&42).map(String::as_str), Some("Harbour & trade"));
}

#[test]
fn guid_text_form() {
    let g = Guid::parse("00214CE2-8775-4F96-99FB-24289941DCD0").unwrap();
    assert_eq!(g.0[..4], [0xe2, 0x4c, 0x21, 0x00]);
    assert_eq!(g.to_string(), "00214ce2-8775-4f96-99fb-24289941dcd0");
    // The compile-time parser agrees with the run-time one.
    assert_eq!(Guid::from_const("00214ce2-8775-4f96-99fb-24289941dcd0"), g);
}

// A ComponentTemplate with one U32 custom property (the shape of the Input
// component), written through the codec's own writers: class table, heap section,
// object headers from the classes' current versions, then each wire prop's
// value in order.
fn synthetic_component() -> Vec<u8> {
    const COMPONENT_TEMPLATE: u32 = name_crc(b"ComponentTemplate");
    // A shipped ComponentTemplate's header: magic, kind letters, then the
    // bytes every shipped file carries and nothing decodes.
    let mut b = Header::MAGIC.to_vec();
    b.extend_from_slice(b"CP\x1c\x09\x01\x09\x01\xce\xf4\xb1\xba\xf0\x1c");
    let entry = |class, instances| TypeEntry {
        class,
        schema: class_info(class).unwrap().hash,
        instances,
    };
    encode_types(
        &[
            entry(COMPONENT_TEMPLATE, 1),
            entry(Class::CUSTOM_PROPERTY, 0),
        ],
        &mut b,
    );
    // The root is the one heap object.
    let root = Allocation {
        kind: AllocKind::OBJECT,
        count: 1,
        class_index: Some(0),
    };
    encode_heap(
        &Heap {
            counts: Vec::new(),
            reserved: 0,
            allocations: vec![root],
        },
        &mut b,
    );
    let guid = Guid::parse("00214ce2-8775-4f96-99fb-24289941dcd0").unwrap();
    let kind = Header::KINDS.iter().find(|k| k.0 == *b"CP").unwrap().1;
    let enum_value = |value| Value::Enum { ty: 0, value };
    new_object(&mut b, ObjectId::Root(guid), COMPONENT_TEMPLATE);
    // AssetType, then CustomProperties: one instance.
    enum_value(u32::from(kind)).encode(&mut b);
    crate::varint::write(&mut b, 1);
    new_object(&mut b, ObjectId::None, Class::CUSTOM_PROPERTY);
    // UniqueId, PropertyType (UInt32), DefaultValue, no ObjectTemplate or
    // EnumAsset, PropertyAccessibility.
    Value::U32(7).encode(&mut b);
    enum_value(2).encode(&mut b);
    DefaultValue::U32(30).encode(&mut b);
    b.extend_from_slice(&[ObjectId::NULL, ObjectId::NULL]);
    enum_value(0).encode(&mut b);
    // PropertyNameCrc and PhoenixDebugName, then two flags.
    Value::U32(crc32(b"FrameRate")).encode(&mut b);
    write_str_payload(&mut b, "FrameRate");
    Value::Bool(false).encode(&mut b);
    Value::Bool(false).encode(&mut b);
    // The root's AllowMultipleInstances and no ParentComponentTemplate.
    Value::Bool(true).encode(&mut b);
    b.push(ObjectId::NULL);
    b
}

// A class count padded to two bytes reads as the same count but would not
// encode back to its bytes, so the asset is refused.
#[test]
fn padded_class_count_is_refused() {
    let b = synthetic_component();
    assert_eq!(
        b[Header::LEN],
        2,
        "the synthetic class table has two classes"
    );
    let mut padded = b.clone();
    padded.splice(Header::LEN..Header::LEN + 1, [0x82, 0x00]);
    let err = format!("{:#}", Asset::parse(padded).unwrap_err());
    assert!(err.contains("class table count"), "{err}");
    Asset::parse(b).unwrap();
}

// The synthetic component round-trips and takes typed edits; garbage and a
// truncated copy of it are refused.
#[test]
fn synthetic_roundtrip_edits_and_garbage() {
    let b = synthetic_component();
    let a = Asset::parse(b.clone()).unwrap();
    check_cover(&a);
    assert_eq!(a.encode(), b);
    assert_eq!(
        a.guid(),
        Guid::parse("00214ce2-8775-4f96-99fb-24289941dcd0")
    );
    assert_eq!(a.properties().len(), 1);
    let p = &a.properties()[0];
    assert_eq!(a.property_name(p), "FrameRate");
    assert!(a.typed());
    assert_eq!((p.uid, p.kind), (7, 0x02));
    assert_eq!(a.property_default(p), &DefaultValue::U32(30));

    let e = a
        .apply(&Edit::Default {
            span: p.default,
            value: DefaultValue::U32(60),
        })
        .unwrap();
    assert_eq!(
        e.asset.property_default(&e.asset.properties()[0]),
        &DefaultValue::U32(60)
    );
    let wrong = Edit::Default {
        span: p.default,
        value: DefaultValue::Bool(1),
    };
    assert!(a.apply(&wrong).is_err());

    let e = a
        .apply(&Edit::Text {
            span: p.name,
            text: "FrameRateHz".into(),
        })
        .unwrap();
    let q = &e.asset.properties()[0];
    assert_eq!(e.asset.property_name(q), "FrameRateHz");
    assert_eq!(e.asset.bytes().len(), b.len() + 2);
    assert_eq!(e.old_crc, Some(crc32(b"FrameRate")));
    assert!(e.crc_refs.is_empty());

    assert!(Asset::parse(b"PHXWI".to_vec()).is_err());
    assert!(Asset::parse(vec![0; 64]).is_err());
    assert!(Asset::parse(b[..60].to_vec()).is_err());
}

// Untrusted input: every truncation and single-byte corruption of a real
// asset either parses (and then roundtrips) or errors; nothing panics.
#[test]
fn corrupt_input_never_panics() {
    let Some(a) = load("03961558") else { return };
    let orig = a.bytes().to_vec();
    for len in 0..orig.len() {
        let _ = Asset::parse(orig[..len].to_vec());
    }
    for i in 0..orig.len() {
        for x in [0x00, 0x01, 0x02, 0x7f, 0x80, 0xff] {
            let mut b = orig.clone();
            b[i] = x;
            if let Ok(a) = Asset::parse(b.clone()) {
                assert_eq!(a.encode(), b, "byte {i:#x} = {x:#x}");
            }
        }
    }
}

// Every extracted sample: byte-exact, rebuilt the same from its records,
// decoded through the class schema, and what the editor reads off it (the
// widget tree, labels and images, stack layouts, bindings, clips) holds
// together.
#[test]
fn every_sample_roundtrips_and_reads_whole() {
    let Some(dir) = sample_dir() else { return };
    // The StackLayout ComponentTemplate.
    const STACK_LAYOUT: &str = "d5481210-2f09-4e19-91f0-f98e3633a6b7";
    let stack_cp = Asset::parse(std::fs::read(dir.join(STACK_LAYOUT)).unwrap()).unwrap();
    let (mut files, mut opaque, mut total) = (0, 0, 0);
    let (mut trees, mut atlases, mut labels, mut texts, mut images) = (0, 0, 0, 0, 0);
    let (mut stacks, mut overridden) = (0, 0);
    let (mut bound, mut indices, mut clips, mut tracks) = (0, 0, 0, 0);
    for (name, a) in samples() {
        files += 1;
        check_cover(&a);
        assert!(a.encode() == a.bytes(), "{name} does not roundtrip");
        // The class counts and heap section follow from the body.
        let rebuilt = a
            .rebuild(&[], &[])
            .unwrap_or_else(|e| panic!("{name}: {e:#}"));
        assert!(rebuilt.bytes() == a.bytes(), "{name}: rebuild differs");
        // Every value of every object is decoded through the class schema.
        assert!(a.typed(), "{name}: fell back to the scan");
        assert_eq!(a.guid(), Guid::parse(&name), "{name}: root guid");
        assert_eq!(
            Some(a.root_class()),
            Header::KINDS
                .iter()
                .find(|k| k.0 == [a.header().0[3], a.header().0[4]])
                .map(|k| k.2),
            "{name}"
        );
        for (i, t) in a.types().iter().enumerate() {
            let allocations = a
                .heap()
                .allocations
                .iter()
                .filter(|tk| tk.class_index == Some(i as u32))
                .count();
            assert_eq!(
                allocations, t.instances as usize,
                "{name}: class {i} instance count"
            );
        }
        opaque += a
            .spans()
            .iter()
            .filter(|s| s.record == Record::Opaque)
            .map(|s| s.range.len())
            .sum::<usize>();
        total += a.bytes().len();
        match a.header().kind_str().as_str() {
            "IM" => {
                let i = image_info(&a).unwrap();
                assert!(!i.path.is_empty() && i.size[0] > 0.0, "{name}");
            }
            "TA" => atlases += 1,
            _ => {}
        }

        // Each WidgetInstance object is in the tree exactly once, and the
        // root's child count matches the heap section.
        let ws = a.widgets();
        if !ws.is_empty() {
            trees += 1;
            let objects = a
                .objects()
                .filter(|(_, o)| is_a(o.class, name_crc(b"WidgetInstance")))
                .count();
            assert_eq!(ws.len(), objects + 1, "{name}");
            let seen: HashSet<usize> = ws.iter().map(|x| x.object).collect();
            assert_eq!(seen.len(), ws.len());
            for (i, x) in ws.iter().enumerate().skip(1) {
                assert!(ws[x.parent.unwrap()].children.contains(&i));
                assert!(x.geometry().is_some());
            }
            assert_eq!(Some(ws[0].children.len() as u32), a.root_children());
        }
        for w in ws.iter() {
            if let Some(l) = w.label() {
                labels += 1;
                assert!(w.image().is_none());
                texts += usize::from(!l.text.is_empty() || !l.localization_id.is_empty());
            }
            if let Some(i) = w.image() {
                images += usize::from(i.asset.is_some_and(|r| r.class == name_crc(b"ImageAsset")));
            }
            // Stack layout components and their overrides: the StackLayout
            // ComponentTemplate in the extract resolves names and defaults.
            for c in a.components(w) {
                if c.template.map(|g| g.to_string()).as_deref() != Some(STACK_LAYOUT) {
                    continue;
                }
                stacks += 1;
                // The template's defaults name the properties; the widget
                // overrides some of them.
                let props = stack_cp.properties();
                let names: Vec<&str> = props.iter().map(|p| stack_cp.property_name(p)).collect();
                assert_eq!(
                    names,
                    [
                        "Orientation",
                        "ChildrenAlignment",
                        "InvertOrder",
                        "SpaceBetweenItems",
                        "GroupMargin"
                    ]
                );
                overridden += usize::from(props.iter().any(|p| c.get(p.uid).is_some()));
            }
        }
        // Shipped files keep a GroupInstance's template in the Groups array
        // of the group holding the instance; the tree edits rely on it.
        for i in 1..ws.len() {
            let Some(g) = a
                .child_group(&ws, i)
                .filter(|_| ws[i].class == name_crc(b"GroupInstance"))
            else {
                continue;
            };
            let holder = a.child_group(&ws, ws[i].parent.unwrap()).unwrap();
            assert_eq!(a.spans()[g].field.unwrap().object, holder, "{name}");
        }

        // Binding slot indices run through PropertyContainers, then the
        // template property containers, then the constants: every index
        // resolves, and no binding writes into a constant.
        let t = a.bindings(&ws);
        bound += usize::from(!t.bindings.is_empty());
        for b in &t.bindings {
            for &i in b.sources.iter().chain(&b.targets) {
                // u32::MAX marks an unused slot (17 of 164,931 in the install).
                if i != u32::MAX as usize {
                    assert!(i < t.slots.len(), "{name}: index {i} of {}", t.slots.len());
                    indices += 1;
                }
            }
            for &i in &b.targets {
                assert!(
                    !matches!(t.slots.get(i), Some(Slot::Constant { .. })),
                    "{name}: binding writes a constant"
                );
            }
        }
        // Every clip track writes a slot of its file's binding table.
        if ws.is_empty() {
            continue;
        }
        for c in a.clips() {
            clips += 1;
            assert!(c.duration() > 0.0 || c.tracks.is_empty(), "{name}");
            for track in &c.tracks {
                tracks += 1;
                let (at, of) = (track.target, t.slots.len());
                assert!(at < of, "{name}: track slot {at} of {of}");
            }
        }
    }
    assert_eq!(files, 733);
    assert_eq!(opaque, 0, "typed decode leaves no opaque bytes");
    eprintln!("733 assets roundtrip; opaque {opaque} of {total} bytes");
    assert_eq!(trees, 609);
    assert_eq!(atlases, 1);
    eprintln!("{labels} labels ({texts} with text), {images} image widgets with an ImageAsset");
    // Most label text is bound at runtime; a third carry a text or loc id.
    assert!(labels > 100 && texts * 3 > labels && images > 1000);
    eprintln!("{stacks} stack layouts, {overridden} with overrides");
    assert!(stacks > 50 && overridden > 0);
    assert!(
        bound > 300 && indices > 10_000,
        "{bound} files, {indices} indices"
    );
    assert!(
        clips > 100 && tracks > clips,
        "{clips} clips, {tracks} tracks"
    );
}

// Runs without game files: shipped builds have only the embedded list.
#[test]
fn builtin_names_embed_the_corpus() {
    let names = Names::builtin();
    assert!(names.map.len() > 22_000, "{} names", names.map.len());
    // Corpus-only names, absent from the class tables.
    for name in ["HeightConfig", "cDisplacementCeil"] {
        assert_eq!(names.get(crc32(name.as_bytes())), Some(name));
    }
}

#[test]
fn summary_counts_kinds() {
    let Some(dir) = sample_dir() else { return };
    let names = Names::builtin();
    let s = summarize(&dir, &names).unwrap();
    assert!(s.failed.is_empty(), "{:?}", s.failed);
    assert_eq!(s.kinds.values().map(|k| k.files).sum::<usize>(), 733);
    assert_eq!(s.kinds["WI"].files, 609);
    assert_eq!(s.kinds["CP"].files, 67);
    for (kind, k) in &s.kinds {
        assert_eq!(k.roundtrip, k.files, "{kind}");
        eprintln!(
            "{kind}: {} files, {} objects ({:.1}% classes named), {} strings, {} refs, \
             {} custom properties, {:.1}% opaque",
            k.files,
            k.objects,
            100.0 * k.classes_named as f64 / k.objects.max(1) as f64,
            k.strings,
            k.references,
            k.properties,
            100.0 * k.opaque_bytes as f64 / k.bytes as f64
        );
    }
}

#[test]
fn names_resolve() {
    let names = Names::builtin();
    assert_eq!(names.get(0xda06_bd6e), Some("ContainerWidgetTemplate"));
    assert_eq!(names.get(Class::COLOR_TYPE), Some("ColorType"));
    let Some(a) = load("00214ce2") else { return };
    // PropertyNameCrc of OnPress resolves through the file's own strings.
    assert_eq!(a.name(&Names::default(), 0x67f6_1060), Some("OnPress"));
    assert_eq!(a.name(&names, a.root_class()), Some("ComponentTemplate"));
}

#[test]
fn widget_nodes_and_cross_refs() {
    let Some(a) = load("03961558") else { return };
    assert_eq!(a.header().kind_str(), "WI");
    assert_eq!(a.root_class(), name_crc(b"ContainerWidgetTemplate"));
    assert_eq!(a.root_children(), Some(1));
    let nodes = a.nodes();
    assert_eq!(nodes.len(), 2);
    assert!(matches!(nodes[0].header.id, ObjectId::Root(_)));
    assert_eq!(nodes[0].header.prop_count, 0x15);
    assert_eq!(nodes[1].header.class, name_crc(b"ContainerWidgetInstance"));
    assert_eq!(*nodes[1].header.mask, [0x7f, 0xff, 0xff, 0xfe]);
    let texts: Vec<&str> = a.strings().map(|(_, s)| s.text.as_str()).collect();
    assert!(texts.contains(&"HintElement"), "{texts:?}");
    // A nested ContainerWidgetTemplate, plus the WidgetTypeTemplate and
    // GameObjectTemplate the widget binds.
    let classes: Vec<u32> = a.cross_refs().assets.iter().map(|r| r.0).collect();
    for c in [
        name_crc(b"ContainerWidgetTemplate"),
        name_crc(b"WidgetTypeTemplate"),
        name_crc(b"GameObjectTemplate"),
    ] {
        assert!(classes.contains(&c), "{c:#x} not in {classes:x?}");
    }
}

// An image asset: its file, size and slice, and the one image it refers to.
#[test]
fn image_asset_info() {
    let Some(a) = load("018af45e") else { return };
    let info = image_info(&a).unwrap();
    assert!(info.path.starts_with("data\\ui\\"), "{}", info.path);
    assert!(info.path.ends_with(".png"));
    assert_eq!(info.size, [544.0, 116.0]);
    assert_eq!(info.slice, [[0.0, 0.0], [1.0, 1.0]]);
    assert_eq!(info.atlas, None);
    assert_eq!(a.cross_refs().images.len(), 1);
}

// Edits on real assets: a crc-prefixed string, a bare image path and a
// color. `apply` checks every other span stays byte-identical; here we also
// check the values and the length bookkeeping.
#[test]
fn edit_string_path_and_color() {
    let mut done = (false, false, false);
    for (_, a) in samples() {
        if !done.0
            && let Some((i, s)) = a.strings().find(|(_, s)| s.with_crc)
        {
            let text = format!("{}_Edited", s.text);
            let old_len = a.spans()[i].range.len();
            let e = a
                .apply(&Edit::Text {
                    span: i,
                    text: text.clone(),
                })
                .unwrap();
            let Record::Str(r) = &e.asset.spans()[i].record else {
                panic!("not a string")
            };
            assert_eq!(r.text, text);
            assert_eq!(e.asset.spans()[i].range.len(), old_len + 7);
            assert_eq!(e.asset.bytes().len(), a.bytes().len() + 7);
            let at = e.asset.spans()[i].range.start;
            assert_eq!(u32_at(e.asset.bytes(), at), Some(crc32(text.as_bytes())));
            done.0 = true;
        }
        if !done.1
            && let Some((i, s)) = a
                .strings()
                .find(|(_, s)| !s.with_crc && is_image_path(&s.text))
        {
            let text = s.text.replace(".png", "_mod.png");
            let e = a
                .apply(&Edit::Text {
                    span: i,
                    text: text.clone(),
                })
                .unwrap();
            assert!(e.asset.cross_refs().images.contains(&text));
            done.1 = true;
        }
        if !done.2
            && let Some(i) = a
                .spans()
                .iter()
                .position(|s| matches!(s.record, Record::Color(_)))
        {
            let e = a
                .apply(&Edit::Color {
                    span: i,
                    rgb: [1, 2, 3],
                })
                .unwrap();
            assert_eq!(e.asset.spans()[i].record, Record::Color([1, 2, 3]));
            same_outside(a.bytes(), e.asset.bytes(), e.asset.spans()[i].range.clone());
            done.2 = true;
        }
    }
    if sample_dir().is_some() {
        assert_eq!(done, (true, true, true));
    }
}

#[test]
fn rejects_unsafe_edits() {
    let Some(a) = load("00214ce2") else { return };
    let (i, _) = a.strings().next().unwrap();
    let retext = |text: &str| Edit::Text {
        span: i,
        text: text.into(),
    };
    assert!(a.apply(&retext("")).is_err());
    assert!(a.apply(&retext("a\u{1}")).is_err());
    assert!(
        a.apply(&Edit::Color {
            span: i,
            rgb: [0; 3]
        })
        .is_err()
    );
    // Events have no default value to edit.
    let p = &a.properties()[0];
    assert_eq!(a.property_default(p), &DefaultValue::None);
    let edit = Edit::Default {
        span: p.default,
        value: DefaultValue::U32(1),
    };
    assert!(a.apply(&edit).is_err());
}

// Custom property instances against the JSON templates they were compiled
// from, matched by the guid in the JSON file name.
#[test]
fn custom_properties_match_json() {
    let Some(dir) = sample_dir() else { return };
    if !Path::new(UI_JSON).is_dir() {
        eprintln!("skipping: {UI_JSON} missing");
        return;
    }
    let mut json_files = Vec::new();
    let mut stack = vec![PathBuf::from(UI_JSON)];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(d).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|x| x == "json") {
                json_files.push(p);
            }
        }
    }
    let (mut assets, mut checked) = (0, 0);
    let mut seen = Vec::new();
    for jp in json_files {
        let stem = jp.file_stem().unwrap().to_str().unwrap().to_owned();
        let guid = stem.rsplit('_').next().unwrap().to_ascii_lowercase();
        let Ok(bytes) = std::fs::read(dir.join(&guid)) else {
            continue;
        };
        let a = Asset::parse(bytes).unwrap();
        let json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&jp).unwrap()).unwrap();
        let obj = &json["PhoenixAssetData"]["Object"];
        let Some(cps) = obj["CustomProperties"].as_array() else {
            continue;
        };
        assets += 1;
        seen.push(guid[..8].to_owned());
        assert_eq!(a.properties().len(), cps.len(), "{stem}: property count");
        for (p, cp) in a.properties().iter().zip(cps) {
            let cp = &cp["Object"];
            let name = a.property_name(p);
            assert_eq!(u64::from(p.uid), cp["UniqueId"].as_u64().unwrap(), "{stem}");
            assert_eq!(name, cp["PropertyName"].as_str().unwrap(), "{stem}");
            let jt = cp["PropertyType"].as_str().unwrap();
            if let Some(t) = property_type_name(p.kind) {
                assert_eq!(
                    t,
                    jt.trim_start_matches("CustomPropertyType_"),
                    "{stem} {name}"
                );
            }
            let dv = &cp["DefaultValue"];
            let v = &dv["Value"];
            let ok = match (a.property_default(p), dv["Type"].as_str().unwrap()) {
                (DefaultValue::Bool(b), "Bool") => (*b != 0) == v.as_bool().unwrap_or(false),
                (DefaultValue::U32(x), "U32") => u64::from(*x) == v.as_u64().unwrap_or(0),
                (DefaultValue::S32(x), "S32") => i64::from(*x) == v.as_i64().unwrap_or(0),
                (DefaultValue::F32(x), "Float") => {
                    f64::from(f32::from_bits(*x)) == v.as_f64().unwrap_or(0.0)
                }
                (DefaultValue::Str(s), "String") => s == v.as_str().unwrap_or(""),
                (DefaultValue::Vec(x), t) if t.starts_with("Vector") => {
                    // JSON omits zero vectors and prints others as "(1.000000, …)".
                    let want: Vec<f32> = match v.as_str() {
                        Some(s) => s
                            .trim_matches(|c| c == '(' || c == ')')
                            .split(',')
                            .map(|f| f.trim().parse().unwrap())
                            .collect(),
                        None => vec![0.0; x.len()],
                    };
                    x.iter().map(|b| f32::from_bits(*b)).collect::<Vec<_>>() == want
                }
                (DefaultValue::None, _) => v.is_null(),
                (DefaultValue::ObjectId(_), "ObjectID") => true,
                (DefaultValue::Object(c), "ColorType") => {
                    // The embedded Color follows the ColorType header.
                    let rgb = a.spans()[p.default..]
                        .iter()
                        .find_map(|s| match s.record {
                            Record::Color(rgb) => Some(rgb),
                            _ => None,
                        })
                        .unwrap();
                    let jc = &v["Object"]["Color"]["Object"];
                    let want = ["R", "G", "B"].map(|k| jc[k].as_u64().unwrap_or(0) as u8);
                    *c == Class::COLOR_TYPE && rgb == want
                }
                _ => false,
            };
            assert!(ok, "{stem} {name}: {:?} vs {dv}", a.property_default(p));
            let enum_guid = cp["EnumAsset"].as_str().and_then(Guid::parse);
            assert_eq!(p.enum_asset.map(|r| r.id), enum_guid, "{stem} {name}");
            if let Some(r) = p.enum_asset {
                assert_eq!(r.class, name_crc(b"EnumAsset"));
            }
            checked += 1;
        }
    }
    eprintln!("{checked} custom properties in {assets} templates match their JSON");
    // The six byte-checked in phx-decode.md section 7 are among them.
    for g in [
        "00214ce2", "16ea67bd", "d5481210", "b973e271", "47602a46", "a91c68a5",
    ] {
        assert!(seen.iter().any(|s| s == g), "{g} not checked");
    }
    assert!(
        assets >= 20 && checked >= 100,
        "only {assets} assets, {checked} properties"
    );
}

// Agreement with scripts/phx.py `tree`: the python node list (root +
// `02 02 <id>` objects) and its string list.
#[test]
fn agrees_with_python_reference() {
    if sample_dir().is_none() || !Path::new(PHX_PY).exists() {
        return;
    }
    if std::process::Command::new("python3")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("skipping: python3 missing");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    for prefix in ["03961558", "00214ce2", "b973e271", "08db6fb5", "018af45e"] {
        let (Some(path), Some(a)) = (sample_path(prefix), load(prefix)) else {
            continue;
        };
        let out = tmp.path().join("out.json");
        let run = std::process::Command::new("python3")
            .args(["-I", PHX_PY, "tree"])
            .arg(&path)
            .arg("-o")
            .arg(&out)
            .output()
            .unwrap();
        assert!(
            run.status.success(),
            "{}",
            String::from_utf8_lossy(&run.stderr)
        );
        let py: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();

        let hex = |v: &serde_json::Value| {
            u32::from_str_radix(v.as_str().unwrap().trim_start_matches("0x"), 16).unwrap()
        };
        // Python does not check node classes against the class table and
        // claims the odd float run (e.g. class 0x66660000 in 08db6fb5);
        // those are its false positives, not our misses.
        let classes: HashSet<u32> = a.types().iter().map(|t| t.class).collect();
        let py_all: Vec<u32> = py["doc"]["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| hex(&n["class_crc"]))
            .collect();
        let py_nodes: Vec<u32> = py_all
            .iter()
            .copied()
            .filter(|c| classes.contains(c))
            .collect();
        assert!(py_all.len() - py_nodes.len() <= 1, "{prefix}: {py_all:x?}");
        let ours: Vec<u32> = a.nodes().iter().map(|n| n.header.class).collect();
        assert_eq!(ours, py_nodes, "{prefix}: node classes");

        // Python's scanner wants a one-byte length and either `00 00`
        // after or 6+ bytes; every string it finds we find too.
        // String-typed custom property defaults are strings too.
        let defaults = a.spans().iter().filter_map(|s| match &s.record {
            Record::Default(DefaultValue::Str(t)) => Some(t.as_str()),
            _ => None,
        });
        let texts: HashSet<&str> = a
            .strings()
            .map(|(_, s)| s.text.as_str())
            .chain(defaults)
            .collect();
        for s in py["doc"]["strings"].as_array().unwrap() {
            let t = s["value"].as_str().unwrap();
            assert!(texts.contains(t), "{prefix}: python string {t:?} missing");
        }
    }
}

// Class and property names hash to the crcs the tables key them by, and
// `name_crc` is the runtime's crc32.
#[test]
fn schema_names_match_their_crcs() {
    for n in ["", "Position", "CustomPropertyInstance", "WidthResizing"] {
        assert_eq!(name_crc(n.as_bytes()), crc32(n.as_bytes()), "{n}");
    }
    for c in tables::registries().flat_map(|r| &r.classes) {
        if let Some(n) = c.name {
            assert_eq!(crc32(n.as_bytes()), c.crc, "{n}");
        }
        for p in c.props {
            if let Some(n) = p.name {
                assert_eq!(crc32(n.as_bytes()), p.name_crc, "{n}");
            }
        }
    }
    let image = wire_props(name_crc(b"ImageWidgetInstance")).unwrap();
    // WidgetInstance's 27 serialized rows, then ImageWidgetInstance's 10.
    assert_eq!(image.len() + 1, 38);
    assert_eq!(image[0].name, Some("Position"));
    assert_eq!(image[27].name, Some("Image"));
    assert!(is_a(
        name_crc(b"ImageWidgetInstance"),
        name_crc(b"WidgetInstance")
    ));
}

// Rows of formats/data/*/classes.tsv as the exes have them, sorted for the
// lookups' binary search.
#[test]
fn embedded_tables_read_known_rows() {
    // Run alone (`cargo test -p anno-formats embedded_tables_read -- --nocapture`)
    // this times the first access, which parses every file.
    let started = std::time::Instant::now();
    let [r117, r1800] = [0, 1].map(|i| tables::registries().nth(i).unwrap());
    eprintln!("tables parsed in {:?}", started.elapsed());

    let sub = class_info(0x072a_cb84).unwrap();
    assert_eq!(sub.name, Some("SubtractConverterInstance"));
    assert_eq!((sub.base, sub.props.len()), (0x0f7f_54d0, 0));
    assert_eq!(
        sub.hash,
        *b"\x3f\x62\x8d\x09\xd3\x03\xd1\x4d\x8e\xc0\xba\xba\x3b\x4a\x20\xcf"
    );
    // An unnamed row of the widget base, every column set.
    let widget = r117.class(name_crc(b"WidgetInstance")).unwrap();
    let anchor = widget.props.iter().find(|p| p.name_crc == 0x5c63_f4bb);
    let anchor = anchor.unwrap();
    assert_eq!(anchor.name, None);
    assert_eq!(
        (anchor.ty, anchor.type_crc, anchor.mode),
        (0x0b00, 0xdc42_28cb, 0x2001)
    );
    assert_eq!(widget.props[0].name, Some("Position"));
    // The old font binding keeps the bool the class dropped (0); the class
    // the exe dropped decodes as its base and has one prop-less old version.
    let font = r117.older.iter().find(|l| l.crc == 0xca32_a806).unwrap();
    let names: Vec<u32> = font.props.iter().map(|p| p.name_crc).collect();
    assert_eq!(
        names,
        [0x70a6_a7ec, 0, 0x9e9f_b14b, 0x1c2c_57ba, 0x619e_4928]
    );
    assert_eq!((font.props[1].ty, font.props[1].mode), (0x0100, 0x2001));
    assert_eq!(font.base, r117.class(0xca32_a806).unwrap().base);
    assert_eq!(r117.removed, [(0xd251_ac32, 0xc346_a1bd)]);
    let dropped = r117.older.iter().find(|l| l.crc == 0xd251_ac32).unwrap();
    assert!(dropped.props.is_empty());
    assert_eq!(dropped.base, 0xc346_a1bd);
    assert_eq!(r117.older.len(), 23);
    // Anno 1800's WidgetInstance is an older version than 117's.
    let old_widget = r1800.class(widget.crc).unwrap();
    assert_ne!(old_widget.hash, widget.hash);
    assert!(r1800.older.is_empty() && r1800.removed.is_empty());
    // A file's class table names the version it was written with, found in
    // whichever registry has it; a class without a hash to go by takes the
    // first registry's.
    // 117's own older rows hold that version too: same hash, same props.
    let named = tables::class_version(widget.crc, &old_widget.hash).unwrap();
    let wire = |c: &Class| -> Vec<(u32, u16)> {
        (c.props.iter().filter(|p| p.serialized()))
            .map(|p| (p.name_crc, p.ty))
            .collect()
    };
    assert_eq!(wire(named), wire(old_widget));
    assert!(std::ptr::eq(tables::class(widget.crc).unwrap(), widget));

    // Every lookup binary-searches these.
    for (i, r) in tables::registries().enumerate() {
        assert!(
            r.classes.windows(2).all(|w| w[0].crc < w[1].crc),
            "registry {i}'s classes"
        );
    }
}

// The star (`8d4a8dec`): three image widgets under the root. A Position
// edit changes only its own bytes, and only to a value of the same type.
#[test]
fn star_widgets_decode_and_move_in_place() {
    let Some(a) = load("8d4a8dec") else { return };
    let w = a.widgets();
    assert_eq!(w.len(), 4);
    assert_eq!(w[0].children, [1, 2, 3]);
    assert_eq!(w[0].children.len() as u32, a.root_children().unwrap());
    assert_eq!(w[1].name.as_deref(), Some("DropShadow"));
    assert_eq!(w[1].parent, Some(0));
    let g = w[1].geometry().unwrap();
    assert_eq!(g.position, [11.0, 11.0, 0.0]);
    assert_eq!(g.pivot, [64.0, 64.0, 0.0]);
    assert_eq!(g.rotation, 0.0);
    assert_eq!(g.scale, [0.265625, 0.265625, 1.0]);
    assert_eq!(g.alpha, 1.0);
    // The stored RGB; geometry() may report a palette colour instead once a
    // palette is registered (another test may have done so).
    assert!(g.tint.is_some());
    let prop = |i: usize, name: &str| w[i].props.iter().find(|p| p.name == Some(name)).unwrap();
    let tint = prop(1, "ColorTint");
    let stored = a.spans()[tint.span..].iter().find_map(|s| match s.record {
        Record::Color(c) => Some(c),
        _ => None,
    });
    assert_eq!(stored, Some([0x21, 0x25, 0x2b]));
    assert!(g.visible && !g.hidden);
    let image = prop(1, "Image");
    assert!(matches!(image.value, Value::Ref(r) if r.class == name_crc(b"ImageAsset")));
    let size = prop(0, "ReferenceSize");
    assert_eq!(size.value, Value::Vec2([1920.0, 1080.0]));

    let pos = prop(1, "Position");
    let e = a
        .apply(&Edit::Value {
            span: pos.span,
            value: Value::Vec3([100.0, -5.5, 0.0]),
        })
        .unwrap();
    same_outside(
        a.bytes(),
        e.asset.bytes(),
        a.spans()[pos.span].range.clone(),
    );
    let g = e.asset.widgets()[1].geometry().unwrap();
    assert_eq!(g.position, [100.0, -5.5, 0.0]);
    // Same type only.
    let wrong = Edit::Value {
        span: pos.span,
        value: Value::F32(1.0),
    };
    assert!(a.apply(&wrong).is_err());
}

// Every asset under data/ui/studio/generated in the installed game: fully
// typed, zero opaque bytes, byte-exact. Slow (4.7k files, ~130 MB), so it
// needs both ANNO117_DIR and ANNO117_SWEEP=1.
#[test]
fn install_sweep() {
    if std::env::var("ANNO117_SWEEP").as_deref() != Ok("1") {
        eprintln!("skipping: set ANNO117_DIR and ANNO117_SWEEP=1");
        return;
    }
    let Some(install) = install() else { return };
    let folder = install.folder("data/ui/studio/generated").unwrap();
    let mut kinds: BTreeMap<String, usize> = BTreeMap::new();
    let mut failed = Vec::new();
    let mut scenes = 0;
    for &f in install.folder_files(folder) {
        let bytes = install.read_file(f).unwrap();
        if !bytes.starts_with(b"PHX") {
            continue;
        }
        let name = install.file_path(f).to_owned();
        match Asset::parse(bytes.clone()) {
            Ok(a) => {
                let opaque = a.spans().iter().any(|s| s.record == Record::Opaque);
                let rebuilt = a.rebuild(&[], &[]).map(|r| r.bytes() == bytes);
                if !a.typed() || opaque || a.encode() != bytes || !matches!(rebuilt, Ok(true)) {
                    failed.push(name);
                    continue;
                }
                *kinds.entry(a.header().kind_str()).or_default() += 1;
                if a.header().kind_str() == "SC" {
                    scenes += 1;
                    assert!(!a.widgets().is_empty(), "{name}: scene without widgets");
                    assert!(a.reference_size().is_some(), "{name}");
                }
            }
            Err(e) => failed.push(format!("{name}: {e:#}")),
        }
    }
    eprintln!("{kinds:?}");
    assert!(
        failed.is_empty(),
        "{} not fully typed: {:?}",
        failed.len(),
        &failed[..failed.len().min(20)]
    );
    assert!(scenes > 150);
}

// A screen: SceneTemplate root, containers pointing at other templates.
#[test]
fn install_scene_widgets() {
    let Some(install) = install() else { return };
    let a = open(&install, "pausemenuscene");
    assert!(a.typed());
    assert_eq!(a.reference_size(), Some([1920.0, 1080.0]));
    let w = a.widgets();
    assert!(w.len() > 1);
    let containers: Vec<Reference> = w.iter().filter_map(|x| x.container_template()).collect();
    assert!(!containers.is_empty());
    // Referenced templates are widget roots in the same folder. Not all
    // exist: `PauseMenu` points at 1d45b9de…, which no archive ships.
    let mut found = 0;
    for r in &containers {
        let Ok(bytes) = install.read(format!("data/ui/studio/generated/{}", r.id)) else {
            continue;
        };
        assert!(
            !Asset::parse(bytes).unwrap().widgets().is_empty(),
            "{}",
            r.id
        );
        found += 1;
    }
    assert!(found > 0);
}

#[test]
fn install_text_styles_and_texts() {
    let Some(install) = install() else { return };
    let folder = install.folder("data/ui/studio/generated").unwrap();
    let mut styles = 0;
    for &f in install.folder_files(folder) {
        let b = install.read_file(f).unwrap();
        if b.starts_with(b"PHXTS") {
            let t = text_style(&Asset::parse(b).unwrap()).unwrap();
            assert!(t.size > 0.0 && t.font.is_some(), "{t:?}");
            styles += 1;
        }
    }
    assert!(styles >= 30);

    let xml = install
        .read("data/base/config/gui/texts_english.xml")
        .unwrap();
    let texts = parse_texts(std::str::from_utf8(&xml).unwrap());
    assert!(texts.len() > 30_000);
    let id = localization_line_id("A01A 9EEC 1DD0 3236").unwrap();
    assert_eq!(id, -6_910_035_941_296_950_730);
    assert_eq!(texts[&id], "Reload required!");
}

// Names of `w`'s subtree in pre-order, `w` first.
fn subtree_names(ws: &[Widget], w: usize) -> Vec<String> {
    let mut out = vec![ws[w].name.clone().unwrap_or_default()];
    for &c in &ws[w].children {
        out.extend(subtree_names(ws, c));
    }
    out
}

fn child_names(ws: &[Widget], w: usize) -> Vec<String> {
    ws[w]
        .children
        .iter()
        .map(|&c| ws[c].name.clone().unwrap_or_default())
        .collect()
}

// The widget in `b`'s list `wb` with the same object id as `w` of `a`.
fn same_widget(a: &Asset, w: &Widget, b: &Asset, wb: &[Widget]) -> Option<usize> {
    let id = |x: &Asset, o: usize| match &x.spans()[o].record {
        Record::Object(h) => h.id,
        _ => ObjectId::None,
    };
    let want = id(a, w.object);
    wb.iter().position(|x| id(b, x.object) == want)
}

// Tree edits on a small template with nested groups and labels (a4f82d26).
#[test]
fn tree_edits_keep_subtrees_order_and_ids() {
    let Some(a) = load("a4f82d26") else { return };
    let ws = a.widgets();
    let is = |i: usize, class: &[u8]| ws[i].class == name_crc(class);
    let g = (1..ws.len())
        .find(|&i| is(i, b"GroupInstance") && ws[i].children.len() >= 2)
        .unwrap();
    let label = (1..ws.len())
        .find(|&i| is(i, b"LabelWidgetInstance"))
        .unwrap();
    let parent = ws[g].parent.unwrap();
    let subtree = subtree_names(&ws, g);

    // Deleting a group removes exactly its subtree; the rest keeps its order,
    // and the result re-parses with consistent class counts and heap section.
    let mut siblings = child_names(&ws, parent);
    siblings.retain(|n| *n != subtree[0]);
    let b = a.remove_widget(&ws, g).unwrap().asset;
    let wb = b.widgets();
    assert_eq!(wb.len(), ws.len() - subtree.len());
    let p = same_widget(&a, &ws[parent], &b, &wb).unwrap();
    assert_eq!(child_names(&wb, p), siblings);
    // Rebuilding again is stable.
    assert!(b.rebuild(&[], &[]).unwrap().bytes() == b.bytes());

    // Duplicating a group next to itself copies its subtree with new ids.
    let at = ws[parent].children.iter().position(|&c| c == g).unwrap();
    let copy = a.widget_copy(&ws, g).unwrap();
    assert!(
        a.insert_widget(&ws, parent, at + 1, &copy, false).is_err(),
        "ids clash"
    );
    let b = a.insert_widget(&ws, parent, at + 1, &copy, true).unwrap();
    let wb = b.widgets();
    assert_eq!(wb.len(), ws.len() + subtree.len());
    let kids = &wb[same_widget(&a, &ws[parent], &b, &wb).unwrap()].children;
    assert_eq!(subtree_names(&wb, kids[at]), subtree);
    assert_eq!(subtree_names(&wb, kids[at + 1]), subtree);
    ids_unique(&b);

    // Moving reorders siblings and reparents into another group.
    let last = *ws[0].children.last().unwrap();
    let b = a.move_widget(&ws, last, 0, 0).unwrap();
    let mut want = child_names(&ws, 0);
    let moved = want.pop().unwrap();
    want.insert(0, moved);
    assert_eq!(child_names(&b.widgets(), 0), want);
    // Into a nested group, as its last child; its subtree comes along.
    let inner = (1..ws.len())
        .find(|&i| is(i, b"GroupInstance") && ws[i].parent != Some(0))
        .unwrap();
    let leaf = (1..ws.len())
        .find(|&i| ws[i].parent == Some(0) && !is(i, b"GroupInstance"))
        .unwrap();
    let c = a.move_widget(&ws, leaf, inner, usize::MAX).unwrap();
    let wc = c.widgets();
    assert_eq!(wc.len(), ws.len());
    let gi = same_widget(&a, &ws[inner], &c, &wc).unwrap();
    assert_eq!(child_names(&wc, gi).last(), ws[leaf].name.as_ref());
    // A group never moves into itself.
    let outer = ws[inner].parent.unwrap();
    assert!(a.move_widget(&ws, outer, inner, 0).is_err());

    // A widget pasted into a file that lacks its classes (any template
    // without labels) adds them.
    let target = samples().map(|(_, t)| t).find(|t| {
        !t.widgets().is_empty()
            && !t
                .types()
                .iter()
                .any(|e| e.class == name_crc(b"LabelWidgetInstance"))
    });
    let Some(t) = target else { return };
    let copy = a.widget_copy(&ws, label).unwrap();
    let tw = t.widgets();
    let b = t.insert_widget(&tw, 0, 0, &copy, true).unwrap();
    let wb = b.widgets();
    assert_eq!(wb.len(), tw.len() + 1);
    assert_eq!(wb[wb[0].children[0]].name, ws[label].name);
}

// Several value edits apply in one round, exactly as one `apply` would.
#[test]
fn batch_apply_many_matches_single_applies() {
    let Some(a) = load("a4f82d26") else { return };
    let mut vectors = vec3s(&a);
    let (s0, mut v0) = vectors.next().expect("an editable vector");
    let (s1, mut v1) = vectors.next().expect("a second editable vector");
    v0[0] += 3.;
    v1[1] += 5.;
    let set = |span, v| Edit::Value {
        span,
        value: Value::Vec3(v),
    };
    let joint = a.apply_many(&[set(s0, v0), set(s1, v1)]).unwrap();
    assert!(joint.asset.typed());
    assert_eq!(joint.asset.encode(), joint.asset.bytes());
    let seq = a.apply(&set(s0, v0)).unwrap().asset;
    let seq = seq.apply(&set(s1, v1)).unwrap().asset;
    assert_eq!(joint.asset.bytes(), seq.bytes());
    assert!(a.apply_many(&[]).is_err());
    assert!(a.apply_many(&[set(s0, v0), set(s0, v1)]).is_err());
}

// ColorTypes that name a palette entry take the palette's colour once the
// palette is registered, also where their embedded RGB says otherwise.
#[test]
fn palette_entries_override_embedded_colours() {
    let Some(palette) = load("c63efd8b") else {
        return;
    };
    let palettes = Arc::new(Palettes::default());
    assert!(
        palettes.register(&palette) > 100,
        "the shared palette has 124 colours"
    );
    let mut checked = 0;
    for (name, a) in samples() {
        a.use_palettes(&palettes);
        for (i, o) in a.objects() {
            if o.class != Class::COLOR_TYPE {
                continue;
            }
            let (Some(Record::Ref(r)), Some(Record::Value(Value::U32(e)))) = (
                a.spans().get(i + 1).map(|s| &s.record),
                a.spans().get(i + 2).map(|s| &s.record),
            ) else {
                continue;
            };
            let Some(want) = palettes.color(r.id, *e) else {
                continue;
            };
            let embedded = a.spans()[i..(i + 8).min(a.spans().len())]
                .iter()
                .find_map(|s| match s.record {
                    Record::Color(c) => Some(c),
                    _ => None,
                });
            if embedded == Some(want) {
                continue;
            }
            // A widget's ColorTint reads through the palette.
            let ws = a.widgets();
            if let Some(w) = ws.iter().find(|w| w.props.iter().any(|p| p.span == i)) {
                assert_eq!(w.geometry().unwrap().tint, Some(want), "{name}");
                checked += 1;
            }
        }
    }
    eprintln!("{checked} palette tints checked");
}

// Needs 1800 (117's side is the samples): both games ship palette
// c63efd8b… with colours of their own. An asset resolves through its own
// install's palettes, and the other install registering its palette
// (another screen laid out in the same process) leaves it alone.
#[test]
fn each_install_resolves_its_own_palette() {
    let (Some(palette), Some(i1800)) = (load("c63efd8b"), install_1800()) else {
        return;
    };
    let g = palette.guid().unwrap();
    let theirs = ["data/ui/studio/generated", "data/ui/studio/images"]
        .iter()
        .find_map(|d| i1800.read(format!("{d}/{g}")).ok())
        .map(|b| Asset::parse(b).unwrap())
        .expect("1800's palette");
    let [p117, p1800] = [&palette, &theirs].map(|p| {
        let palettes = Arc::new(Palettes::default());
        assert!(palettes.register(p) > 0);
        palettes
    });
    // A 117 widget tinted through an entry the two palettes disagree on.
    let (name, tinted, entry) = samples()
        .find_map(|(name, a)| {
            a.objects()
                .filter(|(_, o)| o.class == Class::COLOR_TYPE)
                .find_map(|(i, _)| match a.spans().get(i + 2).map(|s| &s.record) {
                    Some(Record::Value(Value::U32(e)))
                        if p117.color(g, *e).is_some()
                            && p117.color(g, *e) != p1800.color(g, *e) =>
                    {
                        let ws = a.widgets();
                        let w = ws
                            .iter()
                            .position(|w| w.props.iter().any(|p| p.span == i))?;
                        Some((name.clone(), w, *e))
                    }
                    _ => None,
                })
        })
        .expect("a widget tinted through an entry 117 and 1800 colour differently");
    let tint = |a: &Asset| a.widgets()[tinted].geometry().unwrap().tint;
    let ours = load(&name).unwrap();
    ours.use_palettes(&p117);
    assert_eq!(tint(&ours), p117.color(g, entry));
    let other = load(&name).unwrap();
    other.use_palettes(&p1800);
    assert_eq!(tint(&other), p1800.color(g, entry));
    // 1800's palette registered again, as its next layout does.
    p1800.register(&theirs);
    assert_eq!(tint(&ours), p117.color(g, entry), "{name}");
}

// Needs the game: an EnumAsset maps an enum entry to a value per
// ConversionRule (the text styles' font sizes; EnumToValue reads it).
#[test]
fn enum_asset_rules_map_entries_to_values() {
    let Some(install) = install() else { return };
    let a = open(&install, "27940cbf-95c3-43bc-b95f-45b1beea1267");
    assert_eq!(
        a.enum_rule(1_679_288_549, 1_501_221_526),
        Some(DefaultValue::F32(16f32.to_bits()))
    );
    assert_eq!(a.enum_rule(1_679_288_549, 1), None);
}

// Needs the game: the quest tracker's clips. Timeline keys are added and
// removed in place, and a new clip with a new AlphaTint track keeps the
// file's bindings on their slots (src/ui/layout/tests.rs plays them).
#[test]
fn quest_tracker_clips_play_and_are_authored() {
    use super::animation::{NewKey, TrackKind};
    let Some(install) = install() else { return };
    let asset = open(&install, QUEST_TRACKER);
    // A duplicated key shows up as a second key at the same time, a time
    // edit moves it, and removing it again gives back the original bytes.
    let clip = asset
        .clips()
        .into_iter()
        .find(|c| c.tracks.iter().any(|t| !t.keys.is_empty()))
        .expect("a clip with keys");
    let track = clip.tracks.iter().position(|t| !t.keys.is_empty()).unwrap();
    let key = clip.tracks[track].keys[0].clone();
    let keys_of = |a: &Asset| {
        let c = a.clips().into_iter().find(|c| c.name == clip.name);
        c.unwrap().tracks[track].keys.clone()
    };
    let more = asset.duplicate_key(key.span).unwrap();
    let now = keys_of(&more);
    assert_eq!(now.len(), clip.tracks[track].keys.len() + 1);
    assert_eq!(now[1].time, key.time);
    let moved = more
        .apply(&Edit::Value {
            span: now[1].time_span.unwrap(),
            value: Value::F32(key.time + 0.5),
        })
        .unwrap()
        .asset;
    assert_eq!(keys_of(&moved)[1].time, key.time + 0.5);
    let back = more.remove_key(now[1].span).unwrap();
    assert_eq!(back.encode(), asset.encode());

    // A new clip and track: keys land in time order, and every binding still
    // reads and writes what it did.
    let widgets = asset.widgets();
    let before = asset.bindings(&widgets);
    let n = asset.clips().len();
    let a = asset.add_clip("EditorFade", 60, 30).unwrap();
    assert_eq!(a.clips().len(), n + 1);
    let c = &a.clips()[n];
    assert_eq!(
        (c.name.as_str(), c.frame_rate, c.frames),
        ("EditorFade", 60, 30)
    );
    let w = (1..widgets.len())
        .find(|&w| widgets[w].name.is_some())
        .unwrap();
    let key = |time: f32, v: f32| NewKey {
        time,
        ease: 1,
        value: DefaultValue::F32(v.to_bits()),
    };
    let alpha = builtin_prop(w, builtin::ALPHA);
    let a = a
        .add_track(
            &a.widgets(),
            n,
            &alpha,
            TrackKind::Float,
            &[key(0., 1.), key(0.5, 0.)],
        )
        .unwrap();
    let after = a.bindings(&a.widgets());
    let track = &a.clips()[n].tracks[0];
    assert_eq!(
        after.slots[track.target],
        Slot::Widget {
            widget: Some(w),
            path: vec![(1, Slot::BUILTIN | builtin::ALPHA)]
        }
    );
    assert_eq!(resolved(&after), resolved(&before));
    // A key between the two lands between them.
    let a = a.set_key(n, 0, &key(0.25, 0.3)).unwrap();
    let keys = &a.clips()[n].tracks[0].keys;
    assert_eq!(
        keys.iter().map(|k| k.time).collect::<Vec<_>>(),
        [0., 0.25, 0.5]
    );
}

// Needs the game: a binding added to a shipped template keeps every
// existing binding reading and writing the same slots (indices past the
// new containers are renumbered), and removing it gives the template's
// table back (src/ui/layout/tests.rs lays the edits out).
#[test]
fn installed_bindings_add_and_remove() {
    let Some(install) = install() else { return };
    // The HUD icon-text: bindings over constants and template properties.
    let asset = open(&install, "ae3fe300-1235-46e6-a9c0-c9e68720b44a");
    let widgets = asset.widgets();
    let before = asset.bindings(&widgets);
    // A drawn widget with no Visible binding of its own.
    let w = (1..widgets.len())
        .find(|&w| {
            !before.slots.contains(&Slot::Widget {
                widget: Some(w),
                path: vec![(1, Slot::BUILTIN | builtin::VISIBLE)],
            })
        })
        .unwrap();
    // Constant false → the widget's Visible, plus a template property read
    // (a new template container, which moves every constant's index).
    let off = NewSlot::Constant {
        kind: 0,
        value: DefaultValue::Bool(0),
    };
    let edited = asset
        .add_binding(&widgets, &assign(off, builtin_prop(w, builtin::VISIBLE)))
        .unwrap();
    let ew = edited.widgets();
    let after = edited.bindings(&ew);
    assert_eq!(after.bindings.len(), before.bindings.len() + 1);
    assert_eq!(
        resolved(&after)[..before.bindings.len()],
        resolved(&before)[..],
        "existing bindings keep their slots"
    );
    let added = &resolved(&after)[before.bindings.len()];
    assert_eq!(
        added.0,
        vec![Slot::Constant {
            kind: Some(0),
            value: DefaultValue::Bool(0)
        }]
    );
    // A second one through a new template container renumbers constants.
    let prop = (0..edited.properties().len())
        .find(|&i| !after.slots.contains(&Slot::Template(i, 2)))
        .unwrap();
    let read = NewSlot::Template(prop, 2);
    let twice = edited
        .add_binding(&ew, &assign(read, builtin_prop(w, builtin::ALPHA)))
        .unwrap();
    let tw = twice.widgets();
    let t2 = twice.bindings(&tw);
    assert_eq!(resolved(&t2)[..after.bindings.len()], resolved(&after)[..]);
    // Removing them gives the shipped behaviour back.
    let back = twice.remove_binding(&tw, t2.bindings.len() - 1).unwrap();
    let bw = back.widgets();
    let back = back.remove_binding(&bw, before.bindings.len()).unwrap();
    assert_eq!(resolved(&back.bindings(&back.widgets())), resolved(&before));
}

// Needs the game: the root's Shown, read into a clip's Start for the
// first time, is written as a shipped screen writes it: a container for
// built-in 0x3c through the root's provider (one step, DataMode 1). The
// expected bytes are a shipped Shown container's with its provider and use
// count swapped.
#[test]
fn a_root_shown_source_is_written_as_the_game_writes_it() {
    use super::binding::{NewBinding, NewSlot, Slot};
    let Some(install) = install() else { return };
    let shown = vec![(1, Slot::BUILTIN | builtin::SHOWN)];
    let is_shown = |s: &Slot| matches!(s, Slot::Widget { widget: Some(0), path } if *path == shown);
    let containers =
        |a: &Asset| a.values_of(a.widgets()[0].object, name_crc(b"PropertyContainers"));
    let bytes = |a: &Asset, o: usize| a.bytes[a.subtree_bytes(o)].to_vec();
    // A screen whose root's Shown starts its Default clip.
    let shipped = open(&install, "02901f45-5ee6-455a-9b54-7b1e68126cc2");
    let t = shipped.bindings(&shipped.widgets());
    let at = t.slots.iter().position(is_shown).expect("a shipped Shown");
    let expected = bytes(&shipped, containers(&shipped)[at]);
    // The quest button reads no Shown; its clip Nothing nothing starts.
    let asset = open(&install, "ed33b8d5-9c91-4e74-9ab7-35c85be4491b");
    let before = asset.bindings(&asset.widgets());
    assert!(!before.slots.iter().any(is_shown));
    let nothing = asset
        .clips()
        .iter()
        .position(|c| c.name == "Nothing")
        .unwrap();
    let roots = before
        .slots
        .iter()
        .filter(|s| matches!(s, Slot::Widget { widget: Some(0), path } if path.len() == 1))
        .count();
    assert!(roots > 0, "a root provider to share");
    let edited = asset
        .add_binding(
            &asset.widgets(),
            &NewBinding {
                converter: name_crc(b"AssignConverterInstance"),
                sources: vec![NewSlot::Widget {
                    widget: 0,
                    path: shown.clone(),
                }],
                targets: vec![NewSlot::Clip {
                    clip: nothing,
                    path: vec![(1, Slot::BUILTIN | builtin::START)],
                }],
            },
        )
        .unwrap();
    let after = edited.bindings(&edited.widgets());
    let at = after
        .slots
        .iter()
        .position(is_shown)
        .expect("the new Shown");
    let written = bytes(&edited, containers(&edited)[at]);
    // The provider index and the use count end a container.
    let n = written.len();
    assert_eq!(
        written[..n - 8],
        expected[..expected.len() - 8],
        "the same path and shape"
    );
    assert_eq!(written[n - 4..], 1u32.to_le_bytes(), "one use");
    // The root's provider is shared, not written again.
    let provider = |a: &Asset, s: usize| {
        let c = bytes(a, containers(a)[s]);
        u32::from_le_bytes(c[c.len() - 8..c.len() - 4].try_into().unwrap())
    };
    let root = (0..before.slots.len())
        .find(|&s| {
            matches!(
                before.slots[s],
                Slot::Widget {
                    widget: Some(0),
                    ..
                }
            )
        })
        .unwrap();
    assert_eq!(provider(&edited, at), provider(&asset, root));
    // One new provider: clip Nothing's, which nothing addressed before.
    let providers = |a: &Asset| {
        a.values_of(a.widgets()[0].object, name_crc(b"DataProviderContainers"))
            .len()
    };
    assert_eq!(providers(&edited), providers(&asset) + 1, "only the clip's");
}

// Needs the game: a clip's Start that nothing binds yet gets a provider
// and a container in the shape the game's files write them, byte for
// byte: a file clip through one step (DataMode 2), a clip of a nested
// template through the container, then the clip. The expected bytes are
// the shipped neighbours' with the clip's id (and the action) swapped.
#[test]
fn new_clip_slots_are_written_as_the_game_writes_them() {
    let Some(install) = install() else { return };
    // The quest tracker's button: clips OnMouseEnter, OnMouseLeave and
    // Nothing (nothing bound), and BtnOverlay_DS, whose template has
    // Enter, Leave, ContentAplhaShow and ContentAlphaHide.
    let asset = open(&install, "ed33b8d5-9c91-4e74-9ab7-35c85be4491b");
    let overlay = open(&install, "2d3010a3-807d-4c39-b1a5-488001ac2c1d");
    let widgets = asset.widgets();
    let table = asset.bindings(&widgets);
    let clip = |a: &Asset, name: &str| a.clips().iter().position(|c| c.name == name).unwrap();
    let id = |a: &Asset, name: &str| a.clip_ids()[clip(a, name)].unwrap();
    let nothing = clip(&asset, "Nothing");
    let (enter, nothing_id) = (id(&asset, "OnMouseEnter"), id(&asset, "Nothing"));
    let (leave, show) = (id(&overlay, "Leave"), id(&overlay, "ContentAplhaShow"));
    let w = widgets
        .iter()
        .position(|w| w.name.as_deref() == Some("BtnOverlay_DS"))
        .unwrap();
    assert!(
        !table
            .slots
            .iter()
            .any(|s| matches!(s, Slot::Clip { clip, .. } if *clip == nothing))
    );
    // What the new bindings read: what stops clip OnMouseEnter today.
    let stop = |s: &Slot| matches!(s, Slot::Clip { path, .. } if path[..] == [(1, 0x8000_0020)]);
    let from = table
        .bindings
        .iter()
        .find(|b| b.targets.iter().any(|&t| stop(&table.slots[t])))
        .unwrap()
        .sources[0];
    let start = vec![(1, 0x8000_001f)];
    let add = |a: &Asset, target: NewSlot| {
        a.add_binding(&a.widgets(), &assign(NewSlot::Existing(from), target))
            .unwrap()
    };
    let providers =
        |a: &Asset| a.values_of(a.widgets()[0].object, name_crc(b"DataProviderContainers"));
    let containers =
        |a: &Asset| a.values_of(a.widgets()[0].object, name_crc(b"PropertyContainers"));
    let bytes = |a: &Asset, o: usize| a.bytes[a.subtree_bytes(o)].to_vec();
    let swap = |b: &[u8], from: &[u8], to: &[u8]| {
        let at = b.windows(from.len()).position(|x| x == from).unwrap();
        [&b[..at], to, &b[at + from.len()..]].concat()
    };
    // The shipped provider and Stop container of clip OnMouseEnter, and the
    // shipped nested Start container of BtnOverlay_DS's Leave.
    let shipped_provider = |a: &Asset, target: &Guid| {
        providers(a)
            .into_iter()
            .map(|p| bytes(a, p))
            .find(|b| b.windows(16).any(|x| x == target.0))
            .unwrap()
    };
    let enter_provider = shipped_provider(&asset, &enter);
    let leave_provider = shipped_provider(&asset, &leave);
    let leave_start = |s: &Slot| matches!(s, Slot::Nested { inner, path, .. } if inner[..] == [leave] && path[..] == start[..]);
    let shipped_container = |pick: &dyn Fn(&Slot) -> bool| {
        bytes(
            &asset,
            containers(&asset)[table.slots.iter().position(pick).unwrap()],
        )
    };
    let enter_stop = shipped_container(&stop);
    let leave_start_bytes = shipped_container(&leave_start);
    // The provider index and UseCount end a container.
    let tail = |c: &[u8], provider: usize| {
        [
            &c[..c.len() - 8],
            &(provider as u32).to_le_bytes(),
            &1u32.to_le_bytes(),
        ]
        .concat()
    };
    let (p0, c0) = (providers(&asset).len(), containers(&asset).len());

    let one = add(
        &asset,
        NewSlot::Clip {
            clip: nothing,
            path: start.clone(),
        },
    );
    assert_eq!(providers(&one).len(), p0 + 1);
    assert_eq!(
        bytes(&one, providers(&one)[p0]),
        swap(&enter_provider, &enter.0, &nothing_id.0)
    );
    assert_eq!(containers(&one).len(), c0 + 1);
    let start_bytes = 0x8000_001fu32.to_le_bytes();
    assert_eq!(
        bytes(&one, containers(&one)[c0]),
        tail(
            &swap(&enter_stop, &0x8000_0020u32.to_le_bytes(), &start_bytes),
            p0
        )
    );
    let t1 = one.bindings(&one.widgets());
    assert_eq!(
        t1.slots[c0],
        Slot::Clip {
            clip: nothing,
            path: start.clone()
        }
    );
    assert_eq!(t1.bindings.last().unwrap().targets, vec![c0]);

    let two = add(
        &one,
        NewSlot::NestedClip {
            widget: w,
            clip: show,
            path: start.clone(),
        },
    );
    assert_eq!(
        bytes(&two, providers(&two)[p0 + 1]),
        swap(&leave_provider, &leave.0, &show.0)
    );
    assert_eq!(
        bytes(&two, containers(&two)[c0 + 1]),
        tail(&leave_start_bytes, p0 + 1)
    );
    let t2 = two.bindings(&two.widgets());
    assert_eq!(
        t2.slots[c0 + 1],
        Slot::Nested {
            widget: w,
            inner: vec![show],
            path: start.clone()
        }
    );

    // A place the file has is used once more, not written again.
    let uses = |a: &Asset, c: usize| {
        let span = a.values_of(c, name_crc(b"UseCount"))[0];
        a.spans[span].record.clone()
    };
    let leave_at = t2.slots.iter().position(leave_start).unwrap();
    let before = uses(&two, containers(&two)[leave_at]);
    let three = add(
        &two,
        NewSlot::NestedClip {
            widget: w,
            clip: leave,
            path: start.clone(),
        },
    );
    assert_eq!(containers(&three).len(), c0 + 2);
    assert_eq!(providers(&three).len(), p0 + 2);
    assert_ne!(uses(&three, containers(&three)[leave_at]), before);
    let last = three.bindings(&three.widgets()).bindings.len() - 1;
    let back = three.remove_binding(&three.widgets(), last).unwrap();
    assert_eq!(uses(&back, containers(&back)[leave_at]), before);
}

// Needs the game: binding new properties keeps the existing widget providers.
#[test]
fn installed_binding_edits_share_existing_widget_providers() {
    let Some(install) = install() else { return };
    let mut asset = open(&install, "hudgovernorsbarscene");
    let providers = |a: &Asset| {
        a.values_of(a.widgets()[0].object, name_crc(b"DataProviderContainers"))
            .len()
    };
    let before = providers(&asset);
    let bindings = asset.bindings(&asset.widgets()).bindings.len();
    // The shipped scene already has providers for its root and HUD container.
    // New properties on those same objects must share their provider identity.
    for widget in [0, 1] {
        let half = NewSlot::Constant {
            kind: 3,
            value: DefaultValue::F32(0.5f32.to_bits()),
        };
        let binding = assign(half, builtin_prop(widget, builtin::ALPHA));
        asset = asset.add_binding(&asset.widgets(), &binding).unwrap();
        assert_eq!(
            providers(&asset),
            before,
            "same widget acquired a second provider"
        );
        let after = asset.bindings(&asset.widgets());
        let binding = after.bindings.last().unwrap();
        assert_eq!(
            after.slots[binding.targets[0]],
            Slot::Widget {
                widget: Some(widget),
                path: vec![(1, Slot::BUILTIN | builtin::ALPHA)],
            }
        );
    }
    assert_eq!(
        asset.bindings(&asset.widgets()).bindings.len(),
        bindings + 2
    );
    assert_eq!(
        Asset::parse(asset.encode()).unwrap().encode(),
        asset.encode()
    );
}

// ---------------------------------------------------------------------------
// Directory summary (the `phx.py objects` equivalent), for the test above

#[derive(Debug, Clone, Default)]
struct KindSummary {
    files: usize,
    bytes: usize,
    objects: usize,
    strings: usize,
    references: usize,
    properties: usize,
    opaque_bytes: usize,
    /// Object headers whose class crc resolves to a name.
    classes_named: usize,
    /// Files whose encode is byte-identical to the input.
    roundtrip: usize,
}

#[derive(Debug, Clone, Default)]
struct Summary {
    /// Keyed by the two kind letters, e.g. `WI`.
    kinds: BTreeMap<String, KindSummary>,
    failed: Vec<(PathBuf, String)>,
}

/// Parses every `PHX*` file directly in `dir`; other files are skipped.
fn summarize(dir: &Path, names: &Names) -> Result<Summary> {
    let mut summary = Summary::default();
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_file())
        .collect();
    paths.sort();
    for path in paths {
        let bytes = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        if !bytes.starts_with(b"PHX") {
            continue;
        }
        let asset = match Asset::parse(bytes) {
            Ok(a) => a,
            Err(e) => {
                summary.failed.push((path, format!("{e:#}")));
                continue;
            }
        };
        let k = summary.kinds.entry(asset.header.kind_str()).or_default();
        k.files += 1;
        k.bytes += asset.bytes.len();
        for (_, o) in asset.objects() {
            k.objects += 1;
            k.classes_named += usize::from(asset.name(names, o.class).is_some());
        }
        k.strings += asset.strings().count();
        k.references += asset.references().count();
        k.properties += asset.properties.len();
        k.opaque_bytes += asset
            .spans
            .iter()
            .filter(|s| s.record == Record::Opaque)
            .map(|s| s.range.len())
            .sum::<usize>();
        k.roundtrip += usize::from(asset.encode() == asset.bytes);
    }
    Ok(summary)
}
