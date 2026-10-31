//! What a document's name says it is: the one extension → content-type table
//! for files whose bytes don't prove their own type.
//!
//! Pictures and clips prove themselves — the cli sniffs their header, the api
//! probes them — so they aren't here. A document can't: a .docx is a ZIP like
//! any other, and its name is the only evidence. Every side that reads that
//! evidence reads THIS list: the cli declares an attachment's type from it
//! (`mafold attach`), the api types an upload that didn't say from it, and
//! serves an old row that said "I don't know" by it. Before this table the cli
//! had one list and the api another, and Office documents were in neither —
//! every .docx / .xlsx / .pptx a bot sent was stored, served and shown as
//! `application/octet-stream`.
//!
//! A type here is a hint for display and for the app that opens a download.
//! Whether bytes may be SERVED as that type is the api's object allow-list
//! (`files::object_content_type`); nothing here widens it.

const DOCUMENTS: &[(&str, &str)] = &[
    ("html", "text/html; charset=utf-8"),
    ("htm", "text/html; charset=utf-8"),
    ("css", "text/css; charset=utf-8"),
    ("js", "text/javascript; charset=utf-8"),
    ("mjs", "text/javascript; charset=utf-8"),
    ("json", "application/json"),
    ("svg", "image/svg+xml"),
    ("csv", "text/csv; charset=utf-8"),
    ("md", "text/markdown; charset=utf-8"),
    ("markdown", "text/markdown; charset=utf-8"),
    ("txt", "text/plain; charset=utf-8"),
    ("log", "text/plain; charset=utf-8"),
    ("pdf", "application/pdf"),
    ("zip", "application/zip"),
    ("docx", "application/vnd.openxmlformats-officedocument.wordprocessingml.document"),
    ("xlsx", "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"),
    ("pptx", "application/vnd.openxmlformats-officedocument.presentationml.presentation"),
    ("doc", "application/msword"),
    ("xls", "application/vnd.ms-excel"),
    ("ppt", "application/vnd.ms-powerpoint"),
];

/// The type a document with this extension is (`docx`, any case, no dot).
pub fn for_extension(ext: &str) -> Option<&'static str> {
    DOCUMENTS.iter().find(|(e, _)| e.eq_ignore_ascii_case(ext)).map(|(_, m)| *m)
}

/// The type a document called `name` is, by its last extension. A name with
/// no extension is nothing to go on.
pub fn for_name(name: &str) -> Option<&'static str> {
    name.rsplit_once('.').and_then(|(_, ext)| for_extension(ext))
}

/// The extension a document of this type is saved under — the first one the
/// table lists for it, parameters ignored.
pub fn extension_for(mime: &str) -> Option<&'static str> {
    let base = mime.split(';').next().unwrap_or("").trim();
    DOCUMENTS
        .iter()
        .find(|(_, m)| m.split(';').next().unwrap_or("").eq_ignore_ascii_case(base))
        .map(|(e, _)| *e)
}

/// A declared type that says nothing: empty, or `…/octet-stream` — how an
/// uploader writes "I don't know" (the cli for a document it had no entry
/// for, a browser for a File without a type). The name is the better answer.
pub fn is_undeclared(mime: &str) -> bool {
    let base = mime.split(';').next().unwrap_or("").trim();
    base.is_empty() || base.to_ascii_lowercase().ends_with("/octet-stream")
}

#[cfg(test)]
mod mime_tests {
    use super::*;

    #[test]
    fn office_documents_are_known_by_name() {
        assert_eq!(for_name("2027预算.XLSX"), Some("application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"));
        assert_eq!(for_name("报告.docx"), Some("application/vnd.openxmlformats-officedocument.wordprocessingml.document"));
        assert_eq!(for_name("deck.pptx"), Some("application/vnd.openxmlformats-officedocument.presentationml.presentation"));
        assert_eq!(for_extension("ppt"), Some("application/vnd.ms-powerpoint"));
        // The last extension decides; no extension decides nothing.
        assert_eq!(for_name("a.docx.zip"), Some("application/zip"));
        assert_eq!(for_name("docx"), None);
        assert_eq!(for_name("archive.tar.gz"), None);
        // Pictures prove themselves by their bytes: not this table's call.
        assert_eq!(for_name("shot.png"), None);
    }

    #[test]
    fn a_type_maps_back_to_its_extension() {
        assert_eq!(extension_for("application/vnd.openxmlformats-officedocument.wordprocessingml.document"), Some("docx"));
        assert_eq!(extension_for("text/markdown; charset=utf-8"), Some("md"));
        assert_eq!(extension_for("Text/HTML"), Some("html"));
        assert_eq!(extension_for("application/octet-stream"), None);
        for (ext, mime) in DOCUMENTS {
            let back = extension_for(mime).unwrap();
            assert_eq!(for_extension(back), Some(*mime), "{ext}");
        }
    }

    #[test]
    fn octet_stream_and_nothing_are_no_declaration() {
        for m in ["", "  ", "application/octet-stream", "Application/Octet-Stream; x=1", "binary/octet-stream"] {
            assert!(is_undeclared(m), "{m:?}");
        }
        for m in ["image/png", "text/plain; charset=utf-8", "application/zip"] {
            assert!(!is_undeclared(m), "{m:?}");
        }
    }
}
