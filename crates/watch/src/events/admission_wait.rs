//! `admission_wait`: admission has held a heavy Bash call back this long;
//! reported once per call.

use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AdmissionWait {
    /// The waiting session's name and id, when its claude process has a
    /// session file.
    pub session: Option<String>,
    pub session_id: Option<String>,
    pub job: String,
    /// The call's label.
    pub command: String,
    pub waited_secs: u64,
    pub peak_mb: u64,
    /// The free memory it waits for.
    pub need_mb: u64,
    /// The memory free for admission now.
    pub free_mb: u64,
}
