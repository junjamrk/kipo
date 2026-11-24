use std::io::{Cursor, Write};
use std::path::Path;

use kp_ingest::doc_router::parse;
use zip::{write::SimpleFileOptions, CompressionMethod, ZipWriter};

type Result<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

#[test]
fn markdown_uses_heading_breadcrumbs_and_exact_utf8_offsets() -> Result {
    let source = "# Guide\nintro\n\n## 支付\ncharge_card\n\n```rs\n# not a heading\n```\n";
    let doc = parse(Path::new("guide.mdx"), source.as_bytes())?;
    assert_eq!(doc.chunks.len(), 2);
    assert_eq!(doc.chunks[1].heading, "Guide > 支付");
    assert_eq!(doc.chunks[1].anchor.start_line, Some(4));
    assert_eq!(doc.chunks[1].anchor.end_line, Some(9));
    assert_eq!(doc.chunks[1].anchor.start_byte, Some(15));
    assert_eq!(doc.chunks[1].anchor.end_byte, Some(source.len() as u64));
    let setext = parse(Path::new("setext.md"), b"Title\n=====\nbody\n")?;
    assert_eq!(setext.chunks[0].heading, "Title");
    Ok(())
}

#[test]
fn subtitle_and_media_extensions_fail_with_unsupported_document_format() {
    for name in [
        "talk.vtt",
        "talk.srt",
        "lyrics.lrc",
        "demo.mp4",
        "voice.m4a",
        "podcast.mp3",
    ] {
        assert!(!kp_ingest::doc_router::handles(Path::new(name)), "{name}");
        assert!(
            matches!(
                parse(
                    Path::new(name),
                    b"WEBVTT\n\n00:01.000 --> 00:02.000\ntext\n"
                ),
                Err(kp_ingest::IngestError::Invalid(
                    "unsupported document format"
                ))
            ),
            "{name}"
        );
    }
}

fn archive(name: &str, bytes: &[u8]) -> Result<Vec<u8>> {
    archive_parts(&[(name, bytes)])
}

fn archive_parts(parts: &[(&str, &[u8])]) -> Result<Vec<u8>> {
    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    for (name, bytes) in parts {
        zip.start_file(
            *name,
            SimpleFileOptions::default().compression_method(CompressionMethod::Deflated),
        )?;
        zip.write_all(bytes)?;
    }
    Ok(zip.finish()?.into_inner())
}

#[test]
fn office_rejects_zip_bombs_dtd_and_unsafe_paths() -> Result {
    let bomb = archive(
        "word/document.xml",
        &vec![b' '; kp_ingest::MAX_INPUT_BYTES + 1],
    )?;
    assert!(bomb.len() < 100_000);
    assert!(parse(Path::new("bomb.docx"), &bomb).is_err());
    for xml in [
        b"<!DOCTYPE x><root><p><t>text</t></p></root>".as_slice(),
        b"<root><p><t>unterminated",
        b"<root><binData>binary</binData></root>",
    ] {
        assert!(parse(Path::new("bad.docx"), &archive("word/document.xml", xml)?).is_err());
    }
    assert!(parse(Path::new("bad.docx"), &archive("../outside", b"bad")?).is_err());
    Ok(())
}

#[test]
fn decoding_prefers_bom_and_declared_encodings_without_lossy_replacement() -> Result {
    let source = "# coding: big5\n繁體中文";
    let (bytes, _, errors) = encoding_rs::BIG5.encode(source);
    assert!(!errors);
    assert_eq!(
        parse(Path::new("notes.txt"), &bytes)?.chunks[0].body,
        source
    );
    let bytes: Vec<_> = [0xff, 0xfe]
        .into_iter()
        .chain("中文".encode_utf16().flat_map(u16::to_le_bytes))
        .collect();
    assert_eq!(
        parse(Path::new("notes.txt"), &bytes)?.chunks[0].body,
        "中文"
    );
    assert!(parse(Path::new("bad.txt"), b"# coding: utf-8\n\xff").is_err());
    Ok(())
}

#[test]
fn markdown_bom_preserves_original_byte_coordinates() -> Result {
    let source = "\u{feff}# Payments\n正文\n";
    let doc = parse(Path::new("bom.md"), source.as_bytes())?;
    assert_eq!(doc.chunks[0].heading, "Payments");
    assert_eq!(doc.chunks[0].anchor.start_byte, Some(3));
    assert_eq!(doc.chunks[0].anchor.end_byte, Some(source.len() as u64));
    assert_eq!(doc.chunks[0].anchor.start_line, Some(1));
    Ok(())
}

fn spreadsheet(shared: &str, cells: &str) -> Result<Vec<u8>> {
    let strings = format!("<sst xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\"><si><t>{shared}</t></si></sst>");
    let sheet = format!("<worksheet xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\"><sheetData>{cells}</sheetData></worksheet>");
    archive_parts(&[
        ("[Content_Types].xml", br#"<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/><Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/><Override PartName="/xl/sharedStrings.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sharedStrings+xml"/></Types>"#),
        ("_rels/.rels", br#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#),
        ("xl/workbook.xml", br#"<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="Data" sheetId="1" r:id="rId1"/></sheets></workbook>"#),
        ("xl/_rels/workbook.xml.rels", br#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/sharedStrings" Target="sharedStrings.xml"/></Relationships>"#),
        ("xl/sharedStrings.xml", strings.as_bytes()),
        ("xl/worksheets/sheet1.xml", sheet.as_bytes()),
    ])
}

#[test]
fn shared_string_amplification_is_rejected_quickly() -> Result {
    let cells = (1..=2000)
        .map(|row| format!(r#"<row r="{row}"><c r="A{row}" t="s"><v>0</v></c></row>"#))
        .collect::<String>();
    let bytes = spreadsheet(&"x".repeat(64 * 1024), &cells)?;
    assert!(bytes.len() < 32 * 1024);
    let started = std::time::Instant::now();
    assert!(matches!(
        parse(Path::new("amplified.xlsx"), &bytes),
        Err(kp_ingest::IngestError::Limit(_))
    ));
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
    Ok(())
}

#[test]
fn shared_string_budget_stops_before_reading_later_cells() -> Result {
    let cells = format!(
        "{}<row><c r=\"B1\" t=\"s\"><v>invalid</v></c></row>",
        "<row><c r=\"A1\" t=\"s\"><v>0</v></c></row>".repeat(300)
    );
    let bytes = spreadsheet(&"x".repeat(64 * 1024), &cells)?;
    assert!(matches!(
        parse(Path::new("early.xlsx"), &bytes),
        Err(kp_ingest::IngestError::Limit(_))
    ));
    Ok(())
}

#[test]
fn repeated_slide_and_sheet_relationship_targets_expand_once() -> Result {
    for (extension, manifest, manifest_xml, rels, rels_xml, part, part_xml) in [
        ("pptx", "ppt/presentation.xml", "<p:presentation xmlns:p=\"p\" xmlns:r=\"r\"><p:sldIdLst><p:sldId r:id=\"rId1\"/><p:sldId r:id=\"rId1\"/><p:sldId r:id=\"rId2\"/></p:sldIdLst></p:presentation>", "ppt/_rels/presentation.xml.rels", "<Relationships><Relationship Id=\"rId1\" Target=\"slides/slide1.xml\"/><Relationship Id=\"rId2\" Target=\"/ppt/slides/slide1.xml\"/></Relationships>", "ppt/slides/slide1.xml", "<slide><p><t>once</t></p></slide>"),
        ("xlsx", "xl/workbook.xml", "<workbook xmlns:r=\"r\"><sheets><sheet name=\"Data\" r:id=\"rId1\"/><sheet name=\"Again\" r:id=\"rId1\"/><sheet name=\"Alias\" r:id=\"rId2\"/></sheets></workbook>", "xl/_rels/workbook.xml.rels", "<Relationships><Relationship Id=\"rId1\" Target=\"worksheets/sheet1.xml\"/><Relationship Id=\"rId2\" Target=\"/xl/worksheets/sheet1.xml\"/></Relationships>", "xl/worksheets/sheet1.xml", "<worksheet><c r=\"A1\" t=\"inlineStr\"><is><t>once</t></is></c></worksheet>"),
    ] {
        let bytes = archive_parts(&[(manifest, manifest_xml.as_bytes()), (rels, rels_xml.as_bytes()), (part, part_xml.as_bytes())])?;
        let doc = parse(Path::new(&format!("repeated.{extension}")), &bytes)?;
        assert_eq!(doc.chunks.len(), 1);
        if extension == "pptx" {
            assert_eq!(doc.chunks[0].body, "once");
        } else {
            assert_eq!(doc.chunks[0].body, "| once |\n| --- |");
        }
    }
    Ok(())
}

#[test]
fn office_chunk_limits_apply_to_paragraphs_cells_and_across_slides() -> Result {
    let paragraphs = format!("<document>{}</document>", "<p><t>x</t></p>".repeat(10_001));
    let cells = format!(
        "<worksheet>{}</worksheet>",
        "<c r=\"A1\"><v>1</v></c>".repeat(10_001)
    );
    let slide = format!("<slide>{}</slide>", "<p><t>x</t></p>".repeat(6000));
    for (name, bytes) in [
        (
            "many.docx",
            archive("word/document.xml", paragraphs.as_bytes())?,
        ),
        (
            "many.xlsx",
            archive("xl/worksheets/sheet1.xml", cells.as_bytes())?,
        ),
        (
            "many.pptx",
            archive_parts(&[
                ("ppt/slides/slide1.xml", slide.as_bytes()),
                ("ppt/slides/slide2.xml", slide.as_bytes()),
            ])?,
        ),
    ] {
        assert!(
            matches!(
                parse(Path::new(name), &bytes),
                Err(kp_ingest::IngestError::Invalid(
                    "too many Office chunks" | "too many Office XML elements"
                ))
            ),
            "{name}"
        );
    }
    Ok(())
}

#[test]
fn shared_string_byte_budget_is_cumulative_across_sheets() -> Result {
    let shared = format!("<sst><si><t>{}</t></si></sst>", "x".repeat(64 * 1024));
    let sheet = format!(
        "<worksheet>{}</worksheet>",
        "<c r=\"A1\" t=\"s\"><v>0</v></c>".repeat(130)
    );
    let bytes = archive_parts(&[
        ("xl/sharedStrings.xml", shared.as_bytes()),
        ("xl/worksheets/sheet1.xml", sheet.as_bytes()),
        ("xl/worksheets/sheet2.xml", sheet.as_bytes()),
    ])?;
    assert!(matches!(
        parse(Path::new("cumulative.xlsx"), &bytes),
        Err(kp_ingest::IngestError::Limit(_))
    ));
    Ok(())
}

#[test]
fn nested_word_paragraphs_preserve_outer_text_and_document_order() -> Result {
    let xml = br#"<w:document xmlns:w="w"><w:body><w:p><w:r><w:t>before </w:t></w:r><w:txbxContent><w:p><w:r><w:t>inside &amp; </w:t></w:r><w:p/><w:p><w:r><w:t><![CDATA[deep]]></w:t></w:r></w:p></w:p></w:txbxContent><w:r><w:tab/><w:t>after</w:t><w:br/><w:t>end</w:t></w:r></w:p><w:p><w:r><w:t>next</w:t></w:r></w:p></w:body></w:document>"#;
    let doc = parse(
        Path::new("nested.docx"),
        &archive("word/document.xml", xml)?,
    )?;
    assert_eq!(doc.chunks.len(), 2);
    assert_eq!(doc.chunks[0].body, "before inside & deep\tafter\nend");
    assert_eq!(doc.chunks[0].anchor.paragraph, Some(1));
    assert_eq!(doc.chunks[1].body, "next");
    assert_eq!(doc.chunks[1].anchor.paragraph, Some(2));
    Ok(())
}

#[test]
fn fallback_worksheet_names_are_ordered_numerically() -> Result {
    let bytes = archive_parts(&[
        (
            "xl/worksheets/sheet10.xml",
            b"<worksheet><c r=\"A1\"><v>10</v></c></worksheet>",
        ),
        (
            "xl/worksheets/sheet2.xml",
            b"<worksheet><c r=\"A1\"><v>2</v></c></worksheet>",
        ),
        (
            "xl/worksheets/sheet1.xml",
            b"<worksheet><c r=\"A1\"><v>1</v></c></worksheet>",
        ),
    ])?;
    let doc = parse(Path::new("numeric.xlsx"), &bytes)?;
    assert_eq!(
        doc.chunks
            .iter()
            .map(|c| c.body.as_str())
            .collect::<Vec<_>>(),
        ["| 1 |\n| --- |", "| 2 |\n| --- |", "| 10 |\n| --- |"]
    );
    assert_eq!(
        doc.chunks[1].anchor.sheet.as_deref(),
        Some("xl/worksheets/sheet2.xml")
    );
    Ok(())
}

#[test]
fn cached_encoding_declarations_remain_independent_across_documents() -> Result {
    for _ in 0..3 {
        for (source, encoding) in [
            ("# coding: big5\n繁體中文", encoding_rs::BIG5),
            (
                "<?xml version=\"1.0\" encoding=\"windows-1252\"?><text>café</text>",
                encoding_rs::WINDOWS_1252,
            ),
            ("<meta charset='shift_jis'>日本語", encoding_rs::SHIFT_JIS),
            (
                "#!/usr/bin/python\n# coding: utf-8\n中文",
                encoding_rs::UTF_8,
            ),
        ] {
            let (bytes, _, errors) = encoding.encode(source);
            assert!(!errors);
            assert_eq!(
                parse(Path::new("encoding.txt"), &bytes)?.chunks[0].body,
                source
            );
        }
        assert!(parse(Path::new("invalid.txt"), b"# coding: unsupported\ntext").is_err());
    }
    Ok(())
}
