//! Pruebas de valores y adaptadores del puerto, no de conformidad de un soul.
use onyx_core::{
    Attr, AttrOnly, AttrRegistry, ConceptId, Graph, GraphView, OnyxError, ReadOnly, Value,
};

#[test]
fn value_equality_preserves_variants_and_contents() {
    let values = [
        Value::Bool(false),
        Value::Bool(true),
        Value::Int(0),
        Value::Int(1),
        Value::Float(0.0),
        Value::Float(1.0),
        Value::Text(String::new()),
        Value::Text("one".into()),
        Value::Concept(ConceptId(0)),
        Value::Concept(ConceptId(1)),
    ];
    for (i, left) in values.iter().enumerate() {
        assert_eq!(*left, left.clone());
        for (j, right) in values.iter().enumerate() {
            assert_eq!(left == right, i == j);
        }
    }
}

#[test]
fn float_equality_uses_identical_bits_including_nan_and_signed_zero() {
    fn requires_eq<T: Eq>(_: &T) {}

    let nan = Value::Float(f64::from_bits(0x7ff8_0000_0000_0001));
    let same_nan = Value::Float(f64::from_bits(0x7ff8_0000_0000_0001));
    let other_nan = Value::Float(f64::from_bits(0x7ff8_0000_0000_0002));
    let negative_nan = Value::Float(f64::from_bits(0xfff8_0000_0000_0001));
    requires_eq(&nan);
    assert_eq!(nan, nan);
    assert_eq!(nan, same_nan);
    assert_ne!(nan, other_nan);
    assert_ne!(nan, negative_nan);
    assert_ne!(Value::Float(0.0), Value::Float(-0.0));
    assert_eq!(Value::Float(f64::INFINITY), Value::Float(f64::INFINITY));
    assert_eq!(
        Value::Float(f64::NEG_INFINITY),
        Value::Float(f64::NEG_INFINITY)
    );
    assert_ne!(Value::Float(f64::INFINITY), Value::Float(f64::NEG_INFINITY));
}

#[test]
fn external_registry_implementations_can_construct_local_attr_ids() {
    const FIRST: Attr = Attr::from_raw(0);
    const LAST: Attr = Attr::from_raw(u32::MAX);
    assert_eq!(FIRST.raw(), 0);
    assert_eq!(LAST.raw(), u32::MAX);
    assert_ne!(FIRST, LAST);
}

#[test]
fn port_errors_implement_display_and_standard_error_without_losing_details() {
    let cases = [
        (OnyxError::Missing, "missing value"),
        (OnyxError::Type, "type mismatch"),
        (OnyxError::Io("disk unavailable".into()), "disk unavailable"),
        (
            OnyxError::Format("incompatible graph".into()),
            "incompatible graph",
        ),
    ];
    for (error, detail) in cases {
        let standard: &dyn std::error::Error = &error;
        assert!(standard.to_string().contains(detail));
        assert!(standard.source().is_none());
    }
}

// Puerto de prueba con respuestas conocidas para observar el reenvío.
// No implementa almacenamiento ni sirve como prueba de conformidad de Graph.
struct GraphProbe {
    names: Vec<String>,
}

impl GraphProbe {
    fn new() -> Self {
        Self {
            names: vec!["label".into(), "edge".into()],
        }
    }
}

impl AttrRegistry for GraphProbe {
    fn attr(&mut self, name: &str) -> Attr {
        let index = match self.names.iter().position(|existing| existing == name) {
            Some(index) => index,
            None => {
                self.names.push(name.into());
                self.names.len() - 1
            }
        };
        Attr::from_raw(u32::try_from(index).expect("registro de prueba pequeño"))
    }
}

impl GraphView for GraphProbe {
    fn exists(&self, id: ConceptId) -> bool {
        matches!(id, ConceptId(7 | 8))
    }
    fn get(&self, id: ConceptId, attr: Attr) -> Option<Value> {
        (id == ConceptId(7) && attr.raw() == 0).then(|| Value::Text("seven".into()))
    }
    fn related(&self, from: ConceptId, rel: Attr) -> Vec<ConceptId> {
        if from == ConceptId(7) && rel.raw() == 1 {
            vec![ConceptId(8)]
        } else {
            Vec::new()
        }
    }
    fn query(&self, with: &[Attr]) -> Vec<ConceptId> {
        if with.is_empty() {
            vec![ConceptId(7), ConceptId(8)]
        } else if with.iter().all(|attr| attr.raw() == 0) {
            vec![ConceptId(7)]
        } else {
            Vec::new()
        }
    }
    fn props(&self, id: ConceptId) -> Vec<(String, Value)> {
        if id == ConceptId(7) {
            vec![("label".into(), Value::Text("seven".into()))]
        } else {
            Vec::new()
        }
    }
    fn links(&self, id: ConceptId) -> Vec<(String, ConceptId)> {
        if id == ConceptId(7) {
            vec![("edge".into(), ConceptId(8))]
        } else {
            Vec::new()
        }
    }
}

impl Graph for GraphProbe {
    fn create(&mut self) -> ConceptId {
        panic!("el adaptador no debe crear conceptos")
    }
    fn destroy(&mut self, _: ConceptId) {
        panic!("el adaptador no debe borrar conceptos")
    }
    fn set(&mut self, _: ConceptId, _: Attr, _: Value) {
        panic!("el adaptador no debe escribir propiedades")
    }
    fn relate(&mut self, _: ConceptId, _: Attr, _: ConceptId) {
        panic!("el adaptador no debe añadir relaciones")
    }
    fn unrelate(&mut self, _: ConceptId, _: Attr, _: ConceptId) {
        panic!("el adaptador no debe borrar relaciones")
    }
    fn export(&self) -> Vec<u8> {
        panic!("el adaptador no debe exportar")
    }
    fn import(&mut self, _: &[u8]) -> Result<(), OnyxError> {
        panic!("el adaptador no debe importar")
    }
}

#[test]
fn read_only_forwards_every_query_through_dyn_ports_without_upcasting() {
    let mut graph = GraphProbe::new();
    let label = graph.attr("label");
    let edge = graph.attr("edge");
    let unknown = graph.attr("unknown");
    let port: &dyn Graph = &graph;
    let adapter = ReadOnly(port);
    let view: &dyn GraphView = &adapter;

    assert!(view.exists(ConceptId(7)));
    assert!(view.exists(ConceptId(8)));
    assert!(!view.exists(ConceptId(99)));
    assert_eq!(
        view.get(ConceptId(7), label),
        Some(Value::Text("seven".into()))
    );
    assert_eq!(view.get(ConceptId(7), unknown), None);
    assert_eq!(view.get(ConceptId(99), label), None);
    assert_eq!(view.related(ConceptId(7), edge), vec![ConceptId(8)]);
    assert!(view.related(ConceptId(8), edge).is_empty());
    assert!(view.related(ConceptId(7), unknown).is_empty());
    assert_eq!(view.query(&[]), vec![ConceptId(7), ConceptId(8)]);
    assert_eq!(view.query(&[label]), vec![ConceptId(7)]);
    assert!(view.query(&[unknown]).is_empty());
    assert_eq!(
        view.props(ConceptId(7)),
        vec![("label".into(), Value::Text("seven".into()))]
    );
    assert!(view.props(ConceptId(99)).is_empty());
    assert_eq!(
        view.links(ConceptId(7)),
        vec![("edge".into(), ConceptId(8))]
    );
    assert!(view.links(ConceptId(99)).is_empty());
}

#[test]
fn attr_only_registers_in_the_original_graph_through_a_dyn_port() {
    let mut graph = GraphProbe::new();
    let label = graph.attr("label");
    let fresh = {
        let port: &mut dyn Graph = &mut graph;
        let mut adapter = AttrOnly(port);
        let registry: &mut dyn AttrRegistry = &mut adapter;
        assert_eq!(registry.attr("label"), label);
        let fresh = registry.attr("fresh");
        assert_ne!(fresh, label);
        assert_eq!(registry.attr("fresh"), fresh);
        fresh
    };
    assert_eq!(graph.attr("fresh"), fresh);
    assert_eq!(graph.attr("label"), label);
    assert_eq!(
        graph.get(ConceptId(7), label),
        Some(Value::Text("seven".into()))
    );
}

#[test]
fn returned_text_is_owned_and_outlives_the_graph() {
    let mut returned = {
        let mut graph = GraphProbe::new();
        let label = graph.attr("label");
        let adapter = ReadOnly(&graph);
        adapter
            .get(ConceptId(7), label)
            .expect("propiedad del puerto")
    };
    let Value::Text(text) = &mut returned else {
        panic!("la propiedad debe ser texto")
    };
    text.push_str(" owned");
    assert_eq!(returned, Value::Text("seven owned".into()));
}
