//! The PDF decoder runs in a separate process with a bounded byte protocol.

use crate::document_worker::{self, MAX_INPUT};
use anyhow::{Context, Result, bail, ensure};
use lopdf::{DecompressError, Document, Error as LopdfError, LoadOptions};
use serde_json::{Value, json};
const MAX_DECOMPRESSED: usize = 16 * 1024 * 1024;
const MAX_PAGES: usize = 200;
const MAX_CHARACTERS: usize = 2_000_000;
const MAX_OUTPUT: usize = 6 * MAX_CHARACTERS + 4 * MAX_PAGES + 256;
const TOO_LARGE: &str = "The PDF exceeds the 16 MiB input limit.";
const TOO_MANY_PAGES: &str = "The PDF exceeds the 200-page limit.";
const TOO_MUCH_TEXT: &str = "The PDF exceeds the character limit.";
const DECOMPRESSION_LIMIT: &str = "The PDF exceeds the decompression limit.";
const ENCRYPTED: &str = "Encrypted PDFs are unsupported. Provide an unencrypted text-layer PDF.";
const NO_TEXT: &str = "The PDF has no readable text layer. Provide searchable text.";
const INVALID: &str = "The PDF could not be parsed safely.";

pub fn extract(raw: &[u8]) -> Result<Vec<String>> {
    ensure!(raw.len() <= MAX_INPUT, "{TOO_LARGE}");
    decode_response(&document_worker::run(raw, "__pdf-worker", MAX_OUTPUT)?)
}

fn decode_response(output: &[u8]) -> Result<Vec<String>> {
    let mut value: Value = serde_json::from_slice(output).context(INVALID)?;
    ensure!(value["schema_version"].as_u64() == Some(1), "{INVALID}");
    if let Some(message) = value["error"].as_str() {
        ensure!(
            matches!(
                message,
                TOO_LARGE
                    | TOO_MANY_PAGES
                    | TOO_MUCH_TEXT
                    | DECOMPRESSION_LIMIT
                    | ENCRYPTED
                    | NO_TEXT
                    | INVALID
            ),
            "{INVALID}"
        );
        bail!("{message}");
    }
    let pages: Vec<String> = serde_json::from_value(value["pages"].take()).context(INVALID)?;
    validate_pages(&pages).map_err(anyhow::Error::msg)?;
    Ok(pages)
}

fn validate_pages(pages: &[String]) -> std::result::Result<(), &'static str> {
    if pages.len() > MAX_PAGES {
        return Err(TOO_MANY_PAGES);
    }
    let mut characters = pages.len().saturating_sub(1) * 2;
    let mut has_text = false;
    for page in pages {
        if page.contains('\0') {
            return Err(INVALID);
        }
        characters += page.chars().count();
        if characters > MAX_CHARACTERS {
            return Err(TOO_MUCH_TEXT);
        }
        has_text |= !page.trim().is_empty();
    }
    if !has_text {
        return Err(NO_TEXT);
    }
    Ok(())
}

fn parse_error(error: LopdfError) -> &'static str {
    match error {
        LopdfError::Decompress(DecompressError::MemoryLimitExceeded { .. }) => DECOMPRESSION_LIMIT,
        LopdfError::InvalidPassword | LopdfError::UnsupportedSecurityHandler(_) => ENCRYPTED,
        LopdfError::Unimplemented(message) if message.starts_with("PDF is encrypted") => ENCRYPTED,
        _ => INVALID,
    }
}

fn page_count(document: &Document) -> std::result::Result<usize, &'static str> {
    let declared = document
        .catalog()
        .and_then(|catalog| catalog.get_deref(b"Pages", document))
        .and_then(|pages| pages.as_dict())
        .and_then(|pages| pages.get_deref(b"Count", document))
        .and_then(|count| count.as_i64())
        .map_err(parse_error)?;
    let declared = usize::try_from(declared).map_err(|_| INVALID)?;
    if declared > MAX_PAGES {
        return Err(TOO_MANY_PAGES);
    }
    let mut ids = Vec::with_capacity(declared);
    for id in document.page_iter().take(MAX_PAGES + 1) {
        if ids.len() == MAX_PAGES {
            return Err(TOO_MANY_PAGES);
        }
        if ids.contains(&id)
            || document
                .get_dictionary(id)
                .and_then(|page| page.get_type())
                .map_err(parse_error)?
                != b"Page"
        {
            return Err(INVALID);
        }
        ids.push(id);
    }
    if ids.len() != declared {
        return Err(INVALID);
    }
    Ok(declared)
}

fn parse(raw: &[u8]) -> std::result::Result<Vec<String>, &'static str> {
    if raw.len() > MAX_INPUT {
        return Err(TOO_LARGE);
    }
    let document = Document::load_mem_with_options(
        raw,
        LoadOptions {
            strict: true,
            max_decompressed_size: Some(MAX_DECOMPRESSED),
            ..LoadOptions::default()
        },
    )
    .map_err(parse_error)?;
    if document.was_encrypted() || document.trailer.has(b"Encrypt") {
        return Err(ENCRYPTED);
    }
    let count = page_count(&document)?;
    let mut pages = Vec::with_capacity(count);
    let mut characters = count.saturating_sub(1) * 2;
    for number in 1..=count as u32 {
        let text = document
            .extract_text_with_limit(&[number], MAX_DECOMPRESSED)
            .map_err(parse_error)?;
        characters += text.chars().count();
        if characters > MAX_CHARACTERS {
            return Err(TOO_MUCH_TEXT);
        }
        pages.push(text);
    }
    validate_pages(&pages)?;
    Ok(pages)
}

pub fn worker() -> Result<()> {
    document_worker::respond(|raw| {
        parse(raw).map(|pages| json!({"schema_version": 1, "pages": pages}))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use lopdf::{Dictionary, Object, Stream, dictionary};

    fn fixture(contents: &[&[u8]], declared: i64, compress: bool) -> Vec<u8> {
        let mut document = Document::with_version("1.4");
        let tree = document.new_object_id();
        let font = document.add_object(dictionary! {
            "Type" => "Font",
            "Subtype" => "Type1",
            "BaseFont" => "Helvetica",
        });
        let mut pages = Vec::new();
        for bytes in contents {
            let mut stream = Stream::new(Dictionary::new(), bytes.to_vec());
            if compress {
                stream.compress().unwrap();
            }
            let stream = document.add_object(stream);
            let page = document.add_object(dictionary! {
                "Type" => "Page",
                "Parent" => tree,
                "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
                "Resources" => dictionary! { "Font" => dictionary! { "F1" => font } },
                "Contents" => stream,
            });
            pages.push(Object::Reference(page));
        }
        document.objects.insert(
            tree,
            dictionary! { "Type" => "Pages", "Kids" => pages, "Count" => declared }.into(),
        );
        let catalog = document.add_object(dictionary! { "Type" => "Catalog", "Pages" => tree });
        document.trailer.set("Root", catalog);
        let mut bytes = Vec::new();
        document.save_to(&mut bytes).unwrap();
        bytes
    }

    #[test]
    fn blank_page_preserves_the_following_page_number() {
        let bytes = fixture(&[b"", b"BT /F1 11 Tf (cabinet nine) Tj ET"], 2, false);
        assert_eq!(parse(&bytes).unwrap(), ["", "cabinet nine\n"]);
    }

    #[test]
    fn invalid_page_tree_and_unreadable_pdf_are_rejected() {
        assert_eq!(parse(b"%PDF-1.4\ninvalid"), Err(INVALID));
        assert_eq!(parse(&fixture(&[b""], 2, false)), Err(INVALID));
        assert_eq!(parse(&fixture(&[b""], 1, false)), Err(NO_TEXT));
    }

    #[test]
    fn compressed_page_cannot_exceed_the_decompression_budget() {
        let inflated = vec![b' '; MAX_DECOMPRESSED + 1];
        let bytes = fixture(&[&inflated], 1, true);
        assert!(bytes.len() < MAX_INPUT);
        assert_eq!(parse(&bytes), Err(DECOMPRESSION_LIMIT));
    }
}
