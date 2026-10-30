//! Read-only full-text search in PDFs in a selected directory.
//!
//! A separate one-document automation session is used for every file, so neither the
//! caller's open documents nor its text cache is modified. Discovery and results are bounded.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::{Args, Automation, Result, failed};

const MAX_ENTRIES: usize = 10_000;
const MAX_DEPTH: usize = 64;
const MAX_FILES: usize = 5_000;
const MAX_MATCHES: usize = 2_000;
const MAX_WARNINGS: usize = 100;
const MAX_QUERY_BYTES: usize = 4_096;

/// Never follow links, including Windows reparse points and directory junctions.
fn linked(metadata: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

fn is_pdf(path: &Path) -> bool {
    path.extension().and_then(|ext| ext.to_str()).is_some_and(|ext| ext.eq_ignore_ascii_case("pdf"))
}

/// Discover before extracting. Sort each directory, bound entries and nesting, and skip links.
fn discover(folder: &Path, recursive: bool) -> Result<Vec<PathBuf>> {
    fn visit(path: &Path, recursive: bool, depth: usize, visited: &mut usize, files: &mut Vec<PathBuf>) -> Result<()> {
        *visited = visited.saturating_add(1);
        if *visited > MAX_ENTRIES || depth > MAX_DEPTH {
            return Err(failed("PDF search exceeds 10000 directory entries or 64 folder levels"));
        }
        let metadata = path.symlink_metadata().map_err(failed)?;
        if linked(&metadata) {
            return Ok(());
        }
        if metadata.is_file() {
            if is_pdf(path) {
                if files.len() >= MAX_FILES {
                    return Err(failed("PDF search exceeds 5000 files; choose a smaller directory"));
                }
                files.push(path.to_path_buf());
            }
        } else if metadata.is_dir() && (depth == 0 || recursive) {
            let mut children = Vec::new();
            for entry in std::fs::read_dir(path).map_err(failed)? {
                if children.len() >= MAX_ENTRIES.saturating_sub(*visited) {
                    return Err(failed("PDF search exceeds 10000 directory entries"));
                }
                children.push(entry.map_err(failed)?.path());
            }
            children.sort();
            for child in children {
                visit(&child, recursive, depth.saturating_add(1), visited, files)?;
            }
        }
        Ok(())
    }

    let metadata = folder.symlink_metadata().map_err(failed)?;
    if linked(&metadata) || !metadata.is_dir() {
        return Err(failed("directory must be a real folder, not a link or file"));
    }
    let mut files = Vec::new();
    let mut visited = 0;
    visit(folder, recursive, 0, &mut visited, &mut files)?;
    Ok(files)
}

impl Automation {
    /// Search a folder without opening the files in the caller's session.
    /// Unreadable/encrypted PDFs are reported, not silently treated as no matches.
    pub(crate) fn text_find_folder(&self, args: &Args) -> Result<Value> {
        let folder = self.resolve(args.str("directory")?, false)?;
        let query = args.str("query")?;
        if query.trim().is_empty() || query.len() > MAX_QUERY_BYTES {
            return Err(Args::wrong("query", "nonempty text of at most 4096 UTF-8 bytes"));
        }
        let recursive = args.opt_bool("recursive")?.unwrap_or(false);
        let limit = args.opt_int("limit")?.unwrap_or(500);
        if !(1..=MAX_MATCHES as i64).contains(&limit) {
            return Err(Args::wrong("limit", "an integer from 1 to 2000"));
        }
        let limit = usize::try_from(limit).map_err(failed)?;
        let paths = discover(&folder, recursive)?;
        let file_count = paths.len();

        let mut matches = Vec::new();
        let mut warnings = Vec::new();
        let mut warning_count = 0usize;
        let mut searched_files = 0usize;
        let mut matched_files = 0usize;
        let mut truncated = false;

        for path in paths {
            // Confine each file again after discovery. No link/junction may be followed.
            let path_str = match path.to_str() {
                Some(path) => path,
                None => {
                    warning_count += 1;
                    if warnings.len() < MAX_WARNINGS {
                        warnings.push(json!({"error": "PDF path is not UTF-8"}));
                    }
                    continue;
                }
            };
            let checked = self.resolve(path_str, false);
            let result: Result<Value> = match checked {
                Ok(checked) if checked == path && path.symlink_metadata().is_ok_and(|meta| meta.is_file() && !linked(&meta)) => {
                    let mut reader = Automation::new();
                    reader.root = self.root.clone();
                    reader.roots = self.roots.clone();
                    let opened = reader.doc_open(&Args(&json!({"path":path_str})));
                    match opened {
                        Ok(document) => document
                            .get("doc")
                            .and_then(Value::as_u64)
                            .ok_or_else(|| failed("opened PDF has no document id"))
                            .and_then(|id| {
                                let remaining = limit.saturating_sub(matches.len());
                                reader.text_find(&Args(&json!({"doc":id,"query":query,"limit":remaining.saturating_add(1)})))
                            }),
                        Err(err) => Err(err),
                    }
                }
                Ok(_) => Err(failed("PDF path changed after discovery")),
                Err(err) => Err(err),
            };
            searched_files += 1;

            match result {
                Ok(found) => {
                    let Some(rows) = found.get("matches").and_then(Value::as_array) else {
                        return Err(failed("text_find returned no matches array"));
                    };
                    if !rows.is_empty() {
                        matched_files += 1;
                    }
                    for row in rows {
                        if matches.len() >= limit {
                            truncated = true;
                            break;
                        }
                        let mut row = row.clone();
                        if let Some(map) = row.as_object_mut() {
                            map.insert("file".into(), json!(path_str));
                            map.insert("name".into(), json!(path.file_name().map_or_else(|| "".into(), |n| n.to_string_lossy().into_owned())));
                        }
                        matches.push(row);
                    }
                    let failures = found.get("warning_count").and_then(Value::as_u64).unwrap_or(0);
                    if failures > 0 {
                        warning_count = warning_count.saturating_add(usize::try_from(failures).unwrap_or(usize::MAX));
                        if warnings.len() < MAX_WARNINGS {
                            warnings.push(json!({"file":path_str,"warning_count":failures,"reason":"some PDF pages could not be read"}));
                        }
                    }
                    if truncated || found.get("next").is_some() {
                        truncated = true;
                        break;
                    }
                }
                Err(err) => {
                    warning_count = warning_count.saturating_add(1);
                    if warnings.len() < MAX_WARNINGS {
                        warnings.push(json!({"file":path_str,"error":err.to_string()}));
                    }
                }
            }
        }

        let count = matches.len();
        Ok(json!({
            "directory":folder.to_string_lossy(), "query":query, "recursive":recursive,
            "files":file_count, "searched_files":searched_files, "matched_files":matched_files,
            "count":count, "matches":matches, "truncated":truncated,
            "complete":!truncated && warning_count == 0 && searched_files == file_count,
            "warning_count":warning_count, "warnings_truncated":warning_count > warnings.len(),
            "warnings":warnings,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folder_discovery_is_sorted_nonrecursive_by_default_and_ignores_extensions() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("b.PDF"), b"pdf").unwrap();
        std::fs::write(dir.path().join("a.pdf"), b"pdf").unwrap();
        std::fs::write(dir.path().join("c.txt"), b"ignore").unwrap();
        std::fs::create_dir(dir.path().join("nested")).unwrap();
        std::fs::write(dir.path().join("nested").join("d.pdf"), b"pdf").unwrap();

        let direct = discover(dir.path(), false).unwrap();
        assert_eq!(direct.len(), 2);
        assert_eq!(direct[0].file_name().unwrap(), "a.pdf");
        assert_eq!(direct[1].file_name().unwrap(), "b.PDF");
        let recursive = discover(dir.path(), true).unwrap();
        assert_eq!(recursive.len(), 3);
    }

    #[test]
    fn folder_search_rejects_empty_queries_and_files_as_roots() {
        let dir = tempfile::tempdir().unwrap();
        let engine = Automation::new().with_root(dir.path()).unwrap();
        let empty = engine.text_find_folder(&Args(&json!({"directory":".","query":"  "})));
        assert!(empty.is_err());
        std::fs::write(dir.path().join("file.pdf"), b"not a pdf").unwrap();
        assert!(discover(&dir.path().join("file.pdf"), false).is_err());
    }

    /// All fixtures are generated here: there are no external or binary assets.
    fn sample_pdf(text: &str) -> Vec<u8> {
        let contents = format!("BT /F1 12 Tf 40 100 Td ({text}) Tj ET");
        let objects = [
            "<< /Type /Catalog /Pages 2 0 R >>".to_owned(),
            "<< /Type /Pages /Count 1 /Kids [3 0 R] >>".to_owned(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Resources << /Font << /F1 4 0 R >> >> /Contents 5 0 R >>".to_owned(),
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_owned(),
            format!("<< /Length {} >>\nstream\n{contents}\nendstream", contents.len()),
        ];
        let mut pdf = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        for (index, object) in objects.iter().enumerate() {
            offsets.push(pdf.len());
            pdf.extend_from_slice(format!("{} 0 obj\n{object}\nendobj\n", index + 1).as_bytes());
        }
        let xref = pdf.len();
        pdf.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", offsets.len() + 1).as_bytes());
        for offset in offsets {
            pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
        }
        pdf.extend_from_slice(format!("trailer\n<< /Size 6 /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n").as_bytes());
        pdf
    }

    #[test]
    fn folder_search_finds_unopened_documents_and_reports_unreadable_files() {
        let dir = tempfile::tempdir().unwrap();
        let a = sample_pdf("Invoice received");
        let b = sample_pdf("Invoice approved");
        std::fs::write(dir.path().join("a.pdf"), &a).unwrap();
        std::fs::write(dir.path().join("b.PDF"), &b).unwrap();
        std::fs::write(dir.path().join("bad.pdf"), b"not a PDF").unwrap();
        std::fs::create_dir(dir.path().join("nested")).unwrap();
        std::fs::write(dir.path().join("nested").join("c.pdf"), sample_pdf("Invoice archived")).unwrap();

        let engine = Automation::new().with_root(dir.path()).unwrap();
        assert!(engine.session().docs().is_empty());
        let direct = engine.text_find_folder(&Args(&json!({"directory":".","query":"invoice"}))).unwrap();
        assert_eq!(direct["count"], 2, "{direct}");
        assert_eq!(direct["matched_files"], 2);
        assert_eq!(direct["warning_count"], 1);
        assert_eq!(direct["complete"], false);
        assert_eq!(direct["matches"][0]["page"], 1);
        assert!(direct["matches"][0]["file"].as_str().unwrap().ends_with("a.pdf"));
        assert_eq!(std::fs::read(dir.path().join("a.pdf")).unwrap(), a, "search must not modify files");

        let nested = engine.text_find_folder(&Args(&json!({"directory":".","query":"invoice","recursive":true}))).unwrap();
        assert_eq!(nested["count"], 3, "{nested}");
        assert!(engine.session().docs().is_empty(), "search must not open tabs in the caller");

        let capped = engine.text_find_folder(&Args(&json!({"directory":".","query":"invoice","limit":1}))).unwrap();
        assert_eq!(capped["count"], 1);
        assert_eq!(capped["truncated"], true);
        assert_eq!(capped["complete"], false);
    }

    #[test]
    fn public_tool_finds_pdf_text_without_opening_or_mutating_the_session() {
        let dir = tempfile::tempdir().unwrap();
        let original = sample_pdf("Warranty repairs");
        std::fs::write(dir.path().join("repair.pdf"), &original).unwrap();
        let mut engine = Automation::new().with_root(dir.path()).unwrap();
        let found = engine.call(
            "text_find_folder",
            &json!({"directory":".","query":"warranty","limit":10}),
        ).unwrap();
        assert!(matches!(
            found.as_slice(),
            [crate::dispatch::Content::Json(result)]
                if result["count"] == 1 && result["matches"][0]["page"] == 1
        ), "{found:?}");
        assert!(engine.session().docs().is_empty(), "search must not add an open document");
        assert_eq!(std::fs::read(dir.path().join("repair.pdf")).unwrap(), original);
        assert!(engine.call(
            "text_find_folder",
            &json!({"directory":"..","query":"warranty"}),
        ).is_err(), "the read root must confine the selected folder");
    }

    #[cfg(unix)]
    #[test]
    fn folder_search_does_not_follow_symlinked_subfolders() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.pdf"), b"not consulted").unwrap();
        symlink(outside.path(), dir.path().join("external")).unwrap();
        assert!(discover(dir.path(), true).unwrap().is_empty());
    }
}
