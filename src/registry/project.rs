use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub path: PathBuf,
    pub branch: String,
    pub base: String,
    #[serde(with = "time::serde::rfc3339")]
    pub created: OffsetDateTime,
    #[serde(default)]
    pub issue: Option<u32>,
    #[serde(default)]
    pub frozen: bool,
}
