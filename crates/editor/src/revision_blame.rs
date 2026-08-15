use gpui::{App, Context, Entity, SharedString, WeakEntity};
use language::Buffer;
use multi_buffer::{MultiBuffer, RowInfo};
use text::BufferId;

/// Provider-neutral blame metadata for a single line in a source-control working copy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RevisionBlameEntry {
    /// One-based line number in the provider's current blame output.
    pub line_number: u32,
    /// Stable revision identity, used to assign a consistent gutter color.
    pub revision_id: Option<SharedString>,
    /// Human-readable revision label, such as `r1234` for Subversion.
    pub revision_label: Option<SharedString>,
    pub author: Option<SharedString>,
    pub timestamp: Option<SharedString>,
}

struct RevisionBlameBuffer {
    buffer: WeakEntity<Buffer>,
    buffer_id: BufferId,
    reference_text: SharedString,
    reference_entries: Vec<Option<RevisionBlameEntry>>,
    aligned_entries: Vec<Option<RevisionBlameEntry>>,
    edits: text::Subscription<usize>,
}

/// Blame state supplied by a non-Git source-control provider.
///
/// Provider blame initially refers to the working-copy text that was open when
/// blame was requested. We retain that snapshot so subsequent editor changes can
/// be aligned while newly added or modified rows remain unannotated.
pub(crate) struct RevisionBlame {
    buffer: Option<RevisionBlameBuffer>,
}

impl RevisionBlame {
    pub(crate) fn new(
        multi_buffer: Entity<MultiBuffer>,
        reference_text: SharedString,
        entries: Vec<RevisionBlameEntry>,
        cx: &mut Context<Self>,
    ) -> Self {
        let buffer = multi_buffer.read(cx).as_singleton().map(|buffer| {
            let buffer_id = buffer.read(cx).remote_id();
            let current_text = buffer.read(cx).text();
            let edits = buffer.update(cx, |buffer, _| buffer.subscribe());
            let reference_entries = entries_for_text(&reference_text, &entries);
            let aligned_entries = align_entries(&reference_text, &current_text, &reference_entries);
            RevisionBlameBuffer {
                buffer: buffer.downgrade(),
                buffer_id,
                reference_text,
                reference_entries,
                aligned_entries,
                edits,
            }
        });

        Self { buffer }
    }

    pub(crate) fn has_entries(&mut self, cx: &mut App) -> bool {
        self.sync(cx);
        self.buffer
            .as_ref()
            .is_some_and(|buffer| buffer.aligned_entries.iter().any(Option::is_some))
    }

    pub(crate) fn max_author_length(&mut self, cx: &mut App) -> usize {
        self.sync(cx);
        self.buffer
            .as_ref()
            .into_iter()
            .flat_map(|buffer| buffer.aligned_entries.iter())
            .filter_map(|entry| entry.as_ref()?.author.as_ref())
            .map(|author| author.chars().count())
            .max()
            .unwrap_or_default()
    }

    pub(crate) fn entries_for_rows<'a>(
        &'a mut self,
        rows: &'a [RowInfo],
        cx: &mut App,
    ) -> impl Iterator<Item = Option<RevisionBlameEntry>> + use<'a> {
        self.sync(cx);
        let buffer_id = self.buffer.as_ref().map(|buffer| buffer.buffer_id);
        let entries = self.buffer.as_ref().map(|buffer| &buffer.aligned_entries);
        rows.iter().map(move |row| {
            let buffer_id = buffer_id?;
            if row.buffer_id != Some(buffer_id) {
                return None;
            }
            let entries = entries?;
            entries.get(row.buffer_row? as usize)?.clone()
        })
    }

    fn sync(&mut self, cx: &mut App) {
        let Some(buffer) = self.buffer.as_mut() else {
            return;
        };
        if buffer.edits.consume().is_empty() {
            return;
        }
        let Some(live_buffer) = buffer.buffer.upgrade() else {
            return;
        };
        let current_text = live_buffer.read(cx).text();
        buffer.aligned_entries = align_entries(
            &buffer.reference_text,
            &current_text,
            &buffer.reference_entries,
        );
    }
}

fn line_count(text: &str) -> usize {
    text.bytes().filter(|byte| *byte == b'\n').count() + 1
}

fn entries_for_text(text: &str, entries: &[RevisionBlameEntry]) -> Vec<Option<RevisionBlameEntry>> {
    let mut rows = vec![None; line_count(text)];
    for entry in entries {
        if entry.revision_id.is_none() && entry.revision_label.is_none() {
            continue;
        }
        let Some(row) = entry.line_number.checked_sub(1).map(|row| row as usize) else {
            continue;
        };
        if let Some(slot) = rows.get_mut(row) {
            *slot = Some(entry.clone());
        }
    }
    rows
}

fn align_entries(
    reference_text: &str,
    current_text: &str,
    reference_entries: &[Option<RevisionBlameEntry>],
) -> Vec<Option<RevisionBlameEntry>> {
    let mut aligned = Vec::with_capacity(line_count(current_text));
    let mut old_row = 0;
    for (old_range, new_range) in language::line_diff(reference_text, current_text) {
        let unchanged_end = (old_range.start as usize).min(reference_entries.len());
        if old_row < unchanged_end {
            aligned.extend_from_slice(&reference_entries[old_row..unchanged_end]);
        }
        aligned.extend(std::iter::repeat_n(None, new_range.len()));
        old_row = (old_range.end as usize).min(reference_entries.len());
    }
    aligned.extend_from_slice(&reference_entries[old_row..]);
    aligned.resize(line_count(current_text), None);
    aligned.truncate(line_count(current_text));
    aligned
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(line_number: u32, revision: &str) -> RevisionBlameEntry {
        RevisionBlameEntry {
            line_number,
            revision_id: Some(revision.to_owned().into()),
            revision_label: Some(revision.to_owned().into()),
            author: Some("author".into()),
            timestamp: None,
        }
    }

    #[test]
    fn uses_provider_rows_and_aligns_later_editor_edits() {
        let entries = vec![entry(1, "1"), entry(3, "2"), entry(4, "3")];
        let reference_text = "one\nlocal\ntwo\nthree\n";
        let reference_entries = entries_for_text(reference_text, &entries);
        let aligned = align_entries(
            reference_text,
            "one\nlocal\nnew editor line\ntwo changed\nthree\n",
            &reference_entries,
        );
        let revisions = aligned
            .iter()
            .map(|entry| {
                entry
                    .as_ref()
                    .and_then(|entry| entry.revision_label.as_deref())
            })
            .collect::<Vec<_>>();

        assert_eq!(
            revisions,
            vec![Some("1"), None, None, None, Some("3"), None]
        );
    }

    #[test]
    fn leaves_provider_rows_without_a_revision_unannotated() {
        let mut local_entry = entry(2, "local");
        local_entry.revision_id = None;
        local_entry.revision_label = None;
        let rows = entries_for_text("one\nlocal\n", &[entry(1, "1"), local_entry]);

        assert!(rows[0].is_some());
        assert!(rows[1].is_none());
    }
}
