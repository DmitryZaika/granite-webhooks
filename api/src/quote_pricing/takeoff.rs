//! Quantities of one area of a quote (`utils/quote/takeoff.ts`): material,
//! edges, strips, corners and cutouts, as the price lines need them.

use super::doc::{Bump, Counter, Cutout, DetailKind, EdgeType, OtherCounter, QuoteDoc, SinkType};
use super::geometry::{
    LineEdgeInfo, SourceKind, base_shape, bump_area, counter_lines, counter_outline, dist,
    line_edge_of, material_area_sq_in, same_line_edge, segment_length,
};

/// Edge profiles past this are not counted (a broken document, not a quote).
const MAX_PROFILE: i64 = 10_000;

#[derive(Debug, Clone, PartialEq)]
pub struct HeightQty {
    pub height: f64,
    pub sq_ft: f64,
    pub lin_ft: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CornerCount {
    pub kind: String,
    pub size: f64,
    pub length: f64,
    pub depth: f64,
    pub count: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EdgeModCount {
    pub kind: String,
    pub width: f64,
    pub count: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SinkCutoutCount {
    pub kind: SinkType,
    pub width: f64,
    pub count: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NamedCount {
    pub name: String,
    pub count: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WidthCount {
    pub width: f64,
    pub count: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OtherCutoutCount {
    pub name: String,
    pub price: Option<f64>,
    pub allow_discount: bool,
    pub count: f64,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct AreaTakeoff {
    pub counter_sq_ft: f64,
    pub splash: Vec<HeightQty>,
    pub waterfall: Vec<HeightQty>,
    pub waterfall_count: f64,
    pub mitered: Vec<HeightQty>,
    pub mitered_cuts_lin_ft: f64,
    pub miter_joint_lin_ft: f64,
    pub miter_count: f64,
    pub finished_lin_ft: Vec<f64>,
    pub appliance_lin_ft: f64,
    pub full_radius_lin_ft: f64,
    pub full_radius_count: f64,
    pub corners: Vec<CornerCount>,
    pub edge_mods: Vec<EdgeModCount>,
    pub sink_cutouts: Vec<SinkCutoutCount>,
    pub sink_models: Vec<NamedCount>,
    pub faucet_holes: f64,
    pub cooktops: Vec<WidthCount>,
    pub other_cutouts: Vec<OtherCutoutCount>,
    pub outlets: f64,
    pub fabrication_sq_ft: f64,
}

fn bump_by<T>(
    list: &mut Vec<T>,
    matches: impl Fn(&T) -> bool,
    create: impl FnOnce() -> T,
    inc: impl FnOnce(&mut T),
) {
    if let Some(found) = list.iter_mut().find(|x| matches(x)) {
        inc(found);
    } else {
        let mut x = create();
        inc(&mut x);
        list.push(x);
    }
}

#[allow(clippy::float_cmp)]
fn height_entry(list: &mut Vec<HeightQty>, height: f64, inc: impl FnOnce(&mut HeightQty)) {
    bump_by(
        list,
        |x| x.height == height,
        || HeightQty {
            height,
            sq_ft: 0.0,
            lin_ft: 0.0,
        },
        inc,
    );
}

fn add_strip_bumps(list: &mut Vec<HeightQty>, height: f64, bumps: &[Bump]) {
    let area: f64 = bumps.iter().map(bump_area).sum();
    if area == 0.0 || area.is_nan() {
        return;
    }
    height_entry(list, height, |x| x.sq_ft += area / 144.0);
}

fn add_height(list: &mut Vec<HeightQty>, height: f64, len_in: f64) {
    height_entry(list, height, |x| {
        x.sq_ft += len_in * height / 144.0;
        x.lin_ft += len_in / 12.0;
    });
}

fn add_finished(t: &mut AreaTakeoff, profile: i64, len_in: f64) {
    if !(0..=MAX_PROFILE).contains(&profile) {
        return;
    }
    let profile = usize::try_from(profile).unwrap_or_default();
    if t.finished_lin_ft.len() <= profile {
        t.finished_lin_ft.resize(profile + 1, 0.0);
    }
    t.finished_lin_ft[profile] += len_in / 12.0;
}

/// One edge's type as `addEdgeType` reads it.
struct EdgeKind<'a> {
    kind: EdgeType,
    profile: i64,
    splash_height: f64,
    miter_height: f64,
    splash_bumps: &'a [Bump],
}

fn add_edge_type(t: &mut AreaTakeoff, edge: &EdgeKind, straight: f64, path_len: f64) {
    match edge.kind {
        EdgeType::Finished => add_finished(t, edge.profile, path_len),
        EdgeType::Appliance => t.appliance_lin_ft += path_len / 12.0,
        EdgeType::Splash => {
            add_height(&mut t.splash, edge.splash_height, straight);
            add_strip_bumps(&mut t.splash, edge.splash_height, edge.splash_bumps);
        }
        EdgeType::Waterfall => {
            add_height(&mut t.waterfall, edge.miter_height, straight);
            add_strip_bumps(&mut t.waterfall, edge.miter_height, edge.splash_bumps);
            t.waterfall_count += 1.0;
            t.mitered_cuts_lin_ft += 2.0 * straight / 12.0;
            t.miter_joint_lin_ft += straight / 12.0;
            t.miter_count += 1.0;
        }
        EdgeType::Mitered => {
            add_height(&mut t.mitered, edge.miter_height, straight);
            add_strip_bumps(&mut t.mitered, edge.miter_height, edge.splash_bumps);
            t.mitered_cuts_lin_ft += 2.0 * straight / 12.0;
            t.miter_joint_lin_ft += straight / 12.0;
            t.miter_count += 1.0;
            add_finished(t, edge.profile, path_len);
        }
        EdgeType::Unfinished => {}
    }
}

fn add_line_groups(t: &mut AreaTakeoff, list: Vec<(LineEdgeInfo, f64)>) {
    let mut groups: Vec<(LineEdgeInfo, f64)> = Vec::new();
    for (le, len) in list {
        if let Some(g) = groups.iter_mut().find(|g| same_line_edge(&g.0, &le)) {
            g.1 += len;
        } else {
            groups.push((le, len));
        }
    }
    for (le, len) in groups {
        let edge = EdgeKind {
            kind: le.kind,
            profile: le.profile,
            splash_height: le.height,
            miter_height: le.height,
            splash_bumps: &[],
        };
        add_edge_type(t, &edge, len, len);
    }
}

#[allow(clippy::too_many_lines)]
fn add_counter(t: &mut AreaTakeoff, counter: &Counter) {
    t.counter_sq_ft += material_area_sq_in(counter) / 144.0;
    let points = base_shape(counter);
    let outline = counter_outline(counter);
    let m = points.len();
    let lines: Vec<_> = counter_lines(&outline)
        .into_iter()
        .map(|l| {
            let le = line_edge_of(counter, &l);
            (l, le)
        })
        .collect();

    for (k, edge) in counter.edges.iter().enumerate() {
        let Some(&a) = points.get(k) else { continue };
        let straight = dist(a, points[(k + 1) % m]);
        let path_len = outline.edges.get(k).map_or(straight, |e| e.length);
        let mine: Vec<_> = lines.iter().filter(|x| x.0.edge_index == Some(k)).collect();
        if mine.iter().any(|x| x.1.own) {
            add_line_groups(t, mine.iter().map(|x| (x.1.clone(), x.0.length)).collect());
        } else {
            let kind = EdgeKind {
                kind: edge.kind,
                profile: edge.profile,
                splash_height: edge.splash_height,
                miter_height: edge.miter_height,
                splash_bumps: &edge.splash_bumps,
            };
            add_edge_type(t, &kind, straight, path_len);
        }
        if edge.full_radius > 0.0 {
            t.full_radius_count += 1.0;
            t.full_radius_lin_ft += path_len / 12.0;
        }
        for b in &edge.bumps {
            let kind = match (b.arc, b.inward) {
                (true, true) => "bumpInArc",
                (true, false) => "bumpOutArc",
                (false, true) => "bumpIn",
                (false, false) => "bumpOut",
            };
            bump_by(
                &mut t.edge_mods,
                |x| x.kind == kind && x.width == b.width,
                || EdgeModCount {
                    kind: kind.into(),
                    width: b.width,
                    count: 0.0,
                },
                |x| x.count += 1.0,
            );
        }
    }

    for piece in &outline.corners {
        add_line_groups(
            t,
            lines
                .iter()
                .filter(|x| x.0.vertex == Some(piece.vertex))
                .map(|x| (x.1.clone(), x.0.length))
                .collect(),
        );
        for (l, le) in &lines {
            if l.edge_index.is_none() || le.own {
                continue;
            }
            let len: f64 = l
                .sources
                .iter()
                .enumerate()
                .filter(|(_, src)| src.kind == SourceKind::Corner && src.index == piece.vertex)
                .map(|(i, _)| segment_length(&l.segments[i]))
                .sum();
            if len == 0.0 || len.is_nan() {
                continue;
            }
            match le.kind {
                EdgeType::Finished => add_finished(t, le.profile, len),
                EdgeType::Appliance => t.appliance_lin_ft += len / 12.0,
                _ => {}
            }
        }
        let kind = piece.kind.as_str();
        bump_by(
            &mut t.corners,
            |x| {
                x.kind == kind
                    && x.size == piece.size
                    && x.length == piece.length
                    && x.depth == piece.depth
            },
            || CornerCount {
                kind: kind.into(),
                size: piece.size,
                length: piece.length,
                depth: piece.depth,
                count: 0.0,
            },
            |x| x.count += 1.0,
        );
    }

    for run in &counter.runs {
        for c in &run.cutouts {
            add_cutout(t, c);
        }
    }
    #[allow(clippy::cast_precision_loss)]
    {
        t.outlets += counter.outlets as f64;
    }
}

#[allow(clippy::float_cmp)]
fn add_sink(
    t: &mut AreaTakeoff,
    kind: SinkType,
    width: f64,
    model: Option<&str>,
    faucet_holes: f64,
) {
    bump_by(
        &mut t.sink_cutouts,
        |x| x.kind == kind && x.width == width,
        || SinkCutoutCount {
            kind,
            width,
            count: 0.0,
        },
        |x| x.count += 1.0,
    );
    if let Some(name) = model.filter(|m| !m.is_empty()) {
        bump_by(
            &mut t.sink_models,
            |x| x.name == name,
            || NamedCount {
                name: name.into(),
                count: 0.0,
            },
            |x| x.count += 1.0,
        );
    }
    t.faucet_holes += faucet_holes;
}

#[allow(clippy::float_cmp)]
fn add_cutout(t: &mut AreaTakeoff, c: &Cutout) {
    use super::doc::CutoutKind;
    match c.kind {
        CutoutKind::Sink => {
            #[allow(clippy::cast_precision_loss)]
            let holes = c.faucet_holes as f64;
            add_sink(t, c.sink_type, c.width, c.sink_model.as_deref(), holes);
        }
        CutoutKind::Cooktop => bump_by(
            &mut t.cooktops,
            |x| x.width == c.width,
            || WidthCount {
                width: c.width,
                count: 0.0,
            },
            |x| x.count += 1.0,
        ),
        CutoutKind::Other => bump_by(
            &mut t.other_cutouts,
            |x| x.name == c.name && x.price == c.price,
            || OtherCutoutCount {
                name: c.name.clone(),
                price: c.price,
                allow_discount: c.allow_discount,
                count: 0.0,
            },
            |x| x.count += 1.0,
        ),
    }
}

#[allow(clippy::float_cmp)]
fn add_other_counter(t: &mut AreaTakeoff, oc: &OtherCounter) {
    for s in &oc.sizes {
        t.counter_sq_ft += s.sqft.unwrap_or(s.length * s.width / 144.0);
    }
    for d in &oc.details {
        let q = if d.quantity == 0.0 || d.quantity.is_nan() {
            1.0
        } else {
            d.quantity
        };
        match d.kind {
            DetailKind::Splash => add_height(&mut t.splash, d.height, d.length * q),
            DetailKind::Edge => add_finished(t, d.profile, d.length * q),
            DetailKind::Appliance => t.appliance_lin_ft += d.length * q / 12.0,
            DetailKind::Waterfall => {
                add_height(&mut t.waterfall, d.height, d.length * q);
                t.waterfall_count += q;
                t.mitered_cuts_lin_ft += 2.0 * d.length * q / 12.0;
                t.miter_joint_lin_ft += d.length * q / 12.0;
                t.miter_count += q;
            }
            DetailKind::Mitered => {
                add_height(&mut t.mitered, d.height, d.length * q);
                t.mitered_cuts_lin_ft += 2.0 * d.length * q / 12.0;
                t.miter_joint_lin_ft += d.length * q / 12.0;
                t.miter_count += q;
            }
            DetailKind::Outlet => t.outlets += q,
            DetailKind::Corner => {
                let sized = d.reference == "notch" || d.reference == "bumpOut";
                let (size, length, depth) = if sized {
                    (0.0, d.length, d.depth)
                } else {
                    (d.length, 0.0, 0.0)
                };
                bump_by(
                    &mut t.corners,
                    |x| {
                        x.kind == d.reference
                            && x.size == size
                            && x.length == length
                            && x.depth == depth
                    },
                    || CornerCount {
                        kind: d.reference.clone(),
                        size,
                        length,
                        depth,
                        count: 0.0,
                    },
                    |x| x.count += q,
                );
            }
            DetailKind::EdgeMod => bump_by(
                &mut t.edge_mods,
                |x| x.kind == d.reference && x.width == d.width,
                || EdgeModCount {
                    kind: d.reference.clone(),
                    width: d.width,
                    count: 0.0,
                },
                |x| x.count += q,
            ),
            DetailKind::FaucetHole => t.faucet_holes += q,
            DetailKind::SinkCutout => {
                let width = if d.width == 0.0 || d.width.is_nan() {
                    30.0
                } else {
                    d.width
                };
                let mut i = 0.0;
                while i < q {
                    add_sink(t, d.sink_type, width, None, 0.0);
                    i += 1.0;
                }
            }
            DetailKind::SinkModel => bump_by(
                &mut t.sink_models,
                |x| x.name == d.reference,
                || NamedCount {
                    name: d.reference.clone(),
                    count: 0.0,
                },
                |x| x.count += q,
            ),
            DetailKind::Cooktop => bump_by(
                &mut t.cooktops,
                |x| x.width == d.width,
                || WidthCount {
                    width: d.width,
                    count: 0.0,
                },
                |x| x.count += q,
            ),
            DetailKind::OtherCutout => bump_by(
                &mut t.other_cutouts,
                |x| x.name == d.label,
                || OtherCutoutCount {
                    name: d.label.clone(),
                    price: None,
                    allow_discount: true,
                    count: 0.0,
                },
                |x| x.count += q,
            ),
        }
    }
}

pub fn area_takeoff(doc: &QuoteDoc, area_id: &str) -> AreaTakeoff {
    let mut t = AreaTakeoff::default();
    for c in doc.counters.iter().filter(|c| c.area_id == area_id) {
        add_counter(&mut t, c);
    }
    for oc in doc.other_counters.iter().filter(|c| c.area_id == area_id) {
        add_other_counter(&mut t, oc);
    }
    let extra: f64 = t.splash.iter().map(|x| x.sq_ft).sum::<f64>()
        + t.waterfall.iter().map(|x| x.sq_ft).sum::<f64>()
        + t.mitered.iter().map(|x| x.sq_ft).sum::<f64>();
    t.fabrication_sq_ft = t.counter_sq_ft + extra;
    t
}

/// JS `Math.round`: halves go up.
pub fn js_round(v: f64) -> f64 {
    let r = v.floor();
    if v - r >= 0.5 { r + 1.0 } else { r }
}

/// Moraware rounds material quantities to the nearest 0.1 ft.
pub fn round1(v: f64) -> f64 {
    js_round(v * 10.0 + 1e-9) / 10.0
}

#[derive(Debug, Clone, PartialEq)]
pub struct RoundedTakeoff {
    pub counter_sq_ft: f64,
    pub splash: Vec<HeightQty>,
    pub waterfall: Vec<HeightQty>,
    pub mitered: Vec<HeightQty>,
    pub fabrication_sq_ft: f64,
    pub finished_lin_ft: Vec<f64>,
    pub appliance_lin_ft: f64,
    pub mitered_cuts_lin_ft: f64,
    pub miter_joint_lin_ft: f64,
}

pub fn round_takeoff(t: &AreaTakeoff) -> RoundedTakeoff {
    let rounded = |list: &[HeightQty]| -> Vec<HeightQty> {
        list.iter()
            .map(|x| HeightQty {
                height: x.height,
                sq_ft: round1(x.sq_ft),
                lin_ft: round1(x.lin_ft),
            })
            .collect()
    };
    let counter_sq_ft = round1(t.counter_sq_ft);
    let splash = rounded(&t.splash);
    let waterfall = rounded(&t.waterfall);
    let mitered = rounded(&t.mitered);
    let mut sum = counter_sq_ft;
    for x in splash.iter().chain(&waterfall).chain(&mitered) {
        sum += x.sq_ft;
    }
    RoundedTakeoff {
        counter_sq_ft,
        fabrication_sq_ft: round1(sum),
        splash,
        waterfall,
        mitered,
        finished_lin_ft: t.finished_lin_ft.iter().copied().map(round1).collect(),
        appliance_lin_ft: round1(t.appliance_lin_ft),
        mitered_cuts_lin_ft: round1(t.mitered_cuts_lin_ft),
        miter_joint_lin_ft: round1(t.miter_joint_lin_ft),
    }
}
