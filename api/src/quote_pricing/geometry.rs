//! The parts of the CRM's quote geometry (`utils/quote/geometry.ts`) that
//! pricing needs.
//!
//! A counter's outline, its billed area, the lines its edges are priced
//! along and the splash / apron strips cut beside it.

use super::doc::{Bump, BumpPosition, CornerKind, CornerMod, Counter, Edge, EdgeType, LineEdge};
use std::f64::consts::PI;

const EPS: f64 = 1e-9;
const ANGLE_STEP_DEG: f64 = 22.5;
const STRIP_GAP: f64 = 1.0;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pt {
    pub x: f64,
    pub y: f64,
}

const fn pt(x: f64, y: f64) -> Pt {
    Pt { x, y }
}

fn add(a: Pt, b: Pt, k: f64) -> Pt {
    pt(a.x + b.x * k, a.y + b.y * k)
}

fn sub(a: Pt, b: Pt) -> Pt {
    pt(a.x - b.x, a.y - b.y)
}

pub fn dist(a: Pt, b: Pt) -> f64 {
    (a.x - b.x).hypot(a.y - b.y)
}

fn dot(a: Pt, b: Pt) -> f64 {
    a.x * b.x + a.y * b.y
}

fn cross(a: Pt, b: Pt) -> f64 {
    a.x * b.y - a.y * b.x
}

fn norm(a: Pt) -> Pt {
    let l = a.x.hypot(a.y);
    let l = if l == 0.0 || l.is_nan() { 1.0 } else { l };
    pt(a.x / l, a.y / l)
}

fn mid(a: Pt, b: Pt) -> Pt {
    pt((a.x + b.x) / 2.0, (a.y + b.y) / 2.0)
}

fn step_dir(steps: i64) -> Pt {
    #[allow(clippy::cast_precision_loss)]
    let a = steps as f64 * ANGLE_STEP_DEG * PI / 180.0;
    let (x, y) = (a.cos(), a.sin());
    pt(
        if x.abs() < EPS { 0.0 } else { x },
        if y.abs() < EPS { 0.0 } else { y },
    )
}

const fn left_normal(d: Pt) -> Pt {
    pt(d.y, -d.x)
}

fn intersect_lines(p: Pt, d: Pt, q: Pt, e: Pt) -> Option<Pt> {
    let den = cross(d, e);
    if den.abs() < EPS {
        return None;
    }
    let t = cross(sub(q, p), e) / den;
    Some(add(p, d, t))
}

const fn norm_turn(steps: i64) -> i64 {
    let t = steps.rem_euclid(16);
    if t > 8 { t - 16 } else { t }
}

fn run_angles(counter: &Counter) -> Vec<i64> {
    let mut a = counter.angle;
    counter
        .runs
        .iter()
        .enumerate()
        .map(|(i, run)| {
            if i > 0 {
                a += run.turn;
            }
            a
        })
        .collect()
}

fn measured_side_left(counter: &Counter, i: usize) -> bool {
    let n = counter.runs.len();
    if n == 1 {
        return true;
    }
    let t = if i < n - 1 {
        counter.runs[i + 1].turn
    } else {
        counter.runs[i].turn
    };
    norm_turn(t) > 0
}

/// `baseShape(counter).points`: the polygon without corner or edge shapes.
pub fn base_shape(counter: &Counter) -> Vec<Pt> {
    let runs = &counter.runs;
    let n = runs.len();
    let dirs: Vec<Pt> = run_angles(counter).into_iter().map(step_dir).collect();
    let anchor = pt(counter.x, counter.y);
    let n0 = left_normal(dirs[0]);
    let mut left = vec![add(anchor, n0, runs[0].depth / 2.0)];
    let mut right = vec![add(anchor, n0, -runs[0].depth / 2.0)];
    for i in 0..n {
        let d = dirs[i];
        let run = &runs[i];
        let side_left = measured_side_left(counter, i);
        let start = if side_left {
            left[left.len() - 1]
        } else {
            right[right.len() - 1]
        };
        let end = add(start, d, run.length);
        let nn = left_normal(d);
        if i == n - 1 {
            if side_left {
                left.push(end);
                right.push(add(end, nn, -run.depth));
            } else {
                right.push(end);
                left.push(add(end, nn, run.depth));
            }
            break;
        }
        let d2 = dirs[i + 1];
        let next = &runs[i + 1];
        let nn2 = left_normal(d2);
        if side_left {
            let rj = intersect_lines(add(end, nn, -run.depth), d, add(end, nn2, -next.depth), d2)
                .unwrap_or_else(|| add(end, nn, -run.depth));
            left.push(end);
            right.push(rj);
        } else {
            let lj = intersect_lines(add(end, nn, run.depth), d, add(end, nn2, next.depth), d2)
                .unwrap_or_else(|| add(end, nn, run.depth));
            left.push(lj);
            right.push(end);
        }
    }
    right.reverse();
    left.extend(right);
    left
}

fn corner_mod_at(counter: &Counter, vertex: usize) -> Option<&CornerMod> {
    let n = counter.runs.len();
    let (slot, side) = if vertex <= n {
        (vertex, 0)
    } else {
        (2 * n + 1 - vertex, 1)
    };
    let pair = counter.corners.get(slot)?;
    if side == 0 {
        pair.0.as_ref()
    } else {
        pair.1.as_ref()
    }
}

fn polygon_signed_area(points: &[Pt]) -> f64 {
    let mut a = 0.0;
    for (i, p) in points.iter().enumerate() {
        let q = points[(i + 1) % points.len()];
        a += p.x * q.y - q.x * p.y;
    }
    a / 2.0
}

pub fn polygon_area(points: &[Pt]) -> f64 {
    polygon_signed_area(points).abs()
}

/// True for a polygon clockwise on screen (`orientation` 1).
fn clockwise(points: &[Pt]) -> bool {
    polygon_signed_area(points) >= 0.0
}

fn outward_normal(a: Pt, b: Pt, cw: bool) -> Pt {
    let d = norm(sub(b, a));
    if cw { left_normal(d) } else { pt(-d.y, d.x) }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Segment {
    Line {
        from: Pt,
        to: Pt,
    },
    Arc {
        from: Pt,
        to: Pt,
        center: Pt,
        radius: f64,
        sweep: bool,
    },
}

impl Segment {
    const fn line(from: Pt, to: Pt) -> Self {
        Self::Line { from, to }
    }

    const fn from(&self) -> Pt {
        match self {
            Self::Line { from, .. } | Self::Arc { from, .. } => *from,
        }
    }

    const fn to(&self) -> Pt {
        match self {
            Self::Line { to, .. } | Self::Arc { to, .. } => *to,
        }
    }

    const fn is_arc(&self) -> bool {
        matches!(self, Self::Arc { .. })
    }
}

fn arc_sweep(from: Pt, to: Pt, center: Pt, sweep: bool) -> (f64, f64) {
    let a0 = (from.y - center.y).atan2(from.x - center.x);
    let a1 = (to.y - center.y).atan2(to.x - center.x);
    let mut s = a1 - a0;
    if sweep {
        while s < 0.0 {
            s += PI * 2.0;
        }
    } else {
        while s > 0.0 {
            s -= PI * 2.0;
        }
    }
    (a0, s)
}

pub fn segment_length(s: &Segment) -> f64 {
    match *s {
        Segment::Line { from, to } => dist(from, to),
        Segment::Arc {
            from,
            to,
            center,
            radius,
            sweep,
        } => arc_sweep(from, to, center, sweep).1.abs() * radius,
    }
}

fn sample_segment(s: &Segment) -> Vec<Pt> {
    const STEPS: u32 = 16;
    match *s {
        Segment::Line { from, .. } => vec![from],
        Segment::Arc {
            from,
            to,
            center,
            radius,
            sweep,
        } => {
            let (a0, sw) = arc_sweep(from, to, center, sweep);
            (0..STEPS)
                .map(|k| {
                    let a = a0 + sw * f64::from(k) / f64::from(STEPS);
                    pt(center.x + a.cos() * radius, center.y + a.sin() * radius)
                })
                .collect()
        }
    }
}

fn arc_by_sagitta(a: Pt, b: Pt, sagitta: f64, bulge: Pt) -> Segment {
    let chord = dist(a, b);
    if sagitta < EPS || chord < EPS {
        return Segment::line(a, b);
    }
    let r = (sagitta * sagitta + chord * chord / 4.0) / (2.0 * sagitta);
    let m = mid(a, b);
    let center = add(m, bulge, sagitta - r);
    let apex = add(m, bulge, sagitta);
    let sweep = cross(sub(a, center), sub(apex, center)) > 0.0;
    Segment::Arc {
        from: a,
        to: b,
        center,
        radius: r,
        sweep,
    }
}

fn fillet_arc(p0: Pt, p1: Pt, v: Pt, r: f64) -> Segment {
    let m = mid(p0, p1);
    let to_v = norm(sub(v, m));
    let half = dist(p0, p1) / 2.0;
    let h = js_max(0.0, r * r - half * half).sqrt();
    let center = add(m, to_v, -h);
    let sweep = cross(sub(p0, center), sub(p1, center)) > 0.0;
    Segment::Arc {
        from: p0,
        to: p1,
        center,
        radius: r,
        sweep,
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct Trim {
    prev_cut: f64,
    next_cut: f64,
}

fn corner_angle(prev: Pt, v: Pt, next: Pt) -> f64 {
    let a = norm(sub(prev, v));
    let b = norm(sub(next, v));
    dot(a, b).clamp(-1.0, 1.0).acos()
}

/// JS `x || 1` for a number.
fn or_one(x: f64) -> f64 {
    if x == 0.0 || x.is_nan() { 1.0 } else { x }
}

fn corner_trim(m: &CornerMod, alpha: f64) -> Trim {
    match m.kind {
        CornerKind::Clip | CornerKind::InsideDiagonal | CornerKind::RecessedDiagonal => {
            let leg = m.size / or_one(2.0 * (alpha / 2.0).sin());
            Trim {
                prev_cut: leg,
                next_cut: leg,
            }
        }
        CornerKind::Radius | CornerKind::InsideRadius => {
            let t = m.size / or_one(alpha / 2.0).tan();
            Trim {
                prev_cut: t,
                next_cut: t,
            }
        }
        CornerKind::RoundNotch => Trim {
            prev_cut: m.size,
            next_cut: m.size,
        },
        CornerKind::Notch => Trim {
            prev_cut: m.length,
            next_cut: m.depth,
        },
        CornerKind::BumpOut => Trim {
            prev_cut: m.length,
            next_cut: 0.0,
        },
    }
}

fn corner_segments(m: &CornerMod, prev: Pt, v: Pt, next: Pt, trim: Trim) -> Vec<Segment> {
    let ep = norm(sub(prev, v));
    let en = norm(sub(next, v));
    let p0 = add(v, ep, trim.prev_cut);
    let p1 = add(v, en, trim.next_cut);
    match m.kind {
        CornerKind::Clip | CornerKind::InsideDiagonal => vec![Segment::line(p0, p1)],
        CornerKind::RecessedDiagonal => {
            let n = norm(sub(v, mid(p0, p1)));
            let q0 = add(p0, n, dot(sub(v, p0), n) + m.depth);
            let q1 = add(p1, n, dot(sub(v, p1), n) + m.depth);
            vec![
                Segment::line(p0, q0),
                Segment::line(q0, q1),
                Segment::line(q1, p1),
            ]
        }
        CornerKind::Radius | CornerKind::InsideRadius => vec![fillet_arc(p0, p1, v, m.size)],
        CornerKind::RoundNotch => {
            let sweep = cross(sub(p0, v), sub(p1, v)) > 0.0;
            vec![Segment::Arc {
                from: p0,
                to: p1,
                center: v,
                radius: m.size,
                sweep,
            }]
        }
        CornerKind::Notch => {
            let inner = add(p0, en, m.depth);
            vec![Segment::line(p0, inner), Segment::line(inner, p1)]
        }
        CornerKind::BumpOut => {
            let out0 = add(p0, en, -m.depth);
            let out1 = add(v, en, -m.depth);
            vec![
                Segment::line(p0, out0),
                Segment::line(out0, out1),
                Segment::line(out1, v),
            ]
        }
    }
}

fn flare(bump: &Bump, steps: i64) -> f64 {
    if bump.arc {
        return 0.0;
    }
    #[allow(clippy::cast_precision_loss)]
    let a = steps as f64 * ANGLE_STEP_DEG * PI / 180.0;
    bump.depth * a.tan()
}

fn bump_segments(a: Pt, b: Pt, out: Pt, bump: &Bump) -> Vec<Segment> {
    let u = norm(sub(b, a));
    let len = dist(a, b);
    let sign = if bump.inward { -1.0 } else { 1.0 };
    let flare_l = flare(bump, bump.left_angle);
    let flare_r = flare(bump, bump.right_angle);
    let base_width = bump.width + flare_l + flare_r;
    let start = match bump.position {
        BumpPosition::Left => bump.distance,
        BumpPosition::Right => len - bump.distance - base_width,
        BumpPosition::Center => (len - base_width) / 2.0,
    };
    // Math.max(0, Math.min(len - baseWidth, start))
    let start = js_max(0.0, js_min(len - base_width, start));
    let b0 = add(a, u, start);
    let b1 = add(a, u, start + base_width);
    if bump.arc {
        return vec![
            Segment::line(a, b0),
            arc_by_sagitta(b0, b1, bump.depth, pt(out.x * sign, out.y * sign)),
            Segment::line(b1, b),
        ];
    }
    let f0 = add(add(b0, u, flare_l), out, bump.depth * sign);
    let f1 = add(add(b1, u, -flare_r), out, bump.depth * sign);
    vec![
        Segment::line(a, b0),
        Segment::line(b0, f0),
        Segment::line(f0, f1),
        Segment::line(f1, b1),
        Segment::line(b1, b),
    ]
}

const fn js_min(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        f64::NAN
    } else {
        a.min(b)
    }
}

const fn js_max(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        f64::NAN
    } else {
        a.max(b)
    }
}

fn edge_segments(a: Pt, b: Pt, out: Pt, full_radius: f64, bumps: &[Bump]) -> Vec<Segment> {
    if full_radius > 0.0 {
        return vec![arc_by_sagitta(a, b, full_radius, out)];
    }
    if bumps.is_empty() {
        return vec![Segment::line(a, b)];
    }
    let mut segs = vec![Segment::line(a, b)];
    for bump in bumps {
        let mut next = Vec::new();
        let mut placed = false;
        for s in segs {
            match s {
                Segment::Line { from, to } if !placed && dist(from, to) >= bump.width => {
                    next.extend(bump_segments(from, to, out, bump));
                    placed = true;
                }
                other => next.push(other),
            }
        }
        segs = next;
    }
    segs.retain(|s| s.is_arc() || dist(s.from(), s.to()) > EPS);
    segs
}

pub struct OutlineEdge {
    pub segments: Vec<Segment>,
    pub length: f64,
}

pub struct CornerPiece {
    pub vertex: usize,
    pub kind: CornerKind,
    pub size: f64,
    pub length: f64,
    pub depth: f64,
    pub segments: Vec<Segment>,
}

pub struct Outline {
    pub edges: Vec<OutlineEdge>,
    pub corners: Vec<CornerPiece>,
    pub polygon: Vec<Pt>,
}

impl Outline {
    fn corner_at(&self, vertex: usize) -> Option<&CornerPiece> {
        self.corners.iter().find(|c| c.vertex == vertex)
    }
}

fn trims(counter: &Counter, points: &[Pt]) -> Vec<Trim> {
    let m = points.len();
    (0..m)
        .map(|vi| {
            corner_mod_at(counter, vi).map_or_else(Trim::default, |md| {
                corner_trim(
                    md,
                    corner_angle(points[(vi + m - 1) % m], points[vi], points[(vi + 1) % m]),
                )
            })
        })
        .collect()
}

pub fn counter_outline(counter: &Counter) -> Outline {
    let points = base_shape(counter);
    let m = points.len();
    let cw = clockwise(&points);
    let trims = trims(counter, &points);
    let edges: Vec<OutlineEdge> = (0..m)
        .map(|k| {
            let a = points[k];
            let b = points[(k + 1) % m];
            let u = norm(sub(b, a));
            let a2 = add(a, u, trims[k].next_cut);
            let b2 = add(b, u, -trims[(k + 1) % m].prev_cut);
            let out = outward_normal(a, b, cw);
            let segments = counter.edges.get(k).map_or_else(
                || vec![Segment::line(a2, b2)],
                |edge| edge_segments(a2, b2, out, edge.full_radius, &edge.bumps),
            );
            let length = segments.iter().map(segment_length).sum();
            OutlineEdge { segments, length }
        })
        .collect();
    let corners: Vec<CornerPiece> = (0..m)
        .filter_map(|vi| {
            let md = corner_mod_at(counter, vi)?;
            let segments = corner_segments(
                md,
                points[(vi + m - 1) % m],
                points[vi],
                points[(vi + 1) % m],
                trims[vi],
            );
            Some(CornerPiece {
                vertex: vi,
                kind: md.kind,
                size: md.size,
                length: md.length,
                depth: md.depth,
                segments,
            })
        })
        .collect();
    let mut polygon = Vec::new();
    for (k, edge) in edges.iter().enumerate() {
        if let Some(corner) = corners.iter().find(|c| c.vertex == k) {
            for s in &corner.segments {
                polygon.extend(sample_segment(s));
            }
        }
        for s in &edge.segments {
            polygon.extend(sample_segment(s));
        }
    }
    Outline {
        edges,
        corners,
        polygon: dedupe(polygon),
    }
}

fn dedupe(pts: Vec<Pt>) -> Vec<Pt> {
    let mut out: Vec<Pt> = Vec::with_capacity(pts.len());
    for p in pts {
        if out.last().is_none_or(|last| dist(*last, p) > 1e-6) {
            out.push(p);
        }
    }
    if out.len() > 1 && dist(out[0], out[out.len() - 1]) < 1e-6 {
        out.pop();
    }
    out
}

/// Square inches billed for material: the uncut blank (see the CRM).
pub fn material_area_sq_in(counter: &Counter) -> f64 {
    let points = base_shape(counter);
    let m = points.len();
    let mut area = polygon_area(&points);
    for k in 0..m {
        let Some(edge) = counter.edges.get(k) else {
            continue;
        };
        let len = dist(points[k], points[(k + 1) % m]);
        let mut outward = edge.full_radius;
        for b in &edge.bumps {
            if !b.inward {
                outward = js_max(outward, b.depth);
            }
        }
        area += len * outward;
    }
    for vi in 0..m {
        let Some(md) = corner_mod_at(counter, vi) else {
            continue;
        };
        if !matches!(
            md.kind,
            CornerKind::InsideDiagonal | CornerKind::InsideRadius
        ) {
            continue;
        }
        let alpha = corner_angle(points[(vi + m - 1) % m], points[vi], points[(vi + 1) % m]);
        let t = corner_trim(md, alpha);
        area += t.prev_cut * t.next_cut * alpha.sin();
    }
    area
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    Edge,
    Corner,
}

#[derive(Debug, Clone, Copy)]
pub struct LineSource {
    pub kind: SourceKind,
    pub index: usize,
}

pub struct OutlineLine {
    pub key: String,
    pub segments: Vec<Segment>,
    pub sources: Vec<LineSource>,
    pub length: f64,
    pub edge_index: Option<usize>,
    pub vertex: Option<usize>,
}

fn same_line(a: &Segment, b: &Segment) -> bool {
    let (Segment::Line { from: af, to: at }, Segment::Line { from: bf, to: bt }) = (a, b) else {
        return false;
    };
    if dist(*at, *bf) > 1e-6 {
        return false;
    }
    let da = norm(sub(*at, *af));
    let db = norm(sub(*bt, *bf));
    cross(da, db).abs() < 1e-6 && dot(da, db) > 0.0
}

pub fn counter_lines(outline: &Outline) -> Vec<OutlineLine> {
    let mut items: Vec<(Segment, LineSource, usize)> = Vec::new();
    for (k, edge) in outline.edges.iter().enumerate() {
        if let Some(corner) = outline.corner_at(k) {
            for (i, seg) in corner.segments.iter().enumerate() {
                items.push((
                    *seg,
                    LineSource {
                        kind: SourceKind::Corner,
                        index: k,
                    },
                    i,
                ));
            }
        }
        for (i, seg) in edge.segments.iter().enumerate() {
            items.push((
                *seg,
                LineSource {
                    kind: SourceKind::Edge,
                    index: k,
                },
                i,
            ));
        }
    }
    items.retain(|(seg, ..)| seg.is_arc() || dist(seg.from(), seg.to()) > 1e-6);
    let mut groups: Vec<Vec<(Segment, LineSource, usize)>> = Vec::new();
    for it in items {
        match groups.last_mut() {
            Some(last) if same_line(&last[last.len() - 1].0, &it.0) => last.push(it),
            _ => groups.push(vec![it]),
        }
    }
    if groups.len() > 1 {
        let joined = {
            let last = &groups[groups.len() - 1];
            same_line(&last[last.len() - 1].0, &groups[0][0].0)
        };
        if joined {
            let mut last = groups.pop().unwrap_or_default();
            last.append(&mut groups[0]);
            groups[0] = last;
        }
    }
    groups
        .into_iter()
        .map(|g| {
            let edge = g.iter().find(|x| x.1.kind == SourceKind::Edge);
            let primary = edge.unwrap_or(&g[0]);
            let key = format!(
                "{}{}.{}",
                if primary.1.kind == SourceKind::Edge {
                    'e'
                } else {
                    'c'
                },
                primary.1.index,
                primary.2
            );
            let edge_index = edge.map(|e| e.1.index);
            let vertex = if edge.is_some() {
                None
            } else {
                Some(primary.1.index)
            };
            let segments: Vec<Segment> = g.iter().map(|x| x.0).collect();
            OutlineLine {
                key,
                length: segments.iter().map(segment_length).sum(),
                sources: g.iter().map(|x| x.1).collect(),
                segments,
                edge_index,
                vertex,
            }
        })
        .collect()
}

pub const fn is_strip_type(kind: EdgeType) -> bool {
    kind.is_strip()
}

pub fn strip_height(edge: Option<&Edge>) -> f64 {
    match edge {
        Some(e) if e.kind == EdgeType::Splash => e.splash_height,
        Some(e) if matches!(e.kind, EdgeType::Waterfall | EdgeType::Mitered) => e.miter_height,
        _ => 0.0,
    }
}

/// The edge a line has, and whether it was set on the line itself.
#[derive(Debug, Clone, PartialEq)]
pub struct LineEdgeInfo {
    pub kind: EdgeType,
    pub profile: i64,
    pub height: f64,
    pub own: bool,
}

fn default_line_edge(counter: &Counter, line: &OutlineLine) -> LineEdgeInfo {
    let info = |kind, profile, height| LineEdgeInfo {
        kind,
        profile,
        height,
        own: false,
    };
    if let Some(k) = line.edge_index {
        return counter.edges.get(k).map_or_else(
            || info(EdgeType::Finished, 0, 0.0),
            |e| info(e.kind, e.profile, strip_height(Some(e))),
        );
    }
    let v = line.vertex.unwrap_or(0);
    let m = counter.edges.len();
    let md = corner_mod_at(counter, v);
    let near = [counter.edges.get((v + m - 1) % m), counter.edges.get(v)];
    let finished = near.iter().flatten().find(|e| e.kind == EdgeType::Finished);
    if let Some(kind) = md.and_then(|md| md.edge_type) {
        let height = if is_strip_type(kind) {
            md.and_then(|md| md.edge_height).unwrap_or(0.0)
        } else {
            0.0
        };
        return info(kind, finished.map_or(0, |e| e.profile), height);
    }
    if let Some(e) = finished {
        return info(EdgeType::Finished, e.profile, 0.0);
    }
    if let Some(e) = near
        .iter()
        .flatten()
        .find(|e| matches!(e.kind, EdgeType::Unfinished | EdgeType::Appliance))
    {
        return info(e.kind, 0, 0.0);
    }
    let mitered = near.iter().flatten().find(|e| e.kind == EdgeType::Mitered);
    info(EdgeType::Finished, mitered.map_or(0, |e| e.profile), 0.0)
}

pub fn line_edge_of(counter: &Counter, line: &OutlineLine) -> LineEdgeInfo {
    counter
        .line_edges
        .as_ref()
        .and_then(|own| own.get(&line.key))
        .map_or_else(
            || default_line_edge(counter, line),
            |own: &LineEdge| LineEdgeInfo {
                kind: own.kind,
                profile: own.profile,
                height: own.height,
                own: true,
            },
        )
}

pub fn same_line_edge(a: &LineEdgeInfo, b: &LineEdgeInfo) -> bool {
    if a.kind != b.kind {
        return false;
    }
    if matches!(a.kind, EdgeType::Finished | EdgeType::Mitered) && a.profile != b.profile {
        return false;
    }
    !is_strip_type(a.kind) || (a.height - b.height).abs() < 1e-6
}

/// Signed area a bump adds to its piece (bump-ins take it away), in sq in.
pub fn bump_area(bump: &Bump) -> f64 {
    let area = if bump.arc {
        2.0 / 3.0 * bump.width * bump.depth
    } else {
        bump.depth
            * (bump.width + (flare(bump, bump.left_angle) + flare(bump, bump.right_angle)) / 2.0)
    };
    if bump.inward { -area } else { area }
}

fn offset_polyline(pts: &[Pt], cw: bool, d: f64) -> Vec<Pt> {
    let lines: Vec<(Pt, Pt)> = pts
        .windows(2)
        .map(|w| {
            (
                add(w[0], outward_normal(w[0], w[1], cw), d),
                sub(w[1], w[0]),
            )
        })
        .collect();
    (0..pts.len())
        .map(|i| {
            if i == 0 {
                return lines[0].0;
            }
            let prev = lines[i - 1];
            if i == pts.len() - 1 {
                return add(prev.0, prev.1, 1.0);
            }
            let next = lines[i];
            intersect_lines(prev.0, prev.1, next.0, next.1).unwrap_or(next.0)
        })
        .collect()
}

fn path_of_segments(segs: &[Segment]) -> Vec<Pt> {
    let Some(last) = segs.last() else {
        return Vec::new();
    };
    let mut pts: Vec<Pt> = segs.iter().flat_map(sample_segment).collect();
    pts.push(last.to());
    dedupe(pts)
}

/// The polygons a strip is cut as (`cutPieces`, or its outline sampled).
fn strip_polygons(
    path: &[Pt],
    cw: bool,
    h: f64,
    before: Option<Pt>,
    after: Option<Pt>,
    edge: Option<&Edge>,
    joins: &[usize],
) -> Vec<Vec<Pt>> {
    let mut ext: Vec<Pt> = before.into_iter().collect();
    ext.extend_from_slice(path);
    ext.extend(after);
    let from = usize::from(before.is_some());
    let to = ext.len() - usize::from(after.is_some());
    let inner = offset_polyline(&ext, cw, STRIP_GAP)[from..to].to_vec();
    let outer = offset_polyline(&ext, cw, STRIP_GAP + h)[from..to].to_vec();

    if let Some(edge) = edge
        && path.len() == 2
        && !edge.splash_bumps.is_empty()
    {
        let out = outward_normal(path[0], path[1], cw);
        let (i0, i1, o0, o1) = (inner[0], inner[1], outer[0], outer[1]);
        let mut segments = vec![Segment::line(i0, o0)];
        segments.extend(edge_segments(o0, o1, out, 0.0, &edge.splash_bumps));
        segments.push(Segment::line(o1, i1));
        segments.push(Segment::line(i1, i0));
        return vec![segments.iter().flat_map(sample_segment).collect()];
    }
    let mut cuts = vec![0];
    cuts.extend(
        joins
            .iter()
            .copied()
            .filter(|k| *k > 0 && *k < path.len() - 1),
    );
    cuts.push(path.len() - 1);
    if cuts.len() > 2 {
        return cuts
            .windows(2)
            .map(|w| {
                let (start, end) = (w[0], w[1]);
                let mut piece = inner[start..=end].to_vec();
                piece.extend(outer[start..=end].iter().rev());
                piece
            })
            .collect();
    }
    let mut ring = inner;
    ring.extend(outer.iter().rev());
    vec![ring]
}

struct Part<'a> {
    kind: SourceKind,
    index: usize,
    path: Vec<Pt>,
    h: f64,
    edge_type: EdgeType,
    edge: Option<&'a Edge>,
    joins: Vec<usize>,
}

fn touches(a: Pt, b: Pt) -> bool {
    dist(a, b) < 0.01
}

fn dir_in(q: &Part) -> Pt {
    norm(sub(q.path[q.path.len() - 1], q.path[q.path.len() - 2]))
}

fn dir_out(q: &Part) -> Pt {
    norm(sub(q.path[1], q.path[0]))
}

fn join<'a>(mut a: Part<'a>, b: &Part) -> Part<'a> {
    let offset = a.path.len() - 1;
    a.joins.push(offset);
    a.joins.extend(b.joins.iter().map(|j| j + offset));
    a.path.extend_from_slice(&b.path[1..]);
    a
}

/// The pieces every splash, waterfall and mitered apron of a counter is cut
/// as (`counterStrips` → `basePieces`), as polygons.
pub fn strip_pieces(counter: &Counter, outline: &Outline) -> Vec<Vec<Pt>> {
    let cw = clockwise(&base_shape(counter));
    let mut parts: Vec<Part> = Vec::new();
    for line in counter_lines(outline) {
        let info = line_edge_of(counter, &line);
        let kind = if line.edge_index.is_some() {
            SourceKind::Edge
        } else {
            SourceKind::Corner
        };
        let index = line.edge_index.or(line.vertex).unwrap_or(0);
        let h = if is_strip_type(info.kind) {
            info.height
        } else {
            0.0
        };
        let path = path_of_segments(&line.segments);
        if let Some(last) = parts.last_mut()
            && last.kind == kind
            && last.index == index
            && last.edge_type == info.kind
            && (last.h - h).abs() < 1e-6
        {
            last.path
                .extend_from_slice(path.get(1..).unwrap_or_default());
            continue;
        }
        parts.push(Part {
            kind,
            index,
            path,
            h,
            edge_type: info.kind,
            edge: if kind == SourceKind::Edge && !info.own {
                counter.edges.get(index)
            } else {
                None
            },
            joins: Vec::new(),
        });
    }
    let joins_inside = |a: &Part, b: &Part| {
        if a.edge_type != EdgeType::Mitered || b.edge_type != EdgeType::Mitered {
            return false;
        }
        if !(a.h > 0.0) || (a.h - b.h).abs() > 0.01 {
            return false;
        }
        if a.edge.is_some_and(|e| !e.splash_bumps.is_empty())
            || b.edge.is_some_and(|e| !e.splash_bumps.is_empty())
        {
            return false;
        }
        if !touches(a.path[a.path.len() - 1], b.path[0]) {
            return false;
        }
        let out = outward_normal(a.path[a.path.len() - 2], a.path[a.path.len() - 1], cw);
        let d = dir_out(b);
        d.x * out.x + d.y * out.y > 0.01
    };
    let mut live: Vec<Part> = Vec::new();
    for p in parts.into_iter().filter(|q| q.path.len() >= 2) {
        if let Some(last) = live.pop() {
            if joins_inside(&last, &p) {
                live.push(join(last, &p));
            } else {
                live.push(last);
                live.push(p);
            }
        } else {
            live.push(p);
        }
    }
    if live.len() > 1 && joins_inside(&live[live.len() - 1], &live[0]) {
        let last = live.pop().expect("more than one part");
        let joined = join(last, &live[0]);
        live[0] = joined;
    }
    let n = live.len();
    let square = |a: Pt, b: Pt| (a.x * b.x + a.y * b.y).abs() < 0.02;
    let mut pieces = Vec::new();
    for i in 0..n {
        let p = &live[i];
        if !(p.h > 0.0) {
            continue;
        }
        let prev = &live[(i + n - 1) % n];
        let next = &live[(i + 1) % n];
        let joins_prev = (i + n - 1) % n != i
            && prev.h > 0.0
            && touches(prev.path[prev.path.len() - 1], p.path[0]);
        let joins_next =
            (i + 1) % n != i && next.h > 0.0 && touches(next.path[0], p.path[p.path.len() - 1]);
        let mut path = p.path.clone();
        let mut before = None;
        if joins_prev {
            let din = dir_in(prev);
            let dout = dir_out(p);
            if square(din, dout) {
                let prev_out = outward_normal(
                    prev.path[prev.path.len() - 2],
                    prev.path[prev.path.len() - 1],
                    cw,
                );
                let inside = dout.x * prev_out.x + dout.y * prev_out.y > 0.0;
                let skip = STRIP_GAP + prev.h;
                if inside && dist(path[0], path[1]) > skip {
                    path[0] = add(path[0], dout, skip);
                }
            } else {
                before = Some(prev.path[prev.path.len() - 2]);
            }
        }
        let after = (joins_next && !square(dir_in(p), dir_out(next))).then(|| next.path[1]);
        pieces.extend(strip_polygons(
            &path, cw, p.h, before, after, p.edge, &p.joins,
        ));
    }
    pieces
}
