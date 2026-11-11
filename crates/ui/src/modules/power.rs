//! Power module: energy / power consumption timeline and per-query kWh,
//! plus a priced total for a selectable country and database window.
//!
//! The watts timeline draws the host total over the history's window (the
//! session's Overview History bar size, `w`/`W` to change); the series table
//! lists total, CPU packages and GPUs with now/mean/peak watts and kWh over
//! the window; the windows table lists kWh per series over 1 h, 6 h, 12 h
//! and 24 h; the cost row prices the total of one window (`t`/`T` to pick
//! the window) and the tariff list below prices it per country (`j`/`k`
//! move the selected row, live on the cost above). The priced energy is what
//! the local database holds. Read-only: nothing here changes a power limit
//! or cap.

use fox_services_top_core::metric::CanonicalUnit;
use fox_services_top_core::view::{PowerView, PowerWindowView, ViewModel};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use super::ModuleBuildError;
use super::common::{
    ColumnSpec, ManifestSpec, Output, RowCursor, TableCell, build_manifest, draw_caption,
    draw_table_header, draw_table_row, fit_columns, message, no_data, panel_with_keys,
    render_chart,
};
use crate::format::{
    COST_WINDOWS, TARIFFS, clean, format_cost_eur, format_energy, format_span_ns, format_value,
    tariff,
};
use crate::registry::{DisplayContext, DisplayModule, DisplayModuleManifest, RenderOutput, Slot};

use super::history::{
    FULL_COVERAGE, history_nanos_to_seconds as nanos_to_seconds,
    history_recorded_text as recorded_text, why_partly,
};

/// The keys acting on the view, at the right of its title.
const POWER_KEYS: &str = "w/W timescale · j/k country · t/T cost range · / filter";

/// Rows the cost estimate takes: its caption plus the priced total.
const COST_ROWS: u16 = 2;

/// Rows the tariff list takes: its caption plus one row per country.
fn tariff_rows() -> u16 {
    1 + crate::num::saturate_u16(crate::format::TARIFFS.len())
}

/// Rows the watts chart takes: its caption plus the plot.
const CHART_ROWS: u16 = 4;

/// Per-series windows of the kWh table, in order.
const WINDOW_ORDER: [&str; 4] = ["1h", "6h", "12h", "24h"];

const SERIES_COLUMNS: [ColumnSpec; 7] = [
    ColumnSpec::left("SERIES", 12, 100).flexible().prefer(22),
    ColumnSpec::right("NOW", 9, 80),
    ColumnSpec::right("MEAN", 9, 75),
    ColumnSpec::right("PEAK", 9, 70),
    ColumnSpec::right("KWH", 10, 90),
    ColumnSpec::right("COST", 9, 80),
    ColumnSpec::right("COVER", 7, 60),
];

const WINDOW_COLUMNS: [ColumnSpec; 5] = [
    ColumnSpec::left("SERIES", 12, 100).flexible().prefer(22),
    ColumnSpec::right("1H", 9, 80),
    ColumnSpec::right("6H", 9, 75),
    ColumnSpec::right("12H", 9, 70),
    ColumnSpec::right("24H", 9, 65),
];

pub struct PowerModule {
    manifest: DisplayModuleManifest,
}

impl PowerModule {
    pub fn new() -> Result<Self, ModuleBuildError> {
        Ok(Self {
            manifest: build_manifest(ManifestSpec {
                id: "power",
                title: "Power",
                min_columns: 28,
                min_rows: 2,
                priority: 27,
                slots: &[Slot::Overview, Slot::Detail],
            })?,
        })
    }
}

impl DisplayModule for PowerModule {
    fn manifest(&self) -> &DisplayModuleManifest {
        &self.manifest
    }

    fn include_in_overview(&self, model: &ViewModel) -> bool {
        model.session.view.is_fox_services_dashboard()
    }

    fn preferred_rows(&self, model: &ViewModel, _width: u16, slot: Slot) -> Option<u16> {
        if slot == Slot::Overview {
            return Some(5);
        }
        if slot != Slot::Detail {
            return None;
        }
        let power = model.modules.power.as_deref()?;
        Some(
            CHART_ROWS
                + crate::num::saturate_u16(power.rows.len() + 1)
                + crate::num::saturate_u16(window_groups(&power.windows).len() + 1)
                + COST_ROWS
                + tariff_rows(),
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
        let Some(power) = model.modules.power.as_deref() else {
            let inner = panel_with_keys(frame, area, "Power", POWER_KEYS, context);
            message(frame, inner, &no_data("power"), context.theme.muted_style());
            output.label("power: no data");
            return output.finish();
        };
        let window = if power.window_label.is_empty() {
            "Power".to_owned()
        } else {
            format!("Power · {}", power.window_label)
        };
        let totals = format!(
            "{} · {}",
            power.total_w.map_or_else(
                || "n/a".to_owned(),
                |watts| format_value(watts, CanonicalUnit::Watts)
            ),
            power
                .total_joules
                .map_or_else(|| "n/a".to_owned(), format_energy),
        );
        let inner = panel_with_keys(
            frame,
            area,
            &format!("{window} · {totals}"),
            POWER_KEYS,
            context,
        );
        let mut rows = RowCursor::new(inner);
        let filter = model.session.filter.to_lowercase();
        if power.rows.is_empty() {
            message(
                frame,
                inner,
                "no power data in this window",
                context.theme.muted_style(),
            );
            output.label("power: idle");
            return output.finish();
        }
        render_chart_block(frame, &mut rows, power, context);
        render_series(frame, &mut rows, power, &filter, context, &mut output);
        render_windows(frame, &mut rows, power, &filter, context, &mut output);
        render_cost(frame, &mut rows, power, context, &mut output);
        render_tariffs(frame, &mut rows, context, &mut output);
        output.finish()
    }
}

/// Windows grouped by series, in row order.
fn window_groups(windows: &[PowerWindowView]) -> Vec<Vec<&PowerWindowView>> {
    let mut groups: Vec<Vec<&PowerWindowView>> = Vec::new();
    for window in windows {
        match groups.last_mut() {
            Some(group)
                if group.first().is_some_and(|first: &&PowerWindowView| {
                    first.subject == window.subject && first.kind == window.kind
                }) =>
            {
                group.push(window);
            }
            _ => groups.push(vec![window]),
        }
    }
    groups
}

/// The watts timeline over the window, newest bucket at the right edge.
fn render_chart_block(
    frame: &mut Frame<'_>,
    rows: &mut RowCursor,
    power: &PowerView,
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
                Span::styled("watts", context.theme.title_style()),
                Span::styled(
                    format!(
                        " · now {} · peak {}",
                        power.total_w.map_or_else(
                            || "n/a".to_owned(),
                            |watts| format_value(watts, CanonicalUnit::Watts)
                        ),
                        peak_of(power).map_or_else(
                            || "n/a".to_owned(),
                            |watts| format_value(watts, CanonicalUnit::Watts)
                        ),
                    ),
                    context.theme.muted_style(),
                ),
            ]),
            recorded_note(power).as_deref(),
            context,
        );
    }
    let Some(area) = chart_rows else { return };
    render_chart(frame, area, &power.timeline, None, context);
}

/// What of the timeline window the local database holds, and why the rest
/// is missing: `recorded 55m of 1h · only while fox-services-top runs`.
/// `None` while the window is fully covered.
fn recorded_note(power: &PowerView) -> Option<String> {
    let recorded = power.rows.iter().filter_map(|row| row.covered_ns).max()?;
    if nanos_to_seconds(recorded) >= nanos_to_seconds(power.requested_span_ns) * FULL_COVERAGE {
        return None;
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let held = (recorded / 1_000_000_000).max(1);
    Some(format!(
        "recorded {} of {} · {}",
        recorded_text(held),
        format_span_ns(power.requested_span_ns),
        why_partly(&power.disk),
    ))
}

fn peak_of(power: &PowerView) -> Option<f64> {
    power
        .rows
        .iter()
        .filter_map(|row| row.peak_w.filter(|watts| watts.is_finite()))
        .reduce(f64::max)
}

/// One row per power series with now/mean/peak watts, kWh and its cost at
/// the selected tariff.
fn render_series(
    frame: &mut Frame<'_>,
    rows: &mut RowCursor,
    power: &PowerView,
    filter: &str,
    context: &DisplayContext,
    output: &mut Output,
) {
    let kept: Vec<_> = power
        .rows
        .iter()
        .filter(|row| filter.is_empty() || row.subject.to_lowercase().contains(filter))
        .collect();
    if kept.is_empty() {
        return;
    }
    let Some(table) = rows.take(crate::num::saturate_u16(kept.len() + 1).min(rows.remaining()))
    else {
        return;
    };
    let mut cursor = RowCursor::new(table);
    let Some(header) = cursor.next_row() else {
        return;
    };
    let columns = fit_columns(&SERIES_COLUMNS, header.width);
    draw_table_header(
        frame.buffer_mut(),
        header,
        &SERIES_COLUMNS,
        &columns,
        context.theme.title_style(),
    );
    for row in kept {
        let Some(area) = cursor.next_row() else { break };
        draw_table_row(
            frame.buffer_mut(),
            area,
            &columns,
            &series_cells(row, selected_tariff(context).1.price_eur_per_kwh, context),
            None,
        );
        output.label(format!("power {}", clean(&row.subject)));
    }
}

fn series_cells(
    row: &fox_services_top_core::view::PowerRowView,
    price_eur_per_kwh: f64,
    context: &DisplayContext,
) -> Vec<TableCell> {
    let style = context.theme.text_style();
    let peak = row.peak_w.filter(|watts| watts.is_finite() && *watts > 0.0);
    let watts = |value: Option<f64>| {
        value.filter(|watts| watts.is_finite()).map_or_else(
            || TableCell::new("n/a", context.theme.muted_style()),
            |watts| {
                TableCell::new(
                    format_value(watts, CanonicalUnit::Watts),
                    watts_style(watts, peak, context),
                )
            },
        )
    };
    vec![
        TableCell::new(clean(&row.subject), style),
        watts(row.current_w),
        watts(row.mean_w),
        watts(row.peak_w),
        row.energy_joules
            .filter(|joules| joules.is_finite())
            .map_or_else(
                || TableCell::new("n/a", context.theme.muted_style()),
                |joules| TableCell::new(format_energy(joules), style),
            ),
        row.energy_joules
            .filter(|joules| joules.is_finite())
            .map_or_else(
                || TableCell::new("n/a", context.theme.muted_style()),
                |joules| TableCell::new(format_cost_eur(Some(joules), price_eur_per_kwh), style),
            ),
        covered_cell(row.covered_ns, context),
    ]
}

/// Watts in their level color against the series peak, so a hot series
/// reads red at a glance; missing stays muted.
fn watts_style(watts: f64, peak: Option<f64>, context: &DisplayContext) -> Style {
    match peak {
        Some(scale) if scale > 0.0 => Style::new().fg(context.theme.level_color(watts / scale)),
        _ => context.theme.text_style(),
    }
}

/// Hours the history holds in the window (`1.0h`), or `n/a`.
fn covered_cell(covered_ns: Option<u64>, context: &DisplayContext) -> TableCell {
    match covered_ns {
        Some(covered) => {
            let hours = covered as f64 / 3_600_000_000_000.0;
            if hours.is_finite() {
                TableCell::new(format!("{hours:.1}h"), context.theme.label_style())
            } else {
                TableCell::new("n/a", context.theme.muted_style())
            }
        }
        None => TableCell::new("n/a", context.theme.muted_style()),
    }
}

/// kWh per series over 1 h, 6 h, 12 h and 24 h: the per-query energy.
fn render_windows(
    frame: &mut Frame<'_>,
    rows: &mut RowCursor,
    power: &PowerView,
    filter: &str,
    context: &DisplayContext,
    output: &mut Output,
) {
    let mut groups = window_groups(&power.windows);
    groups.retain(|group| {
        group
            .first()
            .is_some_and(|first| filter.is_empty() || first.subject.to_lowercase().contains(filter))
    });
    if groups.is_empty() || rows.remaining() < 2 {
        return;
    }
    let Some(table) = rows.take(crate::num::saturate_u16(groups.len() + 1).min(rows.remaining()))
    else {
        return;
    };
    let mut cursor = RowCursor::new(table);
    let Some(header) = cursor.next_row() else {
        return;
    };
    let columns = fit_columns(&WINDOW_COLUMNS, header.width);
    draw_table_header(
        frame.buffer_mut(),
        header,
        &WINDOW_COLUMNS,
        &columns,
        context.theme.title_style(),
    );
    for group in groups {
        let Some(area) = cursor.next_row() else { break };
        draw_table_row(
            frame.buffer_mut(),
            area,
            &columns,
            &window_cells(&group, context),
            None,
        );
        if let Some(first) = group.first() {
            output.label(format!("power window {}", clean(&first.subject)));
        }
    }
}

fn window_cells(group: &[&PowerWindowView], context: &DisplayContext) -> Vec<TableCell> {
    let style = context.theme.text_style();
    let subject = group
        .first()
        .map_or_else(|| "–".to_owned(), |first| clean(&first.subject));
    let mut cells = vec![TableCell::new(subject, style)];
    for label in WINDOW_ORDER {
        let energy = group
            .iter()
            .find(|window| window.window_label == label)
            .and_then(|window| window.energy_joules.filter(|joules| joules.is_finite()));
        cells.push(match energy {
            Some(joules) => TableCell::new(format_energy(joules), style),
            None => TableCell::new("n/a", context.theme.muted_style()),
        });
    }
    cells
}

struct CostSelection {
    label: String,
    energy_joules: Option<f64>,
    covered_ns: Option<u64>,
    span_ns: Option<u64>,
}

/// Total consumption for the selected cost range: the current timeline
/// window (`total_joules`) or the database's 1 h/6 h/12 h/24 h total-power
/// window, so the priced energy is what the local database holds.
fn cost_of(power: &PowerView, cost_window: u8) -> CostSelection {
    let label = COST_WINDOWS[usize::from(cost_window) % COST_WINDOWS.len()].to_owned();
    if label == "window" {
        let covered = power
            .rows
            .iter()
            .find(|row| row.subject == "total")
            .and_then(|row| row.covered_ns);
        return CostSelection {
            label,
            energy_joules: power.total_joules,
            covered_ns: covered,
            span_ns: Some(power.requested_span_ns),
        };
    }
    let entry = power.windows.iter().find(|window| {
        window.window_label == label
            && window.kind == fox_services_top_core::view::HistoryChartKind::TotalPower
            && window.subject == "total"
    });
    CostSelection {
        label,
        energy_joules: entry.and_then(|window| window.energy_joules),
        covered_ns: entry.and_then(|window| window.covered_ns),
        span_ns: entry.map(|window| window.span_ns),
    }
}

/// Selected country tariff: the module's scroll cursor (`j`/`k`), clamped
/// to the tariff list. Position 0 is the default (Netherlands).
fn selected_tariff(context: &DisplayContext) -> (usize, crate::format::Tariff) {
    let index = usize::from(context.scroll).min(TARIFFS.len() - 1);
    (index, tariff(index))
}

/// Priced total for the selected country and database window
/// (`j`/`k` country, `t`/`T` range), with the kWh beside its cost.
fn render_cost(
    frame: &mut Frame<'_>,
    rows: &mut RowCursor,
    power: &PowerView,
    context: &DisplayContext,
    output: &mut Output,
) {
    if rows.remaining() < COST_ROWS {
        return;
    }
    let selection = cost_of(power, context.power_cost_window);
    let (_, current) = selected_tariff(context);
    let caption = rows.next_row();
    let Some(area) = rows.next_row() else {
        return;
    };
    if let Some(row) = caption {
        let coverage = match (selection.covered_ns, selection.span_ns) {
            (Some(covered), Some(span)) if span > 0 && covered < span => {
                format!(
                    " · recorded {} of {}",
                    super::history::history_recorded_text(covered / 1_000_000_000),
                    format_span_ns(span)
                )
            }
            _ => String::new(),
        };
        draw_caption(
            frame.buffer_mut(),
            row,
            &Line::from(vec![
                Span::styled("cost", context.theme.title_style()),
                Span::styled(
                    format!(
                        " · {} {} €{:.2}/kWh · {}",
                        current.code, current.country, current.price_eur_per_kwh, selection.label
                    ),
                    context.theme.muted_style(),
                ),
            ]),
            Some(format!("j/k country · t/T range{coverage}").as_str()),
            context,
        );
    }
    let energy = selection
        .energy_joules
        .filter(|joules| joules.is_finite())
        .map_or_else(|| "n/a".to_owned(), format_energy);
    let cost = format_cost_eur(selection.energy_joules, current.price_eur_per_kwh);
    // A missing total with GPU series present means a part is unreadable:
    // say so instead of leaving a bare `n/a`.
    let text = if selection.energy_joules.is_none()
        && power
            .rows
            .iter()
            .any(|row| row.kind == fox_services_top_core::view::HistoryChartKind::GpuPower)
    {
        format!("total {energy} ≈ {cost} · needs every CPU and GPU reading")
    } else {
        format!("total {energy} ≈ {cost}")
    };
    crate::modules::common::message(frame, area, &text, context.theme.text_style());
    output.label(format!(
        "power cost {} {} {energy} {cost}",
        current.code, selection.label
    ));
}

/// Country tariff list (Detail slot only): one row per country with its
/// €/kWh price, `j`/`k` moving the selected row that prices the cost above.
fn render_tariffs(
    frame: &mut Frame<'_>,
    rows: &mut RowCursor,
    context: &DisplayContext,
    output: &mut Output,
) {
    if context.slot != Slot::Detail || rows.remaining() < 2 {
        return;
    }
    let (selected, _) = selected_tariff(context);
    let Some(caption) = rows.next_row() else {
        return;
    };
    draw_caption(
        frame.buffer_mut(),
        caption,
        &Line::from(vec![
            Span::styled("tariffs", context.theme.title_style()),
            Span::styled(" · €/kWh · j/k select", context.theme.muted_style()),
        ]),
        None,
        context,
    );
    let visible = usize::from(rows.remaining());
    let start = selected.saturating_sub(visible.saturating_sub(1));
    for (index, current) in TARIFFS.iter().enumerate().skip(start) {
        let Some(area) = rows.next_row() else {
            break;
        };
        let line = Line::from(vec![
            Span::styled(
                if index == selected { "▶ " } else { "  " },
                context.theme.text_style(),
            ),
            Span::styled(
                format!(
                    "{} {} €{:.2}/kWh",
                    current.code, current.country, current.price_eur_per_kwh
                ),
                if index == selected {
                    context.theme.selected_row_style()
                } else {
                    context.theme.text_style()
                },
            ),
        ]);
        super::common::draw_line(frame.buffer_mut(), area, &line);
    }
    output.label("power tariffs");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_fixtures;
    use fox_services_top_core::commands::ViewKind;

    /// The tab shows the watts timeline, the per-series table and the kWh
    /// per query windows.
    #[test]
    fn the_power_tab_charts_watts_and_lists_kwh() {
        let mut model = test_fixtures::view_model(ViewKind::Power, 1);
        model.modules.power = Some(std::sync::Arc::new(test_fixtures::power_view()));
        let text = test_fixtures::render_text(&PowerModule::new().unwrap(), &model, (120, 24));
        for needle in [
            "Power · 1h",
            "watts",
            "SERIES",
            "NOW",
            "KWH",
            "COVER",
            "total",
            "CPU 0",
            "CPU platform",
            "GPU 0",
            "1H",
            "24H",
            "cost",
            "COST",
            "€0.06",
            "NL",
            "€0.30/kWh",
        ] {
            assert!(text.contains(needle), "missing {needle:?}\n{text}");
        }
    }

    /// Without power series the tab says so instead of charting zeros.
    #[test]
    fn without_power_series_the_tab_reports_no_data() {
        let mut model = test_fixtures::view_model(ViewKind::Power, 1);
        model.modules.power = None;
        let text = test_fixtures::render_text(&PowerModule::new().unwrap(), &model, (120, 20));
        assert!(text.contains("no power data"), "{text}");
    }

    /// A partly recorded window names what the local database holds and
    /// why the rest is missing; a full window shows no note.
    #[test]
    fn a_partly_recorded_window_names_its_database_coverage() {
        use fox_services_top_core::view::DiskHistory;

        let mut full = test_fixtures::power_view();
        assert_eq!(recorded_note(&full), None);
        for row in &mut full.rows {
            row.covered_ns = Some(1_800_000_000_000);
        }
        assert_eq!(
            recorded_note(&full).as_deref(),
            Some("recorded 30m of 1h · only while fox-services-top runs")
        );
        full.disk = DiskHistory::Failed("locked".into());
        assert_eq!(
            recorded_note(&full).as_deref(),
            Some("recorded 30m of 1h · disk history unavailable")
        );

        let mut model = test_fixtures::view_model(ViewKind::Power, 1);
        model.modules.power = Some(std::sync::Arc::new(full));
        let text = test_fixtures::render_text(&PowerModule::new().unwrap(), &model, (160, 28));
        assert!(text.contains("recorded 30m of 1h"), "{text}");
        assert!(text.contains("disk history unavailable"), "{text}");
    }

    /// The cost estimate prices the current window by default and the
    /// database's 24 h total on request.
    #[test]
    fn the_cost_estimate_prices_the_selected_database_window() {
        let power = test_fixtures::power_view();
        let window = cost_of(&power, 0);
        assert_eq!(window.label, "window");
        assert_eq!(window.energy_joules, power.total_joules);
        let day = cost_of(&power, 4);
        assert_eq!(day.label, "24h");
        // The fixture's 24 h total holds 200 W over 24 h: 4.8 kWh.
        assert_eq!(day.energy_joules, Some(200.0 * 24.0 * 3_600.0));
        assert_eq!(
            crate::format::format_cost_eur(day.energy_joules, 0.30),
            "€1.44",
            "4.8 kWh at €0.30/kWh"
        );
    }

    /// Selecting another country reprices the same logged consumption.
    #[test]
    fn the_cost_estimate_follows_the_selected_country() {
        let power = test_fixtures::power_view();
        let window = cost_of(&power, 0);
        let by_code = |code: &str| {
            crate::format::TARIFFS
                .iter()
                .find(|tariff| tariff.code == code)
                .copied()
                .unwrap_or_else(|| panic!("{code} tariff"))
        };
        let nl = by_code("NL");
        let de = by_code("DE");
        assert!(de.price_eur_per_kwh > nl.price_eur_per_kwh);
        let nl_cost = crate::format::cost_eur(window.energy_joules, nl.price_eur_per_kwh).unwrap();
        let de_cost = crate::format::cost_eur(window.energy_joules, de.price_eur_per_kwh).unwrap();
        assert!(de_cost > nl_cost);
        // The fixture's window total holds 300 W over 1 h: 0.3 kWh.
        assert!((nl_cost - 0.3 * 0.30).abs() < 1e-9, "{nl_cost}");
    }

    /// The tariff list names every country with its price and marks the
    /// scroll cursor's row; the cursor prices the cost above.
    #[test]
    fn the_tariff_list_marks_the_scrolled_country() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let render_at = |scroll: u16| -> String {
            let model = {
                let mut model = test_fixtures::view_model(ViewKind::Power, 1);
                model.modules.power = Some(std::sync::Arc::new(test_fixtures::power_view()));
                model
            };
            let mut context = test_fixtures::display_context();
            context.slot = crate::registry::Slot::Detail;
            context.scroll = scroll;
            let mut terminal = Terminal::new(TestBackend::new(120, 60)).unwrap();
            terminal
                .draw(|frame| {
                    PowerModule::new()
                        .unwrap()
                        .render(&context, &model, frame.area(), frame);
                })
                .unwrap();
            let buffer = terminal.backend().buffer().clone();
            (0..60)
                .map(|y| {
                    (0..120)
                        .map(|x| buffer[(x, y)].symbol())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n")
        };

        let top = render_at(0);
        assert!(top.contains("tariffs"), "{top}");
        assert!(top.contains("NL Netherlands €0.30/kWh"), "{top}");
        assert!(top.contains("▶ NL Netherlands"), "{top}");
        let moved = render_at(4);
        assert!(moved.contains("▶ DE Germany"), "{moved}");
        assert!(!moved.contains("▶ NL Netherlands"), "{moved}");
        // The cursor is clamped to the list, never past it.
        let clamped = render_at(u16::MAX);
        assert!(clamped.contains("▶ AE UAE"), "{clamped}");
    }
}
