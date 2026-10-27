//! Fox-Services module: which app uses which LLM model, per request.
//!
//! Aggregated from actual LLM requests over the window the cluster monitor
//! reads (`remote.service_window_hours`) — not compose config. One table
//! lists every service with its status and the models it used with real
//! token counts; a second lists the newest requests with attribution. The
//! status comes from the node itself (it matches services to compose
//! projects server-side). Read-only; `j`/`k` scroll the recent requests,
//! `/` filters both tables by service or model.

use fox_services_top_core::view::{RecentRequestView, ServiceRowView, ServicesView, ViewModel};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use super::ModuleBuildError;
use super::common::{
    ColumnSpec, ManifestSpec, Output, RowCursor, TableCell, build_manifest, draw_line,
    draw_table_header, draw_table_row, fit_columns, message, no_data, panel_with_keys,
};
use crate::format::{clean, format_compact_count, format_short_age};
use crate::num::saturate_u16;
use crate::registry::{DisplayContext, DisplayModule, DisplayModuleManifest, RenderOutput, Slot};

/// The keys acting on the view, at the right of its title.
const SERVICES_KEYS: &str = "j/k scroll recent · / filter";

/// `model (countx, tokens tok)` cells of the service table.
const SERVICE_COLUMNS: [ColumnSpec; 6] = [
    ColumnSpec::left("SERVICE", 12, 100).flexible().prefer(20),
    ColumnSpec::left("STATUS", 12, 90),
    ColumnSpec::left("LLM MODELS", 16, 90).flexible().prefer(30),
    ColumnSpec::right("REQS", 6, 70),
    ColumnSpec::right("TOKENS", 9, 70),
    ColumnSpec::right("LAST", 9, 70),
];

const RECENT_COLUMNS: [ColumnSpec; 7] = [
    ColumnSpec::right("TIME", 8, 80),
    ColumnSpec::left("SERVICE", 10, 95).flexible().prefer(16),
    ColumnSpec::left("MODEL", 12, 95).flexible().prefer(18),
    ColumnSpec::right("TOKENS P/C/T", 14, 75),
    ColumnSpec::right("DURATION", 13, 75),
    ColumnSpec::left("CONTENT", 14, 75),
    ColumnSpec::left("PROMPT", 10, 90).flexible().prefer(26),
];

/// What the status column means, under the service table.
const STATUS_LEGEND: &str = "Status: Busy serving now · Live traffic <5m · Running container up · Unreachable · Stopped · Not deployed";

pub struct DockerModule {
    manifest: DisplayModuleManifest,
}

impl DockerModule {
    pub fn new() -> Result<Self, ModuleBuildError> {
        Ok(Self {
            manifest: build_manifest(ManifestSpec {
                id: "docker",
                title: "Fox-Services",
                min_columns: 40,
                min_rows: 8,
                priority: 29,
                slots: &[Slot::Detail],
            })?,
        })
    }
}

impl DisplayModule for DockerModule {
    fn manifest(&self) -> &DisplayModuleManifest {
        &self.manifest
    }

    fn preferred_rows(&self, model: &ViewModel, _width: u16, slot: Slot) -> Option<u16> {
        if slot != Slot::Detail {
            return None;
        }
        let services = model.modules.docker.as_deref()?;
        if !services.enabled {
            return None;
        }
        Some(saturate_u16(
            // Subtitle, service table, legend, recent caption, recent rows.
            3 + services.services.len().min(12) + services.recent.len().min(14),
        ))
    }

    fn render(
        &self,
        context: &DisplayContext,
        model: &ViewModel,
        area: Rect,
        frame: &mut Frame<'_>,
    ) -> RenderOutput {
        let mut output = Output::default();
        let Some(services) = model.modules.docker.as_deref() else {
            let inner = panel_with_keys(frame, area, "Fox-Services", SERVICES_KEYS, context);
            message(
                frame,
                inner,
                &no_data("services"),
                context.theme.muted_style(),
            );
            output.label("services: no data");
            return output.finish();
        };
        let window = if services.window_label.is_empty() {
            "live LLM usage".to_owned()
        } else {
            format!("live LLM usage ({})", services.window_label)
        };
        let inner = panel_with_keys(
            frame,
            area,
            &format!(
                "Fox-Services · {window}{}",
                if services.stale { " · last read" } else { "" }
            ),
            SERVICES_KEYS,
            context,
        );
        if !services.enabled {
            message(
                frame,
                inner,
                "remote monitoring is disabled (config remote.enabled)",
                context.theme.muted_style(),
            );
            output.label("services: disabled");
            return output.finish();
        }
        let mut rows = RowCursor::new(inner);
        draw_line(
            frame.buffer_mut(),
            rows.next_row().unwrap_or_default(),
            &Line::from(Span::styled(
                "Live: which app uses which LLM model, per request — actual requests, not compose config.",
                context.theme.muted_style(),
            )),
        );
        let filter = model.session.filter.to_lowercase();
        render_services_content(
            frame,
            &mut rows,
            services,
            &filter,
            context,
            &mut output,
            true,
        );
        output.finish()
    }
}

/// The Fox-Services tables: services with their models, the status legend,
/// then the recent requests. The Docker tab and the Overview's Fox services
/// sub-tab share them; `legend` keeps the legend line on the full tab.
pub(crate) fn render_services_content(
    frame: &mut Frame<'_>,
    rows: &mut RowCursor,
    services: &ServicesView,
    filter: &str,
    context: &DisplayContext,
    output: &mut Output,
    legend: bool,
) {
    let matching: Vec<&ServiceRowView> = services
        .services
        .iter()
        .filter(|row| {
            filter.is_empty()
                || row.service.to_lowercase().contains(filter)
                || row
                    .models
                    .iter()
                    .any(|model| model.model.to_lowercase().contains(filter))
        })
        .collect();
    if matching.is_empty() {
        if let Some(area) = rows.rest() {
            message(
                frame,
                area,
                "no LLM requests in this window",
                context.theme.muted_style(),
            );
        }
        output.label("services: idle");
        return;
    }
    render_services(frame, rows, &matching, context, output);
    if legend && let Some(row) = rows.next_row() {
        draw_line(
            frame.buffer_mut(),
            row,
            &Line::from(Span::styled(STATUS_LEGEND, context.theme.muted_style())),
        );
    }
    let recent: Vec<&RecentRequestView> = services
        .recent
        .iter()
        .filter(|row| {
            filter.is_empty()
                || row.service.to_lowercase().contains(filter)
                || row.model.to_lowercase().contains(filter)
                || row.query_type.to_lowercase().contains(filter)
                || row.prompt.to_lowercase().contains(filter)
        })
        .collect();
    if !recent.is_empty() {
        render_recent(frame, rows, &recent, context, output);
    }
}

fn render_services(
    frame: &mut Frame<'_>,
    rows: &mut RowCursor,
    services: &[&ServiceRowView],
    context: &DisplayContext,
    output: &mut Output,
) {
    // One header plus one row per service.
    let Some(table) = rows.take(saturate_u16(services.len() + 1).min(rows.remaining().max(3)))
    else {
        return;
    };
    let mut cursor = RowCursor::new(table);
    let Some(header) = cursor.next_row() else {
        return;
    };
    let columns = fit_columns(&SERVICE_COLUMNS, header.width);
    draw_table_header(
        frame.buffer_mut(),
        header,
        &SERVICE_COLUMNS,
        &columns,
        context.theme.title_style(),
    );
    for service in services {
        if cursor.remaining() == 0 {
            break;
        }
        let Some(row) = cursor.next_row() else { break };
        draw_table_row(
            frame.buffer_mut(),
            row,
            &columns,
            &service_cells(service, context),
            None,
        );
        output.label(format!("service {}", service.service));
    }
}

fn service_cells(service: &ServiceRowView, context: &DisplayContext) -> Vec<TableCell> {
    let models = service
        .models
        .iter()
        .map(|model| {
            format!(
                "{} ({}x, {} tok)",
                clean(&model.model),
                model.requests,
                format_compact_count(model.tokens as f64)
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    vec![
        TableCell::new(clean(&service.service), context.theme.text_style()),
        TableCell::new(
            clean(&service.status),
            status_style(&service.status, context),
        ),
        TableCell::new(models, context.theme.text_style()),
        TableCell::new(service.requests.to_string(), context.theme.text_style()),
        TableCell::new(
            format_compact_count(service.tokens as f64),
            context.theme.text_style(),
        ),
        TableCell::new(
            service
                .ago_s
                .map_or_else(|| "–".to_owned(), format_short_age),
            context.theme.muted_style(),
        ),
    ]
}

/// The status cell reads at a glance: serving now is warm, live is green,
/// anything down is red or muted.
fn status_style(status: &str, context: &DisplayContext) -> Style {
    let color = match status {
        "Busy" => context.theme.warning,
        "Live" => context.theme.success,
        "Running" => context.theme.secondary,
        "Unreachable" => context.theme.danger,
        _ => context.theme.text_muted,
    };
    Style::new().fg(color)
}

fn render_recent(
    frame: &mut Frame<'_>,
    rows: &mut RowCursor,
    recent: &[&RecentRequestView],
    context: &DisplayContext,
    output: &mut Output,
) {
    if rows.remaining() < 3 {
        return;
    }
    let Some(caption) = rows.next_row() else {
        return;
    };
    draw_line(
        frame.buffer_mut(),
        caption,
        &Line::from(Span::styled(
            "Recent LLM Requests (per-request attribution)",
            context.theme.title_style(),
        )),
    );
    // One header plus one row per request; `j`/`k` scroll this table.
    let Some(table) = rows.take(saturate_u16(recent.len() + 1).min(rows.remaining().max(2))) else {
        return;
    };
    let mut cursor = RowCursor::new(table);
    let Some(header) = cursor.next_row() else {
        return;
    };
    let columns = fit_columns(&RECENT_COLUMNS, header.width);
    draw_table_header(
        frame.buffer_mut(),
        header,
        &RECENT_COLUMNS,
        &columns,
        context.theme.title_style(),
    );
    let mut skip = usize::from(context.scroll);
    for request in recent {
        if cursor.remaining() == 0 {
            break;
        }
        if skip > 0 {
            skip -= 1;
            continue;
        }
        let Some(row) = cursor.next_row() else { break };
        draw_table_row(
            frame.buffer_mut(),
            row,
            &columns,
            &recent_cells(request, context),
            None,
        );
        output.label(format!("request {} {}", request.service, request.model));
    }
}

fn recent_cells(request: &RecentRequestView, context: &DisplayContext) -> Vec<TableCell> {
    let model = if request.status == "complete" || request.status.is_empty() {
        clean(&request.model)
    } else {
        format!("{} · {}", clean(&request.model), clean(&request.status))
    };
    let duration = request.duration_ms.map_or_else(
        || "–".to_owned(),
        |ms| match request.queue_ms {
            Some(queue) => format!("{ms:.0}ms q{queue:.0}ms"),
            None => format!("{ms:.0}ms"),
        },
    );
    let content = match (request.query_type.as_str(), request.requestor.as_str()) {
        ("", "") => "–".to_owned(),
        (query, "") => clean(query),
        ("", requestor) => clean(requestor),
        (query, requestor) => format!("{} · {}", clean(query), clean(requestor)),
    };
    vec![
        TableCell::new(format_short_age(request.ago_s), context.theme.muted_style()),
        TableCell::new(clean(&request.service), context.theme.text_style()),
        TableCell::new(model, context.theme.text_style()),
        TableCell::new(
            format!(
                "{}/{}/{}",
                request.prompt_tokens, request.completion_tokens, request.total_tokens
            ),
            context.theme.text_style(),
        ),
        TableCell::new(duration, context.theme.text_style()),
        TableCell::new(content, context.theme.muted_style()),
        TableCell::new(
            clean(&request.prompt).replace('\n', " "),
            context.theme.muted_style(),
        ),
    ]
}
