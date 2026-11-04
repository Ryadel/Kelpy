//! Kelpy's memory of the server: Markdown pages in the home's `memory/` folder, listed in `MEMORY.md`, which the
//! model reads with every answer. The model reads a page with `read_memory` and notes a lasting fact with
//! `remember`, one dated line on a page of its topic; the administrator reads and edits the pages too.
//!
//! A page that other accounts could change is neither read nor written: Kelpy runs with administrator rights, and its
//! memory must not be a way to steer it. `server.md` is Kelpy's own page, written from its records: the model reads
//! it, never writes it.

use std::path::{Path, PathBuf};

use crate::{ToolError, invalid};

/// The index of the pages.
pub const INDEX: &str = "MEMORY.md";
/// The page Kelpy writes from its records.
pub const SERVER_PAGE: &str = "server";
/// The most a note may hold.
const MAX_NOTE: usize = 2 << 10;
/// The largest page read or added to.
const MAX_PAGE: u64 = 256 << 10;
/// What `MEMORY.md` says while it lists no page.
const NO_TOPICS: &str = "_None yet._";

/// A page's name, as the model gives it: lower-case letters, digits and -, such as `nginx` or `backups`.
pub fn page(value: &str) -> Result<String, ToolError> {
    let value = value.trim().trim_end_matches(".md");
    let fits = (1..=40).contains(&value.len())
        && value
            .chars()
            .all(|char| char.is_ascii_lowercase() || char.is_ascii_digit() || char == '-');
    if fits {
        Ok(value.to_owned())
    } else {
        Err(invalid(
            "`page` must be a page's name: lower-case letters, digits and -, such as nginx",
        ))
    }
}

/// A note as written: one line, within its limit.
pub fn note(value: &str) -> Result<String, ToolError> {
    let line = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.is_empty() || line.len() > MAX_NOTE {
        return Err(invalid(format!(
            "`note` must hold one fact, at most {} KiB",
            MAX_NOTE >> 10
        )));
    }
    Ok(line)
}

/// The page `name` of the memory in `folder`.
pub(crate) async fn read(folder: &Path, name: &str) -> Result<String, ToolError> {
    let path = folder.join(format!("{name}.md"));
    trusted(&path).await?;
    let unreadable = |source| ToolError::Read {
        path: path.clone(),
        source,
    };
    let metadata = tokio::fs::metadata(&path).await.map_err(|_| {
        invalid(format!(
            "there is no memory page named {name}: the pages are listed in MEMORY.md"
        ))
    })?;
    if metadata.len() > MAX_PAGE {
        return Err(ToolError::TooLarge(path));
    }
    tokio::fs::read_to_string(&path).await.map_err(unreadable)
}

/// Adds `note`, dated `today`, to the page `name`, creating it and listing it in the index if needed. Says where.
pub(crate) async fn remember(
    folder: &Path,
    name: &str,
    note: &str,
    today: &str,
) -> Result<String, ToolError> {
    if name == SERVER_PAGE {
        return Err(invalid(
            "server.md is written by Kelpy from what it changed: note facts on another page",
        ));
    }
    let path = folder.join(format!("{name}.md"));
    let written = |source| ToolError::Write {
        path: path.clone(),
        source,
    };
    let existing = match tokio::fs::metadata(&path).await {
        Ok(metadata) if metadata.len() > MAX_PAGE => return Err(ToolError::TooLarge(path)),
        Ok(_) => {
            trusted(&path).await?;
            Some(read(folder, name).await?)
        }
        Err(_) => None,
    };
    let mut content = existing.unwrap_or_else(|| format!("# {}\n\n", title(name)));
    if !content.ends_with('\n') {
        content.push('\n');
    }
    content.push_str(&format!("- {today}: {note}\n"));
    tokio::fs::write(&path, &content).await.map_err(written)?;
    restrict(&path).await;
    list(folder, name).await?;
    Ok(format!("Remembered on memory/{name}.md."))
}

/// Lists the page `name` in the index, unless it is there.
async fn list(folder: &Path, name: &str) -> Result<(), ToolError> {
    let path = folder.join(INDEX);
    trusted(&path).await?;
    let index = tokio::fs::read_to_string(&path).await.unwrap_or_default();
    let link = format!("({name}.md)");
    if index.contains(&link) {
        return Ok(());
    }
    let entry = format!("- [{}]({name}.md)", title(name));
    let index = if index.contains(NO_TOPICS) {
        index.replacen(NO_TOPICS, &entry, 1)
    } else {
        format!("{}\n{entry}\n", index.trim_end())
    };
    tokio::fs::write(&path, index)
        .await
        .map_err(|source| ToolError::Write { path, source })
}

/// A page's title from its name: `nginx-sites` gives `Nginx sites`.
fn title(name: &str) -> String {
    let words = name.replace('-', " ");
    let mut chars = words.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => words,
    }
}

/// Fails when other accounts could change the file at `path`, or its folder.
pub async fn trusted(path: &Path) -> Result<(), ToolError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        use std::os::unix::fs::MetadataExt;

        // SAFETY: geteuid has no preconditions and cannot fail.
        let me = unsafe { libc::geteuid() };
        let folder = path.parent().map(Path::to_path_buf).unwrap_or_default();
        for checked in [folder, path.to_path_buf()] {
            if let Ok(metadata) = tokio::fs::metadata(&checked).await
                && (metadata.permissions().mode() & 0o022 != 0 || metadata.uid() != me)
            {
                return Err(invalid(format!(
                    "{} can be changed by other accounts, so Kelpy does not trust it: it must belong to Kelpy's \
                     account, and only its owner should be able to write it (chmod go-w)",
                    checked.display()
                )));
            }
        }
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// Makes a page Kelpy wrote readable and writable by its owner only.
async fn restrict(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).await;
    }
    #[cfg(not(unix))]
    let _ = path;
}

/// Writes Kelpy's own page, `server.md`.
pub async fn write_server_page(folder: &Path, content: &str) -> std::io::Result<PathBuf> {
    let path = folder.join(format!("{SERVER_PAGE}.md"));
    let untrusted = |why: String| std::io::Error::new(std::io::ErrorKind::PermissionDenied, why);
    trusted(&path)
        .await
        .map_err(|error| untrusted(error.to_string()))?;
    if tokio::fs::symlink_metadata(&path)
        .await
        .is_ok_and(|metadata| metadata.file_type().is_symlink())
    {
        return Err(untrusted(format!("{} is a link", path.display())));
    }
    tokio::fs::write(&path, content).await?;
    restrict(&path).await;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[tokio::test]
    async fn a_fact_goes_on_its_page_and_the_page_in_the_index() {
        let folder = tempfile::tempdir().unwrap();
        fs::write(
            folder.path().join(INDEX),
            "# Memory\n\n## Topics\n\n_None yet._\n",
        )
        .unwrap();
        let said = remember(
            folder.path(),
            "nginx-sites",
            "The blog is in /var/www/blog.",
            "2026-11-03",
        )
        .await
        .unwrap();
        assert_eq!(said, "Remembered on memory/nginx-sites.md.");
        remember(folder.path(), "nginx-sites", "Port 8080.", "2026-11-04")
            .await
            .unwrap();
        assert_eq!(
            read(folder.path(), "nginx-sites").await.unwrap(),
            "# Nginx sites\n\n- 2026-11-03: The blog is in /var/www/blog.\n- 2026-11-04: Port 8080.\n"
        );
        let index = fs::read_to_string(folder.path().join(INDEX)).unwrap();
        assert_eq!(
            index,
            "# Memory\n\n## Topics\n\n- [Nginx sites](nginx-sites.md)\n"
        );
        remember(folder.path(), "backups", "Nightly at 2:00.", "2026-11-03")
            .await
            .unwrap();
        assert!(
            fs::read_to_string(folder.path().join(INDEX))
                .unwrap()
                .ends_with("(nginx-sites.md)\n- [Backups](backups.md)\n")
        );
        assert!(
            remember(folder.path(), SERVER_PAGE, "x", "2026-11-03")
                .await
                .is_err()
        );
        assert!(read(folder.path(), "missing").await.is_err());
        assert!(page("../etc/passwd").is_err() && page("Nginx").is_err());
        assert_eq!(page("nginx.md").unwrap(), "nginx");
        assert_eq!(note("  two\n lines ").unwrap(), "two lines");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_page_others_can_change_is_not_trusted() {
        use std::os::unix::fs::PermissionsExt;

        let folder = tempfile::tempdir().unwrap();
        let page = folder.path().join("nginx.md");
        fs::write(&page, "# Nginx\n").unwrap();
        fs::set_permissions(&page, fs::Permissions::from_mode(0o666)).unwrap();
        let refused = read(folder.path(), "nginx").await.unwrap_err();
        assert!(refused.to_string().contains("other accounts"), "{refused}");
        fs::set_permissions(&page, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(read(folder.path(), "nginx").await.is_ok());
    }
}
