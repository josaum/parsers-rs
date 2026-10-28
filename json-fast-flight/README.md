# json-fast-flight

Arrow Flight server that streams JSON / JSONL files as Apache Arrow
`RecordBatch`es. Reader-only; auth and telemetry follow the shared
`flight-ipc-config` conventions used by `xlsx-fast-flight` / `docx-fast-flight`
/ `pptx-fast-flight` / `email-fast-flight` / `xsd-fast-flight`.

## Build

```bash
cargo build -p json-fast-flight
```

## Run

```bash
json-fast-flight --bind 0.0.0.0:50053 --root ./data --batch-size 16384
```

Auth is mandatory and env-driven via `flight-ipc-config`
(`FlightRequestAuthVerifier::from_env_required`).

## Protocol

- **Descriptor / ticket** — path relative to `--root` for a `.json` or
  `.jsonl` file, optionally `?batch_size=N`.
- **ListFlights / GetFlightInfo / GetSchema** — schema inferred per file by
  `json-fast-core`'s `arrow` feature.
- **DoGet** — streams `RecordBatch`es (Arrow IPC over gRPC) with per-stream
  telemetry observation.
- **DoAction / ListActions** — `parsers.flight.codec_capabilities.v1`
  (reader-only: no writer route).
- **DoPut / DoExchange** — unimplemented.

## Schema model

Union-of-keys inference over the file's records; nested values become UTF-8
JSON-string columns. Column types: `Boolean`, `Int64`, `Float64`, `Utf8`.
See `../json-fast-core/src/arrow.rs` for details.
