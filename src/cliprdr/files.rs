//! File clipboard metadata, URI lists, and bounded local file enumeration.

use std::path::{Component, Path, PathBuf};

use ironrdp_cliprdr::pdu::{ClipboardFileAttributes, FileDescriptor};

pub const URI_LIST: &str = "text/uri-list";
pub const GNOME_COPIED_FILES: &str = "x-special/gnome-copied-files";
pub const MAX_FILES: usize = 4096;
pub const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Canonical identity of a file list: name, relative path and size per file.
/// Used to recognize rdpclip-style re-announcements of unchanged content.
pub type FileListFingerprint = Vec<(Option<String>, String, Option<u64>)>;

pub fn fingerprint(files: &[FileDescriptor]) -> FileListFingerprint {
    files
        .iter()
        .map(|file| {
            (
                file.relative_path.clone(),
                file.name.clone(),
                file.file_size,
            )
        })
        .collect()
}

pub fn same_fingerprint(a: &FileListFingerprint, b: &[FileDescriptor]) -> bool {
    fingerprint(b) == *a
}

pub fn descriptor(path: &Path, relative_path: Option<String>) -> Result<FileDescriptor, String> {
    let metadata = std::fs::symlink_metadata(path).map_err(|error| error.to_string())?;
    if metadata.file_type().is_symlink() {
        return Err("symbolic links are not supported in clipboard file lists".into());
    }
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or("file name is not valid UTF-8")?;
    let mut attributes = if metadata.permissions().readonly() {
        ClipboardFileAttributes::READONLY
    } else {
        ClipboardFileAttributes::ARCHIVE
    };
    if metadata.is_dir() {
        attributes |= ClipboardFileAttributes::DIRECTORY;
    }
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs().saturating_add(11_644_473_600) * 10_000_000)
        .unwrap_or(0);
    let mut descriptor = FileDescriptor::new(name)
        .with_attributes(attributes)
        .with_last_write_time(modified);
    if !metadata.is_dir() {
        descriptor = descriptor.with_file_size(metadata.len());
    }
    if let Some(relative_path) = relative_path {
        descriptor = descriptor.with_relative_path(relative_path);
    }
    Ok(descriptor)
}

/// Returns descriptors and their same-indexed local paths. Directories are
/// recursively enumerated; index zero is the first copied root.
pub fn local_files(roots: &[PathBuf]) -> Result<(Vec<FileDescriptor>, Vec<PathBuf>), String> {
    fn visit(
        path: &Path,
        relative: Option<String>,
        descriptors: &mut Vec<FileDescriptor>,
        paths: &mut Vec<PathBuf>,
    ) -> Result<(), String> {
        if descriptors.len() >= MAX_FILES {
            return Err(format!("clipboard contains more than {MAX_FILES} files"));
        }
        let item = descriptor(path, relative.clone())?;
        let directory = item
            .attributes
            .is_some_and(|attributes| attributes.contains(ClipboardFileAttributes::DIRECTORY));
        let child_parent = match &relative {
            Some(parent) => format!("{parent}\\{}", item.name),
            None => item.name.clone(),
        };
        descriptors.push(item);
        paths.push(path.to_owned());
        if directory {
            let mut children = std::fs::read_dir(path)
                .map_err(|error| error.to_string())?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| error.to_string())?;
            children.sort_by_key(|entry| entry.file_name());
            for child in children {
                visit(
                    &child.path(),
                    Some(child_parent.clone()),
                    descriptors,
                    paths,
                )?;
            }
        }
        Ok(())
    }

    let mut descriptors = Vec::new();
    let mut paths = Vec::new();
    for root in roots {
        visit(root, None, &mut descriptors, &mut paths)?;
    }
    Ok((descriptors, paths))
}

/// Converts a sanitized RDP descriptor to a path beneath `root`.
pub fn staged_path(root: &Path, descriptor: &FileDescriptor) -> Result<PathBuf, String> {
    let wire = match &descriptor.relative_path {
        Some(parent) if !parent.is_empty() => format!("{parent}\\{}", descriptor.name),
        _ => descriptor.name.clone(),
    };
    let mut path = root.to_owned();
    for component in Path::new(&wire.replace('\\', "/")).components() {
        match component {
            Component::Normal(part) => path.push(part),
            _ => return Err("unsafe path in remote file list".into()),
        }
    }
    if path == root {
        return Err("empty path in remote file list".into());
    }
    Ok(path)
}

pub fn read_range(
    path: &Path,
    position: u64,
    requested: u32,
    limit: u64,
) -> Result<Vec<u8>, String> {
    use std::io::{Read as _, Seek as _};

    let metadata = std::fs::metadata(path).map_err(|error| error.to_string())?;
    if !metadata.is_file() || requested == 0 {
        return Err("invalid clipboard file range request".into());
    }
    let remaining = metadata
        .len()
        .checked_sub(position)
        .ok_or("clipboard file range starts beyond the end of the file")?;
    let length = u64::from(requested).min(remaining).min(limit);
    if length == 0 {
        return Err("clipboard file range is outside the file".into());
    }
    let mut file = std::fs::File::open(path).map_err(|error| error.to_string())?;
    file.seek(std::io::SeekFrom::Start(position))
        .map_err(|error| error.to_string())?;
    let mut data = vec![0_u8; length as usize];
    file.read_exact(&mut data)
        .map_err(|error| error.to_string())?;
    Ok(data)
}

pub fn encode_uri_list(paths: &[PathBuf]) -> Vec<u8> {
    let mut output = String::new();
    for path in paths {
        output.push_str("file://");
        for byte in path.as_os_str().as_encoded_bytes() {
            if byte.is_ascii_alphanumeric() || matches!(*byte, b'/' | b'.' | b'-' | b'_' | b'~') {
                output.push(*byte as char);
            } else {
                output.push_str(&format!("%{byte:02X}"));
            }
        }
        // LF only: paste consumers are GTK/GNOME apps that split
        // `x-special/gnome-copied-files` and `text/uri-list` on `\n`; a
        // CRLF terminator leaves a trailing `\r` inside the decoded path,
        // which makes the file manager stat() a nonexistent name.
        output.push('\n');
    }
    output.into_bytes()
}

pub fn decode_uri_list(data: &[u8]) -> Result<Vec<PathBuf>, String> {
    let text = std::str::from_utf8(data).map_err(|error| error.to_string())?;
    let mut paths = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty()
            || line.starts_with('#')
            || (index == 0 && matches!(line, "copy" | "cut"))
        {
            continue;
        }
        let encoded = line
            .strip_prefix("file://")
            .ok_or("unsupported URI in copied file list")?;
        let encoded = encoded.strip_prefix("localhost").unwrap_or(encoded);
        let mut decoded = Vec::new();
        let mut bytes = encoded.as_bytes();
        while let Some((&byte, rest)) = bytes.split_first() {
            if byte == b'%' {
                if rest.len() < 2 {
                    return Err("invalid URI escape".into());
                }
                let hex = std::str::from_utf8(&rest[..2]).map_err(|_| "invalid URI escape")?;
                decoded.push(u8::from_str_radix(hex, 16).map_err(|_| "invalid URI escape")?);
                bytes = &rest[2..];
            } else {
                decoded.push(byte);
                bytes = rest;
            }
        }
        let path = String::from_utf8(decoded).map_err(|_| "file URI is not UTF-8")?;
        if !path.starts_with('/') {
            return Err("file URI does not contain an absolute local path".into());
        }
        paths.push(PathBuf::from(path));
    }
    if paths.is_empty() {
        return Err("copied file list is empty".into());
    }
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_list_round_trip() {
        let paths = vec![
            PathBuf::from("/tmp/a file.txt"),
            PathBuf::from("/home/user/文档.pdf"),
        ];
        assert_eq!(decode_uri_list(&encode_uri_list(&paths)).unwrap(), paths);
        assert!(decode_uri_list(b"copy\nhttps://example.com/file\r\n").is_err());
    }

    #[test]
    fn recursive_indices_and_ranges_match_local_files() {
        let root = std::env::temp_dir().join(format!("rstsc-files-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let copied = root.join("copied");
        std::fs::create_dir_all(copied.join("nested")).unwrap();
        std::fs::write(copied.join("a.txt"), b"abcdef").unwrap();
        std::fs::write(copied.join("nested/b.txt"), b"nested").unwrap();

        let (descriptors, paths) = local_files(&[copied.clone()]).unwrap();
        assert_eq!(descriptors.len(), 4);
        assert_eq!(paths[1], copied.join("a.txt"));
        assert_eq!(paths[3], copied.join("nested/b.txt"));
        assert_eq!(descriptors[1].relative_path.as_deref(), Some("copied"));
        assert_eq!(
            descriptors[3].relative_path.as_deref(),
            Some("copied\\nested")
        );
        assert_eq!(read_range(&paths[1], 2, 3, 1024).unwrap(), b"cde");
        assert_eq!(read_range(&paths[1], 4, 100, 1024).unwrap(), b"ef");
        assert_eq!(read_range(&paths[1], 0, 100, 2).unwrap(), b"ab");
        assert!(read_range(&paths[1], 6, 1, 1024).is_err());
        assert!(read_range(&paths[0], 0, 1, 1024).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn staged_paths_stay_beneath_root() {
        let root = Path::new("/tmp/stage");
        let file = FileDescriptor::new("file.txt").with_relative_path("dir");
        assert_eq!(staged_path(root, &file).unwrap(), root.join("dir/file.txt"));
        assert!(staged_path(root, &FileDescriptor::new("../escape")).is_err());
        assert!(staged_path(root, &FileDescriptor::new("/etc/passwd")).is_err());
    }
}
