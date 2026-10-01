use super::parse_utils::*;
use super::sqlite_ro::{open_sqlite_ro, virtual_path};
use super::AgentAdapter;
use crate::models::*;
use anyhow::{anyhow, Result};
use std::collections::HashMap;
use std::path::PathBuf;

/// Antigravity CLI / IDE:
/// 会话索引在 `~/.gemini/antigravity/conversation_summaries.db`(或旧版 `antigravity-cli`)，
/// 会话完整明文记录在 `~/.gemini/antigravity/brain/<id>/.system_generated/logs/transcript.jsonl`。
/// 若 transcript.jsonl 存在，则解析全量对话正文（用户提问、模型回答、思考过程、工具调用与结果）；
/// 若不存在，则优雅降级为 SQLite 中的 preview 摘要。
pub struct AntigravityAdapter {
    db: PathBuf,
    brain_dir: PathBuf,
    /// 全表很小(元数据行),按 db mtime 缓存一轮扫描内的重复调用
    rows_cache: MtimeCache<Vec<AgRow>>,
}

impl AntigravityAdapter {
    pub fn new() -> Self {
        let gemini = super::home_dir().unwrap_or_default().join(".gemini");
        let (db, brain_dir) = if gemini
            .join("antigravity")
            .join("conversation_summaries.db")
            .exists()
        {
            (
                gemini.join("antigravity").join("conversation_summaries.db"),
                gemini.join("antigravity").join("brain"),
            )
        } else {
            (
                gemini
                    .join("antigravity-cli")
                    .join("conversation_summaries.db"),
                gemini.join("antigravity-cli").join("brain"),
            )
        };
        Self {
            db,
            brain_dir,
            rows_cache: MtimeCache::new(),
        }
    }

    fn resolve_brain_dir(&self) -> PathBuf {
        if self.brain_dir.is_dir() {
            self.brain_dir.clone()
        } else if let Some(parent) = self.db.parent() {
            if parent.join("brain").is_dir() {
                parent.join("brain")
            } else {
                self.brain_dir.clone()
            }
        } else {
            self.brain_dir.clone()
        }
    }

    fn rows(&self) -> Option<Vec<AgRow>> {
        let mtime = super::sqlite_ro::db_cache_stamp(&self.db);
        self.rows_cache.get_or_try_build(mtime, || {
            let ro = open_sqlite_ro(&self.db, "antigravity")?;
            let mut stmt = ro
                .conn
                .prepare(
                    "SELECT conversation_id, title, preview, step_count, last_modified_time, workspace_uris
                     FROM conversation_summaries
                     WHERE parent_conversation_id = '' AND nesting_depth = 0",
                )
                .ok()?;
            let rows = stmt
                .query_map([], |r| {
                    Ok(AgRow {
                        id: r.get(0)?,
                        title: r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                        preview: r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                        step_count: r.get::<_, Option<i64>>(3)?.unwrap_or(0),
                        modified_ms: sqlite_dt_ms(
                            r.get::<_, Option<String>>(4)?.unwrap_or_default().trim(),
                        ),
                        cwd: first_workspace(&r.get::<_, Option<String>>(5)?.unwrap_or_default()),
                    })
                })
                .ok()?
                .collect::<rusqlite::Result<Vec<_>>>()
                .ok()?;
            Some(rows)
        })
    }

    fn build_meta(
        &self,
        r: &SessionFileRef,
        row: &AgRow,
        messages: &[TranscriptMessage],
    ) -> SessionMeta {
        let title = Some(clean_title_candidate(&row.title))
            .filter(|t| !t.is_empty())
            .or_else(|| Some(clean_title_candidate(&row.preview)).filter(|t| !t.is_empty()))
            .or_else(|| title_from_messages(messages))
            .unwrap_or_else(|| UNTITLED.to_string());

        let first_ts = messages.iter().find_map(|m| m.timestamp);
        let last_ts = messages.iter().rev().find_map(|m| m.timestamp);
        let created_at = first_ts.unwrap_or(if row.modified_ms > 0 {
            row.modified_ms
        } else {
            r.mtime_ms
        });
        let updated_at = last_ts.unwrap_or(if row.modified_ms > 0 {
            row.modified_ms
        } else {
            r.mtime_ms
        });
        let message_count = if messages.is_empty() {
            row.step_count
        } else {
            messages.len() as i64
        };

        SessionMeta {
            key: format!("antigravity:{}", row.id),
            host: String::new(),
            id: row.id.clone(),
            agent: AgentId::Antigravity,
            title,
            project_path: row.cwd.clone(),
            project_name: project_name_of(&row.cwd),
            file_path: r.file_path.clone(),
            created_at,
            updated_at,
            message_count,
            size_bytes: r.size,
            git_branch: None,
            model: None,
            tokens_used: None,
            archived: false,
            source: None,
            favorite: false,
            pinned: false,
        }
    }

    fn parse_transcript_jsonl(&self, native_id: &str) -> Option<Vec<TranscriptMessage>> {
        let brain = self.resolve_brain_dir();
        let transcript = brain
            .join(native_id)
            .join(".system_generated")
            .join("logs")
            .join("transcript.jsonl");
        if !transcript.is_file() {
            return None;
        }

        let file = std::fs::File::open(&transcript).ok()?;
        let reader = std::io::BufReader::new(file);
        let mut messages: Vec<TranscriptMessage> = Vec::new();

        use std::io::BufRead;
        for line in reader.lines() {
            let line = match line {
                Ok(l) => l,
                Err(_) => continue,
            };
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let v: serde_json::Value = match serde_json::from_str(trimmed) {
                Ok(val) => val,
                Err(_) => continue,
            };

            let source = v.get("source").and_then(|s| s.as_str()).unwrap_or("");
            let step_type = v.get("type").and_then(|s| s.as_str()).unwrap_or("");
            let ts = iso_ms(v.get("created_at").and_then(|s| s.as_str()).unwrap_or(""));

            match step_type {
                "USER_INPUT" => {
                    let content = v.get("content").and_then(|c| c.as_str()).unwrap_or("");
                    let user_text = extract_user_request(content);
                    if !user_text.is_empty() {
                        let role =
                            if source == "SYSTEM" || user_text.starts_with("<SYSTEM_MESSAGE>") {
                                Role::System
                            } else {
                                Role::User
                            };
                        messages.push(text_msg(role, &user_text, ts));
                    }
                }
                "PLANNER_RESPONSE" if source == "MODEL" => {
                    let content = v.get("content").and_then(|c| c.as_str()).unwrap_or("");
                    let thinking = v
                        .get("thinking")
                        .and_then(|t| t.as_str())
                        .filter(|s| !s.trim().is_empty())
                        .map(|s| clip(s.trim(), MAX_TOOL_IO).0);

                    let mut tool_calls = Vec::new();
                    if let Some(calls) = v.get("tool_calls").and_then(|c| c.as_array()) {
                        for tc in calls {
                            let name = tc.get("name").and_then(|n| n.as_str()).unwrap_or("tool");
                            let args = tc.get("args").unwrap_or(&serde_json::Value::Null);
                            tool_calls.push(tool_call_view(
                                String::new(),
                                name,
                                args,
                                None,
                                false,
                            ));
                        }
                    }

                    if !content.trim().is_empty() || thinking.is_some() || !tool_calls.is_empty() {
                        let mut msg = text_msg(Role::Assistant, content, ts);
                        msg.thinking = thinking;
                        msg.tool_calls = tool_calls;
                        messages.push(msg);
                    }
                }
                "GENERIC" if source == "MODEL" => {
                    let content = v.get("content").and_then(|c| c.as_str()).unwrap_or("");
                    if !content.trim().is_empty() {
                        if let Some(last_asst) =
                            messages.iter_mut().rev().find(|m| m.role == Role::Assistant)
                        {
                            if let Some(tc) =
                                last_asst.tool_calls.iter_mut().find(|tc| tc.output.is_none())
                            {
                                let (clipped, _) = clip(content, MAX_TOOL_IO);
                                tc.output = Some(clipped);
                            }
                        }
                    }
                }
                "ERROR_MESSAGE" => {
                    let content = v.get("content").and_then(|c| c.as_str()).unwrap_or("");
                    if !content.trim().is_empty() {
                        if let Some(last_asst) =
                            messages.iter_mut().rev().find(|m| m.role == Role::Assistant)
                        {
                            if let Some(tc) =
                                last_asst.tool_calls.iter_mut().find(|tc| tc.output.is_none())
                            {
                                let (clipped, _) = clip(content, MAX_TOOL_IO);
                                tc.output = Some(clipped);
                                tc.is_error = true;
                            } else {
                                messages.push(text_msg(Role::System, content, ts));
                            }
                        } else {
                            messages.push(text_msg(Role::System, content, ts));
                        }
                    }
                }
                _ => {}
            }
        }

        if !messages.is_empty() {
            assign_seq(&mut messages);
            Some(messages)
        } else {
            None
        }
    }

    fn parse(&self, r: &SessionFileRef) -> Result<(SessionMeta, Vec<TranscriptMessage>)> {
        let rows = self
            .rows()
            .ok_or_else(|| anyhow!("cannot open antigravity summaries store"))?;
        let row = rows
            .iter()
            .find(|x| x.id == r.native_id)
            .ok_or_else(|| anyhow!("antigravity conversation {} not in store", r.native_id))?;

        let messages = match self.parse_transcript_jsonl(&r.native_id) {
            Some(msgs) if !msgs.is_empty() => msgs,
            _ => {
                let mut text = String::new();
                if !row.preview.trim().is_empty() {
                    text.push_str(row.preview.trim());
                    text.push_str("\n\n");
                }
                text.push_str(
                    "Antigravity stores conversation content encrypted — only this summary is available in Wake.",
                );
                let mut fallback = vec![text_msg(Role::System, &text, row.modified_ms)];
                assign_seq(&mut fallback);
                fallback
            }
        };

        let meta = self.build_meta(r, row, &messages);
        Ok((meta, messages))
    }
}

#[derive(Clone)]
struct AgRow {
    id: String,
    title: String,
    preview: String,
    step_count: i64,
    modified_ms: i64,
    cwd: String,
}

/// 从 Antigravity 的 content 中提取 <USER_REQUEST>…</USER_REQUEST> 内的用户真实输入。
/// 若无标签则剥离末尾的系统元数据块（如 <ADDITIONAL_METADATA>、<USER_SETTINGS_CHANGE>）。
fn extract_user_request(content: &str) -> String {
    const START: &str = "<USER_REQUEST>";
    const END: &str = "</USER_REQUEST>";
    if let Some(s) = content.find(START) {
        if let Some(e) = content[s..].find(END) {
            return content[s + START.len()..s + e].trim().to_string();
        }
    }
    let mut text = content.trim();
    if let Some(pos) = text.find("<ADDITIONAL_METADATA>") {
        text = text[..pos].trim();
    }
    if let Some(pos) = text.find("<USER_SETTINGS_CHANGE>") {
        text = text[..pos].trim();
    }
    text.to_string()
}

/// workspace_uris JSON 数组("[\"file:///Users/…\"]")首项 → 本地路径
fn first_workspace(raw: &str) -> String {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(raw) else {
        return String::new();
    };
    let Some(uri) = v
        .as_array()
        .and_then(|a| a.first())
        .and_then(|x| x.as_str())
    else {
        return String::new();
    };
    let path = uri.strip_prefix("file://").unwrap_or(uri);
    percent_decode(path)
}

/// file:// URI 的最小 percent-decode(路径含空格/中文时是 %XX 编码)
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

impl AgentAdapter for AntigravityAdapter {
    fn agent(&self) -> AgentId {
        AgentId::Antigravity
    }

    fn list_session_files(&self) -> Result<Vec<SessionFileRef>> {
        let Some(rows) = self.rows() else {
            return Ok(Vec::new());
        };
        let brain = self.resolve_brain_dir();
        Ok(rows
            .into_iter()
            .map(|row| {
                let transcript = brain
                    .join(&row.id)
                    .join(".system_generated")
                    .join("logs")
                    .join("transcript.jsonl");
                let (mtime_ms, size) = if let Ok(meta) = std::fs::metadata(&transcript) {
                    let m = super::parse_utils::mtime_ms(&meta);
                    (
                        if m > 0 { m } else { row.modified_ms },
                        meta.len() as i64,
                    )
                } else {
                    (
                        row.modified_ms,
                        (row.title.len() + row.preview.len()) as i64,
                    )
                };
                SessionFileRef {
                    agent: AgentId::Antigravity,
                    native_id: row.id.clone(),
                    file_path: virtual_path(&self.db, &row.id),
                    mtime_ms,
                    size,
                }
            })
            .collect())
    }

    fn quick_meta(&self, refs: &[SessionFileRef]) -> Option<HashMap<String, SessionMeta>> {
        let rows = self.rows()?;
        let by_id: HashMap<&str, &AgRow> = rows.iter().map(|r| (r.id.as_str(), r)).collect();
        let mut out = HashMap::new();
        for r in refs {
            if let Some(row) = by_id.get(r.native_id.as_str()) {
                out.insert(r.file_path.clone(), self.build_meta(r, row, &[]));
            }
        }
        Some(out)
    }

    fn parse_session(&self, r: &SessionFileRef) -> Result<ParsedSession> {
        let (meta, messages) = self.parse(r)?;
        Ok(ParsedSession::derive(meta, &messages, 0))
    }

    fn parse_transcript(&self, r: &SessionFileRef) -> Result<ParsedTranscript> {
        let (meta, messages) = self.parse(r)?;
        Ok(ParsedTranscript {
            meta,
            mainline: messages,
            sidechains: Vec::new(),
            unknown_line_count: 0,
        })
    }

    fn with_custom_root(&self, dir: PathBuf) -> Box<dyn AgentAdapter> {
        let nested_new = dir.join("antigravity").join("conversation_summaries.db");
        let nested_old = dir.join("antigravity-cli").join("conversation_summaries.db");
        let (db, brain_dir) = if dir.is_file() {
            let parent = dir.parent().unwrap_or(&dir);
            (dir.clone(), parent.join("brain"))
        } else if nested_new.is_file() {
            (nested_new, dir.join("antigravity").join("brain"))
        } else if nested_old.is_file() {
            (nested_old, dir.join("antigravity-cli").join("brain"))
        } else {
            (dir.join("conversation_summaries.db"), dir.join("brain"))
        };
        Box::new(Self {
            db,
            brain_dir,
            rows_cache: MtimeCache::new(),
        })
    }

    fn data_roots(&self) -> Vec<PathBuf> {
        vec![self.db.clone()]
    }
}
