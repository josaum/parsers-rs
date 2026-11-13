//! The `.pptx` Flight server. The server itself is
//! `flight_ipc_config::table_server`, shared with the other OOXML server;
//! this file says only what is particular to `.pptx`.
use std::path::Path;

use arrow_array::RecordBatch;
use arrow_schema::SchemaRef;
use flight_ipc_config::table_server::{self, TableParser};
use flight_ipc_config::ParserRoute;
use pptx_fast_core::arrow::{self, ArrowOptions};
use pptx_fast_core::ParseOptions;

#[cfg(test)]
#[path = "../../flight-ipc-config/test-support/mod.rs"]
mod flight_auth_test_support;

#[cfg(test)]
use arrow_flight::flight_service_server::{FlightService, FlightServiceServer};
#[cfg(test)]
use arrow_flight::{Action, Criteria, Empty, FlightData, FlightDescriptor, FlightInfo, Ticket};
#[cfg(test)]
use arrow_ipc::writer::IpcWriteOptions;
#[cfg(test)]
use flight_ipc_config::{do_put_unsupported, CAPABILITY_ACTION};
#[cfg(test)]
use std::path::PathBuf;
#[cfg(test)]
use tonic::{Request, Status};

/// What `.pptx` contributes to the shared server.
struct Pptx;

impl TableParser for Pptx {
    type ParseOptions = ParseOptions;
    type ArrowOptions = ArrowOptions;

    const ROUTE: ParserRoute = ParserRoute::Pptx;
    const SERVER_NAME: &'static str = "pptx-fast-flight";
    const DEFAULT_BIND: &'static str = "0.0.0.0:50053";
    const DEFAULT_TABLE: &'static str = "slides";
    const MISSING_DEFAULT_SCHEMA: &'static str = "missing schema for default pptx table";
    const EXTENSIONS: &'static [&'static str] = &["pptx", "pptm", "potx", "potm"];

    fn parse_options(max_inflate: usize) -> ParseOptions {
        ParseOptions {
            max_inflate,
            ..ParseOptions::default()
        }
    }

    fn arrow_options(batch_size: usize) -> ArrowOptions {
        ArrowOptions { batch_size }
    }

    fn set_max_inflate(options: &mut ParseOptions, max_inflate: usize) {
        options.max_inflate = max_inflate;
    }

    fn set_batch_size(options: &mut ArrowOptions, batch_size: usize) {
        options.batch_size = batch_size;
    }

    fn list_tables() -> &'static [&'static str] {
        arrow::list_tables()
    }

    fn schema_for_table(table: &str) -> Option<SchemaRef> {
        arrow::schema_for_table(table)
    }

    fn parse_table_batches<F>(
        path: &Path,
        parse_options: ParseOptions,
        arrow_options: ArrowOptions,
        table: &str,
        emit: F,
    ) -> anyhow::Result<()>
    where
        F: FnMut(RecordBatch) -> anyhow::Result<()>,
    {
        arrow::parse_pptx_to_arrow_table_batches(path, parse_options, arrow_options, table, emit)
    }
}

#[cfg(test)]
type PptxFlightService = table_server::TableFlightService<Pptx>;

/// The name the tests below have always called the listing walk by.
#[cfg(test)]
fn collect_pptx_paths(root: &Path, limit: usize) -> Result<Vec<PathBuf>, Status> {
    table_server::collect_paths::<Pptx>(root, limit)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    table_server::run::<Pptx>().await
}

#[cfg(test)]
mod symlink_listing_tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::symlink as symlink_file;
    #[cfg(windows)]
    use std::os::windows::fs::symlink_file;

    #[test]
    fn collect_pptx_paths_skips_symlinks() {
        // WHY: listing must not follow symlinks that could escape the Flight
        // root; canonicalize on DoGet is not enough if ListFlights advertises
        // the linked path.
        let root = tempfile::tempdir().expect("tempdir");
        let outside = tempfile::tempdir().expect("outside");
        let real = outside.path().join("escape.pptx");
        std::fs::write(&real, b"PK").expect("write");
        let link = root.path().join("escape.pptx");
        if let Err(err) = symlink_file(&real, &link) {
            #[cfg(windows)]
            if err.raw_os_error() == Some(1314) {
                return;
            }
            panic!("symlink: {err}");
        }
        let inside = root.path().join("ok.pptx");
        std::fs::write(&inside, b"PK").expect("write inside");

        let found = collect_pptx_paths(root.path(), 100).expect("collect");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0], inside);
    }
}

#[cfg(test)]
mod read_only_flight_contract_tests {
    use super::*;
    use crate::flight_auth_test_support::{required_auth_verifier, with_valid_bearer};

    fn capability_test_service() -> PptxFlightService {
        PptxFlightService {
            root: PathBuf::new(),
            parse_options: ParseOptions {
                max_inflate: 128 * 1024 * 1024,
                ..ParseOptions::default()
            },
            arrow_options: ArrowOptions { batch_size: 64 },
            write_options: IpcWriteOptions::default(),
            capability_body: b"{}".to_vec(),
            auth_verifier: required_auth_verifier(),
        }
    }

    #[test]
    fn do_put_fails_loud_as_read_only() {
        // WHY: there is no write-back protocol; DoPut must fail loud with a
        // stable Unimplemented message so clients do not treat silence as OK.
        let status = do_put_unsupported();
        assert_eq!(status.code(), tonic::Code::Unimplemented);
        assert!(status.message().contains("read-only"));
        assert!(status.message().contains("do_put"));
    }

    #[tokio::test]
    async fn capability_action_and_listing_reject_missing_authorization() {
        let service = capability_test_service();
        let Err(action) = FlightService::do_action(
            &service,
            Request::new(Action {
                r#type: CAPABILITY_ACTION.to_owned(),
                body: Default::default(),
            }),
        )
        .await
        else {
            panic!("missing capability auth must fail");
        };
        let Err(list) = FlightService::list_actions(&service, Request::new(Empty {})).await else {
            panic!("missing listing auth must fail");
        };
        assert_eq!(action.code(), tonic::Code::Unauthenticated);
        assert_eq!(list.code(), tonic::Code::Unauthenticated);
    }

    #[tokio::test]
    async fn retrieval_rejects_missing_authorization_before_request_parsing() {
        let service = capability_test_service();
        let result = FlightService::get_schema(
            &service,
            Request::new(FlightDescriptor::new_path(vec![
                "__missing_auth_probe__.pptx".to_owned(),
            ])),
        )
        .await;
        let Err(error) = result else {
            panic!("unauthenticated retrieval must fail");
        };
        assert_eq!(error.code(), tonic::Code::Unauthenticated);
    }

    #[tokio::test]
    async fn valid_bearer_reaches_retrieval_dispatch() {
        let service = capability_test_service();
        let request = with_valid_bearer(Request::new(FlightDescriptor::new_path(vec![
            "__missing_auth_probe__.pptx".to_owned(),
        ])));
        let Err(error) = FlightService::get_schema(&service, request).await else {
            panic!("missing test file must reach path validation");
        };
        assert_eq!(error.code(), tonic::Code::NotFound);
    }
}

#[cfg(test)]
mod wire_parity_tests {
    //! What a Flight client sees from this server, pinned as text.
    //!
    //! The expectations were captured from the server as it stood before the
    //! docx and pptx servers were moved onto one shared implementation, so a
    //! change to any answer below is a change in behaviour, not in structure.
    //! The fixture is the minimal document `pptx-fast-core`'s own tests build
    //! (`tests/parser_integration.rs`), written as a stored archive.
    use super::*;
    use crate::flight_auth_test_support::{required_auth_verifier, with_valid_bearer};
    use arrow_flight::flight_service_client::FlightServiceClient;
    use std::fmt::Write as _;
    use tonic::transport::server::TcpIncoming;
    use tonic::transport::Channel;

    const FIXTURE: &[u8] = include_bytes!("../tests/fixtures/minimal.pptx");

    const EXPECTED: &str = r#"ListFlights without a bearer
    Unauthenticated: missing authorization metadata
ListFlights
    2 flights
    path=minimal.pptx tickets=["minimal.pptx"] total_bytes=3048 total_records=-1 schema_len=320 schema_fnv=dfff7ba7a41c5408
    path=sub/Upper.PPTX tickets=["sub/Upper.PPTX"] total_bytes=3048 total_records=-1 schema_len=320 schema_fnv=dfff7ba7a41c5408
GetFlightInfo without a bearer
    Unauthenticated: missing authorization metadata
GetFlightInfo minimal.pptx
    path=minimal.pptx tickets=["minimal.pptx"] total_bytes=3048 total_records=-1 schema_len=320 schema_fnv=dfff7ba7a41c5408
GetFlightInfo minimal.pptx?table=paragraphs
    path=minimal.pptx?table=paragraphs tickets=["minimal.pptx"] total_bytes=3048 total_records=-1 schema_len=384 schema_fnv=acc212ad9d8e4435
GetFlightInfo minimal.pptx?table=no_such_table
    InvalidArgument: unknown table
GetFlightInfo sub/Upper.PPTX
    path=sub/Upper.PPTX tickets=["sub/Upper.PPTX"] total_bytes=3048 total_records=-1 schema_len=320 schema_fnv=dfff7ba7a41c5408
GetFlightInfo missing.pptx
    NotFound: file not found
GetFlightInfo ../minimal.pptx
    NotFound: file not found
DoGet without a bearer
    Unauthenticated: missing authorization metadata
DoGet minimal.pptx
    messages=2 bytes=760 fnv=9f1d4e0f9cfe412d batches=1 rows=1 columns=["slide_id", "name", "slide_index"]
DoGet minimal.pptx?table=paragraphs
    messages=2 bytes=1152 fnv=d3d32b4542cd2863 batches=1 rows=2 columns=["paragraph_id", "shape_id", "para_index", "level", "text"]
DoGet minimal.pptx?table=paragraphs&batch=1
    messages=3 bytes=1944 fnv=920edaa49a5652a1 batches=2 rows=2 columns=["paragraph_id", "shape_id", "para_index", "level", "text"]
DoGet minimal.pptx?table=no_such_table
    InvalidArgument: unknown table
DoGet sub/Upper.PPTX
    messages=2 bytes=760 fnv=9f1d4e0f9cfe412d batches=1 rows=1 columns=["slide_id", "name", "slide_index"]
DoGet notes.txt
    Internal: External error: open zip archive
DoGet missing.pptx
    NotFound: file not found
DoGet ../minimal.pptx
    NotFound: file not found
DoPut without a bearer
    Unauthenticated: missing authorization metadata
DoPut
    Unimplemented: do_put is not supported; Flight servers are read-only parse egress (no write-back)
"#;

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
            .map(|descriptor| descriptor.path.join("/"))
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

    async fn client_for(root: &Path) -> FlightServiceClient<Channel> {
        let service = PptxFlightService {
            root: root.to_path_buf(),
            parse_options: ParseOptions {
                max_inflate: 128 * 1024 * 1024,
                ..ParseOptions::default()
            },
            arrow_options: ArrowOptions { batch_size: 64 },
            write_options: IpcWriteOptions::default(),
            capability_body: b"{}".to_vec(),
            auth_verifier: required_auth_verifier(),
        };
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
        FlightServiceClient::new(channel)
    }

    async fn list_flights(
        client: &mut FlightServiceClient<Channel>,
        request: Request<Criteria>,
    ) -> String {
        let mut stream = match client.list_flights(request).await {
            Ok(response) => response.into_inner(),
            Err(status) => return status_line(&status),
        };
        // Directory order belongs to the operating system, not to the server.
        let mut lines = Vec::new();
        loop {
            match stream.message().await {
                Ok(Some(info)) => lines.push(info_line(&info)),
                Ok(None) => break,
                Err(status) => lines.push(status_line(&status)),
            }
        }
        lines.sort();
        format!("{} flights\n    {}", lines.len(), lines.join("\n    "))
    }

    async fn get_flight_info(
        client: &mut FlightServiceClient<Channel>,
        request: Request<FlightDescriptor>,
    ) -> String {
        match client.get_flight_info(request).await {
            Ok(response) => info_line(&response.into_inner()),
            Err(status) => status_line(&status),
        }
    }

    async fn do_get(client: &mut FlightServiceClient<Channel>, request: Request<Ticket>) -> String {
        let mut stream = match client.do_get(request).await {
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
        let batches = arrow_flight::utils::flight_data_to_batches(&messages).expect("decode");
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

    async fn do_put(client: &mut FlightServiceClient<Channel>, authorized: bool) -> String {
        let request = Request::new(futures::stream::iter(vec![FlightData::default()]));
        let request = if authorized {
            with_valid_bearer(request)
        } else {
            request
        };
        match client.do_put(request).await {
            Ok(_) => "accepted".to_owned(),
            Err(status) => status_line(&status),
        }
    }

    #[tokio::test]
    async fn list_get_info_do_get_and_do_put_answer_as_before_the_shared_server() {
        let root = tempfile::tempdir().expect("tempdir");
        std::fs::write(root.path().join("minimal.pptx"), FIXTURE).expect("fixture");
        std::fs::write(root.path().join("notes.txt"), b"not a document").expect("other file");
        std::fs::create_dir(root.path().join("sub")).expect("subdirectory");
        std::fs::write(root.path().join("sub").join("Upper.PPTX"), FIXTURE).expect("nested");
        let mut client = client_for(root.path()).await;
        let descriptor = |path: &str| FlightDescriptor::new_path(vec![path.to_owned()]);
        let ticket = |path: &str| Ticket::new(path.to_owned());

        let mut seen = String::new();
        let mut record = |call: &str, answer: String| {
            writeln!(seen, "{call}\n    {answer}").expect("write to a string");
        };

        record(
            "ListFlights without a bearer",
            list_flights(&mut client, Request::new(Criteria::default())).await,
        );
        record(
            "ListFlights",
            list_flights(
                &mut client,
                with_valid_bearer(Request::new(Criteria::default())),
            )
            .await,
        );
        record(
            "GetFlightInfo without a bearer",
            get_flight_info(&mut client, Request::new(descriptor("minimal.pptx"))).await,
        );
        for path in [
            "minimal.pptx",
            "minimal.pptx?table=paragraphs",
            "minimal.pptx?table=no_such_table",
            "sub/Upper.PPTX",
            "missing.pptx",
            "../minimal.pptx",
        ] {
            record(
                &format!("GetFlightInfo {path}"),
                get_flight_info(
                    &mut client,
                    with_valid_bearer(Request::new(descriptor(path))),
                )
                .await,
            );
        }
        record(
            "DoGet without a bearer",
            do_get(&mut client, Request::new(ticket("minimal.pptx"))).await,
        );
        for path in [
            "minimal.pptx",
            "minimal.pptx?table=paragraphs",
            "minimal.pptx?table=paragraphs&batch=1",
            "minimal.pptx?table=no_such_table",
            "sub/Upper.PPTX",
            "notes.txt",
            "missing.pptx",
            "../minimal.pptx",
        ] {
            record(
                &format!("DoGet {path}"),
                do_get(&mut client, with_valid_bearer(Request::new(ticket(path)))).await,
            );
        }
        record("DoPut without a bearer", do_put(&mut client, false).await);
        record("DoPut", do_put(&mut client, true).await);

        assert_eq!(seen, EXPECTED, "\n{seen}");
    }
}
