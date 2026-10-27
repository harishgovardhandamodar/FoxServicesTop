use fox_services_top_core::process_stat::ProcessStat;

use super::*;

#[test]
fn zombies_are_collected_before_regular_process_rows_are_bounded() {
    let at = SECOND;
    let parent = compact(
        1,
        1,
        100,
        ProcessStat {
            ppid: Some(0),
            state: Some('S'),
            ..ProcessStat::default()
        },
        at,
    );
    let mut first = compact(
        90,
        1,
        0,
        ProcessStat {
            ppid: Some(1),
            state: Some('Z'),
            ..ProcessStat::default()
        },
        at,
    );
    first.provenance.channel = "worker-first".into();
    let mut second = compact(
        91,
        1,
        0,
        ProcessStat {
            ppid: Some(1),
            state: Some('Z'),
            ..ProcessStat::default()
        },
        at,
    );
    second.provenance.channel = "worker-second".into();

    let mut limits = limits();
    limits.max_processes = 1;
    let collector = ProcessCollector::new().unwrap();
    let batch = collect(
        &collector,
        &plan(vec![parent, first, second], vec![], at + 1)
            .limits(limits)
            .build(),
    );
    let table = batch.process_table.as_ref().unwrap();

    assert_eq!(table.rows.len(), 1);
    assert_eq!(table.zombies.len(), 2);
    assert_eq!(table.zombies[0].parent_pid, Some(1));
    assert_eq!(table.zombies[0].parent_name.as_deref(), Some("worker-1"));
    assert_eq!(table.zombies[0].name, "worker-first");
    assert_eq!(table.zombies[1].pid, 91);
}
