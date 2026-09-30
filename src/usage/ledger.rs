use super::attribute::{capture_signals, load_config, load_labels};
use super::parse::{CostState, Request, parse};
use crate::atomic;
use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

#[derive(Clone)]
pub struct Source {
    pub profile: String,
    pub directory: PathBuf,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Title {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time: Option<DateTime<Utc>>,
    #[serde(default)]
    pub file: String,
    #[serde(default)]
    pub offset: u64,
    pub value: String,
    pub source: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Session {
    pub profile: String,
    pub id: String,
    pub first: Option<DateTime<Utc>>,
    pub last: Option<DateTime<Utc>>,
    pub titles: Vec<Title>,
    pub cost_state: Option<CostState>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Cursor {
    pub device: u64,
    pub inode: u64,
    pub size: u64,
    pub offset: u64,
    pub partial: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Cursors {
    pub files: BTreeMap<String, Cursor>,
    pub duplicates: u64,
    pub malformed: u64,
    #[serde(default)]
    pub synthetic_skipped: u64,
    pub sidechain_replays: Vec<String>,
}

#[derive(Default)]
pub struct Ledger {
    pub requests: Vec<Request>,
    pub sessions: Vec<Session>,
    pub cursors: Cursors,
}

#[derive(Debug, Default)]
pub struct IngestReport {
    pub skipped_lock: bool,
    pub bytes_read: u64,
    pub new_requests: usize,
    pub duplicates: u64,
    pub malformed: u64,
    pub synthetic_skipped: u64,
    pub partial: u64,
}

fn json_or_default<T: DeserializeOwned + Default>(path: &Path) -> Result<T> {
    if !path.exists() {
        return Ok(T::default());
    }
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}

fn jsonl<T: DeserializeOwned>(path: &Path) -> Result<Vec<T>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let bytes = fs::read(path)?;
    bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).map_err(Into::into))
        .collect()
}

fn save_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    atomic::write(path, &serde_json::to_vec_pretty(value)?)
}

fn save_jsonl<T: Serialize>(path: &Path, values: &[T]) -> Result<()> {
    let mut bytes = Vec::new();
    for value in values {
        serde_json::to_writer(&mut bytes, value)?;
        bytes.push(b'\n');
    }
    atomic::write(path, &bytes)
}

fn transcript_files(root: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    if !root.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        if metadata.is_dir() {
            transcript_files(&path, out)?;
        } else if metadata.is_file() && path.extension().is_some_and(|ext| ext == "jsonl") {
            out.push(path);
        }
    }
    Ok(())
}

#[cfg(unix)]
fn device_inode(metadata: &fs::Metadata) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    (metadata.dev(), metadata.ino())
}

#[cfg(not(unix))]
fn device_inode(_metadata: &fs::Metadata) -> (u64, u64) {
    (0, 0)
}

pub struct Store {
    pub dir: PathBuf,
    pub sources: Vec<Source>,
}

impl Store {
    pub fn new(dir: PathBuf, sources: Vec<Source>) -> Self {
        Self { dir, sources }
    }

    pub fn load(&self) -> Result<Ledger> {
        let requests_dir = self.dir.join("requests");
        let mut requests = Vec::new();
        if requests_dir.exists() {
            let mut paths = fs::read_dir(requests_dir)?
                .filter_map(|entry| entry.ok().map(|entry| entry.path()))
                .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
                .collect::<Vec<_>>();
            paths.sort();
            for path in paths {
                requests.extend(jsonl(&path)?);
            }
        }
        Ok(Ledger {
            requests,
            sessions: jsonl(&self.dir.join("sessions.jsonl"))?,
            cursors: json_or_default(&self.dir.join("cursors.json"))?,
        })
    }

    fn save_data(&self, ledger: &Ledger) -> Result<()> {
        let mut months = BTreeMap::<String, Vec<&Request>>::new();
        for request in &ledger.requests {
            months
                .entry(request.time.format("%Y-%m").to_string())
                .or_default()
                .push(request);
        }
        let requests_dir = self.dir.join("requests");
        if requests_dir.exists() {
            for entry in fs::read_dir(&requests_dir)? {
                let path = entry?.path();
                if path.extension().is_some_and(|ext| ext == "jsonl")
                    && let Some(month) = path.file_stem().and_then(|stem| stem.to_str())
                    && !months.contains_key(month)
                {
                    atomic::write(&path, b"")?;
                }
            }
        }
        for (month, rows) in months {
            save_jsonl(
                &self.dir.join("requests").join(format!("{month}.jsonl")),
                &rows,
            )?;
        }
        save_jsonl(&self.dir.join("sessions.jsonl"), &ledger.sessions)?;
        Ok(())
    }

    pub(crate) fn try_lock(&self) -> Result<Option<File>> {
        fs::create_dir_all(&self.dir)?;
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(self.dir.join(".lock"))?;
        match file.try_lock() {
            Ok(()) => Ok(Some(file)),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(error) => Err(anyhow::anyhow!(error)),
        }
    }

    pub fn ingest(&self) -> Result<IngestReport> {
        let Some(_lock) = self.try_lock()? else {
            return Ok(IngestReport {
                skipped_lock: true,
                ..IngestReport::default()
            });
        };
        let mut ledger = self.load()?;
        let config = load_config(&self.dir)?;
        let mut report = IngestReport::default();
        let mut keys = ledger
            .requests
            .iter()
            .enumerate()
            .map(|(index, request)| (request.key.clone(), index))
            .collect::<HashMap<_, _>>();
        let mut sidechain_ids = ledger
            .requests
            .iter()
            .enumerate()
            .filter_map(|(index, request)| {
                request.message_id.as_ref().map(|id| {
                    (
                        (request.profile.clone(), request.session.clone(), id.clone()),
                        index,
                    )
                })
            })
            .collect::<HashMap<_, _>>();
        let mut sessions = ledger
            .sessions
            .iter()
            .enumerate()
            .map(|(index, session)| ((session.profile.clone(), session.id.clone()), index))
            .collect::<HashMap<_, _>>();
        let mut sources = self.sources.clone();
        sources.sort_by(|a, b| a.profile.cmp(&b.profile));
        for source in sources {
            let mut files = Vec::new();
            transcript_files(&source.directory.join("projects"), &mut files)?;
            files.sort();
            for path in files {
                let mut file = match File::open(&path) {
                    Ok(file) => file,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(error) => return Err(error.into()),
                };
                let metadata = file.metadata()?;
                let (device, inode) = device_inode(&metadata);
                let path_key = path.to_string_lossy().into_owned();
                let previous = ledger.cursors.files.get(&path_key).cloned();
                if previous.as_ref().is_some_and(|cursor| {
                    cursor.device == device
                        && cursor.inode == inode
                        && cursor.size == metadata.len()
                        && cursor.offset <= metadata.len()
                }) {
                    continue;
                }
                let offset = previous
                    .as_ref()
                    .filter(|cursor| {
                        cursor.device == device
                            && cursor.inode == inode
                            && metadata.len() >= cursor.size
                    })
                    .map_or(0, |cursor| cursor.offset);
                file.seek(SeekFrom::Start(offset))?;
                let mut bytes = Vec::new();
                file.read_to_end(&mut bytes)?;
                report.bytes_read += bytes.len() as u64;
                let complete_len = bytes
                    .iter()
                    .rposition(|byte| *byte == b'\n')
                    .map_or(0, |i| i + 1);
                let partial = complete_len < bytes.len();
                let mut cursor = Cursor {
                    device,
                    inode,
                    size: metadata.len(),
                    offset: offset + complete_len as u64,
                    partial,
                };
                let mut line_offset = offset;
                for line in bytes[..complete_len].split(|byte| *byte == b'\n') {
                    let current_offset = line_offset;
                    line_offset += line.len() as u64 + 1;
                    if line.is_empty() {
                        continue;
                    }
                    // Avoid JSON parsing ordinary transcript lines while retaining
                    // explicit title and cost metadata alongside usage rows.
                    if !line.windows(7).any(|part| part == b"\"usage\"")
                        && !line.windows(12).any(|part| part == b"custom-title")
                        && !line.windows(8).any(|part| part == b"ai-title")
                        && !line.windows(10).any(|part| part == b"cost-state")
                        && !line.windows(8).any(|part| part == b"/rename ")
                    {
                        continue;
                    }
                    let parsed = match parse(line, &source.profile) {
                        Ok(Some(parsed)) => parsed,
                        Ok(None) => continue,
                        Err(()) => {
                            report.malformed += 1;
                            continue;
                        }
                    };
                    report.synthetic_skipped += u64::from(parsed.synthetic_skipped);
                    let session_key = (source.profile.clone(), parsed.session.clone());
                    let index = *sessions.entry(session_key).or_insert_with(|| {
                        ledger.sessions.push(Session {
                            profile: source.profile.clone(),
                            id: parsed.session.clone(),
                            first: None,
                            last: None,
                            titles: Vec::new(),
                            cost_state: None,
                        });
                        ledger.sessions.len() - 1
                    });
                    let session = &mut ledger.sessions[index];
                    if let Some(time) = parsed.time {
                        session.first = Some(session.first.map_or(time, |first| first.min(time)));
                        session.last = Some(session.last.map_or(time, |last| last.max(time)));
                    }
                    if let Some((value, source)) = parsed.title {
                        let title = Title {
                            time: parsed.time,
                            file: path_key.clone(),
                            offset: current_offset,
                            value,
                            source,
                        };
                        if !session.titles.contains(&title) {
                            session.titles.push(title);
                        }
                    }
                    if let Some(mut cost) = parsed.cost {
                        cost.file = path_key.clone();
                        cost.offset = current_offset;
                        if session.cost_state.as_ref().is_none_or(|old| {
                            (cost.file.as_str(), cost.offset) >= (old.file.as_str(), old.offset)
                        }) {
                            session.cost_state = Some(cost);
                        }
                    }
                    for mut request in parsed.requests {
                        capture_signals(&mut request, &config);
                        let replay = request
                            .message_id
                            .as_ref()
                            .and_then(|id| {
                                sidechain_ids.get(&(
                                    request.profile.clone(),
                                    request.session.clone(),
                                    id.clone(),
                                ))
                            })
                            .copied()
                            .filter(|&index| ledger.requests[index].sidechain || request.sidechain);
                        let same_key = keys.get(&request.key).copied();
                        if let Some(existing) = same_key.or(replay) {
                            report.duplicates += 1;
                            let old = &ledger.requests[existing];
                            if old.sidechain || request.sidechain {
                                if same_key.is_none()
                                    && !ledger.cursors.sidechain_replays.contains(&request.key)
                                {
                                    ledger.cursors.sidechain_replays.push(request.key.clone());
                                }
                                if old.sidechain && !request.sidechain {
                                    keys.remove(&old.key);
                                    keys.insert(request.key.clone(), existing);
                                    ledger.requests[existing] = request;
                                } else if old.sidechain == request.sidechain
                                    && request.output > old.output
                                {
                                    ledger.requests[existing] = request;
                                }
                            } else if request.output > old.output {
                                // Streamed copies carry cumulative output. Keep the
                                // largest count, including across separate ingest runs.
                                ledger.requests[existing] = request;
                            }
                        } else {
                            if let Some(id) = &request.message_id {
                                sidechain_ids.insert(
                                    (request.profile.clone(), request.session.clone(), id.clone()),
                                    ledger.requests.len(),
                                );
                            }
                            keys.insert(request.key.clone(), ledger.requests.len());
                            ledger.requests.push(request);
                            report.new_requests += 1;
                        }
                    }
                }
                // A concurrent writer may append while we read. Keep the exact
                // observed byte count so the next pass checks for new data.
                cursor.size = offset + bytes.len() as u64;
                ledger.cursors.files.insert(path_key, cursor);
            }
        }
        report.partial = ledger
            .cursors
            .files
            .values()
            .filter(|cursor| cursor.partial)
            .count() as u64;
        ledger.cursors.duplicates += report.duplicates;
        ledger.cursors.malformed += report.malformed;
        ledger.cursors.synthetic_skipped += report.synthetic_skipped;
        self.save_data(&ledger)?;
        super::report::refresh_summary(self, &ledger)?;
        // Cursor is last: a failed data or summary write leaves a harmless
        // replay for deduplication on the next ingest.
        save_json(&self.dir.join("cursors.json"), &ledger.cursors)?;
        Ok(report)
    }

    pub fn purge_profile(&self, profile: &str) -> Result<usize> {
        let Some(_lock) = self.try_lock()? else {
            anyhow::bail!("usage ledger is busy");
        };
        let mut ledger = self.load()?;
        let removed_sessions = ledger
            .sessions
            .iter()
            .filter(|session| session.profile == profile)
            .map(|session| session.id.clone())
            .collect::<std::collections::HashSet<_>>();
        let before = ledger.requests.len();
        ledger.requests.retain(|row| row.profile != profile);
        ledger.sessions.retain(|row| row.profile != profile);
        ledger.cursors.files.retain(|path, _| {
            !self.sources.iter().any(|source| {
                source.profile == profile && Path::new(path).starts_with(&source.directory)
            })
        });
        self.save_data(&ledger)?;
        let mut labels = load_labels(&self.dir)?;
        labels.sessions.retain(|session, _| {
            !removed_sessions.contains(session)
                || ledger
                    .sessions
                    .iter()
                    .any(|remaining| &remaining.id == session)
        });
        if self.dir.join("labels.json").exists() {
            save_json(&self.dir.join("labels.json"), &labels)?;
        }
        super::report::refresh_summary(self, &ledger)?;
        save_json(&self.dir.join("cursors.json"), &ledger.cursors)?;
        Ok(before - ledger.requests.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Write;

    fn setup() -> (tempfile::TempDir, Store, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let profile = tmp.path().join("profile");
        let projects = profile.join("projects/demo");
        fs::create_dir_all(&projects).unwrap();
        let store = Store::new(
            tmp.path().join("ledger"),
            vec![Source {
                profile: "sample".into(),
                directory: profile,
            }],
        );
        (tmp, store, projects)
    }

    fn row(id: &str, request: &str, session: &str) -> String {
        json!({
            "type":"assistant", "timestamp":"2030-01-01T12:00:00Z", "sessionId":session,
            "requestId":request, "message":{"id":id,"model":"claude-sonnet-5",
                "usage":{"input_tokens":10,"output_tokens":20,"cache_creation_input_tokens":30,
                    "cache_creation":{"ephemeral_5m_input_tokens":10,"ephemeral_1h_input_tokens":20},
                    "cache_read_input_tokens":40}}
        }).to_string() + "\n"
    }

    fn fixture(store: &Store, projects: &Path) {
        let source = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/usage/profile/projects/demo");
        fs::copy(source.join("a.jsonl"), projects.join("a.jsonl")).unwrap();
        fs::copy(source.join("b.jsonl"), projects.join("b.jsonl")).unwrap();
        assert_eq!(store.sources.len(), 1);
    }

    #[test]
    fn parity_cross_file_dedup_and_sidechain_parent_wins() {
        // Known-bad: no dedup inflates requests; file-local dedup misses resumed copies;
        // retaining the sidechain replay loses the parent's token counts.
        let (_tmp, store, projects) = setup();
        fixture(&store, &projects);
        let report = store.ingest().unwrap();
        let ledger = store.load().unwrap();
        assert_eq!(ledger.requests.len(), 3);
        assert_eq!(ledger.sessions.len(), 2);
        assert_eq!(report.duplicates, 3);
        assert_eq!(report.malformed, 1);
        assert_eq!(report.synthetic_skipped, 1);
        assert_eq!(ledger.cursors.sidechain_replays.len(), 1);
        assert_eq!(
            5 - ledger.requests.len(),
            report.synthetic_skipped as usize + ledger.cursors.sidechain_replays.len()
        );
        assert_eq!(
            ledger.cursors.sidechain_replays,
            vec![serde_json::to_string(&("request", "msg-2", "req-parent")).unwrap()]
        );
        assert_eq!(
            ledger
                .requests
                .iter()
                .find(|row| row.message_id.as_deref() == Some("msg-2"))
                .unwrap()
                .input,
            3
        );
        assert_eq!(
            ledger
                .requests
                .iter()
                .find(|row| row.message_id.as_deref() == Some("msg-3"))
                .unwrap()
                .model,
            "claude-haiku-4-5-20251001"
        );
    }

    #[test]
    fn streamed_copies_keep_largest_output_across_ingest_runs() {
        // Known-bad: keeping the first streamed copy leaves output at 8 even
        // after the final 240-token copy arrives in a later ingest run.
        let (_tmp, store, projects) = setup();
        let path = projects.join("stream.jsonl");
        let copy = |time: &str, output: u64| {
            json!({"type":"assistant","timestamp":time,"sessionId":"sample",
                "requestId":"req-1","message":{"id":"msg-1","model":"claude-sonnet-5",
                    "usage":{"input_tokens":10,"output_tokens":output}}})
            .to_string()
                + "\n"
        };
        fs::write(
            &path,
            format!(
                "{}{}",
                copy("2030-01-01T12:00:00Z", 8),
                copy("2030-01-01T12:00:01Z", 8)
            ),
        )
        .unwrap();
        assert_eq!(store.ingest().unwrap().duplicates, 1);
        assert_eq!(store.load().unwrap().requests[0].output, 8);
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(copy("2030-01-01T12:00:02Z", 240).as_bytes())
            .unwrap();
        assert_eq!(store.ingest().unwrap().duplicates, 1);
        let ledger = store.load().unwrap();
        assert_eq!(ledger.requests.len(), 1);
        assert_eq!(ledger.requests[0].output, 240);
        assert_eq!(ledger.cursors.duplicates, 2);
    }

    #[test]
    fn metadata_only_session_uses_last_title_in_file_order() {
        // Known-bad: requiring timestamps on title metadata discards these
        // rows; sorting by invented times cannot identify the last title.
        let (_tmp, store, projects) = setup();
        let path = projects.join("titles.jsonl");
        let titles = [
            json!({"type":"ai-title","sessionId":"sample","aiTitle":"first idea"}),
            json!({"type":"ai-title","sessionId":"sample","aiTitle":"current idea"}),
            json!({"type":"custom-title","sessionId":"sample","customTitle":"first name"}),
        ];
        fs::write(
            &path,
            titles
                .iter()
                .map(|row| format!("{row}\n"))
                .collect::<String>(),
        )
        .unwrap();
        assert_eq!(store.ingest().unwrap().malformed, 0);
        OpenOptions::new().append(true).open(&path).unwrap()
            .write_all(b"{\"type\":\"custom-title\",\"sessionId\":\"sample\",\"customTitle\":\"current name\"}\n")
            .unwrap();
        assert_eq!(store.ingest().unwrap().malformed, 0);
        let ledger = store.load().unwrap();
        let session = &ledger.sessions[0];
        assert_eq!((session.first, session.last), (None, None));
        assert_eq!(session.titles.len(), 4);
        assert_eq!(
            session
                .titles
                .iter()
                .filter(|title| title.source == "rename")
                .max_by_key(|title| (&title.file, title.offset))
                .unwrap()
                .value,
            "current name"
        );
    }

    #[test]
    fn incremental_partial_and_shrunk_file() {
        // Known-bad: advancing past a partial last line loses it; retaining an old
        // offset after shrink misses replacement content; rereading all bytes inflates counts.
        let (_tmp, store, projects) = setup();
        let path = projects.join("live.jsonl");
        let first = row("m1", "r1", "s1");
        let second = row("m2", "r2", "s1");
        fs::write(&path, format!("{first}{}", &second[..second.len() - 2])).unwrap();
        let first_run = store.ingest().unwrap();
        assert_eq!(store.load().unwrap().requests.len(), 1);
        assert_eq!(first_run.partial, 1);
        assert_eq!(store.ingest().unwrap().bytes_read, 0);
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"}\n")
            .unwrap();
        let second_run = store.ingest().unwrap();
        assert_eq!(second_run.new_requests, 1);
        assert_eq!(store.load().unwrap().requests.len(), 2);
        fs::write(&path, &first).unwrap();
        let shrunk = store.ingest().unwrap();
        assert_eq!(shrunk.duplicates, 1);
        assert_eq!(store.load().unwrap().requests.len(), 2);
        let replacement = projects.join("replacement.tmp");
        fs::write(&replacement, row("m3", "r3", "s1")).unwrap();
        fs::rename(replacement, &path).unwrap();
        store.ingest().unwrap();
        assert_eq!(store.load().unwrap().requests.len(), 3);
    }

    #[test]
    fn transcript_deletion_does_not_delete_ledger_rows() {
        // Known-bad: rebuilding the ledger from only currently present transcripts.
        let (_tmp, store, projects) = setup();
        let path = projects.join("history.jsonl");
        fs::write(&path, row("m1", "r1", "s1")).unwrap();
        store.ingest().unwrap();
        fs::remove_file(path).unwrap();
        store.ingest().unwrap();
        assert_eq!(store.load().unwrap().requests.len(), 1);
    }

    #[test]
    fn ingest_is_read_only_toward_source_and_purge_removes_only_selected_profile() {
        // Known-bad: rewriting a transcript during ingest, or deleting ledger
        // rows as a side effect of ordinary source disappearance.
        let (_tmp, store, projects) = setup();
        let path = projects.join("source.jsonl");
        fs::write(&path, row("m1", "r1", "s1")).unwrap();
        let before_bytes = fs::read(&path).unwrap();
        let before_mtime = fs::metadata(&path).unwrap().modified().unwrap();
        store.ingest().unwrap();
        assert_eq!(fs::read(&path).unwrap(), before_bytes);
        assert_eq!(
            fs::metadata(&path).unwrap().modified().unwrap(),
            before_mtime
        );
        assert_eq!(store.purge_profile("sample").unwrap(), 1);
        assert!(store.load().unwrap().requests.is_empty());
    }

    #[test]
    fn held_lock_skips_ingest_without_writing_rows() {
        // Known-bad: proceeding when try_lock reports contention writes
        // ledger rows while another ingest owns the lock.
        let (_tmp, store, projects) = setup();
        fs::write(projects.join("one.jsonl"), row("m1", "r1", "s1")).unwrap();
        fs::create_dir_all(&store.dir).unwrap();
        let lock = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(store.dir.join(".lock"))
            .unwrap();
        lock.try_lock().unwrap();
        assert!(store.ingest().unwrap().skipped_lock);
        assert!(store.load().unwrap().requests.is_empty());
        drop(lock);
        assert_eq!(store.ingest().unwrap().new_requests, 1);
        assert_eq!(store.load().unwrap().requests.len(), 1);
    }

    #[test]
    fn prompt_and_tool_input_canary_never_enters_ledger() {
        // Known-bad: serializing message content or raw file_path, path, and
        // notebook_path tool values leaks prompts and tool-input paths.
        let (_tmp, store, projects) = setup();
        let canary = "CANARY_PRIVATE_PROMPT_0000";
        let mut value: serde_json::Value =
            serde_json::from_str(row("m1", "r1", "s1").trim()).unwrap();
        value["message"]["content"] = json!([
            {"type":"text", "text":canary},
            {"type":"tool_use", "input":{"command":canary,
                "file_path":format!("/synthetic/atlas/{canary}.rs"),
                "path":format!("/synthetic/atlas/{canary}.txt"),
                "notebook_path":format!("/synthetic/atlas/{canary}.ipynb")}}
        ]);
        fs::write(projects.join("one.jsonl"), value.to_string() + "\n").unwrap();
        store.ingest().unwrap();
        for entry in fs::read_dir(&store.dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_file() {
                assert!(
                    !fs::read_to_string(path)
                        .unwrap_or_default()
                        .contains(canary)
                );
            } else if path.is_dir() {
                for file in fs::read_dir(path).unwrap() {
                    assert!(
                        !fs::read_to_string(file.unwrap().path())
                            .unwrap()
                            .contains(canary)
                    );
                }
            }
        }
        let stored = store.load().unwrap();
        assert!(stored.requests[0].touched.is_empty());
        assert_eq!(stored.requests[0].touch_count, 3);
    }
}
