use crate::db::{list_edges, list_nodes, node_by_app_id, Edge, Node};
use anyhow::{anyhow, Result};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Write;
use std::path::PathBuf;
use std::process::Command;

fn escape_label(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => {}
            _ => out.push(ch),
        }
    }
    out
}

// Wrap content for DOT node labels: width is in characters (CJK counts as 1).
// Prefers breaking at the most recent whitespace within the line; falls back
// to a hard break when no space exists (Chinese / long tokens). Escapes quotes
// and backslashes as it goes, and emits `\n` as DOT centered line breaks.
fn wrap_label(s: &str, width: usize) -> String {
    let mut out = String::new();
    let mut current: Vec<char> = Vec::new();

    fn flush_line(out: &mut String, line: &mut Vec<char>) {
        for ch in line.iter() {
            match *ch {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                _ => out.push(*ch),
            }
        }
        line.clear();
    }

    for ch in s.chars() {
        if ch == '\r' { continue; }
        if ch == '\n' {
            flush_line(&mut out, &mut current);
            out.push_str("\\n");
            continue;
        }
        current.push(ch);
        if current.len() >= width {
            // try to break at the last space
            let break_idx = current.iter().rposition(|&c| c == ' ');
            match break_idx {
                Some(idx) if idx >= width / 2 => {
                    let head: Vec<char> = current[..idx].to_vec();
                    let tail: Vec<char> = current[idx + 1..].to_vec();
                    let mut head_mut = head;
                    flush_line(&mut out, &mut head_mut);
                    out.push_str("\\n");
                    current = tail;
                }
                _ => {
                    // hard break (Chinese or very long token)
                    flush_line(&mut out, &mut current);
                    out.push_str("\\n");
                }
            }
        }
    }
    flush_line(&mut out, &mut current);
    out
}

// macOS-bundled font with full CJK glyph coverage.
// Helvetica/PingFang SC don't get picked up by older Graphviz (2.39) fontconfig,
// so CJK content rendered as empty boxes. STHeiti ships with macOS and works.
const FONT: &str = "STHeiti";

// Label wrap width in characters. CJK glyphs count as 1; English will wrap on
// the nearest space within the line.
const WRAP_WIDTH: usize = 16;

pub fn render_dot(graph_name: &str, nodes: &[Node], edges: &[Edge]) -> String {
    let mut out = String::new();
    out.push_str(&format!("digraph \"{}\" {{\n", escape_label(graph_name)));
    out.push_str("  rankdir=LR;\n");
    out.push_str("  graph [splines=true, overlap=false, bgcolor=\"#fafafa\"];\n");
    out.push_str(&format!(
        "  node  [shape=box, style=\"rounded,filled\", fillcolor=\"#ffffff\", color=\"#888888\", fontname=\"{}\", fontsize=11];\n",
        FONT
    ));
    out.push_str(&format!(
        "  edge  [color=\"#888888\", fontname=\"{}\", fontsize=10];\n\n",
        FONT
    ));

    // ---- 1. graph name root node ----
    out.push_str(&format!(
        "  graph_root [label=\"{}\", shape=ellipse, fillcolor=\"#e8eef9\", color=\"#3a73e8\", fontsize=13];\n",
        wrap_label(graph_name, WRAP_WIDTH)
    ));

    // ---- 2. content nodes (no app_id in label, wrapped) ----
    for n in nodes {
        let label = wrap_label(&n.content, WRAP_WIDTH);
        out.push_str(&format!(
            "  n{} [label=\"{}\", tooltip=\"{}\"];\n",
            n.id,
            label,
            escape_label(&n.content)
        ));
    }
    out.push('\n');

    // ---- 3. connect graph_root → top-level (no-reply-parent) nodes ----
    let has_reply_parent: HashSet<i64> = edges
        .iter()
        .filter(|e| e.kind == "reply")
        .map(|e| e.to_node_id)
        .collect();
    for n in nodes {
        if !has_reply_parent.contains(&n.id) {
            out.push_str(&format!(
                "  graph_root -> n{} [arrowhead=vee, color=\"#3a73e8\", penwidth=1.4];\n",
                n.id
            ));
        }
    }

    // ---- 4. user-authored edges ----
    for e in edges {
        let style = if e.kind == "ref" {
            ", style=dashed, color=\"#cc5555\", constraint=false"
        } else {
            ""
        };
        let label_attr = if e.label.is_empty() {
            String::new()
        } else {
            format!(", label=\"{}\"", escape_label(&e.label))
        };
        out.push_str(&format!(
            "  n{} -> n{} [arrowhead=vee{}{}];\n",
            e.from_node_id, e.to_node_id, style, label_attr
        ));
    }
    out.push_str("}\n");
    out
}

pub fn export_dot_to_path(
    conn: &Connection,
    graph_id: i64,
    graph_name: &str,
    out_path: &PathBuf,
) -> Result<()> {
    let nodes = list_nodes(conn, graph_id)?;
    let edges = list_edges(conn, graph_id)?;
    let dot = render_dot(graph_name, &nodes, &edges);
    if let Some(parent) = out_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let mut f = std::fs::File::create(out_path)?;
    f.write_all(dot.as_bytes())?;
    Ok(())
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RenderResult {
    pub gv_path: String,
    pub image_path: String,
}

pub fn render_and_save(
    conn: &Connection,
    graph_id: i64,
    graph_name: &str,
    out_dir: &PathBuf,
    format: &str,
) -> Result<RenderResult> {
    std::fs::create_dir_all(out_dir).ok();
    let safe = graph_name
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect::<String>();
    let gv_path = out_dir.join(format!("{}.gv", safe));
    let img_path = out_dir.join(format!("{}.{}", safe, format));
    export_dot_to_path(conn, graph_id, graph_name, &gv_path)?;

    let dot_bin = which_dot()?;
    let status = Command::new(&dot_bin)
        .arg(format!("-T{}", format))
        .arg(&gv_path)
        .arg("-o")
        .arg(&img_path)
        .status()?;
    if !status.success() {
        return Err(anyhow!("dot exited with status {:?}", status.code()));
    }
    Ok(RenderResult {
        gv_path: gv_path.to_string_lossy().into_owned(),
        image_path: img_path.to_string_lossy().into_owned(),
    })
}

/// Render a focused subgraph showing only the nodes and edges that participate
/// in one or more thinking-path search hits. Multiple paths are coloured
/// distinctly so overlapping paths are visually separable.
pub fn render_paths_dot(graph_name: &str, paths: &[PathHit]) -> String {
    let colors = ["#3a73e8", "#cc5555", "#449944", "#cc8822", "#9966cc", "#1b9aaa", "#d36b6b"];

    let mut all_nodes: HashMap<i64, &Node> = HashMap::new();
    for p in paths {
        for n in &p.nodes { all_nodes.entry(n.id).or_insert(n); }
    }
    let (start_id, end_id) = match paths.first() {
        Some(p) if !p.nodes.is_empty() => (p.nodes.first().unwrap().id, p.nodes.last().unwrap().id),
        _ => (-1, -1),
    };

    let mut out = String::new();
    out.push_str(&format!("digraph \"paths · {}\" {{\n", escape_label(graph_name)));
    out.push_str("  rankdir=LR;\n");
    out.push_str("  graph [splines=true, overlap=false, bgcolor=\"#fafafa\"];\n");
    out.push_str(&format!(
        "  node  [shape=box, style=\"rounded,filled\", fillcolor=\"#ffffff\", color=\"#888888\", fontname=\"{}\", fontsize=11];\n",
        FONT
    ));
    out.push_str(&format!(
        "  edge  [fontname=\"{}\", fontsize=10, penwidth=2];\n\n",
        FONT
    ));

    // Nodes — outline From/To in bold.
    for (id, n) in &all_nodes {
        let extra = if *id == start_id || *id == end_id {
            ", color=\"#1f2330\", penwidth=2.5, fillcolor=\"#fff8e0\""
        } else { "" };
        out.push_str(&format!(
            "  n{} [label=\"{}\", tooltip=\"{}\"{}];\n",
            id,
            wrap_label(&n.content, WRAP_WIDTH),
            escape_label(&n.content),
            extra
        ));
    }
    out.push('\n');

    // Edges — one per (path, step). The same underlying edge can appear in
    // multiple paths; we draw it once per path so each path's colour is visible.
    for (pi, p) in paths.iter().enumerate() {
        let color = colors[pi % colors.len()];
        for (i, step) in p.steps.iter().enumerate() {
            let (from, to) = if step.reversed {
                (p.nodes[i + 1].id, p.nodes[i].id)
            } else {
                (p.nodes[i].id, p.nodes[i + 1].id)
            };
            let style = if step.kind == "ref" { ", style=dashed" } else { "" };
            let label = if step.label.is_empty() {
                format!("p{}", pi + 1)
            } else {
                format!("p{} · {}", pi + 1, step.label)
            };
            out.push_str(&format!(
                "  n{} -> n{} [arrowhead=vee, color=\"{}\"{}, label=\"{}\"];\n",
                from, to, color, style, escape_label(&label)
            ));
        }
    }
    out.push_str("}\n");
    out
}

/// Generate the path-subgraph DOT and render it via `dot`. Returns paths to
/// both the .gv source and the rendered image.
pub fn render_paths_and_save(
    paths: &[PathHit],
    graph_name: &str,
    out_dir: &PathBuf,
    format: &str,
) -> Result<RenderResult> {
    std::fs::create_dir_all(out_dir).ok();
    let dot = render_paths_dot(graph_name, paths);
    let safe = graph_name
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect::<String>();
    let gv_path = out_dir.join(format!("{}_paths.gv", safe));
    let img_path = out_dir.join(format!("{}_paths.{}", safe, format));
    let mut f = std::fs::File::create(&gv_path)?;
    f.write_all(dot.as_bytes())?;
    drop(f);

    let dot_bin = which_dot()?;
    let status = Command::new(&dot_bin)
        .arg(format!("-T{}", format))
        .arg(&gv_path)
        .arg("-o")
        .arg(&img_path)
        .status()?;
    if !status.success() {
        return Err(anyhow!("dot exited with status {:?}", status.code()));
    }
    Ok(RenderResult {
        gv_path: gv_path.to_string_lossy().into_owned(),
        image_path: img_path.to_string_lossy().into_owned(),
    })
}

fn which_dot() -> Result<String> {
    for candidate in &["/usr/local/bin/dot", "/opt/homebrew/bin/dot", "/usr/bin/dot"] {
        if std::path::Path::new(candidate).exists() {
            return Ok((*candidate).to_string());
        }
    }
    // fall back to PATH lookup
    let out = Command::new("sh").arg("-c").arg("command -v dot").output()?;
    if out.status.success() {
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !s.is_empty() {
            return Ok(s);
        }
    }
    Err(anyhow!(
        "graphviz `dot` not found. Install with `brew install graphviz`."
    ))
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct PathStep {
    pub kind: String,      // "reply" | "ref"
    pub reversed: bool,    // true => we walked against the arrow
    pub label: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PathHit {
    pub nodes: Vec<Node>,
    pub steps: Vec<PathStep>, // len = nodes.len() - 1
}

// Shortest-path search between two app_ids treating the graph as **undirected**
// for traversal, but reporting per-step direction so the UI can render arrows
// the same way GraphViz does. Without this, a leaf reply can never "find" its
// ancestor (reply edges only point parent → child), which surprises users.
pub fn find_paths(
    conn: &Connection,
    graph_id: i64,
    from_app_id: &str,
    to_app_id: &str,
    max_paths: usize,
) -> Result<Vec<PathHit>> {
    let start = node_by_app_id(conn, graph_id, from_app_id)?
        .ok_or_else(|| anyhow!("from app_id not found: {}", from_app_id))?;
    let end = node_by_app_id(conn, graph_id, to_app_id)?
        .ok_or_else(|| anyhow!("to app_id not found: {}", to_app_id))?;

    let nodes = list_nodes(conn, graph_id)?;
    let edges = list_edges(conn, graph_id)?;
    let node_map: HashMap<i64, Node> = nodes.into_iter().map(|n| (n.id, n)).collect();

    // (neighbour, kind, reversed, label)
    let mut adj: HashMap<i64, Vec<(i64, String, bool, String)>> = HashMap::new();
    for e in &edges {
        adj.entry(e.from_node_id).or_default().push((
            e.to_node_id, e.kind.clone(), false, e.label.clone(),
        ));
        adj.entry(e.to_node_id).or_default().push((
            e.from_node_id, e.kind.clone(), true, e.label.clone(),
        ));
    }

    type Step = (i64, Option<(String, bool, String)>);
    let mut q: VecDeque<Vec<Step>> = VecDeque::new();
    q.push_back(vec![(start.id, None)]);
    let mut hits: Vec<Vec<Step>> = Vec::new();
    let mut shortest_len: Option<usize> = None;
    let max_depth = 64usize;

    while let Some(path) = q.pop_front() {
        let cur = path.last().unwrap().0;
        if cur == end.id {
            let l = path.len();
            match shortest_len {
                None => shortest_len = Some(l),
                Some(sl) if l > sl => break,
                _ => {}
            }
            hits.push(path);
            if hits.len() >= max_paths {
                break;
            }
            continue;
        }
        if path.len() >= max_depth {
            continue;
        }
        if let Some(neis) = adj.get(&cur) {
            for (nb, kind, reversed, label) in neis {
                if path.iter().any(|(nid, _)| nid == nb) {
                    continue; // no revisits within the same path
                }
                let mut np = path.clone();
                np.push((*nb, Some((kind.clone(), *reversed, label.clone()))));
                q.push_back(np);
            }
        }
    }

    let result = hits
        .into_iter()
        .map(|p| {
            let mut ns = Vec::with_capacity(p.len());
            let mut steps = Vec::with_capacity(p.len().saturating_sub(1));
            for (nid, edge) in p.into_iter() {
                if let Some(n) = node_map.get(&nid) {
                    ns.push(n.clone());
                }
                if let Some((kind, reversed, label)) = edge {
                    steps.push(PathStep { kind, reversed, label });
                }
            }
            PathHit { nodes: ns, steps }
        })
        .collect();
    Ok(result)
}

// Keyword-driven, directed DFS that enumerates every simple path from any node
// whose content (or app_id) matches `from_keyword` to any node matching
// `to_keyword`. Matching is case-insensitive substring. Edges are traversed in
// their natural direction — same shape as the Go reference's `findAllPaths`.
// `max_paths` caps the result set; `max_depth` caps recursion depth so that
// densely-connected graphs do not blow up.
pub fn find_paths_by_keyword(
    conn: &Connection,
    graph_id: i64,
    from_keyword: &str,
    to_keyword: &str,
    max_paths: usize,
) -> Result<Vec<PathHit>> {
    let from_key = from_keyword.trim().to_lowercase();
    let to_key = to_keyword.trim().to_lowercase();
    if from_key.is_empty() {
        return Err(anyhow!("from keyword cannot be empty"));
    }
    if to_key.is_empty() {
        return Err(anyhow!("to keyword cannot be empty"));
    }

    let nodes = list_nodes(conn, graph_id)?;
    let edges = list_edges(conn, graph_id)?;

    let matches = |n: &Node, kw: &str| -> bool {
        n.content.to_lowercase().contains(kw) || n.app_id.to_lowercase().contains(kw)
    };
    let from_nodes: Vec<Node> = nodes.iter().filter(|n| matches(n, &from_key)).cloned().collect();
    let to_ids: HashSet<i64> = nodes.iter().filter(|n| matches(n, &to_key)).map(|n| n.id).collect();

    if from_nodes.is_empty() {
        return Err(anyhow!("no nodes match from keyword: {}", from_keyword));
    }
    if to_ids.is_empty() {
        return Err(anyhow!("no nodes match to keyword: {}", to_keyword));
    }

    // Directed adjacency — matches the Go reference's `buildGraph`.
    let mut adj: HashMap<i64, Vec<(i64, String, String)>> = HashMap::new();
    for e in &edges {
        adj.entry(e.from_node_id).or_default().push((
            e.to_node_id,
            e.kind.clone(),
            e.label.clone(),
        ));
    }

    let node_map: HashMap<i64, Node> = nodes.into_iter().map(|n| (n.id, n)).collect();
    let max_depth: usize = 32;
    let mut all_hits: Vec<PathHit> = Vec::new();

    for from in &from_nodes {
        if all_hits.len() >= max_paths { break; }
        let mut visited: HashSet<i64> = HashSet::new();
        let mut cur_nodes: Vec<i64> = vec![from.id];
        let mut cur_steps: Vec<PathStep> = Vec::new();
        dfs_keyword(
            &adj,
            from.id,
            &to_ids,
            &mut visited,
            &mut cur_nodes,
            &mut cur_steps,
            &mut all_hits,
            &node_map,
            max_paths,
            max_depth,
        );
    }
    Ok(all_hits)
}

fn dfs_keyword(
    adj: &HashMap<i64, Vec<(i64, String, String)>>,
    current: i64,
    to_ids: &HashSet<i64>,
    visited: &mut HashSet<i64>,
    cur_nodes: &mut Vec<i64>,
    cur_steps: &mut Vec<PathStep>,
    all_hits: &mut Vec<PathHit>,
    node_map: &HashMap<i64, Node>,
    max_paths: usize,
    max_depth: usize,
) {
    if all_hits.len() >= max_paths { return; }

    // Record the path once we land on any to-keyword match (and we're not the
    // trivial 1-node path). Stop exploring past the endpoint to keep results
    // bounded — same shape as the Go reference returning at `current == end`.
    if cur_nodes.len() > 1 && to_ids.contains(&current) {
        let path_nodes: Vec<Node> = cur_nodes
            .iter()
            .filter_map(|id| node_map.get(id).cloned())
            .collect();
        all_hits.push(PathHit {
            nodes: path_nodes,
            steps: cur_steps.clone(),
        });
        return;
    }

    if cur_nodes.len() >= max_depth { return; }

    visited.insert(current);
    if let Some(neighbors) = adj.get(&current) {
        for (nb, kind, label) in neighbors {
            if visited.contains(nb) { continue; }
            cur_nodes.push(*nb);
            cur_steps.push(PathStep {
                kind: kind.clone(),
                reversed: false,
                label: label.clone(),
            });
            dfs_keyword(adj, *nb, to_ids, visited, cur_nodes, cur_steps, all_hits, node_map, max_paths, max_depth);
            cur_nodes.pop();
            cur_steps.pop();
            if all_hits.len() >= max_paths { break; }
        }
    }
    visited.remove(&current);
}
