//! Dependency-light, none-only policy for public office parser Flight servers.

pub mod auth;
pub mod table_server;
pub use auth::{
    mint_es256_workload_authorization, mint_local_workload_authorization, AuthClaim, AuthConfig,
    AuthError, FlightAuthorizationMetadata, FlightRequestAuthVerifier,
};

use std::{
    collections::BTreeMap,
    fmt,
    path::{Path, PathBuf},
    pin::Pin,
    time::{Duration, Instant},
};

use arrow_flight::{FlightDescriptor, Ticket};
use arrow_ipc::{writer::IpcWriteOptions, CompressionType};
use futures::{Stream, StreamExt};
use prost::Message;
use serde::{de, Deserializer as _};
use thiserror::Error;
use tonic::Status;

pub const CAPABILITY_ACTION: &str = "parsers.flight.codec_capabilities.v1";
pub const CAPABILITY_ARROW_VERSION: &str = "58.4.1";
pub const CAPABILITY_RECEIVER_PROFILE: &str = "unnegotiated";
pub const CAPABILITY_BUILD_SHA: &str = "unknown";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlightIpcDirection {
    Read,
    Write,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlightRpcAction {
    DoGet,
    DoPut,
    DoExchange,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlightIpcOutcome {
    Ok,
    EncodeError,
    DecodeError,
    ResourceExhausted,
}

impl FlightIpcOutcome {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::EncodeError => "encode_error",
            Self::DecodeError => "decode_error",
            Self::ResourceExhausted => "resource_exhausted",
        }
    }
}

impl FlightIpcDirection {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
        }
    }
}

impl FlightRpcAction {
    const fn as_str(self) -> &'static str {
        match self {
            Self::DoGet => "do_get",
            Self::DoPut => "do_put",
            Self::DoExchange => "do_exchange",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct FlightIpcObservation {
    pub route: ParserRoute,
    pub codec: FlightIpcCompression,
    pub direction: FlightIpcDirection,
    pub action: FlightRpcAction,
    pub outcome: FlightIpcOutcome,
    pub logical_bytes: u64,
    pub wire_bytes: Option<u64>,
    pub duration: Duration,
}

impl FlightIpcObservation {
    pub fn record(self) {
        tracing::debug!(
            target: "parsers.flight.ipc",
            route = self.route.key(),
            codec = %self.codec,
            direction = self.direction.as_str(),
            action = self.action.as_str(),
            outcome = self.outcome.as_str(),
            logical_bytes = self.logical_bytes,
            wire_bytes = self.wire_bytes,
            duration_us = self.duration.as_micros() as u64,
            "Flight IPC operation"
        );
    }
}

pub struct FlightIpcObservationGuard {
    route: ParserRoute,
    codec: FlightIpcCompression,
    direction: FlightIpcDirection,
    action: FlightRpcAction,
    started: Instant,
    finished: bool,
    logical_bytes: u64,
    wire_bytes: u64,
    accounting_overflowed: bool,
}

impl FlightIpcObservationGuard {
    pub fn new(route: ParserRoute, direction: FlightIpcDirection, action: FlightRpcAction) -> Self {
        Self {
            route,
            codec: FlightIpcCompression::None,
            direction,
            action,
            started: Instant::now(),
            finished: false,
            logical_bytes: 0,
            wire_bytes: 0,
            accounting_overflowed: false,
        }
    }

    fn add_frame_bytes(&mut self, logical_bytes: u64, wire_bytes: u64) -> bool {
        let Some(logical_total) = self.logical_bytes.checked_add(logical_bytes) else {
            self.accounting_overflowed = true;
            return false;
        };
        let Some(wire_total) = self.wire_bytes.checked_add(wire_bytes) else {
            self.accounting_overflowed = true;
            return false;
        };
        self.logical_bytes = logical_total;
        self.wire_bytes = wire_total;
        true
    }

    #[cfg(test)]
    fn totals(&self) -> (u64, u64) {
        (self.logical_bytes, self.wire_bytes)
    }

    pub fn finish(
        mut self,
        outcome: FlightIpcOutcome,
        logical_bytes: u64,
        wire_bytes: Option<u64>,
    ) {
        self.finished = true;
        FlightIpcObservation {
            route: self.route,
            codec: self.codec,
            direction: self.direction,
            action: self.action,
            outcome,
            logical_bytes,
            wire_bytes,
            duration: self.started.elapsed(),
        }
        .record();
    }
}

impl Drop for FlightIpcObservationGuard {
    fn drop(&mut self) {
        if !self.finished {
            FlightIpcObservation {
                route: self.route,
                codec: self.codec,
                direction: self.direction,
                action: self.action,
                outcome: if self.accounting_overflowed {
                    FlightIpcOutcome::ResourceExhausted
                } else {
                    FlightIpcOutcome::DecodeError
                },
                logical_bytes: self.logical_bytes,
                wire_bytes: Some(self.wire_bytes),
                duration: self.started.elapsed(),
            }
            .record();
        }
    }
}

/// Attach a guard to a parser's FlightData response stream.  Observation is
/// emitted exactly once when the stream reaches EOF, yields an error, or is
/// dropped, and byte totals are accumulated from the raw protobuf frames.
pub fn observe_flight_stream<S>(
    stream: S,
    guard: FlightIpcObservationGuard,
) -> futures::stream::BoxStream<'static, Result<arrow_flight::FlightData, tonic::Status>>
where
    S: Stream<Item = Result<arrow_flight::FlightData, tonic::Status>> + Send + 'static,
{
    let stream: Pin<Box<S>> = Box::pin(stream);
    futures::stream::unfold(
        (stream, Some(guard)),
        |(mut stream, mut guard)| async move {
            match stream.next().await {
                Some(Ok(frame)) => {
                    let accounting_overflowed = if let Some(ref mut guard) = guard {
                        !guard.add_frame_bytes(
                            frame.data_body.len() as u64,
                            frame.encoded_len() as u64,
                        )
                    } else {
                        false
                    };
                    if accounting_overflowed {
                        if let Some(guard) = guard.take() {
                            let logical_bytes = guard.logical_bytes;
                            let wire_bytes = guard.wire_bytes;
                            guard.finish(
                                FlightIpcOutcome::ResourceExhausted,
                                logical_bytes,
                                Some(wire_bytes),
                            );
                        }
                    }
                    Some((Ok(frame), (stream, guard)))
                }
                Some(Err(status)) => {
                    if let Some(guard) = guard.take() {
                        let logical_bytes = guard.logical_bytes;
                        let wire_bytes = guard.wire_bytes;
                        guard.finish(
                            FlightIpcOutcome::DecodeError,
                            logical_bytes,
                            Some(wire_bytes),
                        );
                    }
                    Some((Err(status), (stream, guard)))
                }
                None => {
                    if let Some(guard) = guard.take() {
                        let logical_bytes = guard.logical_bytes;
                        let wire_bytes = guard.wire_bytes;
                        guard.finish(FlightIpcOutcome::Ok, logical_bytes, Some(wire_bytes));
                    }
                    None
                }
            }
        },
    )
    .boxed()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParserRoute {
    Docx,
    Xlsx,
    Pptx,
    Xsd,
    Email,
    Json,
}

impl ParserRoute {
    pub const fn key(self) -> &'static str {
        match self {
            Self::Docx => "parser.docx.server.response",
            Self::Xlsx => "parser.xlsx.server.response",
            Self::Pptx => "parser.pptx.server.response",
            Self::Xsd => "parser.xsd.server.response",
            Self::Email => "parser.email.server.response",
            Self::Json => "parser.json.server.response",
        }
    }

    pub const fn identity(self) -> &'static str {
        match self {
            Self::Docx => "docx-fast-flight",
            Self::Xlsx => "xlsx-fast-flight",
            Self::Pptx => "pptx-fast-flight",
            Self::Xsd => "xsd-fast-flight",
            Self::Email => "email-fast-flight",
            Self::Json => "json-fast-flight",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlightIpcCompression {
    None,
    Lz4,
    Zstd,
}

impl FlightIpcCompression {
    pub const fn arrow_type(self) -> Option<CompressionType> {
        match self {
            Self::None => None,
            Self::Lz4 => Some(CompressionType::LZ4_FRAME),
            Self::Zstd => Some(CompressionType::ZSTD),
        }
    }
}

impl fmt::Display for FlightIpcCompression {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::None => "none",
            Self::Lz4 => "lz4",
            Self::Zstd => "zstd",
        })
    }
}

impl std::str::FromStr for FlightIpcCompression {
    type Err = FlightIpcConfigError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "none" => Ok(Self::None),
            "lz4" => Ok(Self::Lz4),
            "zstd" => Ok(Self::Zstd),
            _ => Err(FlightIpcConfigError::InvalidCodec),
        }
    }
}

#[derive(Debug)]
pub struct AuthorizedFlightWrite {
    route: ParserRoute,
    codec: FlightIpcCompression,
    options: IpcWriteOptions,
}

impl AuthorizedFlightWrite {
    pub fn route(&self) -> ParserRoute {
        self.route
    }

    pub fn codec(&self) -> FlightIpcCompression {
        self.codec
    }

    pub fn into_ipc_write_options(self) -> IpcWriteOptions {
        self.options
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriterPolicies {
    route: ParserRoute,
}

impl WriterPolicies {
    pub fn authorized_write(&self) -> Result<AuthorizedFlightWrite, FlightIpcConfigError> {
        let codec = FlightIpcCompression::None;
        let options = IpcWriteOptions::default()
            .try_with_compression(codec.arrow_type())
            .map_err(|error| FlightIpcConfigError::ArrowOptions(error.to_string()))?;
        Ok(AuthorizedFlightWrite {
            route: self.route,
            codec,
            options,
        })
    }

    pub fn route(&self) -> ParserRoute {
        self.route
    }
}

/// Parse process values without reading environment state.  Public parser
/// routes are fixed to `none`, including explicit overrides.
pub fn parse_writer_policies_for_process(
    default: Option<&str>,
    overrides: Option<&str>,
    route: ParserRoute,
) -> Result<WriterPolicies, FlightIpcConfigError> {
    let default = default.unwrap_or("none").parse::<FlightIpcCompression>()?;
    if default != FlightIpcCompression::None {
        return Err(FlightIpcConfigError::UnsafeRoute(route.key().to_owned()));
    }
    if let Some(overrides) = overrides {
        let values = parse_overrides(overrides)?;
        for (key, value) in values {
            if key != route.key() {
                return Err(FlightIpcConfigError::UnknownRoute(key));
            }
            if value != FlightIpcCompression::None {
                return Err(FlightIpcConfigError::UnsafeRoute(key));
            }
        }
    }
    Ok(WriterPolicies { route })
}

fn parse_overrides(
    value: &str,
) -> Result<BTreeMap<String, FlightIpcCompression>, FlightIpcConfigError> {
    struct Visitor;
    impl<'de> de::Visitor<'de> for Visitor {
        type Value = BTreeMap<String, FlightIpcCompression>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("a route-to-codec JSON object")
        }

        fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
        where
            A: de::MapAccess<'de>,
        {
            let mut values = BTreeMap::new();
            while let Some((route, codec)) = map.next_entry::<String, String>()? {
                let codec = codec.parse().map_err(de::Error::custom)?;
                if values.insert(route.clone(), codec).is_some() {
                    return Err(de::Error::custom(format!(
                        "duplicate route override: {route}"
                    )));
                }
            }
            Ok(values)
        }
    }
    let mut deserializer = serde_json::Deserializer::from_str(value);
    let parsed = deserializer
        .deserialize_map(Visitor)
        .map_err(|error| FlightIpcConfigError::Overrides(error.to_string()))?;
    deserializer
        .end()
        .map_err(|error| FlightIpcConfigError::Overrides(error.to_string()))?;
    Ok(parsed)
}

/// Cached capability body for parser actions.  It is intentionally plain JSON
/// and contains no endpoint, tenant, ticket, or descriptor labels.
pub fn capability_json(
    route: ParserRoute,
    writer_codec: Option<FlightIpcCompression>,
) -> Result<serde_json::Value, FlightIpcConfigError> {
    let write_routes = match (route, writer_codec) {
        // Reader-only routes (email, json) never advertise a writer codec.
        (ParserRoute::Email | ParserRoute::Json, None) => serde_json::json!({}),
        (ParserRoute::Email | ParserRoute::Json, Some(_)) | (_, None) => {
            return Err(FlightIpcConfigError::CapabilityWriterMismatch(
                route.key().to_owned(),
            ));
        }
        (_, Some(codec)) => serde_json::json!({route.key(): codec.to_string()}),
    };

    Ok(serde_json::json!({
        "receiver_identity": route.identity(),
        "receiver_profile": CAPABILITY_RECEIVER_PROFILE,
        "auth_mode": "existing-auth",
        "build_sha": option_env!("PARSERS_BUILD_SHA").unwrap_or(CAPABILITY_BUILD_SHA),
        "arrow_version": CAPABILITY_ARROW_VERSION,
        "read_codecs": ["none", "lz4", "zstd"],
        "bounded_decode_ready": false,
        "write_routes": write_routes,
    }))
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum FlightIpcConfigError {
    #[error("Flight IPC compression must be exactly one of: none, lz4, zstd")]
    InvalidCodec,
    #[error("unsafe compression override for public route: {0}")]
    UnsafeRoute(String),
    #[error("unknown Flight writer route: {0}")]
    UnknownRoute(String),
    #[error("invalid route overrides: {0}")]
    Overrides(String),
    #[error("Arrow rejected the Flight IPC compression options: {0}")]
    ArrowOptions(String),
    #[error("Flight capability writer presence does not match route: {0}")]
    CapabilityWriterMismatch(String),
}

// ---------------------------------------------------------------------------
// Shared Flight server helpers, extracted byte-identical from the per-parser
// binaries (docx-fast-flight, pptx-fast-flight).  Do not diverge them here.
// ---------------------------------------------------------------------------

/// Canonicalize `path` (joined under `root` when relative) and reject any
/// resolution that escapes the Flight root.
pub fn resolve_path(root: &Path, path: &Path) -> Result<PathBuf, Status> {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };
    let canon = joined
        .canonicalize()
        .map_err(|_| Status::not_found("file not found"))?;
    let root_canon = root
        .canonicalize()
        .map_err(|_| Status::internal("failed to resolve root"))?;
    if !canon.starts_with(&root_canon) {
        return Err(Status::permission_denied("path escapes root"));
    }
    Ok(canon)
}

/// Split `input` on the first `?` into a `(path, query)` pair, zero-copy.
pub fn split_query(input: &str) -> Option<(&str, &str)> {
    let (path, query) = input.split_once('?')?;

    Some((path, query))
}

/// Decode percent-escapes in a query value; `+` becomes a space and invalid
/// `%` sequences are passed through unchanged.
pub fn url_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hi = bytes[i + 1];
                let lo = bytes[i + 2];
                if let (Some(h), Some(l)) = (from_hex(hi), from_hex(lo)) {
                    out.push((h << 4) | l);
                    i += 3;
                    continue;
                }
                out.push(bytes[i]);
            }
            b'+' => out.push(b' '),
            ch => out.push(ch),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

/// Decode one ASCII hex digit for [`url_decode`].
pub fn from_hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Fail-loud DoPut: these servers are read-only parse egress (no write-back).
pub const DO_PUT_UNSUPPORTED: &str =
    "do_put is not supported; Flight servers are read-only parse egress (no write-back)";

/// Stable `Unimplemented` status rejecting DoPut on read-only servers.
pub fn do_put_unsupported() -> Status {
    Status::unimplemented(DO_PUT_UNSUPPORTED)
}

/// Parsed query-string overrides shared by the read-only parser Flight
/// servers: `table` selection plus batch-size and inflate-limit tuning.
#[derive(Default, Debug, Clone)]
pub struct FlightQuery {
    pub table: Option<String>,
    pub batch_size: Option<usize>,
    pub max_inflate: Option<usize>,
}

/// The path and the raw query string a `FlightDescriptor` names, before the
/// path is looked up. The query is empty when the descriptor carries none.
///
/// Servers with their own query keys parse the query themselves and then
/// call [`resolve_path`]; [`descriptor_to_request`] does both for the shared
/// keys.
pub fn descriptor_path_and_query(
    descriptor: &FlightDescriptor,
) -> Result<(PathBuf, String), Status> {
    if !descriptor.path.is_empty() {
        let mut parts = descriptor.path.clone();
        let mut query = String::new();
        if let Some(last) = parts.last_mut() {
            let last_value = last.clone();
            if let Some((base, q)) = split_query(&last_value) {
                *last = base.to_string();
                query = q.to_string();
            }
        }
        let path = parts.into_iter().fold(PathBuf::new(), |mut buf, part| {
            buf.push(part);
            buf
        });
        Ok((path, query))
    } else if !descriptor.cmd.is_empty() {
        let cmd = std::str::from_utf8(&descriptor.cmd)
            .map_err(|_| Status::invalid_argument("descriptor cmd must be utf-8 path"))?;
        let (path_str, query) = split_query(cmd).unwrap_or((cmd, ""));
        Ok((PathBuf::from(path_str), query.to_string()))
    } else {
        Err(Status::invalid_argument("missing flight descriptor path"))
    }
}

/// The path and the raw query string a `Ticket` names, before the path is
/// looked up. The query is empty when the ticket carries none.
pub fn ticket_path_and_query(ticket: &Ticket) -> Result<(&str, &str), Status> {
    let path = std::str::from_utf8(&ticket.ticket)
        .map_err(|_| Status::invalid_argument("ticket must be utf-8 path"))?;
    Ok(split_query(path).unwrap_or((path, "")))
}

/// Resolve a `FlightDescriptor` (path components or UTF-8 cmd) to a rooted
/// path plus its parsed query overrides.
pub fn descriptor_to_request(
    root: &Path,
    descriptor: &FlightDescriptor,
) -> Result<(PathBuf, FlightQuery), Status> {
    let (path, query) = descriptor_path_and_query(descriptor)?;
    let query = parse_query(&query)?;
    resolve_path(root, &path).map(|path| (path, query))
}

/// Resolve a `Ticket` (UTF-8 path with optional query string) to a rooted
/// path plus its parsed query overrides.
pub fn ticket_to_request(root: &Path, ticket: &Ticket) -> Result<(PathBuf, FlightQuery), Status> {
    let (path, query) = ticket_path_and_query(ticket)?;
    let query = parse_query(query)?;
    resolve_path(root, Path::new(path)).map(|path| (path, query))
}

/// Parse `table` / `batch`|`batch_size` / `max_inflate` query pairs; unknown
/// keys are ignored and zero or non-integer limits are rejected.
pub fn parse_query(query: &str) -> Result<FlightQuery, Status> {
    let mut out = FlightQuery::default();
    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let mut iter = pair.splitn(2, '=');
        let key = iter.next().unwrap_or("").trim();
        let value = iter.next().unwrap_or("");
        if key.is_empty() {
            continue;
        }
        match key {
            "table" => {
                let decoded = url_decode(value);
                if !decoded.trim().is_empty() {
                    out.table = Some(decoded.trim().to_string());
                }
            }
            "batch" | "batch_size" => {
                let decoded = url_decode(value);
                let parsed = decoded
                    .parse::<usize>()
                    .map_err(|_| Status::invalid_argument("batch must be a positive integer"))?;
                if parsed == 0 {
                    return Err(Status::invalid_argument("batch must be > 0"));
                }
                out.batch_size = Some(parsed);
            }
            "max_inflate" => {
                let decoded = url_decode(value);
                let parsed = decoded.parse::<usize>().map_err(|_| {
                    Status::invalid_argument("max_inflate must be a positive integer")
                })?;
                if parsed == 0 {
                    return Err(Status::invalid_argument("max_inflate must be > 0"));
                }
                out.max_inflate = Some(parsed);
            }
            _ => {}
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_values_are_none_and_non_none_is_rejected_before_io() {
        let policies = parse_writer_policies_for_process(None, None, ParserRoute::Docx).unwrap();
        assert_eq!(
            policies.authorized_write().unwrap().codec(),
            FlightIpcCompression::None
        );
        assert!(parse_writer_policies_for_process(Some("zstd"), None, ParserRoute::Docx).is_err());
        assert!(parse_writer_policies_for_process(
            None,
            Some(r#"{"parser.docx.server.response":"zstd"}"#),
            ParserRoute::Docx,
        )
        .is_err());
        assert!(parse_writer_policies_for_process(
            None,
            Some(r#"{"parser.docx.server.response":"none","parser.docx.server.response":"none"}"#),
            ParserRoute::Docx,
        )
        .is_err());
    }

    #[test]
    fn capability_reports_existing_auth_and_selected_writer_codec() {
        let capability = capability_json(ParserRoute::Docx, Some(FlightIpcCompression::Lz4))
            .expect("writer capability");
        assert_eq!(capability["auth_mode"], "existing-auth");
        assert_eq!(capability["write_routes"][ParserRoute::Docx.key()], "lz4");

        let email = capability_json(ParserRoute::Email, None).expect("reader-only capability");
        assert_eq!(email["auth_mode"], "existing-auth");
        assert_eq!(email["write_routes"], serde_json::json!({}));

        let json = capability_json(ParserRoute::Json, None).expect("reader-only capability");
        assert_eq!(json["receiver_identity"], "json-fast-flight");
        assert_eq!(json["write_routes"], serde_json::json!({}));
        assert_eq!(ParserRoute::Json.key(), "parser.json.server.response");
    }

    #[test]
    fn capability_rejects_writer_presence_mismatch() {
        assert!(matches!(
            capability_json(ParserRoute::Docx, None),
            Err(FlightIpcConfigError::CapabilityWriterMismatch(route))
                if route == ParserRoute::Docx.key()
        ));
        assert!(matches!(
            capability_json(ParserRoute::Email, Some(FlightIpcCompression::None)),
            Err(FlightIpcConfigError::CapabilityWriterMismatch(route))
                if route == ParserRoute::Email.key()
        ));
    }

    #[test]
    fn telemetry_observation_is_finite_and_payload_free() {
        let observation = FlightIpcObservation {
            route: ParserRoute::Docx,
            codec: FlightIpcCompression::None,
            direction: FlightIpcDirection::Read,
            action: FlightRpcAction::DoGet,
            outcome: FlightIpcOutcome::Ok,
            logical_bytes: 17,
            wire_bytes: Some(21),
            duration: Duration::from_micros(4),
        };
        assert_eq!(observation.route.key(), "parser.docx.server.response");
        observation.record();
    }

    #[test]
    fn observation_guard_uses_checked_shared_totals_on_drop_paths() {
        let mut guard = FlightIpcObservationGuard::new(
            ParserRoute::Docx,
            FlightIpcDirection::Read,
            FlightRpcAction::DoGet,
        );
        assert!(guard.add_frame_bytes(17, 23));
        assert_eq!(guard.totals(), (17, 23));
        guard.logical_bytes = u64::MAX;
        assert!(!guard.add_frame_bytes(1, 1));
        assert_eq!(guard.totals(), (u64::MAX, 23));
        // Drop must observe the accumulated values instead of resetting to
        // zero, including when accounting itself overflows.
        drop(guard);
    }

    #[tokio::test]
    async fn observed_flight_stream_reports_completion_and_error_once() {
        let guard = FlightIpcObservationGuard::new(
            ParserRoute::Docx,
            FlightIpcDirection::Read,
            FlightRpcAction::DoGet,
        );
        let frame = arrow_flight::FlightData {
            data_body: vec![1_u8, 2, 3].into(),
            ..Default::default()
        };
        let mut stream = observe_flight_stream(futures::stream::iter(vec![Ok(frame)]), guard);
        assert!(stream.next().await.is_some());
        assert!(stream.next().await.is_none());

        let guard = FlightIpcObservationGuard::new(
            ParserRoute::Xsd,
            FlightIpcDirection::Read,
            FlightRpcAction::DoExchange,
        );
        let mut stream = observe_flight_stream(
            futures::stream::iter(vec![Err(tonic::Status::internal("payload omitted"))]),
            guard,
        );
        assert!(stream.next().await.is_some());
        assert!(stream.next().await.is_none());
    }

    #[test]
    fn url_decode_handles_percent_plus_and_invalid_sequences() {
        assert_eq!(url_decode("plain"), "plain");
        assert_eq!(url_decode("a%20b%2Fc"), "a b/c");
        assert_eq!(url_decode("a+b"), "a b");
        assert_eq!(url_decode("%4A%4a"), "JJ");
        // Invalid or truncated escapes pass through unchanged.
        assert_eq!(url_decode("100%zz"), "100%zz");
        assert_eq!(url_decode("trailing%"), "trailing%");
        assert_eq!(url_decode("truncated%2"), "truncated%2");
    }

    #[test]
    fn from_hex_decodes_ascii_hex_digits_only() {
        assert_eq!(from_hex(b'0'), Some(0));
        assert_eq!(from_hex(b'9'), Some(9));
        assert_eq!(from_hex(b'a'), Some(10));
        assert_eq!(from_hex(b'f'), Some(15));
        assert_eq!(from_hex(b'A'), Some(10));
        assert_eq!(from_hex(b'F'), Some(15));
        assert_eq!(from_hex(b'g'), None);
        assert_eq!(from_hex(b'%'), None);
    }

    #[test]
    fn split_query_splits_on_first_question_mark_only() {
        assert_eq!(
            split_query("a/b.docx?table=paragraphs"),
            Some(("a/b.docx", "table=paragraphs"))
        );
        assert_eq!(split_query("a?b?c"), Some(("a", "b?c")));
        assert_eq!(split_query("no-query-here"), None);
        assert_eq!(split_query("?"), Some(("", "")));
    }

    #[test]
    fn parse_query_parses_table_batch_and_max_inflate() {
        let query = parse_query("table=paragraphs&batch=512&max_inflate=4096").expect("valid");
        assert_eq!(query.table.as_deref(), Some("paragraphs"));
        assert_eq!(query.batch_size, Some(512));
        assert_eq!(query.max_inflate, Some(4096));

        let aliased = parse_query("batch_size=256").expect("batch_size alias");
        assert_eq!(aliased.batch_size, Some(256));

        let decoded = parse_query("table=my%20table").expect("percent-decoded table");
        assert_eq!(decoded.table.as_deref(), Some("my table"));
    }

    #[test]
    fn parse_query_rejects_zero_and_non_integer_limits() {
        for bad in ["batch=0", "batch_size=0", "max_inflate=0"] {
            let err = parse_query(bad).expect_err("zero limits must fail");
            assert_eq!(err.code(), tonic::Code::InvalidArgument);
        }
        for bad in ["batch=abc", "batch_size=1.5", "max_inflate=-1"] {
            let err = parse_query(bad).expect_err("non-integer limits must fail");
            assert_eq!(err.code(), tonic::Code::InvalidArgument);
        }
    }

    #[test]
    fn parse_query_ignores_unknown_keys_and_empty_pairs() {
        let query = parse_query("&&=orphan&unknown=1&nopair&table=&batch=8&").expect("ignored");
        // Blank table values are dropped rather than stored empty.
        assert_eq!(query.table, None);
        assert_eq!(query.batch_size, Some(8));
        assert_eq!(query.max_inflate, None);
        assert_eq!(parse_query("").expect("empty query").table, None);
    }

    #[test]
    fn resolve_path_rejects_escape_and_accepts_rooted_file() {
        // std-only temp layout: unique subdir under the OS temp dir.
        let unique = format!(
            "flight-ipc-config-resolve-path-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
        );
        let base = std::env::temp_dir().join(unique);
        let root = base.join("root");
        let outside = base.join("outside");
        std::fs::create_dir_all(&root).expect("root dir");
        std::fs::create_dir_all(&outside).expect("outside dir");
        let inside_file = root.join("inside.txt");
        let outside_file = outside.join("outside.txt");
        std::fs::write(&inside_file, b"in").expect("write inside");
        std::fs::write(&outside_file, b"out").expect("write outside");

        let resolved = resolve_path(&root, Path::new("inside.txt")).expect("inside resolves");
        assert_eq!(resolved, inside_file.canonicalize().expect("canon"));

        let err = resolve_path(&root, &outside_file).expect_err("escape must fail");
        assert_eq!(err.code(), tonic::Code::PermissionDenied);
        let err = resolve_path(&root, Path::new("../outside/outside.txt"))
            .expect_err("relative escape must fail");
        assert_eq!(err.code(), tonic::Code::PermissionDenied);

        std::fs::remove_dir_all(&base).expect("cleanup");
    }
}
