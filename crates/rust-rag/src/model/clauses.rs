//! Derive literal direction proposals for separate verification against the full source.

const MAX_DOCUMENT: usize = 12_000;
const MAX_QUOTE: usize = 300;
const MAX_PROPOSALS: usize = 16;

const NONFACTUAL_HEADINGS: &[&str] = &[
    "example",
    "examples",
    "counterexample",
    "counterexamples",
    "hypothetical",
    "hypotheticals",
    "illustration",
    "illustrations",
    "fictional",
    "draft",
    "mock",
    "simulation",
    "simulations",
    "training",
    "exercise",
    "exercises",
];

const HYPOTHETICAL_LEAD_INS: &[&str] = &[
    "assume",
    "assuming",
    "hypothetically",
    "if",
    "imagine",
    "suppose",
    "supposing",
];

const NONCURRENT_STATUS: &[&str] = &[
    "archived",
    "historical",
    "obsolete",
    "retracted",
    "superseded",
    "withdrawn",
];

const AMBIGUOUS_WORDS: &[&str] = &[
    "a",
    "an",
    "all",
    "any",
    "both",
    "each",
    "every",
    "few",
    "many",
    "most",
    "much",
    "none",
    "nobody",
    "nothing",
    "everyone",
    "someone",
    "some",
    "several",
    "either",
    "neither",
    "another",
    "multiple",
    "various",
    "i",
    "we",
    "you",
    "it",
    "he",
    "she",
    "they",
    "them",
    "us",
    "this",
    "that",
    "these",
    "those",
    "who",
    "which",
    "his",
    "her",
    "their",
    "our",
    "its",
    "not",
    "no",
    "never",
    "nor",
    "without",
    "cannot",
    "can",
    "may",
    "might",
    "could",
    "would",
    "should",
    "must",
    "will",
    "shall",
    "does",
    "do",
    "did",
    "if",
    "unless",
    "whether",
    "since",
    "until",
    "and",
    "or",
    "instead",
    "except",
    "during",
    "to",
    "for",
    "only",
    "solely",
    "possibly",
    "perhaps",
    "probably",
    "reportedly",
    "allegedly",
    "supposedly",
    "unconfirmed",
    "former",
    "formerly",
    "previously",
    "proposed",
    "planned",
    "draft",
    "example",
    "examples",
    "hypothetical",
    "obsolete",
    "superseded",
    "claims",
    "said",
    "says",
    "suggests",
    "assumes",
    "according",
    "confirm",
    "ensure",
    "seldom",
];

const FORMS: &[(&str, &str, bool)] = &[
    ("depends on", "depends_on", false),
    ("depend on", "depends_on", false),
    ("is depended on by", "depends_on", true),
    ("are depended on by", "depends_on", true),
    ("requires", "requires", false),
    ("require", "requires", false),
    ("is required by", "requires", true),
    ("are required by", "requires", true),
    ("is owned by", "owned_by", false),
    ("are owned by", "owned_by", false),
    ("owns", "owned_by", true),
    ("own", "owned_by", true),
    ("is part of", "part_of", false),
    ("are part of", "part_of", false),
    ("uses", "uses", false),
    ("use", "uses", false),
    ("is used by", "uses", true),
    ("are used by", "uses", true),
];

const ABBREVIATIONS: &[&str] = &[
    "Mr", "Ms", "Mrs", "Dr", "Prof", "No", "Rev", "Fig", "Inc", "Ltd", "vs", "e.g", "i.e",
];

#[derive(Debug, Eq, PartialEq)]
pub(super) struct Clause<'a> {
    pub(super) subject: &'a str,
    pub(super) predicate: &'static str,
    pub(super) object: &'a str,
    pub(super) quote: &'a str,
    pub(super) quote_start: usize,
    pub(super) quote_end: usize,
}

fn space(c: char) -> bool {
    c.is_whitespace() || matches!(c, '\u{1c}'..='\u{1f}')
}

fn word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn unique_start(text: &str, item: &str) -> Option<usize> {
    let start = text.find(item)?;
    let after_first = start + item.chars().next()?.len_utf8();
    (!text[after_first..].contains(item)).then_some(start)
}

fn source_lines(document: &str) -> impl Iterator<Item = &str> {
    let mut breaks = document
        .char_indices()
        .filter(|(_, c)| {
            matches!(
                c,
                '\r' | '\n' | '\u{0b}' | '\u{0c}' | '\u{1c}'
                    ..='\u{1e}' | '\u{85}' | '\u{2028}' | '\u{2029}'
            )
        })
        .peekable();
    let mut start = 0;
    std::iter::from_fn(move || {
        if start == document.len() {
            return None;
        }
        let mut end = document.len();
        if let Some((i, mark)) = breaks.next() {
            end = i + mark.len_utf8();
            if mark == '\r' && breaks.peek() == Some(&(end, '\n')) {
                breaks.next();
                end += 1;
            }
        }
        let line = &document[start..end];
        start = end;
        Some(line)
    })
}

fn markdown_content(line: &str) -> Option<&str> {
    let text = line.trim_start_matches(' ');
    (line.len() - text.len() <= 3).then_some(text)
}

fn atx(line: &str) -> Option<(usize, &str)> {
    let text = markdown_content(line)?;
    let level = text.bytes().take_while(|&byte| byte == b'#').count();
    if !(1..=6).contains(&level) {
        return None;
    }
    let title = &text[level..];
    (title.is_empty() || title.starts_with([' ', '\t']))
        .then_some((level, title.trim_start_matches([' ', '\t'])))
}

fn setext(line: &str) -> Option<usize> {
    let text = markdown_content(line)?.trim_end_matches([' ', '\t']);
    let mark = *text.as_bytes().first()?;
    if !matches!(mark, b'=' | b'-') || !text.bytes().all(|byte| byte == mark) {
        return None;
    }
    Some(if mark == b'=' { 1 } else { 2 })
}

fn fence(line: &str) -> Option<(u8, usize, &str)> {
    let text = markdown_content(line)?;
    let mark = *text.as_bytes().first()?;
    if !matches!(mark, b'\x60' | b'~') {
        return None;
    }
    let length = text.bytes().take_while(|&byte| byte == mark).count();
    (length >= 3).then_some((mark, length, &text[length..]))
}

fn scope_noun(word: &str) -> bool {
    const NOUNS: &[&str] = &[
        "assertion",
        "claim",
        "configuration",
        "context",
        "dependency",
        "design",
        "document",
        "entry",
        "evidence",
        "graph",
        "material",
        "note",
        "record",
        "relation",
        "relationship",
        "revision",
        "section",
        "statement",
        "topology",
        "wiring",
    ];
    matches!(word, "dependencies" | "entries")
        || NOUNS.contains(&word.strip_suffix('s').unwrap_or(word))
}

fn status_heading(title: &str) -> bool {
    let title = title
        .trim_matches(|c| space(c) || matches!(c, '#' | '*' | '_' | ':' | '.'))
        .to_ascii_lowercase();
    let title = title.as_str();
    if NONCURRENT_STATUS.contains(&title)
        || matches!(
            title,
            "archive" | "history" | "retraction" | "retraction notice"
        )
    {
        return true;
    }
    if ["(", "[", " - ", " — ", " – "].iter().any(|&delimiter| {
        title.rsplit_once(delimiter).is_some_and(|(_, status)| {
            NONCURRENT_STATUS.contains(&status.trim_end_matches([')', ']']).trim())
        })
    }) {
        return true;
    }
    let mut words = title.split_ascii_whitespace();
    let Some(status) = words.next() else {
        return false;
    };
    if !NONCURRENT_STATUS.contains(&status) {
        return false;
    }
    let Some(noun) = words.next_back() else {
        return false;
    };
    scope_noun(noun)
        && words.all(|word| {
            matches!(
                word,
                "configuration"
                    | "dependency"
                    | "design"
                    | "equipment"
                    | "graph"
                    | "operating"
                    | "operational"
                    | "relationship"
                    | "technical"
            )
        })
}

fn status_lead_in(sentence: &str) -> bool {
    let sentence = sentence
        .trim_matches(|c| space(c) || matches!(c, '*' | '_' | '.' | ':' | '!' | '?'))
        .to_ascii_lowercase();
    [
        " is ",
        " are ",
        " was ",
        " were ",
        " has been ",
        " have been ",
    ]
    .iter()
    .any(|&verb| {
        sentence.rsplit_once(verb).is_some_and(|(subject, status)| {
            let demonstrative = ["this ", "these ", "the following "]
                .iter()
                .any(|start| subject.starts_with(start));
            let references_text = subject.ends_with(" below");
            (demonstrative || references_text)
                && subject.split_ascii_whitespace().any(scope_noun)
                && (NONCURRENT_STATUS.contains(&status)
                    || matches!(
                        status,
                        "no longer current" | "for historical reference only"
                    ))
        })
    })
}

fn status_label(text: &str) -> bool {
    status_heading(text)
        || text.split_once(':').is_some_and(|(label, value)| {
            status_heading(label)
                || (label
                    .trim_matches(|c| space(c) || matches!(c, '*' | '_'))
                    .eq_ignore_ascii_case("status")
                    && status_heading(value))
        })
}

fn push_heading(
    headings: &mut Vec<(usize, bool)>,
    excluded_scope: &mut Option<usize>,
    level: usize,
    title: &str,
) {
    while headings.last().is_some_and(|(parent, _)| *parent >= level) {
        headings.pop();
    }
    if excluded_scope.is_some_and(|parent| parent == 0 || level <= parent) {
        *excluded_scope = None;
    }
    let blocked = hypothetical_lead_in(title)
        || status_label(title)
        || title.split(|c| !word_char(c)).any(|word| {
            NONFACTUAL_HEADINGS
                .iter()
                .any(|term| word.eq_ignore_ascii_case(term))
        });
    headings.push((level, blocked));
}

fn finish_paragraph(
    paragraphs: &mut Vec<(usize, usize)>,
    document: &str,
    headings: &[(usize, bool)],
    excluded_scope: &mut Option<usize>,
    start: Option<usize>,
    end: usize,
) {
    let Some(start) = start else { return };
    if excluded_scope.is_some() || headings.iter().any(|(_, blocked)| *blocked) {
        return;
    }
    // A scope marker can apply across blank lines, chunks and nested headings.
    let paragraph = &document[start..end];
    let lead_in = if source_lines(paragraph)
        .next()
        .is_some_and(|line| status_label(line) || status_lead_in(line))
    {
        Some(start)
    } else {
        sentences(paragraph).find_map(|(from, to)| {
            let sentence = &paragraph[from..to];
            (hypothetical_lead_in(sentence) || status_lead_in(sentence)).then_some(start + from)
        })
    };
    if lead_in.is_some() {
        *excluded_scope = Some(headings.last().map_or(0, |(level, _)| *level));
    }
    let stop = lead_in.unwrap_or(end);
    if start < stop {
        paragraphs.push((start, stop));
    }
}

fn paragraphs(document: &str) -> Vec<(usize, usize)> {
    let mut result = Vec::new();
    let mut headings = Vec::new();
    let mut excluded_scope = None;
    let mut paragraph = None;
    let mut open_fence = None;
    let mut quote_block = false;
    let mut comment = false;
    let mut offset = 0;

    for raw in source_lines(document) {
        let start = offset;
        offset += raw.len();
        let line = raw.trim_end_matches(['\r', '\n']);
        let fence_line = fence(line);
        if let Some((opening, minimum)) = open_fence {
            if fence_line.is_some_and(|(mark, length, tail)| {
                mark == opening && length >= minimum && tail.trim_matches(space).is_empty()
            }) {
                open_fence = None;
            }
            continue;
        }
        if comment {
            if line.contains("-->") {
                comment = false;
            }
            continue;
        }

        let title = atx(line);
        let trimmed = line.trim_start_matches(space);
        let blocked_line = markdown_content(line).is_some_and(|text| text.starts_with('>'))
            || line.starts_with("    ")
            || line.starts_with('\t')
            || trimmed.starts_with('<');
        if let (Some(level), Some(begin)) = (setext(line), paragraph) {
            push_heading(
                &mut headings,
                &mut excluded_scope,
                level,
                &document[begin..start],
            );
            paragraph = None;
        } else if title.is_some() || fence_line.is_some() || blocked_line || trimmed.is_empty() {
            finish_paragraph(
                &mut result,
                document,
                &headings,
                &mut excluded_scope,
                paragraph.take(),
                start,
            );
            quote_block = false;
            if let Some((level, title)) = title {
                push_heading(&mut headings, &mut excluded_scope, level, title);
            } else if let Some((mark, length, _)) = fence_line {
                open_fence = Some((mark, length));
            } else if blocked_line {
                quote_block = true;
                comment = trimmed.starts_with("<!--") && !line.contains("-->");
            }
        } else if !quote_block && paragraph.is_none() {
            paragraph = Some(start);
        }
    }
    finish_paragraph(
        &mut result,
        document,
        &headings,
        &mut excluded_scope,
        paragraph,
        offset,
    );
    result
}

fn abbreviation(fragment: &str) -> bool {
    let word = fragment
        .strip_suffix('.')
        .expect("sentence ends in a period");
    let token_boundary = |prefix: &str| prefix.chars().next_back().is_none_or(|c| !word_char(c));
    if let Some(last) = word.chars().next_back()
        && last.is_ascii_uppercase()
        && token_boundary(&word[..word.len() - last.len_utf8()])
    {
        return true;
    }
    ABBREVIATIONS
        .iter()
        .any(|term| word.strip_suffix(term).is_some_and(token_boundary))
}

fn sentences(paragraph: &str) -> impl Iterator<Item = (usize, usize)> {
    let mut cursor = 0;
    paragraph
        .char_indices()
        .filter_map(move |(position, mark)| {
            if !matches!(mark, '.' | '!' | '?') {
                return None;
            }
            let last = position + 1;
            if paragraph[last..].chars().next().is_some_and(|c| !space(c)) {
                return None;
            }
            let fragment = &paragraph[cursor..last];
            if mark == '.' && abbreviation(fragment) {
                return None;
            }
            let quote = fragment.trim_start_matches(space);
            cursor = last;
            Some((last - quote.len(), last))
        })
}

fn hypothetical_lead_in(sentence: &str) -> bool {
    sentence
        .split(|c| !word_char(c))
        .next()
        .is_some_and(|first| {
            HYPOTHETICAL_LEAD_INS
                .iter()
                .any(|marker| first.eq_ignore_ascii_case(marker))
        })
}

fn endpoint(text: &str) -> Option<&str> {
    let text = match text.get(..4) {
        Some(article) if article.eq_ignore_ascii_case("the ") => &text[4..],
        _ => text,
    };
    if !text.chars().next().is_some_and(char::is_uppercase) || text.chars().count() > 160 {
        return None;
    }
    let words = text.split(' ');
    if words.clone().count() > 12 {
        return None;
    }
    for word in words {
        let catalog = word.contains(['-', '_']) && word.chars().any(char::is_numeric);
        if !word.starts_with(char::is_alphanumeric)
            || !word.ends_with(char::is_alphanumeric)
            || !word
                .chars()
                .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_'))
            || (!catalog
                && word.split(['-', '_']).any(|part| {
                    AMBIGUOUS_WORDS
                        .iter()
                        .any(|term| part.eq_ignore_ascii_case(term))
                }))
        {
            return None;
        }
    }
    Some(text)
}

fn form_length(text: &str, form: &str) -> Option<usize> {
    let mut rest = text;
    for token in form.split(' ') {
        let end = rest.find(space)?;
        if !rest[..end].eq_ignore_ascii_case(token) {
            return None;
        }
        rest = rest[end..].trim_start_matches(space);
    }
    Some(text.len() - rest.len())
}

fn roles(quote: &str) -> Option<(&str, &'static str, &str)> {
    let text = quote.strip_suffix('.')?;
    let mut matched = None;
    let mut next = 0;
    for (start, c) in text.char_indices() {
        if start < next || !space(c) {
            continue;
        }
        let rest = text[start..].trim_start_matches(space);
        next = text.len() - rest.len();
        if let Some((length, predicate, reverse)) =
            FORMS.iter().find_map(|(form, predicate, reverse)| {
                form_length(rest, form).map(|length| (length, *predicate, *reverse))
            })
        {
            if matched.is_some() {
                return None;
            }
            next += length;
            matched = Some((start, next, predicate, reverse));
        }
    }
    let (start, end, predicate, reverse) = matched?;
    let (left, right) = (endpoint(&text[..start])?, endpoint(&text[end..])?);
    if left.to_lowercase() == right.to_lowercase() {
        return None;
    }
    Some(if reverse {
        (right, predicate, left)
    } else {
        (left, predicate, right)
    })
}

/// Find only clauses that are unique in the full source and wholly within the active passage.
/// The caller must still verify direction and factuality in full document context.
pub(super) fn proposals<'a>(document: &'a str, passage: &str) -> Vec<Clause<'a>> {
    if passage.trim_matches(space).is_empty()
        || document.chars().count() > MAX_DOCUMENT
        || passage.len() > document.len()
    {
        return Vec::new();
    }
    let Some(passage_start) = unique_start(document, passage) else {
        return Vec::new();
    };
    let passage_end = passage_start + passage.len();
    let mut result = Vec::new();
    for (begin, end) in paragraphs(document) {
        if end <= passage_start || begin >= passage_end {
            continue;
        }
        for (start, stop) in sentences(&document[begin..end]) {
            let (start, stop) = (begin + start, begin + stop);
            let quote = &document[start..stop];
            if start < passage_start || stop > passage_end || quote.chars().count() > MAX_QUOTE {
                continue;
            }
            let Some((subject, predicate, object)) = roles(quote) else {
                continue;
            };
            if unique_start(document, quote) != Some(start) {
                continue;
            }
            let quote_start = document[..start].chars().count();
            result.push(Clause {
                subject,
                predicate,
                object,
                quote,
                quote_start,
                quote_end: quote_start + quote.chars().count(),
            });
            if result.len() > MAX_PROPOSALS {
                return Vec::new();
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check<'a>(
        document: &'a str,
        passage: &str,
        expected: &[(&str, &str, &str)],
    ) -> Vec<Clause<'a>> {
        let rows = proposals(document, passage);
        let triples: Vec<_> = rows
            .iter()
            .map(|row| (row.subject, row.predicate, row.object))
            .collect();
        assert_eq!(triples, expected);
        for row in &rows {
            let literal: String = document
                .chars()
                .skip(row.quote_start)
                .take(row.quote_end - row.quote_start)
                .collect();
            assert_eq!(literal, row.quote);
            assert!(row.quote.ends_with('.'));
        }
        rows
    }

    #[test]
    fn exact_unicode_offsets_with_mixed_case_prose() {
        let document = "# Équipement actuel\r\n\r\nChecks begin at noon.  \
                        Étoile vacuum switch uses Bendix interlock. \
                        The Bendix interlock is part of the Alder assembly. Two bays are open.\r\n";
        let passage = document.split_once("Checks").unwrap().1;
        let rows = check(
            document,
            passage,
            &[
                ("Étoile vacuum switch", "uses", "Bendix interlock"),
                ("Bendix interlock", "part_of", "Alder assembly"),
            ],
        );
        assert_eq!(
            rows[0].quote_start,
            document[..document.find("Étoile").unwrap()].chars().count()
        );
    }

    #[test]
    fn source_grammar_orders_roles_in_both_voices() {
        let document = "Beacon Relay depends on Cedar Switch. Alder Regulator is depended on by Tamarack Panel. \
                        PC-124 requires EX-470. NT-488 is required by KU-726. \
                        Wren Works owns Fir Motor. Fir Motor is owned by Wren Works. \
                        VF-171 uses BS-863. MX-564 is used by DK-907. PX-350 is part of RF-711.";
        check(
            document,
            document,
            &[
                ("Beacon Relay", "depends_on", "Cedar Switch"),
                ("Tamarack Panel", "depends_on", "Alder Regulator"),
                ("PC-124", "requires", "EX-470"),
                ("KU-726", "requires", "NT-488"),
                ("Fir Motor", "owned_by", "Wren Works"),
                ("Fir Motor", "owned_by", "Wren Works"),
                ("VF-171", "uses", "BS-863"),
                ("DK-907", "uses", "MX-564"),
                ("PX-350", "part_of", "RF-711"),
            ],
        );
    }

    #[test]
    fn heading_scope_crosses_chunks_and_resets_at_the_next_peer() {
        let example = "Hawthorn Panel requires Ash Valve.";
        let live = "Willow gateway requires Fir radio.";
        for heading in [
            "Examples",
            "Hypothetical",
            "Draft",
            "Mock",
            "Simulation",
            "Training exercise",
        ] {
            let document = format!(
                "# Operating notes\n\n## {heading}\n\n{example}\n\n\
                 ### Installation\n\nPine hub uses Yew radio.\n\n## Live equipment\n\n{live}\n"
            );
            check(&document, example, &[]);
            check(
                &document,
                live,
                &[("Willow gateway", "requires", "Fir radio")],
            );
            check(
                &document,
                &document,
                &[("Willow gateway", "requires", "Fir radio")],
            );
        }
        let document = "Examples\n========\n\n## Nested\n\nAA-901 uses ZZ-141.\n\n\
                        Active equipment\n================\n\nNO-426 uses RJ-258.\n\n\
                        Examples\n--------\n\nVG-804 uses KH-154.\n\n\
                        Installed units\n---------------\n\nFZ-220 uses RG-494.\n";
        check(
            document,
            document,
            &[("NO-426", "uses", "RJ-258"), ("FZ-220", "uses", "RG-494")],
        );
    }

    #[test]
    fn ignores_code_quotes_and_comments() {
        let document = "# Normal\n\n\x60\x60\x60\x60text\n# Code heading\n\nAA-901 uses ZZ-141.\n\
                        \x60\x60\x60\nBB-852 uses YU-581.\n\x60\x60\x60\x60\n\n\
                        ~~~\nCC-335 uses VR-552.\n~~~\n\n\
                        > Historical fixture\nDD-154 uses TC-391.\n\n\
                        \x20\x20\x20\x20EE-509 uses SK-344.\n\n\
                        <!-- Archived inventory\n\nFF-426 uses RJ-258.\n-->\n\n\
                        GG-697 uses PP-628.\n";
        check(document, document, &[("GG-697", "uses", "PP-628")]);
    }

    #[test]
    fn skips_ambiguous_clauses_and_clipped_quotes() {
        let document = "No Pine Module uses Ash Relay. It uses Ash Relay. \
                        Pine Module does not use Ash Relay. Pine Module may use Ash Relay. \
                        Pine Module reportedly uses Ash Relay. Only Pine Module uses Ash Relay. \
                        Pine Module uses only Ash Relay. The ticket says Pine Module uses Ash Relay. \
                        Pine Module requires Ash Relay uses Oak Panel. Pine Module requires Pine Module. \
                        Dr. Elm uses Pine Valve. Birch Gate uses Alder Valve.";
        check(document, document, &[("Birch Gate", "uses", "Alder Valve")]);
        check(document, "Gate uses Alder Valve.", &[]);
    }

    #[test]
    fn withholds_qualified_endpoints_without_clipping_them() {
        let document = "Nobody owns ZK-14. Nothing requires Relay-9. Everyone uses Relay-9. \
                        Someone owns Pump-3. Ensure Gate-7 uses Relay-9. Confirm Gate-7 requires Relay-9. \
                        Since Monday Gate-7 uses Relay-9. Gate-7 seldom uses Relay-9. \
                        Gate-7 used to use Relay-9. Gate-7 uses Relay-9 except during maintenance. \
                        Relay-9 is required by Gate-7 to start Pump-3. Gate-7 uses Relay-9 instead of Relay-8. \
                        Gate-7 uses Relay-9 until Friday. Gate-7 is used by Relay-9 for backup. \
                        The North gate requires Relay-9. Pump-3 uses anvil.";
        check(document, document, &[("North gate", "requires", "Relay-9")]);
    }

    #[test]
    fn withholds_scenarios_across_paragraphs_and_nested_headings() {
        let example = "Gate-7 uses Relay-9.";
        let live = "Pump-3 requires Valve-4.";
        let document = format!(
            "# Equipment\n\n## Capacity notes\n\nSuppose the plant changes.\n\n{example}\n\n\
             ### Site wiring\n\nGate-8 uses Relay-10.\n\n## Installed equipment\n\n{live}\n"
        );
        check(&document, example, &[]);
        check(&document, &document, &[("Pump-3", "requires", "Valve-4")]);
        let document = format!("{live} Imagine the plant changes! {example}");
        check(&document, &document, &[("Pump-3", "requires", "Valve-4")]);
    }

    #[test]
    fn withholds_explicit_noncurrent_headings_through_nested_sections() {
        let old = "Garnet Panel requires SR-208.";
        for title in [
            "Retracted dependency records",
            "Historical operating notes",
            "Wiring records (superseded)",
            "Obsolete configuration",
            "Archived design records",
            "Withdrawn assertions",
            "Retraction notice: March entries",
            "STATUS: RETRACTED",
        ] {
            let document = format!(
                "# Service notes\n\n## {title}\n\n### Wiring\n\n{old}\n\n\
                 ## Current routes for Historical Records Service\n\n\
                 Historical Records Service uses Garnet Panel.\n"
            );
            check(&document, old, &[]);
            check(
                &document,
                &document,
                &[("Historical Records Service", "uses", "Garnet Panel")],
            );
        }
        let document = "Archived Parts Unit\n===================\n\n\
                        Archived Parts Unit uses Cedar Gauge.";
        check(
            document,
            document,
            &[("Archived Parts Unit", "uses", "Cedar Gauge")],
        );
    }

    #[test]
    fn withholds_explicit_noncurrent_lead_ins_before_the_active_passage() {
        let old = "Larch Controller uses MT-611.";
        let live = "Larch Controller uses SJ-302.";
        for lead_in in [
            "**Retraction notice:** The reviewed copy is below.",
            "Status: retracted",
            "The following records were retracted.",
            "This section is for historical reference only.",
            "The relationships below are superseded.",
        ] {
            let document = format!(
                "## Service notes\n\n{lead_in}\n\n{old}\n\n\
                 ## Active replacement\n\n{live}\n"
            );
            check(&document, old, &[]);
            check(
                &document,
                &document,
                &[("Larch Controller", "uses", "SJ-302")],
            );
        }
        let document = format!(
            "## Service notes\n\n{live} The following entries were archived.\n\n\
             Historical Records Service uses Archive Relay."
        );
        check(
            &document,
            &document,
            &[("Larch Controller", "uses", "SJ-302")],
        );
        let document = format!("Historical Records Service uses Archive Relay. {live}");
        check(
            &document,
            &document,
            &[
                ("Historical Records Service", "uses", "Archive Relay"),
                ("Larch Controller", "uses", "SJ-302"),
            ],
        );
    }

    #[test]
    fn verifies_uniqueness_and_distinct_reciprocal_spans() {
        let document = "Cedar Panel uses Elm Relay. GR-111 requires UV-909. \
                        UV-909 requires GR-111. Cedar Panel uses Elm Relay.";
        check(document, "Cedar Panel uses Elm Relay.", &[]);
        let rows = check(
            document,
            document,
            &[
                ("GR-111", "requires", "UV-909"),
                ("UV-909", "requires", "GR-111"),
            ],
        );
        assert!(rows[0].quote_end < rows[1].quote_start);
    }

    #[test]
    fn bounds_document_quote_and_proposals() {
        let document = format!("FZ-220 uses RG-494.{}", " ".repeat(12_000));
        check(&document, &document, &[]);
        let document = format!("XY-777 uses {}Valve.", "Uppercase ".repeat(32));
        check(&document, &document, &[]);
        let document: String = (0..17).map(|i| format!("AX-{i} uses RX-{i}. ")).collect();
        check(&document, &document, &[]);
    }
}
