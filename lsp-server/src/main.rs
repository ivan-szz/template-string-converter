use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{self, BufRead, Write};
use tree_sitter::{Node, Parser};

macro_rules! log {
    ($($arg:tt)*) => {
        if std::env::var_os("TEMPLATE_STRING_CONVERTER_LOG").is_some() {
            eprintln!("[template-string-converter] {}", format!($($arg)*));
        }
    };
}

enum Incoming {
    Message(Value),
    Malformed,
    Eof,
}

fn read_message(reader: &mut impl BufRead) -> Incoming {
    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => return Incoming::Eof,
            Ok(_) => {}
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            break;
        }
        if let Some((name, value)) = trimmed.split_once(':') {
            if name.trim().eq_ignore_ascii_case("content-length") {
                match value.trim().parse() {
                    Ok(n) => content_length = n,
                    Err(_) => return Incoming::Malformed,
                }
            }
        }
    }
    let mut buf = vec![0u8; content_length];
    if reader.read_exact(&mut buf).is_err() {
        return Incoming::Eof;
    }
    match serde_json::from_slice(&buf) {
        Ok(v) => Incoming::Message(v),
        Err(_) => Incoming::Malformed,
    }
}

fn write_message(writer: &mut impl Write, msg: &Value) {
    let body = msg.to_string();
    write!(writer, "Content-Length: {}\r\n\r\n{}", body.len(), body).ok();
    writer.flush().ok();
}

/// Byte offset -> LSP position (line, UTF-16 column).
fn offset_to_position(text: &str, offset: usize) -> (u32, u32) {
    let before = &text[..offset];
    let line = before.matches('\n').count() as u32;
    let line_start = before.rfind('\n').map_or(0, |i| i + 1);
    let col = before[line_start..].encode_utf16().count() as u32;
    (line, col)
}

/// LSP position (line, UTF-16 column) -> byte offset. Out-of-range values are
/// clamped to the end of the line / document, as the spec requires.
fn position_to_offset(text: &str, line: u32, character: u32) -> usize {
    let mut line_start = 0;
    for _ in 0..line {
        match text[line_start..].find('\n') {
            Some(i) => line_start += i + 1,
            None => return text.len(),
        }
    }
    let line_end = text[line_start..]
        .find('\n')
        .map_or(text.len(), |i| line_start + i);
    let line_text = text[line_start..line_end]
        .strip_suffix('\r')
        .unwrap_or(&text[line_start..line_end]);

    let mut units = 0u32;
    for (i, ch) in line_text.char_indices() {
        if units >= character {
            return line_start + i;
        }
        units += ch.len_utf16() as u32;
    }
    line_start + line_text.len()
}

fn count_preceding_backslashes(bytes: &[u8], pos: usize) -> usize {
    bytes[..pos].iter().rev().take_while(|&&b| b == b'\\').count()
}

/// A `${` that the user just produced by typing.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Candidate {
    /// Byte offset of the `$`.
    dollar: usize,
    /// Whether the `{` was part of the typed text. Only then do we add the
    /// closing `}`: typing `$` in front of an existing `{name}` must not.
    brace_typed: bool,
}

struct Document {
    text: String,
    version: i64,
    language_id: String,
}

/// Applies `contentChanges` in order and returns the `${` pairs that were
/// just typed, with offsets relative to the final text.
///
/// Only short, single-line insertions count as typing: pastes, reloads,
/// full-document replacements and undo of our own edit never trigger.
fn apply_changes(text: &mut String, changes: &[Value]) -> Vec<Candidate> {
    let mut candidates: Vec<Candidate> = Vec::new();

    for change in changes {
        let Some(new_text) = change["text"].as_str() else {
            continue;
        };
        let range = &change["range"];
        if range.is_null() {
            *text = new_text.to_string();
            candidates.clear();
            continue;
        }

        let pos = |p: &Value| {
            position_to_offset(
                text,
                p["line"].as_u64().unwrap_or(0) as u32,
                p["character"].as_u64().unwrap_or(0) as u32,
            )
        };
        let start = pos(&range["start"]);
        let end = pos(&range["end"]).max(start);
        text.replace_range(start..end, new_text);

        // Shift candidates from earlier changes; drop any this change touched.
        let removed = end - start;
        let added = new_text.len();
        candidates.retain_mut(|c| {
            if c.dollar + 2 <= start {
                true
            } else if c.dollar >= end {
                c.dollar = c.dollar + added - removed;
                true
            } else {
                false
            }
        });

        if new_text.is_empty() || new_text.chars().count() > 2 || new_text.contains('\n') {
            continue;
        }

        let bytes = text.as_bytes();
        let ins_end = start + added;
        for dollar in start.saturating_sub(1)..ins_end {
            if bytes.get(dollar) == Some(&b'$') && bytes.get(dollar + 1) == Some(&b'{') {
                let brace_typed = dollar + 1 >= start && dollar + 1 < ins_end;
                if !candidates.iter().any(|c| c.dollar == dollar) {
                    candidates.push(Candidate { dollar, brace_typed });
                }
            }
        }
    }

    candidates
}

struct Parsers {
    typescript: Parser,
    tsx: Parser,
}

impl Parsers {
    fn new() -> Self {
        let mut typescript = Parser::new();
        typescript
            .set_language(&tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into())
            .expect("typescript grammar");
        let mut tsx = Parser::new();
        tsx.set_language(&tree_sitter_typescript::LANGUAGE_TSX.into())
            .expect("tsx grammar");
        Self { typescript, tsx }
    }

    /// Plain TypeScript needs its own grammar (`<T>x` casts); JavaScript, JSX
    /// and TSX all parse with the TSX grammar.
    fn for_language(&mut self, language_id: &str) -> &mut Parser {
        if language_id == "typescript" {
            &mut self.typescript
        } else {
            &mut self.tsx
        }
    }
}

/// Returns the `'...'`/`"..."` string node whose content contains `dollar`,
/// if converting it to a template literal is valid there.
fn convertible_string(root: Node, bytes: &[u8], dollar: usize) -> Option<(usize, usize)> {
    // `$` must be plain string content; inside an escape (`\${`) or anywhere
    // outside a string (JSX text, comments, code) we leave it alone.
    let fragment = root.descendant_for_byte_range(dollar, dollar + 1)?;
    if fragment.kind() != "string_fragment" {
        return None;
    }
    let string = fragment.parent()?;
    if string.kind() != "string" {
        return None;
    }

    // Template literals are not allowed in JSX attributes, import/export
    // sources or as plain object keys.
    let parent = string.parent()?;
    match parent.kind() {
        "jsx_attribute" | "import_statement" | "export_statement" => return None,
        "pair" if parent.child_by_field_name("key") == Some(string) => return None,
        _ => {}
    }

    let (open, end) = (string.start_byte(), string.end_byte());
    let close = end.checked_sub(1)?;
    let quote = bytes[open];
    if close <= dollar || bytes[close] != quote || !matches!(quote, b'"' | b'\'') {
        return None;
    }
    Some((open, close))
}

/// A text edit as (start byte, end byte, replacement).
type Edit = (usize, usize, String);

fn compute_edits(parser: &mut Parser, text: &str, candidates: &[Candidate]) -> Vec<Edit> {
    let Some(tree) = parser.parse(text, None) else {
        return Vec::new();
    };
    let bytes = text.as_bytes();

    // Group by string so multiple cursors in one string don't overlap.
    let mut strings: Vec<(usize, usize, Vec<usize>)> = Vec::new();
    for c in candidates {
        let Some((open, close)) = convertible_string(tree.root_node(), bytes, c.dollar) else {
            continue;
        };
        let slot = c.dollar + 2;
        let needs_brace = c.brace_typed && bytes.get(slot) != Some(&b'}');
        let entry = match strings.iter_mut().find(|s| s.0 == open) {
            Some(e) => e,
            None => {
                strings.push((open, close, Vec::new()));
                strings.last_mut().unwrap()
            }
        };
        if needs_brace && !entry.2.contains(&slot) {
            entry.2.push(slot);
        }
    }

    let mut edits: Vec<Edit> = Vec::new();
    for (open, close, brace_slots) in strings {
        let mut string_edits: Vec<Edit> = vec![(open, open + 1, "`".into())];
        // Unescaped backticks in the content would end the template literal.
        for i in open + 1..close {
            if bytes[i] == b'`' && count_preceding_backslashes(bytes, i) % 2 == 0 {
                string_edits.push((i, i + 1, "\\`".into()));
            }
        }
        string_edits.push((close, close + 1, "`".into()));

        // Fold each `}` into an edit starting at the same offset so the client
        // never has to order an insert against a replace.
        for slot in brace_slots {
            match string_edits.iter_mut().find(|e| e.0 == slot) {
                Some(e) => e.2.insert(0, '}'),
                None => string_edits.push((slot, slot, "}".into())),
            }
        }
        edits.extend(string_edits);
    }

    edits.sort_by_key(|e| (e.0, e.1));
    edits
}

fn main() {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut reader = io::BufReader::new(stdin.lock());
    let mut writer = io::BufWriter::new(stdout.lock());
    let mut documents: HashMap<String, Document> = HashMap::new();
    let mut parsers = Parsers::new();
    let mut next_request_id = 1i64;
    let mut versioned_edits = false;

    log!("server started");

    loop {
        let msg = match read_message(&mut reader) {
            Incoming::Message(m) => m,
            Incoming::Malformed => {
                log!("malformed message, skipping");
                continue;
            }
            Incoming::Eof => {
                log!("stdin closed, exiting");
                break;
            }
        };

        let method = msg["method"].as_str().unwrap_or("");
        let id = msg.get("id").cloned();
        let params = &msg["params"];

        log!("received: {}", method);

        match method {
            "initialize" => {
                versioned_edits = params["capabilities"]["workspace"]["workspaceEdit"]
                    ["documentChanges"]
                    .as_bool()
                    .unwrap_or(false);
                write_message(
                    &mut writer,
                    &json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "capabilities": {
                                "textDocumentSync": {
                                    "openClose": true,
                                    "change": 2
                                }
                            },
                            "serverInfo": {
                                "name": "template-string-converter",
                                "version": env!("CARGO_PKG_VERSION")
                            }
                        }
                    }),
                );
            }

            "initialized" => {}

            "textDocument/didOpen" => {
                let doc = &params["textDocument"];
                if let (Some(uri), Some(text)) = (doc["uri"].as_str(), doc["text"].as_str()) {
                    log!("didOpen: {} (len={})", uri, text.len());
                    documents.insert(
                        uri.to_string(),
                        Document {
                            text: text.to_string(),
                            version: doc["version"].as_i64().unwrap_or(0),
                            language_id: doc["languageId"].as_str().unwrap_or("").to_string(),
                        },
                    );
                }
            }

            "textDocument/didClose" => {
                if let Some(uri) = params["textDocument"]["uri"].as_str() {
                    documents.remove(uri);
                }
            }

            "textDocument/didChange" => {
                let Some(uri) = params["textDocument"]["uri"].as_str() else {
                    continue;
                };
                let Some(doc) = documents.get_mut(uri) else {
                    continue;
                };
                let Some(changes) = params["contentChanges"].as_array() else {
                    continue;
                };

                doc.version = params["textDocument"]["version"]
                    .as_i64()
                    .unwrap_or(doc.version);
                let candidates = apply_changes(&mut doc.text, changes);
                if candidates.is_empty() {
                    continue;
                }
                log!("candidates: {:?}", candidates);

                let parser = parsers.for_language(&doc.language_id);
                let edits = compute_edits(parser, &doc.text, &candidates);
                if edits.is_empty() {
                    continue;
                }

                let lsp_edits: Vec<Value> = edits
                    .iter()
                    .map(|(start, end, new_text)| {
                        let (sl, sc) = offset_to_position(&doc.text, *start);
                        let (el, ec) = offset_to_position(&doc.text, *end);
                        json!({
                            "range": {
                                "start": {"line": sl, "character": sc},
                                "end":   {"line": el, "character": ec}
                            },
                            "newText": new_text
                        })
                    })
                    .collect();

                // With a version the client rejects the edit if the user typed
                // more in the meantime, instead of applying it at stale offsets.
                let edit = if versioned_edits {
                    json!({
                        "documentChanges": [{
                            "textDocument": {"uri": uri, "version": doc.version},
                            "edits": lsp_edits
                        }]
                    })
                } else {
                    json!({ "changes": { uri: lsp_edits } })
                };

                let req_id = next_request_id;
                next_request_id += 1;
                log!("sending workspace/applyEdit (id={})", req_id);
                write_message(
                    &mut writer,
                    &json!({
                        "jsonrpc": "2.0",
                        "id": req_id,
                        "method": "workspace/applyEdit",
                        "params": {
                            "label": "Convert quotes to template literal",
                            "edit": edit
                        }
                    }),
                );
            }

            "shutdown" => {
                write_message(&mut writer, &json!({"jsonrpc": "2.0", "id": id, "result": null}));
            }

            "exit" => break,

            _ => {
                // Requests we don't handle get an empty result. Messages with an
                // id but no method are the client's replies to our applyEdit.
                if id.is_some() && !method.is_empty() {
                    write_message(&mut writer, &json!({"jsonrpc": "2.0", "id": id, "result": null}));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn change(sl: u32, sc: u32, el: u32, ec: u32, text: &str) -> Value {
        json!({
            "range": {"start": {"line": sl, "character": sc}, "end": {"line": el, "character": ec}},
            "text": text
        })
    }

    fn apply(text: &str, edits: &[Edit]) -> String {
        let mut out = text.to_string();
        for (s, e, t) in edits.iter().rev() {
            out.replace_range(*s..*e, t);
        }
        out
    }

    /// Types `typed` at `col` of a single-line `before` and returns the result
    /// after the server's edits (or the unchanged text).
    fn type_at(language: &str, before: &str, col: u32, typed: &str) -> String {
        let mut text = before.to_string();
        let candidates = apply_changes(&mut text, &[change(0, col, 0, col, typed)]);
        let mut parsers = Parsers::new();
        let edits = compute_edits(parsers.for_language(language), &text, &candidates);
        apply(&text, &edits)
    }

    #[test]
    fn converts_typed_brace() {
        assert_eq!(type_at("javascript", r#"a = "hello $""#, 12, "{}"), "a = `hello ${}`");
        assert_eq!(type_at("typescript", "a = 'x $ y'", 8, "{"), "a = `x ${} y`");
    }

    #[test]
    fn dollar_before_existing_brace_adds_no_brace() {
        assert_eq!(type_at("javascript", r#"a = "hello {name}""#, 11, "$"), "a = `hello ${name}`");
    }

    #[test]
    fn brace_without_autoclose_merges_with_closing_quote() {
        assert_eq!(type_at("javascript", r#"a = "a $""#, 8, "{"), "a = `a ${}`");
    }

    #[test]
    fn escapes_backticks_in_content() {
        assert_eq!(type_at("javascript", r#"a = "a $ `b`""#, 8, "{"), r"a = `a ${} \`b\``");
        assert_eq!(type_at("javascript", r#"a = "a $ \`b""#, 8, "{"), r"a = `a ${} \`b`");
    }

    #[test]
    fn ignores_jsx_text_attributes_and_comments() {
        let jsx = r#"x = <p className="x">Cost: {cost} <a href="y" /></p>"#;
        assert_eq!(type_at("javascriptreact", jsx, 27, "$"), jsx.replace(": {", ": ${"));
        let apos = "x = <p>Don't pay {p} won't</p>";
        assert_eq!(type_at("typescriptreact", apos, 17, "$"), apos.replace(" {", " ${"));
        let attr = r#"x = <p className="a $" />"#;
        assert_eq!(type_at("typescriptreact", attr, 21, "{"), attr.replace("$", "${"));
        let comment = "// it's $ isn't";
        assert_eq!(type_at("javascript", comment, 9, "{"), comment.replace("$", "${"));
    }

    #[test]
    fn ignores_escaped_dollar() {
        assert_eq!(type_at("javascript", r"a = '\$'", 7, "{"), r"a = '\${'");
    }

    #[test]
    fn ignores_paste_and_full_replacement() {
        let mut text = "a = ".to_string();
        let pasted = apply_changes(&mut text, &[change(0, 4, 0, 4, r#""${x}""#)]);
        assert!(pasted.is_empty());
        let full = apply_changes(&mut text, &[json!({"text": r#"a = "${""#})]);
        assert!(full.is_empty());
    }

    #[test]
    fn undo_of_conversion_does_not_retrigger() {
        // State after our edit: `a = `x ${}``. Undo restores the quotes.
        let mut text = "a = `x ${}`".to_string();
        let candidates = apply_changes(
            &mut text,
            &[change(0, 4, 0, 5, "\""), change(0, 10, 0, 11, "\"")],
        );
        assert_eq!(text, r#"a = "x ${}""#);
        assert!(candidates.is_empty());
    }

    #[test]
    fn multi_cursor_offsets_are_shifted() {
        let mut text = r#"a = "$"; b = "$";"#.to_string();
        let candidates = apply_changes(
            &mut text,
            &[change(0, 6, 0, 6, "{}"), change(0, 17, 0, 17, "{}")],
        );
        assert_eq!(text, r#"a = "${}"; b = "${}";"#);
        let mut parsers = Parsers::new();
        let edits = compute_edits(parsers.for_language("javascript"), &text, &candidates);
        assert_eq!(apply(&text, &edits), "a = `${}`; b = `${}`;");
    }

    #[test]
    fn multi_cursor_in_same_string_does_not_overlap() {
        let mut text = r#"a = "$ $""#.to_string();
        let candidates = apply_changes(
            &mut text,
            &[change(0, 6, 0, 6, "{"), change(0, 9, 0, 9, "{")],
        );
        let mut parsers = Parsers::new();
        let edits = compute_edits(parsers.for_language("javascript"), &text, &candidates);
        assert_eq!(apply(&text, &edits), "a = `${} ${}`");
    }

    #[test]
    fn position_conversion_handles_utf16_and_crlf() {
        let text = "é😀\"a\"\r\nx";
        assert_eq!(position_to_offset(text, 0, 3), "é😀".len());
        assert_eq!(offset_to_position(text, "é😀".len()), (0, 3));
        assert_eq!(position_to_offset(text, 0, 99), text.find('\r').unwrap());
        assert_eq!(position_to_offset(text, 1, 0), text.find('x').unwrap());
        assert_eq!(offset_to_position(text, text.find('x').unwrap()), (1, 0));
        assert_eq!(position_to_offset(text, 5, 0), text.len());
    }
}
