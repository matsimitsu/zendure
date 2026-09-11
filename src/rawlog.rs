use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

use chrono::{NaiveDate, Utc};
use serde::Serialize;
use serde::de::IgnoredAny;

/// Append-only NDJSON capture of everything entering and leaving the controller.
///
/// Deliberately schema-free: payloads are stored **as received** (raw JSON text),
/// never as re-serialized structs, so a field we don't model today is still on
/// record when we discover we want it. `ZendureProperties` is `Deserialize`-only
/// and the device's API is undocumented, so round-tripping through our own types
/// would quietly discard exactly the fields worth having.
///
/// This is a bridge: the structured journal supersedes it. But these are the
/// source bytes, so a recording made now stays re-derivable into that format —
/// which is the point, because the weeks it takes to build cannot be backfilled.
///
/// Each write is one `write_all` of one line: a single syscall, no fsync. A
/// process crash keeps everything already written (it is in the page cache);
/// only a power cut loses the tail. That is the right trade for a 1Hz control
/// loop, where an fsync stall on SD storage would be felt by the decision path.
///
/// Every failure degrades to "stop logging", never to "stop controlling".
pub struct RawLog {
    dir: PathBuf,
    /// `(UTC date stamp, open handle)` — reopened when the date rolls over.
    /// File names are UTC; the authoritative time is `ts_ms` on each line.
    current: Mutex<Option<(String, File)>>,
    retention_days: i64,
}

impl RawLog {
    /// Returns `None` (with a warning) if the directory can't be used, so the
    /// caller can carry on without a log rather than failing to start.
    pub fn new(dir: PathBuf, retention_days: i64) -> Option<Self> {
        if let Err(e) = fs::create_dir_all(&dir) {
            tracing::warn!("Raw log disabled: cannot create {}: {e}", dir.display());
            return None;
        }
        let log = Self {
            dir,
            current: Mutex::new(None),
            retention_days,
        };
        log.prune();
        Some(log)
    }

    /// Record a payload that is already JSON text, embedding it verbatim.
    /// Anything that doesn't parse is stored as a JSON string instead, so a
    /// malformed device response is still captured and the file stays valid
    /// NDJSON.
    pub fn raw(&self, kind: &str, payload: &str) {
        // Validates the syntax without building a `Value` or allocating.
        match serde_json::from_str::<IgnoredAny>(payload) {
            Ok(_) => self.write_line(kind, payload),
            Err(_) => match serde_json::to_string(payload) {
                Ok(quoted) => self.write_line(kind, &quoted),
                Err(e) => tracing::debug!("Raw log: cannot encode {kind}: {e}"),
            },
        }
    }

    /// Record one of our own types (a decision, a command, an outcome).
    pub fn value<T: Serialize>(&self, kind: &str, value: &T) {
        match serde_json::to_string(value) {
            Ok(json) => self.write_line(kind, &json),
            Err(e) => tracing::debug!("Raw log: cannot serialize {kind}: {e}"),
        }
    }

    fn write_line(&self, kind: &str, payload_json: &str) {
        let now = Utc::now();
        let line = format!(
            "{{\"ts_ms\":{},\"kind\":\"{}\",\"payload\":{}}}\n",
            now.timestamp_millis(),
            kind,
            payload_json,
        );

        let stamp = now.format("%Y-%m-%d").to_string();
        let Ok(mut slot) = self.current.lock() else {
            return; // poisoned by a panic in another writer; drop rather than spread it
        };

        let rolled = !matches!(&*slot, Some((open, _)) if *open == stamp);
        if rolled {
            let path = self.dir.join(format!("{stamp}.ndjson"));
            match OpenOptions::new().create(true).append(true).open(&path) {
                Ok(file) => *slot = Some((stamp, file)),
                Err(e) => {
                    tracing::warn!("Raw log: cannot open {}: {e}", path.display());
                    *slot = None;
                    return;
                }
            }
        }

        if let Some((_, file)) = slot.as_mut()
            && let Err(e) = file.write_all(line.as_bytes())
        {
            tracing::warn!("Raw log: write failed: {e}");
            *slot = None; // reopen on the next line
        }

        drop(slot);
        if rolled {
            self.prune();
        }
    }

    /// Delete captures older than the retention window. Called at startup and
    /// whenever the file rolls over, so it costs one directory scan per day.
    fn prune(&self) {
        let Ok(entries) = fs::read_dir(&self.dir) else {
            return;
        };
        let cutoff = Utc::now().date_naive() - chrono::Duration::days(self.retention_days);

        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("ndjson") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let Ok(date) = NaiveDate::parse_from_str(stem, "%Y-%m-%d") else {
                continue;
            };
            if date < cutoff {
                match fs::remove_file(&path) {
                    Ok(()) => tracing::info!("Raw log: pruned {}", path.display()),
                    Err(e) => tracing::warn!("Raw log: cannot prune {}: {e}", path.display()),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every captured line, oldest first, across whatever files exist.
    fn lines(dir: &std::path::Path) -> Vec<serde_json::Value> {
        let mut files: Vec<_> = fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("ndjson"))
            .collect();
        files.sort();
        files
            .iter()
            .flat_map(|p| {
                fs::read_to_string(p)
                    .unwrap()
                    .lines()
                    .map(|l| serde_json::from_str(l).expect("each line must be valid JSON"))
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    #[test]
    fn valid_json_is_embedded_verbatim() {
        let dir = tempfile::tempdir().unwrap();
        let log = RawLog::new(dir.path().to_path_buf(), 90).unwrap();

        log.raw("shelly", r#"{"total_act_power":150.5,"unmodelled":"kept"}"#);

        let lines = lines(dir.path());
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0]["kind"], "shelly");
        assert_eq!(lines[0]["payload"]["total_act_power"], 150.5);
        // The whole point: a field none of our types know about survives.
        assert_eq!(lines[0]["payload"]["unmodelled"], "kept");
        assert!(lines[0]["ts_ms"].as_i64().unwrap() > 0);
    }

    #[test]
    fn malformed_payload_is_kept_as_a_string() {
        let dir = tempfile::tempdir().unwrap();
        let log = RawLog::new(dir.path().to_path_buf(), 90).unwrap();

        // A truncated device response is exactly the one worth having on record,
        // and it must not corrupt the file for every line after it.
        log.raw("zendure_poll", r#"{"electricLevel": 4"#);
        log.raw("shelly", r#"{"ok":true}"#);

        let lines = lines(dir.path());
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["payload"], r#"{"electricLevel": 4"#);
        assert_eq!(lines[1]["payload"]["ok"], true);
    }

    #[test]
    fn payload_containing_a_newline_stays_on_one_line() {
        let dir = tempfile::tempdir().unwrap();
        let log = RawLog::new(dir.path().to_path_buf(), 90).unwrap();

        log.raw("zendure_poll", "not json\nwith a newline");
        log.raw("shelly", r#"{"after":1}"#);

        // NDJSON is line-delimited, so an embedded newline would split one
        // record into two and corrupt the file.
        let lines = lines(dir.path());
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["payload"], "not json\nwith a newline");
        assert_eq!(lines[1]["payload"]["after"], 1);
    }

    #[test]
    fn serializable_values_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let log = RawLog::new(dir.path().to_path_buf(), 90).unwrap();

        log.value(
            "decision",
            &serde_json::json!({ "outcome": "ok", "command": "set_discharge(145W)" }),
        );

        let lines = lines(dir.path());
        assert_eq!(lines[0]["kind"], "decision");
        assert_eq!(lines[0]["payload"]["command"], "set_discharge(145W)");
    }

    #[test]
    fn unusable_directory_disables_the_log_instead_of_failing() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("not-a-dir");
        fs::write(&blocker, b"i am a file").unwrap();

        // A misconfigured path must never stop the controller from starting.
        assert!(RawLog::new(blocker, 90).is_none());
    }

    #[test]
    fn startup_prunes_beyond_retention() {
        let dir = tempfile::tempdir().unwrap();
        let today = Utc::now().date_naive();
        let old = (today - chrono::Duration::days(40)).format("%Y-%m-%d");
        let recent = (today - chrono::Duration::days(3)).format("%Y-%m-%d");

        fs::write(dir.path().join(format!("{old}.ndjson")), b"{}\n").unwrap();
        fs::write(dir.path().join(format!("{recent}.ndjson")), b"{}\n").unwrap();
        fs::write(dir.path().join("notes.txt"), b"leave me").unwrap();

        let _log = RawLog::new(dir.path().to_path_buf(), 30).unwrap();

        assert!(!dir.path().join(format!("{old}.ndjson")).exists());
        assert!(dir.path().join(format!("{recent}.ndjson")).exists());
        // Anything that isn't ours is left alone.
        assert!(dir.path().join("notes.txt").exists());
    }
}
