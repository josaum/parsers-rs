import json
import pytest

email_fast = pytest.importorskip("email_fast")

ATTACHMENT = (
    b"Content-Type: application/octet-stream\r\n"
    b'Content-Disposition: attachment; filename="../payload.bin"\r\n'
    b"Content-Transfer-Encoding: base64\r\n\r\naGVsbG8=\r\n"
)


def test_byte_api_forwards_aggregate_budget():
    with pytest.raises(RuntimeError, match="budget"):
        email_fast.parse_email_bytes(
            ATTACHMENT, format="eml", max_total_attachment_bytes=1
        )
    parsed = email_fast.parse_email_bytes(
        ATTACHMENT, format="eml", max_attachment_size=5
    )
    assert parsed["attachments"][0]["size"] == 5


@pytest.mark.parametrize(
    "option", ["max_inflate", "max_header_bytes", "max_mime_parts", "max_mime_depth"]
)
def test_limits_reach_native_path_and_bytes(option, tmp_path):
    source = tmp_path / "message.eml"
    source.write_bytes(ATTACHMENT)
    for call in [
        lambda: email_fast.parse_email(str(source), **{option: 0}),
        lambda: email_fast.parse_email_bytes(ATTACHMENT, format="eml", **{option: 0}),
    ]:
        with pytest.raises(RuntimeError):
            call()


def test_extraction_is_contained_and_preserves_existing_files(tmp_path):
    source = tmp_path / "message.eml"
    source.write_bytes(ATTACHMENT)
    output = tmp_path / "attachments"
    output.mkdir()
    (output / "payload.bin").write_bytes(b"existing")
    paths = email_fast.extract_attachments(str(source), str(output))
    assert len(paths) == 1
    from pathlib import Path

    extracted = Path(paths[0])
    assert extracted.parent == output
    assert extracted.read_bytes() == b"hello"
    assert (output / "payload.bin").read_bytes() == b"existing"
    assert not (tmp_path / "payload.bin").exists()


def test_mbox_encoding_and_failures(tmp_path):
    source = tmp_path / "mail.mbox"
    output = tmp_path / "mail.ndjson"
    prefix = b"From a@example.com Mon Jan 1 00:00:00 2024\n"
    source.write_bytes(
        prefix + b"Content-Type: text/plain; charset=iso-8859-1\n\ncaf\xe9\n"
    )
    assert email_fast.parse_mbox_ndjson(str(source), str(output)) == 1
    assert json.loads(output.read_text())["body"]["text"].strip() == "café"
    source.write_bytes(prefix + ATTACHMENT)
    with pytest.raises(RuntimeError, match="offset"):
        email_fast.parse_mbox_ndjson(
            str(source), str(output), max_total_attachment_bytes=1
        )


def test_quoted_address_and_literal_encoded_word():
    parsed = email_fast.parse_email_bytes(
        b'To: "Doe, Jane" <jane@example.com>\r\nSubject: =?UTF-8?Q?=3D=3FUTF-8=3FQ=3Fhello=3F=3D?=\r\n\r\nbody',
        format="eml",
    )
    assert parsed["to"] == [{"name": "Doe, Jane", "address": "jane@example.com"}]
    assert parsed["subject"] == "=?UTF-8?Q?hello?="
