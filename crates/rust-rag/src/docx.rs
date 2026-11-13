//! Extract a bounded main-body view of a Word document in the document worker.

use crate::{document_worker, files};
use anyhow::{Context, Result, bail, ensure};
use roxmltree::{Document, Node, ParsingOptions};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    io::{Cursor, Read},
};
use zip::{CompressionMethod, ZipArchive};

const MAX_ENTRIES: usize = 2_048;
const MAX_EXPANDED: u128 = 64 * 1024 * 1024;
const MAX_XML: usize = 8 * 1024 * 1024;
const MAX_METADATA: usize = 256 * 1024;
const MAX_XML_NODES: u32 = 200_000;
const MAX_DEPTH: usize = 64;
const MAX_PARAGRAPHS: usize = 20_000;
const MAX_CHARACTERS: usize = 2_000_000;
const MAX_ANCHOR: usize = 80;
const MAX_OUTPUT: usize = 6 * MAX_CHARACTERS + (MAX_ANCHOR + 32) * MAX_PARAGRAPHS + 256;
const WORD: &str = "http://schemas.openxmlformats.org/wordprocessingml/2006/main";
const STRICT_WORD: &str = "http://purl.oclc.org/ooxml/wordprocessingml/main";
const RELS: &str = "http://schemas.openxmlformats.org/package/2006/relationships";
const REL_TYPES: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/";
const STRICT_REL_TYPES: &str = "http://purl.oclc.org/ooxml/officeDocument/relationships/";
const TYPES: &str = "http://schemas.openxmlformats.org/package/2006/content-types";
const MAIN_TYPE: &str =
    "application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml";
const INVALID: &str = "The DOCX package or XML could not be parsed safely.";
const LIMIT: &str = "The DOCX exceeds the reader's archive, XML or text limits.";
const DTD: &str = "DTD declarations are unsupported in DOCX XML.";
const ENCRYPTED: &str =
    "Use an unencrypted DOCX package. Encrypted or compound files are unsupported.";
const MACRO: &str = "Macro-enabled Word packages are unsupported. Save a macro-free DOCX.";
const LAYOUT: &str = "This DOCX uses an unsupported namespace, encoding or package layout.";

type ParseResult<T> = std::result::Result<T, &'static str>;
type Archive<'a> = ZipArchive<Cursor<&'a [u8]>>;

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Paragraph {
    pub anchor: String,
    pub text: String,
}

pub fn extract(raw: &[u8]) -> Result<Vec<Paragraph>> {
    ensure!(raw.len() <= document_worker::MAX_INPUT, "{LIMIT}");
    let response = document_worker::run(raw, "__docx-worker", MAX_OUTPUT)?;
    let mut value: Value = serde_json::from_slice(&response).context(INVALID)?;
    ensure!(value["schema_version"].as_u64() == Some(1), "{INVALID}");
    if let Some(error) = value["error"].as_str() {
        ensure!(
            matches!(error, INVALID | LIMIT | DTD | ENCRYPTED | MACRO | LAYOUT),
            "{INVALID}"
        );
        bail!("{error}");
    }
    let paragraphs: Vec<Paragraph> =
        serde_json::from_value(value["paragraphs"].take()).context(INVALID)?;
    ensure!(paragraphs.len() <= MAX_PARAGRAPHS, "{LIMIT}");
    let mut characters = paragraphs.len().saturating_sub(1) * 2;
    for paragraph in &paragraphs {
        ensure!(
            paragraph.anchor.len() <= MAX_ANCHOR && valid_anchor(&paragraph.anchor),
            "{INVALID}"
        );
        ensure!(!paragraph.text.contains('\0'), "{INVALID}");
        characters += paragraph.text.chars().count();
        ensure!(characters <= MAX_CHARACTERS, "{LIMIT}");
    }
    Ok(paragraphs)
}

fn valid_anchor(anchor: &str) -> bool {
    let parts: Vec<_> = anchor.split(':').collect();
    let positive = |part: &str| part.parse::<usize>().is_ok_and(|n| n > 0);
    match parts.as_slice() {
        ["p", p] => positive(p),
        ["table", t, "row", r, "cell", c, "p", p] => {
            [t, r, c, p].into_iter().all(|part| positive(part))
        }
        _ => false,
    }
}

pub fn worker() -> Result<()> {
    document_worker::respond(|raw| {
        parse(raw).map(|paragraphs| json!({"schema_version": 1, "paragraphs": paragraphs}))
    })
}

fn archive(raw: &[u8]) -> ParseResult<Archive<'_>> {
    if raw.len() > document_worker::MAX_INPUT {
        return Err(LIMIT);
    }
    if raw.starts_with(b"\xd0\xcf\x11\xe0\xa1\xb1\x1a\xe1") {
        return Err(ENCRYPTED);
    }
    let mut archive = ZipArchive::new(Cursor::new(raw)).map_err(|_| INVALID)?;
    if archive.len() > MAX_ENTRIES || archive.decompressed_size().is_none_or(|n| n > MAX_EXPANDED) {
        return Err(LIMIT);
    }
    if archive.offset() != 0 || !raw.starts_with(b"PK\x03\x04") {
        return Err(INVALID);
    }
    archive.validate_header_consistency().map_err(|_| INVALID)?;
    let mut names = HashSet::new();
    for index in 0..archive.len() {
        let entry = archive.by_index_data(index).map_err(|_| INVALID)?;
        if entry.encrypted() {
            return Err(ENCRYPTED);
        }
        let name = entry.name().map_err(|_| INVALID)?;
        let name = name.trim_end_matches('/');
        if entry.is_symlink()
            || name.contains(['\\', '\0'])
            || name.split('/').any(|part| matches!(part, "" | "." | ".."))
            || !names.insert(name.to_ascii_lowercase())
        {
            return Err(INVALID);
        }
        if name.rsplit('/').next().is_some_and(|part| {
            part.eq_ignore_ascii_case("vbaProject.bin") || part.eq_ignore_ascii_case("vbaData.xml")
        }) {
            return Err(MACRO);
        }
    }
    Ok(archive)
}

fn read_part(archive: &mut Archive<'_>, name: &str, limit: usize) -> ParseResult<String> {
    let part = archive.by_name(name).map_err(|_| INVALID)?;
    if !part.is_file()
        || !matches!(
            part.compression(),
            CompressionMethod::Stored | CompressionMethod::Deflated
        )
    {
        return Err(LAYOUT);
    }
    let size = part.size();
    if size > limit as u64 {
        return Err(LIMIT);
    }
    let mut raw = Vec::new();
    part.take(limit as u64 + 1)
        .read_to_end(&mut raw)
        .map_err(|_| INVALID)?;
    if raw.len() > limit {
        return Err(LIMIT);
    }
    if raw.len() as u64 != size {
        return Err(INVALID);
    }
    files::decode(&raw).map_err(|_| LAYOUT)
}

fn has_doctype(mut prolog: &str) -> bool {
    // roxmltree allows an empty DTD with its DTD option disabled. Reject that too.
    loop {
        prolog = prolog.trim_start();
        let rest = if prolog.starts_with("<!--") {
            prolog.split_once("-->")
        } else if prolog.starts_with("<?") {
            prolog.split_once("?>")
        } else {
            return prolog.starts_with("<!DOCTYPE");
        };
        let Some((_, remainder)) = rest else {
            return false;
        };
        prolog = remainder;
    }
}

fn xml(text: &str) -> ParseResult<Document<'_>> {
    if has_doctype(text) {
        return Err(DTD);
    }
    let document = Document::parse_with_options(
        text,
        ParsingOptions {
            nodes_limit: MAX_XML_NODES,
            ..Default::default()
        },
    )
    .map_err(|error| match error {
        roxmltree::Error::DtdDetected => DTD,
        roxmltree::Error::NodesLimitReached => LIMIT,
        _ => INVALID,
    })?;
    if document
        .descendants()
        .any(|node| node.ancestors().take(MAX_DEPTH + 1).count() > MAX_DEPTH)
    {
        return Err(LIMIT);
    }
    Ok(document)
}

fn content_type(text: &str) -> ParseResult<()> {
    let document = xml(text)?;
    let root = document.root_element();
    if !root.has_tag_name((TYPES, "Types")) {
        return Err(LAYOUT);
    }
    let mut main = false;
    for node in root.children().filter(Node::is_element) {
        let kind = node.attribute("ContentType").ok_or(INVALID)?;
        let lower = kind.to_ascii_lowercase();
        if lower.contains("macroenabled") || lower.contains("vbaproject") {
            return Err(MACRO);
        }
        if node.has_tag_name((TYPES, "Override"))
            && node.attribute("PartName") == Some("/word/document.xml")
        {
            if main || kind != MAIN_TYPE {
                return Err(LAYOUT);
            }
            main = true;
        }
    }
    main.then_some(()).ok_or(LAYOUT)
}

fn canonical_relationship(
    text: &str,
    kind: &str,
    relative: &str,
    absolute: &str,
) -> ParseResult<bool> {
    let document = xml(text)?;
    let root = document.root_element();
    if !root.has_tag_name((RELS, "Relationships")) {
        return Err(LAYOUT);
    }
    let mut found = false;
    for node in root
        .children()
        .filter(|node| node.has_tag_name((RELS, "Relationship")))
    {
        let Some(relationship) = node.attribute("Type").and_then(|value| {
            value
                .strip_prefix(REL_TYPES)
                .or_else(|| value.strip_prefix(STRICT_REL_TYPES))
        }) else {
            continue;
        };
        if relationship == kind {
            if found
                || !matches!(node.attribute("TargetMode"), None | Some("Internal"))
                || !node
                    .attribute("Target")
                    .is_some_and(|target| target == relative || target == absolute)
            {
                return Err(LAYOUT);
            }
            found = true;
        }
    }
    Ok(found)
}

fn child<'a, 'input>(node: Node<'a, 'input>, ns: &str, name: &str) -> Option<Node<'a, 'input>> {
    node.children().find(|item| item.has_tag_name((ns, name)))
}

fn enabled(node: Node<'_, '_>, ns: &str, attribute: &str) -> bool {
    !matches!(node.attribute((ns, attribute)), Some("0" | "false" | "off"))
}

fn hides(properties: Node<'_, '_>, ns: &str) -> bool {
    properties.children().any(|node| {
        node.tag_name().namespace() == Some(ns)
            && matches!(
                node.tag_name().name(),
                "vanish" | "webHidden" | "specVanish"
            )
            && enabled(node, ns, "val")
    })
}

#[derive(Default)]
struct Styles<'a> {
    by_id: HashMap<&'a str, Style<'a>>,
    paragraph: Option<&'a str>,
    character: Option<&'a str>,
    table: Option<&'a str>,
    document_hidden: bool,
}

struct Style<'a> {
    base: Option<&'a str>,
    hides: bool,
}

impl<'a> Styles<'a> {
    fn read(root: Node<'a, '_>, ns: &str) -> ParseResult<Self> {
        if !root.has_tag_name((ns, "styles")) {
            return Err(LAYOUT);
        }
        let mut styles = Self::default();
        styles.document_hidden = child(root, ns, "docDefaults")
            .and_then(|node| child(node, ns, "rPrDefault"))
            .and_then(|node| child(node, ns, "rPr"))
            .is_some_and(|node| hides(node, ns));
        for node in root
            .children()
            .filter(|item| item.has_tag_name((ns, "style")))
        {
            let id = node.attribute((ns, "styleId")).ok_or(INVALID)?;
            let base = child(node, ns, "basedOn").and_then(|item| item.attribute((ns, "val")));
            let hides_text = child(node, ns, "rPr").is_some_and(|item| hides(item, ns))
                || node
                    .children()
                    .filter(|item| item.has_tag_name((ns, "tblStylePr")))
                    .filter_map(|item| child(item, ns, "rPr"))
                    .any(|item| hides(item, ns));
            if styles
                .by_id
                .insert(
                    id,
                    Style {
                        base,
                        hides: hides_text,
                    },
                )
                .is_some()
            {
                return Err(INVALID);
            }
            if node.attribute((ns, "default")).is_some() && enabled(node, ns, "default") {
                let target = match node.attribute((ns, "type")) {
                    Some("paragraph") => &mut styles.paragraph,
                    Some("character") => &mut styles.character,
                    Some("table") => &mut styles.table,
                    _ => continue,
                };
                if target.replace(id).is_some() {
                    return Err(INVALID);
                }
            }
        }
        Ok(styles)
    }

    fn hides<'b>(&'b self, mut id: Option<&'b str>) -> ParseResult<bool> {
        // Conservative extraction. A hiding declaration anywhere in the applicable
        // style chain suppresses the run, including conditional table formatting.
        for _ in 0..MAX_DEPTH {
            let Some(style) = id.and_then(|name| self.by_id.get::<str>(name)) else {
                return Ok(false);
            };
            if style.hides {
                return Ok(true);
            }
            id = style.base;
        }
        Err(LIMIT)
    }
}

fn visible_children<'a, 'input>(node: Node<'a, 'input>, ns: &str) -> Vec<Node<'a, 'input>> {
    fn walk<'a, 'input>(node: Node<'a, 'input>, ns: &str, result: &mut Vec<Node<'a, 'input>>) {
        for item in node
            .children()
            .filter(|item| item.tag_name().namespace() == Some(ns))
        {
            match item.tag_name().name() {
                "del" | "moveFrom" => (),
                "ins" | "moveTo" | "hyperlink" | "smartTag" | "customXml" | "fldSimple" => {
                    walk(item, ns, result)
                }
                "sdt" => {
                    if let Some(content) = child(item, ns, "sdtContent") {
                        walk(content, ns, result);
                    }
                }
                _ => result.push(item),
            }
        }
    }
    let mut children = Vec::new();
    walk(node, ns, &mut children);
    children
}

#[derive(Default)]
struct Body {
    paragraphs: Vec<Paragraph>,
    characters: usize,
}

impl Body {
    fn add_text(&mut self, output: &mut String, value: &str) -> ParseResult<()> {
        self.characters += value.chars().count();
        if self.characters > MAX_CHARACTERS {
            return Err(LIMIT);
        }
        output.push_str(value);
        Ok(())
    }

    fn paragraph(
        &mut self,
        node: Node<'_, '_>,
        ns: &str,
        anchor: String,
        styles: &Styles<'_>,
    ) -> ParseResult<()> {
        if self.paragraphs.len() == MAX_PARAGRAPHS {
            return Err(LIMIT);
        }
        let paragraph_style = child(node, ns, "pPr")
            .and_then(|properties| child(properties, ns, "pStyle"))
            .and_then(|item| item.attribute((ns, "val")))
            .or(styles.paragraph);
        let mut text = String::new();
        self.characters += usize::from(!self.paragraphs.is_empty()) * 2;
        if self.characters > MAX_CHARACTERS {
            return Err(LIMIT);
        }
        if !styles.document_hidden && !styles.hides(paragraph_style)? {
            for run in visible_children(node, ns)
                .into_iter()
                .filter(|item| item.has_tag_name((ns, "r")))
            {
                let properties = child(run, ns, "rPr");
                let style = properties
                    .and_then(|item| child(item, ns, "rStyle"))
                    .and_then(|item| item.attribute((ns, "val")))
                    .or(styles.character);
                if run
                    .children()
                    .any(|item| item.has_tag_name((ns, "rPr")) && hides(item, ns))
                    || styles.hides(style)?
                {
                    continue;
                }
                for item in run
                    .children()
                    .filter(|item| item.tag_name().namespace() == Some(ns))
                {
                    match item.tag_name().name() {
                        "t" => {
                            for value in item
                                .children()
                                .filter(Node::is_text)
                                .filter_map(|node| node.text())
                            {
                                self.add_text(&mut text, value)?;
                            }
                        }
                        "tab" => self.add_text(&mut text, "\t")?,
                        "br" | "cr" => self.add_text(&mut text, "\n")?,
                        "noBreakHyphen" => self.add_text(&mut text, "\u{2011}")?,
                        "softHyphen" => self.add_text(&mut text, "\u{00ad}")?,
                        _ => (),
                    }
                }
            }
        }
        self.paragraphs.push(Paragraph { anchor, text });
        Ok(())
    }

    fn table(
        &mut self,
        table: Node<'_, '_>,
        ns: &str,
        number: usize,
        styles: &Styles<'_>,
    ) -> ParseResult<()> {
        let style = child(table, ns, "tblPr")
            .and_then(|item| child(item, ns, "tblStyle"))
            .and_then(|item| item.attribute((ns, "val")))
            .or(styles.table);
        if styles.hides(style)? {
            return Ok(());
        }
        for (row, item) in visible_children(table, ns)
            .into_iter()
            .filter(|item| item.has_tag_name((ns, "tr")))
            .enumerate()
        {
            if child(item, ns, "trPr")
                .and_then(|properties| child(properties, ns, "del"))
                .is_some()
            {
                continue;
            }
            for (cell, item) in visible_children(item, ns)
                .into_iter()
                .filter(|item| item.has_tag_name((ns, "tc")))
                .enumerate()
            {
                if child(item, ns, "tcPr")
                    .and_then(|properties| child(properties, ns, "cellDel"))
                    .is_some()
                {
                    continue;
                }
                for (paragraph, item) in visible_children(item, ns)
                    .into_iter()
                    .filter(|item| item.has_tag_name((ns, "p")))
                    .enumerate()
                {
                    self.paragraph(
                        item,
                        ns,
                        format!(
                            "table:{number}:row:{}:cell:{}:p:{}",
                            row + 1,
                            cell + 1,
                            paragraph + 1
                        ),
                        styles,
                    )?;
                }
            }
        }
        Ok(())
    }
}

fn parse(raw: &[u8]) -> ParseResult<Vec<Paragraph>> {
    let mut archive = archive(raw)?;
    content_type(&read_part(
        &mut archive,
        "[Content_Types].xml",
        MAX_METADATA,
    )?)?;
    let package = read_part(&mut archive, "_rels/.rels", MAX_METADATA)?;
    if !canonical_relationship(
        &package,
        "officeDocument",
        "word/document.xml",
        "/word/document.xml",
    )? {
        return Err(LAYOUT);
    }
    let mut has_styles = false;
    if archive
        .index_for_name("word/_rels/document.xml.rels")
        .is_some()
    {
        let relations = read_part(&mut archive, "word/_rels/document.xml.rels", MAX_METADATA)?;
        has_styles =
            canonical_relationship(&relations, "styles", "styles.xml", "/word/styles.xml")?;
    }
    let styles = if has_styles {
        Some(read_part(&mut archive, "word/styles.xml", MAX_XML)?)
    } else {
        None
    };
    let styles_xml = styles.as_deref().map(xml).transpose()?;
    let text = read_part(&mut archive, "word/document.xml", MAX_XML)?;
    let document = xml(&text)?;
    let root = document.root_element();
    let ns = root
        .tag_name()
        .namespace()
        .filter(|ns| matches!(*ns, WORD | STRICT_WORD))
        .ok_or(LAYOUT)?;
    if !root.has_tag_name((ns, "document")) {
        return Err(LAYOUT);
    }
    let mut bodies = root
        .children()
        .filter(|item| item.has_tag_name((ns, "body")));
    let node = bodies.next().ok_or(INVALID)?;
    if bodies.next().is_some() {
        return Err(INVALID);
    }
    let styles = match styles_xml.as_ref() {
        Some(document) => Styles::read(document.root_element(), ns)?,
        None => Styles::default(),
    };
    let mut body = Body::default();
    let mut paragraph = 0;
    let mut table = 0;
    for item in visible_children(node, ns) {
        match item.tag_name().name() {
            "p" => {
                paragraph += 1;
                body.paragraph(item, ns, format!("p:{paragraph}"), &styles)?;
            }
            "tbl" => {
                table += 1;
                body.table(item, ns, table, &styles)?;
            }
            _ => (),
        }
    }
    Ok(body.paragraphs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use zip::{ZipWriter, write::SimpleFileOptions};

    fn zip(parts: &[(&str, &[u8])], compression: CompressionMethod) -> Vec<u8> {
        let mut archive = ZipWriter::new(Cursor::new(Vec::new()));
        let options = SimpleFileOptions::default().compression_method(compression);
        for (name, bytes) in parts {
            archive.start_file(*name, options).unwrap();
            archive.write_all(bytes).unwrap();
        }
        archive.finish().unwrap().into_inner()
    }

    fn package(
        document: &str,
        main_type: &str,
        styles: Option<&str>,
        extra: &[(&str, &[u8])],
    ) -> Vec<u8> {
        let relation_type = if document.contains(STRICT_WORD) {
            STRICT_REL_TYPES
        } else {
            REL_TYPES
        };
        let types = format!(
            r#"<Types xmlns="{TYPES}"><Override PartName="/word/document.xml" ContentType="{main_type}"/></Types>"#
        );
        let root_rels = format!(
            r#"<Relationships xmlns="{RELS}"><Relationship Type="{relation_type}officeDocument" Target="word/document.xml" Id="main"/></Relationships>"#
        );
        let doc_rels = format!(
            r#"<Relationships xmlns="{RELS}"><Relationship Type="{relation_type}styles" Target="styles.xml" Id="styles"/><Relationship Type="{relation_type}hyperlink" Target="https://example.invalid/synthetic" TargetMode="External" Id="link"/></Relationships>"#
        );
        let mut parts = vec![
            ("[Content_Types].xml", types.as_bytes()),
            ("_rels/.rels", root_rels.as_bytes()),
            ("word/document.xml", document.as_bytes()),
        ];
        if let Some(styles) = styles {
            parts.extend([
                ("word/styles.xml", styles.as_bytes()),
                ("word/_rels/document.xml.rels", doc_rels.as_bytes()),
            ]);
        }
        parts.extend(extra);
        zip(&parts, CompressionMethod::Deflated)
    }

    fn document(ns: &str, body: &str) -> String {
        format!(
            r#"<x:document xmlns:x="{ns}" xmlns:other="urn:synthetic"><x:body>{body}</x:body></x:document>"#
        )
    }

    #[test]
    fn visible_main_body_keeps_unicode_runs_and_structural_order() {
        let styles = format!(
            r#"<w:styles xmlns:w="{WORD}">
            <w:style w:styleId="Invisible" w:type="character"><w:rPr><w:vanish/></w:rPr></w:style>
            <w:style w:styleId="Inherited" w:type="character"><w:basedOn w:val="Invisible"/></w:style>
            <w:style w:styleId="HiddenParagraph" w:type="paragraph"><w:rPr><w:webHidden/></w:rPr></w:style>
        </w:styles>"#
        );
        let document = document(
            WORD,
            r#"
            <x:p><x:pPr><x:pStyle x:val="Heading1"/></x:pPr>
                <x:r><x:t>Cafe</x:t></x:r><x:r><x:t>&#x301; 🧭 &amp; </x:t></x:r>
                <x:ins><x:r><x:t>inserted</x:t><x:tab/><x:t>visible</x:t></x:r></x:ins>
                <x:del><x:r><x:t>deleted canary</x:t></x:r></x:del>
                <x:moveFrom><x:r><x:t>moved-out canary</x:t></x:r></x:moveFrom>
                <x:moveTo><x:r><x:br/><x:t>moved-in</x:t></x:r></x:moveTo>
                <x:r><x:rPr><x:vanish/></x:rPr><x:t>hidden canary</x:t></x:r>
                <x:r><x:rPr><x:rStyle x:val="Inherited"/></x:rPr><x:t>inherited hidden canary</x:t></x:r>
                <x:r><x:rPr><x:vanish x:val="0"/></x:rPr><x:t> shown</x:t></x:r>
                <x:r><x:drawing><x:txbxContent><x:p><x:r><x:t>embedded canary</x:t></x:r></x:p></x:txbxContent></x:drawing></x:r>
                <other:r><other:t>foreign canary</other:t></other:r>
            </x:p>
            <x:p><x:pPr><x:pStyle x:val="HiddenParagraph"/></x:pPr><x:r><x:t>paragraph hidden canary</x:t></x:r></x:p>
            <x:tbl>
                <x:tr><x:trPr><x:del/></x:trPr><x:tc><x:p><x:r><x:t>deleted row canary</x:t></x:r></x:p></x:tc></x:tr>
                <x:tr><x:tc><x:p/><x:p><x:r><x:t>row two cell one</x:t></x:r></x:p></x:tc>
                    <x:tc><x:p><x:hyperlink><x:r><x:t>row two cell two</x:t></x:r></x:hyperlink></x:p></x:tc></x:tr>
            </x:tbl>
            <x:p><x:r><x:instrText>field instruction canary</x:instrText><x:t>final paragraph</x:t></x:r></x:p>
        "#,
        );
        let bytes = package(
            &document,
            MAIN_TYPE,
            Some(&styles),
            &[("word/header1.xml", b"header canary")],
        );
        let paragraphs = parse(&bytes).unwrap();
        let view: Vec<_> = paragraphs
            .iter()
            .map(|item| (item.anchor.as_str(), item.text.as_str()))
            .collect();
        assert_eq!(
            view,
            [
                ("p:1", "Cafe\u{301} 🧭 & inserted\tvisible\nmoved-in shown"),
                ("p:2", ""),
                ("table:1:row:2:cell:1:p:1", ""),
                ("table:1:row:2:cell:1:p:2", "row two cell one"),
                ("table:1:row:2:cell:2:p:1", "row two cell two"),
                ("p:3", "final paragraph"),
            ]
        );
        assert!(view.iter().all(|(anchor, _)| valid_anchor(anchor)));
    }

    #[test]
    fn strict_namespace_and_blank_documents_are_readable() {
        let strict = document(STRICT_WORD, "<x:p><x:r><x:t>strict body</x:t></x:r></x:p>");
        assert_eq!(
            parse(&package(&strict, MAIN_TYPE, None, &[])).unwrap()[0].text,
            "strict body"
        );
        let blank = document(WORD, "<x:p/>");
        assert_eq!(
            parse(&package(&blank, MAIN_TYPE, None, &[])).unwrap()[0].text,
            ""
        );
        let unknown = document("urn:unrecognized", "<x:p/>");
        assert_eq!(parse(&package(&unknown, MAIN_TYPE, None, &[])), Err(LAYOUT));
    }

    #[test]
    fn malformed_macros_encryption_and_dtd_are_rejected() {
        let valid = document(WORD, "<x:p/>");
        let mut bytes = package(&valid, MAIN_TYPE, None, &[]);
        let mut encrypted = bytes.clone();
        for (signature, flags) in [(b"PK\x03\x04", 6), (b"PK\x01\x02", 8)] {
            let position = encrypted
                .windows(4)
                .position(|marker| marker == signature)
                .unwrap();
            encrypted[position + flags] |= 1;
        }
        assert_eq!(parse(&encrypted), Err(ENCRYPTED));
        bytes.truncate(bytes.len() - 12);
        assert_eq!(parse(&bytes), Err(INVALID));
        assert_eq!(parse(b"\xd0\xcf\x11\xe0\xa1\xb1\x1a\xe1"), Err(ENCRYPTED));
        let macro_type = "application/vnd.ms-word.document.macroEnabled.main+xml";
        assert_eq!(parse(&package(&valid, macro_type, None, &[])), Err(MACRO));
        assert_eq!(
            parse(&package(
                &valid,
                MAIN_TYPE,
                None,
                &[("word/vbaProject.bin", b"synthetic")]
            )),
            Err(MACRO)
        );
        let dtd = format!(r#"<?xml version="1.0"?><!-- sample --><!DOCTYPE x:document []>{valid}"#);
        assert_eq!(parse(&package(&dtd, MAIN_TYPE, None, &[])), Err(DTD));
        let malformed = document(WORD, "<x:p>");
        assert_eq!(
            parse(&package(&malformed, MAIN_TYPE, None, &[])),
            Err(INVALID)
        );
    }

    #[test]
    fn bounds_compressed_xml_nesting_and_entries() {
        let long = document(WORD, &"x".repeat(MAX_XML));
        let compressed = package(&long, MAIN_TYPE, None, &[]);
        assert!(compressed.len() < document_worker::MAX_INPUT);
        assert_eq!(parse(&compressed), Err(LIMIT));
        let deep = document(
            WORD,
            &format!(
                "{}{}",
                "<x:ins>".repeat(MAX_DEPTH),
                "</x:ins>".repeat(MAX_DEPTH)
            ),
        );
        assert_eq!(parse(&package(&deep, MAIN_TYPE, None, &[])), Err(LIMIT));
        let names: Vec<_> = (0..MAX_ENTRIES + 1)
            .map(|index| format!("part-{index}"))
            .collect();
        let parts: Vec<_> = names
            .iter()
            .map(|name| (name.as_str(), b"".as_slice()))
            .collect();
        assert_eq!(parse(&zip(&parts, CompressionMethod::Stored)), Err(LIMIT));
    }
}
