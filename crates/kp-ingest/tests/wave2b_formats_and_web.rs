mod support;

use std::io::{Cursor, Write};
use std::path::Path;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use axum::{
    extract::State,
    http::{header, StatusCode},
    response::IntoResponse,
    routing::get,
    Router,
};
use kp_ingest::doc_router::{add_url, parse};
use kp_ingest::IngestError;
use kp_model::{estimate_batch_tokens, verify_verbatim_quote, QuoteLocationHint};
use lopdf::{
    content::{Content, Operation},
    dictionary, Document as PdfDocument, Object, Stream,
};
use proptest::prelude::*;
use zip::{write::SimpleFileOptions, CompressionMethod, ZipWriter};

use support::{library, options, Result};

fn zip_archive(parts: &[(&str, &[u8])]) -> Result<Vec<u8>> {
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

fn assert_exact_byte_slices(raw: &[u8], chunks: &[kp_store::ChunkInput]) -> Result {
    assert!(!chunks.is_empty(), "expected non-empty chunks");
    for (idx, chunk) in chunks.iter().enumerate() {
        let start = usize::try_from(chunk.anchor.start_byte.ok_or("missing start_byte")?)?;
        let end = usize::try_from(chunk.anchor.end_byte.ok_or("missing end_byte")?)?;
        assert!(
            start <= end && end <= raw.len(),
            "chunk {idx} out-of-bounds {start}..{end}"
        );
        assert_eq!(
            &raw[start..end],
            chunk.body.as_bytes(),
            "chunk {idx} body must equal exact contiguous slice &raw[{start}..{end}]"
        );
        assert!(
            chunk.anchor.start_line.is_some() && chunk.anchor.end_line.is_some(),
            "chunk {idx} missing line anchor"
        );
    }
    Ok(())
}

#[test]
fn markdown_512_token_chunker_eliminates_stubs_splits_tables_and_lists_with_exact_slices() -> Result
{
    let heading_only = b"# Architecture\n## Storage\n";
    let h_doc = parse(Path::new("headings_only.md"), heading_only)?;
    assert_eq!(h_doc.chunks.len(), 1);
    assert_eq!(h_doc.chunks[0].heading, "Architecture > Storage");
    assert_exact_byte_slices(heading_only, &h_doc.chunks)?;

    let mut md = String::from(
        "# Platform Guide\nPool\nSettings\n--------\n### Connection\nKeep pool size bounded.\n\n",
    );
    md.push_str(
        "## Service Matrix\n| Service | TimeoutMs | Retries | Owner |\n| --- | --- | --- | --- |\n",
    );
    for i in 1..=65 {
        md.push_str(&format!(
            "| payment-gateway-worker-{i:02} | {} | 3 | checkout-platform-team |\n",
            1000 + i * 25
        ));
    }
    md.push_str(
        "\n## Production Guardrails\nNever execute these destructive commands in production:\n",
    );
    for i in 1..=55 {
        md.push_str(&format!(
            "- Drop live shard replica `{i:02}` via `kubectl delete statefulset pg-shard-{i:02} --force` without draining traffic first.\n"
        ));
    }

    let md_bytes = md.as_bytes();
    let doc = parse(Path::new("platform.md"), md_bytes)?;
    assert_exact_byte_slices(md_bytes, &doc.chunks)?;

    for chunk in &doc.chunks {
        let trimmed = chunk.body.trim();
        assert_ne!(trimmed, "# Platform Guide", "emitted empty H1 stub");
        assert_ne!(trimmed, "## Storage", "emitted empty H2 stub");
        assert!(
            estimate_batch_tokens(&chunk.body) <= 768,
            "chunk exceeded hard max 768 tokens: {}",
            estimate_batch_tokens(&chunk.body)
        );
    }

    assert_eq!(
        doc.chunks[0].heading,
        "Platform Guide > Pool Settings > Connection"
    );

    let table_continuations: Vec<_> = doc
        .chunks
        .iter()
        .filter(|c| {
            c.heading
                .contains("[Columns: Service | TimeoutMs | Retries | Owner]")
        })
        .collect();
    assert!(
        !table_continuations.is_empty(),
        "oversized Markdown table must split across rows with [Columns: ...] heading"
    );
    let sample_row = "| payment-gateway-worker-60 | 2500 | 3 | checkout-platform-team |";
    let row_chunk = doc
        .chunks
        .iter()
        .find(|c| c.body.contains(sample_row))
        .ok_or("missing row 60 chunk")?;
    let verified =
        verify_verbatim_quote(&row_chunk.body, sample_row, QuoteLocationHint::default())?;
    assert_eq!(verified.exact_quote, sample_row);

    let list_continuations: Vec<_> = doc
        .chunks
        .iter()
        .filter(|c| {
            c.heading.contains(
                "Platform Guide > Production Guardrails [Context: Never execute these destructive commands in production:]",
            )
        })
        .collect();
    assert!(
        !list_continuations.is_empty(),
        "oversized Markdown list must carry colon lead-in [Context: ...] in continuation headings"
    );

    Ok(())
}

#[test]
fn csv_and_txt_512_token_chunking_preserve_schema_multiline_quotes_and_exact_byte_spans() -> Result
{
    let mut csv = String::from("incident_id,region,summary,duration_sec\n");
    for i in 1..=90 {
        csv.push_str(&format!(
            "INC-{i:04},\"us-east, zone-{z}\",\"Primary failover check {i}\nVerified replica lag zero\",{}\n",
            i * 15,
            z = (i % 3) + 1
        ));
    }
    let csv_doc = parse(Path::new("incidents.csv"), csv.as_bytes())?;
    assert!(
        csv_doc.chunks.len() >= 3,
        "expected multi-chunk CSV split, got {}",
        csv_doc.chunks.len()
    );
    assert_exact_byte_slices(csv.as_bytes(), &csv_doc.chunks)?;
    for chunk in &csv_doc.chunks {
        assert_eq!(
            chunk.heading,
            "CSV [Columns: incident_id | region | summary | duration_sec]"
        );
        assert!(estimate_batch_tokens(&chunk.body) <= 768);
        assert!(
            !chunk.body.starts_with("Verified replica lag zero"),
            "quoted multiline RFC-4180 field was torn across chunks"
        );
    }

    let mut txt = String::new();
    for p in 1..=30 {
        txt.push_str(&format!(
            "Paragraph {p:02} documents the deterministic lease fencing protocol across concurrent SQLite writers and verifies rollback safety under crash injection.\n\n"
        ));
    }
    let mega_para = (1..=45)
        .map(|s| {
            format!(
                "Sentence {s:02} confirms that oversized single paragraphs split cleanly along punctuation boundaries without losing byte coordinates."
            )
        })
        .collect::<Vec<_>>()
        .join(" ");
    txt.push_str(&mega_para);
    txt.push('\n');

    let txt_doc = parse(Path::new("protocol.txt"), txt.as_bytes())?;
    assert!(txt_doc.chunks.len() >= 4);
    assert_exact_byte_slices(txt.as_bytes(), &txt_doc.chunks)?;
    for chunk in &txt_doc.chunks {
        assert!(estimate_batch_tokens(&chunk.body) <= 768);
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]
    #[test]
    fn arbitrary_utf8_markdown_and_txt_preserve_contiguous_verbatim_slices(
        sections in prop::collection::vec(
            (1u8..=4, "[A-Za-z0-9_ \u{4e00}-\u{9fa5}]{1,24}", "[A-Za-z0-9.,:;!? \n\u{4e00}-\u{9fa5}]{1,180}"),
            1..12
        )
    ) {
        let mut md = String::new();
        for (lvl, title, body) in &sections {
            md.push_str(&"#".repeat(usize::from(*lvl)));
            md.push(' ');
            md.push_str(title.trim());
            md.push('\n');
            md.push_str(body);
            md.push_str("\n\n");
        }
        let md_doc = parse(Path::new("prop.md"), md.as_bytes()).map_err(|e| TestCaseError::fail(e.to_string()))?;
        for ch in &md_doc.chunks {
            let s = ch.anchor.start_byte.unwrap_or(0) as usize;
            let e = ch.anchor.end_byte.unwrap_or(0) as usize;
            prop_assert_eq!(&md.as_bytes()[s..e], ch.body.as_bytes());
        }
        let txt_doc = parse(Path::new("prop.txt"), md.as_bytes()).map_err(|e| TestCaseError::fail(e.to_string()))?;
        for ch in &txt_doc.chunks {
            let s = ch.anchor.start_byte.unwrap_or(0) as usize;
            let e = ch.anchor.end_byte.unwrap_or(0) as usize;
            prop_assert_eq!(&md.as_bytes()[s..e], ch.body.as_bytes());
        }
    }
}

#[test]
fn docx_pptx_and_2000_row_xlsx_preserve_structure_headings_tables_and_notes() -> Result {
    let styles_xml = br#"<w:styles xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
        <w:style w:type="paragraph" w:styleId="H1Custom"><w:name w:val="heading 1"/></w:style>
        <w:style w:type="paragraph" w:styleId="H2Outline"><w:pPr><w:outlineLvl w:val="1"/></w:pPr></w:style>
    </w:styles>"#;
    let mut doc_xml = String::from(
        r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body>
        <w:p><w:pPr><w:pStyle w:val="H1Custom"/></w:pPr><w:r><w:t>Security Policy</w:t></w:r></w:p>
        <w:p><w:pPr><w:pStyle w:val="H2Outline"/></w:pPr><w:r><w:t>Database Operations</w:t></w:r></w:p>
        <w:p><w:r><w:t>Never run these commands on primary:</w:t></w:r></w:p>
        <w:p><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="1"/></w:numPr></w:pPr><w:r><w:t>DROP TABLE accounts</w:t></w:r></w:p>
        <w:p><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="1"/></w:numPr></w:pPr><w:r><w:t>TRUNCATE audit_log</w:t></w:r></w:p>
        <w:tbl>
            <w:tr><w:tc><w:p><w:r><w:t>Role</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>Access</w:t></w:r></w:p></w:tc></w:tr>"#,
    );
    for r in 1..=50 {
        doc_xml.push_str(&format!(
            "<w:tr><w:tc><w:p><w:r><w:t>db-operator-role-{r:02}</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>read-only-replica-pool-{r:02}</w:t></w:r></w:p></w:tc></w:tr>"
        ));
    }
    doc_xml.push_str("</w:tbl></w:body></w:document>");

    let docx_bytes = zip_archive(&[
        ("word/styles.xml", styles_xml),
        ("word/document.xml", doc_xml.as_bytes()),
    ])?;
    let docx = parse(Path::new("policy.docx"), &docx_bytes)?;
    assert!(
        docx.chunks
            .iter()
            .all(|c| c.heading == "Security Policy > Database Operations"),
        "expected style-mapped breadcrumb on all chunks: {:?}",
        docx.chunks.iter().map(|c| &c.heading).collect::<Vec<_>>()
    );
    assert!(docx.chunks.iter().all(|c| c.anchor.paragraph.is_some()));
    assert_eq!(
        docx.chunks[0].body,
        "Never run these commands on primary:\n- DROP TABLE accounts\n- TRUNCATE audit_log"
    );
    let docx_tbl_chunks: Vec<_> = docx.chunks.iter().skip(1).collect();
    assert!(
        docx_tbl_chunks.len() >= 2,
        "oversized DOCX table must split across rows"
    );
    for tc in docx_tbl_chunks {
        assert!(
            tc.body.starts_with("| Role | Access |\n| --- | --- |"),
            "every split DOCX table chunk must repeat GFM header: {}",
            tc.body
        );
    }

    let pptx_bytes = zip_archive(&[
        (
            "ppt/presentation.xml",
            br#"<p:presentation xmlns:p="p" xmlns:r="r"><p:sldIdLst><p:sldId id="256" r:id="rId1"/></p:sldIdLst></p:presentation>"#,
        ),
        (
            "ppt/_rels/presentation.xml.rels",
            br#"<Relationships><Relationship Id="rId1" Target="slides/slide1.xml"/></Relationships>"#,
        ),
        (
            "ppt/slides/slide1.xml",
            br#"<p:sld xmlns:p="p" xmlns:a="a"><p:cSld><p:spTree>
                <p:sp><p:nvSpPr><p:nvPr><p:ph type="title"/></p:nvPr></p:nvSpPr><p:txBody><a:p><a:r><a:t>Zero Downtime Cutover</a:t></a:r></a:p></p:txBody></p:sp>
                <p:sp><p:nvSpPr><p:nvPr/></p:nvSpPr><p:txBody><a:p><a:r><a:t>Drain read traffic first</a:t></a:r></a:p><a:p><a:r><a:t>Verify WAL LSN parity</a:t></a:r></a:p></p:txBody></p:sp>
                <p:graphicFrame><a:graphic><a:graphicData><a:tbl>
                    <a:tr><a:tc><a:txBody><a:p><a:r><a:t>Phase</a:t></a:r></a:p></a:txBody></a:tc><a:tc><a:txBody><a:p><a:r><a:t>Budget</a:t></a:r></a:p></a:txBody></a:tc></a:tr>
                    <a:tr><a:tc><a:txBody><a:p><a:r><a:t>Switchover</a:t></a:r></a:p></a:txBody></a:tc><a:tc><a:txBody><a:p><a:r><a:t>30s</a:t></a:r></a:p></a:txBody></a:tc></a:tr>
                </a:tbl></a:graphicData></a:graphic></p:graphicFrame>
            </p:spTree></p:cSld></p:sld>"#,
        ),
        (
            "ppt/slides/_rels/slide1.xml.rels",
            br#"<Relationships><Relationship Id="rIdNotes" Target="../notesSlides/notesSlide1.xml"/></Relationships>"#,
        ),
        (
            "ppt/notesSlides/notesSlide1.xml",
            br#"<p:notes xmlns:p="p" xmlns:a="a"><p:cSld><p:spTree>
                <p:sp><p:nvSpPr><p:nvPr><p:ph type="body"/></p:nvPr></p:nvSpPr><p:txBody><a:p><a:r><a:t>Remind on-call to pause cron workers before LSN freeze.</a:t></a:r></a:p></p:txBody></p:sp>
                <p:sp><p:nvSpPr><p:nvPr><p:ph type="sldNum"/></p:nvPr></p:nvSpPr><p:txBody><a:p><a:r><a:t>1</a:t></a:r></a:p></p:txBody></p:sp>
            </p:spTree></p:cSld></p:notes>"#,
        ),
    ])?;
    let pptx = parse(Path::new("deck.pptx"), &pptx_bytes)?;
    assert_eq!(pptx.chunks.len(), 3);
    assert!(pptx.chunks.iter().all(|c| c.anchor.slide == Some(1)));
    assert_eq!(pptx.chunks[0].heading, "Slide 1 > Zero Downtime Cutover");
    assert_eq!(
        pptx.chunks[0].body,
        "Drain read traffic first\nVerify WAL LSN parity"
    );
    assert_eq!(pptx.chunks[1].heading, "Slide 1 > Zero Downtime Cutover");
    assert_eq!(
        pptx.chunks[1].body,
        "| Phase | Budget |\n| --- | --- |\n| Switchover | 30s |"
    );
    assert_eq!(
        pptx.chunks[2].heading,
        "Slide 1 > Zero Downtime Cutover > Notes"
    );
    assert_eq!(
        pptx.chunks[2].body,
        "Remind on-call to pause cron workers before LSN freeze."
    );

    let shared_xml = br#"<sst xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
        <si><t>Service</t></si>
        <si><t>Active</t></si>
        <si><t>Region</t></si>
        <si><t>LatencyMs</t></si>
        <si><t>checkout-api</t></si>
    </sst>"#;
    let mut sheet_xml = String::from(
        r#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData>
        <row r="1"><c r="A1" t="s"><v>0</v></c><c r="B1" t="s"><v>1</v></c><c r="C1" t="s"><v>2</v></c><c r="D1" t="s"><v>3</v></c></row>"#,
    );
    for r in 2..=2001 {
        let b_flag = usize::from(r % 2 == 0);
        sheet_xml.push_str(&format!(
            r#"<row r="{r}"><c r="A{r}" t="s"><v>4</v></c><c r="B{r}" t="b"><v>{b_flag}</v></c><c r="C{r}" t="inlineStr"><is><t>ap-east-1</t></is></c><c r="D{r}"><v>{}</v></c></row>"#,
            10 + (r % 50)
        ));
    }
    sheet_xml.push_str("</sheetData></worksheet>");
    let xlsx_bytes = zip_archive(&[
        (
            "xl/workbook.xml",
            br#"<workbook xmlns:r="r"><sheets><sheet name="SLOs" sheetId="1" r:id="rId1"/></sheets></workbook>"#,
        ),
        (
            "xl/_rels/workbook.xml.rels",
            br#"<Relationships><Relationship Id="rId1" Target="worksheets/sheet1.xml"/></Relationships>"#,
        ),
        ("xl/sharedStrings.xml", shared_xml),
        ("xl/worksheets/sheet1.xml", sheet_xml.as_bytes()),
    ])?;
    let xlsx = parse(Path::new("slos.xlsx"), &xlsx_bytes)?;
    assert!(
        xlsx.chunks.len() > 10 && xlsx.chunks.len() < 100,
        "2000-row XLSX must batch into < 100 chunks, got {}",
        xlsx.chunks.len()
    );
    assert!(xlsx.chunks[0].heading.starts_with("SLOs (rows 1.."));
    assert!(xlsx.chunks[0]
        .anchor
        .cell
        .as_deref()
        .is_some_and(|c| c.starts_with("A1:D")));
    for chunk in &xlsx.chunks {
        assert_eq!(chunk.anchor.sheet.as_deref(), Some("SLOs"));
        assert!(chunk
            .body
            .starts_with("| Service | Active | Region | LatencyMs |\n| --- | --- | --- | --- |"));
    }
    assert!(xlsx.chunks[0]
        .body
        .contains("| checkout-api | true | ap-east-1 |"));
    assert!(xlsx.chunks[0]
        .body
        .contains("| checkout-api | false | ap-east-1 |"));

    Ok(())
}

fn build_pdf_bytes(pages_spec: &[(&[&str], bool)], encrypted: bool) -> Result<Vec<u8>> {
    let mut doc = PdfDocument::with_version("1.5");
    let pages_id = doc.new_object_id();
    let font_id = doc.add_object(dictionary! {
        "Type" => "Font",
        "Subtype" => "Type0",
        "BaseFont" => "STSong-Light",
        "Encoding" => "UniGB-UTF16-H",
    });
    let img_id = doc.add_object(Stream::new(
        dictionary! {
            "Type" => "XObject",
            "Subtype" => "Image",
            "Width" => 800,
            "Height" => 1100,
            "ColorSpace" => "DeviceGray",
            "BitsPerComponent" => 8,
        },
        vec![0u8; 64],
    ));

    let mut page_refs = Vec::new();
    for (lines, with_image) in pages_spec {
        let mut ops = Vec::new();
        if *with_image {
            ops.push(Operation::new("Do", vec!["Im1".into()]));
        }
        if !lines.is_empty() {
            for line in *lines {
                ops.push(Operation::new("BT", vec![]));
                ops.push(Operation::new("Tf", vec!["F1".into(), 12.into()]));
                let encoded = encode_pdf_utf16be(line);
                ops.push(Operation::new(
                    "Tj",
                    vec![Object::String(encoded, lopdf::StringFormat::Hexadecimal)],
                ));
                ops.push(Operation::new("ET", vec![]));
            }
        }
        let stream_id = doc.add_object(Stream::new(
            dictionary! {},
            Content { operations: ops }.encode()?,
        ));
        let res = if *with_image {
            dictionary! {
                "Font" => dictionary! { "F1" => font_id },
                "XObject" => dictionary! { "Im1" => img_id },
            }
        } else {
            dictionary! {
                "Font" => dictionary! { "F1" => font_id },
            }
        };
        let res_id = doc.add_object(res);
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "Contents" => stream_id,
            "Resources" => res_id,
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
        });
        page_refs.push(page_id.into());
    }
    doc.objects.insert(
        pages_id,
        dictionary! {
            "Type" => "Pages",
            "Kids" => page_refs,
            "Count" => i64::try_from(pages_spec.len())?,
        }
        .into(),
    );
    let catalog = doc.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => pages_id,
    });
    doc.trailer.set("Root", catalog);
    if encrypted {
        let enc_id = doc.add_object(dictionary! {
            "Filter" => "Standard",
            "V" => 2,
            "R" => 3,
            "Length" => 128,
            "P" => -4,
            "O" => Object::string_literal(vec![0x11u8; 32]),
            "U" => Object::string_literal(vec![0x22u8; 32]),
        });
        doc.trailer.set("Encrypt", enc_id);
    }
    let mut out = Vec::new();
    doc.save_to(&mut out)?;
    Ok(out)
}

fn encode_pdf_utf16be(s: &str) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(s.len() * 2);
    for unit in s.encode_utf16() {
        bytes.extend_from_slice(&unit.to_be_bytes());
    }
    bytes
}

#[test]
fn pdf_gate_folds_ligatures_strips_furniture_and_quarantines_watermarks_mojibake_and_encrypted(
) -> Result {
    let enc_bytes = build_pdf_bytes(&[(&["Secret text"], false)], true)?;
    assert!(matches!(
        parse(Path::new("encrypted.pdf"), &enc_bytes),
        Err(IngestError::Invalid("encrypted PDF requires decryption"))
    ));

    let clean_3p = build_pdf_bytes(
        &[
            (
                &[
                    "Kipo Architecture Spec - Draft",
                    "Section 1 covers o\u{FB00}line con\u{FB01}guration and work\u{FB02}ow.",
                    "Page 1 of 3",
                ],
                false,
            ),
            (
                &[
                    "Kipo Architecture Spec - Draft",
                    "Section 2 covers o\u{FB03}ce role isolation and ba\u{FB04}e filters.",
                    "Page 2 of 3",
                ],
                false,
            ),
            (
                &[
                    "Kipo Architecture Spec - Draft",
                    "Section 3 verifies lease fencing invariants.",
                    "Page 3 of 3",
                ],
                false,
            ),
        ],
        false,
    )?;
    let clean_doc = parse(Path::new("spec.pdf"), &clean_3p)?;
    assert!(!clean_doc.needs_ocr);
    assert_eq!(clean_doc.chunks.len(), 3);
    assert_eq!(clean_doc.chunks[0].anchor.page, Some(1));
    assert!(clean_doc.chunks[0]
        .body
        .contains("offline configuration and workflow."));
    assert!(clean_doc.chunks[1]
        .body
        .contains("office role isolation and baffle filters."));
    for ch in &clean_doc.chunks {
        assert!(!ch.body.contains("Kipo Architecture Spec - Draft"));
        assert!(!ch.body.contains("Page "));
    }

    let camscanner = build_pdf_bytes(&[(&["Scanned with CamScanner"], true)], false)?;
    let cam_doc = parse(Path::new("scan.pdf"), &camscanner)?;
    assert!(cam_doc.needs_ocr);
    assert!(cam_doc.chunks.is_empty());

    let pua_mojibake = build_pdf_bytes(
        &[(
            &["\u{E001}\u{E002}\u{E003}\u{FFFD}\u{E010} broken cmap stream"],
            false,
        )],
        false,
    )?;
    let pua_doc = parse(Path::new("pua.pdf"), &pua_mojibake)?;
    assert!(pua_doc.needs_ocr);
    assert!(pua_doc.chunks.is_empty());

    let shifted_soup = build_pdf_bytes(
        &[(&["!#$%&*+<=>@^~|\\`{}!#$%&*+<=>@^~|\\`{}"], false)],
        false,
    )?;
    let soup_doc = parse(Path::new("shifted.pdf"), &shifted_soup)?;
    assert!(soup_doc.needs_ocr);
    assert!(soup_doc.chunks.is_empty());

    let hybrid = build_pdf_bytes(
        &[
            (
                &["Clean digital contract clause 1 governing payment settlement SLAs."],
                false,
            ),
            (
                &["Clean digital contract clause 2 governing dispute arbitration."],
                false,
            ),
            (&["DocuSign Envelope ID: 9F8A"], true),
        ],
        false,
    )?;
    let hybrid_doc = parse(Path::new("hybrid.pdf"), &hybrid)?;
    assert!(
        hybrid_doc.needs_ocr,
        "hybrid PDF must flag needs_ocr = true"
    );
    assert_eq!(hybrid_doc.chunks.len(), 2);
    assert_eq!(hybrid_doc.chunks[0].anchor.page, Some(1));
    assert_eq!(hybrid_doc.chunks[1].anchor.page, Some(2));
    assert!(hybrid_doc
        .chunks
        .iter()
        .all(|c| !c.body.contains("DocuSign")));

    Ok(())
}

#[derive(Clone, Default)]
struct WebTestState {
    fetch_count: Arc<AtomicUsize>,
}

#[tokio::test]
async fn add_url_follows_redirects_blocks_ssrf_rejects_csr_strips_injections_and_keeps_sha_stable(
) -> Result {
    let deep_bomb = format!("{}text{}", "<div>".repeat(300), "</div>".repeat(300));
    assert!(matches!(
        parse(Path::new("bomb.html"), deep_bomb.as_bytes()),
        Err(IngestError::Invalid(
            "HTML nesting depth exceeds safety limit"
        ))
    ));

    let (_dir, mut store) = library()?;
    let opts = options()?;
    let state = WebTestState::default();

    let app = Router::new()
        .route(
            "/hop1",
            get(|| async { (StatusCode::MOVED_PERMANENTLY, [(header::LOCATION, "/hop2#frag")], "") }),
        )
        .route(
            "/hop2",
            get(|| async { (StatusCode::FOUND, [(header::LOCATION, "/docs/guide#install")], "") }),
        )
        .route(
            "/loop-a",
            get(|| async { (StatusCode::FOUND, [(header::LOCATION, "/loop-b")], "") }),
        )
        .route(
            "/loop-b",
            get(|| async { (StatusCode::FOUND, [(header::LOCATION, "/loop-a")], "") }),
        )
        .route(
            "/too-many/{n}",
            get(|axum::extract::Path(n): axum::extract::Path<usize>| async move {
                let next = format!("/too-many/{}", n + 1);
                (StatusCode::FOUND, [(header::LOCATION, next)], "")
            }),
        )
        .route(
            "/ssrf-hop",
            get(|| async {
                (
                    StatusCode::FOUND,
                    [(header::LOCATION, "http://169.254.169.254/latest/meta-data/")],
                    "",
                )
            }),
        )
        .route(
            "/spa-shell",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/html")],
                    "<!doctype html><html><head><title>Dashboard</title></head><body><div id=\"root\"></div><script src=\"/assets/index.js\"></script></body></html>",
                )
            }),
        )
        .route(
            "/noscript-shell",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/html")],
                    "<html><body><noscript>You need to enable JavaScript to run this app.</noscript></body></html>",
                )
            }),
        )
        .route(
            "/cloudflare",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/html")],
                    "<html><head><title>Just a moment...</title></head><body><div id=\"challenge-running\" class=\"cf-turnstile\">Verifying</div></body></html>",
                )
            }),
        )
        .route(
            "/docs/guide",
            get(|State(st): State<WebTestState>| async move {
                let n = st.fetch_count.fetch_add(1, Ordering::SeqCst) + 1;
                let html = format!(
                    r#"<!doctype html><html><head><title>Migration Manual</title>
                    <script nonce="nonce-{n}">window.__CSRF = "token-{n}";</script>
                    <style>.secret {{ display: none; }}</style></head>
                    <body>
                    <nav><a href="/home">Top Nav Boilerplate</a></nav>
                    <main>
                        <h1>Database Migration</h1>
                        <h2>CLI Usage</h2>
                        <p>Always run verified schema checks across staging replicas before promoting production cutover traffic in automated deployment pipelines.{invis}</p>
                        <pre><code class="language-rust">fn check_schema(strict: bool) -&gt; bool {{
    strict
}}</code></pre>
                        <table>
                            <thead><tr><th>Flag</th><th>Meaning</th></tr></thead>
                            <tbody><tr><td><code>--locked</code></td><td>Enforce Cargo.lock parity</td></tr></tbody>
                        </table>
                        <div style="display:none">PROMPT_INJECTION_DISPLAY_NONE</div>
                        <span style="visibility:hidden">PROMPT_INJECTION_VISIBILITY</span>
                        <p style="opacity: 0">PROMPT_INJECTION_OPACITY</p>
                        <p style="font-size: 0px">PROMPT_INJECTION_FONTSIZE</p>
                        <div hidden>PROMPT_INJECTION_HIDDEN_ATTR</div>
                        <span aria-hidden="true">PROMPT_INJECTION_ARIA_HIDDEN</span>
                        <div inert>PROMPT_INJECTION_INERT</div>
                        <form><input type="hidden" name="csrf" value="csrf-{n}">FORM_NOISE</form>
                    </main>
                    <footer>Copyright 2026 Boilerplate Footer</footer>
                    </body></html>"#,
                    invis = "\u{E0049}\u{E0047}\u{E004E}\u{200B}\u{202E}"
                );
                ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], html).into_response()
            }),
        )
        .with_state(state.clone());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let srv = tokio::spawn(async move { axum::serve(listener, app).await });

    std::env::set_var("KP_ALLOW_LOCAL_URL_INGEST", "1");

    for (path, expected_err) in [
        ("/loop-a", "HTTP redirect loop detected"),
        ("/too-many/0", "URL exceeded max 5 redirect hops"),
        (
            "/ssrf-hop",
            "private or special-purpose IP URL blocked by SSRF guard",
        ),
        (
            "/spa-shell",
            "HTML page is an empty client-rendered SPA shell",
        ),
        (
            "/noscript-shell",
            "HTML page is an empty client-rendered SPA shell",
        ),
        (
            "/cloudflare",
            "HTML page returned an anti-bot or CAPTCHA challenge",
        ),
    ] {
        match add_url(
            &mut store,
            &format!("http://{addr}{path}"),
            &opts,
            false,
            None,
        )
        .await
        {
            Err(IngestError::Invalid(msg)) => assert_eq!(msg, expected_err, "path {path}"),
            Err(other) => return Err(format!("path {path}: unexpected error {other}").into()),
            Ok(_) => return Err(format!("path {path}: expected error `{expected_err}`").into()),
        }
    }

    let first = add_url(
        &mut store,
        &format!("http://{addr}/hop1#initial-fragment"),
        &opts,
        false,
        None,
    )
    .await?;
    assert_eq!(first.uri, format!("http://{addr}/docs/guide"));

    assert!(matches!(
        add_url(
            &mut store,
            &format!("http://{addr}/docs/guide"),
            &opts,
            false,
            None,
        )
        .await,
        Err(IngestError::Store(kp_store::StoreError::Conflict))
    ));
    assert_eq!(state.fetch_count.load(Ordering::SeqCst), 2);

    let (_dir2, mut store2) = library()?;
    let third = add_url(
        &mut store2,
        &format!("http://{addr}/docs/guide"),
        &opts,
        false,
        None,
    )
    .await?;
    assert_eq!(state.fetch_count.load(Ordering::SeqCst), 3);
    assert_eq!(
        first.source.content_sha256, third.source.content_sha256,
        "changing <script nonce> and hidden CSRF tokens must NOT alter stored Markdown sha256"
    );

    let stored_md = store
        .raw_source_text(&first.source.source_id)?
        .ok_or("missing stored markdown")?;
    assert_eq!(
        first.source.content_sha256,
        kp_model::ContentHash::digest(stored_md.as_bytes())
    );
    for forbidden in [
        "Top Nav Boilerplate",
        "Boilerplate Footer",
        "PROMPT_INJECTION",
        "FORM_NOISE",
        "nonce-",
        "token-",
        "\u{E0049}",
        "\u{200B}",
        "\u{202E}",
    ] {
        assert!(
            !stored_md.contains(forbidden),
            "stored Markdown leaked `{forbidden}`: {stored_md}"
        );
    }
    assert!(
        stored_md.contains("```rust\nfn check_schema(strict: bool) -> bool {\n    strict\n}\n```")
    );
    assert!(stored_md.contains("| Flag"));
    assert!(stored_md.contains("`--locked`"));

    let stored_chunks: Vec<kp_store::ChunkInput> = store
        .source_anchored_chunks(&first.source.source_id)?
        .into_iter()
        .map(|sc| kp_store::ChunkInput {
            heading: sc.heading,
            body: sc.body,
            anchor: sc.anchor,
        })
        .collect();
    assert!(!stored_chunks.is_empty());
    assert!(stored_chunks
        .iter()
        .any(|c| c.heading == "Database Migration > CLI Usage"));
    assert_exact_byte_slices(stored_md.as_bytes(), &stored_chunks)?;

    std::env::remove_var("KP_ALLOW_LOCAL_URL_INGEST");
    srv.abort();
    Ok(())
}
