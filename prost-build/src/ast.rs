use prost_types::source_code_info::Location;
#[cfg(feature = "cleanup-markdown")]
use pulldown_cmark::CodeBlockKind;
use pulldown_cmark::{Event, Options, Parser, Tag};
use regex::Regex;
use std::sync::LazyLock;

/// Comments on a Protobuf item.
#[derive(Debug, Default, Clone)]
pub struct Comments {
    /// Leading detached blocks of comments.
    pub leading_detached: Vec<Vec<String>>,

    /// Leading comments.
    pub leading: Vec<String>,

    /// Trailing comments.
    pub trailing: Vec<String>,
}

impl Comments {
    pub(crate) fn from_location(location: &Location) -> Comments {
        let leading_detached = location
            .leading_detached_comments
            .iter()
            .map(get_lines)
            .collect();
        let leading = location
            .leading_comments
            .as_ref()
            .map_or(Vec::new(), get_lines);
        let trailing = location
            .trailing_comments
            .as_ref()
            .map_or(Vec::new(), get_lines);
        Comments {
            leading_detached,
            leading,
            trailing,
        }
    }

    /// Appends the comments to a buffer with indentation.
    ///
    /// Each level of indentation corresponds to four space (' ') characters.
    pub fn append_with_indent(&self, indent_level: u8, buf: &mut String) {
        // Append blocks of detached comments.
        for detached_block in &self.leading_detached {
            for line in detached_block {
                for _ in 0..indent_level {
                    buf.push_str("    ");
                }
                buf.push_str("//");
                buf.push_str(&Self::sanitize_line(line));
                buf.push('\n');
            }
            buf.push('\n');
        }

        // Append leading comments.
        Self::append_doc_lines(&self.leading, indent_level, buf);

        // Append an empty comment line if there are leading and trailing comments.
        if !self.leading.is_empty() && !self.trailing.is_empty() {
            for _ in 0..indent_level {
                buf.push_str("    ");
            }
            buf.push_str("///\n");
        }

        // Append trailing comments.
        Self::append_doc_lines(&self.trailing, indent_level, buf);
    }

    fn append_doc_lines(lines: &[String], indent_level: u8, buf: &mut String) {
        let prefix = format!("{}///", "    ".repeat(indent_level.into()));
        let blocks = api_linter_blocks(lines);
        let mut blocks = blocks.into_iter().peekable();
        let mut start = 0;
        let mut lines = lines;
        while !lines.is_empty() {
            if blocks.peek().is_some_and(|block| block.start == start) {
                let block = blocks.next().unwrap();
                let len = block.end - block.start;
                let (metadata, rest) = lines.split_at(len);
                // Keep directive syntax out of Markdown prose, without escaping or
                // rewriting its rule names, reasons, links, or bracketed values.
                let fence_len = metadata
                    .iter()
                    .flat_map(|line| line.split(|c| c != '`'))
                    .map(str::len)
                    .max()
                    .unwrap_or(0)
                    .saturating_add(1)
                    .max(3);
                let fence = "`".repeat(fence_len);
                buf.push_str(&format!("{prefix}\n{prefix} {fence}text\n"));
                for line in metadata {
                    buf.push_str(&format!("{prefix} {line}\n"));
                }
                buf.push_str(&format!("{prefix} {fence}\n{prefix}\n"));
                lines = rest;
                start += len;
            } else {
                buf.push_str(&prefix);
                buf.push_str(&Self::sanitize_line(&lines[0]));
                buf.push('\n');
                lines = &lines[1..];
                start += 1;
            }
        }
    }

    /// Checks whether a RustDoc line should be indented.
    ///
    /// Lines should be indented if:
    /// - they are non-empty, AND
    ///   - they don't already start with a space
    ///     OR
    ///   - they start with several spaces.
    ///
    /// The last condition can happen in the case of multi-line Markdown lists
    /// such as:
    ///
    /// - this is a list
    ///   where some elements spans multiple lines
    /// - but not all elements
    fn should_indent(sanitized_line: &str) -> bool {
        let mut chars = sanitized_line.chars();
        chars
            .next()
            .is_some_and(|c| c != ' ' || chars.next() == Some(' '))
    }

    /// Sanitizes the line for rustdoc by performing the following operations:
    ///     - escape urls as <http://foo.com>
    ///     - escape `[` & `]` if not already escaped and not followed by a parenthesis or bracket
    fn sanitize_line(line: &str) -> String {
        static RULE_URL: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r"https?://[^\s)]+").unwrap());
        static RULE_BRACKETS: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r"(^|[^\]\\])\[(([^\]]*[^\\])?)\]([^(\[]|$)").unwrap());

        let mut s = RULE_URL.replace_all(line, r"<$0>").to_string();
        s = RULE_BRACKETS.replace_all(&s, r"$1\[$2\]$4").to_string();
        if Self::should_indent(&s) {
            s.insert(0, ' ');
        }
        s
    }
}

// API linter directives are metadata, not prose. Only recognize complete
// standalone directives; ordinary comments and incomplete annotations stay intact.
fn api_linter_block_len<S: AsRef<str>>(lines: &[S]) -> Option<usize> {
    if !lines
        .first()?
        .as_ref()
        .trim_start()
        .starts_with("(-- api-linter:")
    {
        return None;
    }
    for (index, line) in lines.iter().enumerate() {
        let line = line.as_ref().trim();
        if index > 0 && line.starts_with("(-- api-linter:") {
            return None;
        }
        if line.ends_with("--)") {
            return Some(index + 1);
        }
    }
    None
}

// Use Markdown source ranges rather than a second fence parser: code blocks
// may be nested in lists, blockquotes, and other Markdown containers.
fn api_linter_blocks<S: AsRef<str>>(lines: &[S]) -> Vec<std::ops::Range<usize>> {
    let mut source = lines
        .iter()
        .map(AsRef::as_ref)
        .collect::<Vec<_>>()
        .join("\n");
    let mut offset = 0;
    let offsets: Vec<_> = lines
        .iter()
        .map(|line| {
            let end = offset + line.as_ref().len();
            let range = offset..end;
            offset = end + 1;
            range
        })
        .collect();
    let mut blocks = Vec::new();
    let mut start = 0;
    while start < lines.len() {
        if let Some(len) = api_linter_block_len(&lines[start..]) {
            let opener = &offsets[start];
            let in_code =
                Parser::new_ext(&source, Options::all() - Options::ENABLE_SMART_PUNCTUATION)
                    .into_offset_iter()
                    .any(|(event, range)| {
                        matches!(event, Event::Start(Tag::CodeBlock(_)))
                            && opener.start < range.end
                            && range.start < opener.end
                    });
            if !in_code {
                // A complete annotation is opaque once its opener is outside
                // existing code. Its payload may itself contain fence syntax.
                // Mask it without moving source offsets so those payload fences
                // cannot affect classification of subsequent annotations.
                for range in &offsets[start..start + len] {
                    source.replace_range(range.clone(), &" ".repeat(range.len()));
                }
                blocks.push(start..start + len);
                start += len;
                continue;
            }
        }
        start += 1;
    }
    blocks
}

/// A service descriptor.
#[derive(Debug, Clone)]
pub struct Service {
    /// The service name in Rust style.
    pub name: String,
    /// The service name as it appears in the .proto file.
    pub proto_name: String,
    /// The package name as it appears in the .proto file.
    pub package: String,
    /// The service comments.
    pub comments: Comments,
    /// The service methods.
    pub methods: Vec<Method>,
    /// The service options.
    pub options: prost_types::ServiceOptions,
}

/// A service method descriptor.
#[derive(Debug, Clone)]
pub struct Method {
    /// The name of the method in Rust style.
    pub name: String,
    /// The name of the method as it appears in the .proto file.
    pub proto_name: String,
    /// The method comments.
    pub comments: Comments,
    /// The input Rust type.
    pub input_type: String,
    /// The output Rust type.
    pub output_type: String,
    /// The input Protobuf type.
    pub input_proto_type: String,
    /// The output Protobuf type.
    pub output_proto_type: String,
    /// The method options.
    pub options: prost_types::MethodOptions,
    /// Identifies if client streams multiple client messages.
    pub client_streaming: bool,
    /// Identifies if server streams multiple server messages.
    pub server_streaming: bool,
}

#[cfg(not(feature = "cleanup-markdown"))]
fn get_lines<S>(comments: S) -> Vec<String>
where
    S: AsRef<str>,
{
    comments.as_ref().lines().map(str::to_owned).collect()
}

#[cfg(feature = "cleanup-markdown")]
fn get_lines<S>(comments: S) -> Vec<String>
where
    S: AsRef<str>,
{
    let comments = comments.as_ref();
    let lines: Vec<_> = comments.lines().collect();
    let blocks = api_linter_blocks(&lines);
    if blocks.is_empty() {
        return get_markdown_lines(comments);
    }
    // Protect raw metadata with collision-free HTML placeholders, then parse
    // the entire document once. Parsing separate prose chunks loses fence,
    // list, and other Markdown context at the chunk boundaries.
    let mut marker = "prost-api-linter-metadata".to_owned();
    while comments.contains(&marker) {
        marker.push('_');
    }
    let mut protected = String::new();
    let mut start = 0;
    let mut replacements = Vec::new();
    for (index, block) in blocks.into_iter().enumerate() {
        for line in &lines[start..block.start] {
            protected.push_str(line);
            protected.push('\n');
        }
        let placeholder = format!("<!--{marker}:{index}-->");
        protected.push_str(&format!("\n{placeholder}\n\n"));
        replacements.push((placeholder, lines[block.clone()].join("\n")));
        start = block.end;
    }
    for line in &lines[start..] {
        protected.push_str(line);
        protected.push('\n');
    }
    let mut cleaned = get_markdown_lines(protected).join("\n");
    for (placeholder, metadata) in replacements {
        cleaned = cleaned.replace(&placeholder, &metadata);
    }
    cleaned.lines().map(str::to_owned).collect()
}

#[cfg(feature = "cleanup-markdown")]
fn get_markdown_lines<S>(comments: S) -> Vec<String>
where
    S: AsRef<str>,
{
    let comments = comments.as_ref();
    let mut buffer = String::with_capacity(comments.len() + 256);
    let opts = pulldown_cmark_to_cmark::Options {
        code_block_token_count: 3,
        ..Default::default()
    };
    match pulldown_cmark_to_cmark::cmark_with_options(
        Parser::new_ext(comments, Options::all() - Options::ENABLE_SMART_PUNCTUATION).map(
            |event| {
                fn map_codeblock(kind: CodeBlockKind) -> CodeBlockKind {
                    match kind {
                        CodeBlockKind::Fenced(s) => {
                            if s.as_ref() == "rust" {
                                CodeBlockKind::Fenced("compile_fail".into())
                            } else {
                                CodeBlockKind::Fenced(format!("text,{s}").into())
                            }
                        }
                        CodeBlockKind::Indented => CodeBlockKind::Fenced("text".into()),
                    }
                }
                match event {
                    Event::Start(Tag::CodeBlock(kind)) => {
                        Event::Start(Tag::CodeBlock(map_codeblock(kind)))
                    }
                    e => e,
                }
            },
        ),
        &mut buffer,
        opts,
    ) {
        Ok(_) => buffer.lines().map(str::to_owned).collect(),
        Err(_) => comments.lines().map(str::to_owned).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_linter_examples_preserve_existing_markdown_code() {
        for input in [
            " ```text\n (-- api-linter: core::0123::rule=disabled --)\n ```\n After prose.",
            " ~~~~text\n (-- api-linter: core::0123::rule=disabled --)\n ~~~\n ~~~~\n After prose.",
            "     (-- api-linter: core::0123::rule=disabled --)\n\n After prose.",
            "\t(-- api-linter: core::0123::rule=disabled --)\n\n After prose.",
            " - ```text\n   (-- api-linter: core::0123::rule=disabled --)\n   ```\n\n After prose.",
            " 1. ~~~text\n    (-- api-linter: core::0123::rule=disabled --)\n    ~~~\n\n After prose.",
            " > ```text\n > (-- api-linter: core::0123::rule=disabled --)\n > ```\n\n After prose.",
            " > - ```text\n >   (-- api-linter: core::0123::rule=disabled --)\n >   ```\n\n After prose.",
            " - ```text\n   (-- api-linter: core::0123::unclosed=disabled\n\n After prose.",
        ] {
            // Both the public Comments API and descriptor comment loading must
            // retain the baseline code structure, without extra metadata fences.
            let raw: Vec<String> = input.lines().map(str::to_owned).collect();
            #[cfg(feature = "cleanup-markdown")]
            let baseline = get_markdown_lines(input);
            #[cfg(not(feature = "cleanup-markdown"))]
            let baseline = raw.clone();
            for lines in [raw.clone(), get_lines(input)] {
                assert!(api_linter_blocks(&lines).is_empty(), "{input}");
                let expected_lines = if lines == raw { &raw } else { &baseline };
                let expected: String = expected_lines
                    .iter()
                    .map(|line| format!("///{}\n", Comments::sanitize_line(line)))
                    .collect();
                let mut actual = String::new();
                Comments {
                    leading: lines,
                    ..Default::default()
                }
                .append_with_indent(0, &mut actual);
                assert_eq!(actual, expected, "{input}");
            }
        }
    }

    #[test]
    fn incomplete_api_linter_metadata_cannot_consume_a_later_directive() {
        let broken = " (-- api-linter: core::0123::broken=disabled";
        let valid = " (-- api-linter: core::0456::valid=disabled --)";
        let input = format!("{broken}\n Important prose.\n{valid}\n After.");
        for lines in [
            input.lines().map(str::to_owned).collect(),
            get_lines(&input),
        ] {
            let blocks = api_linter_blocks(&lines);
            assert_eq!(blocks.len(), 1);
            assert_eq!(&lines[blocks[0].clone()], &[valid.to_owned()]);
            let mut actual = String::new();
            Comments {
                leading: lines,
                ..Default::default()
            }
            .append_with_indent(0, &mut actual);
            let fence = actual.find("/// ```text\n").unwrap();
            assert!(actual[..fence].contains("core::0123::broken=disabled"));
            assert!(actual[..fence].contains("Important prose."));
            assert!(actual[fence..].starts_with(&format!("/// ```text\n/// {valid}\n/// ```\n")));
            assert!(actual.ends_with("/// After.\n"));
        }
    }

    #[cfg(feature = "cleanup-markdown")]
    #[test]
    fn cleanup_preserves_code_context_before_and_after_metadata() {
        let directive = " (-- api-linter: core::0123::rule=disabled --)";
        let input = format!(
            " ```text\n{directive}\n ```\n\n{directive}\n\n ```text\n{directive}\n ```\n After prose."
        );
        let lines = get_lines(&input);
        let blocks = api_linter_blocks(&lines);
        assert_eq!(blocks.len(), 1);
        assert_eq!(&lines[blocks[0].clone()], &[directive.to_owned()]);
        let cleaned = lines.join("\n");
        assert_eq!(cleaned.matches("```text,text").count(), 2);
        assert!(cleaned.ends_with("```\n\nAfter prose."));
    }

    #[test]
    fn api_linter_metadata_is_fenced_and_preserved() {
        for metadata in [
            " (-- api-linter: core::0123::some_rule=disabled --)",
            " (-- api-linter: core::0456::other_rule=disabled\n     aip.dev/not-precedent: ProtoBuf [values] https://example.com\n     reason includes ``` and `inline_code`. --)",
            " (-- api-linter: core::0123::rule=disabled\n ```\n Reason [values] https://example.com\n --)",
        ] {
            for trailing in [false, true] {
                let location = Location {
                    leading_comments: (!trailing).then(|| metadata.to_owned()),
                    trailing_comments: trailing.then(|| metadata.to_owned()),
                    ..Default::default()
                };
                let comments = Comments::from_location(&location);
                let mut actual = String::new();
                comments.append_with_indent(1, &mut actual);
                let fence = if metadata.contains("```") { "````" } else { "```" };
                let mut expected = format!("    ///\n    /// {fence}text\n");
                for line in metadata.lines() {
                    expected.push_str(&format!("    /// {line}\n"));
                }
                expected.push_str(&format!("    /// {fence}\n    ///\n"));
                assert_eq!(expected, actual);
            }
        }
    }

    #[test]
    fn api_linter_fence_payload_cannot_enclose_later_prose_or_metadata() {
        let first = " (-- api-linter: core::0123::rule=disabled\n ```\n Reason [values] https://example.com\n --)";
        let second = " (-- api-linter: core::0456::rule=disabled --)";
        let input = format!("{first}\n After prose.\n{second}");
        for lines in [
            input.lines().map(str::to_owned).collect(),
            get_lines(&input),
        ] {
            let mut actual = String::new();
            Comments {
                leading: lines,
                ..Default::default()
            }
            .append_with_indent(0, &mut actual);
            let expected_payload: String =
                first.lines().map(|line| format!("/// {line}\n")).collect();
            assert!(actual.contains(&format!("/// ````text\n{expected_payload}/// ````\n")));
            assert!(actual.contains(&format!("/// ```text\n/// {second}\n/// ```\n")));
            let docs = actual
                .lines()
                .map(|line| line.strip_prefix("///").unwrap())
                .collect::<Vec<_>>()
                .join("\n");
            let mut in_code = false;
            let mut after_prose = false;
            for event in Parser::new(&docs) {
                match event {
                    Event::Start(Tag::CodeBlock(_)) => in_code = true,
                    Event::End(pulldown_cmark::TagEnd::CodeBlock) => in_code = false,
                    Event::Text(text) if text.contains("After prose.") => {
                        assert!(!in_code, "{docs}");
                        after_prose = true;
                    }
                    _ => {}
                }
            }
            assert!(after_prose, "{docs}");
        }
    }

    #[test]
    fn api_linter_metadata_keeps_surrounding_prose_and_detached_comments() {
        let directive = " (-- api-linter: core::0123::some_rule=disabled --)";
        let comments = Comments::from_location(&Location {
            leading_comments: Some(format!(" Before.\n{directive}\n{directive}\n After.\n")),
            trailing_comments: Some(" Trailing.".into()),
            leading_detached_comments: vec![directive.into()],
            ..Default::default()
        });
        let mut actual = String::new();
        comments.append_with_indent(0, &mut actual);
        assert!(actual.starts_with(&format!("//{directive}\n\n")));
        assert_eq!(actual.matches("/// ```text\n").count(), 2);
        assert!(actual.contains("/// Before.\n"));
        assert!(actual.ends_with("/// After.\n///\n/// Trailing.\n"));
    }

    #[test]
    fn ordinary_or_incomplete_annotations_are_not_classified_as_metadata() {
        for line in [
            " (-- api-linter: core::0123::some_rule=disabled",
            " Mention (-- api-linter: core::0123::some_rule=disabled --) in prose.",
            " (-- other-tool: some_rule=disabled --)",
        ] {
            let comments = Comments {
                leading: vec![line.into()],
                ..Default::default()
            };
            let mut actual = String::new();
            comments.append_with_indent(0, &mut actual);
            assert_eq!(actual, format!("///{line}\n"));
        }
    }

    #[test]
    fn test_comment_append_with_indent_leaves_prespaced_lines() {
        struct TestCases {
            name: &'static str,
            input: String,
            expected: String,
        }

        let tests = vec![
            TestCases {
                name: "existing_space",
                input: " A line with a single leading space.".to_string(),
                expected: "/// A line with a single leading space.\n".to_string(),
            },
            TestCases {
                name: "non_existing_space",
                input: "A line without a single leading space.".to_string(),
                expected: "/// A line without a single leading space.\n".to_string(),
            },
            TestCases {
                name: "empty",
                input: "".to_string(),
                expected: "///\n".to_string(),
            },
            TestCases {
                name: "multiple_leading_spaces",
                input: "  a line with several leading spaces, such as in a markdown list"
                    .to_string(),
                expected: "///   a line with several leading spaces, such as in a markdown list\n"
                    .to_string(),
            },
        ];
        for t in tests {
            let input = Comments {
                leading_detached: vec![],
                leading: vec![],
                trailing: vec![t.input],
            };

            let mut actual = "".to_string();
            input.append_with_indent(0, &mut actual);

            assert_eq!(t.expected, actual, "failed {}", t.name);
        }
    }

    #[test]
    fn test_comment_append_with_indent_sanitizes_comment_doc_url() {
        struct TestCases {
            name: &'static str,
            input: String,
            expected: String,
        }

        let tests = vec![
            TestCases {
                name: "valid_http",
                input: "See https://www.rust-lang.org/".to_string(),
                expected: "/// See <https://www.rust-lang.org/>\n".to_string(),
            },
            TestCases {
                name: "valid_https",
                input: "See https://www.rust-lang.org/".to_string(),
                expected: "/// See <https://www.rust-lang.org/>\n".to_string(),
            },
            TestCases {
                name: "valid_https_parenthesis",
                input: "See (https://www.rust-lang.org/)".to_string(),
                expected: "/// See (<https://www.rust-lang.org/>)\n".to_string(),
            },
            TestCases {
                name: "invalid",
                input: "See note://abc".to_string(),
                expected: "/// See note://abc\n".to_string(),
            },
        ];
        for t in tests {
            let input = Comments {
                leading_detached: vec![],
                leading: vec![],
                trailing: vec![t.input],
            };

            let mut actual = "".to_string();
            input.append_with_indent(0, &mut actual);

            assert_eq!(t.expected, actual, "failed {}", t.name);
        }
    }

    #[test]
    fn test_comment_append_with_indent_sanitizes_square_brackets() {
        struct TestCases {
            name: &'static str,
            input: String,
            expected: String,
        }

        let tests = vec![
            TestCases {
                name: "valid_brackets",
                input: "foo [bar] baz".to_string(),
                expected: "/// foo \\[bar\\] baz\n".to_string(),
            },
            TestCases {
                name: "invalid_start_bracket",
                input: "foo [= baz".to_string(),
                expected: "/// foo [= baz\n".to_string(),
            },
            TestCases {
                name: "invalid_end_bracket",
                input: "foo =] baz".to_string(),
                expected: "/// foo =] baz\n".to_string(),
            },
            TestCases {
                name: "invalid_bracket_combination",
                input: "[0, 9)".to_string(),
                expected: "/// [0, 9)\n".to_string(),
            },
            TestCases {
                name: "valid_brackets_parenthesis",
                input: "foo [bar](bar) baz".to_string(),
                expected: "/// foo [bar](bar) baz\n".to_string(),
            },
            TestCases {
                name: "valid_brackets_end",
                input: "foo [bar]".to_string(),
                expected: "/// foo \\[bar\\]\n".to_string(),
            },
            TestCases {
                name: "valid_brackets_no_parenthesis",
                input: "foo [bar]baz".to_string(),
                expected: "/// foo \\[bar\\]baz\n".to_string(),
            },
            TestCases {
                name: "valid_empty_brackets",
                input: "foo []".to_string(),
                expected: "/// foo \\[\\]\n".to_string(),
            },
            TestCases {
                name: "valid_empty_brackets_parenthesis",
                input: "foo []()".to_string(),
                expected: "/// foo []()\n".to_string(),
            },
            TestCases {
                name: "valid_brackets_brackets",
                input: "foo [bar][bar] baz".to_string(),
                expected: "/// foo [bar][bar] baz\n".to_string(),
            },
            TestCases {
                name: "valid_brackets_brackets_end",
                input: "foo [bar][baz]".to_string(),
                expected: "/// foo [bar][baz]\n".to_string(),
            },
            TestCases {
                name: "valid_brackets_brackets_all",
                input: "[bar][baz]".to_string(),
                expected: "/// [bar][baz]\n".to_string(),
            },
            TestCases {
                name: "escaped_brackets",
                input: "\\[bar\\]\\[baz\\]".to_string(),
                expected: "/// \\[bar\\]\\[baz\\]\n".to_string(),
            },
            TestCases {
                name: "escaped_empty_brackets",
                input: "\\[\\]\\[\\]".to_string(),
                expected: "/// \\[\\]\\[\\]\n".to_string(),
            },
        ];
        for t in tests {
            let input = Comments {
                leading_detached: vec![],
                leading: vec![],
                trailing: vec![t.input],
            };

            let mut actual = "".to_string();
            input.append_with_indent(0, &mut actual);

            assert_eq!(t.expected, actual, "failed {}", t.name);
        }
    }

    #[test]
    fn test_codeblocks() {
        struct TestCase {
            name: &'static str,
            input: &'static str,
            #[allow(unused)]
            cleanedup_expected: Vec<&'static str>,
        }

        let tests = vec![
            TestCase {
                name: "unlabelled_block",
                input: "    thingy\n",
                cleanedup_expected: vec!["", "```text", "thingy", "```"],
            },
            TestCase {
                name: "rust_block",
                input: "```rust\nfoo.bar()\n```\n",
                cleanedup_expected: vec!["", "```compile_fail", "foo.bar()", "```"],
            },
            TestCase {
                name: "js_block",
                input: "```javascript\nfoo.bar()\n```\n",
                cleanedup_expected: vec!["", "```text,javascript", "foo.bar()", "```"],
            },
        ];

        for t in tests {
            let loc = Location {
                path: vec![],
                span: vec![],
                leading_comments: Some(t.input.into()),
                trailing_comments: None,
                leading_detached_comments: vec![],
            };
            let comments = Comments::from_location(&loc);
            #[cfg(feature = "cleanup-markdown")]
            let expected = t.cleanedup_expected;
            #[cfg(not(feature = "cleanup-markdown"))]
            let expected: Vec<&str> = t.input.lines().collect();
            assert_eq!(expected, comments.leading, "failed {}", t.name);
        }
    }
}
