//! EPUB glances: pull the cover image, the metadata strings, and the
//! spine chapters out of an epub container (a zip of XHTML plus an
//! OPF manifest) for Quick Look. Reading stays in the reader (Papers
//! is layer 3): the glance is the cover, who wrote the thing, and
//! glance-grade chapter text.

use std::collections::HashMap;
use std::io::Read as _;
use std::path::Path;

/// What an epub's manifest says about itself.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct BookMeta {
    pub(crate) title: Option<String>,
    pub(crate) author: Option<String>,
    /// The cover image's bytes, when the manifest points at one.
    pub(crate) cover: Option<Vec<u8>>,
}

/// Read an epub's meta: container.xml names the OPF, the OPF's
/// manifest marks the cover and the dublin core names the book.
/// None when the container or the manifest is unreadable, so the
/// caller falls back to the plain card.
pub(crate) fn read_epub_meta(path: &Path) -> Option<BookMeta> {
    let file = std::fs::File::open(path).ok()?;
    let mut zip = zip::ZipArchive::new(std::io::BufReader::new(file)).ok()?;
    let opf_path = open_opf(&mut zip)?;
    let opf_raw = read_entry(&mut zip, &opf_path)?;
    let opf_raw = String::from_utf8_lossy(&opf_raw).into_owned();
    let opf = roxmltree::Document::parse(&opf_raw).ok()?;
    let cover = opf_cover_href(&opf)
        .and_then(|href| read_entry(&mut zip, &opf_entry(&opf_path, &href)));
    Some(BookMeta {
        title: opf_dc(&opf, "title"),
        author: opf_dc(&opf, "creator"),
        cover,
    })
}

/// The spine as zip-root entry paths, in reading order: the OPF's
/// itemrefs through the manifest's id-to-href map, each href resolved
/// against the OPF's own directory. None when unreadable or when the
/// spine is empty; either way paging stays dormant.
pub(crate) fn read_spine(path: &Path) -> Option<Vec<String>> {
    let file = std::fs::File::open(path).ok()?;
    let mut zip = zip::ZipArchive::new(std::io::BufReader::new(file)).ok()?;
    let opf_path = open_opf(&mut zip)?;
    let opf_raw = read_entry(&mut zip, &opf_path)?;
    let opf_raw = String::from_utf8_lossy(&opf_raw).into_owned();
    let opf = roxmltree::Document::parse(&opf_raw).ok()?;
    let manifest: HashMap<String, String> = opf
        .descendants()
        .filter(|node| node.tag_name().name() == "item")
        .filter_map(|node| {
            Some((node.attribute("id")?.to_string(), node.attribute("href")?.to_string()))
        })
        .collect();
    let spine: Vec<String> = opf
        .descendants()
        .filter(|node| node.tag_name().name() == "itemref")
        .filter_map(|node| node.attribute("idref"))
        .filter_map(|idref| manifest.get(idref))
        .map(|href| opf_entry(&opf_path, href))
        .collect();
    (!spine.is_empty()).then_some(spine)
}

/// One spine chapter as glance blocks (1-based). None past the end
/// or on an unreadable container or entry.
pub(crate) fn read_chapter(path: &Path, chapter: usize) -> Option<Vec<ChapterBlock>> {
    let spine = read_spine(path)?;
    let href = spine.get(chapter.checked_sub(1)?)?;
    let file = std::fs::File::open(path).ok()?;
    let mut zip = zip::ZipArchive::new(std::io::BufReader::new(file)).ok()?;
    let bytes = read_entry(&mut zip, href)?;
    Some(chapter_blocks(&bytes))
}

/// The container.xml-to-OPF road every reader takes: the OPF's
/// zip-root path. The OPF itself is parsed by the caller, in place:
/// roxmltree's Document borrows its text, so it cannot travel.
fn open_opf<R: std::io::Read + std::io::Seek>(
    zip: &mut zip::ZipArchive<R>,
) -> Option<String> {
    let container = read_entry(zip, "META-INF/container.xml")?;
    let container = String::from_utf8_lossy(&container).into_owned();
    let container = roxmltree::Document::parse(&container).ok()?;
    let opf_path = container
        .descendants()
        .find(|node| node.tag_name().name() == "rootfile")
        .and_then(|node| node.attribute("full-path"))?
        .to_string();
    Some(opf_path)
}

/// The manifest item marked cover-image (properties is a space
/// list), falling back to the legacy meta name="cover" content id.
fn opf_cover_href(opf: &roxmltree::Document) -> Option<String> {
    let by_property = opf.descendants().find_map(|node| {
        let is_cover = node.tag_name().name() == "item"
            && node
                .attribute("properties")
                .map(|properties| properties.split_whitespace().any(|p| p == "cover-image"))
                .unwrap_or(false);
        if is_cover {
            node.attribute("href").map(String::from)
        } else {
            None
        }
    });
    if by_property.is_some() {
        return by_property;
    }
    let cover_id = opf.descendants().find_map(|node| {
        if node.tag_name().name() == "meta" && node.attribute("name") == Some("cover") {
            node.attribute("content").map(String::from)
        } else {
            None
        }
    })?;
    opf.descendants().find_map(|node| {
        let is_it = node.tag_name().name() == "item" && node.attribute("id") == Some(cover_id.as_str());
        if is_it {
            node.attribute("href").map(String::from)
        } else {
            None
        }
    })
}

/// The text of a dublin core element (dc:title, dc:creator), by
/// local name so the namespace spelling does not matter.
fn opf_dc(opf: &roxmltree::Document, name: &str) -> Option<String> {
    opf.descendants()
        .find(|node| node.tag_name().name() == name)
        .and_then(|node| node.text().map(|text| text.trim().to_string()))
        .filter(|text| !text.is_empty())
}

/// An OPF href is relative to the OPF's own directory, and epub
/// hrefs may be percent-encoded; this handles the %20 case and
/// ignores queries. Deeper encodings stay as-is (rare for covers).
fn opf_entry(opf_path: &str, href: &str) -> String {
    let href = href.split('?').next().unwrap_or(href).replace("%20", " ");
    match opf_path.rsplit_once('/') {
        Some((dir, _)) => format!("{dir}/{href}"),
        None => href,
    }
}

/// One block of chapter content for the glance pane.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum ChapterBlock {
    /// A paragraph of prose with tag-carried runs.
    Para {
        text: String,
        runs: Vec<ChapterRun>,
        /// Inside a blockquote: rendered indented and dim.
        quote: bool,
    },
    /// A heading, level 1..=6; renders bold at a size by level.
    Heading { level: u8, text: String, runs: Vec<ChapterRun> },
    /// A horizontal rule.
    Rule,
}

/// A styled run: a byte length of the block's text plus the flags
/// the surrounding tags carried. Tags only, no CSS is honored here:
/// italic from em/i/cite/dfn/var, bold from strong/b.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ChapterRun {
    pub(crate) len: usize,
    pub(crate) italic: bool,
    pub(crate) bold: bool,
}

/// Chapter prose caps at this many blocks, with a tail marker: a
/// glance pane, not a bottomless scroll.
const CHAPTER_BLOCKS_MAX: usize = 400;

/// XHTML to glance blocks. A hand-rolled scanner rather than a
/// strict XML parse because real-world epub XHTML uses HTML entities
/// (&nbsp;) that strict parsing rejects; a weird document degrades
/// to plain text, never to a hang. Script and style drop with their
/// content; block tags open paragraphs and headings; styling tags
/// open runs; common entities decode; whitespace collapses; the
/// result caps at CHAPTER_BLOCKS_MAX blocks with a tail marker. The
/// head's title drops too: the book label already names the thing.
pub(crate) fn chapter_blocks(bytes: &[u8]) -> Vec<ChapterBlock> {
    let raw = String::from_utf8_lossy(bytes);
    let cleaned = drop_blocks(raw.as_ref(), &["script", "style", "title"]);
    let mut b = Builder::default();
    let mut i = 0;
    while i < cleaned.len() {
        if b.capped {
            break;
        }
        let rest = &cleaned[i..];
        if rest.starts_with('<') {
            // comments and CDATA swallow to their ends
            if rest.starts_with("<!--") {
                match rest.find("-->") {
                    Some(end) => i += end + 3,
                    None => break,
                }
                continue;
            }
            if rest.starts_with("<![") {
                match rest.find("]>") {
                    Some(end) => i += end + 2,
                    None => break,
                }
                continue;
            }
            let after = &rest[1..];
            let closing = after.starts_with('/');
            let name: String = after[if closing { 1 } else { 0 }..]
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric())
                .collect::<String>()
                .to_ascii_lowercase();
            // a '>' inside a quoted attribute would end the tag
            // early: glance grade, the damage is one stray boundary
            match rest.find('>') {
                Some(end) => i += end + 1,
                None => break,
            }
            // styling flags change inside the current block, so any
            // pending space joins the run it belongs to first
            match name.as_str() {
                "em" | "i" | "cite" | "dfn" | "var" => {
                    b.flush_space();
                    if closing {
                        b.italics = b.italics.saturating_sub(1);
                    } else {
                        b.italics += 1;
                    }
                }
                "strong" | "b" => {
                    b.flush_space();
                    if closing {
                        b.bolds = b.bolds.saturating_sub(1);
                    } else {
                        b.bolds += 1;
                    }
                }
                "hr" => {
                    b.flush();
                    b.blocks.push(ChapterBlock::Rule);
                }
                // a br splits the paragraph: close enough for a glance
                "br" => b.flush(),
                "blockquote" => {
                    b.flush();
                    if closing {
                        b.quote_depth = b.quote_depth.saturating_sub(1);
                    } else {
                        b.quote_depth += 1;
                    }
                }
                name if is_block(name) => {
                    b.flush();
                    if !closing {
                        b.kind = match name.strip_prefix('h') {
                            Some(digits) if digits.len() == 1 => {
                                digits.parse::<u8>().unwrap_or(0).clamp(1, 6)
                            }
                            _ => 0,
                        };
                    }
                }
                _ => {}
            }
            continue;
        }
        if rest.starts_with('&') {
            let up_to = rest.len().min(12);
            if let Some(end) = rest[..up_to].find(';').filter(|d| *d > 0) {
                if let Some(text) = decode_entity(&rest[1..end]) {
                    b.flush_space();
                    b.push_raw(&text);
                    i += end + 1;
                    continue;
                }
            }
            b.flush_space();
            b.push_raw("&");
            i += 1;
            continue;
        }
        // source whitespace collapses to one lazy space; text copies
        let ch = rest.chars().next().unwrap();
        if ch.is_whitespace() {
            b.push_ws();
        } else {
            b.flush_space();
            let mut buf = [0u8; 4];
            b.push_raw(ch.encode_utf8(&mut buf));
        }
        i += ch.len_utf8();
    }
    b.flush();
    if b.capped {
        let tail = "\u{2026} the chapter continues in the reader";
        b.blocks.push(ChapterBlock::Para {
            text: tail.to_string(),
            runs: vec![ChapterRun { len: tail.len(), italic: true, bold: false }],
            quote: false,
        });
    }
    b.blocks
}

/// Assembles the block list: the paragraph under construction, the
/// run flags in force, and the cap state.
#[derive(Default)]
struct Builder {
    blocks: Vec<ChapterBlock>,
    text: String,
    runs: Vec<ChapterRun>,
    /// 0 is a paragraph, 1..=6 a heading level
    kind: u8,
    italics: u32,
    bolds: u32,
    quote_depth: u32,
    pending_space: bool,
    /// the cap rejected a flush: content after the cap is not read
    capped: bool,
}

impl Builder {
    fn push_raw(&mut self, text: &str) {
        let extend = self.runs.last_mut().filter(|run| {
            run.italic == (self.italics > 0) && run.bold == (self.bolds > 0)
        });
        match extend {
            Some(run) => run.len += text.len(),
            None => self.runs.push(ChapterRun {
                len: text.len(),
                italic: self.italics > 0,
                bold: self.bolds > 0,
            }),
        }
        self.text.push_str(text);
    }

    /// Whitespace collapses to one space, applied lazily so a block
    /// never starts or ends with it.
    fn push_ws(&mut self) {
        self.pending_space = true;
    }

    /// Land a pending space on the open block; called before any run
    /// flag change or block flush so the space joins the right run.
    fn flush_space(&mut self) {
        if self.pending_space && !self.text.is_empty() && !self.text.ends_with(' ') {
            self.push_raw(" ");
        }
        self.pending_space = false;
    }

    /// Close the open block into the list; an empty one just resets
    /// the kind so stray text after it builds plain paragraphs.
    fn flush(&mut self) {
        self.flush_space();
        let quote = self.quote_depth > 0;
        let kind = self.kind;
        self.kind = 0;
        if self.text.is_empty() {
            self.runs.clear();
            return;
        }
        if self.blocks.len() >= CHAPTER_BLOCKS_MAX {
            self.capped = true;
            return;
        }
        let block = match kind {
            0 => ChapterBlock::Para {
                text: std::mem::take(&mut self.text),
                runs: std::mem::take(&mut self.runs),
                quote,
            },
            level => ChapterBlock::Heading {
                level,
                text: std::mem::take(&mut self.text),
                runs: std::mem::take(&mut self.runs),
            },
        };
        self.blocks.push(block);
    }
}

/// Remove <block ...> ... </block> spans, content and all, case
/// insensitively. Offsets come from one lowercase view of the whole
/// input, so removals never chase a moving index. An unterminated
/// block ends the content: what follows is script source, not text.
fn drop_blocks(raw: &str, blocks: &[&str]) -> String {
    let lower = raw.to_ascii_lowercase();
    let mut cuts: Vec<(usize, usize)> = Vec::new();
    for block in blocks {
        let opener = format!("<{block}");
        let closer = format!("</{block}");
        let mut from = 0;
        while let Some(at) = lower[from..].find(opener.as_str()) {
            let start = from + at;
            let Some(gt) = lower[start..].find('>') else { break };
            let content_start = start + gt + 1;
            let Some(end) = lower[content_start..].find(closer.as_str()) else {
                cuts.push((start, raw.len()));
                break;
            };
            let block_end = content_start + end;
            let span_end = match lower[block_end..].find('>') {
                Some(gt) => block_end + gt + 1,
                None => raw.len(),
            };
            cuts.push((start, span_end));
            from = span_end;
        }
    }
    // apply from the back so earlier offsets stay valid: cuts are
    // collected per block name, not in document order
    cuts.sort_by(|a, b| b.0.cmp(&a.0));
    let mut out = raw.to_owned();
    for (start, end) in cuts {
        out.replace_range(start..end, "");
    }
    out
}

fn is_block(name: &str) -> bool {
    matches!(
        name,
        "p" | "div"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "li"
            | "tr"
            | "blockquote"
            | "section"
            | "article"
            | "header"
            | "footer"
            | "table"
            | "ul"
            | "ol"
            | "pre"
            | "figure"
            | "figcaption"
            | "hr"
    )
}

/// The common named entities plus numeric references; None leaves
/// the ampersand literal.
fn decode_entity(inner: &str) -> Option<String> {
    let named = match inner {
        "amp" => "&".to_string(),
        "lt" => "<".to_string(),
        "gt" => ">".to_string(),
        "quot" => "\"".to_string(),
        "apos" => "'".to_string(),
        "nbsp" => " ".to_string(),
        "mdash" => "\u{2014}".to_string(),
        "ndash" => "\u{2013}".to_string(),
        "hellip" => "\u{2026}".to_string(),
        "ldquo" => "\u{201c}".to_string(),
        "rdquo" => "\u{201d}".to_string(),
        "lsquo" => "\u{2018}".to_string(),
        "rsquo" => "\u{2019}".to_string(),
        _ => {
            // numeric: decimal &#NNN; or hex &#xHH;
            let number = inner.strip_prefix('#')?;
            let value = if let Some(hex) = number.strip_prefix('x').or_else(|| number.strip_prefix('X')) {
                u32::from_str_radix(hex, 16).ok()?
            } else {
                number.parse::<u32>().ok()?
            };
            char::from_u32(value)?.to_string()
        }
    };
    Some(named)
}

fn read_entry<R: std::io::Read + std::io::Seek>(
    zip: &mut zip::ZipArchive<R>,
    name: &str,
) -> Option<Vec<u8>> {
    let mut entry = zip.by_name(name).ok()?;
    let mut buf = Vec::new();
    entry.read_to_end(&mut buf).ok()?;
    Some(buf)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::Write as _;

    /// A minimal but real epub, written in-memory with the same zip
    /// crate the reader uses.
    pub(crate) fn write_epub(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut buf = std::io::Cursor::new(Vec::new());
        let mut zip = zip::ZipWriter::new(&mut buf);
        let options = zip::write::SimpleFileOptions::default();
        for (name, bytes) in entries {
            zip.start_file(*name, options).unwrap();
            zip.write_all(bytes).unwrap();
        }
        zip.finish().unwrap();
        buf.into_inner()
    }

    fn sample_container() -> &'static [u8] {
        br#"<?xml version="1.0"?>
            <container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
              <rootfiles><rootfile full-path="OEBPS/content.opf"/></rootfiles>
            </container>"#
    }

    fn sample_opf(spine_ids: &[&str]) -> Vec<u8> {
        let mut opf = String::from(
            "<?xml version=\"1.0\"?>
            <package xmlns=\"http://www.idpf.org/2007/opf\" version=\"3.0\">
              <metadata xmlns:dc=\"http://purl.org/dc/elements/1.1/\">
                <dc:title>  Neuromancer </dc:title>
                <dc:creator>William Gibson</dc:creator>
              </metadata>
              <manifest>
                <item id=\"cover\" href=\"images/cover%20art.png\" properties=\"cover-image\"/>
                <item id=\"c1\" href=\"text/ch1.xhtml\"/>
                <item id=\"c2\" href=\"text/ch2.xhtml\"/>
              </manifest>
              <spine>",
        );
        for id in spine_ids {
            opf.push_str(&format!("<itemref idref=\"{id}\"/>"));
        }
        opf.push_str("</spine></package>");
        opf.into_bytes()
    }

    #[test]
    fn reads_cover_and_metadata_from_a_real_epub() {
        let mut png = Vec::new();
        image::DynamicImage::new_rgb8(1, 1)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let bytes = write_epub(&[
            ("mimetype", b"application/epub+zip"),
            ("META-INF/container.xml", sample_container()),
            ("OEBPS/content.opf", &sample_opf(&["c1"])),
            ("OEBPS/images/cover art.png", &png),
        ]);
        let dir = std::env::temp_dir().join(format!("koguma-epub-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sample.epub");
        std::fs::write(&path, &bytes).unwrap();
        let meta = read_epub_meta(&path).unwrap();
        assert_eq!(meta.title.as_deref(), Some("Neuromancer"));
        assert_eq!(meta.author.as_deref(), Some("William Gibson"));
        // the href resolved through the OPF's directory and the %20
        assert_eq!(meta.cover.as_deref(), Some(png.as_slice()));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The blocks' prose, one block per line; rules become a bare
    /// newline. For content assertions in and out of this module.
    pub(crate) fn blocks_text(blocks: &[ChapterBlock]) -> String {
        blocks
            .iter()
            .map(|block| match block {
                ChapterBlock::Rule => "\n".to_string(),
                ChapterBlock::Para { text, .. } | ChapterBlock::Heading { text, .. } => {
                    format!("{text}\n")
                }
            })
            .collect()
    }

    #[test]
    fn spine_walks_manifest_order_and_chapters_read_stripped() {
        let ch1 = b"<html><head><style>p { color: red }</style></head>\
            <body><h1>First</h1><p>alpha &amp; omega</p><script>bad()</script>\
            <p>second&nbsp;line</p></body></html>";
        let ch2 = b"<html><body><p>beta &lt;tag&gt; text</p><p>more &#65; here</p></body></html>";
        let bytes = write_epub(&[
            ("META-INF/container.xml", sample_container()),
            ("OEBPS/content.opf", &sample_opf(&["c1", "c2"])),
            ("OEBPS/text/ch1.xhtml", ch1),
            ("OEBPS/text/ch2.xhtml", ch2),
        ]);
        let dir = std::env::temp_dir().join(format!("koguma-epub-spine-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sample.epub");
        std::fs::write(&path, &bytes).unwrap();
        let spine = read_spine(&path).unwrap();
        assert_eq!(
            spine,
            vec!["OEBPS/text/ch1.xhtml".to_string(), "OEBPS/text/ch2.xhtml".to_string()]
        );
        let one = read_chapter(&path, 1).unwrap();
        // the heading structured, the entities decoded, nbsp a plain
        // space, style and script content gone with their blocks
        assert_eq!(
            one[0],
            ChapterBlock::Heading {
                level: 1,
                text: "First".to_string(),
                runs: vec![ChapterRun { len: 5, italic: false, bold: false }],
            }
        );
        assert!(matches!(&one[1], ChapterBlock::Para { text, quote: false, .. } if text == "alpha & omega"));
        assert!(matches!(&one[2], ChapterBlock::Para { text, quote: false, .. } if text == "second line"));
        assert_eq!(one.len(), 3);
        let two = read_chapter(&path, 2).unwrap();
        let two_text = blocks_text(&two);
        assert!(two_text.contains("beta <tag> text"), "entity undecoded: {two_text:?}");
        assert!(two_text.contains("more A here"), "numeric entity undecoded");
        assert_eq!(read_chapter(&path, 3), None, "past the end");
        assert_eq!(read_chapter(&path, 0), None, "cover has no chapter");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn tags_become_structures_and_runs() {
        let blocks = chapter_blocks(
            br#"<h2>Title</h2><blockquote><p>quoted <em>deep</em> words</p></blockquote>
                <p>plain <strong>bold</strong> &amp; <em>both</em></p><hr/><p>after</p>"#,
        );
        assert_eq!(
            blocks[0],
            ChapterBlock::Heading {
                level: 2,
                text: "Title".to_string(),
                runs: vec![ChapterRun { len: 5, italic: false, bold: false }],
            }
        );
        assert_eq!(
            blocks[1],
            ChapterBlock::Para {
                text: "quoted deep words".to_string(),
                runs: vec![
                    ChapterRun { len: 7, italic: false, bold: false },
                    ChapterRun { len: 4, italic: true, bold: false },
                    ChapterRun { len: 6, italic: false, bold: false },
                ],
                quote: true,
            }
        );
        assert_eq!(
            blocks[2],
            ChapterBlock::Para {
                text: "plain bold & both".to_string(),
                runs: vec![
                    ChapterRun { len: 6, italic: false, bold: false },
                    ChapterRun { len: 4, italic: false, bold: true },
                    ChapterRun { len: 3, italic: false, bold: false },
                    ChapterRun { len: 4, italic: true, bold: false },
                ],
                quote: false,
            }
        );
        assert_eq!(blocks[3], ChapterBlock::Rule);
        assert!(matches!(&blocks[4], ChapterBlock::Para { text, quote: false, .. } if text == "after"));
    }

    #[test]
    fn chapter_blocks_degrade_and_cap() {
        // a '>' inside an attribute: one stray boundary, no hang
        let weird = br#"<p title="a>b">text</p><p>more</p>"#;
        let text = blocks_text(&chapter_blocks(weird));
        assert!(text.contains("text"));
        assert!(text.contains("more"));
        // unterminated tags swallow the rest instead of looping
        assert_eq!(blocks_text(&chapter_blocks(b"<p>tail")), "tail\n");
        assert!(chapter_blocks(b"<p open").is_empty());
        // the cap: CHAPTER_BLOCKS_MAX blocks plus the tail marker
        let mut many = String::from("<body>");
        for i in 0..1500 {
            many.push_str(&format!("<p>line {i}</p>"));
        }
        many.push_str("</body>");
        let capped = chapter_blocks(many.as_bytes());
        assert_eq!(capped.len(), CHAPTER_BLOCKS_MAX + 1);
        assert!(blocks_text(&capped).contains("continues"));
    }

    #[test]
    fn legacy_meta_cover_fallback() {
        let opf = roxmltree::Document::parse(
            "<package><metadata><meta name=\"cover\" content=\"c1\"/></metadata>\
             <manifest><item id=\"c1\" href=\"pics/cover.jpg\"/></manifest></package>",
        )
        .unwrap();
        assert_eq!(opf_cover_href(&opf).as_deref(), Some("pics/cover.jpg"));
    }

    #[test]
    fn href_resolution_and_garbage() {
        assert_eq!(
            opf_entry("OEBPS/content.opf", "images/cover.png"),
            "OEBPS/images/cover.png"
        );
        assert_eq!(opf_entry("content.opf", "cover.png"), "cover.png");
        assert_eq!(opf_entry("OEBPS/content.opf", "a%20b.png"), "OEBPS/a b.png");
        let empty = roxmltree::Document::parse("<nothing/>").unwrap();
        assert_eq!(opf_cover_href(&empty), None);
        assert_eq!(opf_dc(&empty, "title"), None);
    }
}
