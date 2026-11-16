use crate::{parse_eml_bytes, Email, ParseOptions};
use anyhow::{ensure, Context, Result};
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;

pub(crate) fn parse(
    path: &Path,
    options: ParseOptions,
    callback: impl FnMut(Email) -> Result<()>,
    on_error: impl FnMut(usize, u64, anyhow::Error) -> Result<()>,
) -> Result<usize> {
    parse_reader(
        BufReader::new(File::open(path)?),
        options,
        callback,
        on_error,
    )
}

pub(crate) fn parse_reader(
    mut reader: impl BufRead,
    options: ParseOptions,
    mut callback: impl FnMut(Email) -> Result<()>,
    mut on_error: impl FnMut(usize, u64, anyhow::Error) -> Result<()>,
) -> Result<usize> {
    let mut dated_separators = false;
    let mut in_body = false;
    let mut line = Vec::new();
    let mut message = Vec::new();
    let (mut offset, mut start, mut index, mut count) = (0u64, 0u64, 0usize, 0usize);
    let mut deliver = |bytes: &[u8], index: usize, start: u64| -> Result<()> {
        let bytes = bytes
            .strip_suffix(b"\r\n")
            .or_else(|| bytes.strip_suffix(b"\n"))
            .unwrap_or(bytes);
        if bytes.is_empty() {
            return Ok(());
        }
        match parse_eml_bytes(bytes, options)
            .with_context(|| format!("mbox message {index} at byte offset {start}"))
        {
            Ok(email) => {
                callback(email)?;
                count += 1;
            }
            Err(error) => on_error(index, start, error)?,
        }
        Ok(())
    };
    loop {
        line.clear();
        let n = (&mut reader)
            .take(options.max_inflate.saturating_add(1) as u64)
            .read_until(b'\n', &mut line)?;
        if n == 0 {
            break;
        }
        ensure!(
            n <= options.max_inflate,
            "mbox line exceeds max_inflate at byte offset {offset}"
        );
        let separator = line.starts_with(b"From ")
            && (index == 0 || !dated_separators || crate::is_mbox_separator(&line));
        if separator {
            if index == 0 {
                dated_separators = crate::is_mbox_separator(&line);
            }
            if index > 0 {
                deliver(&message, index, start)?;
            }
            index += 1;
            start = offset;
            message.clear();
            in_body = false;
        } else if index > 0 {
            // mboxrd quotes every body line matching ^>*From ; remove exactly one >.
            let unquoted = if dated_separators && in_body {
                line.strip_prefix(b">")
                    .filter(|tail| {
                        tail.iter()
                            .skip_while(|&&b| b == b'>')
                            .copied()
                            .take(5)
                            .eq(b"From ".iter().copied())
                    })
                    .unwrap_or(&line)
            } else {
                &line
            };
            if line == b"\n" || line == b"\r\n" {
                in_body = true;
            }
            ensure!(
                message.len().saturating_add(unquoted.len()) <= options.max_inflate,
                "mbox message {index} exceeds max_inflate at byte offset {start}"
            );
            message.extend_from_slice(unquoted);
        }
        offset += n as u64;
    }
    if index > 0 {
        deliver(&message, index, start)?;
    }
    Ok(count)
}
