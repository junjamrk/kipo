mod support;

use std::fs;
use std::io::Write;
use std::path::Path;

use lopdf::{
    content::{Content, Operation},
    dictionary, Document, Object, Stream,
};
use serde_json::Value;
use support::{run, Result};
use zip::{write::SimpleFileOptions, ZipWriter};

fn pdf(path: &Path, text: bool) -> Result {
    let mut doc = Document::with_version("1.5");
    let pages_id = doc.new_object_id();
    let font =
        doc.add_object(dictionary! {"Type"=>"Font", "Subtype"=>"Type1", "BaseFont"=>"Helvetica"});
    let resources = doc.add_object(dictionary! {"Font"=>dictionary! {"F1"=>font}});
    let mut pages = Vec::new();
    for label in ["Firstpdfanchor", "Secondpdfanchor"] {
        let content = Content {
            operations: if text {
                vec![
                    Operation::new("BT", vec![]),
                    Operation::new("Tf", vec!["F1".into(), 12.into()]),
                    Operation::new("Tj", vec![Object::string_literal(label)]),
                    Operation::new("ET", vec![]),
                ]
            } else {
                vec![]
            },
        };
        let stream = doc.add_object(Stream::new(dictionary! {}, content.encode()?));
        pages.push(doc.add_object(dictionary! {"Type"=>"Page","Parent"=>pages_id,"Contents"=>stream,"Resources"=>resources,"MediaBox"=>vec![0.into(),0.into(),600.into(),800.into()]}).into());
    }
    doc.objects.insert(
        pages_id,
        dictionary! {"Type"=>"Pages","Kids"=>pages,"Count"=>2}.into(),
    );
    let catalog = doc.add_object(dictionary! {"Type"=>"Catalog","Pages"=>pages_id});
    doc.trailer.set("Root", catalog);
    doc.save(path)?;
    Ok(())
}

fn zip(path: &Path, parts: &[(&str, &str)]) -> Result {
    let mut zip = ZipWriter::new(fs::File::create(path)?);
    for (name, text) in parts {
        zip.start_file(*name, SimpleFileOptions::default())?;
        zip.write_all(text.as_bytes())?;
    }
    zip.finish()?;
    Ok(())
}

fn anchor(path: &Path, query: &str, source: &str) -> Result<Value> {
    let result = run(path, &["search", query, "--limit", "100"])?;
    result["results"]
        .as_array()
        .ok_or("results")?
        .iter()
        .find(|r| {
            r["anchor"]["file_path"]
                .as_str()
                .is_some_and(|p| p.ends_with(source))
        })
        .map(|r| r["anchor"].clone())
        .ok_or_else(|| format!("missing {source}: {result}").into())
}

#[test]
fn add_directory_routes_all_formats_and_search_returns_original_coordinates() -> Result {
    let dir = tempfile::tempdir()?;
    let library = dir.path().join("library");
    let input = dir.path().join("inputs");
    fs::create_dir(&input)?;
    run(&library, &["init", "--owner", "alice"])?;
    fs::write(input.join("guide.md"), "# Payments\nMarkdownanchor\n")?;
    fs::write(input.join("pay.rs"), "fn rustanchor() -> i32 { 1 }\n")?;
    fs::write(input.join("pay.py"), "def pythonanchor():\n    return 1\n")?;
    fs::write(
        input.join("talk.vtt"),
        "WEBVTT\n\n00:01.250 --> 00:02.750\nVttanchor\n",
    )?;
    fs::write(
        input.join("talk.srt"),
        "1\n00:00:03,500 --> 00:00:04,250\nSrtanchor\n",
    )?;
    fs::write(input.join("table.csv"), "name,value\nCsvanchor,12\n")?;
    pdf(&input.join("text.pdf"), true)?;
    pdf(&input.join("image.pdf"), false)?;
    zip(
        &input.join("word.docx"),
        &[(
            "word/document.xml",
            r#"<w:document xmlns:w="word"><w:body><w:p><w:r><w:t>Docxanchor &amp; text</w:t></w:r></w:p><w:p><w:r><w:t>Second paragraph</w:t></w:r></w:p></w:body></w:document>"#,
        )],
    )?;
    zip(
        &input.join("slides.pptx"),
        &[
            (
                "ppt/presentation.xml",
                r#"<p:presentation xmlns:p="ppt" xmlns:r="rel"><p:sldIdLst><p:sldId id="256" r:id="rId2"/></p:sldIdLst></p:presentation>"#,
            ),
            (
                "ppt/_rels/presentation.xml.rels",
                r#"<Relationships><Relationship Id="rId2" Target="slides/slide2.xml"/></Relationships>"#,
            ),
            (
                "ppt/slides/slide2.xml",
                r#"<p:sld xmlns:p="ppt" xmlns:a="draw"><a:p><a:r><a:t>Pptxanchor</a:t></a:r></a:p></p:sld>"#,
            ),
        ],
    )?;
    zip(
        &input.join("book.xlsx"),
        &[
            (
                "xl/workbook.xml",
                r#"<workbook xmlns:r="rel"><sheets><sheet name="Payments" sheetId="1" r:id="rId1"/></sheets></workbook>"#,
            ),
            (
                "xl/_rels/workbook.xml.rels",
                r#"<Relationships><Relationship Id="rId1" Target="worksheets/sheet1.xml"/></Relationships>"#,
            ),
            (
                "xl/sharedStrings.xml",
                r#"<sst><si><t>Xlsxanchor</t></si></sst>"#,
            ),
            (
                "xl/worksheets/sheet1.xml",
                r#"<worksheet><sheetData><row r="3"><c r="B3" t="s"><v>0</v></c><c r="C3" t="inlineStr"><is><t>Inlineanchor</t></is></c></row></sheetData></worksheet>"#,
            ),
        ],
    )?;
    let added = run(&library, &["add", input.to_str().ok_or("input path")?])?;
    assert_eq!(added.as_array().ok_or("array")?.len(), 11);
    let ocr = added
        .as_array()
        .ok_or("array")?
        .iter()
        .find(|r| r["uri"].as_str().is_some_and(|u| u.ends_with("image.pdf")))
        .ok_or("OCR result")?;
    assert_eq!(ocr["needs_ocr"], true);
    assert_eq!(ocr["chunks"], 0);
    let md = anchor(&library, "Markdownanchor", "guide.md")?;
    assert_eq!(md["start_line"], 1);
    assert_eq!(md["end_line"], 2);
    let rust = anchor(&library, "rustanchor", "pay.rs")?;
    assert_eq!(rust["symbol"], "rustanchor");
    assert_eq!(rust["start_line"], 1);
    assert!(rust["sig_hash"].is_string());
    let python = anchor(&library, "pythonanchor", "pay.py")?;
    assert_eq!(python["symbol"], "pythonanchor");
    assert_eq!(python["end_line"], 2);
    let first = anchor(&library, "Firstpdfanchor", "text.pdf")?;
    let second = anchor(&library, "Secondpdfanchor", "text.pdf")?;
    assert_eq!(first["page"], 1);
    assert_eq!(second["page"], 2);
    let vtt = anchor(&library, "Vttanchor", "talk.vtt")?;
    assert_eq!(vtt["start_ms"], 1250);
    assert_eq!(vtt["end_ms"], 2750);
    assert_eq!(vtt["start_line"], 3);
    let srt = anchor(&library, "Srtanchor", "talk.srt")?;
    assert_eq!(srt["start_ms"], 3500);
    assert_eq!(srt["end_ms"], 4250);
    assert_eq!(anchor(&library, "Docxanchor", "word.docx")?["paragraph"], 1);
    assert_eq!(anchor(&library, "Pptxanchor", "slides.pptx")?["slide"], 1);
    let xlsx = anchor(&library, "Xlsxanchor", "book.xlsx")?;
    assert_eq!(xlsx["sheet"], "Payments");
    assert_eq!(xlsx["cell"], "B3");
    assert_eq!(anchor(&library, "Csvanchor", "table.csv")?["start_line"], 1);
    Ok(())
}

#[test]
fn add_directory_skips_malformed_file_and_reports_failed_files_without_aborting_batch() -> Result {
    let dir = tempfile::tempdir()?;
    let library = dir.path().join("library");
    let input = dir.path().join("inputs");
    fs::create_dir(&input)?;
    run(&library, &["init", "--owner", "alice"])?;
    // `a.md` has a malformed ```kp structured block that fails extraction, alongside valid `b.md` + broken `.docx`.
    fs::write(
        input.join("a.md"),
        "# Broken KP Block\n```kp\n{\"subject\": \"broken\", invalid_json}\n```\n",
    )?;
    fs::write(input.join("b.md"), "# Valid B\nValidBMarkdownAnchor\n")?;
    fs::write(input.join("broken.docx"), b"not-a-valid-zip-archive")?;

    let out = run(
        &library,
        &["add", input.to_str().ok_or("input path")?, "--extract"],
    )?;
    let added = out["added"].as_array().ok_or("added array")?;
    let failed = out["failed_files"].as_array().ok_or("failed_files array")?;
    assert_eq!(added.len(), 1);
    assert_eq!(failed.len(), 2);
    assert!(failed
        .iter()
        .any(|f| f["path"].as_str().is_some_and(|p| p.ends_with("a.md"))
            && !f["error"].as_str().unwrap_or("").is_empty()));
    assert!(failed.iter().any(|f| f["path"]
        .as_str()
        .is_some_and(|p| p.ends_with("broken.docx"))));

    assert_eq!(
        anchor(&library, "ValidBMarkdownAnchor", "b.md")?["start_line"],
        1
    );
    Ok(())
}
