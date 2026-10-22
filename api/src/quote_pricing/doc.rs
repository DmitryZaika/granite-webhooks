//! The quote document (`draft_drawing_json`, version 2).
//!
//! Parsed and checked like the CRM's zod schema (`schemas/quoteDoc.ts`): a
//! document that fails the schema prices as an empty quote there, so it
//! does here too.

use serde::Deserialize;
use std::collections::HashMap;

const fn default_true() -> bool {
    true
}
const fn default_one_f() -> f64 {
    1.0
}
const fn default_one_i() -> i64 {
    1
}
const fn default_burners() -> i64 {
    4
}
const fn default_depth() -> f64 {
    25.5
}
const fn default_rounding() -> f64 {
    0.0625
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum EdgeType {
    #[default]
    Finished,
    Unfinished,
    Appliance,
    Splash,
    Mitered,
    Waterfall,
}

impl EdgeType {
    pub const fn is_strip(self) -> bool {
        matches!(self, Self::Splash | Self::Waterfall | Self::Mitered)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum BumpPosition {
    #[default]
    Center,
    Left,
    Right,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Bump {
    #[serde(default)]
    pub inward: bool,
    #[serde(default)]
    pub arc: bool,
    pub width: f64,
    pub depth: f64,
    #[serde(default)]
    pub left_angle: i64,
    #[serde(default)]
    pub right_angle: i64,
    #[serde(default)]
    pub position: BumpPosition,
    #[serde(default)]
    pub distance: f64,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Edge {
    #[serde(rename = "type", default)]
    pub kind: EdgeType,
    #[serde(default)]
    pub profile: i64,
    #[serde(default)]
    pub splash_height: f64,
    #[serde(default)]
    pub miter_height: f64,
    #[serde(default)]
    pub full_radius: f64,
    #[serde(default)]
    pub bumps: Vec<Bump>,
    #[serde(default)]
    pub splash_bumps: Vec<Bump>,
}

impl Edge {
    pub const fn plain(kind: EdgeType) -> Self {
        Self {
            kind,
            profile: 0,
            splash_height: 0.0,
            miter_height: 0.0,
            full_radius: 0.0,
            bumps: Vec::new(),
            splash_bumps: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CornerKind {
    Radius,
    Clip,
    BumpOut,
    Notch,
    InsideRadius,
    InsideDiagonal,
    RecessedDiagonal,
    RoundNotch,
}

impl CornerKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Radius => "radius",
            Self::Clip => "clip",
            Self::BumpOut => "bumpOut",
            Self::Notch => "notch",
            Self::InsideRadius => "insideRadius",
            Self::InsideDiagonal => "insideDiagonal",
            Self::RecessedDiagonal => "recessedDiagonal",
            Self::RoundNotch => "roundNotch",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        [
            Self::Radius,
            Self::Clip,
            Self::BumpOut,
            Self::Notch,
            Self::InsideRadius,
            Self::InsideDiagonal,
            Self::RecessedDiagonal,
            Self::RoundNotch,
        ]
        .into_iter()
        .find(|kind| kind.as_str() == value)
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CornerMod {
    pub kind: CornerKind,
    #[serde(default)]
    pub size: f64,
    #[serde(default)]
    pub length: f64,
    #[serde(default)]
    pub depth: f64,
    pub edge_type: Option<EdgeType>,
    pub edge_height: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum SinkType {
    #[default]
    Undermount,
    DropIn,
    Farmer,
}

impl SinkType {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Undermount => "undermount",
            Self::DropIn => "dropIn",
            Self::Farmer => "farmer",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CutoutKind {
    Sink,
    Cooktop,
    Other,
}

const SINK_SHAPES: [&str; 19] = [
    "rectangle",
    "square",
    "rounded",
    "oval",
    "double",
    "60/40",
    "40/60",
    "70/30",
    "30/70",
    "double-square",
    "double-rounded",
    "60/40-square",
    "60/40-rounded",
    "40/60-square",
    "40/60-rounded",
    "70/30-square",
    "70/30-rounded",
    "30/70-square",
    "30/70-rounded",
];

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Cutout {
    pub id: String,
    pub kind: CutoutKind,
    #[serde(default)]
    pub anchor_corner: i64,
    #[serde(default)]
    pub along: f64,
    #[serde(default)]
    pub across: f64,
    #[serde(default)]
    pub turn: i64,
    pub width: f64,
    pub depth: f64,
    #[serde(default)]
    pub centerline: Option<String>,
    pub setback: Option<String>,
    #[serde(default)]
    pub sink_model: Option<String>,
    #[serde(default)]
    pub sink_type: SinkType,
    #[serde(default)]
    pub sink_shape: Option<String>,
    #[serde(default)]
    pub faucet_holes: i64,
    #[serde(default = "default_burners")]
    pub burners: i64,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub price: Option<f64>,
    #[serde(default = "default_true")]
    pub allow_discount: bool,
    #[serde(default)]
    pub oval: bool,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Run {
    pub length: f64,
    pub depth: f64,
    #[serde(default)]
    pub turn: i64,
    #[serde(default)]
    pub cutouts: Vec<Cutout>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LineEdge {
    #[serde(rename = "type")]
    pub kind: EdgeType,
    #[serde(default)]
    pub profile: i64,
    #[serde(default)]
    pub height: f64,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Counter {
    pub id: String,
    #[serde(default)]
    pub name: String,
    pub area_id: String,
    pub x: f64,
    pub y: f64,
    #[serde(default)]
    pub angle: i64,
    pub runs: Vec<Run>,
    pub edges: Vec<Edge>,
    pub corners: Vec<(Option<CornerMod>, Option<CornerMod>)>,
    #[serde(default)]
    pub outlets: i64,
    pub line_edges: Option<HashMap<String, LineEdge>>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct OtherCounterSize {
    #[serde(default)]
    pub sqft: Option<f64>,
    #[serde(default)]
    pub length: f64,
    #[serde(default)]
    pub width: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DetailKind {
    Splash,
    Edge,
    Appliance,
    Mitered,
    Waterfall,
    Corner,
    EdgeMod,
    SinkModel,
    SinkCutout,
    Cooktop,
    OtherCutout,
    Outlet,
    FaucetHole,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OtherCounterDetail {
    pub kind: DetailKind,
    #[serde(default)]
    pub label: String,
    #[serde(default = "default_one_f")]
    pub quantity: f64,
    #[serde(default)]
    pub length: f64,
    #[serde(default)]
    pub height: f64,
    #[serde(default)]
    pub profile: i64,
    #[serde(default, rename = "ref")]
    pub reference: String,
    #[serde(default)]
    pub width: f64,
    #[serde(default)]
    pub depth: f64,
    #[serde(default)]
    pub sink_type: SinkType,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OtherCounter {
    pub id: String,
    #[serde(default)]
    pub name: String,
    pub area_id: String,
    #[serde(default)]
    pub sizes: Vec<OtherCounterSize>,
    #[serde(default)]
    pub details: Vec<OtherCounterDetail>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TextNote {
    id: String,
    x: f64,
    y: f64,
    text: String,
    font: Option<String>,
    size: Option<f64>,
    bold: Option<bool>,
    italic: Option<bool>,
    color: Option<String>,
    background: Option<String>,
    turn: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Area {
    pub id: String,
    pub name: String,
    #[serde(default = "default_one_i")]
    pub quantity: i64,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Supplier {
    name: String,
    file: Option<String>,
    price: f64,
    unit: SupplierUnit,
    tax_pct: Option<f64>,
    markup_pct: Option<f64>,
    at: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum SupplierUnit {
    Sqft,
    Slab,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct AreaColor {
    #[serde(default)]
    pub product: String,
    #[serde(default)]
    pub color: String,
    #[serde(default)]
    pub edges: Vec<String>,
    #[serde(default)]
    pub other_price: Option<f64>,
    pub other_by_slab: Option<bool>,
    pub other_slab_size: Option<(f64, f64)>,
    supplier: Option<Supplier>,
    pub delivery: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ColorOption {
    pub id: String,
    #[serde(default)]
    pub by_area: HashMap<String, AreaColor>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Slab {
    pub id: String,
    pub width: f64,
    pub height: f64,
    pub image: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Placement {
    piece_id: String,
    slab_id: Option<String>,
    x: f64,
    y: f64,
    turn: Option<i64>,
    angle: Option<f64>,
    loose: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Seam {
    piece_id: String,
    x1: f64,
    y1: f64,
    x2: f64,
    y2: f64,
    auto: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Default)]
pub struct SlabLayout {
    #[serde(default)]
    show: bool,
    #[serde(default)]
    pub slabs: Vec<Slab>,
    #[serde(default)]
    placements: Vec<Placement>,
    #[serde(default)]
    seams: Vec<Seam>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ItemKind {
    PriceList,
    Misc,
    Text,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuoteItem {
    pub id: String,
    pub kind: ItemKind,
    #[serde(default)]
    pub area_id: Option<String>,
    pub name: String,
    #[serde(default)]
    pub option: Option<String>,
    #[serde(default)]
    pub catalog_id: Option<f64>,
    #[serde(default)]
    pub option_id: Option<String>,
    #[serde(default = "default_one_f")]
    pub quantity: f64,
    #[serde(default)]
    pub amount: Option<f64>,
    pub fill_base: Option<f64>,
    #[serde(default)]
    pub price: Option<f64>,
    #[serde(default)]
    pub hide_on_quote: bool,
    #[serde(default = "default_true")]
    pub allow_discount: bool,
    #[serde(default = "default_true")]
    pub taxable: bool,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuoteDoc {
    version: i64,
    #[serde(default = "default_depth")]
    pub default_depth: f64,
    #[serde(default)]
    pub counters: Vec<Counter>,
    #[serde(default)]
    pub other_counters: Vec<OtherCounter>,
    #[serde(default)]
    texts: Vec<TextNote>,
    #[serde(default)]
    page_breaks: Vec<f64>,
    pub areas: Vec<Area>,
    pub color_options: Vec<ColorOption>,
    #[serde(default)]
    pub layouts: HashMap<String, HashMap<String, SlabLayout>>,
    #[serde(default)]
    pub items: Vec<QuoteItem>,
    #[serde(default)]
    pub price_overrides: HashMap<String, HashMap<String, f64>>,
    #[serde(default = "default_rounding")]
    rounding: f64,
}

fn non_negative(values: &[f64]) -> bool {
    values.iter().all(|v| *v >= 0.0)
}

fn positive(values: &[f64]) -> bool {
    values.iter().all(|v| *v > 0.0)
}

impl Bump {
    fn valid(&self) -> bool {
        positive(&[self.width, self.depth])
            && (0..=3).contains(&self.left_angle)
            && (0..=3).contains(&self.right_angle)
            && self.distance >= 0.0
    }
}

impl Edge {
    fn valid(&self) -> bool {
        self.profile >= 0
            && non_negative(&[self.splash_height, self.miter_height, self.full_radius])
            && self.bumps.iter().all(Bump::valid)
            && self.splash_bumps.iter().all(Bump::valid)
    }
}

impl CornerMod {
    fn valid(&self) -> bool {
        non_negative(&[self.size, self.length, self.depth])
            && self.edge_height.is_none_or(|h| h >= 0.0)
    }
}

impl Cutout {
    fn valid(&self) -> bool {
        (0..=3).contains(&self.anchor_corner)
            && positive(&[self.width, self.depth])
            && (0..=5).contains(&self.faucet_holes)
            && self.burners >= 0
            && self
                .centerline
                .as_deref()
                .is_none_or(|c| matches!(c, "none" | "left" | "right"))
            && self
                .setback
                .as_deref()
                .is_none_or(|c| matches!(c, "none" | "front" | "back"))
            && self
                .sink_shape
                .as_deref()
                .is_none_or(|s| SINK_SHAPES.contains(&s))
    }
}

impl Counter {
    fn valid(&self) -> bool {
        !self.runs.is_empty()
            && self.outlets >= 0
            && self.runs.iter().all(|run| {
                positive(&[run.length, run.depth]) && run.cutouts.iter().all(Cutout::valid)
            })
            && self.edges.iter().all(Edge::valid)
            && self.corners.iter().all(|(a, b)| {
                a.as_ref().is_none_or(CornerMod::valid) && b.as_ref().is_none_or(CornerMod::valid)
            })
            && self
                .line_edges
                .as_ref()
                .is_none_or(|lines| lines.values().all(|l| l.profile >= 0 && l.height >= 0.0))
    }
}

impl QuoteDoc {
    fn valid(&self) -> bool {
        #[allow(clippy::float_cmp)]
        let rounding = [0.5, 0.25, 0.125, 0.0625].contains(&self.rounding);
        self.version == 2
            && self.default_depth > 0.0
            && rounding
            && !self.areas.is_empty()
            && !self.color_options.is_empty()
            && self.areas.iter().all(|a| a.quantity >= 1)
            && self.counters.iter().all(Counter::valid)
            && self.other_counters.iter().all(|oc| {
                oc.sizes
                    .iter()
                    .all(|s| s.sqft.is_none_or(|v| v >= 0.0) && non_negative(&[s.length, s.width]))
                    && oc.details.iter().all(|d| {
                        d.profile >= 0
                            && non_negative(&[d.quantity, d.length, d.height, d.width, d.depth])
                    })
            })
            && self.color_options.iter().all(|o| {
                o.by_area
                    .values()
                    .all(|c| c.delivery.is_none_or(|d| d >= 0.0))
            })
            && self
                .layouts
                .values()
                .flat_map(HashMap::values)
                .all(|layout| layout.slabs.iter().all(|s| positive(&[s.width, s.height])))
    }

    /// The CRM's empty quote: one area and one color option.
    pub fn empty() -> Self {
        Self {
            version: 2,
            default_depth: 25.5,
            counters: Vec::new(),
            other_counters: Vec::new(),
            texts: Vec::new(),
            page_breaks: Vec::new(),
            areas: vec![Area {
                id: "area-1".into(),
                name: "Area #1".into(),
                quantity: 1,
            }],
            color_options: vec![ColorOption {
                id: "option-1".into(),
                by_area: HashMap::new(),
            }],
            layouts: HashMap::new(),
            items: Vec::new(),
            price_overrides: HashMap::new(),
            rounding: 0.0625,
        }
    }

    /// `parseQuoteDoc`; `None` when it does not fit the schema.
    pub fn parse(value: &serde_json::Value) -> Option<Self> {
        let decoded = match value {
            serde_json::Value::String(text) => serde_json::from_str(text).ok()?,
            other => other.clone(),
        };
        let doc: Self = serde_json::from_value(decoded).ok()?;
        doc.valid().then_some(doc)
    }

    /// `docFromQuote`: the document, or the empty quote.
    pub fn from_stored(value: &serde_json::Value) -> Self {
        Self::parse(value).unwrap_or_else(Self::empty)
    }
}
