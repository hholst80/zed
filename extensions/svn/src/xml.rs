use quick_xml::de::from_str;
use serde::{Deserialize, Deserializer};
use source_control::{
    SourceControlBlameLine, SourceControlContentStatus, SourceControlHistoryEntry,
    SourceControlNodeKind, SourceControlPath, SourceControlRevision,
    SourceControlSelectionEligibility, SourceControlStatusEntry,
};
use std::{path::Path, sync::Arc};

use crate::SvnError;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SvnInfo {
    pub path: Arc<Path>,
    pub working_copy_root: Arc<Path>,
    pub url: Arc<str>,
    pub relative_url: Arc<str>,
    pub repository_root: Arc<str>,
    pub repository_uuid: Arc<str>,
    pub revision: Option<u64>,
    pub node_kind: SourceControlNodeKind,
}

#[derive(Deserialize)]
#[serde(rename = "info")]
struct InfoDocument {
    #[serde(rename = "entry", default)]
    entries: Vec<InfoEntry>,
}

#[derive(Deserialize)]
struct InfoEntry {
    #[serde(rename = "@kind")]
    kind: String,
    #[serde(rename = "@path")]
    path: String,
    #[serde(
        rename = "@revision",
        default,
        deserialize_with = "deserialize_optional_revision"
    )]
    revision: Option<u64>,
    url: String,
    #[serde(rename = "relative-url")]
    relative_url: String,
    repository: InfoRepository,
    #[serde(rename = "wc-info")]
    working_copy: WorkingCopyInfo,
}

#[derive(Deserialize)]
struct InfoRepository {
    root: String,
    uuid: String,
}

#[derive(Deserialize)]
struct WorkingCopyInfo {
    #[serde(rename = "wcroot-abspath")]
    root_path: String,
}

fn deserialize_optional_revision<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = String::deserialize(deserializer)?;
    Ok(value.parse().ok())
}

pub fn parse_info(xml: &str) -> Result<Vec<SvnInfo>, SvnError> {
    let document: InfoDocument = from_str(xml).map_err(SvnError::malformed_xml)?;
    document
        .entries
        .into_iter()
        .map(|entry| {
            let working_copy_root = Path::new(&entry.working_copy.root_path);
            if !working_copy_root.is_absolute() {
                return Err(SvnError::MalformedResponse(
                    "svn info returned a non-absolute working-copy root".into(),
                ));
            }
            Ok(SvnInfo {
                path: Arc::from(Path::new(&entry.path)),
                working_copy_root: Arc::from(working_copy_root),
                url: entry.url.into(),
                relative_url: entry.relative_url.into(),
                repository_root: entry.repository.root.into(),
                repository_uuid: entry.repository.uuid.into(),
                revision: entry.revision,
                node_kind: parse_node_kind(&entry.kind),
            })
        })
        .collect()
}

#[derive(Deserialize)]
#[serde(rename = "status")]
struct StatusDocument {
    #[serde(rename = "target", default)]
    targets: Vec<StatusTarget>,
}

#[derive(Deserialize)]
struct StatusTarget {
    #[serde(rename = "entry", default)]
    entries: Vec<StatusEntry>,
}

#[derive(Deserialize)]
struct StatusEntry {
    #[serde(rename = "@path")]
    path: String,
    #[serde(rename = "wc-status")]
    working_copy: WorkingCopyStatus,
    #[serde(rename = "repos-status")]
    repository: Option<RepositoryStatus>,
}

#[derive(Deserialize)]
struct WorkingCopyStatus {
    #[serde(rename = "@item")]
    item: String,
    #[serde(rename = "@props")]
    properties: String,
    #[serde(rename = "@copied", default)]
    copied: bool,
    #[serde(rename = "@switched", default)]
    switched: bool,
    #[serde(rename = "@tree-conflicted", default)]
    tree_conflicted: bool,
    lock: Option<XmlLock>,
}

#[derive(Deserialize)]
struct RepositoryStatus {
    #[serde(rename = "@item")]
    item: String,
    #[serde(rename = "@props")]
    properties: String,
    lock: Option<XmlLock>,
}

#[derive(Deserialize)]
struct XmlLock {}

pub fn parse_status(
    xml: &str,
    working_copy_root: &Path,
) -> Result<Vec<SourceControlStatusEntry>, SvnError> {
    let document: StatusDocument = from_str(xml).map_err(SvnError::malformed_xml)?;
    document
        .targets
        .into_iter()
        .flat_map(|target| target.entries)
        .map(|entry| {
            let content_status = parse_content_status(&entry.working_copy.item);
            let property_status = parse_content_status(&entry.working_copy.properties);
            let selection = selection_eligibility(
                content_status,
                property_status,
                entry.working_copy.tree_conflicted,
            );
            let out_of_date = entry.repository.as_ref().is_some_and(|repository| {
                parse_content_status(&repository.item).is_changed()
                    || parse_content_status(&repository.properties).is_changed()
            });
            let locked = entry.working_copy.lock.is_some()
                || entry
                    .repository
                    .as_ref()
                    .is_some_and(|repository| repository.lock.is_some());
            Ok(SourceControlStatusEntry {
                path: normalize_status_path(&entry.path, working_copy_root)?,
                node_kind: SourceControlNodeKind::Unknown,
                content_status,
                property_status,
                selection,
                copied: entry.working_copy.copied,
                switched: entry.working_copy.switched,
                locked,
                tree_conflicted: entry.working_copy.tree_conflicted,
                out_of_date,
                added_lines: None,
                deleted_lines: None,
            })
        })
        .collect()
}

fn normalize_status_path(
    raw_path: &str,
    working_copy_root: &Path,
) -> Result<SourceControlPath, SvnError> {
    let path = Path::new(raw_path);
    let relative_path = if path.is_absolute() {
        path.strip_prefix(working_copy_root).map_err(|_| {
            SvnError::MalformedResponse(
                "svn status returned a path outside the working copy".into(),
            )
        })?
    } else {
        path
    };
    let path = relative_path
        .components()
        .map(|component| component.as_os_str().to_str())
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| {
            SvnError::MalformedResponse("svn returned a non-UTF-8 working-copy path".into())
        })?
        .join("/");
    SourceControlPath::new(path).map_err(|error| SvnError::MalformedResponse(error.message))
}

fn parse_content_status(value: &str) -> SourceControlContentStatus {
    match value {
        "none" | "normal" => SourceControlContentStatus::Normal,
        "modified" | "merged" => SourceControlContentStatus::Modified,
        "added" => SourceControlContentStatus::Added,
        "deleted" => SourceControlContentStatus::Deleted,
        "missing" => SourceControlContentStatus::Missing,
        "replaced" => SourceControlContentStatus::Replaced,
        "conflicted" => SourceControlContentStatus::Conflicted,
        "incomplete" | "obstructed" => SourceControlContentStatus::Obstructed,
        "unversioned" => SourceControlContentStatus::Unversioned,
        "ignored" => SourceControlContentStatus::Ignored,
        "external" => SourceControlContentStatus::External,
        _ => SourceControlContentStatus::Obstructed,
    }
}

fn selection_eligibility(
    content_status: SourceControlContentStatus,
    property_status: SourceControlContentStatus,
    tree_conflicted: bool,
) -> SourceControlSelectionEligibility {
    if tree_conflicted
        || matches!(
            property_status,
            SourceControlContentStatus::Conflicted | SourceControlContentStatus::Obstructed
        )
    {
        return SourceControlSelectionEligibility::Unavailable;
    }
    match content_status {
        SourceControlContentStatus::Unversioned => SourceControlSelectionEligibility::RequiresAdd,
        SourceControlContentStatus::Missing => SourceControlSelectionEligibility::RequiresDelete,
        SourceControlContentStatus::Ignored
        | SourceControlContentStatus::External
        | SourceControlContentStatus::Obstructed
        | SourceControlContentStatus::Conflicted => SourceControlSelectionEligibility::Unavailable,
        SourceControlContentStatus::Normal if !property_status.is_changed() => {
            SourceControlSelectionEligibility::Unavailable
        }
        SourceControlContentStatus::Normal
        | SourceControlContentStatus::Modified
        | SourceControlContentStatus::Added
        | SourceControlContentStatus::Deleted
        | SourceControlContentStatus::Replaced => {
            SourceControlSelectionEligibility::IncludedByDefault
        }
    }
}

#[derive(Deserialize)]
#[serde(rename = "log")]
struct LogDocument {
    #[serde(rename = "logentry", default)]
    entries: Vec<LogEntry>,
}

#[derive(Deserialize)]
struct LogEntry {
    #[serde(rename = "@revision")]
    revision: u64,
    author: Option<String>,
    date: Option<String>,
    #[serde(default)]
    msg: String,
    paths: Option<LogPaths>,
}

#[derive(Deserialize)]
struct LogPaths {
    #[serde(rename = "path", default)]
    paths: Vec<LogPath>,
}

#[derive(Deserialize)]
struct LogPath {
    #[serde(rename = "$text", default)]
    path: String,
}

pub fn parse_log(xml: &str) -> Result<Vec<SourceControlHistoryEntry>, SvnError> {
    let document: LogDocument = from_str(xml).map_err(SvnError::malformed_xml)?;
    document
        .entries
        .into_iter()
        .map(|entry| {
            let changed_paths = entry
                .paths
                .into_iter()
                .flat_map(|paths| paths.paths)
                .map(|path| {
                    SourceControlPath::new(path.path.trim_start_matches('/').to_owned())
                        .map_err(|error| SvnError::MalformedResponse(error.message))
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(SourceControlHistoryEntry {
                revision: svn_revision(entry.revision),
                author: entry.author.map(Arc::from),
                timestamp: entry.date.map(Arc::from),
                message: entry.msg.into(),
                changed_paths,
            })
        })
        .collect()
}

#[derive(Deserialize)]
#[serde(rename = "blame")]
struct BlameDocument {
    #[serde(rename = "target", default)]
    targets: Vec<BlameTarget>,
}

#[derive(Deserialize)]
struct BlameTarget {
    #[serde(rename = "entry", default)]
    entries: Vec<BlameEntry>,
}

#[derive(Deserialize)]
struct BlameEntry {
    #[serde(rename = "@line-number")]
    line_number: u32,
    commit: Option<BlameCommit>,
}

#[derive(Deserialize)]
struct BlameCommit {
    #[serde(rename = "@revision")]
    revision: u64,
    author: Option<String>,
    date: Option<String>,
}

pub fn parse_blame(xml: &str) -> Result<Vec<SourceControlBlameLine>, SvnError> {
    let document: BlameDocument = from_str(xml).map_err(SvnError::malformed_xml)?;
    Ok(document
        .targets
        .into_iter()
        .flat_map(|target| target.entries)
        .map(|entry| SourceControlBlameLine {
            line_number: entry.line_number,
            revision: entry
                .commit
                .as_ref()
                .map(|commit| svn_revision(commit.revision)),
            author: entry
                .commit
                .as_ref()
                .and_then(|commit| commit.author.as_ref())
                .map(|author| Arc::from(author.as_str())),
            timestamp: entry
                .commit
                .as_ref()
                .and_then(|commit| commit.date.as_ref())
                .map(|date| Arc::from(date.as_str())),
        })
        .collect())
}

fn svn_revision(revision: u64) -> SourceControlRevision {
    SourceControlRevision::new(revision.to_string(), format!("r{revision}"))
}

fn parse_node_kind(value: &str) -> SourceControlNodeKind {
    match value {
        "file" => SourceControlNodeKind::File,
        "dir" => SourceControlNodeKind::Directory,
        _ => SourceControlNodeKind::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_info_document() -> Result<(), Box<dyn std::error::Error>> {
        let info = parse_info(
            r#"<?xml version="1.0" encoding="UTF-8"?>
            <info><entry kind="dir" path="." revision="42">
            <url>https://example.test/svn/project/trunk</url>
            <relative-url>^/trunk</relative-url>
            <repository><root>https://example.test/svn/project</root><uuid>abc-123</uuid></repository>
            <wc-info><wcroot-abspath>/work/project</wcroot-abspath><schedule>normal</schedule><depth>infinity</depth></wc-info>
            </entry></info>"#,
        )?;
        assert_eq!(info.len(), 1);
        let entry = info.first().ok_or("missing info entry")?;
        assert_eq!(entry.repository_uuid.as_ref(), "abc-123");
        assert_eq!(entry.working_copy_root.as_ref(), Path::new("/work/project"));
        assert_eq!(entry.revision, Some(42));
        Ok(())
    }

    #[test]
    fn parses_info_for_paths_scheduled_for_addition() -> Result<(), Box<dyn std::error::Error>> {
        let entries = parse_info(
            r#"<?xml version="1.0" encoding="UTF-8"?>
            <info><entry kind="file" path="/work/project/new.txt" revision="Resource is not under version control.">
            <url>https://example.test/svn/project/trunk/new.txt</url>
            <relative-url>^/trunk/new.txt</relative-url>
            <repository><root>https://example.test/svn/project</root><uuid>abc-123</uuid></repository>
            <wc-info><wcroot-abspath>/work/project</wcroot-abspath><schedule>add</schedule><depth>infinity</depth></wc-info>
            </entry></info>"#,
        )?;
        assert_eq!(entries[0].revision, None);
        assert_eq!(entries[0].node_kind, SourceControlNodeKind::File);
        Ok(())
    }

    #[test]
    fn maps_status_and_property_changes() -> Result<(), Box<dyn std::error::Error>> {
        let statuses = parse_status(
            r#"<?xml version="1.0"?><status><target path=".">
            <entry path="src/main.rs"><wc-status props="none" item="modified" revision="4"/></entry>
            <entry path="new file.txt"><wc-status props="none" item="unversioned"/></entry>
            <entry path="missing.txt"><wc-status props="none" item="missing" revision="4"/></entry>
            <entry path="config"><wc-status props="modified" item="normal" revision="4" switched="true"/></entry>
            <entry path="conflict.txt"><wc-status props="none" item="conflicted" revision="4" tree-conflicted="true"/></entry>
            </target></status>"#,
            Path::new("/work/project"),
        )?;
        assert_eq!(statuses.len(), 5);
        assert_eq!(
            statuses[0].selection,
            SourceControlSelectionEligibility::IncludedByDefault
        );
        assert_eq!(
            statuses[1].selection,
            SourceControlSelectionEligibility::RequiresAdd
        );
        assert_eq!(
            statuses[2].selection,
            SourceControlSelectionEligibility::RequiresDelete
        );
        assert!(statuses[3].switched);
        assert!(statuses[4].is_conflicted());
        assert_eq!(
            statuses[4].selection,
            SourceControlSelectionEligibility::Unavailable
        );
        Ok(())
    }

    #[test]
    fn conflicts_are_not_selected_for_commit() -> Result<(), Box<dyn std::error::Error>> {
        let statuses = parse_status(
            r#"<?xml version="1.0"?><status><target path=".">
            <entry path="property.txt"><wc-status props="conflicted" item="normal" revision="4"/></entry>
            <entry path="tree.txt"><wc-status props="none" item="normal" revision="4" tree-conflicted="true"/></entry>
            </target></status>"#,
            Path::new("/work/project"),
        )?;

        assert!(
            statuses.iter().all(|status| {
                status.selection == SourceControlSelectionEligibility::Unavailable
            })
        );
        Ok(())
    }

    #[test]
    fn parses_log_and_blame() -> Result<(), Box<dyn std::error::Error>> {
        let log = parse_log(
            r#"<?xml version="1.0"?><log><logentry revision="9"><author>Ada</author>
            <date>2026-08-14T10:00:00.000000Z</date><paths><path action="M" kind="file">/trunk/src/main.rs</path></paths>
            <msg>Change the program</msg></logentry></log>"#,
        )?;
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].revision.display.as_ref(), "r9");
        assert_eq!(log[0].changed_paths[0].as_str(), "trunk/src/main.rs");

        let blame = parse_blame(
            r#"<?xml version="1.0"?><blame><target path="src/main.rs"><entry line-number="1">
            <commit revision="9"><author>Ada</author><date>2026-08-14T10:00:00.000000Z</date></commit>
            </entry><entry line-number="2"/></target></blame>"#,
        )?;
        assert_eq!(blame.len(), 2);
        assert_eq!(
            blame[0]
                .revision
                .as_ref()
                .map(|revision| revision.display.as_ref()),
            Some("r9")
        );
        assert!(blame[1].revision.is_none());
        Ok(())
    }
}
