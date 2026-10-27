//! Append-only JSONL session transcripts: one JSON object per line.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use chrono::{DateTime, Utc};
use crusty_core::message::{ContentBlock, Message, Role, blocks_text};
use serde::{Deserialize, Serialize};

use crate::truncate_chars;

/// Agent id of the top-level conversation; only its messages are restored on load.
const ORCHESTRATOR: &str = "orchestrator";
const SUMMARY_HEADER: &str = "[Conversation summary]\n";
const TITLE_CHARS: usize = 80;
const TOOL_INPUT_CHARS: usize = 200;
const TOOL_RESULT_CHARS: usize = 2000;
/// Sessions that `list_sessions` loads; older transcripts are not listed.
const LIST_LIMIT: usize = 50;

/// One line of a transcript file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Entry {
    /// First line of every transcript.
    Meta {
        session_id: String,
        cwd: PathBuf,
        created_at: DateTime<Utc>,
        version: String,
    },
    /// A message produced by `agent_id`.
    Message {
        agent_id: String,
        message: Message,
        ts: DateTime<Utc>,
    },
    /// Everything before this point was replaced by `summary`, except the last `kept`
    /// orchestrator messages (absent in older transcripts: 0).
    Compaction {
        summary: String,
        replaced: usize,
        #[serde(default)]
        kept: usize,
        ts: DateTime<Utc>,
    },
    Title {
        title: String,
    },
    Usage {
        input_tokens: u64,
        output_tokens: u64,
        cost_usd: f64,
        ts: DateTime<Utc>,
    },
    /// The conversation was rewound (`/rewind`): only the first `keep_messages` orchestrator
    /// messages stay.
    Rewind {
        keep_messages: usize,
        ts: DateTime<Utc>,
    },
}

/// A transcript open for appending. Each [`Transcript::append`] writes one full line.
#[derive(Debug)]
pub struct Transcript {
    path: PathBuf,
    session_id: String,
    file: File,
}

impl Transcript {
    /// Creates `dir/<session_id>.jsonl` and writes its `Meta` entry. Fails if it already exists,
    /// so an existing session is never truncated.
    pub fn create(dir: &Path, session_id: &str, cwd: &Path) -> io::Result<Self> {
        fs::create_dir_all(dir)?;
        let path = dir.join(format!("{session_id}.jsonl"));
        let file = OpenOptions::new().write(true).create_new(true).open(&path)?;
        let mut transcript = Transcript { path, session_id: session_id.to_owned(), file };
        transcript.append(&Entry::Meta {
            session_id: session_id.to_owned(),
            cwd: cwd.to_path_buf(),
            created_at: Utc::now(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
        })?;
        Ok(transcript)
    }

    /// Reopens an existing transcript for appending. Its session id comes from the `Meta` line,
    /// falling back to the file name.
    pub fn open_append(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new().append(true).open(path)?;
        let session_id = meta_session_id(path).unwrap_or_else(|| file_stem(path));
        Ok(Transcript { path: path.to_path_buf(), session_id, file })
    }

    /// Appends `entry` as one JSON line and flushes it.
    pub fn append(&mut self, entry: &Entry) -> io::Result<()> {
        let mut line = serde_json::to_string(entry).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        line.push('\n');
        self.file.write_all(line.as_bytes())?;
        self.file.flush()
    }

    /// Location of the transcript file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Session id this transcript belongs to.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }
}

/// A fresh random (v4) session id.
pub fn new_session_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// A transcript restored from disk.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LoadedSession {
    pub session_id: String,
    pub cwd: PathBuf,
    /// Orchestrator messages, with compaction applied.
    pub messages: Vec<Message>,
    /// The last `Title` entry, if any.
    pub title: Option<String>,
    /// Unreadable lines and other recoverable problems.
    pub warnings: Vec<String>,
}

/// Restores a session. Corrupt lines are skipped and reported in `warnings`; only I/O errors
/// fail.
pub fn load(path: &Path) -> io::Result<LoadedSession> {
    let bytes = fs::read(path)?;
    let text = String::from_utf8_lossy(&bytes);
    let mut session = LoadedSession { session_id: file_stem(path), ..LoadedSession::default() };
    let mut saw_meta = false;
    for (index, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let entry = match serde_json::from_str::<Entry>(line) {
            Ok(entry) => entry,
            Err(e) => {
                session.warnings.push(format!("line {}: skipped unreadable entry: {e}", index + 1));
                continue;
            }
        };
        match entry {
            Entry::Meta { session_id, cwd, .. } => {
                if !saw_meta {
                    session.session_id = session_id;
                    session.cwd = cwd;
                    saw_meta = true;
                }
            }
            Entry::Message { agent_id, message, .. } => {
                if agent_id == ORCHESTRATOR {
                    session.messages.push(message);
                }
            }
            Entry::Compaction { summary, kept, .. } => {
                // The kept tail stays verbatim; the summary joins its first message, as in memory.
                let tail_start = session.messages.len().saturating_sub(kept);
                let tail = session.messages.split_off(tail_start);
                session.messages.clear();
                let summary_block = ContentBlock::text(format!("{SUMMARY_HEADER}{summary}"));
                match tail.split_first() {
                    Some((first, rest)) => {
                        let mut first = first.clone();
                        first.content.insert(0, summary_block);
                        session.messages.push(first);
                        session.messages.extend(rest.iter().cloned());
                    }
                    None => session.messages.push(Message { role: Role::User, content: vec![summary_block] }),
                }
            }
            Entry::Title { title } => session.title = Some(title),
            Entry::Rewind { keep_messages, .. } => session.messages.truncate(keep_messages),
            Entry::Usage { .. } => {}
        }
    }
    if !saw_meta {
        session.warnings.push("no meta entry; session id taken from the file name".into());
    }
    Ok(session)
}

/// Summary of a stored session, for pickers and listings.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionSummary {
    pub id: String,
    pub path: PathBuf,
    pub modified: SystemTime,
    /// The session title, else the first user prompt. At most 80 characters.
    pub title_or_first_prompt: String,
    pub messages: usize,
}

/// The newest readable sessions in `dir` (at most 50), newest first. A missing directory yields
/// an empty list.
pub fn list_sessions(dir: &Path) -> Vec<SessionSummary> {
    let mut files = transcript_files(dir);
    files.sort_by_key(|(_, modified)| std::cmp::Reverse(*modified));
    files.truncate(LIST_LIMIT);
    files
        .into_iter()
        .filter_map(|(path, modified)| {
            let session = load(&path).ok()?;
            let title = session.title.as_deref().map(one_line).or_else(|| first_prompt(&session.messages));
            Some(SessionSummary {
                id: session.session_id,
                path,
                modified,
                title_or_first_prompt: title.unwrap_or_default(),
                messages: session.messages.len(),
            })
        })
        .collect()
}

/// Path of the most recently modified transcript in `dir`.
pub fn latest(dir: &Path) -> Option<PathBuf> {
    transcript_files(dir).into_iter().max_by_key(|(_, modified)| *modified).map(|(path, _)| path)
}

/// Path of the most recently modified transcript whose id starts with `id_prefix`. An empty
/// prefix matches nothing.
pub fn find(dir: &Path, id_prefix: &str) -> Option<PathBuf> {
    if id_prefix.is_empty() {
        return None;
    }
    transcript_files(dir)
        .into_iter()
        .filter(|(path, _)| file_stem(path).starts_with(id_prefix))
        .max_by_key(|(_, modified)| *modified)
        .map(|(path, _)| path)
}

/// Renders messages as Markdown: `### User` / `### Assistant` sections, tool calls as quote
/// lines and tool results as fenced blocks. Long tool input and output are truncated.
pub fn export_markdown(messages: &[Message]) -> String {
    let mut out = String::new();
    for message in messages {
        let heading = match message.role {
            Role::User => "User",
            Role::Assistant => "Assistant",
            Role::System => "System",
        };
        out.push_str(&format!("### {heading}\n\n"));
        for block in &message.content {
            match block {
                ContentBlock::Text { text, .. } => out.push_str(&format!("{text}\n\n")),
                ContentBlock::ToolUse { name, input, .. } => {
                    let input = truncate_chars(&input.to_string(), TOOL_INPUT_CHARS);
                    out.push_str(&format!("> tool: {name}({input})\n\n"));
                }
                ContentBlock::ToolResult { content, .. } => {
                    let body = truncate_chars(&blocks_text(content), TOOL_RESULT_CHARS);
                    out.push_str(&format!("```\n{body}\n```\n\n"));
                }
                ContentBlock::Image { .. } => out.push_str("> [image]\n\n"),
                _ => {}
            }
        }
    }
    out
}

/// `(path, modified)` for each `*.jsonl` file directly in `dir`.
fn transcript_files(dir: &Path) -> Vec<(PathBuf, SystemTime)> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                return None;
            }
            let meta = entry.metadata().ok()?;
            if !meta.is_file() {
                return None;
            }
            Some((path, meta.modified().ok()?))
        })
        .collect()
}

fn meta_session_id(path: &Path) -> Option<String> {
    let file = File::open(path).ok()?;
    BufReader::new(file).lines().map_while(Result::ok).find_map(|line| match serde_json::from_str::<Entry>(&line).ok()? {
        Entry::Meta { session_id, .. } => Some(session_id),
        _ => None,
    })
}

fn file_stem(path: &Path) -> String {
    path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
}

fn first_prompt(messages: &[Message]) -> Option<String> {
    messages.iter().find(|m| m.role == Role::User && !m.text().trim().is_empty()).map(|m| one_line(&m.text()))
}

/// Collapses whitespace (including newlines) and truncates to the title length.
fn one_line(text: &str) -> String {
    truncate_chars(&text.split_whitespace().collect::<Vec<_>>().join(" "), TITLE_CHARS)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use chrono::Utc;
    use serde_json::json;
    use tempfile::tempdir;

    use super::*;

    fn msg(agent: &str, message: Message) -> Entry {
        Entry::Message { agent_id: agent.into(), message, ts: Utc::now() }
    }

    #[test]
    fn create_append_load_round_trip() {
        let dir = tempdir().unwrap();
        let cwd = dir.path().join("project");
        let id = new_session_id();
        assert_eq!(id.len(), 36);
        let mut t = Transcript::create(dir.path(), &id, &cwd).unwrap();
        t.append(&msg("orchestrator", Message::user_text("hi"))).unwrap();
        t.append(&msg("orchestrator", Message::assistant_text("hello"))).unwrap();
        t.append(&Entry::Title { title: "Greeting".into() }).unwrap();
        t.append(&Entry::Usage { input_tokens: 1, output_tokens: 2, cost_usd: 0.5, ts: Utc::now() }).unwrap();

        let loaded = load(t.path()).unwrap();
        assert_eq!(loaded.session_id, id);
        assert_eq!(loaded.cwd, cwd);
        assert_eq!(loaded.title.as_deref(), Some("Greeting"));
        assert_eq!(loaded.messages, vec![Message::user_text("hi"), Message::assistant_text("hello")]);
        assert!(loaded.warnings.is_empty());

        let mut reopened = Transcript::open_append(t.path()).unwrap();
        assert_eq!(reopened.session_id(), id);
        reopened.append(&msg("orchestrator", Message::user_text("again"))).unwrap();
        assert_eq!(load(t.path()).unwrap().messages.len(), 3);
    }

    #[test]
    fn compaction_replaces_earlier_messages() {
        let dir = tempdir().unwrap();
        let mut t = Transcript::create(dir.path(), "c1", dir.path()).unwrap();
        t.append(&msg("orchestrator", Message::user_text("one"))).unwrap();
        t.append(&msg("orchestrator", Message::assistant_text("two"))).unwrap();
        t.append(&Entry::Compaction { summary: "earlier work".into(), replaced: 2, kept: 0, ts: Utc::now() }).unwrap();
        t.append(&msg("orchestrator", Message::user_text("three"))).unwrap();

        let loaded = load(t.path()).unwrap();
        assert_eq!(loaded.messages.len(), 2);
        assert_eq!(loaded.messages[0].text(), "[Conversation summary]\nearlier work");
        assert_eq!(loaded.messages[1].text(), "three");
    }

    #[test]
    fn compaction_keeps_the_tail_on_resume() {
        let dir = tempdir().unwrap();
        let mut t = Transcript::create(dir.path(), "c4", dir.path()).unwrap();
        t.append(&msg("orchestrator", Message::user_text("one"))).unwrap();
        t.append(&msg("orchestrator", Message::assistant_text("two"))).unwrap();
        t.append(&msg("orchestrator", Message::user_text("three"))).unwrap();
        t.append(&msg("orchestrator", Message::assistant_text("four"))).unwrap();
        t.append(&Entry::Compaction { summary: "earlier".into(), replaced: 2, kept: 2, ts: Utc::now() }).unwrap();
        t.append(&msg("orchestrator", Message::user_text("five"))).unwrap();

        let loaded = load(t.path()).unwrap();
        // Kept tail: "three" and "four"; the summary joins "three", then "five" follows.
        assert_eq!(loaded.messages.len(), 3);
        assert_eq!(loaded.messages[0].role, Role::User);
        assert_eq!(loaded.messages[0].content[0].as_text(), Some("[Conversation summary]\nearlier"));
        assert!(loaded.messages[0].text().starts_with("[Conversation summary]\nearlier"));
        assert!(loaded.messages[0].text().ends_with("three"));
        assert_eq!(loaded.messages[1].text(), "four");
        assert_eq!(loaded.messages[2].text(), "five");
    }

    #[test]
    fn corrupt_line_is_skipped_with_warning() {
        let dir = tempdir().unwrap();
        let mut t = Transcript::create(dir.path(), "c2", dir.path()).unwrap();
        t.append(&msg("orchestrator", Message::user_text("kept"))).unwrap();
        let mut raw = OpenOptions::new().append(true).open(t.path()).unwrap();
        raw.write_all(b"{not json\n").unwrap();

        let loaded = load(t.path()).unwrap();
        assert_eq!(loaded.messages.len(), 1);
        assert_eq!(loaded.warnings.len(), 1);
        assert!(loaded.warnings[0].starts_with("line 3"));
    }

    #[test]
    fn rewind_truncates_to_kept_messages() {
        let dir = tempdir().unwrap();
        let mut t = Transcript::create(dir.path(), "c3", dir.path()).unwrap();
        t.append(&msg("orchestrator", Message::user_text("one"))).unwrap();
        t.append(&msg("orchestrator", Message::assistant_text("two"))).unwrap();
        t.append(&msg("orchestrator", Message::user_text("three"))).unwrap();
        t.append(&Entry::Rewind { keep_messages: 1, ts: Utc::now() }).unwrap();
        t.append(&msg("orchestrator", Message::user_text("four"))).unwrap();

        let loaded = load(t.path()).unwrap();
        assert_eq!(loaded.messages, vec![Message::user_text("one"), Message::user_text("four")]);
    }

    #[test]
    fn sub_agent_messages_are_not_restored() {
        let dir = tempdir().unwrap();
        let mut t = Transcript::create(dir.path(), "c3", dir.path()).unwrap();
        t.append(&msg("worker-1", Message::user_text("internal"))).unwrap();
        t.append(&msg("orchestrator", Message::user_text("visible"))).unwrap();
        assert_eq!(load(t.path()).unwrap().messages, vec![Message::user_text("visible")]);
    }

    #[test]
    fn list_sessions_is_newest_first_and_find_matches_prefix() {
        let dir = tempdir().unwrap();
        let mut old = Transcript::create(dir.path(), "session-old", dir.path()).unwrap();
        old.append(&msg("orchestrator", Message::user_text("first  prompt\nwith lines"))).unwrap();
        let old_path = old.path().to_path_buf();
        drop(old);
        let mut new = Transcript::create(dir.path(), "session-new", dir.path()).unwrap();
        new.append(&msg("orchestrator", Message::user_text("newer"))).unwrap();
        let new_path = new.path().to_path_buf();
        drop(new);
        let past = SystemTime::now() - Duration::from_secs(600);
        OpenOptions::new().write(true).open(&old_path).unwrap().set_modified(past).unwrap();

        let listed = list_sessions(dir.path());
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].id, "session-new");
        assert_eq!(listed[1].id, "session-old");
        assert_eq!(listed[1].title_or_first_prompt, "first prompt with lines");
        assert_eq!(listed[1].messages, 1);

        assert_eq!(latest(dir.path()), Some(new_path.clone()));
        assert_eq!(find(dir.path(), "session-o"), Some(old_path));
        assert_eq!(find(dir.path(), "session-n"), Some(new_path));
        assert_eq!(find(dir.path(), "zzz"), None);
        assert_eq!(find(dir.path(), ""), None);
        assert!(list_sessions(&dir.path().join("missing")).is_empty());
    }

    #[test]
    fn list_sessions_loads_only_the_newest_fifty() {
        let dir = tempdir().unwrap();
        for i in 0..52u64 {
            let id = format!("s{i:02}");
            let path = Transcript::create(dir.path(), &id, dir.path()).unwrap().path().to_path_buf();
            let mtime = SystemTime::UNIX_EPOCH + Duration::from_secs(1000 + i);
            OpenOptions::new().write(true).open(&path).unwrap().set_modified(mtime).unwrap();
        }
        let listed = list_sessions(dir.path());
        assert_eq!(listed.len(), 50);
        assert_eq!(listed[0].id, "s51");
        assert_eq!(listed[49].id, "s02");
    }

    #[test]
    fn meta_id_is_found_after_a_leading_bad_line() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("file-name.jsonl");
        let meta = Entry::Meta { session_id: "real-id".into(), cwd: dir.path().to_path_buf(), created_at: Utc::now(), version: "t".into() };
        fs::write(&path, format!("not json\n{}\n", serde_json::to_string(&meta).unwrap())).unwrap();
        assert_eq!(Transcript::open_append(&path).unwrap().session_id(), "real-id");
    }

    #[test]
    fn export_markdown_formats_roles_tools_and_results() {
        let messages = vec![
            Message::user_text("Run it"),
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::tool_use("t1", "Bash", json!({"command": "ls"})), ContentBlock::text("done")],
            },
            Message { role: Role::User, content: vec![ContentBlock::tool_result("t1", vec![ContentBlock::text("ok")], false)] },
        ];
        let md = export_markdown(&messages);
        assert!(md.contains("### User\n\nRun it\n\n"));
        assert!(md.contains("### Assistant\n\n> tool: Bash({\"command\":\"ls\"})\n\ndone\n\n"));
        assert!(md.contains("```\nok\n```"));
    }

    #[test]
    fn export_markdown_truncates_long_tool_output() {
        let long = "x".repeat(5000);
        let messages =
            vec![Message { role: Role::User, content: vec![ContentBlock::tool_result("t", vec![ContentBlock::text(long)], false)] }];
        let md = export_markdown(&messages);
        assert!(md.len() < 2200);
        assert!(md.contains('…'));
    }
}
