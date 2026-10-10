//! Per-session usage rollup persisted after ledger ingest.

use super::attribute::{Config, Labels, Resolver};
use super::ledger::{Ledger, Session};
use crate::atomic;
use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::Path;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Chats {
    pub version: u32,
    pub generated_at: DateTime<Utc>,
    pub rows: Vec<ChatRow>,
    pub projects: Vec<ChatProject>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChatRow {
    pub profile: String,
    pub session: String,
    pub model: String,
    pub speed: Option<String>,
    pub requests: u64,
    pub input: u64,
    pub output: u64,
    pub cache_write_5m: u64,
    pub cache_write_1h: u64,
    pub cache_read: u64,
}

impl ChatRow {
    pub fn tokens(&self) -> [u64; 5] {
        [
            self.input,
            self.output,
            self.cache_write_5m,
            self.cache_write_1h,
            self.cache_read,
        ]
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChatProject {
    pub profile: String,
    pub session: String,
    pub project: String,
    pub signal: String,
}

pub fn read(dir: &Path) -> Option<Chats> {
    let value: Chats = serde_json::from_slice(&fs::read(dir.join("chats.json")).ok()?).ok()?;
    (value.version == 1).then_some(value)
}

pub fn write(
    dir: &Path,
    ledger: &Ledger,
    config: &Config,
    labels: &Labels,
    now: DateTime<Utc>,
) -> Result<()> {
    let mut grouped = BTreeMap::<(String, String, String, Option<String>), ChatRow>::new();
    let mut sessions = BTreeSet::<(String, String)>::new();
    for request in &ledger.requests {
        sessions.insert((request.profile.clone(), request.session.clone()));
        let key = (
            request.profile.clone(),
            request.session.clone(),
            request.model.clone(),
            request.speed.clone(),
        );
        let row = grouped.entry(key).or_insert_with(|| ChatRow {
            profile: request.profile.clone(),
            session: request.session.clone(),
            model: request.model.clone(),
            speed: request.speed.clone(),
            requests: 0,
            input: 0,
            output: 0,
            cache_write_5m: 0,
            cache_write_1h: 0,
            cache_read: 0,
        });
        row.requests += 1;
        row.input += request.input;
        row.output += request.output;
        row.cache_write_5m += request.cache_write_5m;
        row.cache_write_1h += request.cache_write_1h;
        row.cache_read += request.cache_read;
    }
    let resolver = Resolver::new(&ledger.requests, config, labels);
    let metadata = ledger
        .sessions
        .iter()
        .map(|session| ((session.profile.as_str(), session.id.as_str()), session))
        .collect::<HashMap<(&str, &str), &Session>>();
    let projects = sessions
        .into_iter()
        .filter_map(|(profile, session)| {
            let (project, signal) = resolver.session_project(
                &profile,
                &session,
                metadata.get(&(profile.as_str(), session.as_str())).copied(),
            )?;
            Some(ChatProject {
                profile,
                session,
                project,
                signal: signal.to_string(),
            })
        })
        .collect();
    atomic::write(
        &dir.join("chats.json"),
        &serde_json::to_vec_pretty(&Chats {
            version: 1,
            generated_at: now,
            rows: grouped.into_values().collect(),
            projects,
        })?,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::attribute::{WorkspaceRule, capture_signals};
    use crate::usage::parse;
    use serde_json::json;

    #[test]
    fn rollup_persists_labelled_session_project() {
        // Known-bad: rows persist but the label-derived project is absent.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("atlas");
        let site = root.join("apps/site");
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::create_dir_all(site.join(".git")).unwrap();
        let config = Config {
            superproject: Some(root),
            workspaces: vec![WorkspaceRule {
                glob: "apps/*".into(),
                name: Some("acme".into()),
                segment: None,
            }],
            ..Config::default()
        };
        let record = json!({"type":"assistant","timestamp":"2030-01-01T12:00:00Z",
            "sessionId":"s","requestId":"req-s","cwd":site,
            "message":{"id":"msg-s","model":"claude-opus-5",
                "usage":{"input_tokens":10,"output_tokens":20}}});
        let mut request = parse::parse(record.to_string().as_bytes(), "work")
            .unwrap()
            .unwrap()
            .requests
            .remove(0);
        capture_signals(&mut request, &config);
        let ledger = Ledger {
            requests: vec![request],
            ..Ledger::default()
        };
        let labels = Labels {
            sessions: BTreeMap::from([("s".into(), "site".into())]),
        };
        let usage = tmp.path().join("usage");
        fs::create_dir_all(&usage).unwrap();
        let now = DateTime::parse_from_rfc3339("2030-01-01T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        write(&usage, &ledger, &config, &labels, now).unwrap();
        let saved = read(&usage).unwrap();
        assert_eq!(saved.version, 1);
        assert_eq!(saved.generated_at, now);
        assert_eq!(saved.rows.len(), 1);
        assert_eq!(saved.projects.len(), 1);
        assert_eq!(saved.projects[0].profile, "work");
        assert_eq!(saved.projects[0].session, "s");
        assert_eq!(saved.projects[0].project, "acme/site");
        assert_eq!(saved.projects[0].signal, "label");
    }

    #[test]
    fn rollup_separates_model_and_speed_within_one_session() {
        // Known-bads: grouping by session without model, or by model without speed.
        let tmp = tempfile::tempdir().unwrap();
        let requests = [
            ("model-a", None, 10),
            ("model-a", Some("fast"), 20),
            ("model-b", None, 30),
        ]
        .into_iter()
        .enumerate()
        .map(|(index, (model, speed, input))| {
            let record = json!({
                "type":"assistant", "timestamp":"2030-01-01T12:00:00Z",
                "sessionId":"s", "requestId":format!("request-{index}"),
                "message":{"id":format!("message-{index}"),"model":model,
                    "usage":{"input_tokens":input,"output_tokens":input * 2,"speed":speed}}
            });
            parse::parse(record.to_string().as_bytes(), "work")
                .unwrap()
                .unwrap()
                .requests
                .remove(0)
        })
        .collect::<Vec<_>>();
        let ledger = Ledger {
            requests,
            ..Ledger::default()
        };
        let at = DateTime::parse_from_rfc3339("2030-01-01T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        write(
            tmp.path(),
            &ledger,
            &Config::default(),
            &Labels::default(),
            at,
        )
        .unwrap();
        let saved = read(tmp.path()).unwrap();
        assert_eq!(saved.rows.len(), 3);
        for (model, speed, input) in [
            ("model-a", None, 10),
            ("model-a", Some("fast"), 20),
            ("model-b", None, 30),
        ] {
            let row = saved
                .rows
                .iter()
                .find(|row| row.model == model && row.speed.as_deref() == speed)
                .unwrap();
            assert_eq!(row.profile, "work");
            assert_eq!(row.session, "s");
            assert_eq!(row.requests, 1);
            assert_eq!(row.input, input);
            assert_eq!(row.output, input * 2);
        }
    }
}
