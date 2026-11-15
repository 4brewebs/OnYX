//! Códec de `Reply`: el contrato de ingress del decisor externo.

use onyx_soul_agents::{ConceptId, Decision, IngressEvent, OnyxEvent, REPLY_SOURCE, Reply, Verb};

#[test]
fn reply_codec_round_trips_payloads_with_tabs_and_newlines() {
    let reply = Reply::done(ConceptId(7), 3, "a\tb\nc");
    assert_eq!(Reply::from_bytes(&reply.to_bytes()), Ok(reply));

    let err = Reply::new(ConceptId(1), 2, Verb::ToolError, "bad");
    assert_eq!(Reply::from_bytes(&err.to_bytes()), Ok(err));
}

#[test]
fn reply_from_bytes_rejects_unknown_verb_missing_fields_and_non_utf8() {
    assert_eq!(
        Reply::from_bytes(b"7\t3\tdance\tx"),
        Err("malformed reply".into())
    );
    assert_eq!(Reply::from_bytes(b"nope"), Err("malformed reply".into()));
    assert_eq!(
        Reply::from_bytes(b"7\t3\tsay"),
        Err("malformed reply".into()),
        "sin payload (splitn 4) no es una respuesta"
    );
    assert_eq!(
        Reply::from_bytes(&[0xff, 0xfe]),
        Err("reply is not UTF-8".into())
    );
}

#[test]
fn reply_from_decision_preserves_call_args_and_empty_args() {
    let with_args = Reply::from_decision(
        ConceptId(1),
        4,
        Decision::Call {
            tool: "search".into(),
            args: "foo bar".into(),
        },
    );
    assert_eq!(with_args, Reply::call(ConceptId(1), 4, "search foo bar"));

    let no_args = Reply::from_decision(
        ConceptId(1),
        4,
        Decision::Call {
            tool: "search".into(),
            args: String::new(),
        },
    );
    assert_eq!(no_args, Reply::call(ConceptId(1), 4, "search"));
    assert_eq!(
        Reply::from_decision(ConceptId(2), 1, Decision::Say("hi".into())),
        Reply::say(ConceptId(2), 1, "hi")
    );
}

#[test]
fn reply_to_event_uses_fixed_source_independent_of_any_vocab_prefix() {
    let ev = Reply::say(ConceptId(9), 1, "ok").to_event();
    match ev {
        OnyxEvent::Ingress(IngressEvent::Raw { source, bytes }) => {
            assert_eq!(source, REPLY_SOURCE);
            assert_eq!(source, "agent.reply");
            assert_eq!(
                Reply::from_bytes(&bytes),
                Ok(Reply::say(ConceptId(9), 1, "ok"))
            );
        }
        other => panic!("se esperaba ingress Raw, llegó {other:?}"),
    }
}

#[test]
fn reply_v2_codec_round_trips_call_id_and_special_characters() {
    let reply = Reply::call_structured(
        ConceptId(5),
        12,
        "fs_write",
        "{\"path\":\"a b.rs\",\"body\":\"line1\\nline2\\t\"}",
    )
    .with_call_id("call:1\talpha");

    let bytes = reply.to_bytes();
    assert!(bytes.starts_with(b"v2\t"));
    assert_eq!(Reply::from_bytes(&bytes), Ok(reply));
}

#[test]
fn reply_v2_rejects_malformed_headers_and_lengths() {
    assert_eq!(
        Reply::from_bytes(b"v2\t1\t2\tsay"),
        Err("malformed reply".into())
    );
    assert_eq!(
        Reply::from_bytes(b"v2\t1\t2\tsay\tnonlen:abc\thi"),
        Err("malformed reply".into())
    );
    assert_eq!(
        Reply::from_bytes(b"v2\t1\t2\tsay\t99:short\thi"),
        Err("malformed reply".into())
    );
    assert_eq!(
        Reply::from_bytes(b"v2\t1\t2\tsay\t3:abcXhi"),
        Err("malformed reply".into())
    );
}

#[test]
fn reply_parse_call_payload_handles_structured_and_legacy() {
    let structured =
        Reply::call_structured(ConceptId(1), 1, "fs_edit", "  leading space\nand\tnewlines");
    assert_eq!(
        Reply::parse_call_payload(&structured.payload),
        ("fs_edit", "  leading space\nand\tnewlines")
    );
    assert_eq!(
        Reply::parse_call_payload("search foo bar"),
        ("search", "foo bar")
    );
    assert_eq!(Reply::parse_call_payload("ping"), ("ping", ""));
}
