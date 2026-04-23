//! This module is responsible for parsing & validating a patch into a list of "hunks".
//! (It does not attempt to actually check that the patch can be applied to the filesystem.)
//!
//! The official Lark grammar for the apply-patch format is:
//!
//! start: begin_patch hunk+ end_patch
//! begin_patch: "*** Begin Patch" LF
//! end_patch: "*** End Patch" LF?
//!
//! hunk: add_hunk | delete_hunk | update_hunk
//! add_hunk: "*** Add File: " filename LF add_line+
//! delete_hunk: "*** Delete File: " filename LF
//! update_hunk: "*** Update File: " filename LF change_move? change?
//! filename: /(.+)/
//! add_line: "+" /(.+)/ LF -> line
//!
//! change_move: "*** Move to: " filename LF
//! change: (change_context | change_line)+ eof_line?
//! change_context: ("@@" | "@@ " /(.+)/) LF
//! change_line: ("+" | "-" | " ") /(.+)/ LF
//! eof_line: "*** End of File" LF
//!
//! The parser below is a little more lenient than the explicit spec and allows for
//! leading/trailing whitespace around patch markers.
use crate::ApplyPatchArgs;
use codex_utils_absolute_path::AbsolutePathBuf;
#[cfg(test)]
use codex_utils_absolute_path::test_support::PathBufExt;
use std::path::Path;
use std::path::PathBuf;

use thiserror::Error;

const BEGIN_PATCH_MARKER: &str = "*** Begin Patch";
const END_PATCH_MARKER: &str = "*** End Patch";
const ADD_FILE_MARKER: &str = "*** Add File: ";
const DELETE_FILE_MARKER: &str = "*** Delete File: ";
const UPDATE_FILE_MARKER: &str = "*** Update File: ";
const MOVE_TO_MARKER: &str = "*** Move to: ";
const EOF_MARKER: &str = "*** End of File";
const CHANGE_CONTEXT_MARKER: &str = "@@ ";
const EMPTY_CHANGE_CONTEXT_MARKER: &str = "@@";

/// Currently, the only OpenAI model that knowingly requires lenient parsing is
/// gpt-4.1. While we could try to require everyone to pass in a strictness
/// param when invoking apply_patch, it is a pain to thread it through all of
/// the call sites, so we resign ourselves allowing lenient parsing for all
/// models. See [`ParseMode::Lenient`] for details on the exceptions we make for
/// gpt-4.1.
const PARSE_IN_STRICT_MODE: bool = false;

#[derive(Debug, PartialEq, Error, Clone)]
pub enum ParseError {
    #[error("invalid patch: {0}")]
    InvalidPatchError(String),
    #[error("invalid hunk at line {line_number}, {message}")]
    InvalidHunkError { message: String, line_number: usize },
}
use ParseError::*;

#[derive(Debug, PartialEq, Clone)]
#[allow(clippy::enum_variant_names)]
pub enum Hunk {
    AddFile {
        path: PathBuf,
        contents: String,
    },
    DeleteFile {
        path: PathBuf,
    },
    UpdateFile {
        path: PathBuf,
        move_path: Option<PathBuf>,

        /// Chunks should be in order, i.e. the `change_context` of one chunk
        /// should occur later in the file than the previous chunk.
        chunks: Vec<UpdateFileChunk>,
    },
}

impl Hunk {
    pub fn resolve_path(&self, cwd: &AbsolutePathBuf) -> AbsolutePathBuf {
        let path = match self {
            Hunk::UpdateFile { path, .. } => path,
            Hunk::AddFile { .. } | Hunk::DeleteFile { .. } => self.path(),
        };
        AbsolutePathBuf::resolve_path_against_base(path, cwd)
    }

    /// Returns the path affected by this hunk, using the move destination for rename hunks.
    pub fn path(&self) -> &Path {
        match self {
            Hunk::AddFile { path, .. } => path,
            Hunk::DeleteFile { path } => path,
            Hunk::UpdateFile {
                move_path: Some(path),
                ..
            } => path,
            Hunk::UpdateFile {
                path,
                move_path: None,
                ..
            } => path,
        }
    }
}

use Hunk::*;

#[derive(Debug, PartialEq, Clone)]
pub struct UpdateFileChunk {
    /// A single line of context used to narrow down the position of the chunk
    /// (this is usually a class, method, or function definition.)
    pub change_context: Option<String>,

    /// A contiguous block of lines that should be replaced with `new_lines`.
    /// `old_lines` must occur strictly after `change_context`.
    pub old_lines: Vec<String>,
    pub new_lines: Vec<String>,

    /// If set to true, `old_lines` must occur at the end of the source file.
    /// (Tolerance around trailing newlines should be encouraged.)
    pub is_end_of_file: bool,
}

pub fn parse_patch(patch: &str) -> Result<ApplyPatchArgs, ParseError> {
    let mode = if PARSE_IN_STRICT_MODE {
        ParseMode::Strict
    } else {
        ParseMode::Lenient
    };
    parse_patch_text(patch, mode)
}

#[derive(Debug, Default, Clone)]
pub struct StreamingPatchParser {
    line_buffer: String,
    state: StreamingParserState,
    hunks: Vec<Hunk>,
}

#[derive(Debug, Default, Clone)]
enum StreamingParserState {
    #[default]
    NotStarted,
    StartedPatch,
    AddFile {
        path: PathBuf,
        contents: String,
    },
    DeleteFile {
        path: PathBuf,
    },
    UpdateFile {
        path: PathBuf,
        header_line_number: usize,
        move_path: Option<PathBuf>,
        can_accept_move: bool,
        chunks: Vec<UpdateFileChunk>,
        current_chunk: Option<UpdateFileChunk>,
    },
    EndedPatch,
    Invalid,
}

impl StreamingPatchParser {
    pub fn push_delta(&mut self, delta: &str) -> Option<Vec<Hunk>> {
        for ch in delta.chars() {
            if ch == '\n' {
                let line = std::mem::take(&mut self.line_buffer);
                let state = std::mem::take(&mut self.state);
                self.state = self.process_line(state, line.trim_end_matches('\r'));
            } else {
                self.line_buffer.push(ch);
            }
        }

        let hunks = self.current_hunks();
        if hunks.is_empty() { None } else { Some(hunks) }
    }

    fn process_line(&mut self, state: StreamingParserState, line: &str) -> StreamingParserState {
        let trimmed = line.trim();
        match state {
            StreamingParserState::NotStarted => {
                if trimmed == BEGIN_PATCH_MARKER {
                    return StreamingParserState::StartedPatch;
                }
                StreamingParserState::Invalid
            }
            StreamingParserState::StartedPatch => {
                if trimmed == END_PATCH_MARKER {
                    return StreamingParserState::EndedPatch;
                }
                if let Some(path) = trimmed.strip_prefix(ADD_FILE_MARKER) {
                    return StreamingParserState::AddFile {
                        path: PathBuf::from(path),
                        contents: String::new(),
                    };
                }
                if let Some(path) = trimmed.strip_prefix(DELETE_FILE_MARKER) {
                    return StreamingParserState::DeleteFile {
                        path: PathBuf::from(path),
                    };
                }
                if let Some(path) = trimmed.strip_prefix(UPDATE_FILE_MARKER) {
                    return StreamingParserState::UpdateFile {
                        path: PathBuf::from(path),
                        header_line_number: 0,
                        move_path: None,
                        can_accept_move: true,
                        chunks: Vec::new(),
                        current_chunk: None,
                    };
                }
                StreamingParserState::StartedPatch
            }
            StreamingParserState::AddFile { path, mut contents } => {
                if trimmed == END_PATCH_MARKER {
                    self.hunks.push(AddFile { path, contents });
                    return StreamingParserState::EndedPatch;
                }
                if let Some(next_path) = trimmed.strip_prefix(ADD_FILE_MARKER) {
                    self.hunks.push(AddFile { path, contents });
                    return StreamingParserState::AddFile {
                        path: PathBuf::from(next_path),
                        contents: String::new(),
                    };
                }
                if let Some(next_path) = trimmed.strip_prefix(DELETE_FILE_MARKER) {
                    self.hunks.push(AddFile { path, contents });
                    return StreamingParserState::DeleteFile {
                        path: PathBuf::from(next_path),
                    };
                }
                if let Some(next_path) = trimmed.strip_prefix(UPDATE_FILE_MARKER) {
                    self.hunks.push(AddFile { path, contents });
                    return StreamingParserState::UpdateFile {
                        path: PathBuf::from(next_path),
                        header_line_number: 0,
                        move_path: None,
                        can_accept_move: true,
                        chunks: Vec::new(),
                        current_chunk: None,
                    };
                }
                if let Some(line_to_add) = line.strip_prefix('+') {
                    contents.push_str(line_to_add);
                    contents.push('\n');
                }
                StreamingParserState::AddFile { path, contents }
            }
            StreamingParserState::DeleteFile { path } => {
                if trimmed == END_PATCH_MARKER {
                    self.hunks.push(DeleteFile { path });
                    return StreamingParserState::EndedPatch;
                }
                if let Some(next_path) = trimmed.strip_prefix(ADD_FILE_MARKER) {
                    self.hunks.push(DeleteFile { path });
                    return StreamingParserState::AddFile {
                        path: PathBuf::from(next_path),
                        contents: String::new(),
                    };
                }
                if let Some(next_path) = trimmed.strip_prefix(DELETE_FILE_MARKER) {
                    self.hunks.push(DeleteFile { path });
                    return StreamingParserState::DeleteFile {
                        path: PathBuf::from(next_path),
                    };
                }
                if let Some(next_path) = trimmed.strip_prefix(UPDATE_FILE_MARKER) {
                    self.hunks.push(DeleteFile { path });
                    return StreamingParserState::UpdateFile {
                        path: PathBuf::from(next_path),
                        header_line_number: 0,
                        move_path: None,
                        can_accept_move: true,
                        chunks: Vec::new(),
                        current_chunk: None,
                    };
                }
                StreamingParserState::DeleteFile { path }
            }
            StreamingParserState::UpdateFile {
                path,
                header_line_number,
                move_path,
                can_accept_move,
                mut chunks,
                mut current_chunk,
            } => {
                let mut move_path = move_path;
                let mut can_accept_move = can_accept_move;
                if trimmed == END_PATCH_MARKER {
                    if let Some(chunk) = current_chunk.take()
                        && (!chunk.old_lines.is_empty()
                            || !chunk.new_lines.is_empty()
                            || chunk.is_end_of_file)
                    {
                        chunks.push(chunk);
                    }
                    if !chunks.is_empty() {
                        self.hunks.push(UpdateFile {
                            path,
                            move_path,
                            chunks,
                        });
                    }
                    return StreamingParserState::EndedPatch;
                }
                if let Some(next_path) = trimmed.strip_prefix(ADD_FILE_MARKER) {
                    if let Some(chunk) = current_chunk.take()
                        && (!chunk.old_lines.is_empty()
                            || !chunk.new_lines.is_empty()
                            || chunk.is_end_of_file)
                    {
                        chunks.push(chunk);
                    }
                    if !chunks.is_empty() {
                        self.hunks.push(UpdateFile {
                            path,
                            move_path,
                            chunks,
                        });
                    }
                    return StreamingParserState::AddFile {
                        path: PathBuf::from(next_path),
                        contents: String::new(),
                    };
                }
                if let Some(next_path) = trimmed.strip_prefix(DELETE_FILE_MARKER) {
                    if let Some(chunk) = current_chunk.take()
                        && (!chunk.old_lines.is_empty()
                            || !chunk.new_lines.is_empty()
                            || chunk.is_end_of_file)
                    {
                        chunks.push(chunk);
                    }
                    if !chunks.is_empty() {
                        self.hunks.push(UpdateFile {
                            path,
                            move_path,
                            chunks,
                        });
                    }
                    return StreamingParserState::DeleteFile {
                        path: PathBuf::from(next_path),
                    };
                }
                if let Some(next_path) = trimmed.strip_prefix(UPDATE_FILE_MARKER) {
                    if let Some(chunk) = current_chunk.take()
                        && (!chunk.old_lines.is_empty()
                            || !chunk.new_lines.is_empty()
                            || chunk.is_end_of_file)
                    {
                        chunks.push(chunk);
                    }
                    if !chunks.is_empty() {
                        self.hunks.push(UpdateFile {
                            path,
                            move_path,
                            chunks,
                        });
                    }
                    return StreamingParserState::UpdateFile {
                        path: PathBuf::from(next_path),
                        header_line_number: 0,
                        move_path: None,
                        can_accept_move: true,
                        chunks: Vec::new(),
                        current_chunk: None,
                    };
                }
                if can_accept_move
                    && move_path.is_none()
                    && let Some(move_to_path) = line.trim().strip_prefix(MOVE_TO_MARKER)
                {
                    move_path = Some(PathBuf::from(move_to_path));
                    return StreamingParserState::UpdateFile {
                        path,
                        header_line_number,
                        move_path,
                        can_accept_move,
                        chunks,
                        current_chunk,
                    };
                }

                can_accept_move = false;
                let trimmed_update_line = line.trim();
                let change_context = if trimmed_update_line == EMPTY_CHANGE_CONTEXT_MARKER {
                    Some(None)
                } else {
                    trimmed_update_line
                        .strip_prefix(CHANGE_CONTEXT_MARKER)
                        .map(|context| Some(context.to_string()))
                };
                if let Some(change_context) = change_context {
                    if let Some(chunk) = current_chunk.take()
                        && (!chunk.old_lines.is_empty()
                            || !chunk.new_lines.is_empty()
                            || chunk.is_end_of_file)
                    {
                        chunks.push(chunk);
                    }
                    current_chunk = Some(UpdateFileChunk {
                        change_context,
                        old_lines: Vec::new(),
                        new_lines: Vec::new(),
                        is_end_of_file: false,
                    });
                    return StreamingParserState::UpdateFile {
                        path,
                        header_line_number,
                        move_path,
                        can_accept_move,
                        chunks,
                        current_chunk,
                    };
                }

                if trimmed == EOF_MARKER {
                    if let Some(chunk) = current_chunk.as_mut() {
                        chunk.is_end_of_file = true;
                    }
                    if let Some(chunk) = current_chunk.take()
                        && (!chunk.old_lines.is_empty()
                            || !chunk.new_lines.is_empty()
                            || chunk.is_end_of_file)
                    {
                        chunks.push(chunk);
                    }
                    return StreamingParserState::UpdateFile {
                        path,
                        header_line_number,
                        move_path,
                        can_accept_move,
                        chunks,
                        current_chunk,
                    };
                }

                if current_chunk.is_none() {
                    current_chunk = Some(UpdateFileChunk {
                        change_context: None,
                        old_lines: Vec::new(),
                        new_lines: Vec::new(),
                        is_end_of_file: false,
                    });
                }
                if let Some(chunk) = current_chunk.as_mut() {
                    match line.chars().next() {
                        None => {
                            chunk.old_lines.push(String::new());
                            chunk.new_lines.push(String::new());
                        }
                        Some(' ') => {
                            chunk.old_lines.push(line[1..].to_string());
                            chunk.new_lines.push(line[1..].to_string());
                        }
                        Some('+') => {
                            chunk.new_lines.push(line[1..].to_string());
                        }
                        Some('-') => {
                            chunk.old_lines.push(line[1..].to_string());
                        }
                        Some(_) => {}
                    }
                }
                StreamingParserState::UpdateFile {
                    path,
                    header_line_number,
                    move_path,
                    can_accept_move,
                    chunks,
                    current_chunk,
                }
            }
            StreamingParserState::EndedPatch | StreamingParserState::Invalid => state,
        }
    }

    fn current_hunks(&self) -> Vec<Hunk> {
        let mut hunks = self.hunks.clone();
        match &self.state {
            StreamingParserState::AddFile { path, contents } => {
                hunks.push(AddFile {
                    path: path.clone(),
                    contents: contents.clone(),
                });
            }
            StreamingParserState::DeleteFile { path } => {
                hunks.push(DeleteFile { path: path.clone() });
            }
            StreamingParserState::UpdateFile {
                path,
                move_path,
                chunks,
                current_chunk,
                ..
            } => {
                let mut chunks = chunks.clone();
                if let Some(chunk) = current_chunk
                    && (!chunk.old_lines.is_empty()
                        || !chunk.new_lines.is_empty()
                        || chunk.is_end_of_file)
                {
                    chunks.push(chunk.clone());
                }
                if !chunks.is_empty() {
                    hunks.push(UpdateFile {
                        path: path.clone(),
                        move_path: move_path.clone(),
                        chunks,
                    });
                }
            }
            StreamingParserState::NotStarted
            | StreamingParserState::StartedPatch
            | StreamingParserState::EndedPatch
            | StreamingParserState::Invalid => {}
        }
        hunks
    }
}

enum ParseMode {
    /// Parse the patch text argument as is.
    Strict,

    /// GPT-4.1 is known to formulate the `command` array for the `local_shell`
    /// tool call for `apply_patch` call using something like the following:
    ///
    /// ```json
    /// [
    ///   "apply_patch",
    ///   "<<'EOF'\n*** Begin Patch\n*** Update File: README.md\n@@...\n*** End Patch\nEOF\n",
    /// ]
    /// ```
    ///
    /// This is a problem because `local_shell` is a bit of a misnomer: the
    /// `command` is not invoked by passing the arguments to a shell like Bash,
    /// but are invoked using something akin to `execvpe(3)`.
    ///
    /// This is significant in this case because where a shell would interpret
    /// `<<'EOF'...` as a heredoc and pass the contents via stdin (which is
    /// fine, as `apply_patch` is specified to read from stdin if no argument is
    /// passed), `execvpe(3)` interprets the heredoc as a literal string. To get
    /// the `local_shell` tool to run a command the way shell would, the
    /// `command` array must be something like:
    ///
    /// ```json
    /// [
    ///   "bash",
    ///   "-lc",
    ///   "apply_patch <<'EOF'\n*** Begin Patch\n*** Update File: README.md\n@@...\n*** End Patch\nEOF\n",
    /// ]
    /// ```
    ///
    /// In lenient mode, we check if the argument to `apply_patch` starts with
    /// `<<'EOF'` and ends with `EOF\n`. If so, we strip off these markers,
    /// trim() the result, and treat what is left as the patch text.
    Lenient,
}

fn parse_patch_text(patch: &str, mode: ParseMode) -> Result<ApplyPatchArgs, ParseError> {
    let lines: Vec<&str> = patch.trim().lines().collect();
    let (patch_lines, hunk_lines) = match mode {
        ParseMode::Strict => check_patch_boundaries_strict(&lines)?,
        ParseMode::Lenient => check_patch_boundaries_lenient(&lines)?,
    };

    let mut hunks: Vec<Hunk> = Vec::new();
    let mut remaining_lines = hunk_lines;
    let mut line_number = 2;
    while !remaining_lines.is_empty() {
        let (hunk, hunk_lines) = parse_one_hunk(remaining_lines, line_number)?;
        hunks.push(hunk);
        line_number += hunk_lines;
        remaining_lines = &remaining_lines[hunk_lines..]
    }
    let patch = patch_lines.join("\n");
    Ok(ApplyPatchArgs {
        hunks,
        patch,
        workdir: None,
    })
}

/// Checks the start and end lines of the patch text for `apply_patch`,
/// returning an error if they do not match the expected markers.
fn check_patch_boundaries_strict<'a>(
    lines: &'a [&'a str],
) -> Result<(&'a [&'a str], &'a [&'a str]), ParseError> {
    let (first_line, last_line) = match lines {
        [] => (None, None),
        [first] => (Some(first), Some(first)),
        [first, .., last] => (Some(first), Some(last)),
    };
    check_start_and_end_lines_strict(first_line, last_line)?;
    Ok((lines, &lines[1..lines.len() - 1]))
}

/// If we are in lenient mode, we check if the first line starts with `<<EOF`
/// (possibly quoted) and the last line ends with `EOF`. There must be at least
/// 4 lines total because the heredoc markers take up 2 lines and the patch text
/// must have at least 2 lines.
///
/// If successful, returns the lines of the patch text that contain the patch
/// contents, excluding the heredoc markers.
fn check_patch_boundaries_lenient<'a>(
    original_lines: &'a [&'a str],
) -> Result<(&'a [&'a str], &'a [&'a str]), ParseError> {
    let original_parse_error = match check_patch_boundaries_strict(original_lines) {
        Ok(lines) => return Ok(lines),
        Err(e) => e,
    };

    match original_lines {
        [first, .., last] => {
            if (first == &"<<EOF" || first == &"<<'EOF'" || first == &"<<\"EOF\"")
                && last.ends_with("EOF")
                && original_lines.len() >= 4
            {
                let inner_lines = &original_lines[1..original_lines.len() - 1];
                check_patch_boundaries_strict(inner_lines)
            } else {
                Err(original_parse_error)
            }
        }
        _ => Err(original_parse_error),
    }
}

fn check_start_and_end_lines_strict(
    first_line: Option<&&str>,
    last_line: Option<&&str>,
) -> Result<(), ParseError> {
    let first_line = first_line.map(|line| line.trim());
    let last_line = last_line.map(|line| line.trim());

    match (first_line, last_line) {
        (Some(first), Some(last)) if first == BEGIN_PATCH_MARKER && last == END_PATCH_MARKER => {
            Ok(())
        }
        (Some(first), _) if first != BEGIN_PATCH_MARKER => Err(InvalidPatchError(String::from(
            "The first line of the patch must be '*** Begin Patch'",
        ))),
        _ => Err(InvalidPatchError(String::from(
            "The last line of the patch must be '*** End Patch'",
        ))),
    }
}

/// Attempts to parse a single hunk from the start of lines.
/// Returns the parsed hunk and the number of lines parsed (or a ParseError).
fn parse_one_hunk(lines: &[&str], line_number: usize) -> Result<(Hunk, usize), ParseError> {
    let first_line = lines[0].trim();
    if let Some(path) = first_line.strip_prefix(ADD_FILE_MARKER) {
        let mut contents = String::new();
        let mut parsed_lines = 1;
        for add_line in &lines[1..] {
            if let Some(line_to_add) = add_line.strip_prefix('+') {
                contents.push_str(line_to_add);
                contents.push('\n');
                parsed_lines += 1;
            } else {
                break;
            }
        }
        return Ok((
            AddFile {
                path: PathBuf::from(path),
                contents,
            },
            parsed_lines,
        ));
    } else if let Some(path) = first_line.strip_prefix(DELETE_FILE_MARKER) {
        return Ok((
            DeleteFile {
                path: PathBuf::from(path),
            },
            1,
        ));
    } else if let Some(path) = first_line.strip_prefix(UPDATE_FILE_MARKER) {
        let mut remaining_lines = &lines[1..];
        let mut parsed_lines = 1;
        let move_path = remaining_lines
            .first()
            .and_then(|x| x.strip_prefix(MOVE_TO_MARKER));

        if move_path.is_some() {
            remaining_lines = &remaining_lines[1..];
            parsed_lines += 1;
        }

        let mut chunks = Vec::new();
        while !remaining_lines.is_empty() {
            if remaining_lines[0].trim().is_empty() {
                parsed_lines += 1;
                remaining_lines = &remaining_lines[1..];
                continue;
            }

            if remaining_lines[0].starts_with('*') {
                break;
            }

            let (chunk, chunk_lines) = parse_update_file_chunk(
                remaining_lines,
                line_number + parsed_lines,
                chunks.is_empty(),
            )?;
            chunks.push(chunk);
            parsed_lines += chunk_lines;
            remaining_lines = &remaining_lines[chunk_lines..]
        }

        if chunks.is_empty() {
            return Err(InvalidHunkError {
                message: format!(
                    "Update file hunk for path '{}' is empty",
                    Path::new(path).display()
                ),
                line_number,
            });
        }

        return Ok((
            UpdateFile {
                path: PathBuf::from(path),
                move_path: move_path.map(PathBuf::from),
                chunks,
            },
            parsed_lines,
        ));
    }

    Err(InvalidHunkError {
        message: format!(
            "'{first_line}' is not a valid hunk header. Valid hunk headers: '*** Add File: {{path}}', '*** Delete File: {{path}}', '*** Update File: {{path}}'"
        ),
        line_number,
    })
}

fn parse_update_file_chunk(
    lines: &[&str],
    line_number: usize,
    allow_missing_context: bool,
) -> Result<(UpdateFileChunk, usize), ParseError> {
    if lines.is_empty() {
        return Err(InvalidHunkError {
            message: "Update hunk does not contain any lines".to_string(),
            line_number,
        });
    }
    let (change_context, start_index) = if lines[0] == EMPTY_CHANGE_CONTEXT_MARKER {
        (None, 1)
    } else if let Some(context) = lines[0].strip_prefix(CHANGE_CONTEXT_MARKER) {
        (Some(context.to_string()), 1)
    } else {
        if !allow_missing_context {
            return Err(InvalidHunkError {
                message: format!(
                    "Expected update hunk to start with a @@ context marker, got: '{}'",
                    lines[0]
                ),
                line_number,
            });
        }
        (None, 0)
    };
    if start_index >= lines.len() {
        return Err(InvalidHunkError {
            message: "Update hunk does not contain any lines".to_string(),
            line_number: line_number + 1,
        });
    }
    let mut chunk = UpdateFileChunk {
        change_context,
        old_lines: Vec::new(),
        new_lines: Vec::new(),
        is_end_of_file: false,
    };
    let mut parsed_lines = 0;
    for line in &lines[start_index..] {
        if *line == EOF_MARKER {
            if parsed_lines == 0 {
                return Err(InvalidHunkError {
                    message: "Update hunk does not contain any lines".to_string(),
                    line_number: line_number + 1,
                });
            }
            chunk.is_end_of_file = true;
            parsed_lines += 1;
            break;
        }

        match line.chars().next() {
            None => {
                chunk.old_lines.push(String::new());
                chunk.new_lines.push(String::new());
            }
            Some(' ') => {
                chunk.old_lines.push(line[1..].to_string());
                chunk.new_lines.push(line[1..].to_string());
            }
            Some('+') => {
                chunk.new_lines.push(line[1..].to_string());
            }
            Some('-') => {
                chunk.old_lines.push(line[1..].to_string());
            }
            Some(_) => {
                if parsed_lines == 0 {
                    return Err(InvalidHunkError {
                        message: format!(
                            "Unexpected line found in update hunk: '{line}'. Every line should start with ' ' (context line), '+' (added line), or '-' (removed line)"
                        ),
                        line_number: line_number + 1,
                    });
                }
                break;
            }
        }
        parsed_lines += 1;
    }

    Ok((chunk, parsed_lines + start_index))
}

#[test]
fn test_streaming_patch_parser_streams_complete_lines_before_end_patch() {
    let mut parser = StreamingPatchParser::default();
    parser.push_delta("*** Begin Patch\n*** Add File: src/hello.txt\n+hello\n+wor");
    assert_eq!(
        parser.current_hunks(),
        vec![AddFile {
            path: PathBuf::from("src/hello.txt"),
            contents: "hello\n".to_string(),
        }]
    );
    parser.push_delta("\n");
    assert_eq!(
        parser.current_hunks(),
        vec![AddFile {
            path: PathBuf::from("src/hello.txt"),
            contents: "hello\nwor\n".to_string(),
        }]
    );

    let mut parser = StreamingPatchParser::default();
    parser.push_delta(
        "*** Begin Patch\n*** Update File: src/old.rs\n*** Move to: src/new.rs\n@@\n-old\n+new\n",
    );
    assert_eq!(
        parser.current_hunks(),
        vec![UpdateFile {
            path: PathBuf::from("src/old.rs"),
            move_path: Some(PathBuf::from("src/new.rs")),
            chunks: vec![UpdateFileChunk {
                change_context: None,
                old_lines: vec!["old".to_string()],
                new_lines: vec!["new".to_string()],
                is_end_of_file: false,
            }],
        }]
    );

    let mut parser = StreamingPatchParser::default();
    assert_eq!(
        parser.push_delta("*** Begin Patch\n*** Delete File: gone.txt"),
        None
    );
    assert_eq!(
        parser.push_delta("\n"),
        Some(vec![DeleteFile {
            path: PathBuf::from("gone.txt"),
        }])
    );
    assert!(
        parse_patch_text(
            "*** Begin Patch\n*** Delete File: gone.txt",
            ParseMode::Strict
        )
        .is_err()
    );

    let mut parser = StreamingPatchParser::default();
    parser.push_delta(
        "*** Begin Patch\n*** Add File: src/one.txt\n+one\n*** Delete File: src/two.txt\n",
    );
    assert_eq!(
        parser.current_hunks(),
        vec![
            AddFile {
                path: PathBuf::from("src/one.txt"),
                contents: "one\n".to_string(),
            },
            DeleteFile {
                path: PathBuf::from("src/two.txt"),
            },
        ]
    );
}

#[test]
fn test_streaming_patch_parser_large_patch_split_by_character() {
    let patch = "\
*** Begin Patch
*** Add File: docs/release-notes.md
+# Release notes
+
+## CLI
+- Surface apply_patch progress while arguments stream.
+- Keep final patch application gated on the completed tool call.
+- Include file summaries in the progress event payload.
*** Update File: src/config.rs
@@ impl Config
-    pub apply_patch_progress: bool,
+    pub stream_apply_patch_progress: bool,
     pub include_diagnostics: bool,
@@ fn default_progress_interval()
-    Duration::from_millis(500)
+    Duration::from_millis(250)
*** Delete File: src/legacy_patch_progress.rs
*** Update File: crates/cli/src/main.rs
*** Move to: crates/cli/src/bin/codex.rs
@@ fn run()
-    let args = Args::parse();
-    dispatch(args)
+    let cli = Cli::parse();
+    dispatch(cli)
*** Add File: tests/fixtures/apply_patch_progress.json
+{
+  \"type\": \"apply_patch_progress\",
+  \"hunks\": [
+    { \"operation\": \"add\", \"path\": \"docs/release-notes.md\" },
+    { \"operation\": \"update\", \"path\": \"src/config.rs\" }
+  ]
+}
*** Update File: README.md
@@ Development workflow
 Build the Rust workspace before opening a pull request.
+When touching streamed tool calls, include parser coverage for partial input.
+Prefer tests that exercise the exact event payload shape.
*** Delete File: docs/old-apply-patch-progress.md
*** End Patch";

    let mut parser = StreamingPatchParser::default();
    let mut max_hunk_count = 0;
    let mut saw_hunk_counts = Vec::new();
    for ch in patch.chars() {
        if let Some(hunks) = parser.push_delta(&ch.to_string()) {
            let hunk_count = hunks.len();
            assert!(
                hunk_count >= max_hunk_count,
                "hunk count should never decrease while streaming: {hunk_count} < {max_hunk_count}",
            );
            if hunk_count > max_hunk_count {
                saw_hunk_counts.push(hunk_count);
                max_hunk_count = hunk_count;
            }
        }
    }

    assert_eq!(saw_hunk_counts, vec![1, 2, 3, 4, 5, 6, 7]);
    let hunks = parser.current_hunks();
    assert_eq!(hunks.len(), 7);
    assert_eq!(
        hunks
            .iter()
            .map(|hunk| match hunk {
                AddFile { .. } => "add",
                DeleteFile { .. } => "delete",
                UpdateFile {
                    move_path: Some(_), ..
                } => "move-update",
                UpdateFile {
                    move_path: None, ..
                } => "update",
            })
            .collect::<Vec<_>>(),
        vec![
            "add",
            "update",
            "delete",
            "move-update",
            "add",
            "update",
            "delete"
        ]
    );
}

#[test]
fn test_streaming_patch_parser_waits_for_complete_lines() {
    let mut parser = StreamingPatchParser::default();
    assert_eq!(parser.push_delta("*** Begin Patch\n"), None);
    assert_eq!(
        parser.push_delta("*** Add File: src/hello.txt\n"),
        Some(vec![AddFile {
            path: PathBuf::from("src/hello.txt"),
            contents: String::new(),
        }])
    );
    let empty_add = Some(vec![AddFile {
        path: PathBuf::from("src/hello.txt"),
        contents: String::new(),
    }]);
    assert_eq!(parser.push_delta("+hel"), empty_add);
    let empty_add = Some(vec![AddFile {
        path: PathBuf::from("src/hello.txt"),
        contents: String::new(),
    }]);
    assert_eq!(parser.push_delta("lo"), empty_add);
    assert_eq!(
        parser.push_delta("\n+world"),
        Some(vec![AddFile {
            path: PathBuf::from("src/hello.txt"),
            contents: "hello\n".to_string(),
        }])
    );
    assert_eq!(
        parser.push_delta("\n*** Delete File: src/gone.txt\n"),
        Some(vec![
            AddFile {
                path: PathBuf::from("src/hello.txt"),
                contents: "hello\nworld\n".to_string(),
            },
            DeleteFile {
                path: PathBuf::from("src/gone.txt"),
            },
        ])
    );
}

#[test]
fn test_parse_patch() {
    assert_eq!(
        parse_patch_text("bad", ParseMode::Strict),
        Err(InvalidPatchError(
            "The first line of the patch must be '*** Begin Patch'".to_string()
        ))
    );
    assert_eq!(
        parse_patch_text("*** Begin Patch\nbad", ParseMode::Strict),
        Err(InvalidPatchError(
            "The last line of the patch must be '*** End Patch'".to_string()
        ))
    );

    assert_eq!(
        parse_patch_text(
            concat!(
                "*** Begin Patch",
                " ",
                "\n*** Add File: foo\n+hi\n",
                " ",
                "*** End Patch"
            ),
            ParseMode::Strict
        )
        .unwrap()
        .hunks,
        vec![AddFile {
            path: PathBuf::from("foo"),
            contents: "hi\n".to_string()
        }]
    );
    assert_eq!(
        parse_patch_text(
            "*** Begin Patch\n\
             *** Update File: test.py\n\
             *** End Patch",
            ParseMode::Strict
        ),
        Err(InvalidHunkError {
            message: "Update file hunk for path 'test.py' is empty".to_string(),
            line_number: 2,
        })
    );
    assert_eq!(
        parse_patch_text(
            "*** Begin Patch\n\
             *** End Patch",
            ParseMode::Strict
        )
        .unwrap()
        .hunks,
        Vec::new()
    );
    assert_eq!(
        parse_patch_text(
            "*** Begin Patch\n\
             *** Add File: path/add.py\n\
             +abc\n\
             +def\n\
             *** Delete File: path/delete.py\n\
             *** Update File: path/update.py\n\
             *** Move to: path/update2.py\n\
             @@ def f():\n\
             -    pass\n\
             +    return 123\n\
             *** End Patch",
            ParseMode::Strict
        )
        .unwrap()
        .hunks,
        vec![
            AddFile {
                path: PathBuf::from("path/add.py"),
                contents: "abc\ndef\n".to_string()
            },
            DeleteFile {
                path: PathBuf::from("path/delete.py")
            },
            UpdateFile {
                path: PathBuf::from("path/update.py"),
                move_path: Some(PathBuf::from("path/update2.py")),
                chunks: vec![UpdateFileChunk {
                    change_context: Some("def f():".to_string()),
                    old_lines: vec!["    pass".to_string()],
                    new_lines: vec!["    return 123".to_string()],
                    is_end_of_file: false
                }]
            }
        ]
    );
    // Update hunk followed by another hunk (Add File).
    assert_eq!(
        parse_patch_text(
            "*** Begin Patch\n\
             *** Update File: file.py\n\
             @@\n\
             +line\n\
             *** Add File: other.py\n\
             +content\n\
             *** End Patch",
            ParseMode::Strict
        )
        .unwrap()
        .hunks,
        vec![
            UpdateFile {
                path: PathBuf::from("file.py"),
                move_path: None,
                chunks: vec![UpdateFileChunk {
                    change_context: None,
                    old_lines: vec![],
                    new_lines: vec!["line".to_string()],
                    is_end_of_file: false
                }],
            },
            AddFile {
                path: PathBuf::from("other.py"),
                contents: "content\n".to_string()
            }
        ]
    );

    // Update hunk without an explicit @@ header for the first chunk should parse.
    // Use a raw string to preserve the leading space diff marker on the context line.
    assert_eq!(
        parse_patch_text(
            r#"*** Begin Patch
*** Update File: file2.py
 import foo
+bar
*** End Patch"#,
            ParseMode::Strict
        )
        .unwrap()
        .hunks,
        vec![UpdateFile {
            path: PathBuf::from("file2.py"),
            move_path: None,
            chunks: vec![UpdateFileChunk {
                change_context: None,
                old_lines: vec!["import foo".to_string()],
                new_lines: vec!["import foo".to_string(), "bar".to_string()],
                is_end_of_file: false,
            }],
        }]
    );
}

#[test]
fn test_parse_patch_accepts_relative_and_absolute_hunk_paths() {
    let dir = tempfile::tempdir().unwrap();
    let absolute_delete = dir.path().join("absolute-delete.py").abs();
    let absolute_update = dir.path().join("absolute-update.py").abs();
    let patch_text = format!(
        r#"*** Begin Patch
*** Add File: relative-add.py
+content
*** Delete File: {}
*** Update File: {}
@@
-old
+new
*** End Patch"#,
        absolute_delete.display(),
        absolute_update.display()
    );

    assert_eq!(
        parse_patch_text(&patch_text, ParseMode::Strict)
            .unwrap()
            .hunks,
        vec![
            AddFile {
                path: PathBuf::from("relative-add.py"),
                contents: "content\n".to_string()
            },
            DeleteFile {
                path: absolute_delete.to_path_buf()
            },
            UpdateFile {
                path: absolute_update.to_path_buf(),
                move_path: None,
                chunks: vec![UpdateFileChunk {
                    change_context: None,
                    old_lines: vec!["old".to_string()],
                    new_lines: vec!["new".to_string()],
                    is_end_of_file: false
                }]
            },
        ]
    );
}

#[test]
fn test_hunk_resolve_path_accepts_relative_and_absolute_paths() {
    let cwd_dir = tempfile::tempdir().unwrap();
    let cwd = cwd_dir.path().to_path_buf().abs();
    let absolute_dir = tempfile::tempdir().unwrap();
    let absolute_add = absolute_dir.path().join("absolute-add.py").abs();
    let absolute_delete = absolute_dir.path().join("absolute-delete.py").abs();
    let absolute_update = absolute_dir.path().join("absolute-update.py").abs();

    for (hunk, expected_path) in [
        (
            AddFile {
                path: PathBuf::from("relative-add.py"),
                contents: String::new(),
            },
            cwd.join("relative-add.py"),
        ),
        (
            DeleteFile {
                path: PathBuf::from("relative-delete.py"),
            },
            cwd.join("relative-delete.py"),
        ),
        (
            UpdateFile {
                path: PathBuf::from("relative-update.py"),
                move_path: None,
                chunks: Vec::new(),
            },
            cwd.join("relative-update.py"),
        ),
        (
            AddFile {
                path: absolute_add.to_path_buf(),
                contents: String::new(),
            },
            absolute_add,
        ),
        (
            DeleteFile {
                path: absolute_delete.to_path_buf(),
            },
            absolute_delete,
        ),
        (
            UpdateFile {
                path: absolute_update.to_path_buf(),
                move_path: None,
                chunks: Vec::new(),
            },
            absolute_update,
        ),
    ] {
        assert_eq!(hunk.resolve_path(&cwd), expected_path);
    }
}

#[test]
fn test_parse_patch_lenient() {
    let patch_text = r#"*** Begin Patch
*** Update File: file2.py
 import foo
+bar
*** End Patch"#;
    let expected_patch = vec![UpdateFile {
        path: PathBuf::from("file2.py"),
        move_path: None,
        chunks: vec![UpdateFileChunk {
            change_context: None,
            old_lines: vec!["import foo".to_string()],
            new_lines: vec!["import foo".to_string(), "bar".to_string()],
            is_end_of_file: false,
        }],
    }];
    let expected_error =
        InvalidPatchError("The first line of the patch must be '*** Begin Patch'".to_string());

    let patch_text_in_heredoc = format!("<<EOF\n{patch_text}\nEOF\n");
    assert_eq!(
        parse_patch_text(&patch_text_in_heredoc, ParseMode::Strict),
        Err(expected_error.clone())
    );
    assert_eq!(
        parse_patch_text(&patch_text_in_heredoc, ParseMode::Lenient),
        Ok(ApplyPatchArgs {
            hunks: expected_patch.clone(),
            patch: patch_text.to_string(),
            workdir: None,
        })
    );

    let patch_text_in_single_quoted_heredoc = format!("<<'EOF'\n{patch_text}\nEOF\n");
    assert_eq!(
        parse_patch_text(&patch_text_in_single_quoted_heredoc, ParseMode::Strict),
        Err(expected_error.clone())
    );
    assert_eq!(
        parse_patch_text(&patch_text_in_single_quoted_heredoc, ParseMode::Lenient),
        Ok(ApplyPatchArgs {
            hunks: expected_patch.clone(),
            patch: patch_text.to_string(),
            workdir: None,
        })
    );

    let patch_text_in_double_quoted_heredoc = format!("<<\"EOF\"\n{patch_text}\nEOF\n");
    assert_eq!(
        parse_patch_text(&patch_text_in_double_quoted_heredoc, ParseMode::Strict),
        Err(expected_error.clone())
    );
    assert_eq!(
        parse_patch_text(&patch_text_in_double_quoted_heredoc, ParseMode::Lenient),
        Ok(ApplyPatchArgs {
            hunks: expected_patch,
            patch: patch_text.to_string(),
            workdir: None,
        })
    );

    let patch_text_in_mismatched_quotes_heredoc = format!("<<\"EOF'\n{patch_text}\nEOF\n");
    assert_eq!(
        parse_patch_text(&patch_text_in_mismatched_quotes_heredoc, ParseMode::Strict),
        Err(expected_error.clone())
    );
    assert_eq!(
        parse_patch_text(&patch_text_in_mismatched_quotes_heredoc, ParseMode::Lenient),
        Err(expected_error.clone())
    );

    let patch_text_with_missing_closing_heredoc =
        "<<EOF\n*** Begin Patch\n*** Update File: file2.py\nEOF\n".to_string();
    assert_eq!(
        parse_patch_text(&patch_text_with_missing_closing_heredoc, ParseMode::Strict),
        Err(expected_error)
    );
    assert_eq!(
        parse_patch_text(&patch_text_with_missing_closing_heredoc, ParseMode::Lenient),
        Err(InvalidPatchError(
            "The last line of the patch must be '*** End Patch'".to_string()
        ))
    );
}
