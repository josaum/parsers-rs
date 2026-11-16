use crate::{mime, Attachment, EmailFormat, ParseOptions};
use anyhow::{Context, Result};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

pub(crate) fn extract(
    path: &Path,
    directory: &Path,
    mut options: ParseOptions,
) -> Result<Vec<PathBuf>> {
    fs::create_dir_all(directory)?;
    let bytes = crate::read_bounded(path, options.max_inflate)?;
    let mut paths = Vec::new();
    let mut used = std::collections::HashSet::new();
    let mut sink = |attachment: &Attachment, reader: &mut dyn Read, limit: usize| {
        let base = crate::attachment_file_name(&attachment.filename, paths.len());
        let (path, mut file) = loop {
            let name = crate::unique_file_name(base.clone(), &mut used);
            let path = directory.join(name);
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(file) => break (path, file),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.into()),
            }
        };
        let result = mime::copy_limited(reader, &mut file, limit).and_then(|size| {
            file.flush()?;
            Ok(size)
        });
        if result.is_err() {
            drop(file);
            let _ = fs::remove_file(&path);
        }
        let size = result.with_context(|| format!("extract {}", attachment.filename))?;
        paths.push(path);
        Ok(size)
    };
    options.extract_attachment_content = false;
    match crate::detect_format(path)? {
        EmailFormat::Eml => {
            crate::parse_eml_with_sink(&bytes, options, Some(&mut sink))?;
        }
        EmailFormat::Msg => {
            crate::msg::parse_with_sink(&bytes, options, Some(&mut sink))?;
        }
        EmailFormat::Mbox => {
            anyhow::bail!("extract attachments from individual messages, not an mbox")
        }
    }
    Ok(paths)
}
