//! Power view builder: watts timeline and kWh per query window from the
//! history's power charts and summaries plus the live snapshot.
//!
//! Pure function over immutable inputs: the history's `TotalPower`,
//! `CpuPower` and `GpuPower` charts give the timeline window (mean watts
//! per bucket, gaps as `None`), and the summaries give the per-window
//! energy (`integral` joules) over 1 h to 24 h. The live [`MetricReader`]
//! contributes the current watts of each series. No I/O, no blocking.

use fox_services_top_core::metric::ids;
use fox_services_top_core::naming::entity;
use fox_services_top_core::observation::EntityKind;
use fox_services_top_core::view::{
    HistoryChartKind, HistoryView, PowerRowView, PowerView, PowerWindowView,
};

use super::reader::MetricReader;

/// Builds the dashboard's power widget from the history service's view and the
/// live snapshot. `None` while the history holds no power series: an empty
/// view would carry no topic, so the widget and the Overview sub-tab say
/// "no power data" instead.
pub(crate) fn build_power_view(
    reader: &MetricReader<'_>,
    history: &HistoryView,
) -> Option<PowerView> {
    let power_charts: Vec<_> = history
        .charts
        .iter()
        .filter(|chart| chart.kind.is_power())
        .collect();
    let total_chart = power_charts
        .iter()
        .find(|chart| chart.kind == HistoryChartKind::TotalPower)
        .copied();
    let timeline: Vec<Option<f64>> = total_chart
        .or(power_charts.first().copied())
        .map(|chart| chart.points.iter().map(|point| point.mean).collect())
        .unwrap_or_default();
    let total_w = total_chart
        .map(|chart| {
            reader
                .cell(
                    chart.series.metric_id.as_str(),
                    chart.series.entity_id.as_str(),
                    chart.series.scope.as_str(),
                )
                .value_f64()
        })
        .unwrap_or_else(|| {
            reader
                .cell(ids::HOST_POWER_TOTAL, entity::HOST, entity::SCOPE_HOST)
                .value_f64()
        });
    let total_joules = total_chart.and_then(|chart| energy_of(chart.mean, chart.covered_ns));
    let mut rows: Vec<PowerRowView> = power_charts
        .iter()
        .map(|chart| {
            let current_w = reader
                .cell(
                    chart.series.metric_id.as_str(),
                    chart.series.entity_id.as_str(),
                    chart.series.scope.as_str(),
                )
                .value_f64();
            PowerRowView {
                kind: chart.kind,
                subject: subject_of(chart.kind, &chart.subject),
                current_w,
                mean_w: chart.mean,
                peak_w: chart.peak,
                covered_ns: chart.covered_ns,
                energy_joules: energy_of(chart.mean, chart.covered_ns),
            }
        })
        .collect();
    let mut windows: Vec<PowerWindowView> = history
        .summaries
        .iter()
        .filter(|summary| summary.kind.is_power())
        .map(|summary| PowerWindowView {
            kind: summary.kind,
            subject: subject_of(summary.kind, &summary.subject),
            window_label: summary.window_label.clone(),
            span_ns: summary.span_ns,
            mean_w: summary.mean,
            peak_w: summary.peak,
            covered_ns: summary.covered_ns,
            energy_joules: summary.integral,
        })
        .collect();
    let (total_w, total_joules) = synthesize_total(
        reader,
        total_chart.is_some(),
        total_w,
        total_joules,
        &mut rows,
        &mut windows,
    );
    if timeline.iter().all(|point| point.is_none()) && rows.is_empty() && windows.is_empty() {
        return None;
    }
    Some(PowerView {
        window_label: history.window_label.clone(),
        requested_span_ns: history.requested_span_ns,
        total_w,
        total_joules,
        timeline,
        rows,
        windows,
        disk: history.disk.clone(),
    })
}

/// Joules of `mean` watts over `covered_ns`; `None` while uncovered.
fn energy_of(mean: Option<f64>, covered_ns: Option<u64>) -> Option<f64> {
    let (mean, covered) = (mean?, covered_ns?);
    if !mean.is_finite() || mean < 0.0 || covered == 0 {
        return None;
    }
    Some(mean * (covered as f64) / 1_000_000_000.0)
}

/// Whether the catalog lists a GPU that has not been removed: the
/// collector only publishes `host.power.total` for such hosts, so without
/// one a missing total means "no total series", not "a part unreadable".
fn has_live_gpus(reader: &MetricReader<'_>) -> bool {
    reader
        .catalog()
        .entities
        .values()
        .any(|record| record.kind == EntityKind::Gpu && record.removed_boot_ns.is_none())
}

/// Finite values only; `None` when any part is missing or unusable, so a
/// synthesized total is never a partial sum.
fn sum_all(values: &[Option<f64>]) -> Option<f64> {
    let mut sum = 0.0;
    for value in values {
        match value {
            Some(value) if value.is_finite() && *value >= 0.0 => sum += *value,
            _ => return None,
        }
    }
    Some(sum)
}

/// Total when the collector did not publish `host.power.total`, without
/// ever falling back to a partial sum:
///
/// * No live GPUs in the catalog: the collector never publishes the total
///   there (it would only repeat the CPU), so the title, the `total` row
///   and the per-window totals are synthesized from the CPU packages. The
///   CPU sum is the whole total here, exact rather than partial.
/// * Otherwise the series exists (or nothing reports at all). When every
///   reporting part currently reads, the live total is their sum — the same
///   instant sum the collector publishes, only missed through cycle timing.
///   When any part lacks a reading, the `n/a` stays: a silent partial sum
///   would lie.
#[allow(clippy::too_many_arguments)]
fn synthesize_total(
    reader: &MetricReader<'_>,
    has_total_chart: bool,
    total_w: Option<f64>,
    total_joules: Option<f64>,
    rows: &mut Vec<PowerRowView>,
    windows: &mut Vec<PowerWindowView>,
) -> (Option<f64>, Option<f64>) {
    if !has_total_chart && !has_live_gpus(reader) {
        return cpu_only_total(total_w, total_joules, rows, windows);
    }
    if total_w.is_some() || !has_total_chart {
        return (total_w, total_joules);
    }
    // The series exists but the live cell does not read: sum the live
    // readings of the parts, and only those.
    let parts: Vec<&PowerRowView> = rows
        .iter()
        .filter(|row| row.kind != HistoryChartKind::TotalPower)
        .collect();
    if parts.is_empty() {
        return (total_w, total_joules);
    }
    let Some(current) = sum_all(&parts.iter().map(|row| row.current_w).collect::<Vec<_>>()) else {
        return (total_w, total_joules);
    };
    if let Some(total) = rows
        .iter_mut()
        .find(|row| row.kind == HistoryChartKind::TotalPower)
    {
        total.current_w = Some(current);
    }
    (Some(current), total_joules)
}

/// Total from the CPU packages when the host has no GPUs (see
/// [`synthesize_total`]).
fn cpu_only_total(
    total_w: Option<f64>,
    total_joules: Option<f64>,
    rows: &mut Vec<PowerRowView>,
    windows: &mut Vec<PowerWindowView>,
) -> (Option<f64>, Option<f64>) {
    let cpu: Vec<&PowerRowView> = rows
        .iter()
        .filter(|row| row.kind == HistoryChartKind::CpuPower)
        .collect();
    if cpu.is_empty() {
        return (total_w, total_joules);
    }
    let current_w = sum_all(&cpu.iter().map(|row| row.current_w).collect::<Vec<_>>());
    let energy_joules = sum_all(&cpu.iter().map(|row| row.energy_joules).collect::<Vec<_>>());
    let covered_ns = cpu.iter().filter_map(|row| row.covered_ns).max();
    let mean_w = match (energy_joules, covered_ns) {
        (Some(joules), Some(covered)) if covered > 0 => {
            Some(joules * 1_000_000_000.0 / (covered as f64))
        }
        _ => None,
    };
    if current_w.is_none() && energy_joules.is_none() {
        return (total_w, total_joules);
    }
    let peak_w = cpu
        .iter()
        .filter_map(|row| row.peak_w.filter(|watts| watts.is_finite()))
        .reduce(f64::max);
    rows.insert(
        0,
        PowerRowView {
            kind: HistoryChartKind::TotalPower,
            subject: "total".to_owned(),
            current_w,
            mean_w,
            peak_w,
            covered_ns,
            energy_joules,
        },
    );
    synthesize_total_windows(windows);
    (current_w, energy_joules)
}

/// Per-window `total` entries summed from the CPU packages' summaries, in
/// first-seen window order, skipping windows that already carry a total.
fn synthesize_total_windows(windows: &mut Vec<PowerWindowView>) {
    let mut labels: Vec<String> = Vec::new();
    for window in windows.iter() {
        if !labels.contains(&window.window_label) {
            labels.push(window.window_label.clone());
        }
    }
    let mut synthesized = Vec::new();
    for label in labels {
        if windows.iter().any(|window| {
            window.window_label == label
                && window.kind == HistoryChartKind::TotalPower
                && window.subject == "total"
        }) {
            continue;
        }
        let cpu: Vec<&PowerWindowView> = windows
            .iter()
            .filter(|window| {
                window.window_label == label && window.kind == HistoryChartKind::CpuPower
            })
            .collect();
        if cpu.is_empty() {
            continue;
        }
        let energy_joules = sum_all(
            &cpu.iter()
                .map(|window| window.energy_joules)
                .collect::<Vec<_>>(),
        );
        let Some(energy) = energy_joules else {
            continue;
        };
        let covered_ns = cpu.iter().filter_map(|window| window.covered_ns).max();
        let span_ns = cpu.iter().map(|window| window.span_ns).max().unwrap_or(0);
        let mean_w = match covered_ns {
            Some(covered) if covered > 0 => Some(energy * 1_000_000_000.0 / (covered as f64)),
            _ => None,
        };
        let peak_w = cpu
            .iter()
            .filter_map(|window| window.peak_w.filter(|watts| watts.is_finite()))
            .reduce(f64::max);
        synthesized.push(PowerWindowView {
            kind: HistoryChartKind::TotalPower,
            subject: "total".to_owned(),
            window_label: label,
            span_ns,
            mean_w,
            peak_w,
            covered_ns,
            energy_joules: Some(energy),
        });
    }
    windows.splice(..0, synthesized);
}

/// Display subject: the host total reads `total`, the rest as charted.
fn subject_of(kind: HistoryChartKind, subject: &str) -> String {
    if kind == HistoryChartKind::TotalPower {
        return "total".to_owned();
    }
    if subject.is_empty() {
        return kind_label(kind).to_owned();
    }
    subject.to_owned()
}

fn kind_label(kind: HistoryChartKind) -> &'static str {
    match kind {
        HistoryChartKind::TotalPower => "total",
        HistoryChartKind::CpuPower => "CPU",
        HistoryChartKind::GpuPower => "GPU",
        _ => "power",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::presentation::testkit::{Fixture, NOW, sample};
    use fox_services_top_core::view::HistoryChartView;

    fn history_with_power() -> (Fixture, HistoryView) {
        let mut fixture = Fixture::new();
        fixture.add(
            "cpu",
            sample(ids::HOST_POWER_TOTAL, "host", "host").f64(200.0),
        );
        fixture.add(
            "cpu",
            sample(ids::CPU_POWER_PACKAGE, "cpu.package.0", "package:0").f64(50.0),
        );
        let history = HistoryView {
            window_label: "1h".to_owned(),
            requested_span_ns: 3_600_000_000_000,
            start_utc_ns: 0,
            end_utc_ns: 3_600_000_000_000,
            utc_offset_s: 0,
            charts: vec![
                chart(HistoryChartKind::TotalPower, "", 200.0),
                chart(HistoryChartKind::CpuPower, "CPU 0", 50.0),
            ],
            summaries: vec![
                summary(),
                window_summary(HistoryChartKind::CpuPower, "CPU 0", "1h", 50.0, 180_000.0),
            ],
            disk: fox_services_top_core::view::DiskHistory::Off,
            error: None,
        };
        (fixture, history)
    }

    fn chart(
        kind: HistoryChartKind,
        subject: &str,
        mean: f64,
    ) -> fox_services_top_core::view::HistoryChartView {
        use fox_services_top_core::metric::CanonicalUnit;
        let (metric, entity, scope) = match (kind, subject) {
            (HistoryChartKind::TotalPower, _) => (ids::HOST_POWER_TOTAL, "host", "host"),
            (_, "CPU platform") => (ids::CPU_POWER_PLATFORM, "host", "platform"),
            _ => (ids::CPU_POWER_PACKAGE, "cpu.package.0", "package:0"),
        };
        HistoryChartView {
            kind,
            subject: subject.to_owned(),
            series: fox_services_top_core::testing::metric_key(entity, metric, scope),
            unit: CanonicalUnit::Watts,
            scale_max: None,
            points: vec![
                point(100.0),
                point(200.0),
                fox_services_top_core::view::HistoryPointView {
                    start_utc_ns: 2,
                    end_utc_ns: 3,
                    mean: None,
                    min: None,
                    max: None,
                },
            ],
            mean: Some(mean),
            peak: Some(mean * 1.2),
            covered_ns: Some(3_600_000_000_000),
            display_downsampled: false,
            pending: false,
            error: None,
            parts: Vec::new(),
        }
    }

    fn point(mean: f64) -> fox_services_top_core::view::HistoryPointView {
        fox_services_top_core::view::HistoryPointView {
            start_utc_ns: 0,
            end_utc_ns: 1,
            mean: Some(mean),
            min: None,
            max: Some(mean),
        }
    }

    fn summary() -> fox_services_top_core::view::HistoryWindowSummary {
        window_summary(HistoryChartKind::TotalPower, "", "1h", 200.0, 720_000.0)
    }

    fn window_summary(
        kind: HistoryChartKind,
        subject: &str,
        label: &str,
        mean: f64,
        integral: f64,
    ) -> fox_services_top_core::view::HistoryWindowSummary {
        use fox_services_top_core::metric::CanonicalUnit;
        fox_services_top_core::view::HistoryWindowSummary {
            kind,
            subject: subject.to_owned(),
            window_label: label.to_owned(),
            span_ns: 3_600_000_000_000,
            unit: CanonicalUnit::Watts,
            mean: Some(mean),
            peak: Some(mean * 1.2),
            covered_ns: Some(3_600_000_000_000),
            integral: Some(integral),
            pending: false,
        }
    }

    #[test]
    fn timeline_rows_and_windows_come_from_the_power_charts() {
        let (fixture, history) = history_with_power();
        let reader = MetricReader::new(&fixture.snapshot, &fixture.catalog, NOW);
        let view = build_power_view(&reader, &history).expect("power charts fill the view");
        assert_eq!(view.window_label, "1h");
        assert_eq!(view.timeline, vec![Some(100.0), Some(200.0), None]);
        assert_eq!(view.total_w, Some(200.0));
        assert_eq!(view.total_joules, Some(720_000.0));
        assert_eq!(view.rows.len(), 2);
        assert_eq!(view.rows[0].subject, "total");
        assert_eq!(view.rows[0].energy_joules, Some(720_000.0));
        assert_eq!(view.windows.len(), 2);
        assert_eq!(view.windows[0].energy_joules, Some(720_000.0));
        assert_eq!(view.windows[1].subject, "CPU 0");
        assert_eq!(view.windows[1].energy_joules, Some(180_000.0));
        assert_eq!(
            view.disk,
            fox_services_top_core::view::DiskHistory::Off,
            "the view carries where its timeline is kept"
        );
    }

    #[test]
    fn the_cpu_platform_power_is_listed_with_the_packages() {
        let (mut fixture, mut history) = history_with_power();
        fixture.add(
            "cpu",
            sample(ids::CPU_POWER_PLATFORM, "host", "platform").f64(60.0),
        );
        history
            .charts
            .push(chart(HistoryChartKind::CpuPower, "CPU platform", 60.0));
        let reader = MetricReader::new(&fixture.snapshot, &fixture.catalog, NOW);
        let view = build_power_view(&reader, &history).expect("power charts fill the view");
        let platform = view
            .rows
            .iter()
            .find(|row| row.subject == "CPU platform")
            .expect("platform row");
        assert_eq!(platform.current_w, Some(60.0));
        assert_eq!(platform.energy_joules, Some(60.0 * 3_600.0));
    }

    #[test]
    fn the_view_marks_timelines_kept_in_the_local_database() {
        let (fixture, mut history) = history_with_power();
        history.disk = fox_services_top_core::view::DiskHistory::On;
        let reader = MetricReader::new(&fixture.snapshot, &fixture.catalog, NOW);
        let view = build_power_view(&reader, &history).expect("power charts fill the view");
        assert_eq!(view.disk, fox_services_top_core::view::DiskHistory::On);
    }

    #[test]
    fn empty_history_fills_no_view() {
        let fixture = Fixture::new();
        let reader = MetricReader::new(&fixture.snapshot, &fixture.catalog, NOW);
        let history = HistoryView {
            window_label: "1m".to_owned(),
            requested_span_ns: 60_000_000_000,
            start_utc_ns: 0,
            end_utc_ns: 60_000_000_000,
            utc_offset_s: 0,
            charts: Vec::new(),
            summaries: Vec::new(),
            disk: fox_services_top_core::view::DiskHistory::Off,
            error: None,
        };
        assert!(build_power_view(&reader, &history).is_none());
    }

    /// A CPU-only history on a host without GPUs: the collector never
    /// publishes `host.power.total` there, so the view synthesizes the
    /// exact total from the CPU packages instead of showing `n/a`.
    fn cpu_only_history() -> (Fixture, HistoryView) {
        let mut fixture = Fixture::new();
        fixture.add(
            "cpu",
            sample(ids::CPU_POWER_PACKAGE, "cpu.package.0", "package:0").f64(50.0),
        );
        let history = HistoryView {
            window_label: "1h".to_owned(),
            requested_span_ns: 3_600_000_000_000,
            start_utc_ns: 0,
            end_utc_ns: 3_600_000_000_000,
            utc_offset_s: 0,
            charts: vec![chart(HistoryChartKind::CpuPower, "CPU 0", 50.0)],
            summaries: vec![window_summary(
                HistoryChartKind::CpuPower,
                "CPU 0",
                "1h",
                50.0,
                180_000.0,
            )],
            disk: fox_services_top_core::view::DiskHistory::Off,
            error: None,
        };
        (fixture, history)
    }

    #[test]
    fn a_host_without_gpus_totals_its_cpu_packages() {
        let (fixture, history) = cpu_only_history();
        assert!(
            fixture.catalog.entities.values().all(|record| {
                record.kind != fox_services_top_core::observation::EntityKind::Gpu
            }),
            "the fixture host has no GPUs"
        );
        let reader = MetricReader::new(&fixture.snapshot, &fixture.catalog, NOW);
        let view = build_power_view(&reader, &history).expect("CPU watts fill the view");
        assert_eq!(view.total_w, Some(50.0));
        assert_eq!(view.total_joules, Some(180_000.0));
        assert_eq!(view.rows.len(), 2);
        assert_eq!(view.rows[0].subject, "total");
        assert_eq!(view.rows[0].energy_joules, Some(180_000.0));
        let total_window = view
            .windows
            .iter()
            .find(|window| window.kind == HistoryChartKind::TotalPower && window.subject == "total")
            .expect("a per-window total from the CPU summaries");
        assert_eq!(total_window.window_label, "1h");
        assert_eq!(total_window.energy_joules, Some(180_000.0));
    }

    #[test]
    fn a_host_with_a_gpu_keeps_a_missing_total_as_na() {
        let (mut fixture, history) = cpu_only_history();
        fixture.add_entity(fox_services_top_core::testing::entity_record(
            "gpu:GPU-0",
            fox_services_top_core::observation::EntityKind::Gpu,
            "GPU 0",
            &[],
        ));
        let reader = MetricReader::new(&fixture.snapshot, &fixture.catalog, NOW);
        let view = build_power_view(&reader, &history).expect("CPU watts fill the view");
        assert_eq!(view.total_w, None, "a partial sum would lie");
        assert_eq!(view.total_joules, None);
        assert!(
            view.rows.iter().all(|row| row.subject != "total"),
            "no synthesized total row"
        );
        assert!(
            view.windows
                .iter()
                .all(|window| window.kind != HistoryChartKind::TotalPower),
            "no synthesized total windows"
        );
    }

    #[test]
    fn a_live_total_missed_through_timing_is_summed_from_its_parts() {
        // Total series in history, a GPU in the catalog, but no live
        // `host.power.total` cell: every reporting part reads, so the
        // title total is their exact sum rather than `n/a`.
        let mut fixture = Fixture::new();
        fixture.add(
            "cpu",
            sample(ids::CPU_POWER_PACKAGE, "cpu.package.0", "package:0").f64(50.0),
        );
        fixture.add(
            "gpu",
            sample(ids::GPU_POWER_DRAW, "gpu:GPU-0", "gpu").f64(220.0),
        );
        fixture.add_entity(fox_services_top_core::testing::entity_record(
            "gpu:GPU-0",
            fox_services_top_core::observation::EntityKind::Gpu,
            "GPU 0",
            &[],
        ));
        let history = HistoryView {
            window_label: "1h".to_owned(),
            requested_span_ns: 3_600_000_000_000,
            start_utc_ns: 0,
            end_utc_ns: 3_600_000_000_000,
            utc_offset_s: 0,
            charts: vec![
                chart(HistoryChartKind::TotalPower, "", 200.0),
                chart(HistoryChartKind::CpuPower, "CPU 0", 50.0),
                HistoryChartView {
                    kind: HistoryChartKind::GpuPower,
                    subject: "GPU 0".to_owned(),
                    series: fox_services_top_core::testing::metric_key(
                        "gpu:GPU-0",
                        ids::GPU_POWER_DRAW,
                        "gpu",
                    ),
                    unit: fox_services_top_core::metric::CanonicalUnit::Watts,
                    scale_max: None,
                    points: vec![point(220.0)],
                    mean: Some(200.0),
                    peak: Some(240.0),
                    covered_ns: Some(3_600_000_000_000),
                    display_downsampled: false,
                    pending: false,
                    error: None,
                    parts: Vec::new(),
                },
            ],
            summaries: vec![
                window_summary(HistoryChartKind::TotalPower, "", "1h", 200.0, 720_000.0),
                window_summary(HistoryChartKind::CpuPower, "CPU 0", "1h", 50.0, 180_000.0),
            ],
            disk: fox_services_top_core::view::DiskHistory::Off,
            error: None,
        };
        let reader = MetricReader::new(&fixture.snapshot, &fixture.catalog, NOW);
        let view = build_power_view(&reader, &history).expect("power charts fill the view");
        assert_eq!(view.total_w, Some(270.0));
        assert_eq!(view.total_joules, Some(720_000.0));
        let total = view
            .rows
            .iter()
            .find(|row| row.subject == "total")
            .expect("total row");
        assert_eq!(total.current_w, Some(270.0));
    }
}
