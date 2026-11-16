use crate::{parse_eml_bytes, Email, ParseOptions};
use anyhow::{ensure, Context, Result};
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;

pub(crate) fn parse(
    path: &Path,
    options: ParseOptions,
    mut callback: impl FnMut(Email) -> Result<()>,
    mut on_error: impl FnMut(usize, u64, anyhow::Error) -> Result<()>,
) -> Result<usize> {
    let mut reader = BufReader::new(File::open(path)?);
    let mut line = Vec::new();
    let mut message = Vec::new();
    let (mut offset, mut start, mut index, mut count) = (0u64, 0u64, 0usize, 0usize);
    let mut deliver = |bytes: &[u8], index: usize, start: u64| -> Result<()> {
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
        if line.starts_with(b"From ") {
            if index > 0 {
                deliver(&message, index, start)?;
            }
            index += 1;
            start = offset;
            message.clear();
        } else if index > 0 {
            // mboxrd quotes every body line matching ^>*From ; remove exactly one >.
            let unquoted = line
                .strip_prefix(b">")
                .filter(|tail| {
                    tail.iter()
                        .skip_while(|&&b| b == b'>')
                        .copied()
                        .take(5)
                        .eq(b"From ".iter().copied())
                })
                .unwrap_or(&line);
            ensure!(
                message.len().saturating_add(unquoted.len()) <= options.max_inflate,
                "mbox message {index} exceeds max_inflate at byte offset {start}"
            );
            message.extend_from_slice(unquoted);
        } else {
            ensure!(
                line.iter().all(u8::is_ascii_whitespace),
                "mbox missing From separator at byte offset {offset}"
            );
        }
        offset += n as u64;
    }
    if index > 0 {
        deliver(&message, index, start)?;
    }
    Ok(count)
}
