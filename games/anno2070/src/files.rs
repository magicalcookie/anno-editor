// The kinds of file 2070 holds beyond core's generic ones: its models and
// effects, its islands (opened in the island editor) and maps, its many
// text formats, and its GUI masks (`.abm`), which this file decodes.

use anno_core::files::{CFG, FC, Group, IFO, Kind, RDM, View};
use anyhow::{Context, ensure};
use gpui_editor_kit::texture::Texture;

pub static KINDS: &[Kind] = &[
    CFG,
    RDM,
    FC,
    IFO,
    // Effects sit beside the models, as 117's particles do.
    Kind::ext("efc", "Effect", Group::Models, &["efc"], None),
    Kind::ext(
        "isd",
        "Island",
        Group::Islands,
        &["isd"],
        Some(View::Island),
    ),
    // XML with binary values: a world's island layout, opened in the map
    // editor.
    Kind::ext("www", "World", Group::Islands, &["www"], Some(View::Map)),
    Kind {
        id: "abm",
        label: "GUI mask",
        group: Group::Textures,
        exts: &["abm"],
        folder: None,
        view: Some(View::Picture),
        decode: Some(abm),
        deps: None,
    },
    // GUI templates, frames and backgrounds; map templates; AI layouts,
    // sounds and scenario scripts; shaders and their sources.
    Kind::ext(
        "text",
        "Text",
        Group::Text,
        &[
            "tpl", "dlg", "bkg", "tww", "rsd", "snd", "abo", "abl", "seq", "csl", "fxs", "pfx",
            "css", "tcd",
        ],
        None,
    ),
];

/// A 1-bit mask, the GUI's, beside its art: "ABM", a version byte, width
/// and height, then rows of bits, least significant first, each row whole
/// bytes. Set bits draw white on clear.
fn abm(bytes: &[u8]) -> anyhow::Result<(Texture, String)> {
    ensure!(
        bytes.len() >= 12 && bytes.starts_with(b"ABM"),
        "not an ABM mask"
    );
    let w = u32::from_le_bytes(bytes[4..8].try_into()?);
    let h = u32::from_le_bytes(bytes[8..12].try_into()?);
    let row = w.div_ceil(8) as usize;
    ensure!(w > 0 && h > 0 && w <= 8192 && h <= 8192, "ABM size {w}×{h}");
    let bits = (bytes.get(12..12 + row * h as usize))
        .with_context(|| format!("ABM {w}×{h} wants {} bytes", 12 + row * h as usize))?;
    let mut rgba = vec![0u8; (w * h * 4) as usize];
    for y in 0..h as usize {
        for x in 0..w as usize {
            if bits[y * row + x / 8] >> (x % 8) & 1 == 1 {
                rgba[(y * w as usize + x) * 4..][..4].fill(255);
            }
        }
    }
    Ok((Texture::from_rgba(w, h, rgba)?, "1-bit mask".to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    // An ABM mask's bits run least significant first, rows padded to whole
    // bytes: a 10×2 mask with the first and last texel of row 0 set and
    // the second texel of row 1.
    #[test]
    fn abm_masks_draw_set_bits_white() {
        let mut mask = b"ABM\x01\x0a\x00\x00\x00\x02\x00\x00\x00".to_vec();
        mask.extend([0b0000_0001, 0b0000_0010, 0b0000_0010, 0]);
        let (image, facts) = abm(&mask).unwrap();
        assert_eq!(
            (image.width, image.height, facts.as_str()),
            (10, 2, "1-bit mask")
        );
        let set: Vec<usize> = (0..20).filter(|i| image.rgba[i * 4 + 3] == 255).collect();
        assert_eq!(set, [0, 9, 11]);
        assert!(abm(&mask[..14]).is_err());
    }
}
