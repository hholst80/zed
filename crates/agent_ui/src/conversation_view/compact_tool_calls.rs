use std::ops::Range;

use acp_thread::{AgentThreadEntry, AssistantMessageChunk, ToolCallStatus};
use collections::HashSet;
use gpui::App;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ActivityKind {
    Thinking,
    Read,
    Search,
    Execute,
    Edit,
    Other,
}

impl ActivityKind {
    pub fn for_entry(entry: &AgentThreadEntry, cx: &App) -> Option<Self> {
        match entry {
            AgentThreadEntry::ToolCall(call) => {
                if call.is_subagent()
                    || !matches!(
                        call.status,
                        ToolCallStatus::Pending
                            | ToolCallStatus::InProgress
                            | ToolCallStatus::Completed
                    )
                    || call.terminals().any(|terminal| {
                        terminal.read(cx).output().is_some_and(|output| {
                            output.exit_status.is_some_and(|status| !status.success())
                        })
                    })
                {
                    return None;
                }
                use agent_client_protocol::schema::v1::ToolKind;
                Some(match call.kind {
                    ToolKind::Read => Self::Read,
                    ToolKind::Search => Self::Search,
                    ToolKind::Execute => Self::Execute,
                    ToolKind::Edit | ToolKind::Delete | ToolKind::Move => Self::Edit,
                    _ => Self::Other,
                })
            }
            AgentThreadEntry::AssistantMessage(message)
                if !message.chunks.is_empty()
                    && message
                        .chunks
                        .iter()
                        .all(|chunk| matches!(chunk, AssistantMessageChunk::Thought { .. })) =>
            {
                Some(Self::Thinking)
            }
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum ThreadRow {
    Entry(usize),
    Activity {
        entries: Range<usize>,
        expanded: bool,
    },
}

impl ThreadRow {
    pub fn first_entry(&self) -> usize {
        match self {
            Self::Entry(index) => *index,
            Self::Activity { entries, .. } => entries.start,
        }
    }

    pub fn contains_entry(&self, index: usize) -> bool {
        match self {
            Self::Entry(entry) => *entry == index,
            Self::Activity { entries, expanded } => {
                if *expanded {
                    entries.start == index
                } else {
                    entries.contains(&index)
                }
            }
        }
    }
}

pub(super) fn thread_rows(
    kinds: &[Option<ActivityKind>],
    compact: bool,
    expanded: &HashSet<usize>,
) -> Vec<ThreadRow> {
    let mut rows = Vec::new();
    let mut index = 0;
    while index < kinds.len() {
        if !compact || kinds[index].is_none() {
            rows.push(ThreadRow::Entry(index));
            index += 1;
            continue;
        }
        let start = index;
        while index < kinds.len() && kinds[index].is_some() {
            index += 1;
        }
        let expanded = expanded.contains(&start);
        rows.push(ThreadRow::Activity {
            entries: start..index,
            expanded,
        });
        if expanded {
            // The first entry shares a row with the disclosure header, keeping
            // its existing focus handle usable in both display modes.
            rows.extend((start + 1..index).map(ThreadRow::Entry));
        }
    }
    rows
}

pub(super) fn row_for_entry(rows: &[ThreadRow], entry: usize) -> usize {
    rows.iter()
        .position(|row| row.contains_entry(entry))
        .unwrap_or(rows.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_activity_between_messages_and_actionable_entries() {
        let kinds = [
            None,
            Some(ActivityKind::Thinking),
            Some(ActivityKind::Read),
            Some(ActivityKind::Execute),
            None,
            Some(ActivityKind::Edit),
            None,
        ];
        let rows = thread_rows(&kinds, true, &HashSet::default());
        assert_eq!(
            rows,
            vec![
                ThreadRow::Entry(0),
                ThreadRow::Activity {
                    entries: 1..4,
                    expanded: false
                },
                ThreadRow::Entry(4),
                ThreadRow::Activity {
                    entries: 5..6,
                    expanded: false
                },
                ThreadRow::Entry(6)
            ]
        );
        assert_eq!(row_for_entry(&rows, 3), 1);
        assert_eq!(row_for_entry(&rows, 4), 2);
        assert_eq!(row_for_entry(&rows, kinds.len()), rows.len());
    }

    #[test]
    fn expansion_preserves_entry_order_and_navigation() {
        let kinds = [
            None,
            Some(ActivityKind::Read),
            Some(ActivityKind::Execute),
            None,
        ];
        let mut expanded = HashSet::default();
        expanded.insert(1);
        let rows = thread_rows(&kinds, true, &expanded);
        assert_eq!(
            rows,
            vec![
                ThreadRow::Entry(0),
                ThreadRow::Activity {
                    entries: 1..3,
                    expanded: true
                },
                ThreadRow::Entry(2),
                ThreadRow::Entry(3)
            ]
        );
        for index in 0..kinds.len() {
            assert_eq!(row_for_entry(&rows, index), index);
        }
    }

    #[test]
    fn streaming_growth_and_permission_requests_change_group_boundaries() {
        let mut kinds = vec![Some(ActivityKind::Execute)];
        let expanded = HashSet::default();
        kinds.push(Some(ActivityKind::Read));
        assert_eq!(
            thread_rows(&kinds, true, &expanded),
            vec![ThreadRow::Activity {
                entries: 0..2,
                expanded: false
            }]
        );
        kinds[0] = None;
        assert_eq!(
            thread_rows(&kinds, true, &expanded),
            vec![
                ThreadRow::Entry(0),
                ThreadRow::Activity {
                    entries: 1..2,
                    expanded: false
                }
            ]
        );
    }

    #[test]
    fn disabled_mode_preserves_the_original_list() {
        let kinds = [
            Some(ActivityKind::Thinking),
            Some(ActivityKind::Execute),
            None,
        ];
        assert_eq!(
            thread_rows(&kinds, false, &HashSet::default()),
            (0..kinds.len()).map(ThreadRow::Entry).collect::<Vec<_>>()
        );
        assert!(thread_rows(&[], true, &HashSet::default()).is_empty());
    }
}
