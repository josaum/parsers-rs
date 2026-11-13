/** Parse a JSON document into a JS value (SIMD fast path). */
export declare function parseJson(s: string): any;

/** Parse a JSONL document into an array of JS values (blank lines skipped). */
export declare function parseJsonl(s: string): any[];

/** Count non-blank lines in a JSONL document (fast streaming pass). */
export declare function countLines(s: string): number;

/** Return the six-type classification of a JSON document's root value. */
export declare function jsonType(s: string): 'string' | 'number' | 'boolean' | 'null' | 'array' | 'object';

export interface SchemaFinding {
  severity: 'error' | 'warning';
  path: string;
  message: string;
}

/**
 * Validate a JSON document against a JSON Schema; returns an array of findings.
 * `resources` maps a URI to the JSON text of another schema document a `$ref`
 * may name. Nothing is fetched: a reference that resolves nowhere is an error.
 */
export declare function validate(
  s: string,
  schema: string,
  resources?: Record<string, string>,
): SchemaFinding[];

/** Parse and validate a JSON-RPC 2.0 request envelope. */
export declare function jsonRpcRequest(s: string): any;

/** Parse and validate a JSON-RPC 2.0 response envelope. */
export declare function jsonRpcResponse(s: string): any;

/** Parse and validate a JSON:API document envelope. */
export declare function jsonApiDocument(s: string): any;

export interface JwtParts {
  header: any;
  payload: any;
  signature: string;
}

/** Split a compact JWT into `{header, payload, signature}`. */
export declare function jwtParts(s: string): JwtParts;

export interface JsonLdMeta {
  context?: any;
  graph?: any;
  type?: any;
}

/** Peek at a JSON-LD document's `@context` / `@graph` / `@type`. */
export declare function jsonLdMeta(s: string): JsonLdMeta;

/** Crate / binding version string. */
export declare function version(): string;
