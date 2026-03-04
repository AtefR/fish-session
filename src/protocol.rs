use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub name: String,
    pub cwd: PathBuf,
    pub pid: i32,
    pub attached: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TerminalEnv {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub term: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub colorterm: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub term_program: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub term_program_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminfo: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminfo_dirs: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    Ping,
    List,
    Create {
        name: String,
        cwd: Option<PathBuf>,
        #[serde(skip_serializing_if = "Option::is_none")]
        terminal_env: Option<TerminalEnv>,
    },
    Delete {
        name: String,
    },
    Rename {
        from: String,
        to: String,
    },
    Attach {
        name: String,
        rows: Option<u16>,
        cols: Option<u16>,
        replay: Option<bool>,
    },
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sessions: Option<Vec<SessionInfo>>,
}

impl Response {
    pub fn ok() -> Self {
        Self {
            ok: true,
            error: None,
            sessions: None,
        }
    }

    pub fn with_sessions(sessions: Vec<SessionInfo>) -> Self {
        Self {
            ok: true,
            error: None,
            sessions: Some(sessions),
        }
    }

    pub fn err(message: impl Into<String>) -> Self {
        Self {
            ok: false,
            error: Some(message.into()),
            sessions: None,
        }
    }
}
