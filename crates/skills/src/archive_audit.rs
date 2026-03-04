use std::{
    io::Write,
    path::{Component, Path, PathBuf},
};

/// Maximum downloaded archive size (compressed), in bytes.
pub const MAX_ARCHIVE_DOWNLOAD_BYTES: usize = 20 * 1024 * 1024; // 20 MB
/// Maximum number of entries in a tar archive.
pub const MAX_ARCHIVE_ENTRIES: usize = 1000;
/// Maximum single entry size (uncompressed), in bytes.
pub const MAX_ARCHIVE_SINGLE_ENTRY_BYTES: u64 = 10 * 1024 * 1024; // 10 MB
/// Maximum total archive payload size (uncompressed), in bytes.
pub const MAX_ARCHIVE_TOTAL_BYTES: u64 = 50 * 1024 * 1024; // 50 MB
/// Maximum accepted SKILL markdown size, in bytes.
pub const MAX_SKILL_MARKDOWN_BYTES: usize = 64 * 1024; // 64 KB

const BLOCKED_BINARY_EXTENSIONS: &[&str] = &[
    "exe",
    "dll",
    "so",
    "dylib",
    "msi",
    "dmg",
    "iso",
    "pkg",
    "deb",
    "rpm",
    "apk",
    "appimage",
    "bin",
];

pub fn enforce_download_size_hint(content_length: Option<u64>) -> anyhow::Result<()> {
    let Some(size) = content_length else {
        return Ok(());
    };
    if size > MAX_ARCHIVE_DOWNLOAD_BYTES as u64 {
        anyhow::bail!(
            "archive exceeds download size limit ({} bytes > {} bytes)",
            size,
            MAX_ARCHIVE_DOWNLOAD_BYTES
        );
    }
    Ok(())
}

pub fn enforce_download_size_bytes(size: usize) -> anyhow::Result<()> {
    if size > MAX_ARCHIVE_DOWNLOAD_BYTES {
        anyhow::bail!(
            "archive exceeds download size limit ({} bytes > {} bytes)",
            size,
            MAX_ARCHIVE_DOWNLOAD_BYTES
        );
    }
    Ok(())
}

pub fn enforce_skill_markdown_size(path: &Path, markdown: &str) -> anyhow::Result<()> {
    let size = markdown.len();
    if size > MAX_SKILL_MARKDOWN_BYTES {
        anyhow::bail!(
            "skill markdown too large at {} ({} bytes > {} bytes)",
            path.display(),
            size,
            MAX_SKILL_MARKDOWN_BYTES
        );
    }
    Ok(())
}

pub fn audit_archive_bytes(bytes: &[u8]) -> anyhow::Result<()> {
    let decoder = flate2::read::GzDecoder::new(bytes);
    let mut archive = tar::Archive::new(decoder);
    let mut entry_count = 0usize;
    let mut total_uncompressed_bytes = 0u64;

    for entry in archive.entries()? {
        let entry = entry?;
        entry_count += 1;
        if entry_count > MAX_ARCHIVE_ENTRIES {
            anyhow::bail!(
                "archive has too many entries ({} > {})",
                entry_count,
                MAX_ARCHIVE_ENTRIES
            );
        }

        let size = entry.header().size().unwrap_or_default();
        if size > MAX_ARCHIVE_SINGLE_ENTRY_BYTES {
            anyhow::bail!(
                "archive entry exceeds size budget ({} bytes > {} bytes)",
                size,
                MAX_ARCHIVE_SINGLE_ENTRY_BYTES
            );
        }
        total_uncompressed_bytes = total_uncompressed_bytes
            .checked_add(size)
            .ok_or_else(|| anyhow::anyhow!("archive uncompressed size overflow"))?;
        if total_uncompressed_bytes > MAX_ARCHIVE_TOTAL_BYTES {
            anyhow::bail!(
                "archive total size exceeds budget ({} bytes > {} bytes)",
                total_uncompressed_bytes,
                MAX_ARCHIVE_TOTAL_BYTES
            );
        }

        let ty = entry.header().entry_type();
        if ty.is_symlink() || ty.is_hard_link() {
            anyhow::bail!("archive contains symlink/hardlink entries");
        }
        if !ty.is_file() && !ty.is_dir() {
            anyhow::bail!("archive contains unsupported entry type");
        }

        let path = entry.path()?.into_owned();
        let Some(stripped) = sanitize_archive_path(&path)? else {
            continue;
        };
        if has_blocked_binary_extension(&stripped) {
            anyhow::bail!(
                "archive contains blocked native binary payload: {}",
                stripped.display()
            );
        }
    }

    Ok(())
}

fn has_blocked_binary_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_ascii_lowercase())
        .map(|ext| BLOCKED_BINARY_EXTENSIONS.contains(&ext.as_str()))
        .unwrap_or(false)
}

fn sanitize_archive_path(path: &Path) -> anyhow::Result<Option<PathBuf>> {
    let stripped: PathBuf = path.components().skip(1).collect();
    if stripped.as_os_str().is_empty() {
        return Ok(None);
    }

    for component in stripped.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {},
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                anyhow::bail!("archive contains unsafe path component: {}", path.display());
            },
        }
    }

    Ok(Some(stripped))
}

#[allow(clippy::expect_used, clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;

    fn build_archive(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut tar_bytes = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar_bytes);
            for (path, bytes) in files {
                let mut header = tar::Header::new_gnu();
                header.set_size(bytes.len() as u64);
                header.set_mode(0o644);
                header.set_cksum();
                builder.append_data(&mut header, *path, *bytes).unwrap();
            }
            builder.finish().unwrap();
        }

        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(&tar_bytes).unwrap();
        enc.finish().unwrap()
    }

    fn build_archive_with_symlink() -> Vec<u8> {
        let mut tar_bytes = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar_bytes);
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(tar::EntryType::Symlink);
            header.set_size(0);
            header.set_mode(0o777);
            header.set_cksum();
            builder
                .append_link(&mut header, "repo-root/bad-link", "target")
                .unwrap();
            builder.finish().unwrap();
        }
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(&tar_bytes).unwrap();
        enc.finish().unwrap()
    }

    #[test]
    fn rejects_oversized_archive_download_hint() {
        assert!(enforce_download_size_hint(Some(
            MAX_ARCHIVE_DOWNLOAD_BYTES as u64 + 1
        ))
        .is_err());
    }

    #[test]
    fn rejects_oversized_archive_download_bytes() {
        assert!(enforce_download_size_bytes(MAX_ARCHIVE_DOWNLOAD_BYTES + 1).is_err());
    }

    #[test]
    fn rejects_archive_with_oversized_entry() {
        let large = vec![b'a'; MAX_ARCHIVE_SINGLE_ENTRY_BYTES as usize + 1];
        let archive = build_archive(&[("repo-root/skills/huge/SKILL.md", &large)]);
        assert!(audit_archive_bytes(&archive).is_err());
    }

    #[test]
    fn rejects_archive_with_too_many_entries() {
        let files: Vec<(String, Vec<u8>)> = (0..=MAX_ARCHIVE_ENTRIES)
            .map(|i| {
                (
                    format!("repo-root/skills/s{i}/SKILL.md"),
                    b"---\nname: t\ndescription: t\n---\nbody\n".to_vec(),
                )
            })
            .collect();
        let refs: Vec<(&str, &[u8])> = files
            .iter()
            .map(|(p, b)| (p.as_str(), b.as_slice()))
            .collect();
        let archive = build_archive(&refs);
        assert!(audit_archive_bytes(&archive).is_err());
    }

    #[test]
    fn rejects_archive_with_blocked_binary_payload() {
        let archive = build_archive(&[("repo-root/skills/demo/payload.exe", b"MZ")]);
        assert!(audit_archive_bytes(&archive).is_err());
    }

    #[test]
    fn rejects_archive_with_symlink_entry() {
        let archive = build_archive_with_symlink();
        assert!(audit_archive_bytes(&archive).is_err());
    }

    #[test]
    fn accepts_valid_small_archive() {
        let archive = build_archive(&[(
            "repo-root/skills/demo/SKILL.md",
            b"---\nname: demo\ndescription: demo\n---\nBody\n",
        )]);
        assert!(audit_archive_bytes(&archive).is_ok());
    }

    #[test]
    fn rejects_oversized_skill_markdown() {
        let large = "a".repeat(MAX_SKILL_MARKDOWN_BYTES + 1);
        assert!(enforce_skill_markdown_size(Path::new("SKILL.md"), &large).is_err());
    }
}
