export declare function listElements(schemaPath: string, maxIncludes?: number): string[];
export declare function describeElementJson(schemaPath: string, element: string, maxDepth?: number, mapDates?: boolean, includeAttributes?: boolean): string;
export declare function validateJson(schemaPath: string, xmlPath: string, maxErrors?: number, checkFacets?: boolean, checkCardinality?: boolean): string;
export declare function validateBytesJson(schemaPath: string, xmlBytes: Buffer, maxErrors?: number, checkFacets?: boolean, checkCardinality?: boolean): string;
export declare function writeSchemaIpc(schemaPath: string, element: string, outPath: string, maxDepth?: number, mapDates?: boolean): void;
export declare function schemaIpcBytes(schemaPath: string, element: string, maxDepth?: number, mapDates?: boolean): Buffer;
export declare function allArrowSchemasJson(schemaPath: string, maxDepth?: number, mapDates?: boolean, includeAttributes?: boolean): string;
