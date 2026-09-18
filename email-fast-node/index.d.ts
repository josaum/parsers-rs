/**
 * email-fast-node - High-performance email parser
 * 
 * @example
 * ```typescript
 * import { parseEmail, extractAttachments } from 'email-fast-node';
 * 
 * const email = parseEmail('/path/to/message.eml');
 * console.log(email.from.address);
 * console.log(email.subject);
 * 
 * // Extract attachments
 * const paths = extractAttachments('/path/to/message.eml', './attachments');
 * ```
 */

/**
 * Options for parsing email files
 */
export interface ParseOptions {
  /** Max attachment size to process in bytes (default: 104857600 = 100 MB) */
  maxAttachmentSize?: number;
  /** Max attachment size (snake_case alias) */
  max_attachment_size?: number;
  /** Include all raw headers in output (default: false) */
  includeRawHeaders?: boolean;
  /** Include raw headers (snake_case alias) */
  include_raw_headers?: boolean;
  /** Convert HTML body to plain text (default: false) */
  htmlToText?: boolean;
  /** HTML to text (snake_case alias) */
  html_to_text?: boolean;
  /** Include attachment content as base64 (default: false) */
  extractAttachmentContent?: boolean;
  /** Extract attachment content (snake_case alias) */
  extract_attachment_content?: boolean;
  /** Batch size for Arrow output (default: 10000) */
  batchSize?: number;
  /** Batch size (snake_case alias) */
  batch_size?: number;
}

/**
 * Email address with optional display name
 */
export interface EmailAddress {
  /** Display name (e.g., "John Doe") */
  name?: string;
  /** Email address (e.g., "john@example.com") */
  address: string;
}

/**
 * Email attachment
 */
export interface Attachment {
  filename: string;
  content_type: string;
  size: number;
  content_id?: string;
  is_inline: boolean;
  content?: string;  // base64 encoded if extractAttachmentContent is true
}

/**
 * Email body content
 */
export interface EmailBody {
  text?: string;
  html?: string;
}

/**
 * Threading information
 */
export interface ThreadInfo {
  message_id?: string;
  in_reply_to?: string;
  references: string[];
}

/**
 * Complete email message
 */
export interface Email {
  message_id?: string;
  from: EmailAddress;
  reply_to?: EmailAddress;
  to: EmailAddress[];
  cc: EmailAddress[];
  bcc: EmailAddress[];
  subject?: string;
  date?: string;  // ISO 8601 format
  body: EmailBody;
  attachments: Attachment[];
  in_reply_to?: string;
  references: string[];
  priority?: number;
  headers?: Record<string, string[]>;
}

/**
 * Parse an email file (auto-detects .eml or .msg format)
 * 
 * @param path - Path to the email file
 * @param options - Parsing options
 * @returns The parsed email
 * @throws Error if the file cannot be parsed
 */
export function parseEmail(path: string, options?: ParseOptions): Email;

/**
 * Parse an EML file
 * 
 * @param path - Path to the .eml file
 * @param options - Parsing options
 * @returns The parsed email
 * @throws Error if the file cannot be parsed
 */
export function parseEml(path: string, options?: ParseOptions): Email;

/**
 * Parse an email file to NDJSON
 * 
 * @param path - Path to the email file
 * @param outPath - Path for the output .ndjson file
 * @param options - Parsing options
 * @throws Error if the file cannot be parsed or written
 */
export function parseEmailNdjson(path: string, outPath: string, options?: ParseOptions): void;

/**
 * Parse an mbox file to NDJSON
 * 
 * @param path - Path to the .mbox file
 * @param outPath - Path for the output .ndjson file
 * @param options - Parsing options
 * @returns Number of emails processed
 * @throws Error if the file cannot be parsed or written
 */
export function parseMboxNdjson(path: string, outPath: string, options?: ParseOptions): number;

/**
 * Parse an mbox file to Apache Arrow IPC format
 * 
 * @param path - Path to the .mbox file
 * @param outPath - Path for the output .arrow file(s)
 * @param options - Parsing options
 * @returns Number of emails processed
 * @throws Error if the file cannot be parsed or written
 */
export function parseMboxArrow(path: string, outPath: string, options?: ParseOptions): number;

/**
 * Extract attachments from an email file
 * 
 * @param path - Path to the email file
 * @param outDir - Output directory for extracted attachments
 * @returns Array of paths to extracted files
 * @throws Error if the file cannot be parsed or attachments cannot be extracted
 */
export function extractAttachments(path: string, outDir: string, maxAttachmentSize?: number): string[];
