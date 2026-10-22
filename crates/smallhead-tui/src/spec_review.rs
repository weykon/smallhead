//! Durable specification review. The caller owns broker coordination and command dispatch.
//!
//! Save after every mutation. Comment submissions retain their original block/revision
//! until `finish_comment`, allowing callers to report conflicts without losing a draft.

use anyhow::{ensure, Context, Result};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{
    layout::Rect,
    style::{Color, Style},
    text::Line,
    widgets::{Block, Borders, Paragraph},
    Frame,
};
use serde::{Deserialize, Serialize};
use smallhead_core::spec_review::SpecDocument as BrokerDocument;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};
use unicode_width::UnicodeWidthChar;

pub const MAX_MARKDOWN_BYTES: usize = smallhead_core::spec_review::MAX_SPEC_BYTES;
pub const MAX_COMMENT_BYTES: usize = 16 * 1024;
pub const MAX_RECORD_BYTES: usize = 8 * 1024 * 1024;
const MAX_COMMENTS: usize = 4096;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SpecBlock {
    pub id: u64,
    pub markdown: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SpecComment {
    pub id: u64,
    pub revision: u64,
    pub block_id: u64,
    /// Snapshot keeps old comments understandable after a block is replaced.
    pub block_markdown: String,
    pub text: String,
    pub resolved: bool,
    pub replies: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SpecDocument {
    pub id: String,
    pub title: String,
    pub markdown: String,
    pub revision: u64,
    pub approved_revision: Option<u64>,
    pub blocks: Vec<SpecBlock>,
    pub comments: Vec<SpecComment>,
    next_block_id: u64,
    next_comment_id: u64,
    /// Storage generation also changes for comment-only edits.
    generation: u64,
}

impl SpecDocument {
    pub fn from_document(doc: &BrokerDocument) -> Result<Self> {
        validate_source(doc)?;
        let mut result = Self {
            id: doc.id.clone(),
            title: doc.title.clone(),
            markdown: doc.markdown.clone(),
            revision: doc.revision,
            approved_revision: doc.approved_revision,
            blocks: Vec::new(),
            comments: Vec::new(),
            next_block_id: 1,
            next_comment_id: 1,
            generation: 0,
        };
        result.blocks = result.match_blocks(&doc.markdown)?;
        Ok(result)
    }

    #[cfg(test)]
    pub fn from_markdown(id: &str, title: &str, markdown: &str) -> Result<Self> {
        Self::from_document(&BrokerDocument {
            id: id.into(),
            title: title.into(),
            markdown: markdown.into(),
            revision: 1,
            approved_revision: None,
        })
    }

    #[cfg(test)]
    pub fn from_path(id: &str, title: &str, path: impl AsRef<Path>) -> Result<Self> {
        Self::from_markdown(id, title, &read_markdown(path)?)
    }

    pub fn to_document(&self) -> BrokerDocument {
        BrokerDocument {
            id: self.id.clone(),
            title: self.title.clone(),
            markdown: self.markdown.clone(),
            revision: self.revision,
            approved_revision: self.approved_revision,
        }
    }

    /// Broker revisions are authoritative, including skipped or identical-content revisions.
    pub fn sync_from_document(&mut self, doc: &BrokerDocument) -> Result<bool> {
        validate_source(doc)?;
        ensure!(doc.id == self.id, "spec document identity mismatch");
        ensure!(
            doc.revision >= self.revision,
            "stale spec revision {} (current {})",
            doc.revision,
            self.revision
        );
        if doc.revision == self.revision {
            ensure!(
                doc.title == self.title && doc.markdown == self.markdown,
                "conflicting contents for spec revision {}",
                self.revision
            );
            let changed = self.approved_revision != doc.approved_revision;
            self.approved_revision = doc.approved_revision;
            return Ok(changed);
        }
        let blocks = self.match_blocks(&doc.markdown)?;
        self.blocks = blocks;
        self.markdown = doc.markdown.clone();
        self.title = doc.title.clone();
        self.revision = doc.revision;
        // Approval may only arrive through an exact-revision broker checkpoint.
        self.approved_revision = doc.approved_revision;
        Ok(true)
    }

    #[cfg(test)]
    pub fn replace_markdown(&mut self, expected_revision: u64, markdown: &str) -> Result<bool> {
        self.check_revision(expected_revision)?;
        let revision = self
            .revision
            .checked_add(1)
            .context("spec revision exhausted")?;
        self.sync_from_document(&BrokerDocument {
            id: self.id.clone(),
            title: self.title.clone(),
            markdown: markdown.into(),
            revision,
            approved_revision: None,
        })
    }

    #[cfg(test)]
    pub fn replace_from_path(
        &mut self,
        expected_revision: u64,
        path: impl AsRef<Path>,
    ) -> Result<bool> {
        self.check_revision(expected_revision)?;
        self.replace_markdown(expected_revision, &read_markdown(path)?)
    }

    pub fn add_comment(&mut self, revision: u64, block_id: u64, text: &str) -> Result<u64> {
        self.check_revision(revision)?;
        validate_comment(text)?;
        ensure!(self.comments.len() < MAX_COMMENTS, "too many spec comments");
        let block = self
            .blocks
            .iter()
            .find(|b| b.id == block_id)
            .context("spec block not found")?;
        let id = self.next_comment_id;
        let next = id.checked_add(1).context("comment ids exhausted")?;
        self.comments.push(SpecComment {
            id,
            revision,
            block_id,
            block_markdown: block.markdown.clone(),
            text: text.trim().into(),
            resolved: false,
            replies: Vec::new(),
        });
        self.next_comment_id = next;
        self.approved_revision = None;
        Ok(id)
    }

    pub fn reply(&mut self, expected_revision: u64, comment_id: u64, text: &str) -> Result<()> {
        self.check_revision(expected_revision)?;
        validate_comment(text)?;
        let comment = self.comment_mut(comment_id)?;
        ensure!(
            comment.replies.len() < MAX_COMMENTS,
            "too many comment replies"
        );
        comment.replies.push(text.trim().into());
        Ok(())
    }

    #[cfg(test)]
    pub fn resolve_comment(
        &mut self,
        expected_revision: u64,
        comment_id: u64,
        resolved: bool,
    ) -> Result<()> {
        self.check_revision(expected_revision)?;
        self.comment_mut(comment_id)?.resolved = resolved;
        Ok(())
    }

    #[cfg(test)]
    pub fn approve(&mut self, revision: u64) -> Result<()> {
        self.check_revision(revision)?;
        self.approved_revision = Some(revision);
        Ok(())
    }

    pub fn is_approved(&self) -> bool {
        self.approved_revision == Some(self.revision)
    }

    pub fn comment_message(&self, comment_id: u64) -> Result<String> {
        let comment = self
            .comments
            .iter()
            .find(|c| c.id == comment_id)
            .context("spec comment not found")?;
        Ok(format!("Specification review comment (not approval). Revise with update_spec using the current document id and expected_revision; do not implement until explicitly approved.\n{}",
            serde_json::to_string(&serde_json::json!({
                "document_id": self.id, "current_revision": self.revision, "comment": comment,
            }))?))
    }

    pub fn approval_message(&self) -> Result<String> {
        ensure!(self.is_approved(), "current spec revision is not approved");
        Ok(format!("Specification approved for implementation, exclusively for this document id and revision. Any further revision requires fresh approval.\n{}",
            serde_json::to_string(&serde_json::json!({"document_id": self.id, "revision": self.revision, "title": self.title}))?))
    }

    fn check_revision(&self, revision: u64) -> Result<()> {
        ensure!(
            revision == self.revision,
            "stale spec revision {revision} (current {})",
            self.revision
        );
        Ok(())
    }

    fn comment_mut(&mut self, id: u64) -> Result<&mut SpecComment> {
        self.comments
            .iter_mut()
            .find(|c| c.id == id)
            .context("spec comment not found")
    }

    fn match_blocks(&mut self, markdown: &str) -> Result<Vec<SpecBlock>> {
        let mut existing: HashMap<&str, VecDeque<u64>> = HashMap::new();
        for block in &self.blocks {
            existing
                .entry(&block.markdown)
                .or_default()
                .push_back(block.id);
        }
        let mut next = self.next_block_id;
        let mut result = Vec::new();
        for text in split_blocks(markdown) {
            let id = match existing
                .get_mut(text.as_str())
                .and_then(VecDeque::pop_front)
            {
                Some(id) => id,
                None => {
                    let id = next;
                    next = next.checked_add(1).context("block ids exhausted")?;
                    id
                }
            };
            result.push(SpecBlock { id, markdown: text });
        }
        self.next_block_id = next;
        Ok(result)
    }

    fn validate(&self) -> Result<()> {
        validate_source(&self.to_document())?;
        let parsed = split_blocks(&self.markdown);
        ensure!(
            parsed.len() == self.blocks.len()
                && parsed
                    .iter()
                    .zip(&self.blocks)
                    .all(|(a, b)| a == &b.markdown),
            "invalid spec blocks"
        );
        let mut ids = HashSet::new();
        ensure!(
            self.next_block_id > 0 && self.next_comment_id > 0,
            "invalid next id"
        );
        for block in &self.blocks {
            ensure!(
                block.id > 0 && block.id < self.next_block_id && ids.insert(block.id),
                "invalid block id"
            );
        }
        ids.clear();
        ensure!(
            self.comments.len() <= MAX_COMMENTS,
            "too many spec comments"
        );
        for comment in &self.comments {
            ensure!(
                comment.id > 0 && comment.id < self.next_comment_id && ids.insert(comment.id),
                "invalid comment id"
            );
            ensure!(
                comment.revision > 0
                    && comment.revision <= self.revision
                    && comment.block_id > 0
                    && comment.block_id < self.next_block_id,
                "invalid comment anchor"
            );
            ensure!(
                comment.block_markdown.len() <= MAX_MARKDOWN_BYTES
                    && !comment.block_markdown.trim().is_empty(),
                "invalid comment snapshot"
            );
            if comment.revision == self.revision {
                ensure!(
                    self.blocks
                        .iter()
                        .any(|b| b.id == comment.block_id && b.markdown == comment.block_markdown),
                    "invalid current comment anchor"
                );
            }
            validate_comment(&comment.text)?;
            ensure!(comment.replies.len() <= MAX_COMMENTS, "too many replies");
            for reply in &comment.replies {
                validate_comment(reply)?;
            }
        }
        Ok(())
    }
}

fn validate_source(doc: &BrokerDocument) -> Result<()> {
    validate_id(&doc.id)?;
    ensure!(
        !doc.title.trim().is_empty() && doc.title.len() <= 160,
        "spec title must contain 1..160 bytes"
    );
    ensure!(
        !doc.markdown.trim().is_empty() && doc.markdown.len() <= MAX_MARKDOWN_BYTES,
        "spec Markdown must contain 1..{MAX_MARKDOWN_BYTES} bytes"
    );
    ensure!(
        doc.revision > 0 && doc.approved_revision.is_none_or(|r| r == doc.revision),
        "invalid spec revision or approval"
    );
    Ok(())
}

fn validate_id(id: &str) -> Result<()> {
    ensure!(
        !id.is_empty()
            && id.len() <= 128
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
        "spec id must be 1..128 ASCII letters, digits, '-' or '_'"
    );
    Ok(())
}

fn validate_comment(text: &str) -> Result<()> {
    ensure!(
        !text.trim().is_empty() && text.len() <= MAX_COMMENT_BYTES,
        "comment must contain 1..{MAX_COMMENT_BYTES} bytes"
    );
    Ok(())
}

/// Paragraph blocks, with headings separated and fenced code kept intact.
fn split_blocks(markdown: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut current = Vec::new();
    let mut fence: Option<(char, usize)> = None;
    for line in markdown.lines() {
        let trimmed = line.trim_start();
        let first = trimmed.chars().next().unwrap_or(' ');
        let count = trimmed.chars().take_while(|c| *c == first).count();
        let marker = (first == '`' || first == '~') && count >= 3;
        if let Some((kind, length)) = fence {
            current.push(line);
            if marker && first == kind && count >= length && trimmed[count..].trim().is_empty() {
                fence = None;
                blocks.push(current.join("\n"));
                current.clear();
            }
        } else if marker {
            if !current.is_empty() {
                blocks.push(current.join("\n"));
                current.clear();
            }
            fence = Some((first, count));
            current.push(line);
        } else if line.trim().is_empty() {
            if !current.is_empty() {
                blocks.push(current.join("\n"));
                current.clear();
            }
        } else if trimmed.starts_with('#') && trimmed.trim_start_matches('#').starts_with(' ') {
            if !current.is_empty() {
                blocks.push(current.join("\n"));
                current.clear();
            }
            blocks.push(line.into());
        } else {
            current.push(line);
        }
    }
    if !current.is_empty() {
        blocks.push(current.join("\n"));
    }
    blocks
}

/// Read a UTF-8 regular file, bounded even if it grows after metadata inspection.
pub fn read_markdown(path: impl AsRef<Path>) -> Result<String> {
    let text = read_bounded(path.as_ref(), MAX_MARKDOWN_BYTES)?;
    ensure!(!text.trim().is_empty(), "spec Markdown is empty");
    Ok(text)
}

fn read_bounded(path: &Path, limit: usize) -> Result<String> {
    ensure!(
        fs::metadata(path)
            .with_context(|| format!("inspect {}", path.display()))?
            .is_file(),
        "expected a regular file: {}",
        path.display()
    );
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file() && metadata.len() <= limit as u64,
        "file is not regular or exceeds {limit} bytes"
    );
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= limit, "file exceeds {limit} bytes");
    String::from_utf8(bytes).context("file is not UTF-8")
}

#[derive(Clone, Debug)]
pub struct SpecStore {
    directory: PathBuf,
}

impl SpecStore {
    pub fn new(root: impl AsRef<Path>) -> Self {
        Self {
            directory: root.as_ref().join("specs"),
        }
    }

    /// Restore the last successfully saved document without requiring its identity.
    pub fn load_current(&self) -> Result<Option<SpecDocument>> {
        let path = self.directory.join(".current.json");
        match fs::symlink_metadata(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
            Ok(meta) => ensure!(
                meta.is_file(),
                "current spec pointer must be a regular file"
            ),
        }
        let id: String = serde_json::from_str(&read_bounded(&path, 256)?)
            .context("invalid current spec pointer")?;
        validate_id(&id)?;
        let doc = self.load(&id)?.context("current spec record is missing")?;
        Ok(Some(doc))
    }

    pub fn load(&self, id: &str) -> Result<Option<SpecDocument>> {
        validate_id(id)?;
        let path = self.directory.join(format!("{id}.json"));
        match fs::symlink_metadata(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
            Ok(meta) => ensure!(meta.is_file(), "spec record must be a regular file"),
        }
        let doc: SpecDocument = serde_json::from_str(&read_bounded(&path, MAX_RECORD_BYTES)?)
            .context("invalid persisted spec record")?;
        ensure!(
            doc.id == id && doc.generation > 0,
            "invalid persisted spec identity or generation"
        );
        doc.validate()?;
        Ok(Some(doc))
    }

    /// Atomic replacement with a generation check and exclusive per-document writer lock.
    /// The lock file stays in place; the guard explicitly unlocks on return.
    pub fn save(&self, doc: &mut SpecDocument) -> Result<()> {
        doc.validate()?;
        fs::create_dir_all(&self.directory)?;
        let lock_path = self.directory.join(format!("{}.lock", doc.id));
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .context("open spec writer lock")?;
        fs4::FileExt::try_lock_exclusive(&lock).context("spec is locked by another writer")?;
        // Never unlink a lock file: another writer may already hold its inode open.
        let _lock = SpecWriterLock(lock);
        let stored = self.load(&doc.id)?;
        let mut checkpoint = doc.clone();
        checkpoint.generation = doc
            .generation
            .checked_add(1)
            .context("storage generation exhausted")?;
        // A prior attempt may have committed the record but failed its pointer or sync.
        // Only an exact next-generation checkpoint can complete that same save.
        let retry = stored.as_ref() == Some(&checkpoint);
        ensure!(
            stored.as_ref().map_or(0, |s| s.generation) == doc.generation || retry,
            "stale persisted spec; reload before saving"
        );
        if let Some(stored) = &stored {
            ensure!(
                doc.revision >= stored.revision,
                "cannot save an older spec revision"
            );
            if doc.revision == stored.revision {
                ensure!(
                    doc.markdown == stored.markdown
                        && doc.title == stored.title
                        && doc.blocks == stored.blocks,
                    "contents changed without a new revision"
                );
            }
        }
        let bytes = serde_json::to_vec_pretty(&checkpoint)?;
        ensure!(
            bytes.len() <= MAX_RECORD_BYTES,
            "spec record exceeds {MAX_RECORD_BYTES} bytes"
        );
        if !retry {
            atomic_write(&self.directory, &format!("{}.json", doc.id), &bytes)?;
        }
        atomic_write(
            &self.directory,
            ".current.json",
            &serde_json::to_vec(&doc.id)?,
        )?;
        File::open(&self.directory)?.sync_all()?;
        doc.generation = checkpoint.generation;
        Ok(())
    }
}

fn atomic_write(directory: &Path, name: &str, bytes: &[u8]) -> Result<()> {
    static NONCE: AtomicU64 = AtomicU64::new(0);
    let temp_path = directory.join(format!(
        ".{name}.{}-{}.tmp",
        std::process::id(),
        NONCE.fetch_add(1, Ordering::Relaxed)
    ));
    let mut temp = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temp_path)?;
    let _temp_guard = RemoveOnDrop(temp_path.clone());
    temp.write_all(bytes)?;
    temp.sync_all()?;
    drop(temp);
    fs::rename(&temp_path, directory.join(name))?;
    Ok(())
}

struct SpecWriterLock(File);
impl Drop for SpecWriterLock {
    fn drop(&mut self) {
        // A forked child can temporarily retain the open file description even
        // with CLOEXEC. Closing only our descriptor may then delay lock release.
        let _ = fs4::FileExt::unlock(&self.0);
    }
}

struct RemoveOnDrop(PathBuf);
impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

#[derive(Clone, Debug, Default)]
pub struct SpecViewState {
    pub selected: usize,
    pub scroll: usize,
    pub input: Option<String>,
    pub input_revision: Option<u64>,
    pub input_block: Option<u64>,
    follow_selection: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SpecAction {
    None,
    CommentSubmitted(String),
    ApproveRequested,
    ReloadRequested,
    Back,
}

impl SpecViewState {
    pub fn selected_block(&self, doc: &SpecDocument) -> Option<u64> {
        doc.blocks
            .get(self.selected.min(doc.blocks.len().saturating_sub(1)))
            .map(|b| b.id)
    }

    pub fn finish_comment(&mut self) {
        self.input = None;
        self.input_revision = None;
        self.input_block = None;
    }

    pub fn handle_key(&mut self, key: KeyEvent, doc: &SpecDocument) -> SpecAction {
        if key.kind == KeyEventKind::Release {
            return SpecAction::None;
        }
        if let Some(input) = &mut self.input {
            match key.code {
                KeyCode::Esc => self.finish_comment(),
                KeyCode::Enter if !input.trim().is_empty() => {
                    return SpecAction::CommentSubmitted(input.trim().into())
                }
                KeyCode::Backspace => {
                    input.pop();
                }
                KeyCode::Char(c)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                        && !c.is_control()
                        && input.len() + c.len_utf8() <= MAX_COMMENT_BYTES =>
                {
                    input.push(c)
                }
                _ => {}
            }
            return SpecAction::None;
        }
        self.selected = self.selected.min(doc.blocks.len().saturating_sub(1));
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.selected = self.selected.saturating_sub(1);
                self.follow_selection = true;
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.selected = (self.selected + 1).min(doc.blocks.len().saturating_sub(1));
                self.follow_selection = true;
            }
            KeyCode::PageUp => {
                self.scroll = self.scroll.saturating_sub(5);
                self.follow_selection = false;
            }
            KeyCode::PageDown => {
                self.scroll = self.scroll.saturating_add(5);
                self.follow_selection = false;
            }
            KeyCode::Home => {
                self.selected = 0;
                self.scroll = 0;
                self.follow_selection = true;
            }
            KeyCode::End => {
                self.selected = doc.blocks.len().saturating_sub(1);
                self.follow_selection = true;
            }
            KeyCode::Char('c') => {
                if let Some(id) = self.selected_block(doc) {
                    self.input = Some(String::new());
                    self.input_revision = Some(doc.revision);
                    self.input_block = Some(id);
                }
            }
            KeyCode::Char('a') => return SpecAction::ApproveRequested,
            KeyCode::Char('r') => return SpecAction::ReloadRequested,
            KeyCode::Esc | KeyCode::Char('q') => return SpecAction::Back,
            _ => {}
        }
        SpecAction::None
    }
}

/// Standalone view; manual wrapping keeps scroll offsets accurate for narrow terminals.
pub fn draw(f: &mut Frame, area: Rect, doc: &SpecDocument, state: &mut SpecViewState) {
    let border = Block::default().borders(Borders::ALL).title(format!(
        "Spec r{} {}",
        doc.revision,
        if doc.is_approved() {
            "APPROVED"
        } else {
            "REVIEW"
        }
    ));
    let inner = border.inner(area);
    f.render_widget(border, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    state.selected = state.selected.min(doc.blocks.len().saturating_sub(1));
    let input_height = if state.input.is_some() {
        inner.height.min(4)
    } else {
        0
    };
    let body_height = inner.height.saturating_sub(input_height);
    let mut lines: Vec<Line<'static>> = Vec::new();
    append_wrapped(&mut lines, &doc.title, inner.width, Color::White);
    let mut selected_start = 0;
    for (index, block) in doc.blocks.iter().enumerate() {
        let selected = index == state.selected;
        if selected {
            selected_start = lines.len();
        }
        let color = if selected { Color::Cyan } else { Color::White };
        append_wrapped(
            &mut lines,
            &format!("{} Block {}", if selected { ">" } else { " " }, block.id),
            inner.width,
            color,
        );
        append_wrapped(&mut lines, &block.markdown, inner.width, color);
        for comment in doc.comments.iter().filter(|c| c.block_id == block.id) {
            render_comment(&mut lines, comment, inner.width);
        }
        lines.push(Line::raw(""));
    }
    let orphaned: Vec<_> = doc
        .comments
        .iter()
        .filter(|c| !doc.blocks.iter().any(|b| b.id == c.block_id))
        .collect();
    if !orphaned.is_empty() {
        append_wrapped(
            &mut lines,
            "Comments on earlier blocks",
            inner.width,
            Color::Yellow,
        );
        for comment in orphaned {
            append_wrapped(
                &mut lines,
                &comment.block_markdown,
                inner.width,
                Color::DarkGray,
            );
            render_comment(&mut lines, comment, inner.width);
        }
    }
    let height = body_height as usize;
    if state.follow_selection && height > 0 {
        if selected_start < state.scroll || selected_start >= state.scroll.saturating_add(height) {
            state.scroll = selected_start;
        }
        state.follow_selection = false;
    }
    state.scroll = state.scroll.min(lines.len().saturating_sub(height));
    let visible: Vec<_> = lines.into_iter().skip(state.scroll).take(height).collect();
    f.render_widget(
        Paragraph::new(visible),
        Rect {
            height: body_height,
            ..inner
        },
    );
    if let Some(input) = &state.input {
        let mut input_lines = Vec::new();
        append_wrapped(
            &mut input_lines,
            &format!(
                "Comment r{} / block {}",
                state.input_revision.unwrap_or(doc.revision),
                state.input_block.unwrap_or(0)
            ),
            inner.width,
            Color::Yellow,
        );
        let header = input_lines.remove(0);
        input_lines.clear();
        append_wrapped(
            &mut input_lines,
            &format!("{input}_"),
            inner.width,
            Color::White,
        );
        let skip = input_lines
            .len()
            .saturating_sub(input_height.saturating_sub(1) as usize);
        let visible: Vec<_> = std::iter::once(header)
            .chain(input_lines.into_iter().skip(skip))
            .take(input_height as usize)
            .collect();
        f.render_widget(
            Paragraph::new(visible),
            Rect {
                y: inner.y + body_height,
                height: input_height,
                ..inner
            },
        );
    }
}

fn render_comment(lines: &mut Vec<Line<'static>>, comment: &SpecComment, width: u16) {
    append_wrapped(
        lines,
        &format!(
            "[{}] Comment {} r{}: {}",
            if comment.resolved { "resolved" } else { "open" },
            comment.id,
            comment.revision,
            comment.text
        ),
        width,
        Color::Yellow,
    );
    for reply in &comment.replies {
        append_wrapped(lines, &format!("Reply: {reply}"), width, Color::Gray);
    }
}

fn append_wrapped(lines: &mut Vec<Line<'static>>, text: &str, width: u16, color: Color) {
    let width = usize::from(width).max(1);
    for source in text.split('\n') {
        let mut current = String::new();
        let mut used = 0;
        let clean: String = source
            .chars()
            .filter(|c| !c.is_control() || *c == '\t')
            .map(|c| if c == '\t' { ' ' } else { c })
            .collect();
        for word in clean.split_inclusive(' ') {
            let word_width: usize = word
                .trim_end()
                .chars()
                .map(|c| c.width().unwrap_or(0))
                .sum();
            if word_width <= width && used + word_width > width && !current.is_empty() {
                lines.push(Line::styled(
                    std::mem::take(&mut current),
                    Style::default().fg(color),
                ));
                used = 0;
            }
            for c in word.chars() {
                let size = c.width().unwrap_or(0);
                if used + size > width && !current.is_empty() {
                    lines.push(Line::styled(
                        std::mem::take(&mut current),
                        Style::default().fg(color),
                    ));
                    used = 0;
                    if c == ' ' {
                        continue;
                    }
                }
                if size > width {
                    current.push('?');
                    used += 1;
                } else {
                    current.push(c);
                    used += size;
                }
            }
        }
        lines.push(Line::styled(current, Style::default().fg(color)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};

    fn document() -> SpecDocument {
        SpecDocument::from_markdown(
            "test-spec",
            "Review",
            "# Design\n\nKeep this paragraph.\n\nChange this paragraph.",
        )
        .unwrap()
    }

    #[test]
    fn revision_changes_preserve_anchors_and_invalidate_approval() {
        let mut doc = document();
        let kept = doc.blocks[1].id;
        let removed = doc.blocks[2].id;
        let comment = doc.add_comment(1, removed, "Please clarify.").unwrap();
        doc.approve(1).unwrap();
        assert!(doc.approval_message().unwrap().contains("\"revision\":1"));
        doc.replace_markdown(
            1,
            "# Design\n\nInserted.\n\nKeep this paragraph.\n\nChanged.",
        )
        .unwrap();
        assert_eq!(doc.revision, 2);
        assert_eq!(doc.blocks[2].id, kept);
        assert!(!doc.blocks.iter().any(|b| b.id == removed));
        assert!(!doc.is_approved());
        assert!(doc.approval_message().is_err());
        assert_eq!(doc.comments[0].revision, 1);
        assert_eq!(doc.comments[0].block_markdown, "Change this paragraph.");
        assert!(doc
            .comment_message(comment)
            .unwrap()
            .contains("not approval"));
        let before = doc.clone();
        assert!(doc.replace_markdown(1, "stale").is_err());
        assert!(doc.add_comment(1, kept, "stale").is_err());
        assert!(doc.approve(1).is_err());
        assert_eq!(doc, before);
    }

    #[test]
    fn broker_sync_preserves_exact_revision_and_rejects_conflicts() {
        let mut doc = document();
        let mut broker = doc.to_document();
        broker.revision = 37;
        doc.sync_from_document(&broker).unwrap();
        assert_eq!(doc.revision, 37);
        broker.approved_revision = Some(37);
        assert!(doc.sync_from_document(&broker).unwrap());
        assert!(doc.is_approved());
        assert!(!doc.sync_from_document(&broker).unwrap());
        let before = doc.clone();
        broker.markdown = "different".into();
        assert!(doc.sync_from_document(&broker).is_err());
        broker.revision = 36;
        assert!(doc.sync_from_document(&broker).is_err());
        broker = before.to_document();
        broker.id = "other".into();
        assert!(doc.sync_from_document(&broker).is_err());
        assert_eq!(doc, before);
        broker = before.to_document();
        broker.revision = 38;
        broker.approved_revision = None;
        doc.sync_from_document(&broker).unwrap();
        assert!(!doc.is_approved());
        assert_eq!(doc.blocks, before.blocks);
    }

    #[test]
    fn comments_revoke_approval_and_support_replies_and_resolution() {
        let mut doc = document();
        doc.approve(1).unwrap();
        assert!(doc.add_comment(1, 999, "missing block").is_err());
        assert!(doc.add_comment(1, doc.blocks[0].id, " ").is_err());
        assert!(doc.is_approved());
        let id = doc.add_comment(1, doc.blocks[0].id, "Question").unwrap();
        assert!(!doc.is_approved());
        doc.reply(1, id, "Answer").unwrap();
        doc.resolve_comment(1, id, true).unwrap();
        assert_eq!(doc.comments[0].replies, ["Answer"]);
        assert!(doc.comments[0].resolved);
        doc.replace_markdown(1, "Updated").unwrap();
        assert!(doc.reply(1, id, "stale").is_err());
        assert!(doc.resolve_comment(1, id, false).is_err());
        doc.resolve_comment(2, id, false).unwrap();
        doc.validate().unwrap();
    }

    #[test]
    fn duplicate_blocks_are_unique_and_fences_remain_whole() {
        let mut doc = SpecDocument::from_markdown(
            "duplicates",
            "Duplicates",
            "same\n\nsame\n\n```rust\nlet x = 1;\n\n# inside\n```\n\nlast",
        )
        .unwrap();
        assert_eq!(doc.blocks.len(), 4);
        let ids: Vec<_> = doc.blocks.iter().map(|b| b.id).collect();
        assert_ne!(ids[0], ids[1]);
        assert!(doc.blocks[2].markdown.contains("\n\n# inside"));
        doc.replace_markdown(
            1,
            "last\n\nsame\n\nsame\n\n```rust\nlet x = 1;\n\n# inside\n```",
        )
        .unwrap();
        assert_eq!(
            doc.blocks.iter().map(|b| b.id).collect::<Vec<_>>(),
            [ids[3], ids[0], ids[1], ids[2]]
        );
    }

    #[test]
    fn persistence_roundtrip_current_pointer_and_stale_writer() {
        let dir = tempfile::tempdir().unwrap();
        let store = SpecStore::new(dir.path());
        assert!(store.load_current().unwrap().is_none());
        let mut doc = document();
        let id = doc.add_comment(1, doc.blocks[0].id, "Question").unwrap();
        doc.reply(1, id, "Answer").unwrap();
        doc.approve(1).unwrap();
        store.save(&mut doc).unwrap();
        assert_eq!(store.load_current().unwrap(), Some(doc.clone()));
        let mut stale = store.load(&doc.id).unwrap().unwrap();
        doc.resolve_comment(1, id, true).unwrap();
        store.save(&mut doc).unwrap();
        stale.resolve_comment(1, id, false).unwrap();
        assert!(store.save(&mut stale).is_err());
        assert_eq!(store.load(&doc.id).unwrap(), Some(doc));
        let mut other =
            SpecDocument::from_markdown("current", "Other", "A different spec").unwrap();
        store.save(&mut other).unwrap();
        assert_eq!(store.load_current().unwrap(), Some(other));
        assert!(dir.path().join("specs/test-spec.lock").exists());
        assert!(fs::read_dir(dir.path().join("specs")).unwrap().all(|e| !e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .ends_with(".tmp")));
    }

    #[test]
    fn failed_pointer_write_retries_exact_checkpoint_without_losing_conflict_checks() {
        for existing in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let store = SpecStore::new(dir.path());
            let mut desired = document();
            if existing {
                store.save(&mut desired).unwrap();
                desired.replace_markdown(1, "Updated document").unwrap();
                fs::remove_file(dir.path().join("specs/.current.json")).unwrap();
            }
            desired
                .add_comment(desired.revision, desired.blocks[0].id, "Keep this comment")
                .unwrap();
            let original = desired.clone();
            let pointer = dir.path().join("specs/.current.json");
            fs::create_dir_all(&pointer).unwrap();
            assert!(store.save(&mut desired).is_err());
            assert_eq!(desired, original);
            let committed = store.load(&desired.id).unwrap().unwrap();
            assert_eq!(committed.generation, original.generation + 1);
            assert!(store.save(&mut desired).is_err());
            assert_eq!(desired, original);
            fs::remove_dir(&pointer).unwrap();

            let mut different_comment = original.clone();
            different_comment
                .reply(desired.revision, 1, "Different reply")
                .unwrap();
            assert!(store.save(&mut different_comment).is_err());
            let mut different_content = original.clone();
            different_content
                .replace_markdown(desired.revision, "Conflicting content")
                .unwrap();
            assert!(store.save(&mut different_content).is_err());
            assert!(!pointer.exists());
            assert_eq!(store.load(&desired.id).unwrap(), Some(committed.clone()));

            // App discards the failed mutation clone and retries its original record.
            let mut retry = original;
            store.save(&mut retry).unwrap();
            assert_eq!(retry, committed);
            assert_eq!(store.load_current().unwrap(), Some(retry));
        }
    }

    #[cfg(unix)]
    #[test]
    fn writer_lock_release_is_not_delayed_by_an_inherited_descriptor() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("writer.lock");
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        fs4::FileExt::try_lock_exclusive(&lock).unwrap();
        let inherited = lock.try_clone().unwrap();
        drop(SpecWriterLock(lock));
        let next_writer = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        fs4::FileExt::try_lock_exclusive(&next_writer)
            .expect("prior writer must release even while its descriptor copy is alive");
        drop(SpecWriterLock(next_writer));
        drop(inherited);
    }

    #[test]
    fn validation_rejects_bad_paths_oversize_and_corrupt_records() {
        let dir = tempfile::tempdir().unwrap();
        let store = SpecStore::new(dir.path());
        assert!(store.load("../escape").is_err());
        assert!(store.load("/absolute").is_err());
        assert!(read_markdown(dir.path()).is_err());
        let path = dir.path().join("input.md");
        fs::write(&path, [0xff]).unwrap();
        assert!(read_markdown(&path).is_err());
        fs::write(&path, "x".repeat(MAX_MARKDOWN_BYTES + 1)).unwrap();
        assert!(read_markdown(&path).is_err());
        fs::write(&path, "# Good\n\nContent").unwrap();
        assert_eq!(read_markdown(&path).unwrap(), "# Good\n\nContent");
        let mut imported = SpecDocument::from_path("imported", "Imported", &path).unwrap();
        fs::write(&path, "Updated from disk").unwrap();
        imported.replace_from_path(1, &path).unwrap();
        assert_eq!(imported.revision, 2);
        assert_eq!(imported.markdown, "Updated from disk");
        let mut doc = document();
        store.save(&mut doc).unwrap();
        fs::write(dir.path().join("specs/.current.json"), "\"../escape\"").unwrap();
        assert!(store.load_current().is_err());
        fs::write(dir.path().join("specs/test-spec.json"), "{corrupt").unwrap();
        assert!(store.load("test-spec").is_err());
    }

    #[test]
    fn concurrent_saves_never_lose_a_winning_update() {
        let dir = tempfile::tempdir().unwrap();
        let store = SpecStore::new(dir.path());
        let mut doc = document();
        store.save(&mut doc).unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let handles: Vec<_> = (0..2)
            .map(|i| {
                let store = store.clone();
                let mut doc = doc.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    doc.add_comment(1, doc.blocks[0].id, &format!("comment {i}"))
                        .unwrap();
                    barrier.wait();
                    store.save(&mut doc).is_ok()
                })
            })
            .collect();
        assert_eq!(
            handles
                .into_iter()
                .map(|h| usize::from(h.join().unwrap()))
                .sum::<usize>(),
            1
        );
        assert_eq!(store.load_current().unwrap().unwrap().comments.len(), 1);
    }

    #[test]
    fn advisory_lock_blocks_live_writer_and_recovers_after_process_exit() {
        let dir = tempfile::tempdir().unwrap();
        let store = SpecStore::new(dir.path());
        let mut doc = document();
        store.save(&mut doc).unwrap();
        let lock_path = dir.path().join("specs/test-spec.lock");
        let live_lock = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&lock_path)
            .unwrap();
        fs4::FileExt::try_lock_exclusive(&live_lock).unwrap();
        let live_lock = SpecWriterLock(live_lock);
        assert!(store.save(&mut doc).is_err());
        drop(live_lock);
        store.save(&mut doc).unwrap();
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["spec_advisory_lock_exit_child", "--ignored"])
            .env("SMALLHEAD_SPEC_TEST_LOCK", &lock_path)
            .status()
            .unwrap();
        assert!(child.success());
        assert!(lock_path.exists());
        store.save(&mut doc).unwrap();
        assert_eq!(store.load_current().unwrap(), Some(doc));
    }

    #[test]
    #[ignore = "subprocess helper for advisory lock crash recovery"]
    fn spec_advisory_lock_exit_child() {
        let path = std::env::var_os("SMALLHEAD_SPEC_TEST_LOCK").expect("parent sets lock path");
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .unwrap();
        fs4::FileExt::try_lock_exclusive(&lock).unwrap();
        // Exit without Rust destructors, just as a terminated writer would.
        std::process::exit(0);
    }

    #[test]
    fn input_captures_revision_and_preserves_draft_on_submission() {
        let mut doc = document();
        let mut state = SpecViewState::default();
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        state.handle_key(key(KeyCode::Down), &doc);
        let anchor = state.selected_block(&doc).unwrap();
        state.handle_key(key(KeyCode::Char('c')), &doc);
        for c in "Please change".chars() {
            state.handle_key(key(KeyCode::Char(c)), &doc);
        }
        doc.replace_markdown(1, "New blocks").unwrap();
        assert_eq!(
            state.handle_key(key(KeyCode::Enter), &doc),
            SpecAction::CommentSubmitted("Please change".into())
        );
        assert_eq!(state.input_revision, Some(1));
        assert_eq!(state.input_block, Some(anchor));
        assert!(state.input.is_some());
        assert!(doc
            .add_comment(
                state.input_revision.unwrap(),
                state.input_block.unwrap(),
                state.input.as_deref().unwrap()
            )
            .is_err());
        assert!(doc.comments.is_empty());
        assert_eq!(state.input.as_deref(), Some("Please change"));
        state.finish_comment();
        assert!(state.input.is_none());
        assert_eq!(
            state.handle_key(key(KeyCode::Char('a')), &doc),
            SpecAction::ApproveRequested
        );
        assert_eq!(
            state.handle_key(key(KeyCode::Char('r')), &doc),
            SpecAction::ReloadRequested
        );
        assert_eq!(state.handle_key(key(KeyCode::Esc), &doc), SpecAction::Back);
    }

    #[test]
    fn render_narrow_unicode_comments_and_tiny_areas() {
        let mut doc = SpecDocument::from_markdown("narrow", "Review", "# Design\n\nA long paragraph with Unicode: \u{4e2d}\u{6587} and averylongunbrokenword.\n\nLast block").unwrap();
        doc.add_comment(1, doc.blocks[0].id, "Please clarify")
            .unwrap();
        for (width, height) in [(1, 1), (3, 3), (12, 8), (32, 16), (80, 24)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            let mut state = SpecViewState::default();
            terminal
                .draw(|f| draw(f, f.area(), &doc, &mut state))
                .unwrap();
            if width >= 32 {
                let text: String = terminal
                    .backend()
                    .buffer()
                    .content
                    .iter()
                    .map(|c| c.symbol())
                    .collect();
                assert!(text.contains("Design"));
                assert!(text.contains("Please"));
                assert!(text.contains("clarify"));
            }
            state.handle_key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE), &doc);
            terminal
                .draw(|f| draw(f, f.area(), &doc, &mut state))
                .unwrap();
            if width == 12 {
                let text: String = terminal
                    .backend()
                    .buffer()
                    .content
                    .iter()
                    .map(|c| c.symbol())
                    .collect();
                assert!(text.contains("Last block"));
            }
            state.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE), &doc);
            state.input = Some("long draft text that wraps".into());
            terminal
                .draw(|f| draw(f, f.area(), &doc, &mut state))
                .unwrap();
        }
    }
}
