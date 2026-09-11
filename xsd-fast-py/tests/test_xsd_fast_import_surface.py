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
