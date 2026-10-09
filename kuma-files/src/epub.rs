//! EPUB glances: pull the cover image and the metadata strings out
//! of an epub container (a zip of XHTML plus an OPF manifest) for
//! Quick Look's book glance. Reading stays in the reader (Papers is
//! layer 3): the glance is the cover and who wrote the thing.

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
    let container = read_entry(&mut zip, "META-INF/container.xml")?;
    let container = String::from_utf8_lossy(&container).into_owned();
    let container = roxmltree::Document::parse(&container).ok()?;
    let opf_path = container
        .descendants()
        .find(|node| node.tag_name().name() == "rootfile")
        .and_then(|node| node.attribute("full-path"))?;
    let opf = read_entry(&mut zip, &opf_path)?;
    let opf = String::from_utf8_lossy(&opf).into_owned();
    let opf = roxmltree::Document::parse(&opf).ok()?;
    let cover = opf_cover_href(&opf)
        .and_then(|href| read_entry(&mut zip, &opf_entry(&opf_path, &href)));
    Some(BookMeta {
        title: opf_dc(&opf, "title"),
        author: opf_dc(&opf, "creator"),
        cover,
    })
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

    #[test]
    fn reads_cover_and_metadata_from_a_real_epub() {
        let mut png = Vec::new();
        image::DynamicImage::new_rgb8(1, 1)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let opf = br#"<?xml version="1.0"?>
            <package xmlns="http://www.idpf.org/2007/opf" version="3.0">
              <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
                <dc:title>  Neuromancer </dc:title>
                <dc:creator>William Gibson</dc:creator>
              </metadata>
              <manifest>
                <item id="cover" href="images/cover%20art.png" properties="cover-image"/>
              </manifest>
              <spine/>
            </package>"#;
        let bytes = write_epub(&[
            ("mimetype", b"application/epub+zip"),
            ("META-INF/container.xml", sample_container()),
            ("OEBPS/content.opf", opf),
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
