//! `FIX-31` formato wire: la época de sesión viaja en el `Reply` (v3) y la
//! compatibilidad con v1/v2 se mantiene.
//!
//! v1: `agent \t request \t verb \t payload`
//! v2: `v2 \t agent \t request \t verb \t len:call_id \t payload`
//! v3: `v3 \t agent \t request \t verb \t epoch \t len:call_id \t payload`
//! (el slot de `call_id` va SIEMPRE presente: un cid vacío es `0:` → `None`).
//!
//! Solo API pública de `onyx-soul-agents`.

use onyx_soul_agents::{ConceptId, Reply};

#[test]
fn v1_and_v2_roundtrip_without_epoch() {
    let v1 = Reply::say(ConceptId(7), 3, "hola");
    assert_eq!(v1.epoch, None);
    assert!(!v1.to_bytes().starts_with(b"v"));
    assert_eq!(Reply::from_bytes(&v1.to_bytes()).unwrap(), v1);

    let v2 = Reply::call(ConceptId(7), 3, "tool x").with_call_id("cid-1");
    assert_eq!(v2.epoch, None);
    assert!(v2.to_bytes().starts_with(b"v2\t"));
    assert_eq!(Reply::from_bytes(&v2.to_bytes()).unwrap(), v2);
}

#[test]
fn v3_roundtrip_with_and_without_call_id() {
    let a = Reply::done(ConceptId(2), 5, "fin").with_epoch(4);
    let bytes = a.to_bytes();
    assert!(bytes.starts_with(b"v3\t"));
    let rt = Reply::from_bytes(&bytes).unwrap();
    assert_eq!(rt, a);
    assert_eq!(rt.epoch, Some(4));
    assert_eq!(rt.call_id, None);

    let b = Reply::observe(ConceptId(2), 5, "out")
        .with_call_id("c9")
        .with_epoch(0);
    let rt = Reply::from_bytes(&b.to_bytes()).unwrap();
    assert_eq!(rt, b);
    assert_eq!(rt.call_id.as_deref(), Some("c9"));
    assert_eq!(rt.epoch, Some(0));
}

#[test]
fn v3_without_call_id_keeps_uniform_slot() {
    // Slot uniforme sin ambigüedad: cid vacío → `0:\t` → `None` al parsear.
    let a = Reply::say(ConceptId(1), 1, "x").with_epoch(9);
    let bytes = a.to_bytes();
    let s = std::str::from_utf8(&bytes).unwrap();
    let mut parts = s.splitn(6, '\t');
    assert_eq!(parts.next(), Some("v3"));
    assert_eq!(parts.next(), Some("1"));
    assert_eq!(parts.next(), Some("1"));
    assert_eq!(parts.next(), Some("say"));
    assert_eq!(parts.next(), Some("9"));
    assert_eq!(parts.next(), Some("0:\tx"));
    assert_eq!(Reply::from_bytes(&a.to_bytes()).unwrap().call_id, None);
}

#[test]
fn stripping_epoch_keeps_transcript_bytes_v1_v2() {
    // `FIX-31` (c): el puente graba una copia con `epoch = None`; el byte
    // resultante es idéntico al formato v1/v2 (transcripciones compatibles).
    let v3 = Reply::call(ConceptId(3), 2, "tool y")
        .with_call_id("k")
        .with_epoch(7);
    let mut stripped = v3.clone();
    stripped.epoch = None;
    let plain = Reply::call(ConceptId(3), 2, "tool y").with_call_id("k");
    assert_eq!(stripped.to_bytes(), plain.to_bytes());

    let v3b = Reply::done(ConceptId(3), 2, "fin").with_epoch(7);
    let mut stripped_b = v3b.clone();
    stripped_b.epoch = None;
    let plain_b = Reply::done(ConceptId(3), 2, "fin");
    assert_eq!(stripped_b.to_bytes(), plain_b.to_bytes());
}

#[test]
fn v3_malformed_is_rejected() {
    // Falta la época.
    assert!(Reply::from_bytes(b"v3\t1\t2\tsay\tpayload").is_err());
    // Época no numérica.
    assert!(Reply::from_bytes(b"v3\t1\t2\tsay\tnotanumber\t0:\tpayload").is_err());
    // Longitud de `call_id` incoherente con el slot.
    assert!(Reply::from_bytes(b"v3\t1\t2\tsay\t1\t5:ab\tpayload").is_err());
}
