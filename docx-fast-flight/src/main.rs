//! The `.docx` Flight server. The server itself is
//! `flight_ipc_config::table_server`, shared with the other OOXML server;
//! this file says only what is particular to `.docx`.
use std::path::Path;

use arrow_array::RecordBatch;
use arrow_schema::SchemaRef;
use docx_fast_core::arrow::{self, ArrowOptions};
use docx_fast_core::ParseOptions;
use flight_ipc_config::table_server::{self, TableParser};
use flight_ipc_config::ParserRoute;

#[cfg(test)]
#[path = "../../flight-ipc-config/test-support/mod.rs"]
mod flight_auth_test_support;

#[cfg(test)]
use arrow_flight::flight_service_server::FlightService;
#[cfg(test)]
use arrow_flight::{Action, Empty, FlightDescriptor};
#[cfg(test)]
use arrow_ipc::writer::IpcWriteOptions;
#[cfg(test)]
use flight_ipc_config::{do_put_unsupported, CAPABILITY_ACTION};
#[cfg(test)]
use std::path::PathBuf;
#[cfg(test)]
use tonic::{Request, Status};

/// What `.docx` contributes to the shared server.
struct Docx;

impl TableParser for Docx {
    type ParseOptions = ParseOptions;
    type ArrowOptions = ArrowOptions;

    const ROUTE: ParserRoute = ParserRoute::Docx;
    const SERVER_NAME: &'static str = "docx-fast-flight";
    const DEFAULT_BIND: &'static str = "0.0.0.0:50052";
    const DEFAULT_TABLE: &'static str = "paragraphs";
    const MISSING_DEFAULT_SCHEMA: &'static str = "missing schema for default docx table";
    const EXTENSIONS: &'static [&'static str] = &["docx", "docm", "dotx", "dotm"];

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
        arrow::parse_docx_to_arrow_table_batches(path, parse_options, arrow_options, table, emit)
    }
}

#[cfg(test)]
type DocxFlightService = table_server::TableFlightService<Docx>;

/// The name the tests below have always called the listing walk by.
#[cfg(test)]
fn collect_docx_paths(root: &Path, limit: usize) -> Result<Vec<PathBuf>, Status> {
    table_server::collect_paths::<Docx>(root, limit)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    table_server::run::<Docx>().await
}

#[cfg(test)]
mod symlink_listing_tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::symlink as symlink_file;
    #[cfg(windows)]
    use std::os::windows::fs::symlink_file;

    #[test]
    fn collect_docx_paths_skips_symlinks() {
        // WHY: listing must not follow symlinks that could escape the Flight
        // root; canonicalize on DoGet is not enough if ListFlights advertises
        // the linked path.
        let root = tempfile::tempdir().expect("tempdir");
        let outside = tempfile::tempdir().expect("outside");
        let real = outside.path().join("escape.docx");
        std::fs::write(&real, b"PK").expect("write");
        let link = root.path().join("escape.docx");
        if let Err(err) = symlink_file(&real, &link) {
            #[cfg(windows)]
            if err.raw_os_error() == Some(1314) {
                return;
            }
            panic!("symlink: {err}");
        }
        let inside = root.path().join("ok.docx");
        std::fs::write(&inside, b"PK").expect("write inside");

        let found = collect_docx_paths(root.path(), 100).expect("collect");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0], inside);
    }
}

#[cfg(test)]
mod read_only_flight_contract_tests {
    use super::*;
    use crate::flight_auth_test_support::{required_auth_verifier, with_valid_bearer};

    fn capability_test_service() -> DocxFlightService {
        DocxFlightService {
            // A directory that exists: a root that cannot be resolved is an
            // internal error, not a missing file.
            root: std::env::temp_dir(),
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
                "__missing_auth_probe__.docx".to_owned(),
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
            "__missing_auth_probe__.docx".to_owned(),
        ])));
        let Err(error) = FlightService::get_schema(&service, request).await else {
            panic!("missing test file must reach path validation");
        };
        assert_eq!(error.code(), tonic::Code::NotFound);
    }
}

#[cfg(all(test, unix))]
#[path = "../../flight-ipc-config/test-support/wire.rs"]
mod flight_wire_test_support;

#[cfg(all(test, unix))]
mod wire_parity_tests {
    //! What a Flight client sees from this server, pinned as text in
    //! `tests/wire_transcript.txt`.
    //!
    //! A change to any answer in the transcript is a change in behaviour,
    //! not in structure. The fixture is the minimal document
    //! `docx-fast-core`'s own tests build (`tests/parser_integration.rs`),
    //! written as a stored archive. Unix only: the probes include symbolic
    //! links.
    use super::*;
    use crate::flight_auth_test_support::required_auth_verifier;
    use crate::flight_wire_test_support::Wire;

    const FIXTURE: &[u8] = include_bytes!("../tests/fixtures/minimal.docx");

    const EXPECTED: &str = include_str!("../tests/wire_transcript.txt");

    fn service(root: &Path) -> DocxFlightService {
        DocxFlightService {
            root: root.to_path_buf(),
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

    #[tokio::test]
    async fn a_path_outside_the_root_is_answered_like_a_missing_one() {
        let base = tempfile::tempdir().expect("tempdir");
        let root = base.path().join("root");
        std::fs::create_dir_all(&root).expect("directories");
        let mut wire = Wire::start(service(&root), base.path()).await;
        wire.assert_outside_paths_look_missing(base.path(), "docx")
            .await;
    }

    #[tokio::test]
    async fn list_get_info_do_get_and_do_put_answer_as_before_the_shared_server() {
        let base = tempfile::tempdir().expect("tempdir");
        let root = base.path().join("root");
        std::fs::create_dir_all(root.join("sub")).expect("directories");
        std::fs::write(root.join("minimal.docx"), FIXTURE).expect("fixture");
        std::fs::write(root.join("notes.txt"), b"not a document").expect("other file");
        std::fs::write(root.join("sub").join("Upper.DOCX"), FIXTURE).expect("nested");
        let mut wire = Wire::start(service(&root), base.path()).await;

        wire.list_flights(false).await;
        wire.list_flights(true).await;
        wire.get_flight_info("minimal.docx", false).await;
        for path in [
            "minimal.docx",
            "minimal.docx?table=metadata",
            "minimal.docx?table=no_such_table",
            "sub/Upper.DOCX",
            "missing.docx",
            "../minimal.docx",
        ] {
            wire.get_flight_info(path, true).await;
        }
        wire.do_get(b"minimal.docx", false).await;
        for path in [
            "minimal.docx",
            "minimal.docx?table=metadata",
            "minimal.docx?table=paragraphs&batch=1",
            "minimal.docx?table=no_such_table",
            "sub/Upper.DOCX",
            "notes.txt",
            "missing.docx",
            "../minimal.docx",
        ] {
            wire.do_get(path.as_bytes(), true).await;
        }
        wire.do_put(false).await;
        wire.do_put(true).await;
        wire.path_probes(&root, base.path(), "docx").await;

        assert_eq!(wire.seen, EXPECTED, "\n<<<WIRE\n{}WIRE>>>", wire.seen);
        assert_eq!(
            wire.answers_naming_server_paths,
            Vec::<String>::new(),
            "an answer named a directory on the server"
        );
    }
}
