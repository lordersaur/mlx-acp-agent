use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use reqwest::Url;

pub const MAX_SEARCH_OUTPUT_CHARS: usize = 6000;
pub const IGNORED_LIST_DIR_NAMES: &[&str] =
    &[".git", ".venv", "venv", "__pycache__", "site-packages"];

pub fn safe_path(cwd: &Path, path: &str) -> Result<PathBuf> {
    let base = cwd
        .canonicalize()
        .with_context(|| format!("failed to resolve workspace root {}", cwd.display()))?;

    let normalized_path = path.trim();
    let relative_or_absolute = if normalized_path.starts_with("file:") {
        let url = Url::parse(normalized_path).context("invalid file URL")?;
        url.to_file_path()
            .map_err(|_| anyhow::anyhow!("invalid file URL path"))?
    } else {
        PathBuf::from(normalized_path)
    };

    let target = if relative_or_absolute.is_absolute() {
        relative_or_absolute
    } else {
        base.join(relative_or_absolute)
    };
    let resolved = normalize_existing_parent(&target)?;

    if !resolved.starts_with(&base) {
        bail!("Path escapes project root");
    }

    Ok(resolved)
}

pub fn read_file(cwd: &Path, path: &str) -> Result<String> {
    let file = safe_path(cwd, path)?;
    fs::read_to_string(&file).with_context(|| format!("failed to read {}", file.display()))
}

pub fn write_file(cwd: &Path, path: &str, content: &str) -> Result<String> {
    let file = safe_path(cwd, path)?;
    if let Some(parent) = file.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create parent dir {}", parent.display()))?;
    }
    fs::write(&file, content).with_context(|| format!("failed to write {}", file.display()))?;
    Ok(file.display().to_string())
}

pub fn list_dir(cwd: &Path, path: &str) -> Result<Vec<String>> {
    let directory = safe_path(cwd, path)?;
    let mut entries = fs::read_dir(&directory)
        .with_context(|| format!("failed to list {}", directory.display()))?
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let name = entry.file_name();
            let name = name.to_str()?;
            if IGNORED_LIST_DIR_NAMES.contains(&name) || name.ends_with(".pyc") {
                return None;
            }
            Some(name.to_owned())
        })
        .collect::<Vec<_>>();
    entries.sort();
    Ok(entries)
}

pub fn search_code(cwd: &Path, query: &str, glob: Option<&str>) -> Result<String> {
    let base = cwd
        .canonicalize()
        .with_context(|| format!("failed to resolve workspace root {}", cwd.display()))?;

    let mut rg_args = vec![
        "-n".to_owned(),
        "-C".to_owned(),
        "3".to_owned(),
        "--hidden".to_owned(),
        "--glob".to_owned(),
        "!.git".to_owned(),
        "--glob".to_owned(),
        "!.venv/**".to_owned(),
        "--glob".to_owned(),
        "!**/__pycache__/**".to_owned(),
        "--glob".to_owned(),
        "!**/site-packages/**".to_owned(),
        "--glob".to_owned(),
        "!**/*.pyc".to_owned(),
        "--max-columns".to_owned(),
        "200".to_owned(),
        "--max-count".to_owned(),
        "80".to_owned(),
    ];
    if let Some(glob) = glob {
        rg_args.push("--glob".to_owned());
        rg_args.push(glob.to_owned());
    }
    rg_args.push(query.to_owned());
    rg_args.push(base.display().to_string());

    let output = match Command::new("rg").args(&rg_args).output() {
        Ok(output) => output,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Command::new("grep")
            .args(["-R", "-n", query, &base.display().to_string()])
            .output()
            .context("failed to run grep fallback")?,
        Err(error) => return Err(error).context("failed to run rg"),
    };

    let stdout = truncate_output(
        String::from_utf8_lossy(&output.stdout).trim(),
        MAX_SEARCH_OUTPUT_CHARS,
    );
    let stderr = truncate_output(String::from_utf8_lossy(&output.stderr).trim(), 2000);
    let code = output.status.code().unwrap_or(-1);

    if code != 0 && code != 1 {
        return Ok(format!("search failed\nstderr:\n{stderr}"));
    }

    if stdout.is_empty() {
        return Ok("No matches found.".to_owned());
    }

    Ok(stdout)
}

pub fn delete_path(cwd: &Path, path: &str, recursive: bool) -> Result<String> {
    let base = cwd
        .canonicalize()
        .with_context(|| format!("failed to resolve workspace root {}", cwd.display()))?;
    let target = safe_path(cwd, path)?;

    if target == base {
        bail!("Refusing to delete the workspace root");
    }
    if !target.exists() {
        bail!("Delete target does not exist");
    }

    let metadata = fs::symlink_metadata(&target)
        .with_context(|| format!("failed to stat {}", target.display()))?;
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        if recursive {
            fs::remove_dir_all(&target)
                .with_context(|| format!("failed to recursively delete {}", target.display()))?;
        } else {
            fs::remove_dir(&target)
                .with_context(|| format!("failed to delete directory {}", target.display()))?;
        }
    } else {
        fs::remove_file(&target)
            .with_context(|| format!("failed to delete {}", target.display()))?;
    }

    Ok(target.display().to_string())
}

pub fn apply_patch(
    cwd: &Path,
    path: &str,
    old_text: &str,
    new_text: &str,
    replace_all: bool,
) -> Result<String> {
    let file = safe_path(cwd, path)?;
    let original =
        fs::read_to_string(&file).with_context(|| format!("failed to read {}", file.display()))?;
    let occurrences = original.matches(old_text).count();

    if occurrences == 0 {
        bail!("Patch target not found in file");
    }
    if !replace_all && occurrences > 1 {
        bail!(
            "Patch target appears multiple times; set replace_all=true or use a more specific old_text"
        );
    }

    let updated = if replace_all {
        original.replace(old_text, new_text)
    } else {
        original.replacen(old_text, new_text, 1)
    };

    fs::write(&file, updated).with_context(|| format!("failed to write {}", file.display()))?;
    Ok(file.display().to_string())
}

fn normalize_existing_parent(path: &Path) -> Result<PathBuf> {
    let mut existing_ancestor = path;
    while !existing_ancestor.exists() {
        existing_ancestor = existing_ancestor
            .parent()
            .ok_or_else(|| anyhow::anyhow!("path has no existing ancestor"))?;
    }

    let canonical_ancestor = existing_ancestor
        .canonicalize()
        .with_context(|| format!("failed to resolve {}", existing_ancestor.display()))?;

    let suffix = path
        .strip_prefix(existing_ancestor)
        .with_context(|| format!("failed to normalize {}", path.display()))?;

    if suffix.as_os_str().is_empty() {
        return Ok(canonical_ancestor);
    }

    Ok(canonical_ancestor.join(suffix))
}

fn truncate_output(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    let omitted = text.len() - limit;
    let end = floor_char_boundary(text, limit);
    format!("{}\n... [truncated {omitted} chars]", &text[..end])
}

fn floor_char_boundary(text: &str, index: usize) -> usize {
    let mut index = index.min(text.len());
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

#[allow(dead_code)]
fn ends_with_pyc(path: &Path) -> bool {
    path.extension()
        .and_then(OsStr::to_str)
        .map(|extension| extension == "pyc")
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::{
        apply_patch, delete_path, list_dir, read_file, safe_path, search_code, write_file,
    };

    #[test]
    fn safe_path_rejects_escape() {
        let tempdir = TempDir::new().expect("tempdir");
        let error = safe_path(tempdir.path(), "../outside").expect_err("should reject escape");
        assert_eq!(error.to_string(), "Path escapes project root");
    }

    #[test]
    fn read_and_list_dir_match_python_behavior() {
        let tempdir = TempDir::new().expect("tempdir");
        fs::write(tempdir.path().join("a.txt"), "hello").expect("write file");
        fs::write(tempdir.path().join("z.pyc"), "ignored").expect("write pyc");
        fs::create_dir(tempdir.path().join(".git")).expect("create .git");
        fs::create_dir(tempdir.path().join("src")).expect("create src");

        assert_eq!(
            read_file(tempdir.path(), "a.txt").expect("read file"),
            "hello"
        );
        assert_eq!(
            list_dir(tempdir.path(), ".").expect("list dir"),
            vec!["a.txt".to_owned(), "src".to_owned()]
        );
    }

    #[test]
    fn read_file_reports_actual_missing_path_error() {
        let tempdir = TempDir::new().expect("tempdir");

        let error = read_file(tempdir.path(), "tools/mod.rs").expect_err("missing file");

        let debug = format!("{error:?}");
        assert!(debug.contains("failed to read"));
        assert!(debug.contains("tools/mod.rs"));
        assert!(debug.contains("No such file") || debug.contains("os error"));
        assert!(!debug.contains("search_code_tool"));
        assert!(!debug.contains("list_dir_tool"));
    }

    #[test]
    fn write_patch_and_delete_match_python_behavior() {
        let tempdir = TempDir::new().expect("tempdir");

        let written = write_file(tempdir.path(), "nested/file.txt", "alpha\nalpha\n")
            .expect("write nested file");
        assert!(written.ends_with("/nested/file.txt"));

        let error = apply_patch(tempdir.path(), "nested/file.txt", "alpha", "beta", false)
            .expect_err("should reject ambiguous single replace");
        assert_eq!(
            error.to_string(),
            "Patch target appears multiple times; set replace_all=true or use a more specific old_text"
        );

        let patched = apply_patch(tempdir.path(), "nested/file.txt", "alpha", "beta", true)
            .expect("patch file");
        assert!(patched.ends_with("/nested/file.txt"));
        assert_eq!(
            read_file(tempdir.path(), "nested/file.txt").expect("read patched file"),
            "beta\nbeta\n"
        );

        let deleted = delete_path(tempdir.path(), "nested/file.txt", false).expect("delete file");
        assert!(deleted.ends_with("/nested/file.txt"));
        assert!(!tempdir.path().join("nested/file.txt").exists());
    }

    #[test]
    fn delete_rejects_workspace_root() {
        let tempdir = TempDir::new().expect("tempdir");
        let error = delete_path(tempdir.path(), ".", true).expect_err("should reject root");
        assert_eq!(error.to_string(), "Refusing to delete the workspace root");
    }

    #[test]
    fn search_code_returns_matches_or_no_matches() {
        let tempdir = TempDir::new().expect("tempdir");
        fs::create_dir(tempdir.path().join("src")).expect("create src");
        fs::write(
            tempdir.path().join("src/lib.rs"),
            "fn alpha() {}\nfn beta() {}\n",
        )
        .expect("write file");

        let matches = search_code(tempdir.path(), "alpha", Some("*.rs")).expect("search");
        assert!(matches.contains("src/lib.rs:1:fn alpha() {}"));

        let none = search_code(tempdir.path(), "gamma", Some("*.rs")).expect("search");
        assert_eq!(none, "No matches found.");
    }
}
