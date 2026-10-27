use std::collections::BTreeMap;

use fox_services_top_core::canonical::ProcessTable;
use fox_services_top_core::view::{ZombieGroupView, ZombieProcessView, ZombiesView};

pub(crate) fn build_zombies_view(table: &ProcessTable) -> ZombiesView {
    let mut groups: BTreeMap<Option<i32>, ZombieGroupView> = BTreeMap::new();
    for process in &table.zombies {
        let group = groups
            .entry(process.parent_pid)
            .or_insert_with(|| ZombieGroupView {
                parent_pid: process.parent_pid,
                parent_name: process.parent_name.clone(),
                processes: Vec::new(),
            });
        group.processes.push(ZombieProcessView {
            pid: process.pid,
            name: process.name.clone(),
            command: process.command.clone(),
        });
    }

    let mut groups: Vec<ZombieGroupView> = groups.into_values().collect();
    groups.sort_by_key(|group| {
        (
            group.parent_pid.is_none(),
            group.parent_pid.unwrap_or_default(),
        )
    });
    let total_processes = groups.iter().fold(0_u32, |total, group| {
        total.saturating_add(u32::try_from(group.processes.len()).unwrap_or(u32::MAX))
    });

    ZombiesView {
        groups,
        total_processes,
        truncated: false,
    }
}

#[cfg(test)]
mod tests {
    use fox_services_top_core::canonical::ZombieProcess;

    use super::*;

    #[test]
    fn groups_zombies_by_parent_and_keeps_unknown_parent_last() {
        let table = ProcessTable {
            zombies: vec![
                ZombieProcess {
                    pid: 18,
                    parent_pid: None,
                    name: "unknown-parent-child".into(),
                    command: String::new(),
                    parent_name: None,
                },
                ZombieProcess {
                    pid: 12,
                    parent_pid: Some(5),
                    name: "worker-a".into(),
                    command: "worker --a".into(),
                    parent_name: Some("supervisor".into()),
                },
                ZombieProcess {
                    pid: 13,
                    parent_pid: Some(5),
                    name: "worker-b".into(),
                    command: "worker --b".into(),
                    parent_name: Some("supervisor".into()),
                },
            ],
            ..ProcessTable::default()
        };

        let view = build_zombies_view(&table);
        assert_eq!(view.total_processes, 3);
        assert_eq!(view.groups.len(), 2);
        assert_eq!(view.groups[0].parent_pid, Some(5));
        assert_eq!(view.groups[0].parent_name.as_deref(), Some("supervisor"));
        assert_eq!(view.groups[0].processes.len(), 2);
        assert_eq!(view.groups[1].parent_pid, None);
    }
}
