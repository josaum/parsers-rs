//! A recording Flight client for the wire tests of the server binaries.
//!
//! Each test serves its service on a loopback port, drives every RPC through
//! a real client, with and without a bearer token, and compares the recorded
//! text with a pinned transcript. Paths under the test's base directory and
//! the operating system's own error text are replaced by placeholders so the
//! transcript is the same on every run.
#![allow(dead_code)]

use std::fmt::Write as _;
use std::path::Path;

use arrow_flight::flight_service_client::FlightServiceClient;
use arrow_flight::flight_service_server::{FlightService, FlightServiceServer};
use arrow_flight::{
    Action, Criteria, Empty, FlightData, FlightDescriptor, FlightInfo, HandshakeRequest, Ticket,
};
use tonic::transport::server::TcpIncoming;
use tonic::transport::Channel;
use tonic::{Request, Status};

use crate::flight_auth_test_support::with_valid_bearer;

/// A client connected to one served service, and everything it has seen.
pub struct Wire {
    client: FlightServiceClient<Channel>,
    placeholders: Vec<(String, &'static str)>,
    /// The transcript so far.
    pub seen: String,
}

fn fnv1a(parts: &[&[u8]]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in parts.iter().flat_map(|part| part.iter()) {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn status_line(status: &Status) -> String {
    format!("{:?}: {}", status.code(), status.message())
}

fn info_line(info: &FlightInfo) -> String {
    let path = info
        .flight_descriptor
        .as_ref()
        .map(|descriptor| {
            if descriptor.path.is_empty() {
                format!("cmd:{}", String::from_utf8_lossy(&descriptor.cmd))
            } else {
                descriptor.path.join("/")
            }
        })
        .unwrap_or_default();
    let tickets: Vec<String> = info
        .endpoint
        .iter()
        .filter_map(|endpoint| endpoint.ticket.as_ref())
        .map(|ticket| String::from_utf8_lossy(&ticket.ticket).into_owned())
        .collect();
    format!(
        "path={path} tickets={tickets:?} total_bytes={} total_records={} schema_len={} schema_fnv={:016x}",
        info.total_bytes,
        info.total_records,
        info.schema.len(),
        fnv1a(&[&info.schema]),
    )
}

/// Replace the operating system's wording of an I/O error, which differs
/// between platforms, with a placeholder.
fn without_os_error_text(line: &str) -> String {
    let Some(tail) = line.find(" (os error ") else {
        return line.to_owned();
    };
    let start = line[..tail].rfind(": ").map_or(0, |at| at + 2);
    let end = line[tail..]
        .find(')')
        .map_or(line.len(), |at| tail + at + 1);
    format!("{}<os error>{}", &line[..start], &line[end..])
}

fn data_stream_line(messages: &[FlightData]) -> String {
    let parts: Vec<&[u8]> = messages
        .iter()
        .flat_map(|message| {
            [
                message.data_header.as_ref(),
                message.app_metadata.as_ref(),
                message.data_body.as_ref(),
            ]
        })
        .collect();
    let bytes: usize = parts.iter().map(|part| part.len()).sum();
    let batches = arrow_flight::utils::flight_data_to_batches(messages).expect("decode");
    let rows: usize = batches.iter().map(|batch| batch.num_rows()).sum();
    let columns: Vec<String> = batches
        .first()
        .map(|batch| {
            batch
                .schema()
                .fields()
                .iter()
                .map(|f| f.name().clone())
                .collect()
        })
        .unwrap_or_default();
    format!(
        "messages={} bytes={bytes} fnv={:016x} batches={} rows={rows} columns={columns:?}",
        messages.len(),
        fnv1a(&parts),
        batches.len(),
    )
}

async fn drain_data(
    response: Result<tonic::Response<tonic::Streaming<FlightData>>, Status>,
) -> String {
    let mut stream = match response {
        Ok(response) => response.into_inner(),
        Err(status) => return status_line(&status),
    };
    let mut messages = Vec::new();
    loop {
        match stream.message().await {
            Ok(Some(message)) => messages.push(message),
            Ok(None) => break,
            Err(status) => return status_line(&status),
        }
    }
    data_stream_line(&messages)
}

impl Wire {
    /// Serve `service` on a loopback port and connect to it. Every path under
    /// `base` is written as `<base>` in the transcript.
    pub async fn start<S: FlightService>(service: S, base: &Path) -> Self {
        let incoming =
            TcpIncoming::bind("127.0.0.1:0".parse().expect("loopback address")).expect("bind");
        let addr = incoming.local_addr().expect("local address");
        tokio::spawn(
            tonic::transport::Server::builder()
                .add_service(FlightServiceServer::new(service))
                .serve_with_incoming(incoming),
        );
        let channel = tonic::transport::Endpoint::from_shared(format!("http://{addr}"))
            .expect("endpoint")
            .connect()
            .await
            .expect("connect");
        let mut placeholders = Vec::new();
        if let Ok(canonical) = base.canonicalize() {
            placeholders.push((canonical.to_string_lossy().into_owned(), "<base>"));
        }
        placeholders.push((base.to_string_lossy().into_owned(), "<base>"));
        Self {
            client: FlightServiceClient::new(channel),
            placeholders,
            seen: String::new(),
        }
    }

    fn record(&mut self, call: &str, bearer: bool, answer: String) {
        let suffix = if bearer { "" } else { " without a bearer" };
        let mut text = format!("{call}{suffix}\n    {answer}");
        for (path, placeholder) in &self.placeholders {
            text = text.replace(path.as_str(), placeholder);
        }
        for line in text.lines() {
            writeln!(self.seen, "{}", without_os_error_text(line)).expect("write to a string");
        }
    }

    fn request<T>(message: T, bearer: bool) -> Request<T> {
        let request = Request::new(message);
        if bearer {
            with_valid_bearer(request)
        } else {
            request
        }
    }

    /// Handshake with one empty request.
    pub async fn handshake(&mut self, bearer: bool) {
        let request = Self::request(
            futures::stream::iter(vec![HandshakeRequest::default()]),
            bearer,
        );
        let answer = match self.client.handshake(request).await {
            Ok(response) => {
                let mut stream = response.into_inner();
                let mut count = 0usize;
                let mut end = String::new();
                loop {
                    match stream.message().await {
                        Ok(Some(_)) => count += 1,
                        Ok(None) => break,
                        Err(status) => {
                            end = format!(" then {}", status_line(&status));
                            break;
                        }
                    }
                }
                format!("{count} responses{end}")
            }
            Err(status) => status_line(&status),
        };
        self.record("Handshake", bearer, answer);
    }

    /// ListFlights, sorted: directory order belongs to the operating system.
    pub async fn list_flights(&mut self, bearer: bool) {
        let request = Self::request(Criteria::default(), bearer);
        let answer = match self.client.list_flights(request).await {
            Ok(response) => {
                let mut stream = response.into_inner();
                let mut lines = Vec::new();
                loop {
                    match stream.message().await {
                        Ok(Some(info)) => lines.push(info_line(&info)),
                        Ok(None) => break,
                        Err(status) => {
                            lines.push(status_line(&status));
                            break;
                        }
                    }
                }
                lines.sort();
                format!("{} flights\n    {}", lines.len(), lines.join("\n    "))
            }
            Err(status) => status_line(&status),
        };
        self.record("ListFlights", bearer, answer);
    }

    async fn flight_info(&mut self, call: String, descriptor: FlightDescriptor, bearer: bool) {
        let request = Self::request(descriptor, bearer);
        let answer = match self.client.get_flight_info(request).await {
            Ok(response) => info_line(&response.into_inner()),
            Err(status) => status_line(&status),
        };
        self.record(&call, bearer, answer);
    }

    /// GetFlightInfo for a one-component path descriptor.
    pub async fn get_flight_info(&mut self, path: &str, bearer: bool) {
        let descriptor = FlightDescriptor::new_path(vec![path.to_owned()]);
        self.flight_info(format!("GetFlightInfo {path}"), descriptor, bearer)
            .await;
    }

    /// GetFlightInfo for a descriptor whose path has these components.
    pub async fn get_flight_info_parts(&mut self, parts: &[&str], bearer: bool) {
        let descriptor =
            FlightDescriptor::new_path(parts.iter().map(|part| (*part).to_owned()).collect());
        self.flight_info(format!("GetFlightInfo parts {parts:?}"), descriptor, bearer)
            .await;
    }

    /// GetFlightInfo for a command descriptor.
    pub async fn get_flight_info_cmd(&mut self, cmd: &[u8], bearer: bool) {
        let descriptor = FlightDescriptor::new_cmd(cmd.to_vec());
        let call = format!("GetFlightInfo cmd {}", String::from_utf8_lossy(cmd));
        self.flight_info(call, descriptor, bearer).await;
    }

    /// PollFlightInfo for a one-component path descriptor.
    pub async fn poll_flight_info(&mut self, path: &str, bearer: bool) {
        let request = Self::request(FlightDescriptor::new_path(vec![path.to_owned()]), bearer);
        let answer = match self.client.poll_flight_info(request).await {
            Ok(_) => "accepted".to_owned(),
            Err(status) => status_line(&status),
        };
        self.record(&format!("PollFlightInfo {path}"), bearer, answer);
    }

    /// GetSchema for a one-component path descriptor.
    pub async fn get_schema(&mut self, path: &str, bearer: bool) {
        let request = Self::request(FlightDescriptor::new_path(vec![path.to_owned()]), bearer);
        let answer = match self.client.get_schema(request).await {
            Ok(response) => {
                let schema = response.into_inner().schema;
                format!(
                    "schema_len={} schema_fnv={:016x}",
                    schema.len(),
                    fnv1a(&[&schema])
                )
            }
            Err(status) => status_line(&status),
        };
        self.record(&format!("GetSchema {path}"), bearer, answer);
    }

    /// DoGet for a ticket.
    pub async fn do_get(&mut self, ticket: &[u8], bearer: bool) {
        let request = Self::request(Ticket::new(ticket.to_vec()), bearer);
        let answer = drain_data(self.client.do_get(request).await).await;
        let call = format!("DoGet {}", String::from_utf8_lossy(ticket));
        self.record(&call, bearer, answer);
    }

    /// DoPut with one empty message.
    pub async fn do_put(&mut self, bearer: bool) {
        let request = Self::request(futures::stream::iter(vec![FlightData::default()]), bearer);
        let answer = match self.client.do_put(request).await {
            Ok(_) => "accepted".to_owned(),
            Err(status) => status_line(&status),
        };
        self.record("DoPut", bearer, answer);
    }

    /// DoExchange with these messages, named `what` in the transcript.
    pub async fn do_exchange(&mut self, what: &str, messages: Vec<FlightData>, bearer: bool) {
        let request = Self::request(futures::stream::iter(messages), bearer);
        let answer = drain_data(self.client.do_exchange(request).await).await;
        self.record(&format!("DoExchange {what}"), bearer, answer);
    }

    /// DoExchange whose first message carries a one-component path descriptor.
    pub async fn do_exchange_path(&mut self, path: &str, bearer: bool) {
        let message = FlightData {
            flight_descriptor: Some(FlightDescriptor::new_path(vec![path.to_owned()])),
            ..FlightData::default()
        };
        self.do_exchange(&format!("descriptor {path}"), vec![message], bearer)
            .await;
    }

    /// DoExchange whose first message carries a ticket in `app_metadata`.
    pub async fn do_exchange_metadata(&mut self, ticket: &str, bearer: bool) {
        let message = FlightData {
            app_metadata: ticket.as_bytes().to_vec().into(),
            ..FlightData::default()
        };
        self.do_exchange(&format!("app_metadata {ticket}"), vec![message], bearer)
            .await;
    }

    /// DoAction whose result differs from run to run: only the number of
    /// results and their lengths are recorded.
    pub async fn do_action_lengths(&mut self, kind: &str, body: &str, bearer: bool) {
        let request = Self::request(
            Action {
                r#type: kind.to_owned(),
                body: body.as_bytes().to_vec().into(),
            },
            bearer,
        );
        let answer = match self.client.do_action(request).await {
            Ok(response) => {
                let mut stream = response.into_inner();
                let mut lengths = Vec::new();
                while let Ok(Some(result)) = stream.message().await {
                    lengths.push(result.body.len());
                }
                format!("result lengths {lengths:?}")
            }
            Err(status) => status_line(&status),
        };
        self.record(&format!("DoAction {kind} {body}"), bearer, answer);
    }

    /// DoAction; each result is recorded by length and hash, or as text when
    /// `show_text` is set.
    pub async fn do_action(&mut self, kind: &str, body: &str, show_text: bool, bearer: bool) {
        let request = Self::request(
            Action {
                r#type: kind.to_owned(),
                body: body.as_bytes().to_vec().into(),
            },
            bearer,
        );
        let answer = match self.client.do_action(request).await {
            Ok(response) => {
                let mut stream = response.into_inner();
                let mut results = Vec::new();
                loop {
                    match stream.message().await {
                        Ok(Some(result)) if show_text => {
                            results.push(String::from_utf8_lossy(&result.body).into_owned());
                        }
                        Ok(Some(result)) => results.push(format!(
                            "len={} fnv={:016x}",
                            result.body.len(),
                            fnv1a(&[&result.body])
                        )),
                        Ok(None) => break,
                        Err(status) => {
                            results.push(status_line(&status));
                            break;
                        }
                    }
                }
                format!("{results:?}")
            }
            Err(status) => status_line(&status),
        };
        self.record(&format!("DoAction {kind} {body}"), bearer, answer);
    }

    /// ListActions.
    pub async fn list_actions(&mut self, bearer: bool) {
        let request = Self::request(Empty {}, bearer);
        let answer = match self.client.list_actions(request).await {
            Ok(response) => {
                let mut stream = response.into_inner();
                let mut actions = Vec::new();
                loop {
                    match stream.message().await {
                        Ok(Some(action)) => {
                            actions.push(format!("{}: {}", action.r#type, action.description));
                        }
                        Ok(None) => break,
                        Err(status) => {
                            actions.push(status_line(&status));
                            break;
                        }
                    }
                }
                format!("{actions:?}")
            }
            Err(status) => status_line(&status),
        };
        self.record("ListActions", bearer, answer);
    }
}

/// A ZIP archive whose members are stored without compression, enough to
/// build a small OOXML fixture without a ZIP dependency.
pub fn stored_zip(members: &[(&str, &str)]) -> Vec<u8> {
    fn crc32(data: &[u8]) -> u32 {
        let mut crc = !0u32;
        for &byte in data {
            crc ^= u32::from(byte);
            for _ in 0..8 {
                crc = if crc & 1 != 0 {
                    (crc >> 1) ^ 0xEDB8_8320
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }
    fn u16le(out: &mut Vec<u8>, value: usize) {
        out.extend_from_slice(&u16::try_from(value).expect("fits u16").to_le_bytes());
    }
    fn u32le(out: &mut Vec<u8>, value: usize) {
        out.extend_from_slice(&u32::try_from(value).expect("fits u32").to_le_bytes());
    }
    // Fields shared by a local header and a central directory entry: version
    // needed, flags, method (stored), time, date (1980-01-01), CRC, sizes,
    // name length, extra length.
    fn common(out: &mut Vec<u8>, name: &str, data: &[u8]) {
        for value in [20, 0, 0, 0, 33] {
            u16le(out, value);
        }
        out.extend_from_slice(&crc32(data).to_le_bytes());
        u32le(out, data.len());
        u32le(out, data.len());
        u16le(out, name.len());
        u16le(out, 0);
    }

    let mut out = Vec::new();
    let mut central = Vec::new();
    for (name, contents) in members {
        let data = contents.as_bytes();
        let offset = out.len();
        out.extend_from_slice(&0x0403_4b50_u32.to_le_bytes());
        common(&mut out, name, data);
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(data);

        central.extend_from_slice(&0x0201_4b50_u32.to_le_bytes());
        u16le(&mut central, 20);
        common(&mut central, name, data);
        // Comment length, disk number, internal and external attributes.
        u16le(&mut central, 0);
        u16le(&mut central, 0);
        u16le(&mut central, 0);
        u32le(&mut central, 0);
        u32le(&mut central, offset);
        central.extend_from_slice(name.as_bytes());
    }
    let central_offset = out.len();
    out.extend_from_slice(&central);
    out.extend_from_slice(&0x0605_4b50_u32.to_le_bytes());
    u16le(&mut out, 0);
    u16le(&mut out, 0);
    u16le(&mut out, members.len());
    u16le(&mut out, members.len());
    u32le(&mut out, central.len());
    u32le(&mut out, central_offset);
    u16le(&mut out, 0);
    out
}
