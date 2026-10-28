use eve_marshal::{StringTable, Value};
use eve_proto::{
    decode_payload, encode_frame, encode_payload, frame, llv, packet, decompress, FrameReader,
    MachoAddress, MachoPacket, MessageType, ProtoError, VersionChallenge,
};

const TABLE_TXT: &str =
    concat!(env!("CARGO_MANIFEST_DIR"), "/../eve-marshal/tests/data/strings.txt");

fn table() -> StringTable {
    StringTable::parse(&std::fs::read(TABLE_TXT).expect("strings.txt"))
}

#[test]
fn frames_round_trip() {
    let payload = b"~\x00\x00\x00\x00\x08";
    let framed = encode_frame(payload).unwrap();
    assert_eq!(&framed[..4], &(payload.len() as u32).to_le_bytes());

    let mut r = FrameReader::new();
    r.push(&framed);
    assert_eq!(r.next_frame().unwrap().as_deref(), Some(&payload[..]));
    assert_eq!(r.next_frame().unwrap(), None);
}

#[test]
fn frame_reader_handles_split_and_coalesced_reads() {
    let a = encode_frame(b"first").unwrap();
    let b = encode_frame(b"second").unwrap();
    let mut stream = a.clone();
    stream.extend_from_slice(&b);

    let mut r = FrameReader::new();
    let mut got: Vec<Vec<u8>> = Vec::new();
    for byte in &stream {
        r.push(&[*byte]);
        while let Some(f) = r.next_frame().unwrap() {
            got.push(f);
        }
    }
    assert_eq!(got, vec![b"first".to_vec(), b"second".to_vec()]);

    let mut r = FrameReader::new();
    r.push(&stream);
    assert_eq!(r.next_frame().unwrap().unwrap(), b"first");
    assert_eq!(r.next_frame().unwrap().unwrap(), b"second");
    assert_eq!(r.next_frame().unwrap(), None);
    assert_eq!(r.buffered(), 0);
}

#[test]
fn frame_length_bounds_match_the_client() {
    let mut r = FrameReader::new();
    r.push(&0u32.to_le_bytes());
    assert_eq!(r.next_frame(), Err(ProtoError::EmptyFrame));

    // The client sends frames past what it accepts (portrait uploads); only
    // the memory guard refuses one.
    let mut r = FrameReader::new();
    r.push(&(frame::MAX_FRAME_LEN + 1).to_le_bytes());
    assert_eq!(r.next_frame().unwrap(), None);

    let mut r = FrameReader::new();
    r.push(&(frame::MAX_INCOMING_FRAME_LEN + 1).to_le_bytes());
    assert_eq!(r.next_frame(), Err(ProtoError::FrameTooLarge(frame::MAX_INCOMING_FRAME_LEN + 1)));

    assert_eq!(encode_frame(b""), Err(ProtoError::EmptyFrame));
    let big = vec![b'~'; frame::MAX_FRAME_LEN as usize + 1];
    assert_eq!(encode_frame(&big), Err(ProtoError::FrameTooLarge(frame::MAX_FRAME_LEN + 1)));
}

#[test]
fn compression_sentinel_is_the_marshal_magic() {
    let table = table();
    let value = Value::List(vec![Value::bytes("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"); 200]);
    let raw = eve_marshal::save(&value, &table).unwrap();
    let squashed = frame::maybe_compress(&raw, 0);

    assert!(squashed.len() < raw.len(), "should have compressed");
    assert_ne!(squashed[0], b'~', "compressed payload must not start with the magic");
    assert_eq!(decompress(&squashed).unwrap(), raw);
    assert_eq!(decompress(&raw).unwrap(), raw);
}

#[test]
fn small_payloads_are_left_uncompressed() {
    let small = vec![b'~'; 150];
    assert_eq!(frame::maybe_compress(&small, 0), small);
    let big = vec![b'~'; 5000];
    assert_eq!(frame::maybe_compress(&big, 4000), big);
}

#[test]
fn address_shapes_round_trip() {
    let cases = vec![
        MachoAddress::Client {
            client_id: Some(42),
            call_id: Some(7),
            service: Some(b"objectCaching".to_vec()),
        },
        MachoAddress::Node { node_id: Some(1), service: Some(b"machoNet".to_vec()), call_id: None },
        MachoAddress::Any { service: Some(b"authentication".to_vec()), call_id: Some(3) },
        MachoAddress::Broadcast {
            broadcast_id: Value::bytes("OnItemChange"),
            narrowcast: Value::List(vec![Value::Int(1)]),
            id_type: Value::bytes("clientID"),
        },
    ];
    for addr in cases {
        let v = addr.to_value();
        assert_eq!(MachoAddress::from_value(&v).unwrap(), addr, "{addr:?}");
    }
}

#[test]
fn client_and_node_addresses_swap_service_and_callid() {
    let c = MachoAddress::Client {
        client_id: Some(10),
        call_id: Some(20),
        service: Some(b"svc".to_vec()),
    };
    let items = c.to_value();
    let Value::Instance { state, .. } = &items else { panic!("not an instance") };
    let t = state.seq().unwrap();
    assert_eq!(t[0], Value::bytes("C"));
    assert_eq!(t[1], Value::Int(10));
    assert_eq!(t[2], Value::Int(20));
    assert_eq!(t[3], Value::bytes("svc"));

    let n = MachoAddress::Node {
        node_id: Some(10),
        service: Some(b"svc".to_vec()),
        call_id: Some(20),
    };
    let Value::Instance { state, .. } = n.to_value() else { panic!() };
    let t = state.seq().unwrap();
    assert_eq!(t[0], Value::bytes("N"));
    assert_eq!(t[1], Value::Int(10));
    assert_eq!(t[2], Value::bytes("svc"));
    assert_eq!(t[3], Value::Int(20));
}

#[test]
fn any_address_has_arity_three() {
    let a = MachoAddress::Any { service: Some(b"s".to_vec()), call_id: None };
    let Value::Instance { state, .. } = a.to_value() else { panic!() };
    assert_eq!(state.seq().unwrap().len(), 3);
}

#[test]
fn address_guid_uses_the_string_table() {
    let table = table();
    let a = MachoAddress::Any { service: None, call_id: None };
    let bytes = eve_marshal::save(&a.to_value(), &table).unwrap();
    assert_eq!(&bytes[5..8], &[0x17, 0x11, 48]);
}

#[test]
fn packet_round_trips_through_marshal() {
    let table = table();
    let mut p = MachoPacket::call_req(Value::Tuple(vec![Value::bytes("ping")]));
    p.source = Some(MachoAddress::Client { client_id: Some(5), call_id: Some(1), service: None });
    p.destination = Some(MachoAddress::Any { service: Some(b"machoNet".to_vec()), call_id: None });
    p.user_id = Some(99);

    let framed = encode_payload(&p.to_value().unwrap(), &table).unwrap();
    let mut r = FrameReader::new();
    r.push(&framed);
    let payload = r.next_frame().unwrap().unwrap();
    let value = decode_payload(&payload, &table).unwrap();
    assert_eq!(MachoPacket::from_value(&value).unwrap(), p);
}

#[test]
fn call_req_and_notification_bodies_are_always_two_tuples() {
    let p = MachoPacket::call_req(Value::Int(1));
    assert_eq!(p.body, vec![Value::Int(1), Value::None]);
    p.validate().unwrap();

    let n = MachoPacket::notification(Value::Int(1));
    assert_eq!(n.body, vec![Value::Int(1), Value::None]);
    n.validate().unwrap();

    let r = MachoPacket::call_rsp(Value::Int(1));
    assert_eq!(r.body.len(), 1);
    r.validate().unwrap();

    let bad = MachoPacket::new(MessageType::CallReq, vec![Value::Int(1)]);
    assert!(matches!(bad.validate(), Err(ProtoError::BadArity { got: 1, .. })));
}

#[test]
fn session_change_notification_accepts_one_or_two_element_bodies() {
    let change = Value::Tuple(vec![Value::Int(0), Value::Dict(vec![])]);
    let two = MachoPacket::new(
        MessageType::SessionChangeNotification,
        vec![change.clone(), Value::List(vec![])],
    );
    two.validate().unwrap();
    let one = MachoPacket::new(MessageType::SessionChangeNotification, vec![change]);
    one.validate().unwrap();
}

#[test]
fn auth_request_may_omit_its_trailing_address() {
    let three = MachoPacket::new(
        MessageType::AuthenticationReq,
        vec![Value::Dict(vec![]), Value::bytes("user"), Value::bytes("pw")],
    );
    three.validate().unwrap();
    let four = MachoPacket::new(
        MessageType::AuthenticationReq,
        vec![Value::Dict(vec![]), Value::bytes("user"), Value::bytes("pw"), Value::None],
    );
    four.validate().unwrap();
    let five = MachoPacket::new(MessageType::AuthenticationReq, vec![Value::None; 5]);
    assert!(five.validate().is_err());
}

#[test]
fn pinned_channels_appear_in_oob() {
    let p = MachoPacket::new(
        MessageType::AuthenticationReq,
        vec![Value::Dict(vec![]), Value::bytes("u"), Value::bytes("p")],
    );
    let v = p.to_value().unwrap();
    let Value::Instance { state, .. } = &v else { panic!() };
    let oob = &state.seq().unwrap()[5];
    assert_eq!(oob.get_str("channel"), Some(&Value::bytes("authentication")));

    let c = MachoPacket::call_req(Value::None);
    let Value::Instance { state, .. } = c.to_value().unwrap() else { panic!() };
    assert_eq!(state.seq().unwrap()[5], Value::None);
}

#[test]
fn response_swaps_source_and_destination() {
    let mut req = MachoPacket::call_req(Value::None);
    req.source = Some(MachoAddress::Client { client_id: Some(5), call_id: Some(77), service: None });
    req.destination = Some(MachoAddress::Any { service: Some(b"svc".to_vec()), call_id: None });
    req.user_id = Some(3);

    let rsp = req.respond(MessageType::CallRsp, vec![Value::Int(1)]);
    assert_eq!(rsp.source, req.destination);
    assert_eq!(rsp.destination, req.source);
    assert_eq!(rsp.user_id, Some(3));
    assert_eq!(rsp.destination.as_ref().unwrap().call_id(), Some(77));

    let err = req.error_response(packet::MACHONETERR_WRAPPEDEXCEPTION, Value::bytes("boom"));
    assert_eq!(err.command, MessageType::ErrorResponse);
    assert_eq!(err.body[0], Value::Int(MessageType::CallReq.code()));
    assert_eq!(err.body[1], Value::Int(2));
}

#[test]
fn request_response_parity_and_client_allowlist() {
    assert!(MessageType::CallReq.is_request());
    assert!(!MessageType::CallRsp.is_request());
    for t in [MessageType::AuthenticationReq, MessageType::CallReq, MessageType::PingReq] {
        assert!(t.allowed_from_client(), "{t:?}");
    }
    for t in [
        MessageType::AuthenticationRsp,
        MessageType::SessionChangeNotification,
        MessageType::IdentificationReq,
    ] {
        assert!(!t.allowed_from_client(), "{t:?}");
    }
}

#[test]
fn guid_mismatch_is_rejected() {
    let state = Value::Tuple(vec![
        Value::Int(MessageType::CallReq.code()),
        Value::None,
        Value::None,
        Value::None,
        Value::Tuple(vec![Value::None, Value::None]),
        Value::None,
    ]);
    let lying = Value::Instance {
        class: Box::new(Value::bytes("macho.PingReq")),
        state: Box::new(state),
    };
    assert!(matches!(
        MachoPacket::from_value(&lying),
        Err(ProtoError::GuidMismatch { .. })
    ));
}

#[test]
fn version_challenge_matches_the_client() {
    let table = table();
    let server = VersionChallenge::server(1234);
    let bytes = eve_marshal::save(&server.to_value(), &table).unwrap();
    let back = VersionChallenge::from_value(&eve_marshal::load(&bytes, &table).unwrap()).unwrap();
    assert_eq!(back, server);
    back.validate().unwrap();

    assert_eq!(server.magic, 170472);
    assert_eq!(server.macho_version, 139);
    assert_eq!(server.boot_build, 3913);
    assert_eq!(server.codename, b"Blood");
    assert_eq!(server.boot_version, 2.1);

    assert_eq!(VersionChallenge::client().user_count, 0);
}

#[test]
fn version_challenge_rejects_wrong_shape_and_version() {
    let short = Value::Tuple(vec![Value::Int(llv::LLV_MAGIC); 5]);
    assert!(matches!(
        VersionChallenge::from_value(&short),
        Err(ProtoError::VersionMismatch(_))
    ));

    let mut wrong = VersionChallenge::server(0);
    wrong.macho_version = 120;
    assert!(matches!(wrong.validate(), Err(ProtoError::VersionMismatch(_))));

    let mut alien = VersionChallenge::server(0);
    alien.magic = 1;
    assert!(matches!(alien.validate(), Err(ProtoError::VersionMismatch(_))));
}

#[test]
fn llv_frame_is_uncompressed_and_small() {
    let table = table();
    let payload = eve_marshal::save(&VersionChallenge::server(0).to_value(), &table).unwrap();
    assert_eq!(payload[0], b'~', "the LLV frame is never zlib-wrapped");
    assert!(payload.len() < llv::MAX_CLIENT_LLV_BYTES);
}

#[test]
fn close_reason_is_a_bare_string_and_round_trips() {
    let table = table();
    let raw = llv::close_reason("There was a low-level protocol version mismatch.", &table).unwrap();

    let err = decode_payload(&raw, &table).unwrap_err();
    match err {
        ProtoError::PeerClosed { reason, bootver } => {
            assert_eq!(reason, "There was a low-level protocol version mismatch.");
            let (version, build, codename) = bootver.expect("bootver should parse");
            assert_eq!(version, 2.1);
            assert_eq!(build, 3913);
            assert_eq!(codename, b"Blood");
        }
        other => panic!("expected PeerClosed, got {other}"),
    }
}
