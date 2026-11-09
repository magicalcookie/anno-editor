// Decoded RGBA images, block-compressed (BC1-BC5, BC7) images with their mip
// chains, plain DDS containers, and texel sampling.
//
// Game-specific containers (AVTX, DDSC headers, ...) are parsed in the game
// crate, which hands the mip bytes here. Standard DDS is shared by enough games
// to live here.
use anyhow::{Result, bail, ensure};
use std::sync::{Arc, Mutex};

#[derive(Clone, Debug)]
pub struct Texture {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// Block-compressed formats, named after their DXGI equivalents.
/// sRGB variants decode identically; the flag is the container's business.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Block {
    /// DXT1: RGB with optional 1-bit alpha.
    Bc1,
    /// DXT3: explicit 4-bit alpha.
    Bc2,
    /// DXT5: interpolated alpha.
    Bc3,
    /// One channel, decoded to red.
    Bc4,
    /// Two channels, decoded to red and green. Usually a normal map.
    Bc5,
    /// Mode-switched RGBA; most modern color textures.
    Bc7,
}

impl Block {
    pub fn bytes(self) -> usize {
        match self {
            Self::Bc1 | Self::Bc4 => 8,
            _ => 16,
        }
    }

    /// Bytes for one `width` x `height` mip, including partial edge blocks.
    pub fn size(self, width: u32, height: u32) -> usize {
        width.div_ceil(4) as usize * height.div_ceil(4) as usize * self.bytes()
    }
}

impl Texture {
    pub fn solid(rgba: [u8; 4]) -> Self {
        Self {
            width: 1,
            height: 1,
            rgba: rgba.to_vec(),
        }
    }

    pub fn from_rgba(width: u32, height: u32, rgba: Vec<u8>) -> Result<Self> {
        let image = Self {
            width,
            height,
            rgba,
        };
        image.validate()?;
        Ok(image)
    }

    /// Checks the dimensions and pixel buffer before an image crosses into GPU upload.
    pub fn validate(&self) -> Result<()> {
        ensure!(self.width > 0 && self.height > 0, "Texture has no pixels");
        ensure!(
            self.width <= MAX_SIDE && self.height <= MAX_SIDE,
            "Texture {}x{} is too large",
            self.width,
            self.height
        );
        ensure!(
            self.rgba.len() == self.width as usize * self.height as usize * 4,
            "Expected {}x{} RGBA pixels, got {} bytes",
            self.width,
            self.height,
            self.rgba.len()
        );
        Ok(())
    }

    /// Decodes one block-compressed mip. Extra trailing bytes are ignored,
    /// since containers often pack the next mip directly after.
    pub fn decode(format: Block, width: u32, height: u32, data: &[u8]) -> Result<Self> {
        ensure!(width > 0 && height > 0, "Texture has no pixels");
        ensure!(
            width <= MAX_SIDE && height <= MAX_SIDE,
            "Texture {width}x{height} is too large"
        );
        let Some(data) = data.get(..format.size(width, height)) else {
            bail!("Truncated {format:?} texture data for {width}x{height}");
        };
        let mut rgba = vec![0; width as usize * height as usize * 4];
        let blocks_x = width.div_ceil(4) as usize;
        for (index, block) in data.chunks_exact(format.bytes()).enumerate() {
            let pixels = decode_block(block, format);
            let bx = (index % blocks_x) as u32 * 4;
            let by = (index / blocks_x) as u32 * 4;
            for y in 0..4 {
                for x in 0..4 {
                    if bx + x < width && by + y < height {
                        let destination = (((by + y) * width + bx + x) * 4) as usize;
                        rgba[destination..destination + 4]
                            .copy_from_slice(&pixels[(y * 4 + x) as usize]);
                    }
                }
            }
        }
        Ok(Self {
            width,
            height,
            rgba,
        })
    }

    /// An image for GPUI's `img()`. The atlas takes BGRA, so channels swap
    /// here. Release it with `window.drop_image` when it leaves the UI, or the
    /// atlas keeps it.
    /// Each texel repeated `k` times across and down: for a preview shown
    /// at a whole number of device pixels per texel, so the GPU's linear
    /// sampler draws square texels instead of smoothing them.
    pub fn upscaled(&self, k: u32) -> Texture {
        if k <= 1 {
            return self.clone();
        }
        let (w, h, k) = (self.width as usize, self.height as usize, k as usize);
        let mut rgba = Vec::with_capacity(w * k * h * k * 4);
        for row in self.rgba.chunks_exact(w * 4) {
            let wide: Vec<u8> = row
                .chunks_exact(4)
                .flat_map(|texel| std::iter::repeat_n(texel, k).flatten().copied())
                .collect();
            for _ in 0..k {
                rgba.extend_from_slice(&wide);
            }
        }
        Texture {
            width: (w * k) as u32,
            height: (h * k) as u32,
            rgba,
        }
    }

    /// Draws texel edges on a texture [`upscaled`](Self::upscaled) by `k`:
    /// each texel's first row and column mixed toward mid grey by
    /// `strength` (0–1), visible on dark and light texels alike. For close
    /// inspection, where telling one texel from the next is the point; keep
    /// it faint where texels are still small, or it hides alpha edges.
    pub fn texel_grid(mut self, k: u32, strength: f32) -> Texture {
        if k < 2 || strength <= 0. {
            return self;
        }
        let mix = (strength.min(1.) * 256.) as u16;
        let (w, k) = (self.width as usize, k as usize);
        for (y, row) in self.rgba.chunks_exact_mut(w * 4).enumerate() {
            for (x, px) in row.chunks_exact_mut(4).enumerate() {
                if x % k == 0 || y % k == 0 {
                    for c in &mut px[..3] {
                        *c = ((u16::from(*c) * (256 - mix) + 128 * mix) >> 8) as u8;
                    }
                    px[3] = px[3].max((mix / 4) as u8);
                }
            }
        }
        self
    }

    pub fn render_image(&self) -> std::sync::Arc<gpui::RenderImage> {
        let bgra = self
            .rgba
            .as_chunks::<4>()
            .0
            .iter()
            .flat_map(|&[r, g, b, a]| [b, g, r, a])
            .collect();
        let frame = image::RgbaImage::from_raw(self.width, self.height, bgra)
            .expect("validated texture dimensions");
        std::sync::Arc::new(gpui::RenderImage::new(vec![image::Frame::new(frame)]))
    }

    /// Bilinear sample with wrapping. Texel centers sit at half-texel offsets,
    /// matching GPU repeat samplers, so CPU alpha tests agree with the shader.
    pub fn sample(&self, uv: [f32; 2]) -> [u8; 4] {
        bilinear(self.width, self.height, uv, |x, y| {
            let at = (y as usize * self.width as usize + x as usize) * 4;
            self.rgba[at..at + 4].try_into().expect("four channels")
        })
    }
}

/// Bilinear filter with wrapping over `texel(x, y)`, texel centers at half
/// offsets like a GPU repeat sampler.
fn bilinear(width: u32, height: u32, uv: [f32; 2], texel: impl Fn(u32, u32) -> [u8; 4]) -> [u8; 4] {
    let x = uv[0].rem_euclid(1.) * width as f32 - 0.5;
    let y = uv[1].rem_euclid(1.) * height as f32 - 0.5;
    let ix = x.floor() as i32;
    let iy = y.floor() as i32;
    let pixel = |x: i32, y: i32| {
        texel(
            x.rem_euclid(width as i32) as u32,
            y.rem_euclid(height as i32) as u32,
        )
    };
    let a = pixel(ix, iy);
    let b = pixel(ix + 1, iy);
    let c = pixel(ix, iy + 1);
    let d = pixel(ix + 1, iy + 1);
    let u = x - x.floor();
    let v = y - y.floor();
    std::array::from_fn(|i| {
        let top = f32::from(a[i]) * (1. - u) + f32::from(b[i]) * u;
        let bottom = f32::from(c[i]) * (1. - u) + f32::from(d[i]) * u;
        (top * (1. - v) + bottom * v).round() as u8
    })
}

// 16k is the D3D11 limit; anything larger is a corrupt header.
const MAX_SIDE: u32 = 16384;

/// A block-compressed image and its mip chain, kept compressed so the GPU
/// can sample it directly: a quarter to an eighth of RGBA's memory.
///
/// `mips[0]` is `width` x `height`; each later level halves both sides
/// (rounding down, at least 1) and holds exactly `block.size(w, h)` bytes.
/// The chain may stop before 1x1.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Compressed {
    pub block: Block,
    pub width: u32,
    pub height: u32,
    pub mips: Vec<Vec<u8>>,
    /// The container marks the texels sRGB-encoded. The kit's renderer shades
    /// in display space and samples every texture as stored, RGBA included,
    /// so this is informational there.
    pub srgb: bool,
}

impl Compressed {
    pub fn validate(&self) -> Result<()> {
        ensure!(self.width > 0 && self.height > 0, "Texture has no pixels");
        ensure!(
            self.width <= MAX_SIDE && self.height <= MAX_SIDE,
            "Texture {}x{} is too large",
            self.width,
            self.height
        );
        let chain = 32 - self.width.max(self.height).leading_zeros();
        ensure!(
            (1..=chain as usize).contains(&self.mips.len()),
            "{}x{} texture has {} mips, expected 1 to {chain}",
            self.width,
            self.height,
            self.mips.len()
        );
        for (level, data) in self.mips.iter().enumerate() {
            let (w, h) = self.mip_size(level);
            ensure!(
                data.len() == self.block.size(w, h),
                "{:?} mip {level} ({w}x{h}) has {} bytes, expected {}",
                self.block,
                data.len(),
                self.block.size(w, h)
            );
        }
        Ok(())
    }

    pub fn mip_size(&self, level: usize) -> (u32, u32) {
        ((self.width >> level).max(1), (self.height >> level).max(1))
    }

    /// Decodes one level to RGBA.
    pub fn decode(&self, level: usize) -> Result<Texture> {
        let Some(data) = self.mips.get(level) else {
            bail!("Texture has {} mips, not {}", self.mips.len(), level + 1);
        };
        let (w, h) = self.mip_size(level);
        Texture::decode(self.block, w, h, data)
    }

    /// Drops top levels until both sides fit in `max`, keeping at least the
    /// last level. For hosts that cap texture memory per use.
    pub fn shrink(&mut self, max: u32) {
        let drop = (0..self.mips.len() - 1)
            .find(|&level| {
                let (w, h) = self.mip_size(level);
                w <= max && h <= max
            })
            .unwrap_or(self.mips.len() - 1);
        (self.width, self.height) = self.mip_size(drop);
        self.mips.drain(..drop);
    }

    /// Bilinear sample of level 0 with wrapping, like `Texture::sample`. It
    /// decodes only the blocks under the sample, so picking through a
    /// compressed cutout costs no decoded copy.
    pub fn sample(&self, uv: [f32; 2]) -> [u8; 4] {
        let blocks_x = self.width.div_ceil(4) as usize;
        bilinear(self.width, self.height, uv, |x, y| {
            let index = (y / 4) as usize * blocks_x + (x / 4) as usize;
            let bytes = self.block.bytes();
            let block = &self.mips[0][index * bytes..(index + 1) * bytes];
            decode_block(block, self.block)[(y % 4 * 4 + x % 4) as usize]
        })
    }
}

/// A texture as the GPU renderer takes it: decoded RGBA, or block-compressed
/// with mips, kept or `Releasable`. Clones share the pixels.
#[derive(Clone, Debug)]
pub enum Image {
    Rgba(Arc<Texture>),
    Compressed(Arc<Compressed>),
    Releasable(Arc<Releasable>),
}

impl Image {
    pub fn size(&self) -> (u32, u32) {
        match self {
            Self::Rgba(t) => (t.width, t.height),
            Self::Compressed(c) => (c.width, c.height),
            Self::Releasable(r) => (r.width, r.height),
        }
    }

    /// Mip levels present; RGBA images have one.
    pub fn levels(&self) -> usize {
        match self {
            Self::Rgba(_) => 1,
            Self::Compressed(c) => c.mips.len(),
            Self::Releasable(r) => r.levels,
        }
    }

    /// The block format of compressed images.
    pub fn block(&self) -> Option<Block> {
        match self {
            Self::Rgba(_) => None,
            Self::Compressed(c) => Some(c.block),
            Self::Releasable(r) => Some(r.block),
        }
    }

    pub fn validate(&self) -> Result<()> {
        match self {
            Self::Rgba(t) => t.validate(),
            Self::Compressed(c) => c.validate(),
            // Validated when made; reloads must match.
            Self::Releasable(_) => Ok(()),
        }
    }

    /// Bilinear sample of the full-size level with wrapping.
    pub fn sample(&self, uv: [f32; 2]) -> [u8; 4] {
        match self {
            Self::Rgba(t) => t.sample(uv),
            Self::Compressed(c) => c.sample(uv),
            Self::Releasable(r) => r.texels().map_or([0; 4], |c| c.sample(uv)),
        }
    }
}

/// A block-compressed image whose texels a renderer lets go once its GPU
/// copy is in, loading them again through `reload` if it needs them anew
/// (a lost device). Hosts with hundreds of model textures then keep only
/// the GPU copy: 418 MB on the continental island.
pub struct Releasable {
    pub block: Block,
    pub width: u32,
    pub height: u32,
    levels: usize,
    texels: Mutex<Option<Arc<Compressed>>>,
    reload: Box<dyn Fn() -> Option<Compressed> + Send + Sync>,
}

impl Releasable {
    /// `texels` now; `reload` must give the same image again.
    pub fn new(
        texels: Compressed,
        reload: impl Fn() -> Option<Compressed> + Send + Sync + 'static,
    ) -> Result<Self> {
        texels.validate()?;
        Ok(Self {
            block: texels.block,
            width: texels.width,
            height: texels.height,
            levels: texels.mips.len(),
            texels: Mutex::new(Some(Arc::new(texels))),
            reload: Box::new(reload),
        })
    }

    /// The texels, loaded again if released; `None` when the reload fails
    /// or no longer matches.
    pub fn texels(&self) -> Option<Arc<Compressed>> {
        let mut texels = self.texels.lock().unwrap();
        if texels.is_none() {
            *texels = (self.reload)()
                .filter(|c| {
                    (c.block, c.width, c.height, c.mips.len())
                        == (self.block, self.width, self.height, self.levels)
                        && c.validate().is_ok()
                })
                .map(Arc::new);
        }
        texels.clone()
    }

    /// Lets the texels go; the next `texels` reloads them.
    pub fn release(&self) {
        *self.texels.lock().unwrap() = None;
    }

    pub fn is_released(&self) -> bool {
        self.texels.lock().unwrap().is_none()
    }
}

impl std::fmt::Debug for Releasable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Releasable")
            .field("block", &self.block)
            .field("size", &(self.width, self.height))
            .field("levels", &self.levels)
            .field("released", &self.is_released())
            .finish()
    }
}

impl From<Arc<Texture>> for Image {
    fn from(texture: Arc<Texture>) -> Self {
        Self::Rgba(texture)
    }
}

impl From<Texture> for Image {
    fn from(texture: Texture) -> Self {
        Self::Rgba(Arc::new(texture))
    }
}

impl From<Arc<Compressed>> for Image {
    fn from(texture: Arc<Compressed>) -> Self {
        Self::Compressed(texture)
    }
}

impl From<Releasable> for Image {
    fn from(texture: Releasable) -> Self {
        Self::Releasable(Arc::new(texture))
    }
}

impl From<Compressed> for Image {
    fn from(texture: Compressed) -> Self {
        Self::Compressed(Arc::new(texture))
    }
}

/// Pixel layouts a DDS file may carry that this module can decode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Block(Block),
    Rgba8,
    Bgra8,
    /// One byte, shown as gray.
    R8,
    /// Two bytes, shown as red and green.
    Rg8,
    /// Two 16-bit channels, top bytes shown as red and green.
    Rg16,
    /// One 16-bit channel, top byte shown as gray.
    R16,
}

impl Format {
    /// Bytes for one `width` x `height` mip.
    pub fn size(self, width: u32, height: u32) -> usize {
        let texels = width as usize * height as usize;
        match self {
            Self::Block(block) => block.size(width, height),
            Self::Rgba8 | Self::Bgra8 | Self::Rg16 => texels * 4,
            Self::R8 => texels,
            Self::Rg8 | Self::R16 => texels * 2,
        }
    }
}

/// A parsed DDS header: the first surface of the first array slice, with its
/// mip chain. Cube faces and further slices are ignored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Dds {
    pub width: u32,
    pub height: u32,
    pub mips: u32,
    pub format: Format,
    /// DXGI format number when the file has a DX10 header, for display.
    pub dxgi: Option<u32>,
    data: usize,
}

impl Dds {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let u32_at = |at: usize| -> Result<u32> {
            let Some(b) = bytes.get(at..at + 4) else {
                bail!("Truncated DDS header");
            };
            Ok(u32::from_le_bytes(b.try_into().expect("four bytes")))
        };
        ensure!(bytes.starts_with(b"DDS "), "Not a DDS file");
        ensure!(u32_at(4)? == 124, "DDS header size is not 124");
        let height = u32_at(12)?;
        let width = u32_at(16)?;
        let mips = u32_at(28)?.max(1);
        let flags = u32_at(80)?;
        let fourcc = bytes.get(84..88).unwrap_or_default();
        let (format, dxgi, data) = if fourcc == b"DX10" {
            let dxgi = u32_at(128)?;
            // DXGI_FORMAT numbers; typeless/sRGB siblings decode the same.
            let format = match dxgi {
                70..=72 => Format::Block(Block::Bc1),
                73..=75 => Format::Block(Block::Bc2),
                76..=78 => Format::Block(Block::Bc3),
                79..=81 => Format::Block(Block::Bc4),
                82..=84 => Format::Block(Block::Bc5),
                97..=99 => Format::Block(Block::Bc7),
                27..=32 => Format::Rgba8,
                87 | 88 | 90..=93 => Format::Bgra8,
                60..=64 => Format::R8,
                48..=52 => Format::Rg8,
                33..=38 => Format::Rg16,
                53..=59 => Format::R16,
                _ => bail!("Unsupported DXGI format {dxgi}"),
            };
            (format, Some(dxgi), 148)
        } else if flags & 4 != 0 {
            let format = match fourcc {
                b"DXT1" => Block::Bc1,
                b"DXT2" | b"DXT3" => Block::Bc2,
                b"DXT4" | b"DXT5" => Block::Bc3,
                b"ATI1" | b"BC4U" => Block::Bc4,
                b"ATI2" | b"BC5U" => Block::Bc5,
                _ => bail!(
                    "Unsupported DDS FourCC {:?}",
                    String::from_utf8_lossy(fourcc)
                ),
            };
            (Format::Block(format), None, 128)
        } else {
            let bits = u32_at(88)?;
            let red = u32_at(92)?;
            let format = match (bits, red) {
                (32, 0xff) => Format::Rgba8,
                (32, 0xff0000) => Format::Bgra8,
                (8, _) => Format::R8,
                (16, 0xff) => Format::Rg8,
                _ => bail!("Unsupported uncompressed DDS layout ({bits} bits, red mask {red:#x})"),
            };
            (format, None, 128)
        };
        ensure!(width > 0 && height > 0, "DDS has no pixels");
        ensure!(
            width <= MAX_SIDE && height <= MAX_SIDE,
            "DDS {width}x{height} is too large"
        );
        Ok(Self {
            width,
            height,
            mips,
            format,
            dxgi,
            data,
        })
    }

    pub fn mip_size(&self, mip: u32) -> (u32, u32) {
        ((self.width >> mip).max(1), (self.height >> mip).max(1))
    }

    /// The first mip no larger than `max` on either side, for thumbnails and
    /// previews that should not decode a full 4k surface.
    pub fn mip_for(&self, max: u32) -> u32 {
        (0..self.mips)
            .find(|&mip| {
                let (w, h) = self.mip_size(mip);
                w <= max && h <= max
            })
            .unwrap_or(self.mips - 1)
    }

    /// Where `mip` of the first surface starts in the file.
    pub fn mip_offset(&self, mip: u32) -> usize {
        self.data
            + (0..mip)
                .map(|m| {
                    let (w, h) = self.mip_size(m);
                    self.format.size(w, h)
                })
                .sum::<usize>()
    }

    /// The bytes of one mip of the first surface.
    fn mip<'a>(&self, bytes: &'a [u8], mip: u32) -> Result<&'a [u8]> {
        ensure!(
            mip < self.mips,
            "DDS has {} mips, not {}",
            self.mips,
            mip + 1
        );
        let offset = self.mip_offset(mip);
        let (width, height) = self.mip_size(mip);
        let size = self.format.size(width, height);
        let Some(data) = bytes.get(offset..offset + size) else {
            bail!("Truncated DDS mip {mip} ({width}x{height})");
        };
        Ok(data)
    }

    /// The first surface with every mip, still compressed. Mips past the end
    /// of a truncated file are dropped; the top level must be there.
    pub fn compressed(&self, bytes: &[u8]) -> Result<Compressed> {
        self.compressed_from(0, bytes.get(self.data..).unwrap_or_default())
    }

    /// `compressed` from mip `first` down, from `tail`: the file's bytes
    /// from `mip_offset(first)` on. A host can read a large texture's small
    /// mips without its top levels, most of the file.
    pub fn compressed_from(&self, first: u32, tail: &[u8]) -> Result<Compressed> {
        let Format::Block(block) = self.format else {
            bail!("DDS format {:?} is not block-compressed", self.format);
        };
        // Headers sometimes count past 1x1.
        let chain = self
            .mips
            .min(32 - self.width.max(self.height).leading_zeros());
        ensure!(first < chain, "DDS has {chain} mips, not {}", first + 1);
        let base = self.mip_offset(first);
        let mut mips = Vec::new();
        for level in first..chain {
            let at = self.mip_offset(level) - base;
            let (width, height) = self.mip_size(level);
            let Some(data) = tail.get(at..at + block.size(width, height)) else {
                ensure!(
                    level > first,
                    "Truncated DDS mip {level} ({width}x{height})"
                );
                break;
            };
            mips.push(data.to_vec());
        }
        let (width, height) = self.mip_size(first);
        Ok(Compressed {
            block,
            width,
            height,
            mips,
            // DXGI *_UNORM_SRGB; legacy FourCC headers cannot say.
            srgb: matches!(self.dxgi, Some(72 | 75 | 78 | 99)),
        })
    }

    /// The first surface for the GPU: compressed with its mips when it is
    /// block-compressed, otherwise the top level decoded to RGBA.
    pub fn image(&self, bytes: &[u8]) -> Result<Image> {
        Ok(match self.format {
            Format::Block(_) => self.compressed(bytes)?.into(),
            _ => self.decode(bytes, 0)?.into(),
        })
    }

    /// Decodes one mip of the first surface.
    pub fn decode(&self, bytes: &[u8], mip: u32) -> Result<Texture> {
        let data = self.mip(bytes, mip)?;
        let (width, height) = self.mip_size(mip);
        let texels = width as usize * height as usize;
        let rgba = match self.format {
            Format::Block(block) => return Texture::decode(block, width, height, data),
            Format::Rgba8 => data.to_vec(),
            Format::Bgra8 => data
                .as_chunks::<4>()
                .0
                .iter()
                .flat_map(|&[b, g, r, a]| [r, g, b, a])
                .collect(),
            Format::R8 => data.iter().flat_map(|&v| [v, v, v, 255]).collect(),
            Format::Rg8 => data
                .as_chunks::<2>()
                .0
                .iter()
                .flat_map(|&[r, g]| [r, g, 0, 255])
                .collect(),
            Format::Rg16 => data
                .as_chunks::<4>()
                .0
                .iter()
                .flat_map(|&[_, r, _, g]| [r, g, 0, 255])
                .collect(),
            Format::R16 => data
                .as_chunks::<2>()
                .0
                .iter()
                .flat_map(|&[_, v]| [v, v, v, 255])
                .collect(),
        };
        debug_assert_eq!(rgba.len(), texels * 4);
        Texture::from_rgba(width, height, rgba)
    }
}

fn decode_block(block: &[u8], format: Block) -> [[u8; 4]; 16] {
    if format == Block::Bc7 {
        let mut pixels = [[0; 4]; 16];
        bcdec_rs::bc7(block, pixels.as_flattened_mut(), 16);
        return pixels;
    }
    if matches!(format, Block::Bc4 | Block::Bc5) {
        let red = decode_alpha(&block[..8]);
        let green = if format == Block::Bc5 {
            decode_alpha(&block[8..])
        } else {
            [0; 16]
        };
        return std::array::from_fn(|i| [red[i], green[i], 0, 255]);
    }
    let colors = if format == Block::Bc1 {
        block
    } else {
        &block[8..]
    };
    let c0 = u16::from_le_bytes([colors[0], colors[1]]);
    let c1 = u16::from_le_bytes([colors[2], colors[3]]);
    let mut palette = [rgb565(c0), rgb565(c1), [0; 4], [0; 4]];
    // Only BC1 has the three-color mode with transparent black; BC2/BC3
    // always interpolate four opaque colors.
    let four_colors = c0 > c1 || format != Block::Bc1;
    let [a, b, _, _] = palette;
    for (channel, (a, b)) in a.into_iter().zip(b).take(3).enumerate() {
        let a = u16::from(a);
        let b = u16::from(b);
        if four_colors {
            palette[2][channel] = ((2 * a + b) / 3) as u8;
            palette[3][channel] = ((a + 2 * b) / 3) as u8;
        } else {
            palette[2][channel] = ((a + b) / 2) as u8;
        }
    }
    palette[2][3] = 255;
    if four_colors {
        palette[3][3] = 255;
    }
    let indices = u32::from_le_bytes([colors[4], colors[5], colors[6], colors[7]]);
    let mut pixels = std::array::from_fn(|i| palette[((indices >> (i * 2)) & 3) as usize]);
    match format {
        Block::Bc2 => {
            let alpha =
                u64::from_le_bytes(block[..8].try_into().expect("eight-byte BC2 alpha block"));
            for (i, pixel) in pixels.iter_mut().enumerate() {
                pixel[3] = ((alpha >> (i * 4)) & 15) as u8 * 17;
            }
        }
        Block::Bc3 => {
            for (pixel, alpha) in pixels.iter_mut().zip(decode_alpha(&block[..8])) {
                pixel[3] = alpha;
            }
        }
        _ => {}
    }
    pixels
}

fn rgb565(color: u16) -> [u8; 4] {
    let r = ((color >> 11) & 31) as u8;
    let g = ((color >> 5) & 63) as u8;
    let b = (color & 31) as u8;
    [
        (r << 3) | (r >> 2),
        (g << 2) | (g >> 4),
        (b << 3) | (b >> 2),
        255,
    ]
}

/// The BC3 alpha / BC4 channel block: two endpoints and 3-bit indices.
fn decode_alpha(block: &[u8]) -> [u8; 16] {
    let a = u16::from(block[0]);
    let b = u16::from(block[1]);
    let mut values = [block[0], block[1], 0, 0, 0, 0, 0, 255];
    if a > b {
        for i in 1..=6 {
            values[i + 1] = (((7 - i) as u16 * a + i as u16 * b) / 7) as u8;
        }
    } else {
        for i in 1..=4 {
            values[i + 1] = (((5 - i) as u16 * a + i as u16 * b) / 5) as u8;
        }
    }
    let mut packed = [0; 8];
    packed[..6].copy_from_slice(&block[2..8]);
    let indices = u64::from_le_bytes(packed);
    std::array::from_fn(|i| values[((indices >> (i * 3)) & 7) as usize])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upscaling_repeats_each_texel_into_a_square() {
        let t = Texture {
            width: 2,
            height: 1,
            rgba: vec![1, 1, 1, 1, 2, 2, 2, 2],
        };
        let up = t.upscaled(2);
        assert_eq!((up.width, up.height), (4, 2));
        let row: Vec<u8> = up.rgba.chunks_exact(4).map(|p| p[0]).collect();
        assert_eq!(row, [1, 1, 2, 2, 1, 1, 2, 2]);
        assert_eq!(t.upscaled(1).rgba, t.rgba);
    }

    // Expected values carry over from the JC3 world editor's decoder tests.
    #[test]
    fn invalid_raw_images_are_rejected_before_gpu_upload() {
        assert!(Texture::from_rgba(u32::MAX, u32::MAX, vec![]).is_err());
        assert!(Texture::from_rgba(1, 0, vec![]).is_err());
        assert!(Texture::from_rgba(2, 1, vec![255; 4]).is_err());
        assert!(Texture::solid([1, 2, 3, 4]).validate().is_ok());
    }

    #[test]
    fn bc1_colors_transparency_and_clipped_edge_blocks() {
        let image =
            Texture::decode(Block::Bc1, 3, 2, &[0, 248, 224, 7, 0b11100100, 0, 0, 0]).unwrap();
        assert_eq!(image.rgba.len(), 3 * 2 * 4);
        assert_eq!(
            &image.rgba[..12],
            &[255, 0, 0, 255, 0, 255, 0, 255, 170, 85, 0, 255]
        );
        let image =
            Texture::decode(Block::Bc1, 4, 4, &[0, 0, 255, 255, 255, 255, 255, 255]).unwrap();
        assert!(
            image
                .rgba
                .as_chunks::<4>()
                .0
                .iter()
                .all(|p| *p == [0, 0, 0, 0])
        );
    }

    #[test]
    fn bc3_uses_opaque_color_interpolation_and_separate_alpha() {
        let mut block = [0; 16];
        block[0] = 200;
        block[1] = 100;
        block[2] = 2;
        block[10..12].copy_from_slice(&65535_u16.to_le_bytes());
        block[12] = 3;
        let image = Texture::decode(Block::Bc3, 4, 4, &block).unwrap();
        assert_eq!(&image.rgba[..4], &[170, 170, 170, 185]);
    }

    #[test]
    fn decodes_bc2_explicit_alpha_and_bc4_bc5_channels() {
        let mut block = [0; 16];
        block[0] = 0x21;
        block[8..10].copy_from_slice(&0xf800_u16.to_le_bytes());
        let image = Texture::decode(Block::Bc2, 4, 4, &block).unwrap();
        assert_eq!(&image.rgba[..8], &[255, 0, 0, 17, 255, 0, 0, 34]);
        let image = Texture::decode(Block::Bc4, 4, 4, &[255, 0, 7, 0, 0, 0, 0, 0]).unwrap();
        assert_eq!(&image.rgba[..8], &[36, 0, 0, 255, 255, 0, 0, 255]);
        let block = [200, 100, 0, 0, 0, 0, 0, 0, 17, 7, 0, 0, 0, 0, 0, 0];
        let image = Texture::decode(Block::Bc5, 4, 4, &block).unwrap();
        assert_eq!(&image.rgba[..4], &[200, 17, 0, 255]);
    }

    fn dds_header(width: u32, height: u32, mips: u32, dxgi: u32) -> Vec<u8> {
        let mut bytes = vec![0; 148];
        bytes[..4].copy_from_slice(b"DDS ");
        bytes[4..8].copy_from_slice(&124_u32.to_le_bytes());
        bytes[12..16].copy_from_slice(&height.to_le_bytes());
        bytes[16..20].copy_from_slice(&width.to_le_bytes());
        bytes[28..32].copy_from_slice(&mips.to_le_bytes());
        bytes[80..84].copy_from_slice(&4_u32.to_le_bytes());
        bytes[84..88].copy_from_slice(b"DX10");
        bytes[128..132].copy_from_slice(&dxgi.to_le_bytes());
        bytes
    }

    #[test]
    fn dds_walks_the_mip_chain_and_swizzles_bgra() {
        // 2x2 BGRA (DXGI 87) with a 1x1 second mip.
        let mut bytes = dds_header(2, 2, 2, 87);
        bytes.extend([10, 20, 30, 40].repeat(4));
        bytes.extend([1, 2, 3, 4]);
        let dds = Dds::parse(&bytes).unwrap();
        assert_eq!(dds.format, Format::Bgra8);
        assert_eq!(dds.mip_for(1), 1);
        assert_eq!(dds.decode(&bytes, 0).unwrap().rgba[..4], [30, 20, 10, 40]);
        let small = dds.decode(&bytes, 1).unwrap();
        assert_eq!((small.width, small.rgba.as_slice()), (1, &[3, 2, 1, 4][..]));
        assert!(dds.decode(&bytes[..bytes.len() - 1], 1).is_err());
        assert!(dds.decode(&bytes, 2).is_err());
    }

    #[test]
    fn dds_small_mips_read_from_their_offset_match_the_whole_chain() {
        // 16x16 BC1 (DXGI 71) with five mips, each block numbered by mip.
        let mut bytes = dds_header(16, 16, 5, 71);
        for (mip, side) in [16u32, 8, 4, 2, 1].into_iter().enumerate() {
            let blocks = side.div_ceil(4).pow(2) as usize;
            bytes.extend([mip as u8; 8].repeat(blocks));
        }
        let dds = Dds::parse(&bytes).unwrap();
        let mut whole = dds.compressed(&bytes).unwrap();
        whole.shrink(4);
        let first = dds.mip_for(4);
        let tail = dds
            .compressed_from(first, &bytes[dds.mip_offset(first)..])
            .unwrap();
        assert_eq!((tail.width, tail.height), (4, 4));
        assert_eq!(tail.mips, whole.mips);
        assert_eq!(tail.mips[0], [2; 8]);
        // The first level must be there; later ones may be cut off.
        assert!(
            dds.compressed_from(first, &bytes[bytes.len() - 4..])
                .is_err()
        );
    }

    #[test]
    fn dds_bc7_mode6_solid_block() {
        // Mode 6 (bit 6 set), both endpoints 7-bit 127 + p-bit 1 -> 255 in
        // every channel: a solid white opaque block.
        let mut block = [0_u8; 16];
        block[0] = 0x40;
        // 8 endpoint values of 7 bits each follow the mode bits, then 2 p-bits.
        let mut bits = 0_u128;
        for i in 0..8 {
            bits |= 0x7f << (7 + i * 7);
        }
        bits |= 0b11 << 63;
        block.copy_from_slice(&(bits | 0x40).to_le_bytes());
        let mut bytes = dds_header(4, 4, 1, 98);
        bytes.extend(block);
        let image = Dds::parse(&bytes).unwrap().decode(&bytes, 0).unwrap();
        assert!(
            image.rgba.iter().all(|&v| v == 255),
            "{:?}",
            &image.rgba[..8]
        );
    }

    #[test]
    fn dds_keeps_compressed_mips_and_the_srgb_flag() {
        // 8x4 BC1 sRGB (DXGI 72) claiming five mips: 8x4, 4x2, 2x1, 1x1, and
        // one past the chain that must be ignored. The last real mip is cut short.
        let mut bytes = dds_header(8, 4, 5, 72);
        bytes.extend([1; 16]);
        bytes.extend([2; 8]);
        bytes.extend([3; 8]);
        bytes.extend([4; 4]);
        let dds = Dds::parse(&bytes).unwrap();
        let image = dds.compressed(&bytes).unwrap();
        assert_eq!((image.block, image.srgb), (Block::Bc1, true));
        assert_eq!(image.mips, vec![vec![1; 16], vec![2; 8], vec![3; 8]]);
        image.validate().unwrap();
        assert!(matches!(dds.image(&bytes).unwrap(), Image::Compressed(_)));
        assert!(dds.compressed(&bytes[..150]).is_err());

        let mut rgba = dds_header(1, 1, 1, 28);
        rgba.extend([1, 2, 3, 4]);
        let dds = Dds::parse(&rgba).unwrap();
        assert_eq!(dds.format, Format::Rgba8);
        assert!(matches!(dds.image(&rgba).unwrap(), Image::Rgba(_)));
    }

    #[test]
    fn compressed_images_validate_shrink_and_sample_like_decoded_ones() {
        let block = |l: usize| [0, 248, 224, 7, 0b11100100 ^ l as u8, 0, 0, 0];
        let mut image = Compressed {
            block: Block::Bc1,
            width: 8,
            height: 8,
            mips: (0..4)
                .map(|l| block(l).repeat(Block::Bc1.size(8 >> l, 8 >> l) / 8))
                .collect(),
            srgb: false,
        };
        image.validate().unwrap();
        let decoded = image.decode(0).unwrap();
        for uv in [[0.1, 0.2], [0.5, 0.5], [0.97, 0.03], [-0.3, 1.4]] {
            assert_eq!(image.sample(uv), decoded.sample(uv));
        }
        let mut wrong = image.clone();
        wrong.mips[1].pop();
        assert!(wrong.validate().is_err());
        wrong.mips = vec![vec![0; 32]; 5];
        assert!(wrong.validate().is_err());

        image.shrink(4);
        assert_eq!((image.width, image.mips.len()), (4, 3));
        assert_eq!(image.mips[0], block(1).to_vec());
        image.shrink(0);
        assert_eq!((image.width, image.mips.len()), (1, 1));
    }

    #[test]
    fn rejects_truncated_and_empty_images() {
        assert!(Texture::decode(Block::Bc3, 8, 4, &[0; 16]).is_err());
        assert!(Texture::decode(Block::Bc1, 0, 4, &[0; 8]).is_err());
        assert!(Texture::from_rgba(2, 2, vec![0; 12]).is_err());
    }

    #[test]
    fn sampling_wraps_and_interpolates_texel_centers() {
        let texture = Texture::from_rgba(2, 1, vec![0, 0, 0, 255, 200, 100, 50, 255]).unwrap();
        assert_eq!(texture.sample([0.25, 0.5]), [0, 0, 0, 255]);
        assert_eq!(texture.sample([0.75, 0.5]), [200, 100, 50, 255]);
        assert_eq!(texture.sample([0.5, 0.5]), [100, 50, 25, 255]);
        assert_eq!(texture.sample([1.25, -0.5]), [0, 0, 0, 255]);
        assert_eq!(texture.sample([0., 0.5]), [100, 50, 25, 255]);
    }
}
