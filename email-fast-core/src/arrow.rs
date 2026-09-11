//! Apache Arrow IPC output for email parsing
//!
//! This module provides functions to export email data to Apache Arrow format
//! for efficient data interchange with data processing systems.

use crate::{Email, EmailAddress, ParseOptions};
use anyhow::Result;
use arrow_array::{
    builder::{StringBuilder, TimestampMicrosecondBuilder, UInt32Builder},
    RecordBatch,
};
use arrow_ipc::writer::FileWriter;
use arrow_schema::{DataType, Field, Schema, TimeUnit};
use std::io::Write;
use std::path::Path;

/// Arrow export options
#[derive(Debug, Clone, Copy)]
pub struct ArrowOptions {
    /// Batch size for Arrow record batches
    pub batch_size: usize,
}

impl Default for ArrowOptions {
    fn default() -> Self {
        Self { batch_size: 10_000 }
    }
}

/// Convert email addresses to Arrow array
fn addresses_to_string(addresses: &[EmailAddress]) -> String {
    addresses
        .iter()
        .map(|a| {
            if let Some(name) = &a.name {
                format!("\"{}\" <{}>", name, a.address)
            } else {
                a.address.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Stable Arrow schema for email rows (Flight / IPC / in-memory ETL).
pub fn emails_schema() -> Schema {
    Schema::new(vec![
        Field::new("message_id", DataType::Utf8, true),
        Field::new("from_address", DataType::Utf8, false),
        Field::new("from_name", DataType::Utf8, true),
        Field::new("to_addresses", DataType::Utf8, false),
        Field::new("cc_addresses", DataType::Utf8, true),
        Field::new("subject", DataType::Utf8, true),
        Field::new(
            "date",
            DataType::Timestamp(TimeUnit::Microsecond, None),
            true,
        ),
        Field::new("body_text", DataType::Utf8, true),
        Field::new("body_html", DataType::Utf8, true),
        Field::new("has_attachments", DataType::Boolean, false),
        Field::new("attachment_count", DataType::UInt32, false),
        Field::new("in_reply_to", DataType::Utf8, true),
        Field::new("references", DataType::Utf8, true),
        Field::new("priority", DataType::UInt8, true),
    ])
}

/// Stable Arrow schema for attachment rows (one row per attachment).
pub fn attachments_schema() -> Schema {
    Schema::new(vec![
        Field::new("email_message_id", DataType::Utf8, true),
        Field::new("filename", DataType::Utf8, false),
        Field::new("content_type", DataType::Utf8, false),
        Field::new("size", DataType::UInt32, false),
        Field::new("content_id", DataType::Utf8, true),
        Field::new("is_inline", DataType::Boolean, false),
    ])
}

/// Materialize emails as a single [`RecordBatch`] without writing IPC.
pub fn emails_to_record_batch(emails: &[Email]) -> Result<RecordBatch> {
    let schema = emails_schema();
    let mut message_id_builder = StringBuilder::new();
    let mut from_address_builder = StringBuilder::new();
    let mut from_name_builder = StringBuilder::new();
    let mut to_addresses_builder = StringBuilder::new();
    let mut cc_addresses_builder = StringBuilder::new();
    let mut subject_builder = StringBuilder::new();
    let mut date_builder = TimestampMicrosecondBuilder::new();
    let mut body_text_builder = StringBuilder::new();
    let mut body_html_builder = StringBuilder::new();
    let mut has_attachments_builder = arrow_array::builder::BooleanBuilder::new();
    let mut attachment_count_builder = UInt32Builder::new();
    let mut in_reply_to_builder = StringBuilder::new();
    let mut references_builder = StringBuilder::new();
    let mut priority_builder = arrow_array::builder::UInt8Builder::new();

    for email in emails {
        message_id_builder.append_option(email.message_id.as_ref());
        from_address_builder.append_value(&email.from.address);
        from_name_builder.append_option(email.from.name.as_ref());
        to_addresses_builder.append_value(addresses_to_string(&email.to));
        cc_addresses_builder.append_value(addresses_to_string(&email.cc));
        subject_builder.append_option(email.subject.as_ref());

        if let Some(date) = email.date {
            date_builder.append_value(date.timestamp_micros());
        } else {
            date_builder.append_null();
        }

        body_text_builder.append_option(email.body.text.as_ref());
        body_html_builder.append_option(email.body.html.as_ref());
        has_attachments_builder.append_value(!email.attachments.is_empty());
        attachment_count_builder.append_value(email.attachments.len() as u32);
        in_reply_to_builder.append_option(email.thread_info.in_reply_to.as_ref());
        let refs = if email.thread_info.references.is_empty() {
            None
        } else {
            Some(email.thread_info.references.join(" "))
        };
        references_builder.append_option(refs);
        priority_builder.append_option(email.priority);
    }

    Ok(RecordBatch::try_new(
        std::sync::Arc::new(schema),
        vec![
            std::sync::Arc::new(message_id_builder.finish()),
            std::sync::Arc::new(from_address_builder.finish()),
            std::sync::Arc::new(from_name_builder.finish()),
            std::sync::Arc::new(to_addresses_builder.finish()),
            std::sync::Arc::new(cc_addresses_builder.finish()),
            std::sync::Arc::new(subject_builder.finish()),
            std::sync::Arc::new(date_builder.finish()),
            std::sync::Arc::new(body_text_builder.finish()),
            std::sync::Arc::new(body_html_builder.finish()),
            std::sync::Arc::new(has_attachments_builder.finish()),
            std::sync::Arc::new(attachment_count_builder.finish()),
            std::sync::Arc::new(in_reply_to_builder.finish()),
            std::sync::Arc::new(references_builder.finish()),
            std::sync::Arc::new(priority_builder.finish()),
        ],
    )?)
}

/// Export emails to Arrow IPC file
pub fn export_emails_to_arrow_ipc<W: Write>(emails: &[Email], writer: &mut W) -> Result<()> {
    let batch = emails_to_record_batch(emails)?;
    let mut writer = FileWriter::try_new(writer, &batch.schema())?;
    writer.write(&batch)?;
    writer.finish()?;
    Ok(())
}

/// Export attachments to Arrow IPC file
pub fn export_attachments_to_arrow_ipc<W: Write>(emails: &[Email], writer: &mut W) -> Result<()> {
    // Define schema
    let schema = attachments_schema();

    // Build arrays
    let mut email_message_id_builder = StringBuilder::new();
    let mut filename_builder = StringBuilder::new();
    let mut content_type_builder = StringBuilder::new();
    let mut size_builder = UInt32Builder::new();
    let mut content_id_builder = StringBuilder::new();
    let mut is_inline_builder = arrow_array::builder::BooleanBuilder::new();

    for email in emails {
        for attachment in &email.attachments {
            email_message_id_builder.append_option(email.message_id.as_ref());
            filename_builder.append_value(&attachment.filename);
            content_type_builder.append_value(&attachment.content_type);
            size_builder.append_value(attachment.size as u32);
            content_id_builder.append_option(attachment.content_id.as_ref());
            is_inline_builder.append_value(attachment.is_inline);
        }
    }

    // Create record batch
    let batch = RecordBatch::try_new(
        std::sync::Arc::new(schema),
        vec![
            std::sync::Arc::new(email_message_id_builder.finish()),
            std::sync::Arc::new(filename_builder.finish()),
            std::sync::Arc::new(content_type_builder.finish()),
            std::sync::Arc::new(size_builder.finish()),
            std::sync::Arc::new(content_id_builder.finish()),
            std::sync::Arc::new(is_inline_builder.finish()),
        ],
    )?;

    // Write to IPC file
    let mut writer = FileWriter::try_new(writer, &batch.schema())?;
    writer.write(&batch)?;
    writer.finish()?;

    Ok(())
}

/// Parse emails and export to Arrow IPC
pub fn parse_emails_to_arrow_ipc(
    paths: &[impl AsRef<Path>],
    out_path: impl AsRef<Path>,
    parse_options: ParseOptions,
    _arrow_options: ArrowOptions,
) -> Result<()> {
    let mut emails = Vec::new();

    for path in paths {
        let email = crate::parse_email(path, parse_options)?;
        emails.push(email);
    }

    let mut file = std::fs::File::create(out_path)?;
    export_emails_to_arrow_ipc(&emails, &mut file)?;

    Ok(())
}

/// Parse mbox and export to Arrow IPC
pub fn parse_mbox_to_arrow_ipc(
    path: impl AsRef<Path>,
    out_path: impl AsRef<Path>,
    parse_options: ParseOptions,
    arrow_options: ArrowOptions,
) -> Result<usize> {
    let mut emails = Vec::with_capacity(arrow_options.batch_size);
    let mut total_count = 0;
    let mut file_counter = 0;

    crate::parse_mbox(path, parse_options, |email| {
        emails.push(email);

        if emails.len() >= arrow_options.batch_size {
            let out_file = format!("{}_{:04}.arrow", out_path.as_ref().display(), file_counter);
            let mut file = std::fs::File::create(&out_file)?;
            export_emails_to_arrow_ipc(&emails, &mut file)?;
            file_counter += 1;
            total_count += emails.len();
            emails.clear();
        }

        Ok(())
    })?;

    // Write remaining emails
    if !emails.is_empty() {
        let out_file = if file_counter == 0 {
            out_path.as_ref().to_string_lossy().to_string()
        } else {
            format!("{}_{:04}.arrow", out_path.as_ref().display(), file_counter)
        };
        let mut file = std::fs::File::create(&out_file)?;
        export_emails_to_arrow_ipc(&emails, &mut file)?;
        total_count += emails.len();
    }

    Ok(total_count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{EmailAddress, EmailBody, ThreadInfo};

    fn sample_email() -> Email {
        Email {
            message_id: Some("<a@b>".into()),
            from: EmailAddress {
                name: Some("Ada".into()),
                address: "ada@example.com".into(),
            },
            reply_to: None,
            to: vec![EmailAddress {
                name: None,
                address: "bob@example.com".into(),
            }],
            cc: vec![],
            bcc: vec![],
            subject: Some("Hello".into()),
            date: None,
            body: EmailBody {
                text: Some("hi".into()),
                html: None,
            },
            attachments: vec![],
            thread_info: ThreadInfo::default(),
            priority: None,
            headers: None,
        }
    }

    #[test]
    fn empty_emails_batch_is_schema_stable() {
        // WHY: Flight handlers must accept empty pages without schema drift.
        let batch = emails_to_record_batch(&[]).expect("empty");
        assert_eq!(batch.num_rows(), 0);
        assert_eq!(
            batch.schema().fields().len(),
            emails_schema().fields().len()
        );
    }

    #[test]
    fn in_memory_batch_preserves_message_id() {
        // WHY: IPC write must not be required for ETL — RecordBatch is the SOTA path.
        let batch = emails_to_record_batch(&[sample_email()]).expect("batch");
        assert_eq!(batch.num_rows(), 1);
        let ids = batch
            .column(0)
            .as_any()
            .downcast_ref::<arrow_array::StringArray>()
            .expect("message_id");
        assert_eq!(ids.value(0), "<a@b>");
    }
}
