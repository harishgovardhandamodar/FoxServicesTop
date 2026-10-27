//! Processes module: the projection's visible window of the process table.
//!
//! Columns (D-66): GPU, PID, USER, TYPE (compute `C`, graphics `G` or both,
//! as `nvidia-smi` prints them, D-54), CPU%, GPU util, GPU memory, CPU
//! memory (the resident set), NAME, set apart by `│` and as wide as the
//! drawn rows need. A tracked process's name follows a `■` in its color
//! (D-64), so the table names the colors of the memory bars and charts.
//! Rows are identified by `ProcessKey`; the selected row is marked with `▶`
//! (not only by color) and published as the only `K` hit target.

use fox_services_top_core::canonical::GpuUsageKind;
use fox_services_top_core::identity::EntityId;
use fox_services_top_core::metric::CanonicalUnit;
use fox_services_top_core::view::{ProcessGpuView, ProcessRowView, ProcessesView, ViewModel};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};

use super::ModuleBuildError;
use super::common::{
    ColumnSpec, FittedColumn, ManifestSpec, Output, RowCursor, TableCell, build_manifest,
    draw_column_separators, draw_line, draw_table_header, draw_table_row, fit_separated_columns,
    message, no_data, panel, panel_with_keys,
};
use crate::format::{
    NOT_AVAILABLE, clean, display_width, format_energy, format_optional_bytes,
    format_optional_percent, format_value, truncate_to_width,
};
use crate::num::saturate_u16;
use crate::registry::{
    DisplayContext, DisplayModule, DisplayModuleManifest, OverviewCell, RenderOutput, Slot,
    controls,
};

const SELECTION_MARKER: &str = "▶";
/// The keys acting on this table, at the right of its title (D-70).
const PROCESS_KEYS: &str =
    "j/k/↑/↓ select · s sort · r reverse · g GPU filter · / filter · K terminate · F9 kill";
/// A tracked process's color, before its name (D-64).
const COLOR_SWATCH: &str = "■";
/// Position of the swatch column in [`COLUMNS`].
const SWATCH_COLUMN: usize = 9;

/// The GPU first (D-66), then identity, type and CPU use, GPU use, the
/// memory on the CPU side and the name. Minimum widths fit one GPU's
/// values; [`fitted_columns`] widens a column to what the rows show.
const COLUMNS: [ColumnSpec; 11] = [
    ColumnSpec::left("", 1, 255),
    ColumnSpec::left("GPU", 3, 100).attached(),
    ColumnSpec::right("PID", 7, 98),
    ColumnSpec::left("USER", 4, 40),
    ColumnSpec::left("TYPE", 4, 45),
    ColumnSpec::right("CPU%", 5, 80),
    ColumnSpec::right("GPU%", 5, 85),
    ColumnSpec::right("GPU MEM", 8, 70),
    ColumnSpec::right("CPU MEM", 8, 60),
    ColumnSpec::left("", 1, 97),
    ColumnSpec::left("NAME", 10, 96).flexible().attached(),
];

/// Module id of the process table; the Controller keeps its Overview
/// sub-tab here (`0` GPU Processes, `1` Fox services, `2` Power, `3` LAN).
pub const PROCESSES_MODULE_ID: &str = "processes";

/// The Overview sub-tabs of the process panel, in order.
const SUB_TABS: [&str; 4] = ["GPU Processes", "Fox services", "Power", "LAN Machines"];

pub struct ProcessesModule {
    manifest: DisplayModuleManifest,
}

impl ProcessesModule {
    pub fn new() -> Result<Self, ModuleBuildError> {
        Ok(Self {
            manifest: build_manifest(ManifestSpec {
                id: PROCESSES_MODULE_ID,
                title: "Processes (GPU first)",
                min_columns: 40,
                min_rows: 5,
                priority: 75,
                slots: &[Slot::Overview],
            })?,
        })
    }
}

impl DisplayModule for ProcessesModule {
    fn manifest(&self) -> &DisplayModuleManifest {
        &self.manifest
    }

    fn overview_cell(&self) -> Option<OverviewCell> {
        Some(OverviewCell::at(3, 0, 100).filling())
    }

    /// Enough rows for every GPU process (borders, column header, rows and
    /// the status line), so a short list leaves its rows to the GPU panel.
    fn preferred_rows(&self, model: &ViewModel, _width: u16, _slot: Slot) -> Option<u16> {
        let rows = model.modules.processes.as_deref().map_or(1, |view| {
            usize::try_from(view.total_rows).unwrap_or(usize::MAX)
        });
        // Borders, header, its rule, the status line and one row per
        // process (D-100).
        let rows = saturate_u16(rows.max(1));
        Some(rows.saturating_add(5))
    }

    fn render(
        &self,
        context: &DisplayContext,
        model: &ViewModel,
        area: Rect,
        frame: &mut Frame<'_>,
    ) -> RenderOutput {
        let mut output = Output::default();
        let Some(view) = model.modules.processes.as_deref() else {
            let inner = panel(frame, area, "Processes", context);
            message(
                frame,
                inner,
                &no_data("processes"),
                context.theme.muted_style(),
            );
            output.label("processes: no data");
            return output.finish();
        };
        // The Overview panel carries parallel views in the same area.
        let sub_tab = context.tab.min(3);
        let title = match sub_tab {
            1 => services_title(model),
            2 => power_title(model),
            3 => nodes_title(model),
            _ => title_text(view),
        };
        // `j`/`k` still select processes on the Overview; the sub-tab
        // filters like its tab, and scrolls it there (key `7`).
        let keys = match sub_tab {
            1..=3 => "/ filter",
            _ => PROCESS_KEYS,
        };
        let inner = panel_with_keys(frame, area, &title, keys, context);
        output.label(format!(
            "processes: {} rows, sort {} {}",
            view.total_rows,
            view.sort.label(),
            if view.ascending {
                "ascending"
            } else {
                "descending"
            }
        ));
        if inner.is_empty() {
            return output.finish();
        }
        let mut rows = RowCursor::new(inner);
        render_sub_tabs(
            frame,
            &mut rows,
            sub_tab,
            model,
            &self.manifest,
            context,
            &mut output,
        );
        if sub_tab == 1 {
            let Some(services) = model.modules.docker.as_deref() else {
                if let Some(area) = rows.rest() {
                    message(
                        frame,
                        area,
                        &no_data("services"),
                        context.theme.muted_style(),
                    );
                }
                output.label("services: no data");
                return output.finish();
            };
            if !services.enabled {
                if let Some(area) = rows.rest() {
                    message(
                        frame,
                        area,
                        "remote monitoring is disabled (config remote.enabled)",
                        context.theme.muted_style(),
                    );
                }
                output.label("services: disabled");
                return output.finish();
            }
            super::docker::render_services_content(
                frame,
                &mut rows,
                services,
                &model.session.filter.to_lowercase(),
                context,
                &mut output,
                true,
            );
            return output.finish();
        }
        if sub_tab == 2 {
            render_power_content(
                frame,
                &mut rows,
                model,
                &model.session.filter.to_lowercase(),
                context,
                &mut output,
            );
            return output.finish();
        }
        if sub_tab == 3 {
            super::cluster::render_overview_nodes(frame, &mut rows, model, context, &mut output);
            return output.finish();
        }
        if view.rows.is_empty() {
            // The sub-tab row above stays: the message takes the rows below
            // it, not the whole panel (which would overprint the tabs).
            if let Some(area) = rows.rest() {
                message(frame, area, &empty_text(view), context.theme.muted_style());
            }
            output.label(empty_text(view));
            return output.finish();
        }
        let table = TableLayout::new(&mut rows);
        let window = DisplayWindow::new(view, table.body.height);
        let filter = view.gpu_filter.as_ref().map(|(id, _)| id);
        let drawn: Vec<(&ProcessRowView, bool, [TableCell; 11])> = view
            .rows
            .iter()
            .skip(window.offset)
            .take(window.count)
            .map(|row| {
                let selected = view.selected.as_ref() == Some(&row.key);
                (row, selected, row_cells(row, selected, filter, context))
            })
            .collect();
        let specs = fitted_columns(drawn.iter().map(|(_, _, cells)| cells));
        let columns = fit_separated_columns(&specs, table.body.width.saturating_sub(1));
        let buffer = frame.buffer_mut();
        draw_table_header(
            buffer,
            table.header,
            &specs,
            &columns,
            context.theme.header_style(),
        );
        draw_column_separators(buffer, table.header, &columns, context.theme.muted_style());
        if let Some(rule) = table.rule {
            draw_header_rule(buffer, rule, &columns, context);
        }
        let mut cursor = RowCursor::new(table.body);
        for (row, selected, cells) in drawn {
            let Some(rect) = cursor.next_row() else { break };
            let style = selected.then(|| context.theme.selected_row_style());
            draw_table_row(frame.buffer_mut(), rect, &columns, &cells, style);
            let separator = style.map_or(context.theme.muted_style(), |row| {
                context.theme.muted_style().patch(row)
            });
            draw_column_separators(frame.buffer_mut(), rect, &columns, separator);
            if selected {
                keep_swatch_color(frame, rect, &columns, &cells[SWATCH_COLUMN]);
            }
            let control = if selected {
                controls::PROCESS_ROW_SELECTED
            } else {
                controls::PROCESS_ROW
            };
            output.process_target(model, &self.manifest, control, rect, row.key.clone());
            output.label(row_label(row, selected, filter));
        }
        draw_scrollbar(frame, table.body, view, &window, context);
        if let Some(status) = table.status {
            let text = status_text(view, &window, filter);
            frame.buffer_mut().set_stringn(
                status.x,
                status.y,
                truncate_to_width(&text, usize::from(status.width)),
                usize::from(status.width),
                context.theme.label_style(),
            );
        }
        output.finish()
    }
}

/// The sub-tab strip at the top of the Overview panel.
fn render_sub_tabs(
    frame: &mut Frame<'_>,
    rows: &mut RowCursor,
    active_tab: u8,
    model: &ViewModel,
    manifest: &DisplayModuleManifest,
    context: &DisplayContext,
    output: &mut Output,
) {
    let Some(strip) = rows.next_row() else { return };
    let mut x = strip.x.saturating_add(1);
    let compact_labels = ["GPU", "Services", "Power", "LAN"];
    let labels = if strip.width < 54 {
        &compact_labels
    } else {
        &SUB_TABS
    };
    for (index, label) in labels.iter().enumerate() {
        let active = index == usize::from(active_tab.min(3));
        let width = display_width(label).min(usize::from(strip.width.saturating_sub(x - strip.x)));
        if width == 0 {
            break;
        }
        let rect = Rect::new(x, strip.y, saturate_u16(width + 2), 1);
        let text = format!(" {label} ");
        frame.buffer_mut().set_stringn(
            rect.x,
            rect.y,
            truncate_to_width(&text, usize::from(rect.width)),
            usize::from(rect.width),
            if active {
                context.theme.selected_row_style()
            } else {
                context.theme.muted_style()
            },
        );
        let control = match index {
            1 => controls::OVERVIEW_SUB_SERVICES,
            2 => controls::OVERVIEW_SUB_POWER,
            3 => controls::OVERVIEW_SUB_NODES,
            _ => controls::OVERVIEW_SUB_PROCESSES,
        };
        output.target(model, manifest, control, rect);
        output.label(format!("overview sub-tab {label}"));
        x = rect.x.saturating_add(rect.width).saturating_add(1);
    }
}

fn nodes_title(model: &ViewModel) -> String {
    match model.modules.cluster.as_deref() {
        Some(cluster) => {
            let online = cluster.nodes.iter().filter(|node| node.online).count();
            format!("LAN Machines · {online}/{} online", cluster.nodes.len())
        }
        None => "LAN Machines".to_owned(),
    }
}

/// Panel title on the Fox services sub-tab.
fn services_title(model: &ViewModel) -> String {
    match model.modules.docker.as_deref() {
        Some(services) => format!(
            "Fox services · {} services{}",
            services.services.len(),
            if services.stale { " · last read" } else { "" }
        ),
        None => "Fox services".to_owned(),
    }
}

/// Panel title on the Power sub-tab.
fn power_title(model: &ViewModel) -> String {
    match model.modules.power.as_deref() {
        Some(power) => {
            let window = if power.window_label.is_empty() {
                String::new()
            } else {
                format!(" · {}", power.window_label)
            };
            format!("Power{window}")
        }
        None => "Power".to_owned(),
    }
}

/// The Power sub-tab: total watts and kWh plus the busiest series, in the
/// process panel's area.
fn render_power_content(
    frame: &mut Frame<'_>,
    rows: &mut RowCursor,
    model: &ViewModel,
    filter: &str,
    context: &DisplayContext,
    output: &mut Output,
) {
    let Some(power) = model.modules.power.as_deref() else {
        if let Some(area) = rows.rest() {
            message(frame, area, &no_data("power"), context.theme.muted_style());
        }
        output.label("power: no data");
        return;
    };
    if power.rows.is_empty() {
        if let Some(area) = rows.rest() {
            message(
                frame,
                area,
                "no power data in this window",
                context.theme.muted_style(),
            );
        }
        output.label("power: idle");
        return;
    }
    let total = format!(
        "{} · {}",
        power.total_w.map_or_else(
            || "n/a".to_owned(),
            |watts| format_value(watts, CanonicalUnit::Watts)
        ),
        power
            .total_joules
            .map_or_else(|| "n/a".to_owned(), format_energy),
    );
    if let Some(row) = rows.next_row() {
        draw_line(
            frame.buffer_mut(),
            row,
            &Line::from(vec![
                Span::styled("total ", context.theme.label_style()),
                Span::styled(total, context.theme.text_style()),
            ]),
        );
    }
    for row in power
        .rows
        .iter()
        .filter(|row| filter.is_empty() || row.subject.to_lowercase().contains(filter))
    {
        let Some(area) = rows.next_row() else { break };
        let line = format!(
            "{} · {} · {}",
            clean(&row.subject),
            row.current_w.map_or_else(
                || "n/a".to_owned(),
                |watts| format_value(watts, CanonicalUnit::Watts)
            ),
            row.energy_joules
                .map_or_else(|| "n/a".to_owned(), format_energy),
        );
        draw_line(
            frame.buffer_mut(),
            area,
            &Line::from(Span::styled(line, context.theme.text_style())),
        );
        output.label(format!("power {}", clean(&row.subject)));
    }
}

/// Header, the rule under it, body and optional status row of the table
/// area.
struct TableLayout {
    header: Rect,
    rule: Option<Rect>,
    body: Rect,
    status: Option<Rect>,
}

impl TableLayout {
    /// The header, rule, body and status of what `rows` still has: the
    /// caller draws its own rows (the sub-tab strip) first.
    fn new(rows: &mut RowCursor) -> Self {
        let remaining = rows.remaining();
        let header = rows.next_row().unwrap_or_default();
        // The rule needs a row besides the header, a process and the status.
        let rule = (remaining >= 4).then(|| rows.next_row()).flatten();
        let status_rows = u16::from(remaining >= 4);
        let body = rows
            .take(rows.remaining().saturating_sub(status_rows))
            .unwrap_or_default();
        let status = rows.next_row();
        Self {
            header,
            rule,
            body,
            status,
        }
    }
}

/// A gray `────┼────` under the header that sets it apart from the rows,
/// crossing each column separator (D-72).
fn draw_header_rule(
    buffer: &mut ratatui::buffer::Buffer,
    rule: Rect,
    columns: &[FittedColumn],
    context: &DisplayContext,
) {
    let style = context.theme.muted_style();
    buffer.set_stringn(
        rule.x,
        rule.y,
        "─".repeat(usize::from(rule.width)),
        usize::from(rule.width),
        style,
    );
    for column in columns.iter().filter(|column| column.separated) {
        let x = rule.x.saturating_add(column.x).saturating_sub(2);
        if x < rule.right() {
            buffer.set_stringn(x, rule.y, "┼", 1, style);
        }
    }
}

/// Which projected rows are drawn. The projection carries the visible window
/// plus overscan with the selection centred; the drawn sub-window centres
/// the selected row too and clamps at both ends, so it is always visible and
/// the first/last rows of the list stay reachable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DisplayWindow {
    offset: usize,
    count: usize,
}

impl DisplayWindow {
    fn new(view: &ProcessesView, body_height: u16) -> Self {
        let capacity = usize::from(body_height);
        let selected_position = view
            .selected
            .as_ref()
            .and_then(|key| view.rows.iter().position(|row| &row.key == key));
        let max_offset = view.rows.len().saturating_sub(capacity);
        let offset = selected_position
            .map_or(0, |position| position.saturating_sub(capacity / 2))
            .min(max_offset);
        let count = view.rows.len().saturating_sub(offset).min(capacity);
        Self { offset, count }
    }

    /// Absolute index (in the full table) of the first drawn row.
    fn first_absolute(&self, view: &ProcessesView) -> u64 {
        u64::from(view.window_start).saturating_add(u64::try_from(self.offset).unwrap_or(u64::MAX))
    }
}

fn title_text(view: &ProcessesView) -> String {
    let mut title = "GPU Processes".to_owned();
    let arrow = if view.ascending { "▲" } else { "▼" };
    title.push_str(&format!(" · sort {} {arrow}", view.sort.label()));
    if let Some((_, label)) = &view.gpu_filter {
        title.push_str(&format!(" · GPU {}", clean(label)));
    }
    if !view.filter.is_empty() {
        title.push_str(&format!(" · filter \"{}\"", clean(&view.filter)));
    }
    title
}

fn empty_text(view: &ProcessesView) -> String {
    if !view.filter.is_empty() {
        format!("no processes match filter \"{}\"", clean(&view.filter))
    } else if let Some((_, label)) = &view.gpu_filter {
        format!("no processes on GPU {}", clean(label))
    } else {
        "no process is using a GPU".to_owned()
    }
}

fn gpu_entries<'a>(row: &'a ProcessRowView, filter: Option<&EntityId>) -> Vec<&'a ProcessGpuView> {
    row.gpus
        .iter()
        .filter(|gpu| filter.is_none_or(|id| &gpu.gpu_entity_id == id))
        .collect()
}

fn joined<'a>(
    entries: &[&'a ProcessGpuView],
    text: impl Fn(&'a ProcessGpuView) -> String,
) -> String {
    if entries.is_empty() {
        return "none".to_owned();
    }
    entries
        .iter()
        .map(|gpu| text(gpu))
        .collect::<Vec<_>>()
        .join(",")
}

/// `C`, `G` or `C+G`: the contexts a process holds on a GPU.
fn usage_code(usage: GpuUsageKind) -> &'static str {
    match usage {
        GpuUsageKind::Compute => "C",
        GpuUsageKind::Graphics => "G",
        GpuUsageKind::ComputeAndGraphics => "C+G",
        GpuUsageKind::Unknown => NOT_AVAILABLE,
    }
}

/// `GPU 0` → `0` inside the GPU column (its header already says GPU).
fn short_gpu_label(label: &str) -> String {
    let label = clean(label);
    label
        .strip_prefix("GPU ")
        .map_or_else(|| label.clone(), str::to_owned)
}

/// [`COLUMNS`] with each column's preferred width grown to its title and
/// the widest of its `rows`' cells (a process on several GPUs lists a value
/// for each).
fn fitted_columns<'a>(rows: impl Iterator<Item = &'a [TableCell; 11]>) -> [ColumnSpec; 11] {
    let mut specs = COLUMNS;
    for cells in rows {
        for (spec, cell) in specs.iter_mut().zip(cells) {
            spec.preferred = spec.preferred.max(saturate_u16(display_width(&cell.text)));
        }
    }
    for spec in &mut specs {
        spec.preferred = spec.preferred.max(saturate_u16(display_width(spec.title)));
    }
    specs
}

fn row_cells(
    row: &ProcessRowView,
    selected: bool,
    filter: Option<&EntityId>,
    context: &DisplayContext,
) -> [TableCell; 11] {
    let palette = &context.theme;
    let gpus = gpu_entries(row, filter);
    let marker = if selected { SELECTION_MARKER } else { "" };
    let missing_style = palette.tone_style(crate::format::CellTone::Missing);
    let numeric = |text: String| {
        let style = if text == NOT_AVAILABLE {
            missing_style
        } else {
            palette.text_style()
        };
        TableCell::new(text, style)
    };
    [
        TableCell::new(marker, palette.title_style()),
        TableCell::new(
            joined(&gpus, |gpu| short_gpu_label(&gpu.gpu_label)),
            palette.secondary_style(),
        ),
        TableCell::new(row.key.pid.to_string(), palette.label_style()),
        TableCell::new(clean(&row.user), palette.label_style()),
        numeric(joined(&gpus, |gpu| usage_code(gpu.usage).to_owned())),
        numeric(format_optional_percent(row.cpu_percent)),
        numeric(joined(&gpus, |gpu| {
            format_optional_percent(gpu.utilization_percent)
        })),
        numeric(joined(&gpus, |gpu| format_optional_bytes(gpu.memory_bytes))),
        numeric(format_optional_bytes(row.rss_bytes)),
        swatch_cell(row, context),
        TableCell::new(clean(&row.name), palette.text_style()),
    ]
}

/// `■` in the process's color when it is tracked, else nothing.
fn swatch_cell(row: &ProcessRowView, context: &DisplayContext) -> TableCell {
    match row.color_slot {
        Some(slot) => TableCell::new(
            COLOR_SWATCH,
            ratatui::style::Style::new().fg(context.theme.process_color(slot)),
        ),
        None => TableCell::new("", context.theme.text_style()),
    }
}

/// The selected row's style recolors its cells; the swatch keeps the color
/// it names.
fn keep_swatch_color(
    frame: &mut Frame<'_>,
    row: Rect,
    columns: &[FittedColumn],
    swatch: &TableCell,
) {
    let (Some(column), Some(color)) = (
        columns.iter().find(|column| column.spec == SWATCH_COLUMN),
        swatch.style.fg,
    ) else {
        return;
    };
    let x = row.x.saturating_add(column.x);
    if x < row.right() && !swatch.text.is_empty() {
        frame.buffer_mut()[(x, row.y)].set_fg(color);
    }
}

fn row_label(row: &ProcessRowView, selected: bool, filter: Option<&EntityId>) -> String {
    let gpus = gpu_entries(row, filter);
    format!(
        "{}process pid {} {} gpu {} type {} gpu util {} gpu mem {} cpu {} cpu mem {}",
        if selected { "selected " } else { "" },
        row.key.pid,
        clean(&row.name),
        joined(&gpus, |gpu| short_gpu_label(&gpu.gpu_label)),
        joined(&gpus, |gpu| usage_code(gpu.usage).to_owned()),
        joined(&gpus, |gpu| format_optional_percent(
            gpu.utilization_percent
        )),
        joined(&gpus, |gpu| format_optional_bytes(gpu.memory_bytes)),
        format_optional_percent(row.cpu_percent),
        format_optional_bytes(row.rss_bytes),
    )
}

fn status_text(view: &ProcessesView, window: &DisplayWindow, filter: Option<&EntityId>) -> String {
    let first = window.first_absolute(view).saturating_add(1);
    let last = first
        .saturating_add(u64::try_from(window.count).unwrap_or(u64::MAX))
        .saturating_sub(1);
    let mut text = format!("rows {first}–{last} of {}", view.total_rows);
    if view.truncated {
        text.push_str(" · list truncated at the process limit");
    }
    match view
        .selected
        .as_ref()
        .and_then(|key| view.rows.iter().find(|row| &row.key == key))
    {
        Some(row) => {
            text.push_str(&format!(
                " · selected pid {} {} · start {}",
                row.key.pid,
                clean(&row.name),
                row.key.start_time_ticks
            ));
            if let Some(reason) = row.missing_reason.as_deref() {
                text.push_str(&format!(" · missing: {}", clean(reason)));
            }
            let gpus = gpu_entries(row, filter);
            if gpus.len() > 1 {
                text.push_str(&format!(" · {} GPUs", gpus.len()));
            }
        }
        None if view.selected.is_some() => text.push_str(" · selection outside the visible rows"),
        None => match &view.selected_missing {
            Some(gone) => text.push_str(&format!(
                " · selected pid {} exited; its values are unavailable",
                gone.pid
            )),
            None => text.push_str(" · no selection (j/k to select)"),
        },
    }
    text
}

fn draw_scrollbar(
    frame: &mut Frame<'_>,
    body: Rect,
    view: &ProcessesView,
    window: &DisplayWindow,
    context: &DisplayContext,
) {
    let total = u64::from(view.total_rows);
    let shown = u64::try_from(window.count).unwrap_or(u64::MAX);
    if body.is_empty() || total <= shown || total == 0 {
        return;
    }
    let height = u64::from(body.height);
    let thumb_start = window.first_absolute(view).saturating_mul(height) / total;
    let thumb_length = (shown.saturating_mul(height) / total).max(1);
    let x = body.right() - 1;
    for offset in 0..body.height {
        let position = u64::from(offset);
        let in_thumb = position >= thumb_start && position < thumb_start + thumb_length;
        let (symbol, style) = if in_thumb {
            ("┃", context.theme.title_style())
        } else {
            ("│", context.theme.muted_style())
        };
        frame
            .buffer_mut()
            .set_string(x, body.y + offset, symbol, style);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_fixtures;

    #[test]
    fn status_reports_absolute_rows_and_missing_reasons() {
        let mut view = test_fixtures::processes_view(10, 40, 412);
        view.selected = Some(view.rows[2].key.clone());
        view.rows[2].missing_reason = Some("cpu: permission denied".into());
        view.truncated = true;
        let window = DisplayWindow::new(&view, 8);
        let text = status_text(&view, &window, None);
        assert!(text.starts_with("rows 41–48 of 412"), "{text}");
        assert!(text.contains("truncated"));
        assert!(text.contains("missing: cpu: permission denied"));
    }

    #[test]
    fn an_exited_selection_is_reported_as_unavailable() {
        let mut view = test_fixtures::processes_view(3, 0, 3);
        let gone = view.rows[1].key.clone();
        view.rows.remove(1);
        view.total_rows = 2;
        view.selected = None;
        view.selected_index = None;
        view.selected_missing = Some(gone.clone());
        let window = DisplayWindow::new(&view, 8);
        let text = status_text(&view, &window, None);
        assert!(
            text.contains(&format!("selected pid {} exited", gone.pid)),
            "{text}"
        );
        assert!(text.contains("unavailable"), "{text}");
    }

    #[test]
    fn gpu_filter_limits_values_to_that_gpu() {
        let view = test_fixtures::processes_view(3, 0, 3);
        let multi = &view.rows[0];
        assert!(multi.gpus.len() >= 2);
        let first_gpu = multi.gpus[0].gpu_entity_id.clone();
        assert_eq!(gpu_entries(multi, Some(&first_gpu)).len(), 1);
        assert_eq!(joined(&[], |_| String::new()), "none");
    }

    /// A tracked process's name follows a swatch in its color, also on the
    /// selected row; an untracked one has none.
    #[test]
    fn a_tracked_process_names_its_color_before_its_name() {
        let context = test_fixtures::display_context();
        let mut view = test_fixtures::processes_view(2, 0, 2);
        view.rows[0].color_slot = Some(1);
        let cells = row_cells(&view.rows[0], true, None, &context);
        assert_eq!(cells[SWATCH_COLUMN].text, COLOR_SWATCH);
        assert_eq!(
            cells[SWATCH_COLUMN].style.fg,
            Some(context.theme.process_color(1))
        );
        assert_eq!(
            row_cells(&view.rows[1], false, None, &context)[SWATCH_COLUMN].text,
            ""
        );
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 7)).unwrap();
        let mut model =
            test_fixtures::view_model(fox_services_top_core::commands::ViewKind::Overview, 1);
        model.modules.processes = Some(std::sync::Arc::new(view));
        terminal
            .draw(|frame| {
                ProcessesModule::new()
                    .unwrap()
                    .render(&context, &model, frame.area(), frame);
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let swatch = (0..120)
            .find(|x| buffer[(*x, 4)].symbol() == COLOR_SWATCH)
            .expect("the selected row draws its swatch");
        assert_eq!(buffer[(swatch, 4)].fg, context.theme.process_color(1));
        assert_eq!(buffer[(swatch, 4)].bg, context.theme.highlight_background);
    }

    /// The GPU leads (D-66), CPU% follows the type, the host memory is
    /// `CPU MEM`; columns are as wide as what they show and set apart by
    /// `│`, the swatch kept beside its name.
    #[test]
    fn the_table_leads_with_the_gpu_and_sets_its_columns_apart() {
        let context = test_fixtures::display_context();
        let view = test_fixtures::processes_view(2, 0, 2);
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 7)).unwrap();
        let mut model =
            test_fixtures::view_model(fox_services_top_core::commands::ViewKind::Overview, 1);
        model.modules.processes = Some(std::sync::Arc::new(view));
        terminal
            .draw(|frame| {
                ProcessesModule::new()
                    .unwrap()
                    .render(&context, &model, frame.area(), frame);
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let row = |y: u16| -> String { (0..120).map(|x| buffer[(x, y)].symbol()).collect() };
        // Row 1 is the Overview sub-tab strip; the table starts below it.
        assert!(row(1).contains("GPU Processes"), "{}", row(1));
        assert!(row(1).contains("Fox services"), "{}", row(1));
        let header = row(2);
        let titles = [
            "GPU ", "PID", "USER", "TYPE", "CPU%", "GPU%", "GPU MEM", "CPU MEM", "NAME",
        ];
        let at: Vec<usize> = titles
            .iter()
            .map(|title| {
                header
                    .find(title)
                    .unwrap_or_else(|| panic!("{title}: {header}"))
            })
            .collect();
        assert!(at.windows(2).all(|pair| pair[0] < pair[1]), "{header}");
        assert!(!header.contains("RSS"), "{header}");
        assert_eq!(header.matches(" │ ").count(), 8, "{header}");
        let first = row(4);
        assert!(
            row(3).contains("─┼─"),
            "the rule under the header: {}",
            row(3)
        );
        let bars = |line: &str| -> Vec<usize> {
            line.chars()
                .enumerate()
                .filter(|(_, symbol)| *symbol == '│')
                .map(|(x, _)| x)
                .collect()
        };
        assert_eq!(bars(&header), bars(&first), "aligned:\n{header}\n{first}");
        assert!(
            first.contains("▶ 0,1 │    4242 │ 1000 │ C,C+G │ 12.5% │ "),
            "{first}"
        );
        assert!(
            first.contains(" │ 45.0%,n/a │ 10.0 GiB,2.0 GiB │ "),
            "{first}"
        );
        assert!(
            first.contains(" │   python3 "),
            "no swatch, then the name: {first}"
        );
    }

    /// An empty GPU table keeps its sub-tab row: the "no process" message
    /// goes below the tabs, never over them.
    #[test]
    fn an_empty_table_keeps_its_sub_tabs_above_the_message() {
        let context = test_fixtures::display_context();
        let view = test_fixtures::processes_view(0, 0, 0);
        let mut model =
            test_fixtures::view_model(fox_services_top_core::commands::ViewKind::Overview, 1);
        model.modules.processes = Some(std::sync::Arc::new(view));
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 7)).unwrap();
        terminal
            .draw(|frame| {
                ProcessesModule::new()
                    .unwrap()
                    .render(&context, &model, frame.area(), frame);
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let lines: Vec<String> = (0..7)
            .map(|y| (0..120).map(|x| buffer[(x, y)].symbol()).collect())
            .collect();
        let tabs = lines
            .iter()
            .position(|line| line.contains("Fox services"))
            .expect("the sub-tab row is drawn");
        assert!(
            lines[tabs].contains("GPU Processes") && lines[tabs].contains("Power"),
            "tabs intact: {}",
            lines[tabs]
        );
        let message = lines
            .iter()
            .position(|line| line.contains("no process is using a GPU"))
            .expect("the empty message is drawn");
        assert!(
            message > tabs,
            "message below the tabs:\n{}",
            lines.join("\n")
        );
    }

    /// A gray rule sets the header apart, and every process takes one row,
    /// with no blank row between two (D-100).
    #[test]
    fn each_process_takes_one_row_with_no_blank_row_between() {
        let context = test_fixtures::display_context();
        let draw = |count: usize, height: u16| -> Vec<String> {
            let view = test_fixtures::processes_view(count, 0, u32::try_from(count).unwrap());
            let mut model =
                test_fixtures::view_model(fox_services_top_core::commands::ViewKind::Overview, 1);
            model.modules.processes = Some(std::sync::Arc::new(view));
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, height)).unwrap();
            terminal
                .draw(|frame| {
                    ProcessesModule::new()
                        .unwrap()
                        .render(&context, &model, frame.area(), frame);
                })
                .unwrap();
            let buffer = terminal.backend().buffer();
            (0..height)
                .map(|y| (0..120).map(|x| buffer[(x, y)].symbol()).collect())
                .collect()
        };
        let rows_of = |lines: &[String]| -> Vec<usize> {
            lines
                .iter()
                .enumerate()
                .filter(|(_, line)| line.contains("│ 1000 │"))
                .map(|(y, _)| y)
                .collect()
        };
        // Three processes in a body of five rows: still one row each.
        // (The strip above the header costs one row.)
        let few = draw(3, 10);
        assert!(few[3].contains("─┼─"), "{}", few.join("\n"));
        assert_eq!(rows_of(&few), vec![4, 5, 6], "{}", few.join("\n"));
        // Six processes in the same body of four rows: one row each.
        let packed = draw(6, 10);
        assert_eq!(rows_of(&packed), vec![4, 5, 6, 7], "{}", packed.join("\n"));
    }

    #[test]
    fn the_type_column_names_each_gpus_contexts() {
        let view = test_fixtures::processes_view(2, 0, 2);
        let cells = row_cells(
            &view.rows[0],
            false,
            None,
            &test_fixtures::display_context(),
        );
        assert_eq!(cells[4].text, "C,C+G");
        let filtered = gpu_entries(&view.rows[1], None);
        assert_eq!(
            joined(&filtered, |gpu| usage_code(gpu.usage).to_owned()),
            "G"
        );
        assert_eq!(usage_code(GpuUsageKind::Unknown), NOT_AVAILABLE);
        assert!(row_label(&view.rows[0], false, None).contains("type C,C+G"));
    }
}
