// Names for the ids the shipped UI files use (PropertyId records) that
// `data/anno117/data_fields.tsv` lacks, written as `data/anno117/named_ids.tsv`:
// cargo run -p anno-formats --example phx_named_ids -- GAME117_DIR GAME1800_DIR > formats/data/anno117/named_ids.tsv
//
// Sources, Anno 117's first:
// - Anno117.exe's id/name rows, `{type ptr, u32 id, 0, name ptr}` or
//   `{type ptr, u32 id, 0, u64 -1, name ptr}`: game objects' events and
//   actions (the scene transition's ShowSingle) and their other fields, which
//   the 0x38-byte property rows `data_fields.tsv` comes from do not cover. On
//   the ids both name, 4357 of 4360 agree.
// - Anno1800.exe, which builds its rows at start-up: `mov [rip+x], id` then
//   `lea rax, [rip+name]`. Files kept from Anno 1800 (the construction bar)
//   use its ids; the two games share one id space (175 of the 188 ids both
//   name agree).
// - The component and effect JSON (`data/ui/pwl/{components,effects}`):
//   UniqueId, PropertyName and the component's name, for paths whose
//   component instance sits in a template that does not ship.
use std::collections::{BTreeMap, BTreeSet};

use anno_formats::vfs::Install;

/// A PE image: bytes and sections as (va, virtual size, file offset, raw size).
struct Pe {
    b: Vec<u8>,
    base: u64,
    secs: Vec<(u64, u64, usize, usize)>,
}

impl Pe {
    fn read(path: &str) -> anyhow::Result<Pe> {
        let b = std::fs::read(path)?;
        let u16_at = |o: usize| u16::from_le_bytes([b[o], b[o + 1]]) as usize;
        let u32_at = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
        let pe = u32_at(0x3c) as usize;
        let opt = pe + 24;
        let base = u64::from_le_bytes(b[opt + 24..opt + 32].try_into().unwrap());
        let table = opt + u16_at(pe + 20);
        let secs = (0..u16_at(pe + 6))
            .map(|i| {
                let s = table + i * 40;
                (
                    base + u64::from(u32_at(s + 12)),
                    u64::from(u32_at(s + 8)),
                    u32_at(s + 20) as usize,
                    u32_at(s + 16) as usize,
                )
            })
            .collect();
        Ok(Pe { b, base, secs })
    }

    fn u32_at(&self, o: usize) -> u32 {
        u32::from_le_bytes(self.b[o..o + 4].try_into().unwrap())
    }

    fn u64_at(&self, o: usize) -> u64 {
        u64::from_le_bytes(self.b[o..o + 8].try_into().unwrap())
    }

    fn offset(&self, va: u64) -> Option<usize> {
        self.secs.iter().find_map(|&(v, size, off, raw)| {
            (va >= v && va < v + size && ((va - v) as usize) < raw).then(|| off + (va - v) as usize)
        })
    }

    fn va(&self, o: usize) -> Option<u64> {
        self.secs
            .iter()
            .find_map(|&(v, _, off, raw)| (o >= off && o < off + raw).then(|| v + (o - off) as u64))
    }

    /// The identifier at `va` (letters, digits, `_`), if one is there.
    fn ident(&self, va: u64) -> Option<String> {
        if va < self.base {
            return None;
        }
        let o = self.offset(va)?;
        let s = &self.b[o..];
        let end = s.iter().take(81).position(|&c| c == 0)?;
        let s = &s[..end];
        (!s.is_empty()
            && s[0].is_ascii_alphabetic()
            && s.iter().all(|c| c.is_ascii_alphanumeric() || *c == b'_'))
        .then(|| String::from_utf8_lossy(s).into_owned())
    }
}

/// Game-object (2) and component (7) ids carry their kind in the top nibble.
fn tagged(id: u32) -> bool {
    matches!(id >> 28, 2 | 7)
}

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let dir117 = args.next().expect("GAME117_DIR");
    let dir1800 = args.next().expect("GAME1800_DIR");
    let install = Install::open(&dir117)?;

    let known: BTreeSet<u32> = include_str!("../data/anno117/data_fields.tsv")
        .lines()
        .filter(|l| !l.starts_with('#'))
        .filter_map(|l| l.split('\t').next()?.strip_prefix("0x"))
        .map(|h| u32::from_str_radix(h, 16).unwrap())
        .collect();

    // PropertyId objects: `02 00`, class 0x6e5a7f0e, 3 fields, mask 0x60,
    // then PropertyField and UniqueId.
    const PROPERTY_ID: [u8; 6] = [0x0e, 0x7f, 0x5a, 0x6e, 0x03, 0x60];
    let mut used = BTreeSet::new();
    for id in install.search("studio/generated/", usize::MAX) {
        let b = install.read(install.file_path(id))?;
        for at in b.windows(14).filter(|w| w[..6] == PROPERTY_ID) {
            let uid = u32::from_le_bytes(at[10..14].try_into().unwrap());
            if tagged(uid) {
                used.insert(uid);
            }
        }
    }

    let mut names: BTreeMap<u32, String> = BTreeMap::new();
    let exe = Pe::read(&format!("{dir117}/Bin/Win64/Anno117.exe"))?;
    let mut rows: BTreeMap<u32, BTreeSet<String>> = BTreeMap::new();
    for &(_, _, off, raw) in &exe.secs {
        let mut o = off + 8;
        while o + 24 <= off + raw {
            let id = exe.u32_at(o);
            if tagged(id) && exe.u32_at(o + 4) == 0 && exe.offset(exe.u64_at(o - 8)).is_some() {
                let at = if exe.u64_at(o + 8) == 0xffff_ffff {
                    exe.u64_at(o + 16)
                } else {
                    exe.u64_at(o + 8)
                };
                if let Some(name) = exe.ident(at) {
                    rows.entry(id).or_default().insert(name);
                }
            }
            o += 8;
        }
    }
    // An id with two names (1 of 5519) is left out.
    for (id, n) in rows {
        if n.len() == 1 && used.contains(&id) && !known.contains(&id) {
            names.insert(id, n.into_iter().next().unwrap());
        }
    }

    let exe = Pe::read(&format!("{dir1800}/Bin/Win64/Anno1800.exe"))?;
    let b = &exe.b;
    for o in 0..b.len().saturating_sub(17) {
        // c7 05 rel32 imm32: mov dword [rip+rel], id; 48 8d 05 rel32: lea rax.
        if b[o] != 0xc7 || b[o + 1] != 0x05 || b[o + 10..o + 13] != [0x48, 0x8d, 0x05] {
            continue;
        }
        let id = exe.u32_at(o + 6);
        if !tagged(id) || !used.contains(&id) || known.contains(&id) || names.contains_key(&id) {
            continue;
        }
        let rel = i64::from(exe.u32_at(o + 13) as i32);
        if let Some(name) = exe
            .va(o + 17)
            .and_then(|va| exe.ident(va.wrapping_add_signed(rel)))
        {
            names.insert(id, name);
        }
    }

    let mut props: BTreeMap<u32, (String, String)> = BTreeMap::new();
    for folder in ["pwl/components/", "pwl/effects/"] {
        for id in install.search(folder, usize::MAX) {
            let path = install.file_path(id);
            if !path.ends_with(".json") {
                continue;
            }
            let json: serde_json::Value = serde_json::from_slice(&install.read(path)?)?;
            // "Tween_A91C68A5-…": the name before the guid.
            let Some(component) = json["PhoenixAssetHeader"]["Name"]
                .as_str()
                .and_then(|n| n.rsplit_once('_'))
                .map(|(n, _)| n.to_owned())
            else {
                continue;
            };
            let mut stack = vec![&json];
            while let Some(v) = stack.pop() {
                match v {
                    serde_json::Value::Object(o) => {
                        if let (Some(uid), Some(name)) = (
                            o.get("UniqueId").and_then(|u| u.as_u64()),
                            o.get("PropertyName").and_then(|n| n.as_str()),
                        ) {
                            let uid = uid as u32;
                            if used.contains(&uid) {
                                props
                                    .entry(uid)
                                    .or_insert((component.clone(), name.to_owned()));
                            }
                        }
                        stack.extend(o.values());
                    }
                    serde_json::Value::Array(a) => stack.extend(a),
                    _ => {}
                }
            }
        }
    }

    // The format formats/src/phx/tables.rs reads.
    println!(
        "# Generated by `cargo run -p anno-formats --example phx_named_ids -- GAME117_DIR GAME1800_DIR > formats/data/anno117/named_ids.tsv`; do not edit."
    );
    println!(
        "# Names for ids the shipped UI files use that data_fields.tsv lacks: Anno117.exe's event and action rows, Anno1800.exe's start-up rows (files kept from Anno 1800 use its ids), and the component and effect JSON (see the example for how each is found)."
    );
    println!("# Tab-separated, kind first, each kind sorted by id.");
    println!("# object\tid\tname");
    println!("#   A game-object event, action or field.");
    println!("# component\tid\tcomponent\tproperty");
    println!("#   A component's or effect's custom property by UniqueId.");
    for (id, name) in &names {
        println!("object\t{id:#010x}\t{name}");
    }
    for (id, (component, name)) in &props {
        println!("component\t{id:#010x}\t{component}\t{name}");
    }
    eprintln!(
        "{} used ids, {} named here, {} component properties",
        used.len(),
        names.len(),
        props.len()
    );
    Ok(())
}
