//! Zombie process table, grouped by parent process ID.

use std::collections::BTreeMap;

use fox_services_top_core::view::{ViewModel, ZombieGroupView, ZombieProcessView, ZombiesView};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};

use super::ModuleBuildError;
use super::common::{
    ManifestSpec, Output, RowCursor, build_manifest, draw_line, message, no_data, panel_with_keys,
};
use crate::format::clean;
use crate::registry::{DisplayContext, DisplayModule, DisplayModuleManifest, RenderOutput, Slot};

const ZOMBIES_KEYS: &str = "j/k scroll · / filter";

pub struct ZombiesModule {
    manifest: DisplayModuleManifest,
}

impl ZombiesModule {
    pub fn new() -> Result<Self, ModuleBuildError> {
        Ok(Self {
            manifest: build_manifest(ManifestSpec {
                id: "zombies",
                title: "Zombie Processes",
                min_columns: 48,
                min_rows: 5,
                priority: 80,
                slots: &[Slot::Detail],
            })?,
        })
    }
}

impl DisplayModule for ZombiesModule {
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
        let Some(zombies) = model.modules.zombies.as_deref() else {
            let inner = panel_with_keys(frame, area, "Zombie Processes", ZOMBIES_KEYS, context);
            message(
                frame,
                inner,
                &no_data("zombie process"),
                context.theme.muted_style(),
            );
            output.label("zombies: no data");
            return output.finish();
        };

        let parent_count = zombies.groups.len();
        let inner = panel_with_keys(
            frame,
            area,
            &format!(
                "Zombie Processes · {} in {} parent groups",
                zombies.total_processes, parent_count
            ),
            ZOMBIES_KEYS,
            context,
        );
        let mut rows = RowCursor::new(inner);
        let filter = model.session.filter.to_lowercase();
        let lines = zombie_lines(zombies, &filter, context);
        if lines.is_empty() {
            if let Some(area) = rows.rest() {
                message(
                    frame,
                    area,
                    if zombies.total_processes == 0 {
                        "no zombie processes found in the latest scan"
                    } else {
                        "no zombie processes match the filter"
                    },
                    context.theme.muted_style(),
                );
            }
        } else {
            let start = usize::from(context.scroll).min(lines.len().saturating_sub(1));
            for line in lines.iter().skip(start).take(usize::from(rows.remaining())) {
                let Some(area) = rows.next_row() else {
                    break;
                };
                draw_line(frame.buffer_mut(), area, line);
            }
        }
        if let Some(area) = rows.next_row() {
            message(
                frame,
                area,
                if zombies.truncated {
                    "list shortened to fit the projection budget"
                } else {
                    "process scan covers up to 4,096 PIDs per poll"
                },
                context.theme.muted_style(),
            );
        }
        output.label(format!(
            "zombies: {} processes in {parent_count} parent groups",
            zombies.total_processes
        ));
        output.finish()
    }
}

fn zombie_lines(
    zombies: &ZombiesView,
    filter: &str,
    context: &DisplayContext,
) -> Vec<Line<'static>> {
    let matching_groups: Vec<(&ZombieGroupView, Vec<&ZombieProcessView>)> = zombies
        .groups
        .iter()
        .filter_map(|group| {
            let parent_matches = parent_matches(group, filter);
            let processes: Vec<&ZombieProcessView> = group
                .processes
                .iter()
                .filter(|process| parent_matches || process_matches(process, filter))
                .collect();
            (!processes.is_empty()).then_some((group, processes))
        })
        .collect();
    if matching_groups.is_empty() {
        return Vec::new();
    }

    let total = matching_groups
        .iter()
        .map(|(_, processes)| processes.len())
        .sum();
    let mut sources = BTreeMap::<String, usize>::new();
    let mut reaping_contexts = BTreeMap::<String, usize>::new();
    for (group, processes) in &matching_groups {
        let source = group
            .parent_name
            .as_deref()
            .map(clean)
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| "(unknown parent)".to_owned());
        *sources.entry(source).or_default() += processes.len();

        let context = match (group.parent_pid, group.parent_name.is_some()) {
            (None, _) => "PPID unavailable",
            (Some(_), true) => "parent visible in scan",
            (Some(_), false) => "parent not in scan",
        };
        *reaping_contexts.entry(context.to_owned()).or_default() += processes.len();
    }

    let mut lines = Vec::new();
    lines.push(Line::from(Span::styled(
        "Parent source (displayed entries)",
        context.theme.title_style(),
    )));
    append_distribution(&mut lines, sources, total);
    lines.push(Line::from(Span::styled(
        "Reaping context (inferred; displayed entries)",
        context.theme.title_style(),
    )));
    append_distribution(&mut lines, reaping_contexts, total);
    lines.push(Line::from(
        "Cause: child exited; exact reason the parent has not reaped it is unknown.",
    ));

    for (group, processes) in matching_groups {
        let parent_id = group
            .parent_pid
            .map_or_else(|| "unknown".to_owned(), |pid| pid.to_string());
        let parent_name = group
            .parent_name
            .as_deref()
            .map(clean)
            .filter(|name| !name.is_empty())
            .map_or_else(String::new, |name| format!(" · {name}"));
        let parent_state = group
            .parent_state
            .map(|state| format!(" · state {} ({state})", process_state_name(state)))
            .unwrap_or_default();
        lines.push(Line::from(Span::styled(
            format!(
                "PPID {parent_id}{parent_name}{parent_state} · {} zombie(s)",
                processes.len(),
            ),
            context.theme.title_style(),
        )));
        if group
            .parent_name
            .as_deref()
            .is_some_and(is_fox_services_top)
        {
            lines.push(Line::from("  Fox-Services Top is the direct parent"));
        }
        for process in processes {
            let command = clean(&process.command);
            let description = if command.is_empty() {
                clean(&process.name)
            } else {
                format!("{} · {command}", clean(&process.name))
            };
            lines.push(Line::from(format!(
                "  PID {:>6}  {description}",
                process.pid
            )));
        }
    }
    lines
}

fn process_state_name(state: char) -> &'static str {
    match state {
        'R' => "running",
        'S' => "sleeping",
        'D' => "uninterruptible sleep",
        'T' | 't' => "stopped",
        'Z' => "zombie",
        'I' => "idle",
        'X' | 'x' => "dead",
        _ => "unknown",
    }
}

fn is_fox_services_top(name: &str) -> bool {
    let executable = name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(name)
        .to_ascii_lowercase();
    matches!(
        executable.strip_suffix(".exe").unwrap_or(&executable),
        "fox-services-top" | "fox_services_top"
    )
}

fn append_distribution(
    lines: &mut Vec<Line<'static>>,
    distribution: BTreeMap<String, usize>,
    total: usize,
) {
    let mut categories: Vec<(String, usize)> = distribution.into_iter().collect();
    categories.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));

    let omitted = categories.len().saturating_sub(3);
    let displayed: Vec<(String, usize)> = categories.into_iter().take(3).collect();
    let displayed_count: usize = displayed.iter().map(|(_, count)| count).sum();
    for (label, count) in displayed {
        lines.push(distribution_line(&label, count, total));
    }
    if omitted > 0 {
        let other_count = total.saturating_sub(displayed_count);
        lines.push(distribution_line("other categories", other_count, total));
    }
}

fn distribution_line(label: &str, count: usize, total: usize) -> Line<'static> {
    const BAR_WIDTH: usize = 8;
    let largest = total.max(1);
    let filled = count
        .saturating_mul(BAR_WIDTH)
        .div_ceil(largest)
        .min(BAR_WIDTH);
    let percentage = count.saturating_mul(100) / largest;
    Line::from(format!(
        "{label} {} {count} ({percentage}%)",
        "█".repeat(filled)
    ))
}

fn parent_matches(group: &ZombieGroupView, filter: &str) -> bool {
    filter.is_empty()
        || group
            .parent_pid
            .is_some_and(|pid| pid.to_string().contains(filter))
        || group
            .parent_name
            .as_deref()
            .is_some_and(|name| name.to_lowercase().contains(filter))
}

fn process_matches(process: &ZombieProcessView, filter: &str) -> bool {
    process.pid.to_string().contains(filter)
        || process.name.to_lowercase().contains(filter)
        || process.command.to_lowercase().contains(filter)
}
