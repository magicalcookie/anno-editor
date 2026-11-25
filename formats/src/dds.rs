// DDS headers of the textures the editor writes: an island's tint tiles
// (legacy header, DXT1) and ground material maps (DX10, R16G16_UNORM), and
// its map image (DX10, RGBA8 with mips). Each form is a `Layout`; the header
// fields they share are named once below. Reads check what the readers rely
// on, writes fill every field as shipped files do.

use anyhow::{Result, ensure};

const MAGIC: &[u8; 4] = b"DDS ";

// `DDS_HEADER`, offsets from the file start (after the magic).
const HEADER_SIZE_AT: usize = 4;
const FLAGS_AT: usize = 8;
const HEIGHT_AT: usize = 12;
const WIDTH_AT: usize = 16;
const PITCH_AT: usize = 20;
const DEPTH_AT: usize = 24;
const MIPS_AT: usize = 28;
const FORMAT_SIZE_AT: usize = 76;
const FORMAT_FLAGS_AT: usize = 80;
const FOURCC_AT: usize = 84;
const CAPS_AT: usize = 108;
/// Magic and `DDS_HEADER`: where a legacy file's texels start.
const LEGACY_LEN: usize = 128;

// `DDS_HEADER_DXT10`, right after the legacy header.
const DXGI_FORMAT_AT: usize = LEGACY_LEN;
const DIMENSION_AT: usize = LEGACY_LEN + 4;
const ARRAY_SIZE_AT: usize = LEGACY_LEN + 12;
const DX10_LEN: usize = LEGACY_LEN + 20;

// Values every form stores.
const HEADER_SIZE: u32 = 124;
const FORMAT_SIZE: u32 = 32;
const FORMAT_FOURCC: u32 = 0x4;
const TEXTURE_2D: u32 = 3;

/// One form of DDS file the editor writes.
pub struct Layout {
    /// Header bytes before the texels.
    pub(crate) header_len: usize,
    fourcc: &'static [u8; 4],
    /// The DX10 extension's `DXGI_FORMAT`; `None` for a legacy header.
    dxgi_format: Option<u32>,
    /// `dwFlags`, `dwDepth` and `dwCaps` as shipped files of this form hold
    /// them.
    flags: u32,
    depth: u32,
    caps: u32,
    /// What the pitch field holds for a width and height: bytes per texel
    /// row, per row of 4×4 blocks, or the top level's size.
    pitch: fn(usize, usize) -> usize,
}

impl Layout {
    /// Tint tiles: DXT1 (8 bytes per 4×4 block), one mip, legacy header.
    pub(crate) const DXT1: Layout = Layout {
        header_len: LEGACY_LEN,
        fourcc: b"DXT1",
        dxgi_format: None,
        flags: 0x0008_3007,
        depth: 0,
        caps: 0x0400_1000,
        pitch: |width, _| width / 4 * 8,
    };

    /// Ground material maps: one u32 slot word per texel (R16G16_UNORM).
    pub(crate) const R16G16: Layout = Layout {
        header_len: DX10_LEN,
        fourcc: b"DX10",
        dxgi_format: Some(36),
        flags: 0x0000_300f,
        depth: 1,
        caps: 0x0400_1008,
        pitch: |width, _| width * size_of::<u32>(),
    };

    /// Map images: RGBA8 (DXGI_FORMAT_R8G8B8A8_UNORM) with a mip chain, the
    /// pitch field holding the top level's size.
    pub const RGBA8: Layout = Layout {
        header_len: DX10_LEN,
        fourcc: b"DX10",
        dxgi_format: Some(28),
        flags: 0x000a_1007,
        depth: 0,
        caps: 0x0040_1008,
        pitch: |width, height| width * height * 4,
    };

    /// `[width, height]` of a file in this form, from its header.
    pub(crate) fn size(&self, bytes: &[u8]) -> Result<[usize; 2]> {
        ensure!(
            bytes.len() >= self.header_len && bytes.starts_with(MAGIC),
            "not a DDS"
        );
        let word =
            |at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().expect("four bytes"));
        let fourcc = &bytes[FOURCC_AT..FOURCC_AT + 4];
        ensure!(
            fourcc == self.fourcc,
            "DDS is {}, expected {}",
            String::from_utf8_lossy(fourcc),
            String::from_utf8_lossy(self.fourcc)
        );
        if let Some(format) = self.dxgi_format {
            let stored = word(DXGI_FORMAT_AT);
            ensure!(stored == format, "DDS is DXGI {stored}, expected {format}");
        }
        Ok([word(WIDTH_AT) as usize, word(HEIGHT_AT) as usize])
    }

    /// The header of a `width` × `height` file in this form with `mips`
    /// levels; the levels follow it, largest first.
    pub fn header(&self, width: usize, height: usize, mips: usize) -> Vec<u8> {
        let mut out = vec![0; self.header_len];
        out[..MAGIC.len()].copy_from_slice(MAGIC);
        let mut put = |at: usize, v: u32| out[at..at + 4].copy_from_slice(&v.to_le_bytes());
        put(HEADER_SIZE_AT, HEADER_SIZE);
        put(FLAGS_AT, self.flags);
        put(HEIGHT_AT, height as u32);
        put(WIDTH_AT, width as u32);
        put(PITCH_AT, (self.pitch)(width, height) as u32);
        put(DEPTH_AT, self.depth);
        put(MIPS_AT, mips as u32);
        put(FORMAT_SIZE_AT, FORMAT_SIZE);
        put(FORMAT_FLAGS_AT, FORMAT_FOURCC);
        put(CAPS_AT, self.caps);
        if let Some(format) = self.dxgi_format {
            put(DXGI_FORMAT_AT, format);
            put(DIMENSION_AT, TEXTURE_2D);
            put(ARRAY_SIZE_AT, 1);
        }
        out[FOURCC_AT..FOURCC_AT + 4].copy_from_slice(self.fourcc);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The map image header as the shell spelled it out word by word before
    // it wrote through `Layout::RGBA8` (2026-11-20): a 256² image, 9 mips.
    #[test]
    fn rgba8_writes_the_map_image_header() {
        let mut want = b"DDS ".to_vec();
        let words: [u32; 36] = [
            124,
            0x000a_1007,
            256,
            256,
            256 * 256 * 4,
            0,
            9, //
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0, // reserved
            32,
            4,
            u32::from_le_bytes(*b"DX10"),
            0,
            0,
            0,
            0,
            0, // pixel format
            0x0040_1008,
            0,
            0,
            0,
            0, // caps, reserved
            28,
            3,
            0,
            1,
            0, // DX10: format, 2D, misc, array size, alpha mode
        ];
        want.extend(words.iter().flat_map(|w| w.to_le_bytes()));
        let header = Layout::RGBA8.header(256, 256, 9);
        assert_eq!(header, want);
        assert_eq!(Layout::RGBA8.size(&header).unwrap(), [256, 256]);
    }
}
