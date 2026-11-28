//! The fleet as a picture, drawn here as SVG with a fixed, layered layout: the control plane on top, a column per
//! machine below it holding its nodes and their plugins, a line from the control plane to each machine (dashed when
//! none of its nodes is linked), and, in the space under the row, an arc from each machine that shares a tool to each
//! machine with a node its `allow:` names. No layout engine and no script: the shape of a fleet is always these layers.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use crate::model::{Node, Snapshot};
use crate::views::esc;

const COL_W: f64 = 196.0;
const GAP: f64 = 28.0;
const PAD: f64 = 24.0;
const TOP: f64 = 24.0;
const CP_H: f64 = 52.0;
const ROW_GAP: f64 = 56.0;
const MACHINE_HEAD: f64 = 34.0;
const NODE_HEAD: f64 = 26.0;
const CHIP_H: f64 = 20.0;
const CHIP_GAP: f64 = 6.0;
const CHAR_W: f64 = 6.4;
const MAX_COLS: usize = 6;
/// How far under the row the first shared-tool arc reaches, and how much further each next one does.
const ARC_FIRST: f64 = 30.0;
const ARC_STEP: f64 = 24.0;

struct Chip {
    label: String,
    kind: &'static str,
    x: f64,
    y: f64,
    w: f64,
}

struct NodeBox<'a> {
    node: &'a Node,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    chips: Vec<Chip>,
}

/// Lay chips out in rows within `width`; returns them and the height used.
fn flow(labels: Vec<(String, &'static str)>, x0: f64, y0: f64, width: f64) -> (Vec<Chip>, f64) {
    let mut chips = vec![];
    let (mut x, mut y) = (x0, y0);
    for (label, kind) in labels {
        let w = (label.chars().count() as f64 * CHAR_W + 18.0).min(width);
        if x > x0 && x + w > x0 + width {
            x = x0;
            y += CHIP_H + CHIP_GAP;
        }
        chips.push(Chip { label, kind, x, y, w });
        x += w + CHIP_GAP;
    }
    let h = if chips.is_empty() { 0.0 } else { y - y0 + CHIP_H };
    (chips, h)
}

fn glob(pattern: &str, s: &str) -> bool {
    match pattern.split_once('*') {
        None => pattern == s,
        Some((pre, rest)) => {
            s.starts_with(pre) && {
                let tail = &s[pre.len()..];
                (0..=tail.len()).any(|i| tail.is_char_boundary(i) && glob(rest, &tail[i..]))
            }
        }
    }
}

fn node_health(n: &Node) -> &'static str {
    if !n.online {
        "bad"
    } else if n.problem().is_some() {
        "warn"
    } else {
        "ok"
    }
}

struct Frame<'a> {
    name: &'a str,
    x: f64,
    y: f64,
    h: f64,
    row: usize,
    health: &'static str,
}

/// A shared tool drawn from the machine that offers it to a machine whose node may use it.
struct Arc {
    tool: String,
    from: usize,
    to: usize,
}

pub fn svg(s: &Snapshot) -> String {
    let machines = s.machines();
    if machines.is_empty() {
        return r#"<p class="empty">Nothing to draw yet: no machine has linked to the control plane.</p>"#.into();
    }
    let cols = machines.len().clamp(1, MAX_COLS);
    let width = PAD * 2.0 + cols as f64 * COL_W + (cols - 1) as f64 * GAP;
    let inner = COL_W - 24.0;
    let machine_of = |node: &str| machines.iter().position(|(_, ns)| ns.iter().any(|n| n.name == node));

    // Each tool to each machine with a node its `allow:` names, once.
    let mut arcs: Vec<Arc> = vec![];
    for t in &s.tools {
        let Some(from) = machine_of(&t.node) else { continue };
        for n in &s.nodes {
            let allowed = t.allow.iter().any(|p| glob(p.rsplit_once('@').map_or(p.as_str(), |(_, n)| n), &n.name));
            let Some(to) = machine_of(&n.name) else { continue };
            if allowed && to != from && !arcs.iter().any(|a| a.tool == t.name && a.from == from && a.to == to) {
                arcs.push(Arc { tool: t.name.clone(), from, to });
            }
        }
    }
    // Shorter arcs nearer the boxes, so they nest rather than cross.
    arcs.sort_by_key(|a| (a.from.abs_diff(a.to), a.from, a.to));

    // Machines in rows of `cols`, each row as tall as its tallest machine, with room under it for its arcs.
    let mut boxes: Vec<NodeBox> = vec![];
    let mut frames: Vec<Frame> = vec![];
    let mut y_row = TOP + CP_H + ROW_GAP;
    for (r, row) in machines.chunks(cols).enumerate() {
        let mut row_h: f64 = 0.0;
        let first = frames.len();
        for (i, (name, nodes)) in row.iter().enumerate() {
            let x = PAD + i as f64 * (COL_W + GAP);
            let mut y = y_row + MACHINE_HEAD;
            for n in nodes {
                let mut labels: Vec<(String, &'static str)> = n
                    .status
                    .as_ref()
                    .map(|st| st.plugins.iter().filter(|p| p.enabled).map(|p| (p.name.clone(), p.health())).collect())
                    .unwrap_or_default();
                labels.extend(s.tools_on(&n.name).into_iter().map(|t| (t.name.clone(), "tool")));
                let (chips, ch) = flow(labels, x + 20.0, y + NODE_HEAD, inner - 16.0);
                let h = NODE_HEAD + ch + if ch > 0.0 { 10.0 } else { 4.0 };
                boxes.push(NodeBox { node: n, x: x + 12.0, y, w: inner, h, chips });
                y += h + 10.0;
            }
            row_h = row_h.max(y - y_row + 2.0);
            let health = if nodes.iter().all(|n| !n.online) {
                "bad"
            } else if nodes.iter().any(|n| n.problem().is_some()) {
                "warn"
            } else {
                "ok"
            };
            frames.push(Frame { name, x, y: y_row, h: 0.0, row: r, health });
        }
        for f in &mut frames[first..] {
            f.h = row_h;
        }
        let under = arcs.iter().filter(|a| frames.get(a.from).is_some_and(|f| f.row == r)).count();
        y_row += row_h + ROW_GAP.max(ARC_FIRST + under as f64 * ARC_STEP + 24.0);
    }
    let last_row = frames.last().map_or(0, |f| f.row);
    let under_last = arcs.iter().filter(|a| frames[a.from].row == last_row).count();
    let bottom = frames.iter().map(|f| f.y + f.h).fold(0.0, f64::max);
    let height =
        bottom + if under_last > 0 { ARC_FIRST + (under_last - 1) as f64 * ARC_STEP + 16.0 } else { 0.0 } + PAD;

    let cp_w = 220.0_f64.min(width - 2.0 * PAD);
    let cp_x = (width - cp_w) / 2.0;
    let cp_bottom = TOP + CP_H;
    let mut o = String::new();
    let _ = write!(
        o,
        r#"<div class="graph-scroll"><svg class="graph" viewBox="0 0 {width:.0} {height:.0}" width="{width:.0}" height="{height:.0}" role="img" aria-labelledby="graph-title"><title id="graph-title">{} linked to the control plane</title><defs><marker id="g-arrow" viewBox="0 0 8 8" refX="7" refY="4" markerWidth="7" markerHeight="7" orient="auto-start-reverse"><path class="g-arrow" d="M1 1L7 4L1 7"/></marker></defs>"#,
        esc(&format!("{} machines and {} nodes", machines.len(), s.nodes.len()))
    );

    // One link from the control plane to each machine, under everything else.
    o.push_str(r#"<g class="g-links">"#);
    for f in &frames {
        let (x1, y1) = (width / 2.0, cp_bottom);
        let (x2, y2) = (f.x + COL_W / 2.0, f.y);
        let my = (y1 + y2) / 2.0;
        let _ = write!(
            o,
            r#"<path class="g-link{}" d="M{x1:.1},{y1:.1} C{x1:.1},{my:.1} {x2:.1},{my:.1} {x2:.1},{y2:.1}"/>"#,
            if f.health == "bad" { " g-link-off" } else { "" }
        );
    }
    o.push_str("</g>");

    // Machines.
    for f in &frames {
        let _ = write!(
            o,
            r#"<g class="g-machine"><rect x="{:.1}" y="{:.1}" width="{COL_W}" height="{:.1}" rx="12"/><circle class="g-dot g-{}" cx="{:.1}" cy="{:.1}" r="3.5"/><text class="g-machine-name" x="{:.1}" y="{:.1}">{}</text></g>"#,
            f.x,
            f.y,
            f.h,
            f.health,
            f.x + 16.0,
            f.y + 18.0,
            f.x + 26.0,
            f.y + 22.0,
            esc(f.name)
        );
    }

    // Nodes and their plugins.
    for b in &boxes {
        let _ = write!(
            o,
            r#"<g class="g-node"><rect x="{:.1}" y="{:.1}" width="{:.1}" height="{:.1}" rx="8"/><circle class="g-dot g-{}" cx="{:.1}" cy="{:.1}" r="3"/><text class="g-node-name" x="{:.1}" y="{:.1}">{}</text><text class="g-node-user" x="{:.1}" y="{:.1}" text-anchor="end">{}</text>"#,
            b.x,
            b.y,
            b.w,
            b.h,
            node_health(b.node),
            b.x + 12.0,
            b.y + 14.0,
            b.x + 21.0,
            b.y + 18.0,
            esc(&b.node.name),
            b.x + b.w - 10.0,
            b.y + 18.0,
            esc(b.node.user.as_deref().unwrap_or(""))
        );
        for c in &b.chips {
            let _ = write!(
                o,
                r#"<g class="g-chip g-chip-{}"><rect x="{:.1}" y="{:.1}" width="{:.1}" height="{CHIP_H}" rx="10"/><text x="{:.1}" y="{:.1}" text-anchor="middle">{}</text></g>"#,
                c.kind,
                c.x,
                c.y,
                c.w,
                c.x + c.w / 2.0,
                c.y + 14.0,
                esc(&c.label)
            );
        }
        o.push_str("</g>");
    }

    // Shared tools: arcs under the row, from the machine that offers each to the machine that may use it, nested
    // by length, each with its tool's name at its lowest point.
    o.push_str(r#"<g class="g-tools">"#);
    let mut leaving: BTreeMap<usize, usize> = BTreeMap::new();
    let mut arriving: BTreeMap<usize, usize> = BTreeMap::new();
    let mut depth_in_row: BTreeMap<usize, usize> = BTreeMap::new();
    for a in &arcs {
        let (f, t) = (&frames[a.from], &frames[a.to]);
        let lo = leaving.entry(a.from).or_default();
        let ao = arriving.entry(a.to).or_default();
        let sx = f.x + COL_W / 2.0 - 12.0 - *lo as f64 * 18.0;
        let tx = t.x + COL_W / 2.0 + 12.0 + *ao as f64 * 18.0;
        *lo += 1;
        *ao += 1;
        let k = depth_in_row.entry(f.row).or_default();
        let d = ARC_FIRST + *k as f64 * ARC_STEP;
        *k += 1;
        let (sy, ty) = (f.y + f.h, t.y + t.h);
        let (cy1, cy2) = (sy + d / 0.75, ty + d / 0.75);
        let _ = write!(
            o,
            r#"<path class="g-tool" d="M{sx:.1},{sy:.1} C{sx:.1},{cy1:.1} {tx:.1},{cy2:.1} {tx:.1},{ty:.1}" marker-end="url(#g-arrow)"><title>{}</title></path>"#,
            esc(&format!("{} on {} is shared with {}", a.tool, f.name, t.name))
        );
        if f.row == t.row {
            let (lx, ly) = ((sx + tx) / 2.0, sy + d);
            let w = a.tool.chars().count() as f64 * CHAR_W + 12.0;
            let _ = write!(
                o,
                r#"<g class="g-tool-label"><rect x="{:.1}" y="{:.1}" width="{w:.1}" height="16" rx="8"/><text x="{lx:.1}" y="{:.1}" text-anchor="middle">{}</text></g>"#,
                lx - w / 2.0,
                ly - 8.0,
                ly + 3.5,
                esc(&a.tool)
            );
        }
    }
    o.push_str("</g>");

    // The control plane last, so links start under it.
    let _ = write!(
        o,
        r#"<g class="g-cp"><rect x="{cp_x:.1}" y="{TOP}" width="{cp_w:.1}" height="{CP_H}" rx="12"/><text class="g-cp-name" x="{:.1}" y="{:.1}" text-anchor="middle">Control plane</text><text class="g-cp-sub" x="{:.1}" y="{:.1}" text-anchor="middle">{}</text></g>"#,
        width / 2.0,
        TOP + 22.0,
        width / 2.0,
        TOP + 39.0,
        esc(&format!("{} of {} nodes linked", s.nodes.iter().filter(|n| n.online).count(), s.nodes.len()))
    );
    o.push_str("</svg></div>");
    o
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs_match_node_names() {
        assert!(glob("*", "laptop"));
        assert!(glob("agents*", "agents-triage"));
        assert!(glob("a*s", "agents"));
        assert!(!glob("agents", "agents-triage"));
    }

    #[test]
    fn the_demo_draws_every_node_and_its_tool_edges() {
        let s = crate::demo::Demo::new().snapshot();
        let g = svg(&s);
        for n in &s.nodes {
            assert!(g.contains(&format!(">{}</text>", n.name)), "{}", n.name);
        }
        assert_eq!(
            g.matches(r#"class="g-tool""#).count(),
            4,
            "browser and folders to agents, folders to studio, gpu to agents"
        );
        assert!(g.contains("g-link-off"), "old-mini is offline");
        assert_eq!(g.matches(r#"class="g-tool-label""#).count(), 4, "every arc is named");
    }

    #[test]
    fn an_empty_fleet_says_so() {
        assert!(svg(&Snapshot::default()).contains("Nothing to draw"));
    }
}
