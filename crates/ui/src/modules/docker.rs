//! Docker module: every node's compose projects, containers and tailed
//! logs (read-only monitor + logs, no actions).
//!
//! Projects read as one table with their containers beneath them; the log
//! tails follow. The session filter narrows projects and containers by
//! name; `j`/`k` scroll the logs.

use fox_services_top_core::view::{DockerProjectView, DockerView, ViewModel};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use super::ModuleBuildError;
use super::common::{
    ColumnSpec, ManifestSpec, Output, RowCursor, TableCell, build_manifest, draw_line,
    draw_table_header, draw_table_row, fit_columns, message, no_data, panel_with_keys,
};
use crate::format::clean;
use crate::num::saturate_u16;
use crate::registry::{DisplayContext, DisplayModule, DisplayModuleManifest, RenderOutput, Slot};

/// The keys acting on the view, at the right of its title.
const DOCKER_KEYS: &str = "j/k scroll logs · / filter";

const PROJECT_COLUMNS: [ColumnSpec; 6] = [
    ColumnSpec::left("NODE", 8, 100).flexible().prefer(12),
    ColumnSpec::left("PROJECT", 12, 95).flexible().prefer(20),
    ColumnSpec::left("STATUS", 8, 85),
    ColumnSpec::right("CTNS", 6, 70),
    ColumnSpec::right("CPU%", 8, 60),
    ColumnSpec::right("MEM", 10, 50),
];

pub struct DockerModule {
    manifest: DisplayModuleManifest,
}

impl DockerModule {
    pub fn new() -> Result<Self, ModuleBuildError> {
        Ok(Self {
            manifest: build_manifest(ManifestSpec {
                id: "docker",
                title: "Docker services",
                min_columns: 40,
                min_rows: 5,
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

    fn render(
        &self,
        context: &DisplayContext,
        model: &ViewModel,
        area: Rect,
        frame: &mut Frame<'_>,
    ) -> RenderOutput {
        let mut output = Output::default();
        let Some(docker) = model.modules.docker.as_deref() else {
            let inner = panel_with_keys(frame, area, "Docker services", DOCKER_KEYS, context);
            message(frame, inner, &no_data("docker"), context.theme.muted_style());
            output.label("docker: no data");
            return output.finish();
        };
        let running: u32 = docker.projects.iter().map(|project| project.running).sum();
        let total: u32 = docker.projects.iter().map(|project| project.total).sum();
        let inner = panel_with_keys(
            frame,
            area,
            &format!("Docker services · {running}/{total} containers up"),
            DOCKER_KEYS,
            context,
        );
        if !docker.enabled {
            message(
                frame,
                inner,
                "remote monitoring is disabled (config remote.enabled)",
                context.theme.muted_style(),
            );
            output.label("docker: disabled");
            return output.finish();
        }
        let mut rows = RowCursor::new(inner);
        let filter = model.session.filter.to_lowercase();
        let projects: Vec<&DockerProjectView> = docker
            .projects
            .iter()
            .filter(|project| {
                filter.is_empty()
                    || project.project.to_lowercase().contains(&filter)
                    || project.node.to_lowercase().contains(&filter)
                    || project
                        .containers
                        .iter()
                        .any(|container| container.name.to_lowercase().contains(&filter))
            })
            .collect();
        if projects.is_empty() {
            message(
                frame,
                inner,
                "no projects reported (nodes offline or nothing deployed)",
                context.theme.muted_style(),
            );
            output.label("docker: none reported");
            return output.finish();
        }
        render_projects(frame, &mut rows, &projects, context, &mut output);
        render_logs(frame, &mut rows, docker, context, &mut output);
        output.finish()
    }
}

fn render_projects(
    frame: &mut Frame<'_>,
    rows: &mut RowCursor,
    projects: &[&DockerProjectView],
    context: &DisplayContext,
    output: &mut Output,
) {
    // One header, one row per project and one per container.
    let wanted = projects
        .iter()
        .map(|project| project.containers.len().max(1) + 1)
        .sum::<usize>()
        + 1;
    let Some(table) = rows.take(saturate_u16(wanted.max(2)).min(rows.remaining().max(4))) else {
        return;
    };
    let mut cursor = RowCursor::new(table);
    let Some(header) = cursor.next_row() else { return };
    let columns = fit_columns(&PROJECT_COLUMNS, header.width);
    draw_table_header(
        frame.buffer_mut(),
        header,
        &PROJECT_COLUMNS,
        &columns,
        context.theme.title_style(),
    );
    for project in projects {
        if cursor.remaining() == 0 {
            break;
        }
        let Some(row) = cursor.next_row() else { break };
        draw_table_row(
            frame.buffer_mut(),
            row,
            &columns,
            &project_cells(project, context.theme.text_style()),
            None,
        );
        output.label(format!("project {}/{}", project.node, project.project));
        for container in &project.containers {
            let Some(row) = cursor.next_row() else { break };
            let text = format!(
                "  {} · {} · {} · {}",
                clean(&container.name),
                clean(&container.status),
                clean(&container.image),
                clean(&container.ports),
            );
            draw_line(
                frame.buffer_mut(),
                row,
                &Line::from(Span::styled(text, context.theme.muted_style())),
            );
        }
    }
}

fn project_cells(project: &DockerProjectView, style: Style) -> Vec<TableCell> {
    let cpu: f64 = project
        .containers
        .iter()
        .filter_map(|container| container.cpu_pct)
        .sum();
    let mem: f64 = project
        .containers
        .iter()
        .filter_map(|container| container.mem_used_mib)
        .sum();
    // A float `sum` of no items is `-0.0`; display it as zero.
    let (cpu, mem) = (cpu + 0.0, mem + 0.0);
    vec![
        TableCell::new(clean(&project.node), style),
        TableCell::new(clean(&project.project), style),
        TableCell::new(clean(&project.status), style),
        TableCell::new(format!("{}/{}", project.running, project.total), style),
        TableCell::new(format!("{cpu:.1}"), style),
        TableCell::new(format!("{mem:.0} MiB"), style),
    ]
}

fn render_logs(
    frame: &mut Frame<'_>,
    rows: &mut RowCursor,
    docker: &DockerView,
    context: &DisplayContext,
    output: &mut Output,
) {
    if docker.logs.is_empty() || rows.remaining() < 3 {
        return;
    }
    let Some(title) = rows.next_row() else { return };
    draw_line(
        frame.buffer_mut(),
        title,
        &Line::from(Span::styled(
            format!("Logs ({})", docker.logs.len()),
            context.theme.title_style(),
        )),
    );
    let mut skip = usize::from(context.scroll);
    for entry in &docker.logs {
        if rows.remaining() == 0 {
            break;
        }
        if skip > 0 {
            skip = skip.saturating_sub(1 + entry.lines.len());
            if skip > 0 {
                continue;
            }
        }
        let Some(header) = rows.next_row() else { break };
        draw_line(
            frame.buffer_mut(),
            header,
            &Line::from(Span::styled(
                format!("{} · {}", clean(&entry.node), clean(&entry.container)),
                context.theme.text_style(),
            )),
        );
        output.label(format!("logs {}", entry.container));
        for line in &entry.lines {
            let Some(row) = rows.next_row() else { break };
            draw_line(
                frame.buffer_mut(),
                row,
                &Line::from(Span::styled(
                    format!("  {}", clean(line)),
                    context.theme.muted_style(),
                )),
            );
        }
    }
}
