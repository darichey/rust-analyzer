//! A `cargo-metadata`-equivalent for non-Cargo build systems.
use std::{io, process::Command};

use crossbeam_channel::Sender;
use paths::{AbsPathBuf, Utf8Path, Utf8PathBuf};
use project_model::ProjectJsonData;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::command::{CommandHandle, ParseFromLine};

/// A command wrapper for getting a `rust-project.json`.
///
/// This is analogous to `cargo-metadata`, but for non-Cargo build systems.
pub struct Discover {
    command: Vec<String>,
    sender: Sender<DiscoverProjectMessage>,
}

#[derive(PartialEq, Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Arguments {
    Path(
        #[serde(serialize_with = "serialize_abs_pathbuf")]
        #[serde(deserialize_with = "deserialize_abs_pathbuf")]
        AbsPathBuf,
    ),
    Label(String),
}

fn deserialize_abs_pathbuf<'de, D>(de: D) -> std::result::Result<AbsPathBuf, D::Error>
where
    D: serde::de::Deserializer<'de>,
{
    let path = String::deserialize(de)?;

    AbsPathBuf::try_from(path.as_ref())
        .map_err(|err| serde::de::Error::custom(format!("invalid path name: {err:?}")))
}

fn serialize_abs_pathbuf<S>(path: &AbsPathBuf, se: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    let path: &Utf8Path = path.as_ref();
    se.serialize_str(path.as_str())
}

impl Discover {
    /// Create a new [Discover].
    pub fn new(sender: Sender<DiscoverProjectMessage>, command: Vec<String>) -> Self {
        Self { sender, command }
    }

    /// Spawn the command inside [Discover] and report progress, if any.
    pub fn spawn(&self, arg: Arguments) -> io::Result<DiscoverHandle> {
        let command = &self.command[0];
        let args = &self.command[1..];

        let mut cmd = Command::new(command);
        cmd.args(args);

        let arg = serde_json::to_string(&arg)?;
        cmd.arg(arg);

        Ok(DiscoverHandle { _handle: CommandHandle::spawn(cmd, self.sender.clone())? })
    }
}

/// A handle to a spawned [Discover].
#[derive(Debug)]
pub struct DiscoverHandle {
    _handle: CommandHandle<DiscoverProjectMessage>,
}

/// An enum containing either progress messages or the materialized rust-project.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "kind")]
#[serde(rename_all = "snake_case")]
enum DiscoverProjectData {
    Finished { buildfile: Utf8PathBuf, project: ProjectJsonData },
    Error { error: String, source: Option<String> },
    Progress { message: String },
}

#[derive(Debug, PartialEq, Clone)]
pub enum DiscoverProjectMessage {
    Finished { project: ProjectJsonData, buildfile: AbsPathBuf },
    Error { error: String, source: Option<String> },
    Progress { message: String },
}

impl DiscoverProjectMessage {
    fn new(data: DiscoverProjectData) -> Self {
        match data {
            DiscoverProjectData::Finished { project, buildfile, .. } => {
                let buildfile = buildfile.try_into().expect("Unable to make path absolute");
                DiscoverProjectMessage::Finished { project, buildfile }
            }
            DiscoverProjectData::Error { error, source } => {
                DiscoverProjectMessage::Error { error, source }
            }
            DiscoverProjectData::Progress { message } => {
                DiscoverProjectMessage::Progress { message }
            }
        }
    }
}

impl ParseFromLine for DiscoverProjectMessage {
    fn from_line(line: &str, _error: &mut String) -> Option<Self> {
        // can the line even be deserialized as JSON?
        let Ok(data) = serde_json::from_str::<Value>(line) else {
            let err = DiscoverProjectData::Error { error: line.to_owned(), source: None };
            return Some(DiscoverProjectMessage::new(err));
        };

        let Ok(data) = serde_json::from_value::<DiscoverProjectData>(data) else {
            return None;
        };

        let msg = DiscoverProjectMessage::new(data);
        Some(msg)
    }

    fn from_eof() -> Option<Self> {
        None
    }
}

#[test]
fn test_deserialization() {
    let message = r#"
    {"kind": "progress", "message":"querying build system","input":{"files":["src/main.rs"]}}
    "#;
    let message: DiscoverProjectData =
        serde_json::from_str(message).expect("Unable to deserialize message");
    assert!(matches!(message, DiscoverProjectData::Progress { .. }));

    let message = r#"
    {"kind": "error", "error":"failed to deserialize command output","source":"command"}
    "#;

    let message: DiscoverProjectData =
        serde_json::from_str(message).expect("Unable to deserialize message");
    assert!(matches!(message, DiscoverProjectData::Error { .. }));

    let message = r#"
    {"kind": "finished", "project": {"sysroot": "foo", "crates": [], "runnables": []}, "buildfile":"/Users/dbarsky/Developer/rust-analyzer"}
    "#;

    let message: DiscoverProjectData =
        serde_json::from_str(message).expect("Unable to deserialize message");
    assert!(matches!(message, DiscoverProjectData::Finished { .. }));
}
