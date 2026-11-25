// The u32 LEB128 varint ADF and Phoenix files store their lengths and counts
// in. Both formats read canonical encodings only: a padded form (`85 00` for
// 5) or a fifth byte past u32 is refused, so every value read re-encodes to
// its own bytes and both codecs stay byte-exact. No shipped file of either
// pads one.

/// The value at `p` and the position after it; `None` when the bytes end
/// first or the encoding is padded or overflows u32.
pub(crate) fn read(b: &[u8], mut p: usize) -> Option<(u32, usize)> {
    let mut v: u32 = 0;
    for shift in (0..u32::BITS).step_by(7) {
        let x = *b.get(p)?;
        p += 1;
        // The fifth byte holds the top four bits and ends the value.
        if shift == 28 && x > 0x0f {
            return None;
        }
        v |= u32::from(x & 0x7f) << shift;
        if x < 0x80 {
            // A zero final byte after the first only pads.
            return (shift == 0 || x != 0).then_some((v, p));
        }
    }
    None
}

pub(crate) fn write(out: &mut Vec<u8>, mut v: u32) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_roundtrip_in_their_shortest_form() {
        for (v, len) in [
            (0u32, 1),
            (1, 1),
            (127, 1),
            (128, 2),
            (232, 2),
            (16383, 2),
            (16384, 3),
            (u32::MAX, 5),
        ] {
            let mut out = Vec::new();
            write(&mut out, v);
            assert_eq!(out.len(), len, "{v}");
            assert_eq!(read(&out, 0), Some((v, len)));
        }
        assert_eq!(read(&[0xe8, 0x01], 0), Some((232, 2)));
        assert_eq!(read(&[0xff, 0x80, 0x01], 1), Some((128, 3)));
    }

    #[test]
    fn padded_overflowing_and_truncated_forms_are_refused() {
        // 5 padded to two and three bytes; 0 padded to two.
        assert_eq!(read(&[0x85, 0x00], 0), None);
        assert_eq!(read(&[0x85, 0x80, 0x00], 0), None);
        assert_eq!(read(&[0x80, 0x00], 0), None);
        // A fifth byte past the top four bits, or one that continues.
        assert_eq!(read(&[0xff, 0xff, 0xff, 0xff, 0x10], 0), None);
        assert_eq!(read(&[0xff, 0xff, 0xff, 0xff, 0x8f, 0x00], 0), None);
        assert_eq!(read(&[0x80], 0), None);
        assert_eq!(read(&[], 0), None);
    }
}
