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
    let mut next_suffix = std::collections::HashMap::<String, u64>::new();
    let mut sink = |attachment: &Attachment, reader: &mut dyn Read, limit: usize| {
        let name = attachment
            .filename
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or("attachment.bin");
        let name = crate::sanitize_filename_component(name);
        let name = name.trim_matches(['.', ' ']);
        let name = if name.is_empty() {
            "attachment.bin"
        } else {
            name
        };
        let mut suffix = *next_suffix.get(name).unwrap_or(&0);
        let (path, mut file) = loop {
            let path = directory.join(if suffix == 0 {
                name.to_owned()
            } else {
                format!("{name}.{suffix}")
            });
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(file) => break (path, file),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => suffix += 1,
                Err(error) => return Err(error.into()),
            }
        };
        next_suffix.insert(name.to_owned(), suffix + 1);
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
