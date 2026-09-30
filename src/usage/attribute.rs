use super::ledger::Session;
use super::parse::Request;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub superproject: Option<PathBuf>,
    #[serde(default)]
    pub project_globs: Vec<String>,
    #[serde(default)]
    pub workspaces: Vec<WorkspaceRule>,
    #[serde(default)]
    pub aliases: BTreeMap<String, String>,
    #[serde(default)]
    pub ignore_paths: Vec<PathBuf>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkspaceRule {
    pub glob: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub segment: Option<usize>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Labels {
    #[serde(default)]
    pub sessions: BTreeMap<String, String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attribution {
    pub workspace: String,
    pub project: String,
    pub signal: &'static str,
    pub candidates: Vec<String>,
}

pub fn load_config(dir: &Path) -> Result<Config> {
    let path = dir.join("config.json");
    let config: Config = if path.exists() {
        serde_json::from_slice(&fs::read(path)?)?
    } else {
        Config::default()
    };
    if config.ignore_paths.iter().any(|path| !path.is_absolute()) {
        anyhow::bail!("ignore_paths entries must be absolute");
    }
    Ok(config)
}

pub fn load_labels(dir: &Path) -> Result<Labels> {
    let path = dir.join("labels.json");
    if path.exists() {
        Ok(serde_json::from_slice(&fs::read(path)?)?)
    } else {
        Ok(Labels::default())
    }
}

fn components(path: &Path) -> Vec<String> {
    path.components()
        .map(|part| part.as_os_str().to_string_lossy().into_owned())
        .collect()
}

fn glob_part(pattern: &str, value: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if let Some((before, after)) = pattern.split_once('*') {
        return value.starts_with(before)
            && value.ends_with(after)
            && value.len() >= before.len() + after.len();
    }
    pattern == value
}

fn prefix_match(glob: &str, relative: &Path) -> Option<usize> {
    let pattern = glob
        .split('/')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    let parts = components(relative);
    if pattern.len() > parts.len() {
        return None;
    }
    pattern
        .iter()
        .zip(&parts)
        .all(|(p, v)| glob_part(p, v))
        .then_some(pattern.len())
}

fn workspace_for(relative: &Path, config: &Config) -> Option<String> {
    config
        .workspaces
        .iter()
        .filter_map(|rule| {
            let depth = prefix_match(&rule.glob, relative)?;
            let name = rule.name.clone().or_else(|| {
                rule.segment
                    .and_then(|index| components(relative).get(index).cloned())
            })?;
            Some((depth, name))
        })
        .max_by_key(|(depth, _)| *depth)
        .map(|(_, name)| name)
}

fn repo_root(path: &Path) -> Option<PathBuf> {
    let mut current = if path.is_dir() { path } else { path.parent()? };
    loop {
        if current.join(".git").exists() {
            return Some(current.to_path_buf());
        }
        current = current.parent()?;
    }
}

fn ignored(path: &Path, config: &Config) -> bool {
    config
        .ignore_paths
        .iter()
        .any(|prefix| path.starts_with(prefix))
}

pub fn project_for_path(path: &Path, config: &Config) -> Option<(String, String)> {
    if ignored(path, config) {
        return None;
    }
    let superproject = config.superproject.as_deref();
    if let Some(root) = repo_root(path)
        && superproject != Some(root.as_path())
    {
        let workspace = superproject
            .and_then(|base| root.strip_prefix(base).ok())
            .and_then(|relative| workspace_for(relative, config))
            .unwrap_or_else(|| {
                root.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            });
        let name = root.file_name()?.to_string_lossy().into_owned();
        return Some((workspace.clone(), format!("{workspace}/{name}")));
    }
    let relative = path.strip_prefix(superproject?).ok()?;
    let (depth, _) = config
        .project_globs
        .iter()
        .filter_map(|glob| prefix_match(glob, relative).map(|depth| (depth, glob)))
        .max_by_key(|(depth, _)| *depth)?;
    let parts = components(relative);
    let name = parts.get(depth - 1)?.clone();
    let workspace = workspace_for(relative, config).unwrap_or_else(|| "(unattributed)".into());
    Some((workspace.clone(), format!("{workspace}/{name}")))
}

pub fn workspace_for_path(path: &Path, config: &Config) -> Option<String> {
    if ignored(path, config) {
        return None;
    }
    if let Some(superproject) = &config.superproject
        && let Ok(relative) = path.strip_prefix(superproject)
    {
        return workspace_for(relative, config);
    }
    repo_root(path).and_then(|root| {
        root.file_name()
            .map(|name| name.to_string_lossy().into_owned())
    })
}

/// Resolve path signals while the source directories still exist.
pub fn capture_signals(request: &mut Request, config: &Config) {
    let cwd = request.cwd.as_deref().map(PathBuf::from);
    if cwd.as_deref().is_some_and(|path| ignored(path, config)) {
        request.cwd = None;
    }
    request.cwd_project = request
        .cwd
        .as_deref()
        .and_then(|cwd| project_for_path(Path::new(cwd), config).map(|(_, project)| project));
    request.workspace = request
        .cwd
        .as_deref()
        .and_then(|cwd| workspace_for_path(Path::new(cwd), config));
    for touch in &mut request.tool_touches {
        for raw_path in touch.paths.drain(..) {
            let path = Path::new(&raw_path);
            let absolute = if path.is_absolute() {
                Some(path.to_path_buf())
            } else {
                cwd.as_deref().map(|cwd| cwd.join(path))
            };
            if absolute
                .as_deref()
                .is_some_and(|path| ignored(path, config))
            {
                continue;
            }
            touch.count += 1;
            if let Some(absolute) = absolute {
                if let Some((_, project)) = project_for_path(&absolute, config) {
                    touch.projects.push(project);
                }
                if let Some(workspace) = workspace_for_path(&absolute, config) {
                    touch.workspaces.push(workspace);
                }
            }
        }
        if touch.count > 0 && request.seen_tool_use_ids.insert(touch.id.clone()) {
            request.touch_count += touch.count;
            request
                .touched_projects
                .extend(touch.projects.iter().cloned());
            request
                .touched_workspaces
                .extend(touch.workspaces.iter().cloned());
        }
    }
}

/// Add only tool ids the stored request has not counted yet.
pub fn merge_touch_signals(stored: &mut Request, copy: &Request) {
    for touch in &copy.tool_touches {
        if touch.count > 0 && stored.seen_tool_use_ids.insert(touch.id.clone()) {
            stored.touch_count += touch.count;
            stored
                .touched_projects
                .extend(touch.projects.iter().cloned());
            stored
                .touched_workspaces
                .extend(touch.workspaces.iter().cloned());
        }
    }
}

/// Carry the union through a counter or sidechain replacement.
pub fn transfer_touch_signals(stored: &mut Request, replacement: &mut Request) {
    replacement.touch_count = stored.touch_count;
    replacement.touched_projects = std::mem::take(&mut stored.touched_projects);
    replacement.touched_workspaces = std::mem::take(&mut stored.touched_workspaces);
    replacement.seen_tool_use_ids = std::mem::take(&mut stored.seen_tool_use_ids);
}

fn alias<'a>(project: &'a str, config: &'a Config) -> &'a str {
    config.aliases.get(project).map_or(project, String::as_str)
}

fn catalog(requests: &[Request], config: &Config) -> BTreeSet<String> {
    requests
        .iter()
        .flat_map(|request| request.cwd_project.iter().chain(&request.touched_projects))
        .map(|project| alias(project, config).to_string())
        .chain(config.aliases.values().cloned())
        .collect()
}

fn label_project(label: &str, known: &BTreeSet<String>) -> Result<String, Vec<String>> {
    if known.contains(label) {
        return Ok(label.to_string());
    }
    let candidates = known
        .iter()
        .filter(|project| project.rsplit('/').next() == Some(label))
        .cloned()
        .collect::<Vec<_>>();
    if candidates.len() == 1 {
        Ok(candidates[0].clone())
    } else {
        Err(candidates)
    }
}

fn session_title_label(session: Option<&Session>) -> Option<&str> {
    session?
        .titles
        .iter()
        .filter(|title| title.source == "rename")
        .max_by_key(|title| (&title.file, title.offset))
        .and_then(|title| title.value.split_once(':').map(|(label, _)| label.trim()))
}

fn project_workspace(project: &str, fallback: Option<&str>) -> String {
    project
        .split_once('/')
        .map(|(workspace, _)| workspace.to_string())
        .or_else(|| fallback.map(str::to_string))
        .unwrap_or_else(|| "(unattributed)".into())
}

pub struct Resolver<'a> {
    config: &'a Config,
    labels: &'a Labels,
    known: BTreeSet<String>,
    dominant: HashMap<(String, String), String>,
    dominant_workspace: HashMap<(String, String), String>,
    best: HashMap<(String, String), Vec<String>>,
}

impl<'a> Resolver<'a> {
    pub fn new(requests: &[Request], config: &'a Config, labels: &'a Labels) -> Self {
        let mut counts = HashMap::<
            (String, String),
            (usize, HashMap<String, usize>, HashMap<String, usize>),
        >::new();
        for row in requests {
            let entry = counts
                .entry((row.profile.clone(), row.session.clone()))
                .or_default();
            entry.0 += row.touch_count;
            for project in &row.touched_projects {
                *entry
                    .1
                    .entry(alias(project, config).to_string())
                    .or_default() += 1;
            }
            for workspace in &row.touched_workspaces {
                *entry.2.entry(workspace.clone()).or_default() += 1;
            }
        }
        let mut dominant = HashMap::new();
        let mut dominant_workspace = HashMap::new();
        let mut best = HashMap::new();
        for (session, (total, projects, workspaces)) in counts {
            let mut ranked = projects.into_iter().collect::<Vec<_>>();
            ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            if let Some((project, count)) = ranked.first()
                && total > 0
                && count * 10 >= total * 6
            {
                dominant.insert(session.clone(), project.clone());
            }
            best.insert(
                session.clone(),
                ranked
                    .into_iter()
                    .take(3)
                    .map(|(project, _)| project)
                    .collect(),
            );
            let mut ranked_workspaces = workspaces.into_iter().collect::<Vec<_>>();
            ranked_workspaces.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            if let Some((workspace, count)) = ranked_workspaces.first()
                && total > 0
                && count * 10 >= total * 6
            {
                dominant_workspace.insert(session, workspace.clone());
            }
        }
        Self {
            config,
            labels,
            known: catalog(requests, config),
            dominant,
            dominant_workspace,
            best,
        }
    }

    pub fn attribute(&self, request: &Request, session: Option<&Session>) -> Attribution {
        let config = self.config;
        let explicit = self
            .labels
            .sessions
            .get(&request.session)
            .map(String::as_str);
        let title = session_title_label(session);
        let mut candidates = Vec::new();
        for label in [explicit, title].into_iter().flatten() {
            match label_project(label, &self.known) {
                Ok(project) => {
                    return Attribution {
                        workspace: project_workspace(&project, request.workspace.as_deref()),
                        project,
                        signal: "label",
                        candidates,
                    };
                }
                Err(found) => {
                    candidates.push(format!("unresolved label '{label}'"));
                    candidates.extend(
                        found
                            .into_iter()
                            .map(|project| format!("label match '{project}'")),
                    );
                }
            }
        }
        if let Some(project) = &request.cwd_project {
            let project = alias(project, config).to_string();
            return Attribution {
                workspace: project_workspace(&project, request.workspace.as_deref()),
                project,
                signal: "cwd",
                candidates,
            };
        }
        if let Some(project) = self
            .dominant
            .get(&(request.profile.clone(), request.session.clone()))
        {
            return Attribution {
                workspace: project_workspace(project, request.workspace.as_deref()),
                project: project.clone(),
                signal: "files",
                candidates,
            };
        }
        candidates.extend(
            self.best
                .get(&(request.profile.clone(), request.session.clone()))
                .into_iter()
                .flatten()
                .map(|project| format!("file touch '{project}'")),
        );
        if let Some(workspace) = &request.workspace {
            return Attribution {
                workspace: workspace.clone(),
                project: format!("{workspace}/(workspace files)"),
                signal: "workspace",
                candidates,
            };
        }
        if let Some(workspace) = self
            .dominant_workspace
            .get(&(request.profile.clone(), request.session.clone()))
        {
            return Attribution {
                workspace: workspace.clone(),
                project: format!("{workspace}/(workspace files)"),
                signal: "files workspace",
                candidates,
            };
        }
        Attribution {
            workspace: "(unattributed)".into(),
            project: "(unattributed)".into(),
            signal: "none",
            candidates,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::parse;
    use serde_json::json;

    fn row(cwd: &Path, touched: &[&Path], session: &str) -> Request {
        let tools = touched
            .iter()
            .enumerate()
            .map(|(index, path)| {
                json!({"type":"tool_use",
                "id":format!("toolu_synthetic_{session}_{index}"),
                "input":{"file_path":path}})
            })
            .collect::<Vec<_>>();
        let line = json!({"type":"assistant","timestamp":"2030-01-01T12:00:00Z","sessionId":session,
            "requestId":format!("req-{session}"),"cwd":cwd,"message":{"id":format!("msg-{session}"),
                "model":"claude-sonnet-5","content":tools,"usage":{"input_tokens":1,"output_tokens":1}}});
        parse::parse(line.to_string().as_bytes(), "sample")
            .unwrap()
            .unwrap()
            .requests
            .remove(0)
    }

    #[test]
    fn attribution_priority_and_explanation_signals() {
        // Known-bad: letting cwd beat a label, or files beat cwd, or treating
        // a workspace root as a project rather than its fallback bucket.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("atlas");
        let team = root.join("teams/blue");
        let alpha = team.join("apps/alpha");
        let beta = team.join("apps/beta");
        fs::create_dir_all(alpha.join(".git")).unwrap();
        fs::create_dir_all(beta.join(".git")).unwrap();
        fs::create_dir_all(root.join(".git")).unwrap();
        let misc = root.join("misc");
        fs::create_dir_all(&misc).unwrap();
        let config = Config {
            superproject: Some(root.clone()),
            project_globs: vec!["teams/*/apps/*".into()],
            workspaces: vec![WorkspaceRule {
                glob: "teams/*".into(),
                name: None,
                segment: Some(1),
            }],
            aliases: BTreeMap::new(),
            ignore_paths: Vec::new(),
        };
        let mut rows = vec![
            row(&beta, &[&alpha.join("a.rs")], "labelled"),
            row(&beta, &[&alpha.join("a.rs")], "cwd"),
            row(
                &team,
                &[&alpha.join("a.rs"), &alpha.join("b.rs"), &beta.join("c.rs")],
                "files",
            ),
            row(&team, &[], "workspace"),
            row(&team, &[&alpha.join("a.rs"), &beta.join("b.rs")], "weak"),
            row(&misc, &[], "none"),
        ];
        for request in &mut rows {
            capture_signals(request, &config);
        }
        let labels = Labels {
            sessions: BTreeMap::from([("labelled".into(), "alpha".into())]),
        };
        let resolver = Resolver::new(&rows, &config, &labels);
        let cases = [
            ("labelled", "blue/alpha", "label"),
            ("cwd", "blue/beta", "cwd"),
            ("files", "blue/alpha", "files"),
            ("workspace", "blue/(workspace files)", "workspace"),
            ("weak", "blue/(workspace files)", "workspace"),
            ("none", "(unattributed)", "none"),
        ];
        for (session, project, signal) in cases {
            let request = rows.iter().find(|row| row.session == session).unwrap();
            let got = resolver.attribute(request, None);
            assert_eq!((got.project.as_str(), got.signal), (project, signal));
            if session == "weak" {
                assert_eq!(
                    got.candidates,
                    vec!["file touch 'blue/alpha'", "file touch 'blue/beta'"]
                );
            }
        }
    }

    #[test]
    fn nested_repo_alias_relative_paths_and_ambiguous_label() {
        // Known-bad: a parent repo winning over a nested repo, ignoring a
        // relative tool path, or silently guessing an ambiguous bare label.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("atlas");
        let one = root.join("teams/blue/apps/site");
        let two = root.join("teams/red/apps/site");
        let three = root.join("teams/green/apps/site");
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::create_dir_all(one.join(".git")).unwrap();
        fs::create_dir_all(two.join(".git")).unwrap();
        fs::create_dir_all(three.join(".git")).unwrap();
        let config = Config {
            superproject: Some(root.clone()),
            project_globs: vec!["teams/*/apps/*".into()],
            workspaces: vec![WorkspaceRule {
                glob: "teams/*".into(),
                name: None,
                segment: Some(1),
            }],
            aliases: BTreeMap::from([("blue/site".into(), "blue/web".into())]),
            ignore_paths: Vec::new(),
        };
        let mut rows = vec![
            row(&one, &[Path::new("src/main.rs")], "one"),
            row(&two, &[], "two"),
            row(&three, &[], "three"),
        ];
        for request in &mut rows {
            capture_signals(request, &config);
        }
        assert_eq!(rows[0].touched_projects, vec!["blue/site"]);
        let labels = Labels {
            sessions: BTreeMap::from([("one".into(), "site".into())]),
        };
        let resolver = Resolver::new(&rows, &config, &labels);
        let first = resolver.attribute(&rows[0], None);
        assert_eq!((first.project.as_str(), first.signal), ("blue/web", "cwd"));
        assert!(
            first
                .candidates
                .iter()
                .any(|candidate| candidate.contains("unresolved label"))
        );
    }

    #[test]
    fn file_touches_can_choose_the_session_workspace_bucket() {
        // Known-bad: reading the workspace only from cwd leaves this request
        // unattributed even though all session file touches are in one workspace.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("atlas");
        let files = root.join("teams/blue/scratch");
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::create_dir_all(&files).unwrap();
        let config = Config {
            superproject: Some(root),
            project_globs: vec!["teams/*/apps/*".into()],
            workspaces: vec![WorkspaceRule {
                glob: "teams/*".into(),
                name: None,
                segment: Some(1),
            }],
            aliases: BTreeMap::new(),
            ignore_paths: Vec::new(),
        };
        let mut request = row(
            &tmp.path().join("outside"),
            &[&files.join("a.md"), &files.join("b.md")],
            "sample",
        );
        request.cwd = None;
        capture_signals(&mut request, &config);
        assert!(request.cwd_project.is_none());
        assert!(request.workspace.is_none());
        assert!(request.touched_projects.is_empty());
        assert_eq!(request.touched_workspaces, vec!["blue", "blue"]);
        let rows = [request];
        let labels = Labels::default();
        let resolver = Resolver::new(&rows, &config, &labels);
        let result = resolver.attribute(&rows[0], None);
        assert_eq!(result.project, "blue/(workspace files)");
        assert_eq!(result.signal, "files workspace");
    }

    #[test]
    fn ignored_paths_leave_no_cwd_signal_or_denominator_weight() {
        // Known-bad: an ignored cwd still wins attribution, or ignored file
        // touches count against the 60% denominator.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("atlas");
        let alpha = root.join("teams/blue/apps/alpha");
        let beta = root.join("teams/blue/apps/beta");
        let ignored = beta.join("scratch");
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::create_dir_all(alpha.join(".git")).unwrap();
        fs::create_dir_all(beta.join(".git")).unwrap();
        fs::create_dir_all(&ignored).unwrap();
        let config = Config {
            superproject: Some(root),
            project_globs: vec!["teams/*/apps/*".into()],
            workspaces: vec![WorkspaceRule {
                glob: "teams/*".into(),
                name: None,
                segment: Some(1),
            }],
            aliases: BTreeMap::new(),
            ignore_paths: vec![ignored.clone()],
        };
        let mut request = row(
            &ignored,
            &[
                &ignored.join("a.rs"),
                &ignored.join("b.rs"),
                &alpha.join("c.rs"),
            ],
            "sample",
        );
        capture_signals(&mut request, &config);
        assert!(request.cwd.is_none());
        assert!(request.cwd_project.is_none());
        assert!(request.workspace.is_none());
        assert_eq!(request.touch_count, 1);
        assert_eq!(request.touched_projects, vec!["blue/alpha"]);
        let rows = [request];
        let labels = Labels::default();
        let resolver = Resolver::new(&rows, &config, &labels);
        let result = resolver.attribute(&rows[0], None);
        assert_eq!(
            (result.project.as_str(), result.signal),
            ("blue/alpha", "files")
        );
    }

    #[test]
    fn ignore_prefix_matches_path_components() {
        // Known-bad: a string-prefix check drops scratchpad when only scratch
        // is listed; Path::starts_with keeps these components distinct.
        let config = Config {
            ignore_paths: vec![PathBuf::from("/synthetic/scratch")],
            ..Config::default()
        };
        let mut request = row(
            Path::new("/synthetic/scratchpad"),
            &[Path::new("/synthetic/scratchpad/a.rs")],
            "prefix",
        );
        capture_signals(&mut request, &config);
        assert_eq!(request.cwd.as_deref(), Some("/synthetic/scratchpad"));
        assert_eq!(request.touch_count, 1);
    }
}
