//! Nodes module: the fox-services cluster as one table (read-only).
//!
//! One row per statically configured node with its latest polled hardware
//! snapshot, then the Fox Mesh peers the nodes reported. An offline node
//! keeps its row with the error; a disabled monitor explains itself.

use fox_services_top_core::view::{ClusterNodeView, ClusterView, ViewModel};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use super::ModuleBuildError;
use super::common::{
    ColumnSpec, ManifestSpec, Output, RowCursor, TableCell, build_manifest, draw_line,
    draw_table_header, draw_table_row, fit_columns, message, no_data, panel_with_keys,
};
use crate::format::{clean, format_bytes};
use crate::num::saturate_u16;
use crate::registry::{DisplayContext, DisplayModule, DisplayModuleManifest, RenderOutput, Slot};

/// The keys acting on the view, at the right of its title.
const NODES_KEYS: &str = "j/k scroll · / filter";

const NODE_COLUMNS: [ColumnSpec; 8] = [
    ColumnSpec::left("NODE", 10, 100).flexible().prefer(16),
    ColumnSpec::left("STATE", 7, 90),
    ColumnSpec::right("CPU%", 6, 80),
    ColumnSpec::right("MEM%", 6, 75),
    ColumnSpec::left("GPU", 12, 70),
    ColumnSpec::right("NET RX/TX", 13, 60),
    ColumnSpec::right("TOK/S", 7, 50),
    ColumnSpec::right("CTNS", 9, 40),
];

pub struct ClusterModule {
    manifest: DisplayModuleManifest,
}

impl ClusterModule {
    pub fn new() -> Result<Self, ModuleBuildError> {
        Ok(Self {
            manifest: build_manifest(ManifestSpec {
                id: "cluster",
                title: "Nodes",
                min_columns: 40,
                min_rows: 5,
                priority: 30,
                slots: &[Slot::Detail],
            })?,
        })
    }
}

impl DisplayModule for ClusterModule {
    fn manifest(&self) -> &DisplayModuleManifest {
        &self.manifest
    }

    fn render(
        &self,
        context: &DisplayContext,
        model: &ViewModel,
        area: Rect,
        frame: &mut Frame<'_>,
    ) -> RenderOutput {
        let mut output = Output::default();
        let Some(cluster) = model.modules.cluster.as_deref() else {
            let inner = panel_with_keys(frame, area, "Nodes", NODES_KEYS, context);
            message(frame, inner, &no_data("cluster"), context.theme.muted_style());
            output.label("nodes: no data");
            return output.finish();
        };
        let online = cluster.nodes.iter().filter(|node| node.online).count();
        let inner = panel_with_keys(
            frame,
            area,
            &format!("Nodes · {online}/{} online", cluster.nodes.len()),
            NODES_KEYS,
            context,
        );
        if !cluster.enabled {
            message(
                frame,
                inner,
                "remote monitoring is disabled (config remote.enabled)",
                context.theme.muted_style(),
            );
            output.label("nodes: disabled");
            return output.finish();
        }
        let mut rows = RowCursor::new(inner);
        let filter = model.session.filter.to_lowercase();
        let nodes: Vec<&ClusterNodeView> = cluster
            .nodes
            .iter()
            .filter(|node| filter.is_empty() || node.name.to_lowercase().contains(&filter))
            .collect();
        if nodes.is_empty() {
            message(
                frame,
                inner,
                "no nodes configured (config remote.nodes)",
                context.theme.muted_style(),
            );
            output.label("nodes: none configured");
            return output.finish();
        }
        render_node_table(frame, &mut rows, &nodes, context, &mut output);
        render_offline(frame, &mut rows, &nodes, context, &mut output);
        render_peers(frame, &mut rows, cluster, context, &mut output);
        output.finish()
    }
}

fn render_node_table(
    frame: &mut Frame<'_>,
    rows: &mut RowCursor,
    nodes: &[&ClusterNodeView],
    context: &DisplayContext,
    output: &mut Output,
) {
    let Some(table) = rows.take(saturate_u16(nodes.len().max(1) + 1).min(rows.remaining().max(3))) else {
        return;
    };
    let mut cursor = RowCursor::new(table);
    let Some(header) = cursor.next_row() else { return };
    let columns = fit_columns(&NODE_COLUMNS, header.width);
    draw_table_header(
        frame.buffer_mut(),
        header,
        &NODE_COLUMNS,
        &columns,
        context.theme.title_style(),
    );
    for node in nodes {
        let Some(row) = cursor.next_row() else {
            break;
        };
        draw_table_row(
            frame.buffer_mut(),
            row,
            &columns,
            &node_cells(node, context.theme.text_style()),
            None,
        );
        output.label(format!("node {}", node.name));
    }
}

fn node_cells(node: &ClusterNodeView, style: Style) -> Vec<TableCell> {
    let state = if node.online { "online" } else { "offline" };
    vec![
        TableCell::new(clean(&node.name), style),
        TableCell::new(state.to_owned(), style),
        TableCell::new(percent(node.cpu_busy_pct), style),
        TableCell::new(percent(node.memory_busy_pct), style),
        TableCell::new(gpu_text(node), style),
        TableCell::new(net_text(node), style),
        TableCell::new(
            node.tokens_per_sec.map_or_else(|| "–".to_owned(), |rate| format!("{rate:.1}")),
            style,
        ),
        TableCell::new(containers_text(node), style),
    ]
}

fn percent(value: Option<f64>) -> String {
    value.map_or_else(|| "–".to_owned(), |percent| format!("{percent:.1}"))
}

fn gpu_text(node: &ClusterNodeView) -> String {
    match (node.gpu_kind.as_deref(), node.gpu_util_pct) {
        (Some(kind), Some(util)) => format!("{kind} {util:.0}%"),
        (Some(kind), None) => kind.to_owned(),
        (None, _) => "–".to_owned(),
    }
}

fn net_text(node: &ClusterNodeView) -> String {
    match (node.net_rx_bytes_s, node.net_tx_bytes_s) {
        (Some(rx), Some(tx)) => format!("{}/{}", short_rate(rx), short_rate(tx)),
        _ => "–".to_owned(),
    }
}

fn short_rate(bytes_s: u64) -> String {
    if bytes_s == 0 {
        return "0".to_owned();
    }
    format_bytes(bytes_s).replace([' ', 'B'], "")
}

fn containers_text(node: &ClusterNodeView) -> String {
    match (node.containers_running, node.containers_total) {
        (Some(running), Some(total)) => format!("{running}/{total}"),
        _ => "–".to_owned(),
    }
}

fn render_offline(
    frame: &mut Frame<'_>,
    rows: &mut RowCursor,
    nodes: &[&ClusterNodeView],
    context: &DisplayContext,
    output: &mut Output,
) {
    for node in nodes.iter().filter(|node| !node.online) {
        let Some(error) = node.error.as_deref() else { continue };
        let Some(row) = rows.next_row() else { break };
        draw_line(
            frame.buffer_mut(),
            row,
            &Line::from(Span::styled(
                format!("{}: {}", clean(&node.name), clean(error)),
                context.theme.muted_style(),
            )),
        );
        output.label(format!("node {} offline", node.name));
    }
}

fn render_peers(
    frame: &mut Frame<'_>,
    rows: &mut RowCursor,
    cluster: &ClusterView,
    context: &DisplayContext,
    output: &mut Output,
) {
    if cluster.peers.is_empty() {
        return;
    }
    let Some(title) = rows.next_row() else { return };
    draw_line(
        frame.buffer_mut(),
        title,
        &Line::from(Span::styled(
            format!("Mesh peers ({})", cluster.peers.len()),
            context.theme.title_style(),
        )),
    );
    let skip = usize::from(context.scroll);
    for peer in cluster.peers.iter().skip(skip) {
        let Some(row) = rows.next_row() else { break };
        let state = if peer.online { "online" } else { "offline" };
        let name = peer.name.as_deref().unwrap_or(&peer.node_id);
        draw_line(
            frame.buffer_mut(),
            row,
            &Line::from(Span::styled(
                format!("{} · via {} · {state}", clean(name), clean(&peer.via)),
                context.theme.text_style(),
            )),
        );
        output.label(format!("peer {}", peer.node_id));
    }
}
