use eve_marshal::{load, save, tags, Decoder, Error, StringTable, Value};

const DATA: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data");

fn table() -> StringTable {
    StringTable::parse(&std::fs::read(format!("{DATA}/strings.txt")).expect("strings.txt"))
}

fn vector(name: &str) -> Vec<u8> {
    std::fs::read(format!("{DATA}/{name}")).expect(name)
}

#[test]
fn string_table_matches_the_client_file() {
    let t = table();
    assert_eq!(t.len(), 81, "this build ships 81 entries");
    assert_eq!(t.get(1), Some(&b"*corpid"[..]), "indices are 1-based");
    assert_eq!(t.get(81), Some(&b"util.Rowset"[..]));
    assert_eq!(t.get(80), Some(&b"util.Row"[..]));
    assert_eq!(t.get(46), Some(&b"macho.CallReq"[..]));
    assert_eq!(t.get(0), None);
    assert_eq!(t.get(82), None);
    assert_eq!(t.index_of(b"util.Rowset"), Some(81));
    assert_eq!(t.index_of(b"macho.CallReq"), Some(46));
    assert_eq!(t.index_of(b"not in the table"), None);
}

#[test]
fn unknown_string_table_index_yields_empty_not_error() {
    let t = table();
    let buf = [b'~', 0, 0, 0, 0, tags::STRING_TABLE, 200];
    assert_eq!(load(&buf, &t).unwrap(), Value::Bytes(Vec::new()));
}

#[test]
fn decodes_every_real_vector() {
    let t = table();
    for name in ["user__.dat", "char__.dat", "pc.dat"] {
        let bytes = vector(name);
        assert_eq!(bytes[0], b'~', "{name} should be a marshal stream");
        let v = load(&bytes, &t).unwrap_or_else(|e| panic!("{name}: {e}"));
        let Value::Dict(sections) = &v else { panic!("{name}: root is {}", v.type_name()) };
        assert!(!sections.is_empty(), "{name}: no sections");
    }
}

#[test]
fn round_trips_real_vectors_byte_for_byte() {
    let t = table();
    for name in ["user__.dat", "char__.dat", "pc.dat"] {
        let original = vector(name);
        let value = load(&original, &t).unwrap_or_else(|e| panic!("{name}: {e}"));
        let reencoded = save(&value, &t).unwrap();
        assert_eq!(
            reencoded, original,
            "{name}: re-encode differs (len {} vs {})",
            reencoded.len(),
            original.len()
        );
    }
}

#[test]
fn user_settings_has_the_expected_shape() {
    let t = table();
    let v = load(&vector("user__.dat"), &t).unwrap();
    assert!(v.get_str("suppress").is_some());
    assert!(v.get_str("listgroups").is_some());
    let tabgroups = v.get_str("tabgroups").expect("tabgroups section");
    let Value::Dict(entries) = tabgroups else { panic!("not a dict") };
    for (_k, val) in entries {
        assert!(matches!(val, Value::Tuple(t) if t.len() == 2), "expected a 2-tuple");
    }
}

fn enc(v: &Value) -> Vec<u8> {
    save(v, &StringTable::empty()).unwrap()[5..].to_vec()
}

#[test]
fn integer_thresholds_match_the_writer() {
    assert_eq!(enc(&Value::Int(-1)), [tags::INT_MINUS1]);
    assert_eq!(enc(&Value::Int(0)), [tags::INT_0]);
    assert_eq!(enc(&Value::Int(1)), [tags::INT_1]);
    assert_eq!(enc(&Value::Int(2)), [tags::INT8, 2]);
    assert_eq!(enc(&Value::Int(127)), [tags::INT8, 0x7F]);
    assert_eq!(enc(&Value::Int(128)), [tags::INT16, 0x80, 0x00]);
    assert_eq!(enc(&Value::Int(-128)), [tags::INT8, 0x80]);
    assert_eq!(enc(&Value::Int(-129)), [tags::INT16, 0x7F, 0xFF]);
    assert_eq!(enc(&Value::Int(32767)), [tags::INT16, 0xFF, 0x7F]);
    assert_eq!(enc(&Value::Int(32768)), [tags::INT32, 0x00, 0x80, 0x00, 0x00]);
    assert_eq!(enc(&Value::Long(1)), [tags::INT64, 1, 0, 0, 0, 0, 0, 0, 0]);
}

#[test]
fn extsize_escape_is_at_255_not_256() {
    let s = Value::Bytes(vec![b'x'; 255]);
    let bytes = enc(&s);
    assert_eq!(bytes[0], tags::STRING_SHORT);
    assert_eq!(bytes[1], 0xFF);
    assert_eq!(bytes.len(), 2 + 255);

    let list = Value::List(vec![Value::None; 255]);
    let bytes = enc(&list);
    assert_eq!(bytes[0], tags::LIST);
    assert_eq!(&bytes[1..6], &[0xFF, 255, 0, 0, 0]);

    let list = Value::List(vec![Value::None; 254]);
    let bytes = enc(&list);
    assert_eq!(&bytes[0..2], &[tags::LIST, 254]);
}

#[test]
fn unicode_prefers_utf8_only_when_strictly_shorter() {
    assert_eq!(enc(&Value::unicode("hello"))[0], tags::UNICODE_UTF8);
    assert_eq!(enc(&Value::unicode("приве"))[0], tags::UNICODE_UCS2);
    assert_eq!(enc(&Value::unicode(""))[0], tags::UNICODE_EMPTY);
    assert_eq!(enc(&Value::unicode("a")), [tags::UNICODE_CHAR, b'a', 0]);
}

#[test]
fn string_table_hit_beats_every_length_rule() {
    let t = table();
    let bytes = save(&Value::bytes("util.Rowset"), &t).unwrap();
    assert_eq!(&bytes[5..], &[tags::STRING_TABLE, 81]);
}

#[test]
fn container_shortcuts() {
    assert_eq!(enc(&Value::Tuple(vec![]))[0], tags::TUPLE_0);
    assert_eq!(enc(&Value::Tuple(vec![Value::None]))[0], tags::TUPLE_1);
    assert_eq!(enc(&Value::Tuple(vec![Value::None; 2]))[0], tags::TUPLE_2);
    assert_eq!(enc(&Value::Tuple(vec![Value::None; 3]))[0], tags::TUPLE);
    assert_eq!(enc(&Value::List(vec![]))[0], tags::LIST_0);
    assert_eq!(enc(&Value::List(vec![Value::None]))[0], tags::LIST_1);
    assert_eq!(enc(&Value::List(vec![Value::None; 2]))[0], tags::LIST);
}

#[test]
fn dict_writes_value_before_key() {
    let d = Value::Dict(vec![(Value::bytes("k"), Value::Int(0))]);
    assert_eq!(enc(&d), [tags::DICT, 1, tags::INT_0, tags::STRING_CHAR, b'k']);
}

#[test]
fn encodes_the_low_level_version_challenge() {
    let t = table();
    let challenge = Value::Tuple(vec![
        Value::Int(170472),
        Value::Int(139),
        Value::Int(0),
        Value::Double(2.1),
        Value::Int(3913),
        Value::bytes("Blood"),
    ]);

    let bytes = save(&challenge, &t).unwrap();
    assert_eq!(&bytes[..7], &[b'~', 0, 0, 0, 0, tags::TUPLE, 6]);
    assert!(bytes.len() < 64, "challenge should be tiny, got {}", bytes.len());
    assert_eq!(load(&bytes, &t).unwrap(), challenge);
}

#[test]
fn rowset_instance_uses_a_string_table_class_identity() {
    let t = table();
    let rowset = Value::Instance {
        class: Box::new(Value::bytes("util.Rowset")),
        state: Box::new(Value::Dict(vec![(
            Value::bytes("header"),
            Value::List(vec![Value::bytes("itemID")]),
        )])),
    };
    let bytes = save(&rowset, &t).unwrap();
    assert_eq!(&bytes[5..9], &[tags::INSTANCE, tags::STRING_TABLE, 81, tags::DICT]);
    assert_eq!(load(&bytes, &t).unwrap(), rowset);
}

#[test]
fn marshal_stream_tag_has_no_type_object() {
    let t = table();
    let frame: &[u8] = &[
        0x7e, 0x00, 0x00, 0x00, 0x00, 0x1c, 0x53, 0x14, 0xe7, 0xc3, 0x17, 0x11, 0x2e, 0x14, 0x06,
        0x06, 0x06, 0x17, 0x11, 0x30, 0x14, 0x04, 0x0f, 0x43, 0x08, 0x03, 0x02, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x01, 0x17, 0x11, 0x30, 0x14, 0x03, 0x0f, 0x41, 0x11, 0x05, 0x01,
        0x06, 0x02, 0x2c, 0x2c, 0x08, 0x2b, 0x38, 0x7e, 0x00, 0x00, 0x00, 0x00, 0x14, 0x04, 0x09,
        0x10, 0x1c, 0x47, 0x65, 0x74, 0x50, 0x6f, 0x73, 0x74, 0x41, 0x75, 0x74, 0x68, 0x65, 0x6e,
        0x74, 0x69, 0x63, 0x61, 0x74, 0x69, 0x6f, 0x6e, 0x4d, 0x65, 0x73, 0x73, 0x61, 0x67, 0x65,
        0x24, 0x16, 0x01, 0x09, 0x10, 0x0c, 0x6d, 0x61, 0x63, 0x68, 0x6f, 0x56, 0x65, 0x72, 0x73,
        0x69, 0x6f, 0x6e, 0x01, 0x01,
    ];

    let (packet, had_crc) = eve_marshal::load_with_crc(frame, &t).expect("frame must decode");
    assert!(had_crc, "every client packet carries a marshal CRC");

    let Value::Instance { class, state } = &packet else { panic!("not an instance") };
    assert_eq!(class.as_bytes(), Some(&b"macho.CallReq"[..]));
    let fields = state.seq().unwrap();
    assert_eq!(fields[0], Value::Int(6));
    assert_eq!(fields[3], Value::Int(2));

    let body = fields[4].seq().unwrap();
    let payload = body[0].seq().unwrap();
    assert_eq!(payload[0], Value::Int(0));
    let Value::MarshalStream(inner) = &payload[1] else {
        panic!("payload blob must be a MarshalStream, got {}", payload[1].type_name())
    };
    assert_eq!(inner.len(), 0x38, "the byte after the tag is a length, not a tag");

    let call = eve_marshal::load(inner, &t).expect("nested stream must decode");
    let parts = call.seq().unwrap();
    assert_eq!(parts[0], Value::Int(1));
    assert_eq!(parts[1], Value::bytes("GetPostAuthenticationMessage"));
    assert_eq!(parts[2], Value::Tuple(vec![]));
    assert_eq!(parts[3].get_str("machoVersion"), Some(&Value::Int(1)));

    assert_eq!(save(&packet, &t).unwrap().len() + 5, frame.len());
}

#[test]
fn resolves_shared_references() {
    let t = table();
    let mut buf = vec![b'~', 1, 0, 0, 0];
    buf.push(tags::TUPLE_2);
    buf.push(tags::STRING_SHORT | 0x40);
    buf.push(2);
    buf.extend_from_slice(b"hi");
    buf.push(tags::REFERENCE);
    buf.push(1);
    buf.extend_from_slice(&1u32.to_le_bytes());

    let v = load(&buf, &t).unwrap();
    assert_eq!(v, Value::Tuple(vec![Value::bytes("hi"), Value::bytes("hi")]));
}

#[test]
fn rejects_bogus_map_entries() {
    let t = table();
    let mut buf = vec![b'~', 1, 0, 0, 0, tags::NONE];
    buf.extend_from_slice(&5u32.to_le_bytes());
    assert_eq!(load(&buf, &t), Err(Error::BogusMap));
}

#[test]
fn rejects_map_larger_than_buffer() {
    let t = table();
    let buf = [b'~', 0xFF, 0xFF, 0xFF, 0x7F, tags::NONE];
    assert!(matches!(load(&buf, &t), Err(Error::TooLittleData { .. })));
}

#[test]
fn non_magic_buffer_is_a_pickle_not_an_error() {
    let t = table();
    assert_eq!(load(b"(dp1\n.", &t).unwrap(), Value::Pickle(b"(dp1\n.".to_vec()));
}

#[test]
fn checksum_record_is_transparent() {
    let t = table();
    let mut buf = vec![b'~', 0, 0, 0, 0, tags::CHECKSUM];
    buf.extend_from_slice(&0xDEADBEEFu32.to_le_bytes());
    buf.push(tags::INT_1);
    let mut d = Decoder::new(&buf, &t).unwrap();
    assert_eq!(d.root().unwrap(), Value::Int(1));
    assert!(d.had_crc, "packetHadCrc should be set");
    assert_eq!(d.crc, Some(0xDEAD_BEEF));
}

#[test]
fn invalid_and_unsupported_tags_error_cleanly() {
    let t = table();
    for tag in [0x00u8, 0x0C, 0x1D, 0x1E, 0x2F, 0x3F] {
        let buf = [b'~', 0, 0, 0, 0, tag];
        assert!(matches!(load(&buf, &t), Err(Error::InvalidTag(_))), "tag {tag:#04x}");
    }
    for tag in [tags::DBROW, 0x22, 0x23] {
        let buf = [b'~', 0, 0, 0, 0, tag];
        assert!(
            matches!(load(&buf, &t), Err(Error::UnsupportedTag { .. })),
            "tag {tag:#04x}"
        );
    }
}

#[test]
fn deep_nesting_is_rejected_not_stack_overflowed() {
    let t = table();
    let mut buf = vec![b'~', 0, 0, 0, 0];
    buf.extend(std::iter::repeat(tags::TUPLE_1).take(5000));
    buf.push(tags::NONE);
    assert_eq!(load(&buf, &t), Err(Error::DepthLimit));
}

#[test]
fn huge_declared_count_does_not_allocate_wildly() {
    let t = table();
    let buf = [b'~', 0, 0, 0, 0, tags::LIST, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF];
    assert!(matches!(load(&buf, &t), Err(Error::UnexpectedEof { .. })));
}

#[test]
fn truncated_streams_never_panic() {
    let t = table();
    for name in ["user__.dat", "char__.dat", "pc.dat"] {
        let full = vector(name);
        for cut in 0..full.len() {
            let _ = load(&full[..cut], &t);
        }
    }
}

#[test]
fn corrupted_streams_never_panic() {
    let t = table();
    let full = vector("user__.dat");
    for i in 0..full.len() {
        for bit in 0..8 {
            let mut bad = full.clone();
            bad[i] ^= 1 << bit;
            let _ = load(&bad, &t);
        }
    }
}
