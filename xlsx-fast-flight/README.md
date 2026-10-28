# xlsx-fast-flight

Arrow Flight server for streaming XLSX cells as Arrow IPC.

## Run server

```
PARSERS_FLIGHT_REQUIRE_AUTH=true \
PARSERS_JWT_PUBLIC_KEY_PATH=/run/secrets/parsers-jwt-public.pem \
cargo run -p xlsx-fast-flight -- --root /path/to/xlsx --bind 0.0.0.0:50051
```

The process intentionally exits before binding if authentication is disabled
or signing material is unavailable. Retrieval RPCs require
`authorization: Bearer <JWT>` metadata. See the workspace
[Flight server authentication guide](../README.md#arrow-flight-servers) for
issuer/audience constraints and the temporary legacy-HS256 migration settings.
Never put a token or signing secret in the image or command committed to source.

Flags:
- `--root`: root directory to serve files from (required for path resolution)
- `--bind`: listen address (default `0.0.0.0:50051`)
- `--batch-size`: Arrow batch size (default `16384`)
- `--threads`: parallel sheet workers (default `0` = serial, `auto` = CPU count)
- `--no-dates`: disable date decoding
- `--include-formulas`: include formulas in output
- `--no-sheet-names`: omit sheet name column
- `--raw-values`: skip numeric/date parsing (emit raw strings)

## Client example

```
cargo run -p xlsx-fast-flight --example client -- http://127.0.0.1:50051 data/sample.xlsx
```

The example is a transport sketch; callers must wrap `GetFlightInfo` and
`DoGet` payloads in tonic requests carrying the bearer metadata required above.

## Query parameters

You can add query parameters to the descriptor or ticket path:
- `sheet=Sheet1` or `sheet=Sheet1,Sheet2` to filter by sheet name
- `range=A1:D20` to filter cells by inclusive A1 range
- `range=A:D` for column-only ranges, `range=1:100` for row-only ranges
- multiple ranges via `range=A1:B2,C5:D6`
- open-ended ranges: `range=A1:` or `range=:D20`
- `raw=true` to emit raw values (skip numeric/date parsing)
- `dates=false` to disable date decoding
- `formulas=true` to include formulas
- `sheet_names=false` to omit sheet name column
- `batch=65536` to override batch size
- `threads=4` to override parallel sheet workers

Example:
- `data/sample.xlsx?sheet=Sheet1`

## DoExchange behavior

`DoExchange` expects the first inbound `FlightData` to include either:
- `flight_descriptor` with a path (preferred), or
- `app_metadata` containing a UTF-8 path

Only the first message is used; any remaining inbound messages are drained.
