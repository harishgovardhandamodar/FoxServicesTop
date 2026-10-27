//! Tokens module: ingress and egress tokens per app and per model, over
//! the window the cluster monitor aggregates (`remote.token_window_hours`),
//! with the engine's models below.
//!
//! Ingress is prompt tokens (what an app sends), egress is completion
//! tokens (what it receives). Apps read most egress first; each app's
//! models follow it. The engine block lists Ollama's models — installed,
//! loaded and resident in memory/VRAM. Read-only: nothing here starts,
//! unloads or switches a model.

use fox_services_top_core::view::{
    TokenAppView, TokenEngineView, TokenModelView, TokensView, ViewModel,
};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use super::ModuleBuildError;
use super::common::{
    ColumnSpec, ManifestSpec, Output, RowCursor, TableCell, build_manifest, draw_caption,
    draw_colored_header, draw_line, draw_table_row, fit_columns, message, no_data, panel_with_keys,
    render_chart_with_parts,
};
use crate::format::{clean, format_bytes, format_tps, pretty_model_name};
use crate::registry::{
    DisplayContext, DisplayModule, DisplayModuleManifest, OverviewCell, RenderOutput, Slot,
};
use crate::theme::ThemePalette;

/// The keys acting on the view, at the right of its title.
const TOKENS_KEYS: &str = "j/k scroll · / filter";

/// Rows the tokens-per-second chart takes: its caption plus the plot.
const CHART_ROWS: u16 = 4;

const APP_COLUMNS: [ColumnSpec; 9] = [
    ColumnSpec::left("APP", 12, 100).flexible().prefer(20),
    ColumnSpec::right("REQS", 6, 70),
    ColumnSpec::right("INGRESS", 12, 90),
    ColumnSpec::right("EGRESS", 12, 95),
    ColumnSpec::right("IN TPS", 9, 65),
    ColumnSpec::right("OUT TPS", 9, 68),
    ColumnSpec::right("AVG", 9, 60),
    ColumnSpec::right("TOTAL", 11, 50),
    ColumnSpec::left("MODEL", 14, 80).flexible().prefer(24),
];

pub struct TokensModule {
    manifest: DisplayModuleManifest,
}

impl TokensModule {
    pub fn new() -> Result<Self, ModuleBuildError> {
        Ok(Self {
            manifest: build_manifest(ManifestSpec {
                id: "tokens",
                title: "Tokens",
                min_columns: 28,
                // The Overview card shares a row, so its floor is small; the
                // tab's rows come from `preferred_rows`.
                min_rows: 2,
                priority: 28,
                // Also a card on the Overview: the ingress/egress summary.
                slots: &[Slot::Overview, Slot::Detail],
            })?,
        })
    }
}

impl DisplayModule for TokensModule {
    fn manifest(&self) -> &DisplayModuleManifest {
        &self.manifest
    }

    /// Shares the disk/network row: a summary card that costs the Overview
    /// no height (D-118).
    fn overview_cell(&self) -> Option<OverviewCell> {
        Some(OverviewCell::at(2, 2, 30))
    }

    /// The Overview card follows the height of the panels it shares its row
    /// with; the tab sizes itself to the apps and engines it lists.
    fn preferred_rows(&self, model: &ViewModel, _width: u16, slot: Slot) -> Option<u16> {
        if slot != Slot::Detail {
            return None;
        }
        let tokens = model.modules.tokens.as_deref()?;
        if !tokens.enabled {
            return None;
        }
        Some(
            // The chart's caption and rows lead the tab.
            CHART_ROWS
                + crate::num::saturate_u16(2 + tokens.apps.len().min(10) * 3)
                + crate::num::saturate_u16(tokens.engines.len().min(2) * 2),
        )
    }

    fn render(
        &self,
        context: &DisplayContext,
        model: &ViewModel,
        area: Rect,
        frame: &mut Frame<'_>,
    ) -> RenderOutput {
        let mut output = Output::default();
        let Some(tokens) = model.modules.tokens.as_deref() else {
            let inner = panel_with_keys(frame, area, "Tokens", TOKENS_KEYS, context);
            message(
                frame,
                inner,
                &no_data("tokens"),
                context.theme.muted_style(),
            );
            output.label("tokens: no data");
            return output.finish();
        };
        let window = if tokens.window_label.is_empty() {
            "Tokens".to_owned()
        } else {
            format!("Tokens · {}", tokens.window_label)
        };
        // Totals carried over from the last answered poll stay marked, so a
        // busy node never reads as fresh data (D-118).
        let totals = format!(
            "{} in / {} out{}",
            format_bytes(tokens.total_ingress),
            format_bytes(tokens.total_egress),
            if tokens.stale { " · last read" } else { "" }
        );
        let inner = panel_with_keys(
            frame,
            area,
            &format!("{window} · {totals}"),
            TOKENS_KEYS,
            context,
        );
        if !tokens.enabled {
            message(
                frame,
                inner,
                "remote monitoring is disabled (config remote.enabled)",
                context.theme.muted_style(),
            );
            output.label("tokens: disabled");
            return output.finish();
        }
        let mut rows = RowCursor::new(inner);
        let filter = model.session.filter.to_lowercase();
        let apps: Vec<&TokenAppView> = tokens
            .apps
            .iter()
            .filter(|app| filter.is_empty() || app.app.to_lowercase().contains(&filter))
            .collect();
        if apps.is_empty() {
            message(
                frame,
                inner,
                "no token traffic in this window",
                context.theme.muted_style(),
            );
            output.label("tokens: idle");
            return output.finish();
        }
        // The Overview card keeps a summary; the tab lists every app.
        let flow = Flow::of(tokens);
        if context.slot != Slot::Detail {
            render_summary(
                frame,
                &mut rows,
                &apps,
                &tokens.engines,
                &flow,
                context,
                &mut output,
            );
            return output.finish();
        }
        render_chart(frame, &mut rows, &flow, context);
        render_apps(frame, &mut rows, &apps, context, &mut output);
        render_engines(frame, &mut rows, &tokens.engines, context, &mut output);
        output.finish()
    }
}

/// The two TPS series of the timeline, oldest bucket first: ingress above,
/// egress at the foot of each column.
struct Flow {
    egress: Vec<Option<f64>>,
    totals: Vec<Option<f64>>,
    peak_ingress: f64,
    peak_egress: f64,
}

impl Flow {
    fn of(tokens: &TokensView) -> Self {
        let mut flow = Self {
            egress: Vec::new(),
            totals: Vec::new(),
            peak_ingress: 0.0,
            peak_egress: 0.0,
        };
        for bucket in &tokens.timeline {
            let ingress = bucket.ingress_tps.max(0.0);
            let egress = bucket.egress_tps.max(0.0);
            flow.peak_ingress = flow.peak_ingress.max(ingress);
            flow.peak_egress = flow.peak_egress.max(egress);
            flow.egress.push(Some(egress));
            flow.totals.push(Some(ingress + egress));
        }
        flow
    }
}

fn total_of(model: &TokenModelView) -> f64 {
    model.ingress_tps + model.egress_tps
}

/// Ingress trend color: the chart's base (`sparkline`), so the IN TPS text
/// reads as the same series.
fn ingress_style(theme: &ThemePalette) -> Style {
    Style::new().fg(theme.sparkline)
}

/// Egress trend color: the chart's foot part (`accent`), so the OUT TPS
/// text reads as the same series.
fn egress_style(theme: &ThemePalette) -> Style {
    Style::new().fg(theme.accent)
}

fn has_tps(value: f64) -> bool {
    value.is_finite() && value > 0.0
}

/// TPS text in its trend color; `n/a` stays muted so absence never reads
/// as activity.
fn tps_style(value: f64, trend: Style, theme: &ThemePalette) -> Style {
    if has_tps(value) {
        trend
    } else {
        theme.muted_style()
    }
}

/// The tokens-per-second chart over the window, with its caption: both
/// directions stacked per bucket, newest bucket at the right edge.
fn render_chart(
    frame: &mut Frame<'_>,
    rows: &mut RowCursor,
    flow: &Flow,
    context: &DisplayContext,
) {
    if rows.remaining() == 0 {
        return;
    }
    let caption = rows.next_row();
    let chart_rows = rows.take(rows.remaining().min(3));
    if let Some(row) = caption {
        draw_caption(
            frame.buffer_mut(),
            row,
            &Line::from(vec![
                Span::styled("tokens per second", context.theme.title_style()),
                Span::styled(" · peak in ", context.theme.muted_style()),
                Span::styled(
                    format_tps(flow.peak_ingress),
                    tps_style(
                        flow.peak_ingress,
                        ingress_style(&context.theme),
                        &context.theme,
                    ),
                ),
                Span::styled(" / out ", context.theme.muted_style()),
                Span::styled(
                    format_tps(flow.peak_egress),
                    tps_style(
                        flow.peak_egress,
                        egress_style(&context.theme),
                        &context.theme,
                    ),
                ),
            ]),
            Some("egress at the foot, ingress above"),
            context,
        );
    }
    let Some(area) = chart_rows else { return };
    render_chart_with_parts(
        frame,
        area,
        &flow.totals,
        (
            vec![(
                flow.egress.as_slice(),
                Style::new().fg(context.theme.accent),
            )],
            Some(context.theme.sparkline),
        ),
        None,
        context,
    );
}

/// The Overview card: the rate chart, the busiest model, then the engine.
fn render_summary(
    frame: &mut Frame<'_>,
    rows: &mut RowCursor,
    apps: &[&TokenAppView],
    engines: &[TokenEngineView],
    flow: &Flow,
    context: &DisplayContext,
    output: &mut Output,
) {
    let engine = engines.first().map(engine_summary);
    let mut models = busiest_models(apps);
    // The chart and the engine keep their rows; models fill what is left.
    let fixed_rows = usize::from(!models.is_empty()) + usize::from(engine.is_some());
    let budget = (rows.remaining() as usize).saturating_sub(fixed_rows + 1);
    if let Some(area) = rows.take(crate::num::saturate_u16(budget.min(2))) {
        render_chart_with_parts(
            frame,
            area,
            &flow.totals,
            (
                vec![(
                    flow.egress.as_slice(),
                    Style::new().fg(context.theme.accent),
                )],
                Some(context.theme.sparkline),
            ),
            None,
            context,
        );
    }
    if !models.is_empty() {
        let remaining = (rows.remaining() as usize).saturating_sub(usize::from(engine.is_some()));
        for model in models.drain(..).take(remaining.max(1)) {
            let Some(row) = rows.next_row() else { break };
            draw_line(
                frame.buffer_mut(),
                row,
                &model_line(model, context.theme.text_style(), &context.theme),
            );
            output.label(format!("tokens {}", clean(&model.model)));
        }
    }
    if let Some(text) = engine
        && let Some(row) = rows.next_row()
    {
        draw_line(
            frame.buffer_mut(),
            row,
            &Line::from(Span::styled(text, context.theme.muted_style())),
        );
    }
}

/// Every model of every app, busiest first.
fn busiest_models<'a>(apps: &[&'a TokenAppView]) -> Vec<&'a TokenModelView> {
    let mut models: Vec<&TokenModelView> = apps.iter().flat_map(|app| app.models.iter()).collect();
    models.sort_by(|a, b| {
        total_of(b)
            .partial_cmp(&total_of(a))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    models
}

fn model_line(model: &TokenModelView, base: Style, theme: &ThemePalette) -> Line<'static> {
    Line::from(vec![
        Span::styled(pretty_model_name(&model.model), base),
        Span::styled(" · ingress ", base),
        Span::styled(
            format_tps(model.ingress_tps),
            tps_style(model.ingress_tps, ingress_style(theme), theme),
        ),
        Span::styled(" TPS · egress ", base),
        Span::styled(
            format_tps(model.egress_tps),
            tps_style(model.egress_tps, egress_style(theme), theme),
        ),
        Span::styled(" TPS · ", base),
        Span::styled(format!("{} reqs", model.requests), base),
    ])
}

fn engine_summary(engine: &TokenEngineView) -> String {
    if engine.available {
        format!(
            "{} · {}/{} loaded · {} resident",
            clean(&engine.engine),
            engine.loaded_count,
            engine.model_count,
            format_bytes(engine.vram_total_bytes)
        )
    } else {
        format!("{} not available", clean(&engine.engine))
    }
}

fn render_apps(
    frame: &mut Frame<'_>,
    rows: &mut RowCursor,
    apps: &[&TokenAppView],
    context: &DisplayContext,
    output: &mut Output,
) {
    // One header plus one row per app and per model.
    let wanted = apps.len()
        + 1
        + apps
            .iter()
            .map(|app| app.models.len().min(4))
            .sum::<usize>();
    let Some(table) = rows.take(crate::num::saturate_u16(wanted).min(rows.remaining().max(4)))
    else {
        return;
    };
    let mut cursor = RowCursor::new(table);
    let Some(header) = cursor.next_row() else {
        return;
    };
    let columns = fit_columns(&APP_COLUMNS, header.width);
    draw_colored_header(
        frame.buffer_mut(),
        header,
        &APP_COLUMNS,
        &columns,
        |spec| match spec {
            4 => ingress_style(&context.theme),
            5 => egress_style(&context.theme),
            _ => context.theme.title_style(),
        },
    );
    for app in apps {
        if cursor.remaining() == 0 {
            break;
        }
        let Some(row) = cursor.next_row() else { break };
        draw_table_row(
            frame.buffer_mut(),
            row,
            &columns,
            &app_cells(app, &context.theme),
            None,
        );
        output.label(format!("tokens {}", app.app));
        // One row per model the app used, with its rate.
        for model in app.models.iter().take(4) {
            let Some(row) = cursor.next_row() else { break };
            let mut line = model_line(model, context.theme.muted_style(), &context.theme);
            line.spans
                .insert(0, Span::styled("  ", context.theme.muted_style()));
            draw_line(frame.buffer_mut(), row, &line);
        }
    }
}

fn app_cells(app: &TokenAppView, theme: &ThemePalette) -> Vec<TableCell> {
    let total = app.ingress_tokens.saturating_add(app.egress_tokens);
    let top = app
        .models
        .first()
        .map_or_else(|| "–".to_owned(), |model| clean(&model.model));
    let style = theme.text_style();
    vec![
        TableCell::new(clean(&app.app), style),
        TableCell::new(app.requests.to_string(), style),
        TableCell::new(format_bytes(app.ingress_tokens), style),
        TableCell::new(format_bytes(app.egress_tokens), style),
        TableCell::new(
            format_tps(app.ingress_tps),
            tps_style(app.ingress_tps, ingress_style(theme), theme),
        ),
        TableCell::new(
            format_tps(app.egress_tps),
            tps_style(app.egress_tps, egress_style(theme), theme),
        ),
        TableCell::new(
            app.avg_duration_ms
                .map_or_else(|| "–".to_owned(), |ms| format!("{ms:.0} ms")),
            style,
        ),
        TableCell::new(format_bytes(total), style),
        TableCell::new(top, style),
    ]
}

fn render_engines(
    frame: &mut Frame<'_>,
    rows: &mut RowCursor,
    engines: &[TokenEngineView],
    context: &DisplayContext,
    output: &mut Output,
) {
    for engine in engines {
        let Some(header) = rows.next_row() else { break };
        let heading = if engine.available {
            format!(
                "{} models · {}/{} loaded · {} resident",
                engine.engine,
                engine.loaded_count,
                engine.model_count,
                format_bytes(engine.vram_total_bytes),
            )
        } else {
            format!("{} not available on this node", engine.engine)
        };
        draw_line(
            frame.buffer_mut(),
            header,
            &Line::from(Span::styled(heading, context.theme.title_style())),
        );
        output.label(format!("engine {}", engine.engine));
        for model in engine.models.iter().take(6) {
            if rows.remaining() == 0 {
                break;
            }
            let Some(row) = rows.next_row() else { break };
            let state = if model.running { "loaded" } else { "idle" };
            let size = model.parameter_size.as_str();
            let quant = model.quantization.as_str();
            let detail = match (size, quant) {
                ("", "") => String::new(),
                (size, "") => format!(" · {size}"),
                ("", quant) => format!(" · {quant}"),
                (size, quant) => format!(" · {size} {quant}"),
            };
            let vram = model.size_vram_bytes.map_or_else(String::new, |bytes| {
                format!(" · {} VRAM", format_bytes(bytes))
            });
            draw_line(
                frame.buffer_mut(),
                row,
                &Line::from(Span::styled(
                    format!(
                        "  {} · {state} · {}{vram}{detail}",
                        clean(&model.model),
                        format_bytes(model.size_bytes)
                    ),
                    context.theme.muted_style(),
                )),
            );
        }
    }
}
