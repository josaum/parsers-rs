import pdf_fast


def test_pdf_wheel_exposes_text_parser():
    assert callable(pdf_fast.parse_pdf_text)
    assert callable(pdf_fast.parse_pdf_text_bytes)
    assert "parse_pdf_text_bytes" in pdf_fast.__all__
