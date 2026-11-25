// Owns 2070's lakes as the sea's pass draws them: each lake's outline and
// level from the island file's `Lakes`, its look from lakesettings.xml, and
// the triangles the pass draws them with (lake.fx draws the outline at the
// lake's level, and the ground hides what lies under it).

use anno_formats::isd::{Doc, NodeId, Value};
use anyhow::{Context, Result, bail};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, TryRecvError};

/// The lakes' looks, by the names the islands' lakes give.
pub const SETTINGS: &str = "data/config/engine/lakesettings.xml";
/// The look a lake naming none known takes.
const DEFAULT_LOOK: &str = "A5_Default";
/// Lake positions and levels are fixed point, 4096 to the metre: the
/// outlines then fall inside their islands (the largest is 256 m), as no
/// other scale puts them.
const FIXED: f32 = 4096.;
/// Lakes a look table holds; lakesettings.xml ships 23. Bounds the storage
/// buffer the pass binds.
pub const MAX_LOOKS: usize = 64;

/// One lake setting: `Color` (lake.fx's WaterPollute with pollution at
/// zero; alpha how much of it the deep water shows) and `MaxDepth`, the
/// metres of view through water at which it shows fully.
#[derive(Clone, Debug, PartialEq)]
pub struct Look {
    pub name: String,
    pub color: [f32; 4],
    pub max_depth: f32,
}

/// Every `LakeSetting` in `xml`, in file order, at most [`MAX_LOOKS`];
/// settings missing a value are left out.
pub fn looks(xml: &str) -> Vec<Look> {
    use anno_formats::xml::{block, tag};
    let number = |xml: &str, name: &str| -> Option<f32> { tag(xml, name)?.parse().ok() };
    (xml.split("<LakeSetting>").skip(1))
        .filter_map(|setting| {
            let color = block(setting, "Color")?;
            Some(Look {
                name: tag(setting, "Name")?.to_owned(),
                color: [
                    number(color, "r")?,
                    number(color, "g")?,
                    number(color, "b")?,
                    number(color, "a")?,
                ],
                max_depth: number(setting, "MaxDepth")?,
            })
        })
        .take(MAX_LOOKS)
        .collect()
}

/// One lake: the index of its look, its level and its outline in world X
/// and Z.
#[derive(Clone, Debug, PartialEq)]
pub struct Lake {
    pub look: u32,
    pub height: f32,
    pub outline: Vec<[f32; 2]>,
}

/// The lakes in island file `file`, their looks found by name in `looks`
/// (the default look, or the first, for a name not there).
pub fn read(file: &[u8], looks: &[Look]) -> Result<Vec<Lake>> {
    let doc = Doc::parse(file)?;
    let Some(lakes) = doc.root("Lakes") else {
        return Ok(Vec::new());
    };
    let text = |id: Option<NodeId>| -> Option<&str> {
        match doc.value(id?) {
            Value::Text(t) => std::str::from_utf8(t).ok(),
            _ => None,
        }
    };
    let fallback = looks
        .iter()
        .position(|l| l.name == DEFAULT_LOOK)
        .unwrap_or(0);
    let mut out = Vec::new();
    for lake in doc.children(lakes) {
        let name = text(doc.child(lake, "m_Setting")).unwrap_or_default();
        let look = looks
            .iter()
            .position(|l| l.name == name)
            .unwrap_or(fallback);
        let height: i64 = (text(doc.child(lake, "m_WaterHeight")))
            .context("a lake without m_WaterHeight")?
            .trim()
            .parse()?;
        let points = doc
            .child(lake, "m_Points")
            .context("a lake without m_Points")?;
        let outline = (doc.children(points))
            .map(|p| match doc.value(p) {
                Value::Blob(b) if b.len() == 16 => {
                    let at = |k: usize| {
                        i64::from_le_bytes(b[k..k + 8].try_into().expect("8 bytes")) as f32 / FIXED
                    };
                    Ok([at(0), at(8)])
                }
                _ => bail!("a lake point that is not two 64-bit coordinates"),
            })
            .collect::<Result<Vec<_>>>()?;
        out.push(Lake {
            look: look as u32,
            height: height as f32 / FIXED,
            outline,
        });
    }
    Ok(out)
}

/// The lakes' triangles for the pass, three vertices each: world X, the
/// lake's level, world Z, and its look's index.
pub fn triangles(lakes: &[Lake]) -> Vec<[f32; 4]> {
    let mut out = Vec::new();
    for lake in lakes {
        for t in triangulate(&lake.outline) {
            for i in t {
                let [x, z] = lake.outline[i];
                out.push([x, lake.height, z, lake.look as f32]);
            }
        }
    }
    out
}

/// A simple polygon cut into triangles by ear clipping, either winding.
/// Outlines are a few dozen points (the shipped most is 20), so the
/// quadratic walk costs nothing. A degenerate rest is dropped.
pub fn triangulate(outline: &[[f32; 2]]) -> Vec<[usize; 3]> {
    let area: f32 = (0..outline.len())
        .map(|i| {
            let (a, b) = (outline[i], outline[(i + 1) % outline.len()]);
            a[0] * b[1] - b[0] * a[1]
        })
        .sum();
    let turn = |a: [f32; 2], b: [f32; 2], c: [f32; 2]| {
        let cross = (b[0] - a[0]) * (c[1] - a[1]) - (b[1] - a[1]) * (c[0] - a[0]);
        cross * area.signum()
    };
    let mut left: Vec<usize> = (0..outline.len()).collect();
    let mut out = Vec::new();
    while left.len() > 3 {
        let n = left.len();
        let ear = (0..n).find(|&i| {
            let (a, b, c) = (left[(i + n - 1) % n], left[i], left[(i + 1) % n]);
            let (pa, pb, pc) = (outline[a], outline[b], outline[c]);
            turn(pa, pb, pc) > 0.
                && !(left.iter()).any(|&j| {
                    j != a
                        && j != b
                        && j != c
                        && turn(pa, pb, outline[j]) >= 0.
                        && turn(pb, pc, outline[j]) >= 0.
                        && turn(pc, pa, outline[j]) >= 0.
                })
        });
        let Some(i) = ear else {
            // Self-touching or collinear rest: no ear left to cut.
            return out;
        };
        out.push([left[(i + n - 1) % n], left[i], left[(i + 1) % n]]);
        left.remove(i);
    }
    if left.len() == 3 {
        out.push([left[0], left[1], left[2]]);
    }
    out
}

/// The lakes of the island file a view shows, read off the UI thread when
/// the file changes (another island): parsing a 2 MB file whole took
/// longer than a frame. The last read stays drawn meanwhile.
#[derive(Default)]
pub struct Reader {
    /// The file asked for last, by pointer.
    asked: usize,
    drawn: Option<Arc<Vec<[f32; 4]>>>,
    reading: Option<Receiver<Vec<[f32; 4]>>>,
}

impl Reader {
    pub fn want(&mut self, file: Option<&Arc<[u8]>>, looks: &Arc<Vec<Look>>) {
        let key = file.map_or(0, |f| Arc::as_ptr(f) as *const u8 as usize);
        if key == self.asked {
            return;
        }
        self.asked = key;
        self.drawn = None;
        self.reading = None;
        let Some(file) = file else {
            return;
        };
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let (file, looks) = (file.clone(), looks.clone());
        let spawned = std::thread::Builder::new()
            .name("2070 lakes".into())
            .spawn(move || {
                let lakes = read(&file, &looks).unwrap_or_else(|e| {
                    eprintln!("lakes: {e:#}");
                    Vec::new()
                });
                let _ = tx.send(triangles(&lakes));
            });
        if spawned.is_ok() {
            self.reading = Some(rx);
        }
    }

    /// Takes a finished read.
    pub fn land(&mut self) {
        let Some(rx) = &self.reading else {
            return;
        };
        match rx.try_recv() {
            Ok(t) => self.drawn = Some(Arc::new(t)),
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => {}
        }
        self.reading = None;
    }

    /// The triangles to draw; `None` until the file's lakes are read.
    pub fn triangles(&self) -> Option<&Arc<Vec<[f32; 4]>>> {
        self.drawn.as_ref()
    }

    pub fn reading(&self) -> bool {
        self.reading.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area(outline: &[[f32; 2]], t: [usize; 3]) -> f32 {
        let [a, b, c] = t.map(|i| outline[i]);
        ((b[0] - a[0]) * (c[1] - a[1]) - (b[1] - a[1]) * (c[0] - a[0])).abs() * 0.5
    }

    // A concave outline, either way round, comes out as triangles that
    // cover exactly its area and never the notch.
    #[test]
    fn a_concave_outline_triangulates_to_its_area() {
        // A 4x4 square with a 2x2 notch cut from its top edge: 12 m².
        let mut outline = vec![
            [0., 0.],
            [4., 0.],
            [4., 4.],
            [3., 4.],
            [3., 2.],
            [1., 2.],
            [1., 4.],
            [0., 4.],
        ];
        for _ in 0..2 {
            let triangles = triangulate(&outline);
            assert_eq!(triangles.len(), outline.len() - 2);
            let total: f32 = triangles.iter().map(|&t| area(&outline, t)).sum();
            assert!((total - 12.).abs() < 1e-5, "{total}");
            // The notch's centre lies in no triangle.
            let inside = |t: [usize; 3], p: [f32; 2]| {
                let [a, b, c] = t.map(|i| outline[i]);
                let s = |u: [f32; 2], v: [f32; 2]| {
                    (v[0] - u[0]) * (p[1] - u[1]) - (v[1] - u[1]) * (p[0] - u[0])
                };
                let (d1, d2, d3) = (s(a, b), s(b, c), s(c, a));
                (d1 >= 0. && d2 >= 0. && d3 >= 0.) || (d1 <= 0. && d2 <= 0. && d3 <= 0.)
            };
            assert!(!triangles.iter().any(|&t| inside(t, [2., 3.])));
            outline.reverse();
        }
    }

    // An island file's lake comes out with its look by name, its level and
    // outline from 1/4096 m fixed point; an unknown name takes the default.
    #[test]
    fn a_lake_reads_its_look_level_and_outline() {
        let point = |x: i64, z: i64| {
            let mut b = b"<i>CDATA[".to_vec();
            b.extend(16u32.to_le_bytes());
            b.extend(x.to_le_bytes());
            b.extend(z.to_le_bytes());
            b.extend(b"]</i>\r\n");
            b
        };
        let lake = |name: &str, height: i64, points: &[(i64, i64)]| {
            let mut b = format!(
                "<i><m_Setting>{name}</m_Setting>\r\n<m_WaterHeight>{height}</m_WaterHeight>\r\n<m_Points>"
            )
            .into_bytes();
            for &(x, z) in points {
                b.extend(point(x, z));
            }
            b.extend(b"</m_Points>\r\n<m_DamHeight>0</m_DamHeight>\r\n</i>\r\n");
            b
        };
        let mut file = b"<Width>32</Width>\r\n<Lakes>".to_vec();
        file.extend(lake(
            "N_L17",
            16023,
            &[(4096, 8192), (20480, 8192), (20480, 40960)],
        ));
        file.extend(lake("nowhere", 1843, &[(0, 0), (4096, 0), (0, 4096)]));
        file.extend(b"</Lakes>\r\n");
        let looks = vec![
            Look {
                name: "A5_Default".into(),
                color: [0.149, 0.176, 0.267, 0.7],
                max_depth: 0.9,
            },
            Look {
                name: "N_L17".into(),
                color: [0.2, 0.333, 0.333, 0.9],
                max_depth: 0.7,
            },
        ];
        let lakes = read(&file, &looks).unwrap();
        assert_eq!(
            lakes,
            vec![
                Lake {
                    look: 1,
                    height: 16023. / 4096.,
                    outline: vec![[1., 2.], [5., 2.], [5., 10.]],
                },
                Lake {
                    look: 0,
                    height: 1843. / 4096.,
                    outline: vec![[0., 0.], [1., 0.], [0., 1.]],
                },
            ]
        );
        assert_eq!(triangles(&lakes).len(), 6);
    }

    #[test]
    fn lake_settings_read_by_name() {
        let xml = "<LakeSettings><LakeSetting><Name>N_L17</Name>\
            <Dystopia><r>0.14</r><g>0.18</g><b>0.19</b><a>0.9</a></Dystopia>\
            <Color><r>0.2</r><g>0.3333333</g><b>0.3333333</b><a>0.9</a></Color>\
            <Utopia><r>0.27</r><g>0.41</g><b>0.46</b><a>0.9</a></Utopia>\
            <MaxDepth>0.7</MaxDepth></LakeSetting></LakeSettings>";
        assert_eq!(
            looks(xml),
            vec![Look {
                name: "N_L17".into(),
                color: [0.2, 0.3333333, 0.3333333, 0.9],
                max_depth: 0.7,
            }]
        );
    }
}
