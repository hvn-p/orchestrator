//! What `orchestrator`'s read commands print, as text for a reader, a human
//! or a coordinator: `watch` hands the same text to a coordinator when it
//! wakes one, so that it need not run the commands itself.

use claude_code::messages;
use prefix::admission;

use system::cgroup;

use system::memory;

use learning::peaks;

use system::procfs;

use system::runtime;

use claude_code::sessions;

use anyhow::Result;
use std::fmt::Write as _;
use std::path::Path;

/// Characters of a command line shown by `orchestrator sessions`.
const DISPLAY_CMDLINE: usize = 70;

/// Memory per session with its largest process, and the orphaned processes.
/// With `heads`, processes show as events name them: no argument that could
/// hold a credential, for a reader that hands the output to a model. A
/// session shows by what messages address it by.
pub fn sessions(proc_root: &Path, sessions_dir: &Path, heads: bool) -> Result<String> {
    let available = memory::available_mb(&proc_root.join("meminfo"))?;
    let att = crate::scan(proc_root, sessions_dir)?;
    let shown = |p: &crate::attribution::ProcRef| {
        if heads {
            p.command_head.clone()
        } else {
            procfs::truncate(&p.cmdline, DISPLAY_CMDLINE)
        }
    };
    let mut out = format!("Available memory: {available} MB\n\n");
    let _ = writeln!(out, "{:<34} {:>7}  LARGEST PROCESS", "SESSION", "RSS MB");
    for s in &att.sessions {
        let _ = writeln!(
            out,
            "{:<34} {:>7}  {} MB  pid {}  {}",
            s.name,
            s.rss_kb / 1024,
            s.largest.rss_kb / 1024,
            s.largest.pid,
            shown(&s.largest),
        );
    }
    if att.orphans.is_empty() {
        out.push_str("\nNo orphaned process.\n");
    } else {
        out.push_str("\nORPHANS (session gone)\n");
        for o in &att.orphans {
            let _ = writeln!(
                out,
                "{:>7} MB  {} process(es)  pid {}  session {}  {}",
                o.rss_kb / 1024,
                o.processes,
                o.root.pid,
                o.session_id.chars().take(8).collect::<String>(),
                shown(&o.root),
            );
        }
    }
    Ok(out)
}

/// The session a job group belongs to, by what messages address it by and
/// the start of its id, else by its scope.
fn session_label(group: &str, cgroup_root: &Path, known: &[sessions::ClaudeSession]) -> String {
    let scope = cgroup::session_of(group);
    let session = scope
        .and_then(|s| cgroup::main_pid(cgroup_root, s))
        .and_then(|pid| known.iter().find(|s| s.pid == pid));
    match (session, scope) {
        (Some(s), _) => format!(
            "{} ({})",
            messages::address(s),
            s.session_id.chars().take(8).collect::<String>()
        ),
        (None, Some(scope)) => scope.rsplit('/').next().unwrap_or(scope).to_string(),
        (None, None) => group.to_string(),
    }
}

/// The configuration file and where the coordinator's instructions are.
pub fn config(path: &Path) -> Result<String> {
    let mut out = match config::load(path)? {
        Some(config) => format!(
            "{}\n{}\n",
            path.display(),
            serde_json::to_string_pretty(&config)?
        ),
        None => format!("No configuration at {}.\n", path.display()),
    };
    let instructions = config::instructions_path(path);
    if instructions.exists() {
        let _ = writeln!(
            out,
            "The coordinators' instructions: {}",
            instructions.display()
        );
    }
    Ok(out)
}

/// The admission thresholds, the calls waiting for memory and the
/// reservations of running heavy calls.
pub fn admission(
    paths: &admission::Paths,
    config_path: &Path,
    sessions_dir: &Path,
) -> Result<String> {
    let snap = admission::snapshot(paths)?;
    let mut out = match config::load(config_path) {
        Ok(Some(config::Config {
            admission: Some(a), ..
        })) => format!(
            "Admission: a call expected to peak at {} MB or more waits until free memory covers its peak plus {} MB, {} s at most.\n",
            a.heavy_mb, a.margin_mb, a.max_wait_secs
        ),
        Ok(_) => format!(
            "Admission: off, no admission section in {}.\n",
            config_path.display()
        ),
        Err(e) => format!("Admission: off, {e:#}.\n"),
    };
    let _ = writeln!(
        out,
        "Available memory: {} MB; running heavy calls still hold {} MB of it: {} MB free for admission.\n",
        snap.available_mb,
        snap.held_mb,
        snap.free_mb()
    );
    let known = sessions::read_sessions(sessions_dir).unwrap_or_default();
    let label = |group: &str| session_label(group, &paths.cgroup_root, &known);
    let now = u64::try_from(runtime::now_ms()).unwrap_or(u64::MAX);
    if snap.waiting.is_empty() {
        out.push_str("No call waits for memory.\n");
    } else {
        out.push_str("WAITING FOR MEMORY\n");
        let _ = writeln!(
            out,
            "  {:<40} {:>7} {:>8} {:>9}  COMMAND",
            "SESSION", "WAITED", "PEAK MB", "NEEDS MB"
        );
        for w in &snap.waiting {
            let _ = writeln!(
                out,
                "  {:<40} {:>5} s {:>8} {:>9}  {}",
                label(&w.group),
                now.saturating_sub(w.since_ms) / 1000,
                w.peak_mb,
                w.need_mb,
                w.label
            );
        }
    }
    out.push('\n');
    if snap.reserved.is_empty() {
        out.push_str("No heavy call runs with a reservation.\n");
    } else {
        out.push_str("RESERVED BY RUNNING HEAVY CALLS\n");
        let _ = writeln!(
            out,
            "  {:<40} {:>8} {:>8} {:>8}  COMMAND",
            "SESSION", "HOLDS MB", "PEAK MB", "USES MB"
        );
        for r in &snap.reserved {
            let _ = writeln!(
                out,
                "  {:<40} {:>8} {:>8} {:>8}  {}",
                label(&r.group),
                r.held_mb,
                r.peak_mb,
                r.current_mb.map_or("?".to_string(), |mb| mb.to_string()),
                if r.label.is_empty() { "?" } else { &r.label }
            );
        }
    }
    Ok(out)
}

/// The learned peaks, by repository, heaviest first. A command shows as its
/// label; its id's start tells apart two commands with the same label. With
/// `at_least_mb`, only the commands that heavy; with `most`, only that many
/// of the heaviest.
pub fn peaks(state: &Path, at_least_mb: Option<u64>, most: Option<usize>) -> Result<String> {
    let mut chosen: Vec<(String, peaks::Entry)> = peaks::list(&peaks::dir(state))?
        .into_iter()
        .flat_map(|repo| {
            let name = repo.repository;
            repo.commands.into_iter().map(move |c| (name.clone(), c))
        })
        .filter(|(_, c)| at_least_mb.is_none_or(|mb| c.peak_mb >= mb))
        .collect();
    let by_peak = |a: &(String, peaks::Entry), b: &(String, peaks::Entry)| {
        b.1.peak_mb
            .cmp(&a.1.peak_mb)
            .then_with(|| a.1.id.cmp(&b.1.id))
    };
    chosen.sort_by(by_peak);
    if let Some(most) = most {
        chosen.truncate(most);
    }
    chosen.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| by_peak(a, b)));
    if chosen.is_empty() {
        return Ok(match at_least_mb {
            Some(mb) => format!("No command learned at {mb} MB or more.\n"),
            None => "No peak learned yet.\n".to_string(),
        });
    }
    let mut out = String::new();
    for commands in chosen.chunk_by(|a, b| a.0 == b.0) {
        let width = commands
            .iter()
            .map(|(_, c)| c.label.chars().count().min(peaks::LABEL_MAX))
            .fold("COMMAND".len(), usize::max);
        let _ = writeln!(out, "{}", commands[0].0);
        let _ = writeln!(
            out,
            "  {:>7}  {:<8}  {:<width$}  LATEST CALLS, MB (* ALONE)",
            "PEAK MB", "ID", "COMMAND",
        );
        for (_, c) in commands {
            let calls: Vec<String> = c
                .recent
                .iter()
                .rev()
                .map(|peaks::Call(mb, alone)| format!("{mb}{}", if *alone { "*" } else { "" }))
                .collect();
            let _ = writeln!(
                out,
                "  {:>7}  {:<8}  {:<width$}  {}",
                c.peak_mb,
                c.id.get(..8).unwrap_or(&c.id),
                procfs::truncate(&c.label, peaks::LABEL_MAX),
                calls.join(" "),
            );
        }
        out.push('\n');
    }
    Ok(out)
}
