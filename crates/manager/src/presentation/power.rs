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
use fox_services_top_core::view::{
    HistoryChartKind, HistoryView, PowerRowView, PowerView, PowerWindowView,
};

use super::reader::MetricReader;

/// Builds the Power tab's view from the history service's view and the
/// live snapshot. `None` while the history holds no power series: an empty
/// view would carry no topic, so the tab and the Overview sub-tab say
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
    let rows: Vec<PowerRowView> = power_charts
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
    let windows: Vec<PowerWindowView> = history
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
}
