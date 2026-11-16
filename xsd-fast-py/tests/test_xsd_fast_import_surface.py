import xsd_fast


SIMPLE_XSD = b"""\
<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema">
  <xs:element name="record">
    <xs:complexType>
      <xs:sequence>
        <xs:element name="name" type="xs:string"/>
      </xs:sequence>
    </xs:complexType>
  </xs:element>
</xs:schema>
"""


def test_xsd_wheel_exposes_introspection_and_validation():
    assert callable(xsd_fast.list_elements)
    assert callable(xsd_fast.validate)


def test_xsd_wheel_introspects_schema_bytes_without_filesystem_access():
    assert xsd_fast.list_elements_bytes(SIMPLE_XSD) == ["record"]
    assert xsd_fast.describe_element_bytes(SIMPLE_XSD, "record") == [
        {"path": "name", "arrow_type": "Utf8", "nullable": False}
    ]


def _schema_path(tmp_path):
    path = tmp_path / "schema.xsd"
    path.write_bytes(SIMPLE_XSD)
    return str(path)


def test_path_entry_points_round_trip_a_schema_built_here(tmp_path):
    schema = _schema_path(tmp_path)
    assert xsd_fast.list_elements(schema) == ["record"]
    assert xsd_fast.describe_element(schema, "record") == xsd_fast.describe_element_bytes(
        SIMPLE_XSD, "record"
    )
    good = b"<record><name>a</name></record>"
    bad = b"<record><other>a</other></record>"
    assert xsd_fast.validate_bytes(schema, good) == []
    errors = xsd_fast.validate_bytes(schema, bad, max_errors=5)
    assert errors and {"severity", "path", "message"} <= set(errors[0])
    xml = tmp_path / "doc.xml"
    xml.write_bytes(bad)
    assert xsd_fast.validate(schema, str(xml)) == xsd_fast.validate_bytes(schema, bad)


def test_all_arrow_schemas_describes_every_global_element(tmp_path):
    schema = _schema_path(tmp_path)
    assert xsd_fast.all_arrow_schemas(schema) == {
        "record": xsd_fast.describe_element(schema, "record")
    }
    assert xsd_fast.all_arrow_schemas(schema, max_depth=4, map_dates=False) == {
        "record": [{"path": "name", "arrow_type": "Utf8", "nullable": False}]
    }


def test_schema_ipc_bytes_is_an_arrow_stream_pyarrow_reads(tmp_path):
    import pytest

    ipc = pytest.importorskip("pyarrow.ipc")
    payload = xsd_fast.schema_ipc_bytes(_schema_path(tmp_path), "record")
    assert isinstance(payload, bytes) and payload
    table = ipc.open_stream(payload).read_all()
    assert table.num_rows == 1
    assert "name" in str(table.to_pylist())


def test_errors_are_runtime_errors(tmp_path):
    import pytest

    schema = _schema_path(tmp_path)
    missing = str(tmp_path / "missing.xsd")
    for call in (
        lambda: xsd_fast.list_elements(missing),
        lambda: xsd_fast.describe_element(missing, "record"),
        lambda: xsd_fast.all_arrow_schemas(missing),
        lambda: xsd_fast.schema_ipc_bytes(missing, "record"),
        lambda: xsd_fast.validate(schema, str(tmp_path / "missing.xml")),
        lambda: xsd_fast.validate_bytes(missing, b"<record/>"),
        lambda: xsd_fast.list_elements_bytes(b"<not-a-schema"),
        lambda: xsd_fast.describe_element(schema, "no_such_element"),
        lambda: xsd_fast.describe_element_bytes(SIMPLE_XSD, "no_such_element"),
    ):
        with pytest.raises(RuntimeError):
            call()
    with pytest.raises(TypeError):
        xsd_fast.list_elements_bytes("text, not bytes")
    with pytest.raises(TypeError):
        xsd_fast.validate_bytes(schema, "text, not bytes")


def test_two_threads_validate_at_once(tmp_path):
    import threading

    schema = _schema_path(tmp_path)
    results = [None, None]

    def work(slot):
        results[slot] = xsd_fast.validate_bytes(schema, b"<record><name>a</name></record>")

    threads = [threading.Thread(target=work, args=(slot,)) for slot in (0, 1)]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join(timeout=60)
    assert results == [[], []]


def test_the_wheel_ships_type_information_that_matches_the_module():
    import inspect
    from pathlib import Path

    assert (Path(xsd_fast.__file__).parent / "py.typed").is_file()
    public = {
        name
        for name in dir(xsd_fast)
        if not name.startswith("_") and inspect.isfunction(getattr(xsd_fast, name))
    }
    assert public == set(xsd_fast.__all__)
    for name in public:
        assert "return" in getattr(xsd_fast, name).__annotations__, name
