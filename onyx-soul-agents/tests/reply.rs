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
