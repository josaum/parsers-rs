const native = require('./email_fast_node.node');

function parseEmail(path, options = undefined) {
  let opts = undefined;
  if (options) {
    opts = {
      maxInflate: options.maxInflate ?? options.max_inflate,
      maxMimeDepth: options.maxMimeDepth ?? options.max_mime_depth,
      maxMimeParts: options.maxMimeParts ?? options.max_mime_parts,
      maxHeaderBytes: options.maxHeaderBytes ?? options.max_header_bytes,
      maxTotalAttachmentBytes: options.maxTotalAttachmentBytes ?? options.max_total_attachment_bytes,
      maxAttachmentSize: options.maxAttachmentSize ?? options.max_attachment_size,
      includeRawHeaders: options.includeRawHeaders ?? options.include_raw_headers,
      htmlToText: options.htmlToText ?? options.html_to_text,
      extractAttachmentContent: options.extractAttachmentContent ?? options.extract_attachment_content,
    };
  }
  return native.parseEmailJs(path, opts);
}

function parseEml(path, options = undefined) {
  let opts = undefined;
  if (options) {
    opts = {
      maxInflate: options.maxInflate ?? options.max_inflate,
      maxMimeDepth: options.maxMimeDepth ?? options.max_mime_depth,
      maxMimeParts: options.maxMimeParts ?? options.max_mime_parts,
      maxHeaderBytes: options.maxHeaderBytes ?? options.max_header_bytes,
      maxTotalAttachmentBytes: options.maxTotalAttachmentBytes ?? options.max_total_attachment_bytes,
      maxAttachmentSize: options.maxAttachmentSize ?? options.max_attachment_size,
      includeRawHeaders: options.includeRawHeaders ?? options.include_raw_headers,
      htmlToText: options.htmlToText ?? options.html_to_text,
      extractAttachmentContent: options.extractAttachmentContent ?? options.extract_attachment_content,
    };
  }
  return native.parseEmlJs(path, opts);
}

function parseEmailNdjson(path, outPath, options = undefined) {
  let opts = undefined;
  if (options) {
    opts = {
      maxInflate: options.maxInflate ?? options.max_inflate,
      maxMimeDepth: options.maxMimeDepth ?? options.max_mime_depth,
      maxMimeParts: options.maxMimeParts ?? options.max_mime_parts,
      maxHeaderBytes: options.maxHeaderBytes ?? options.max_header_bytes,
      maxTotalAttachmentBytes: options.maxTotalAttachmentBytes ?? options.max_total_attachment_bytes,
      maxAttachmentSize: options.maxAttachmentSize ?? options.max_attachment_size,
      includeRawHeaders: options.includeRawHeaders ?? options.include_raw_headers,
      htmlToText: options.htmlToText ?? options.html_to_text,
      extractAttachmentContent: options.extractAttachmentContent ?? options.extract_attachment_content,
    };
  }
  return native.parseEmailNdjsonJs(path, outPath, opts);
}

function parseMboxNdjson(path, outPath, options = undefined) {
  let opts = undefined;
  if (options) {
    opts = {
      maxInflate: options.maxInflate ?? options.max_inflate,
      maxMimeDepth: options.maxMimeDepth ?? options.max_mime_depth,
      maxMimeParts: options.maxMimeParts ?? options.max_mime_parts,
      maxHeaderBytes: options.maxHeaderBytes ?? options.max_header_bytes,
      maxTotalAttachmentBytes: options.maxTotalAttachmentBytes ?? options.max_total_attachment_bytes,
      maxAttachmentSize: options.maxAttachmentSize ?? options.max_attachment_size,
      includeRawHeaders: options.includeRawHeaders ?? options.include_raw_headers,
      htmlToText: options.htmlToText ?? options.html_to_text,
      extractAttachmentContent: options.extractAttachmentContent ?? options.extract_attachment_content,
    };
  }
  return native.parseMboxNdjsonJs(path, outPath, opts);
}

function parseMboxArrow(path, outPath, options = undefined) {
  let opts = undefined;
  let arrowOpts = undefined;
  if (options) {
    opts = {
      maxInflate: options.maxInflate ?? options.max_inflate,
      maxMimeDepth: options.maxMimeDepth ?? options.max_mime_depth,
      maxMimeParts: options.maxMimeParts ?? options.max_mime_parts,
      maxHeaderBytes: options.maxHeaderBytes ?? options.max_header_bytes,
      maxTotalAttachmentBytes: options.maxTotalAttachmentBytes ?? options.max_total_attachment_bytes,
      maxAttachmentSize: options.maxAttachmentSize ?? options.max_attachment_size,
      includeRawHeaders: options.includeRawHeaders ?? options.include_raw_headers,
      htmlToText: options.htmlToText ?? options.html_to_text,
      extractAttachmentContent: options.extractAttachmentContent ?? options.extract_attachment_content,
    };
    arrowOpts = {
      batchSize: options.batchSize ?? options.batch_size,
    };
  }
  return native.parseMboxArrowJs(path, outPath, opts, arrowOpts);
}

function extractAttachments(path, outDir, maxAttachmentSize = undefined, options = undefined) {
  if (typeof maxAttachmentSize === 'object' && maxAttachmentSize !== null) {
    options = maxAttachmentSize;
    maxAttachmentSize = options.maxAttachmentSize ?? options.max_attachment_size;
  }
  const opts = options && {
    maxInflate: options.maxInflate ?? options.max_inflate,
    maxMimeDepth: options.maxMimeDepth ?? options.max_mime_depth,
    maxMimeParts: options.maxMimeParts ?? options.max_mime_parts,
    maxHeaderBytes: options.maxHeaderBytes ?? options.max_header_bytes,
    maxTotalAttachmentBytes: options.maxTotalAttachmentBytes ?? options.max_total_attachment_bytes,
  };
  return native.extractAttachmentsJs(path, outDir, maxAttachmentSize, opts);
}

module.exports = {
  parseEmail,
  parseEml,
  parseEmailNdjson,
  parseMboxNdjson,
  parseMboxArrow,
  extractAttachments,
};
