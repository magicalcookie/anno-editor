// World Machine project templates: writes `.tmd` worlds that build an
// Anno-style island and export what the World Machine link imports (a
// 16-bit "Height" PNG and an RGBA "Splatmap" PNG, both on every build).
//
// A `.tmd` ("TMDFile2") is a tree of records: "WM", a name length byte, the
// name, a u64 payload size and the payload, which is either data or more
// records behind a short prefix (a count, a type tag). Nothing documents
// the devices, so every device here is a copy of one World Machine saved
// (`devices.tmd`: one of each; `default.tmd`: WM's New Project, for the
// outputs and the Texture Weightmap; `empty.tmd`: an empty world), with its
// name, position, id and parameters patched in place. Parameter values keep
// their size, so only curves change a record's length.
use anyhow::{Context as _, Result, bail, ensure};
use std::ops::Range;

const DEVICES: &[u8] = include_bytes!("devices.tmd");
const DEFAULT: &[u8] = include_bytes!("default.tmd");
const EMPTY: &[u8] = include_bytes!("empty.tmd");
/// One of every device the templates use, saved by World Machine through
/// its console (`--cli`: `add <type>`, `save <path>`).
const PROTOS: &[u8] = include_bytes!("protos.tmd");
/// The same, later: adds Chooser, Clamp and current Blur.
const PROTOS2: &[u8] = include_bytes!("protos2.tmd");
/// And the colour devices: Channel Combiner, Constant, Colorizer.
const PROTOS3: &[u8] = include_bytes!("protos3.tmd");
const MAGIC: &[u8] = b"TMDFile2";

/// Heightfield value of sea level in every template, and the metres between
/// 0 and 1: the link settings that turn a template's export into Anno
/// heights (shelf at -3 m, plateaus 8-15 m, peaks 40-60 m).
pub const SEA: f32 = 0.2;
pub const RANGE: f32 = 100.;
/// Scene width in metres: a Large Anno island. The importer resamples to
/// the island's own size, so smaller islands use the same world.
pub const SCENE: f32 = 384.;
/// WM's internal length unit: world coordinates, generator sizes and the
/// render extents are stored in multiples of 8 km.
const UNIT: f32 = 8000.;
/// Centre of the scene in internal units (4 km), where WM's generators
/// such as the radial gradient centre themselves.
const CENTRE: f32 = 0.5;

/// The templates' splatmap channels: five RGBA images of four, each image
/// a theme, the same in every template so a channel always means one
/// thing. Roles follow where retail islands put each material (measured
/// over the roman and celtic pools); the names are
/// palette files to use, best first, roman then celtic. 1-16 are ground
/// channels sharing one weightmap; 17-20 overlay patches (see `world`).
pub const CHANNELS: [(&str, &[&str]); 20] = [
    // 1 rock
    ("cliff", &["cliff_limestone", "celtic_cliffs_05", "cliff"]),
    (
        "sharp cliff",
        &[
            "roman_cliff_sharp",
            "celtic_cliffs_06",
            "cliff_blocky",
            "cliff",
        ],
    ),
    ("coastal rock", &["rock_limestone", "rocks_sharp", "rock"]),
    ("gravel", &["gravel_01", "gravel_round", "gravel"]),
    // 2 shore
    (
        "seagrass",
        &[
            "underwatergrass",
            "mud_01_darker.",
            "mud_01_darker_dummy",
            "mud",
        ],
    ),
    (
        "beach & shelf",
        &["beach_04", "beach_01.", "beach_01_dummy", "beach"],
    ),
    ("waterline", &["beach_02", "beach_01_b", "beach"]),
    ("shore sand", &["sand_01", "gravel_round", "sand", "beach"]),
    // 3 water
    ("river bed", &["river_pebbles", "gravel_round", "gravel"]),
    (
        "river banks",
        &["mud_01.", "mud_01_dummy", "mud_01_darker_wet", "mud"],
    ),
    (
        "river shore",
        &["beach_05", "mud_01_darker.", "mud_01_darker_dummy", "mud"],
    ),
    ("pebble beach", &["beach_03", "gravel_round", "beach"]),
    // 4 ground
    ("dirt", &["dirt_01", "mud_01_darker_wet", "dirt", "mud"]),
    ("dry grass", &["grass_01_h_dry", "grass_01_c_dry", "grass"]),
    (
        "lush grass",
        &["grass_01_f_dark", "grass_01.", "grass_01_dummy", "grass"],
    ),
    (
        "grass",
        &["grass_01_e", "grass_01.", "grass_01_dummy", "grass"],
    ),
    // 5 overlays
    (
        "upland grass",
        &["grass_01_g_dry", "grass_01_c_dry", "grass"],
    ),
    (
        "flowers",
        &["grass_01_d_lavender", "grass_01_b_light", "grass"],
    ),
    ("marsh", &["marshfloor", "mud_01.", "mud_01_dummy", "mud"]),
    (
        "bare earth",
        &["dirt_01", "mud_01_darker.", "mud_01_darker_dummy", "mud"],
    ),
];

/// The theme of each splatmap image, in order.
pub const IMAGES: [&str; 5] = ["rock", "shore", "water", "ground", "overlay"];

/// The five splatmap outputs, named so they sort in channel order.
const SPLATMAPS: [&str; 5] = [
    "Splatmap 1",
    "Splatmap 2",
    "Splatmap 3",
    "Splatmap 4",
    "Splatmap 5",
];

/// The palette material for each channel: the first file name (without
/// its folder) that starts with one of the channel's preferences.
pub fn channel_materials(
    game: anno_formats::game::Game,
    materials: &[(u8, String)],
) -> Vec<[Option<u8>; 4]> {
    let pick = |prefs: &[&str]| {
        prefs.iter().find_map(|want| {
            materials.iter().find_map(|(id, file)| {
                let stem = super::material_name(file);
                stem.starts_with(want).then_some(*id)
            })
        })
    };
    CHANNELS
        .chunks(4)
        .map(|four| {
            std::array::from_fn(|c| {
                pick(four[c].1).or_else(|| {
                    // 1800 uses regional prefixes and a smaller set of ground
                    // roles. Every WM channel still needs usable material weight.
                    if !game.profile().regional_materials {
                        return None;
                    }
                    let fallback: &[&str] = match four[c].0 {
                        "cliff" | "sharp cliff" => &["mountain", "rock", "stone"],
                        "coastal rock" => &["rock", "stone", "mountain"],
                        "gravel" | "river bed" => &["gravle", "gravel", "stone", "beach"],
                        "seagrass" => &["underwater", "mud", "snow", "stone", "rock", "beach"],
                        "beach & shelf" | "waterline" | "shore sand" | "pebble beach" => {
                            &["beach", "snow", "stone"]
                        }
                        "river banks" | "river shore" | "dirt" | "marsh" | "bare earth" => {
                            &["mud", "roots", "snow", "stone", "gras"]
                        }
                        _ => &["grass", "gras", "snow", "mud"],
                    };
                    pick(fallback)
                })
            })
        })
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Template {
    /// Wide buildable plateaus behind a cliff coast, as in Anno's concept
    /// art: the default.
    Plateau,
    /// The same plateau with deep, sheltered inlets for harbours.
    Bay,
    /// A plateau rising to a central mountain.
    Mountain,
    /// Tiered plateaus: inland cliffs step up to higher ground, as on the
    /// rims of Anno's concept island.
    Highlands,
}

impl Template {
    pub const ALL: [Template; 4] = [
        Template::Plateau,
        Template::Bay,
        Template::Mountain,
        Template::Highlands,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Template::Plateau => "Plateau & cliffs",
            Template::Bay => "Bay coast",
            Template::Mountain => "Mountain",
            Template::Highlands => "Highlands",
        }
    }
}

// --- records -----------------------------------------------------------------

/// A record: its name, where its u64 size sits, and its payload.
#[derive(Clone, Debug)]
struct Rec {
    name: String,
    size_at: usize,
    body: Range<usize>,
}

/// The records in `b[range]`, after a prefix of up to 16 bytes.
fn records(b: &[u8], range: Range<usize>) -> Vec<Rec> {
    let Some(mut i) = (range.start..(range.start + 16).min(range.end))
        .find(|&k| b.get(k..k + 2) == Some(b"WM".as_slice()))
    else {
        return Vec::new();
    };
    let mut out = Vec::new();
    while i + 3 <= range.end && &b[i..i + 2] == b"WM" {
        let name_end = i + 3 + b[i + 2] as usize;
        if name_end + 8 > range.end {
            break;
        }
        let size = u64::from_le_bytes(b[name_end..name_end + 8].try_into().unwrap()) as usize;
        let start = name_end + 8;
        if start + size > range.end {
            break;
        }
        out.push(Rec {
            name: String::from_utf8_lossy(&b[i + 3..name_end]).into_owned(),
            size_at: name_end,
            body: start..start + size,
        });
        i = start + size;
    }
    out
}

/// The chain of records from `root` down `names`.
fn path(b: &[u8], root: Range<usize>, names: &[&str]) -> Option<Vec<Rec>> {
    let mut chain = Vec::new();
    let mut at = root;
    for name in names {
        let r = records(b, at).into_iter().find(|r| r.name == *name)?;
        at = r.body.clone();
        chain.push(r);
    }
    Some(chain)
}

/// Replaces the payload of the last record of `chain` and fixes the sizes
/// of every record on the way down.
fn splice(b: &mut Vec<u8>, chain: &[Rec], payload: &[u8]) {
    let last = chain.last().expect("a path");
    let delta = payload.len() as i64 - last.body.len() as i64;
    for r in chain {
        let size = (r.body.len() as i64 + delta) as u64;
        b[r.size_at..r.size_at + 8].copy_from_slice(&size.to_le_bytes());
    }
    b.splice(last.body.clone(), payload.iter().copied());
}

fn record(name: &str, payload: &[u8]) -> Vec<u8> {
    let mut out = b"WM".to_vec();
    out.push(name.len() as u8);
    out.extend(name.as_bytes());
    out.extend((payload.len() as u64).to_le_bytes());
    out.extend(payload);
    out
}

/// Fritsch-Carlson slopes: a cubic through the points that never
/// overshoots, as WM's "Monotone Cubic" curves.
fn monotone_slopes(p: &[[f64; 2]]) -> Vec<f64> {
    let n = p.len();
    if n < 2 {
        return vec![0.; n];
    }
    let d: Vec<f64> = p
        .windows(2)
        .map(|w| (w[1][1] - w[0][1]) / (w[1][0] - w[0][0]).max(1e-9))
        .collect();
    let mut m: Vec<f64> = (0..n)
        .map(|i| match i {
            0 => d[0],
            i if i == n - 1 => d[n - 2],
            i if d[i - 1] * d[i] <= 0. => 0.,
            i => (d[i - 1] + d[i]) / 2.,
        })
        .collect();
    for i in 0..n - 1 {
        if d[i] == 0. {
            m[i] = 0.;
            m[i + 1] = 0.;
            continue;
        }
        let (a, b) = (m[i] / d[i], m[i + 1] / d[i]);
        let s = a * a + b * b;
        if s > 9. {
            let t = 3. / s.sqrt();
            m[i] = t * a * d[i];
            m[i + 1] = t * b * d[i];
        }
    }
    m
}

/// The curve through `p` with slopes `m` at `x`.
fn hermite(p: &[[f64; 2]], m: &[f64], x: f64) -> f64 {
    let Some(i) = p.windows(2).position(|w| x <= w[1][0]) else {
        return p.last().map_or(x, |q| q[1]);
    };
    let ([x0, y0], [x1, y1]) = (p[i], p[i + 1]);
    let h = (x1 - x0).max(1e-9);
    let t = ((x - x0) / h).clamp(0., 1.);
    let (t2, t3) = (t * t, t * t * t);
    (2. * t3 - 3. * t2 + 1.) * y0
        + (t3 - 2. * t2 + t) * h * m[i]
        + (-2. * t3 + 3. * t2) * y1
        + (t3 - t2) * h * m[i + 1]
}

// --- devices -----------------------------------------------------------------

/// One `Device2` record, whole.
#[derive(Clone)]
struct Device(Vec<u8>);

impl Device {
    fn root(&self) -> Range<usize> {
        0..self.0.len()
    }

    fn kind(&self) -> String {
        path(&self.0, self.root(), &["Device2", "ID"])
            .map(|c| String::from_utf8_lossy(&self.0[c[1].body.clone()]).into_owned())
            .unwrap_or_default()
    }

    fn inner(&self, names: &[&str]) -> Option<Vec<Rec>> {
        let mut full = vec!["Device2", "Contents", "Device"];
        full.extend(names);
        path(&self.0, self.root(), &full)
    }

    /// The parameter `id`'s record chain, from the device root.
    fn param(&self, id: &str) -> Result<Vec<Rec>> {
        let chain = self
            .inner(&["params", "ParmGroup", "parms"])
            .context("device without parameters")?;
        let parms = chain.last().unwrap().body.clone();
        for p in records(&self.0, parms) {
            let Some(r) = records(&self.0, p.body.clone())
                .into_iter()
                .find(|r| r.name == "id")
            else {
                continue;
            };
            if &self.0[r.body.clone()] == id.as_bytes() {
                let mut out = chain;
                out.push(p);
                return Ok(out);
            }
        }
        bail!("{} has no parameter {id:?}", self.kind())
    }

    /// Overwrites `param/val/<inner…>` with bytes of the same size.
    fn put(&mut self, param: &str, inner: &[&str], bytes: &[u8]) -> Result<()> {
        let chain = self.param(param)?;
        let mut names = vec!["val"];
        names.extend(inner);
        let tail = path(&self.0, chain.last().unwrap().body.clone(), &names)
            .with_context(|| format!("{}: {param} is not {inner:?}", self.kind()))?;
        let body = tail.last().unwrap().body.clone();
        ensure!(
            body.len() == bytes.len(),
            "{}: {param} holds {} bytes, not {}",
            self.kind(),
            body.len(),
            bytes.len()
        );
        self.0[body].copy_from_slice(bytes);
        Ok(())
    }

    fn float(&mut self, param: &str, v: f32) -> Result<&mut Self> {
        self.put(param, &["float"], &v.to_le_bytes())?;
        Ok(self)
    }

    fn int(&mut self, param: &str, v: i32) -> Result<&mut Self> {
        self.put(param, &["int"], &v.to_le_bytes())?;
        Ok(self)
    }

    /// Bools are one byte in some devices and four in others; keep the
    /// size the saved record has.
    fn flag(&mut self, param: &str, on: bool) -> Result<&mut Self> {
        let chain = self.param(param)?;
        let tail = path(
            &self.0,
            chain.last().unwrap().body.clone(),
            &["val", "bool"],
        )
        .with_context(|| format!("{}: {param} is not a bool", self.kind()))?;
        let mut bytes = vec![0u8; tail.last().unwrap().body.len()];
        if let Some(first) = bytes.first_mut() {
            *first = on as u8;
        }
        self.put(param, &["bool"], &bytes)?;
        Ok(self)
    }

    fn range(&mut self, param: &str, lo: f32, hi: f32) -> Result<&mut Self> {
        let mut v = lo.to_le_bytes().to_vec();
        v.extend(hi.to_le_bytes());
        self.put(param, &["range"], &v)?;
        Ok(self)
    }

    /// Picks an enum entry by its label, as World Machine shows it.
    fn choose(&mut self, param: &str, label: &str) -> Result<&mut Self> {
        let chain = self.param(param)?;
        let items = path(
            &self.0,
            chain.last().unwrap().body.clone(),
            &["val", "enum", "items"],
        )
        .with_context(|| format!("{}: {param} is not a choice", self.kind()))?;
        let index = records(&self.0, items.last().unwrap().body.clone())
            .iter()
            .position(|r| &self.0[r.body.clone()] == label.as_bytes())
            .with_context(|| format!("{}: {param} has no {label:?}", self.kind()))?;
        self.put(param, &["enum", "choice"], &(index as u32).to_le_bytes())?;
        Ok(self)
    }

    /// Sets a Curves device's curve: (input, output) points, 0..1, x
    /// increasing. The device keeps the curve twice: the `curvedata`
    /// parameter and `Contents/HeightCurve`, the copy it evaluates (with a
    /// 512-entry table, `oldcurve`, kept for old versions).
    fn curve(&mut self, points: &[[f64; 2]]) -> Result<&mut Self> {
        let slopes = monotone_slopes(points);
        // Each point: x, y and the curve's slope there as f64, then four
        // flag bytes and four bytes WM leaves uninitialised.
        let mut list = Vec::new();
        for (&[x, y], &m) in points.iter().zip(&slopes) {
            let mut p = Vec::with_capacity(32);
            for v in [x, y, m] {
                p.extend(v.to_le_bytes());
            }
            p.extend([0; 8]);
            list.extend(record("pt2", &p));
        }
        let spline = ["Curve1DSpline"];
        let roots: [Vec<&str>; 2] = [
            vec!["Device2", "Contents", "HeightCurve", "curve"],
            vec![
                "Device2",
                "Contents",
                "Device",
                "params",
                "ParmGroup",
                "parms",
            ],
        ];
        for (k, root) in roots.iter().enumerate() {
            // The parameter copy sits under its P4FULL; find it by id.
            let base = if k == 0 {
                path(&self.0, self.root(), root)
            } else {
                Some(self.param("curvedata")?).map(|mut c| {
                    c.extend(
                        path(
                            &self.0,
                            c.last().unwrap().body.clone(),
                            &["val", "custom", "value"],
                        )
                        .unwrap_or_default(),
                    );
                    c
                })
            };
            let Some(base) = base else {
                continue;
            };
            for (field, payload) in [
                ("numpts", (points.len() as u32).to_le_bytes().to_vec()),
                ("pointlist", list.clone()),
            ] {
                let mut chain = base.clone();
                chain.extend(
                    path(
                        &self.0,
                        chain.last().unwrap().body.clone(),
                        &[spline[0], field],
                    )
                    .with_context(|| format!("curve without {field}"))?,
                );
                splice(&mut self.0, &chain, &payload);
            }
        }
        if let Some(table) = path(
            &self.0,
            self.root(),
            &["Device2", "Contents", "HeightCurve", "oldcurve"],
        ) {
            let body = table.last().unwrap().body.clone();
            let n = body.len() / 4;
            let mut v = Vec::with_capacity(body.len());
            for i in 0..n {
                let x = i as f64 / (n - 1).max(1) as f64;
                v.extend((hermite(points, &slopes, x) as f32).to_le_bytes());
            }
            self.0[body].copy_from_slice(&v);
        }
        Ok(self)
    }

    /// Canvas position, display name (it names the output file) and id.
    fn place(&mut self, at: [i32; 2], name: &str, id: u32) -> Result<()> {
        let basic = self.inner(&["basic"]).context("device without basic")?;
        let body = basic.last().unwrap().body.clone();
        ensure!(body.len() >= 32, "basic is {} bytes", body.len());
        let mut v = at[0].to_le_bytes().to_vec();
        v.extend(at[1].to_le_bytes());
        // 24 bytes of name; unnamed devices keep stale memory there.
        let mut label = [0u8; 24];
        let n = name.len().min(23);
        label[..n].copy_from_slice(&name.as_bytes()[..n]);
        v.extend(label);
        self.0[body.start..body.start + 32].copy_from_slice(&v);
        let id_chain = self.inner(&["id"]).context("device without id")?;
        let body = id_chain.last().unwrap().body.clone();
        self.0[body].copy_from_slice(&id.to_le_bytes());
        Ok(())
    }
}

/// Every device of a saved world.
fn devices_of(world: &[u8]) -> Vec<Device> {
    let Some(chain) = path(world, MAGIC.len()..world.len(), &["DEVICEWORLD", "Devices"]) else {
        return Vec::new();
    };
    let body = chain.last().unwrap().body.clone();
    records(world, body)
        .into_iter()
        .filter(|r| r.name == "Device2")
        .map(|r| Device(world[r.body.start - 8 - 3 - r.name.len()..r.body.end].to_vec()))
        .collect()
}

/// A saved device of `kind`, preferring WM's New Project (configured
/// outputs and weightmap) over the one-of-each sample.
fn prototype(kind: &str, named: Option<&str>) -> Result<Device> {
    let default = devices_of(DEFAULT);
    let named_match = named.and_then(|n| {
        default.iter().find(|d| {
            d.kind() == kind
                && d.inner(&["basic"]).is_some_and(|c| {
                    let b = &d.0[c.last().unwrap().body.clone()];
                    b.get(8..8 + n.len()) == Some(n.as_bytes())
                })
        })
    });
    named_match
        .cloned()
        .or_else(|| devices_of(PROTOS2).into_iter().find(|d| d.kind() == kind))
        .or_else(|| devices_of(PROTOS3).into_iter().find(|d| d.kind() == kind))
        .or_else(|| devices_of(PROTOS).into_iter().find(|d| d.kind() == kind))
        .or_else(|| default.iter().find(|d| d.kind() == kind).cloned())
        .or_else(|| devices_of(DEVICES).into_iter().find(|d| d.kind() == kind))
        .with_context(|| format!("no saved {kind} device"))
}

/// A device graph to write as a world.
struct Graph {
    devices: Vec<Device>,
    /// (from, output port, to, input port)
    links: Vec<(usize, u8, usize, u8)>,
}

impl Graph {
    fn add(&mut self, kind: &str, name: &str, at: [i32; 2]) -> Result<usize> {
        let mut d = prototype(kind, Some(name))?;
        // Ids only need to be unique within the world.
        d.place(at, name, 0x200 + self.devices.len() as u32)?;
        self.devices.push(d);
        Ok(self.devices.len() - 1)
    }

    fn get(&mut self, i: usize) -> &mut Device {
        &mut self.devices[i]
    }

    fn link(&mut self, from: usize, out: u8, to: usize, input: u8) {
        self.links.push((from, out, to, input));
    }

    /// The empty world with these devices, wires and water level.
    fn write(&self, sea: f32) -> Result<Vec<u8>> {
        let mut b = EMPTY.to_vec();
        // Scale: the scene is one island wide and the working elevation
        // range is RANGE metres, so slopes, erosion and the export's 0..1
        // heights are Anno's. WorldCommonData holds the highest elevation
        // (in units) after a u32 version; both render extent copies hold
        // (x0, y0, x1, y1).
        let root = MAGIC.len()..b.len();
        let common = path(&b, root.clone(), &["DEVICEWORLD", "WorldCommonData"])
            .context("no WorldCommonData")?;
        let body = common.last().unwrap().body.clone();
        b[body.start + 4..body.start + 8].copy_from_slice(&(RANGE / UNIT).to_le_bytes());
        let half = SCENE / UNIT / 2.;
        let mut loc = Vec::new();
        for v in [CENTRE - half, CENTRE - half, CENTRE + half, CENTRE + half] {
            loc.extend(v.to_le_bytes());
        }
        let rext = b
            .windows(4)
            .position(|w| w == b"REXT")
            .context("no REXT render extents")?;
        b[rext + 5..rext + 21].copy_from_slice(&loc);
        let re = path(
            &b,
            root.clone(),
            &["DEVICEWORLD", "RenderExtentsLTE", "list", "RE", "loc"],
        )
        .context("no RE extents")?;
        let body = re.last().unwrap().body.clone();
        b[body].copy_from_slice(&loc);
        let root = MAGIC.len()..b.len();
        let chain = path(&b, root.clone(), &["DEVICEWORLD", "Devices"]).context("no Devices")?;
        let mut payload = (self.devices.len() as u32).to_le_bytes().to_vec();
        for d in &self.devices {
            payload.extend(&d.0);
        }
        splice(&mut b, &chain, &payload);
        // Each wire: 01, source index u16, output port, 01, target index
        // u16, input port, 00; the list ends with 00.
        let mut links = Vec::new();
        for &(from, out, to, input) in &self.links {
            links.push(1);
            links.extend((from as u16).to_le_bytes());
            links.push(out);
            links.push(1);
            links.extend((to as u16).to_le_bytes());
            links.extend([input, 0]);
        }
        links.push(0);
        let root = MAGIC.len()..b.len();
        let chain = path(&b, root, &["DEVICEWORLD", "Links"]).context("no Links")?;
        splice(&mut b, &chain, &links);
        // Water level guide: a mode byte, then the level. Shown in the 3D
        // view, so the sea in WM sits where the island's sea will.
        for names in [
            &["DEVICEWORLD", "Waterlevel"][..],
            &["projectauxdata", "guideWater"][..],
        ] {
            let root = MAGIC.len()..b.len();
            if let Some(c) = path(&b, root, names) {
                let body = c.last().unwrap().body.clone();
                if body.len() >= 5 {
                    b[body.start + 1..body.start + 5].copy_from_slice(&sea.to_le_bytes());
                }
            }
        }
        Ok(b)
    }
}

// --- templates ---------------------------------------------------------------

/// Metres as WM's internal unit, for generator sizes and erosion scale.
fn m(metres: f32) -> f32 {
    metres / UNIT
}

/// A world for `template`. `seed` varies the noise, so the same template
/// gives a new island each time.
pub fn world(template: Template, seed: u32) -> Result<Vec<u8>> {
    use Template::*;
    let mut g = Graph {
        devices: Vec::new(),
        links: Vec::new(),
    };
    let col = |c: i32| -800 + c * 160;
    let row = |r: i32| -300 + r * 90;
    let seed = |k: u32| (seed.wrapping_mul(2_654_435_761).wrapping_add(k * 7919) % 60_000) as i32;

    // Outline: a radial falloff, warped by a large noise so the coast gets
    // lobes, bays and peninsulas instead of a circle, then roughened by a
    // smaller one.
    let falloff = g.add("CGRD", "Island falloff", [col(0), row(0)])?;
    // Cone: an even slope out to the edge. Spherical steepens there and
    // squeezes the coast profile into a cliff.
    g.get(falloff).choose("type", "Cone")?.float(
        "width",
        m(match template {
            Highlands => 300.,
            Bay => 290.,
            _ => 280.,
        }),
    )?;
    let warp_noise = g.add("APRL", "Warp noise", [col(0), row(1)])?;
    g.get(warp_noise)
        .float("Scale", m(if template == Highlands { 200. } else { 260. }))?
        .int("Seed", seed(1))?
        .choose("Style", "Basic")?;
    let warp = g.add("WARP", "Warp outline", [col(1), row(0)])?;
    // Strength is in world units: 0.08 bends a 300 m island into lobes
    // and a bay; 0.7 shatters it into fjords.
    g.get(warp).float(
        "Strength",
        match template {
            Bay => 0.12,
            Highlands => 0.1,
            Mountain => 0.05,
            Plateau => 0.08,
        },
    )?;
    g.link(falloff, 0, warp, 0);
    g.link(warp_noise, 0, warp, 1);
    let coast = g.add("APRL", "Coast noise", [col(1), row(1)])?;
    g.get(coast)
        .float("Scale", m(90.))?
        .int("Seed", seed(2))?
        .choose("Style", "Basic")?;
    let shape = g.add("CMB2", "Shape", [col(2), row(0)])?;
    g.get(shape)
        .choose("Method", "Average")?
        .float("Strength", if template == Highlands { 0.75 } else { 0.25 })?;
    g.link(warp, 0, shape, 0);
    g.link(coast, 0, shape, 1);
    // The warp can push land to the scene's edge; a wider, unwarped
    // falloff keeps a ring of open sea for ships around every island.
    let border = g.add("CGRD", "Sea border", [col(1), row(2)])?;
    g.get(border)
        .choose("type", "Spherical")?
        .float("width", m(240.))?;
    let bounded = g.add("CMB2", "Keep sea border", [col(2), row(1)])?;
    g.get(bounded)
        .choose("Method", "Multiply")?
        .float("Strength", 1.)?;
    g.link(shape, 0, bounded, 0);
    g.link(border, 0, bounded, 1);
    let shape = bounded;

    // Two coast profiles over the same shape: gentle (beach, then grass
    // rising over ~40 m to the plateau, so the interior is reachable from
    // every gentle shore) and cliff (a step straight up from a narrow
    // beach). A noise picks cliffs for some stretches only. Heights are
    // SEA + metres / RANGE; the shelf sits at -3 m.
    let h = |metres: f64| (SEA as f64) + metres / RANGE as f64;
    let sea_floor = [[0.0, h(-18.)], [0.3, h(-12.)], [0.4, h(-3.5)]];
    let gentle = [
        [0.455, h(-1.2)],
        [0.48, h(0.4)],
        [0.52, h(1.2)],
        [0.6, h(2.8)],
        [0.7, h(5.)],
        [0.82, h(8.)],
        [1.0, h(11.)],
    ];
    let cliff = [
        [0.45, h(-2.)],
        [0.475, h(0.6)],
        [0.5, h(7.)],
        [0.6, h(9.)],
        [0.75, h(11.)],
        [0.9, h(13.)],
        [1.0, h(15.)],
    ];
    let soft = g.add("CURV", "Gentle coast", [col(3), row(0)])?;
    g.get(soft).curve(&[&sea_floor[..], &gentle[..]].concat())?;
    let steep = g.add("CURV", "Cliff coast", [col(3), row(1)])?;
    g.get(steep).curve(&[&sea_floor[..], &cliff[..]].concat())?;
    g.link(shape, 0, soft, 0);
    g.link(shape, 0, steep, 0);
    let character = g.add("APRL", "Coast character", [col(2), row(3)])?;
    g.get(character)
        .float("Scale", m(130.))?
        .int("Seed", seed(6))?
        .choose("Style", "Basic")?;
    // The share of cliff coast: noise above the threshold is cliff.
    let cliff_from = match template {
        Highlands => 0.5,
        Plateau | Mountain => 0.58,
        Bay => 0.62,
    };
    let cliffs = g.add("CURV", "Cliff stretches", [col(3), row(3)])?;
    g.get(cliffs).curve(&[
        [0.0, 0.0],
        [cliff_from, 0.0],
        [cliff_from + 0.08, 1.0],
        [1.0, 1.0],
    ])?;
    g.link(character, 0, cliffs, 0);
    let coastline = g.add("CHOS", "Coast type", [col(4), row(0)])?;
    g.link(soft, 0, coastline, 0);
    g.link(steep, 0, coastline, 1);
    g.link(cliffs, 0, coastline, 2);

    // Rolling detail and low hills, small enough for buildable ground.
    let detail = g.add("APRL", "Ground detail", [col(4), row(1)])?;
    g.get(detail)
        .float("Scale", m(45.))?
        .int("Seed", seed(3))?
        .choose("Style", "Smooth Billowy")?;
    let rough = g.add("CMB2", "Add detail", [col(5), row(0)])?;
    g.get(rough)
        .choose("Method", "Add")?
        .float("Strength", 0.02)?;
    g.link(coastline, 0, rough, 0);
    g.link(detail, 0, rough, 1);
    let hills = g.add("APRL", "Hills", [col(5), row(1)])?;
    g.get(hills)
        .float("Scale", m(160.))?
        .int("Seed", seed(4))?
        .choose("Style", "Smooth Billowy")?;
    let rolled = g.add("CMB2", "Add hills", [col(6), row(0)])?;
    g.get(rolled)
        .choose("Method", "Add")?
        .float("Strength", 0.045)?;
    g.link(rough, 0, rolled, 0);
    g.link(hills, 0, rolled, 1);

    // A mountain range: ridged noise where a large noise is high, kept
    // inland by the outline's interior, so it reads as a range on one side
    // rather than a cone in the middle.
    let range_noise = g.add("APRL", "Range placement", [col(4), row(4)])?;
    g.get(range_noise)
        .float("Scale", m(230.))?
        .int("Seed", seed(7))?
        .choose("Style", "Basic")?;
    let range_band = g.add("CURV", "Range band", [col(5), row(4)])?;
    let (band_from, band_to) = if template == Mountain {
        (0.42, 0.62)
    } else {
        (0.52, 0.7)
    };
    g.get(range_band)
        .curve(&[[0.0, 0.0], [band_from, 0.0], [band_to, 1.0], [1.0, 1.0]])?;
    g.link(range_noise, 0, range_band, 0);
    let inland = g.add("CURV", "Inland", [col(5), row(5)])?;
    g.get(inland)
        .curve(&[[0.0, 0.0], [0.56, 0.0], [0.72, 1.0], [1.0, 1.0]])?;
    g.link(shape, 0, inland, 0);
    let place = g.add("CMB2", "Range mask", [col(6), row(4)])?;
    g.get(place)
        .choose("Method", "Multiply")?
        .float("Strength", 1.)?;
    g.link(range_band, 0, place, 0);
    g.link(inland, 0, place, 1);
    let ridges = g.add("APRL", "Ridges", [col(6), row(5)])?;
    g.get(ridges)
        .float("Scale", m(90.))?
        .int("Seed", seed(5))?
        .choose("Style", "Ridged")?;
    let range = g.add("CMB2", "Range", [col(7), row(4)])?;
    g.get(range)
        .choose("Method", "Multiply")?
        .float("Strength", 1.)?;
    g.link(place, 0, range, 0);
    g.link(ridges, 0, range, 1);
    let raised = g.add("CMB2", "Add range", [col(7), row(0)])?;
    g.get(raised).choose("Method", "Add")?.float(
        "Strength",
        match template {
            Mountain => 0.55,
            Highlands => 0.3,
            Plateau => 0.25,
            Bay => 0.18,
        },
    )?;
    g.link(rolled, 0, raised, 0);
    g.link(range, 0, raised, 1);

    // Erosion carves gullies and valleys; its flow mask marks where water
    // gathers, and the strongest flow is cut in as river channels (~2 m)
    // that run down to the sea. Weathering then softens the cuts.
    let erosion = g.add("ERD2", "Erosion", [col(8), row(0)])?;
    g.get(erosion)
        .float("Amount", 60.)?
        .float("featureScale", m(40.))?;
    g.link(raised, 0, erosion, 0);
    let channels = g.add("CURV", "River channels", [col(8), row(2)])?;
    g.get(channels)
        .curve(&[[0.0, 0.0], [0.45, 0.0], [0.8, 1.0], [1.0, 1.0]])?;
    // Erosion's outputs count from 0: 1 is the flow mask.
    g.link(erosion, 1, channels, 0);
    let soften = g.add("BLUR", "Soften channels", [col(9), row(2)])?;
    // Scale-independent blur measures its radius against the whole world
    // and smears a 2 m channel into nothing; off, the radius is local.
    // Only the strongest flow is cut (the curve above): every trickle cut
    // sharp shatters the island into fjords.
    g.get(soften)
        .float("Blur radius", 2.)?
        .flag("Scale-independant", false)?;
    g.link(channels, 0, soften, 0);
    let carved = g.add("CMB2", "Carve rivers", [col(9), row(0)])?;
    g.get(carved)
        .choose("Method", "Subtract")?
        .float("Strength", 0.025)?;
    g.link(erosion, 0, carved, 0);
    g.link(soften, 0, carved, 1);
    let weather = g.add("EWTH", "Weathering", [col(10), row(0)])?;
    // Less talus than WM's default: at island scale it reads as speckle.
    g.get(weather).float("Talus Production", 0.25)?;
    g.link(carved, 0, weather, 0);
    let terrain = weather;

    let height = g.add("OUTP", "Height", [col(11), row(0)])?;
    g.get(height)
        .choose("format", "PNG (16bit)")?
        .flag("exportAlways", true)?;
    g.link(terrain, 0, height, 0);

    // Colormap for the ground tint. The importer takes tint relative to the
    // image's mean, so only the variation matters: each channel is a flat
    // base plus or minus three masks. Wet ground (erosion's flow, blurred
    // into a band along valleys and gullies) turns greener and darker, dry
    // slopes warmer and lighter, and a large noise breaks up flat plains.
    // Flow values are small away from the main channels: lift them so
    // every gully and stream counts, then spread them into a band.
    let flow = g.add("CURV", "Wet ground", [col(8), row(7)])?;
    g.get(flow)
        .curve(&[[0.0, 0.0], [0.04, 0.5], [0.15, 0.9], [1.0, 1.0]])?;
    g.link(erosion, 1, flow, 0);
    let wet_band = g.add("BLUR", "Wetness", [col(9), row(7)])?;
    g.get(wet_band)
        .float("Blur radius", 2.)?
        .flag("Scale-independant", false)?;
    g.link(flow, 0, wet_band, 0);
    // Slope selection has hard edges at island scale; blurred, dry ground
    // fades in over a few metres.
    let steep = g.add("SSEL", "Dry slopes", [col(8), row(8)])?;
    g.get(steep)
        .range("range", 15., 45.)?
        .float("Falloff", 0.5)?;
    g.link(terrain, 0, steep, 0);
    let dry_band = g.add("BLUR", "Dryness", [col(9), row(8)])?;
    g.get(dry_band)
        .float("Blur radius", 1.5)?
        .flag("Scale-independant", false)?;
    g.link(steep, 0, dry_band, 0);
    // Only land: the shelf's drop-off is steep and the flow runs on under
    // water, which would put the tint's variation in the sea.
    let land = g.add("HSEL", "Land", [col(8), row(9)])?;
    g.get(land)
        .range("range", SEA + 0.003, 1.)?
        .float("Falloff", 0.02)?;
    g.link(terrain, 0, land, 0);
    let on_land = |g: &mut Graph, mask: usize, name: &str, r: i32| -> Result<usize> {
        let m = g.add("CMB2", name, [col(10) - 80, row(r)])?;
        g.get(m)
            .choose("Method", "Multiply")?
            .float("Strength", 1.)?;
        g.link(mask, 0, m, 0);
        g.link(land, 0, m, 1);
        Ok(m)
    };
    let wet = on_land(&mut g, wet_band, "Wet land", 7)?;
    let dry = on_land(&mut g, dry_band, "Dry land", 8)?;
    let patches = g.add("APRL", "Tint patches", [col(9), row(9)])?;
    g.get(patches)
        .float("Scale", m(140.))?
        .int("Seed", seed(8))?
        .choose("Style", "Smooth Billowy")?;
    // (wet, dry, patches) weights per channel; negative subtracts.
    let channels = [
        ("Tint red", [-0.1f32, 0.1, 0.05]),
        ("Tint green", [0.06, 0.03, 0.05]),
        ("Tint blue", [-0.03, -0.05, 0.03]),
    ];
    let combine = g.add("PRCC", "Tint channels", [col(12), row(8)])?;
    for (c, (name, weights)) in channels.into_iter().enumerate() {
        let base = g.add("CHGT", name, [col(10), row(7 + c as i32)])?;
        // Mid-range, so a few percent either way stays inside 0..1.
        g.get(base).float("Height", 0.5)?;
        let mut at = base;
        for (k, (source, w)) in [wet, dry, patches].into_iter().zip(weights).enumerate() {
            let step = g.add(
                "CMB2",
                if w < 0. { "Tint less" } else { "Tint more" },
                [col(10) + 40 * (k as i32 + 1), row(7 + c as i32)],
            )?;
            g.get(step)
                .choose("Method", if w < 0. { "Subtract" } else { "Add" })?
                .float("Strength", w.abs())?;
            g.link(at, 0, step, 0);
            g.link(source, 0, step, 1);
            at = step;
        }
        g.link(at, 0, combine, c as u8);
    }
    let colormap = g.add("PRBO", "Colormap", [col(13), row(8)])?;
    g.get(colormap)
        .choose("File Format", "PNG  (8bits/channel)")?
        .flag("Output File on every Build", true)?;
    g.link(combine, 0, colormap, 0);

    // Splatmaps: 20 channels in five RGBA images, as retail islands use
    // about 20 ground materials. Each channel is a mask; the weightmap
    // favours earlier inputs where masks overlap, so specific features
    // come first and the base grass last takes what is left. Which palette
    // material each channel gets is `CHANNELS`.
    let h = |metres: f32| SEA + metres / RANGE;
    let mut masks: Vec<usize> = Vec::new();
    let mut row_at = 12;
    let mut next = |g: &mut Graph, kind: &str, name: &str| -> Result<usize> {
        row_at += 1;
        g.add(kind, name, [col(10), row(row_at)])
    };
    let times = |g: &mut Graph, a: usize, b: usize, name: &str| -> Result<usize> {
        let m = g.add("CMB2", name, [col(11), row(14)])?;
        g.get(m)
            .choose("Method", "Multiply")?
            .float("Strength", 1.)?;
        g.link(a, 0, m, 0);
        g.link(b, 0, m, 1);
        Ok(m)
    };
    let slope = |g: &mut Graph,
                 next: &mut dyn FnMut(&mut Graph, &str, &str) -> Result<usize>,
                 name: &str,
                 lo: f32,
                 hi: f32,
                 falloff: f32|
     -> Result<usize> {
        let m = next(g, "SSEL", name)?;
        g.get(m).range("range", lo, hi)?.float("Falloff", falloff)?;
        g.link(terrain, 0, m, 0);
        Ok(m)
    };
    let band = |g: &mut Graph,
                next: &mut dyn FnMut(&mut Graph, &str, &str) -> Result<usize>,
                name: &str,
                lo: f32,
                hi: f32,
                falloff: f32|
     -> Result<usize> {
        let m = next(g, "HSEL", name)?;
        g.get(m)
            // Height ranges outside 0..1 take World Machine down mid-build.
            .range("range", h(lo).clamp(0., 1.), h(hi).clamp(0., 1.))?
            .float("Falloff", falloff)?;
        g.link(terrain, 0, m, 0);
        Ok(m)
    };
    // Soft patches of noise above `from`, for meadows and variety.
    let patch = |g: &mut Graph,
                 next: &mut dyn FnMut(&mut Graph, &str, &str) -> Result<usize>,
                 name: &str,
                 size: f32,
                 k: u32,
                 from: f64|
     -> Result<usize> {
        let n = next(g, "APRL", &format!("{name} noise"))?;
        g.get(n)
            .float("Scale", m(size))?
            .int("Seed", seed(20 + k))?
            .choose("Style", "Basic")?;
        let c = next(g, "CURV", name)?;
        g.get(c)
            .curve(&[[0.0, 0.0], [from, 0.0], [from + 0.12, 1.0], [1.0, 1.0]])?;
        g.link(n, 0, c, 0);
        Ok(c)
    };
    // Gentle land: flat sea floor is gentle too, and must stay sea.
    let gentle_any = slope(&mut g, &mut next, "Gentle", 0., 14., 0.3)?;
    let gentle = times(&mut g, gentle_any, land, "Gentle land")?;
    let high = band(&mut g, &mut next, "Highland", 22., 100., 0.1)?;
    let screen = |g: &mut Graph, a: usize, b: usize, name: &str| -> Result<usize> {
        let u = g.add("CMB2", name, [col(11), row(14)])?;
        g.get(u).choose("Method", "Screen")?.float("Strength", 1.)?;
        g.link(a, 0, u, 0);
        g.link(b, 0, u, 1);
        Ok(u)
    };
    // The channels follow `CHANNELS`, whose roles come from where retail
    // islands put each material.

    // Image 1, rock. Cliffs on steep land, the sharpest faces apart; rock
    // on the rocky shore, where retail puts it (mostly at or under the
    // waterline, not inland); gravel where erosion drops scree and on the
    // middling slopes.
    let cliff = slope(&mut g, &mut next, "Cliff", 35., 90., 0.12)?;
    masks.push(times(&mut g, cliff, land, "Cliff on land")?);
    let sharp = slope(&mut g, &mut next, "Sharp cliff", 48., 90., 0.1)?;
    masks.push(times(&mut g, sharp, land, "Sharp cliff on land")?);
    let rough = slope(&mut g, &mut next, "Rough", 12., 90., 0.2)?;
    let shore_band = band(&mut g, &mut next, "Shore band", -6., 3., 0.02)?;
    masks.push(times(&mut g, rough, shore_band, "Coastal rock")?);
    let scree = next(&mut g, "CURV", "Scree")?;
    g.get(scree)
        .curve(&[[0.0, 0.0], [0.35, 0.0], [0.7, 1.0], [1.0, 1.0]])?;
    g.link(erosion, 3, scree, 0);
    let mid = slope(&mut g, &mut next, "Middling slopes", 20., 32., 0.25)?;
    let gravel = screen(&mut g, scree, mid, "Scree or slope")?;
    masks.push(times(&mut g, gravel, land, "Gravel")?);

    // Image 2, shore, from the sea up: seagrass in the shallows, beach
    // sand from the shelf to the beach (one material covers both in
    // retail), a wet band at the waterline, dry sand just above.
    masks.push(band(&mut g, &mut next, "Seagrass", -6., -1.2, 0.02)?);
    masks.push(band(&mut g, &mut next, "Beach and shelf", -15., 0.8, 0.01)?);
    masks.push(band(&mut g, &mut next, "Waterline", -0.4, 0.4, 0.01)?);
    masks.push(band(&mut g, &mut next, "Shore sand", 0.6, 3., 0.02)?);

    // Image 3, water: pebbles in the carved channels, mud on the banks, a
    // wider damp shore along rivers, and pebble coves along the beach.
    masks.push(times(&mut g, soften, land, "River bed")?);
    let banks = next(&mut g, "CURV", "Wet banks")?;
    g.get(banks)
        .curve(&[[0.0, 0.0], [0.12, 0.0], [0.35, 1.0], [1.0, 1.0]])?;
    g.link(erosion, 1, banks, 0);
    let narrow = next(&mut g, "BLUR", "Bank width")?;
    g.get(narrow)
        .float("Blur radius", 1.5)?
        .flag("Scale-independant", false)?;
    g.link(banks, 0, narrow, 0);
    masks.push(times(&mut g, narrow, land, "River banks")?);
    let wide = next(&mut g, "BLUR", "Shore width")?;
    g.get(wide)
        .float("Blur radius", 4.)?
        .flag("Scale-independant", false)?;
    g.link(banks, 0, wide, 0);
    let damp_shore = times(&mut g, wide, gentle, "River shore")?;
    masks.push(damp_shore);
    let coves = patch(&mut g, &mut next, "Pebble coves", 80., 4, 0.6)?;
    let beachy = band(&mut g, &mut next, "Cove band", -0.6, 1.5, 0.02)?;
    masks.push(times(&mut g, coves, beachy, "Pebble beach")?);

    // Image 4, ground: dirt on worn ridges, dry grass on slopes, lush
    // grass where water collects, and base grass for the rest.
    let wear = next(&mut g, "CURV", "Worn ground")?;
    g.get(wear)
        .curve(&[[0.0, 0.0], [0.5, 0.0], [0.85, 1.0], [1.0, 1.0]])?;
    g.link(erosion, 2, wear, 0);
    masks.push(times(&mut g, wear, land, "Dirt")?);
    let dry = slope(&mut g, &mut next, "Dry grass", 11., 20., 0.4)?;
    masks.push(times(&mut g, dry, land, "Dry grass on land")?);
    let damp = next(&mut g, "CURV", "Damp ground")?;
    g.get(damp)
        .curve(&[[0.0, 0.0], [0.5, 0.0], [0.85, 1.0], [1.0, 1.0]])?;
    g.link(wet, 0, damp, 0);
    masks.push(times(&mut g, damp, gentle, "Lush grass")?);
    // Base grass fills whatever the other channels leave: their union
    // (Screen: 1 - (1 - a)(1 - b)), inverted, on land. Faint traces do not
    // count: grass is full up to a union of 0.25 and gone by 0.6.
    let mut union = masks[0];
    for &mask in &masks[1..] {
        union = screen(&mut g, union, mask, "Union")?;
    }
    let rest = next(&mut g, "CURV", "Grass")?;
    g.get(rest)
        .curve(&[[0.0, 1.0], [0.25, 1.0], [0.6, 0.0], [1.0, 0.0]])?;
    g.link(union, 0, rest, 0);
    masks.push(times(&mut g, rest, land, "Grass on land")?);

    // Image 5, overlay patches over the ground: dry upland grass, flower
    // meadows, marsh on flat ground just above the sea, bare earth.
    let mut overlays: Vec<usize> = Vec::new();
    overlays.push(times(&mut g, high, gentle, "Upland grass")?);
    let flowers = patch(&mut g, &mut next, "Flower meadow", 70., 1, 0.62)?;
    overlays.push(times(&mut g, flowers, gentle, "Flowers")?);
    let flat = slope(&mut g, &mut next, "Flat", 0., 4., 0.3)?;
    let low = band(&mut g, &mut next, "Low land", 0.3, 2., 0.02)?;
    overlays.push(times(&mut g, flat, low, "Marsh")?);
    let earth = patch(&mut g, &mut next, "Bare earth", 45., 3, 0.72)?;
    overlays.push(times(&mut g, earth, land, "Earth")?);
    debug_assert_eq!(masks.len() + overlays.len(), CHANNELS.len());

    // World Machine Basic crashes building a weightmap of more than 16
    // inputs, so the ground's 16 channels share one (they sum to one
    // everywhere) and the four overlay patches a second. Its fifth input,
    // a constant, wins where no patch is, so overlays are zero there and
    // reach about 75 % in a patch; the link adds channel weights, so
    // overlays blend over the ground.
    let weights = g.add("NORM", "Weights", [col(12), row(16)])?;
    g.get(weights)
        .int("Number of Inputs", masks.len() as i32)?
        .choose("outputType", "Weight map (packed)")?
        // Masks compete by strength. Favouring top inputs decays later
        // inputs so steeply over 16 that the last never win anywhere.
        .choose("Priority Behavior", "Equal priority")?
        .float("Exclusion", 0.3)?;
    for (port, mask) in masks.iter().enumerate() {
        g.link(*mask, 0, weights, port as u8);
    }
    let none = g.add("CHGT", "No overlay", [col(11), row(22)])?;
    g.get(none).float("Height", 0.35)?;
    let overlay = g.add("NORM", "Overlay weights", [col(12), row(22)])?;
    g.get(overlay)
        .int("Number of Inputs", overlays.len() as i32 + 1)?
        .choose("outputType", "Weight map (packed)")?
        .choose("Priority Behavior", "Equal priority")?;
    for (port, mask) in overlays.iter().chain([&none]).enumerate() {
        g.link(*mask, 0, overlay, port as u8);
    }
    // One image per four channels; names sort in channel order. The
    // overlay weightmap's second image (only "none") is not exported.
    for (k, name) in SPLATMAPS.iter().enumerate() {
        let out = g.add("PRBO", name, [col(13), row(14 + k as i32)])?;
        g.get(out)
            .choose("File Format", "PNG  (8bits/channel)")?
            .flag("Output File on every Build", true)?;
        if k < 4 {
            g.link(weights, k as u8, out, 0);
        } else {
            g.link(overlay, 0, out, 0);
        }
    }

    g.write(SEA)
}

#[cfg(test)]
mod tests {
    use super::{MAGIC, Template, devices_of, path, records, world};

    /// Parameter value bytes of `kind`'s `param` in a written world.
    fn value(b: &[u8], name: &str, param: &str, inner: &[&str]) -> Vec<u8> {
        let d = devices_of(b)
            .into_iter()
            .find(|d| {
                d.inner(&["basic"]).is_some_and(|c| {
                    let body = &d.0[c.last().unwrap().body.clone()];
                    body.get(8..8 + name.len()) == Some(name.as_bytes())
                })
            })
            .unwrap_or_else(|| panic!("no device {name}"));
        let chain = d.param(param).unwrap();
        let mut names = vec!["val"];
        names.extend(inner);
        let tail = path(&d.0, chain.last().unwrap().body.clone(), &names).unwrap();
        d.0[tail.last().unwrap().body.clone()].to_vec()
    }

    #[test]
    fn channels_pick_palette_materials_by_role() {
        let palette: Vec<(u8, String)> = [
            (2, "x/cliff_limestone_01.xml"),
            (9, "x/rock_limestone_01.xml"),
            (21, "x/river_pebbles_01.xml"),
            (7, "x/beach_04.xml"),
            (1, "x/grass_01_e.xml"),
            (13, "x/grass_01_f_dark.xml"),
        ]
        .map(|(id, f)| (id, f.to_owned()))
        .into();
        let c = super::channel_materials(anno_formats::game::Game::Anno117, &palette);
        assert_eq!(c.len(), 5);
        // Rock: cliff twice (no sharp cliff in this palette), rock, no gravel.
        assert_eq!(c[0], [Some(2), Some(2), Some(9), None]);
        assert_eq!(c[1][1], Some(7), "beach & shelf");
        assert_eq!(c[2][0], Some(21), "river bed");
        assert_eq!(c[3][2], Some(13), "lush grass");
        assert_eq!(c[3][3], Some(1), "base grass");
    }

    #[test]
    fn all_template_channels_have_materials_in_1800_regions() {
        for names in [
            ["m_mountain_01", "m_stone_03", "m_beach_01", "m_gras_02"],
            [
                "tropical_mountain_01",
                "s_rock_01",
                "s_beach_06",
                "d_grass_07",
            ],
            ["p_mountain_01", "p_stone_01", "p_beach_01", "p_snow_01"],
            [
                "colony02_mountain_01",
                "colony02_rock_01",
                "colony02_beach_06",
                "colony02_grass_07",
            ],
        ] {
            let palette: Vec<_> = names
                .iter()
                .enumerate()
                .map(|(i, name)| (i as u8, format!("terrain/{name}.xml")))
                .collect();
            let channels = super::channel_materials(anno_formats::game::Game::Anno1800, &palette);
            assert_eq!(channels.len(), 5);
            assert!(
                channels.iter().flatten().all(Option::is_some),
                "{names:?}: {channels:?}"
            );
        }
    }

    #[test]
    fn templates_write_wired_worlds_world_machine_can_read_back() {
        for t in Template::ALL {
            let b = world(t, 7).unwrap();
            assert!(b.starts_with(MAGIC));
            // The record tree spans the whole file, sizes and all.
            let top = records(&b, MAGIC.len()..b.len());
            assert_eq!(top[0].name, "DEVICEWORLD");
            let devices = devices_of(&b);
            assert!(devices.len() >= 60, "{t:?}: {} devices", devices.len());
            let kinds: Vec<String> = devices.iter().map(|d| d.kind()).collect();
            for k in ["CGRD", "APRL", "CURV", "ERD2", "OUTP", "NORM", "PRBO"] {
                assert!(kinds.iter().any(|x| x == k), "{t:?} lacks {k}");
            }
            // Wires decode back to valid device indices, ending in 00.
            let links = path(&b, MAGIC.len()..b.len(), &["DEVICEWORLD", "Links"]).unwrap();
            let l = &b[links.last().unwrap().body.clone()];
            assert_eq!(l.len() % 9, 1);
            assert_eq!(*l.last().unwrap(), 0);
            for w in l[..l.len() - 1].chunks(9) {
                let (from, to) = (
                    u16::from_le_bytes([w[1], w[2]]) as usize,
                    u16::from_le_bytes([w[5], w[6]]) as usize,
                );
                assert!(w[0] == 1 && w[4] == 1 && w[8] == 0);
                assert!(from < devices.len() && to < devices.len());
            }
            // Names reach the file names the link looks for.
            assert_eq!(
                value(&b, "Splatmap", "Output File on every Build", &["bool"])[0],
                1
            );
            assert_eq!(value(&b, "Height", "exportAlways", &["bool"])[0], 1);
            // A colormap for the link's ground tint, written on every build,
            // from three channels.
            assert_eq!(
                value(&b, "Colormap", "Output File on every Build", &["bool"])[0],
                1
            );
            assert!(kinds.iter().filter(|k| *k == "CHGT").count() >= 3);
            assert!(kinds.iter().any(|k| k == "PRCC"));
            assert_eq!(
                value(&b, "Weights", "Number of Inputs", &["int"]),
                16i32.to_le_bytes()
            );
            // The plateau curve holds the profile, starting in deep water.
            let n = value(
                &b,
                "Gentle coast",
                "curvedata",
                &["custom", "value", "Curve1DSpline", "numpts"],
            );
            assert!(u32::from_le_bytes(n.try_into().unwrap()) >= 9);
        }
        // Seeds give different worlds.
        assert_ne!(
            world(Template::Plateau, 1).unwrap(),
            world(Template::Plateau, 2).unwrap()
        );
    }
}

#[cfg(test)]
mod write_out {
    // Iteration aid: writes every template to $WM_TEMPLATES_OUT.
    #[test]
    #[ignore]
    fn write_templates() {
        let dir = std::path::PathBuf::from(
            std::env::var_os("WM_TEMPLATES_OUT").expect("WM_TEMPLATES_OUT"),
        );
        for t in super::Template::ALL {
            let name = format!("{t:?}.tmd").to_lowercase();
            std::fs::write(dir.join(&name), super::world(t, 20261101).unwrap()).unwrap();
        }
    }
}
