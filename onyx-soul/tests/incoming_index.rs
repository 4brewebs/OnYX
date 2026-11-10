//! Pruebas del índice de entrantes del grafo (`incoming_props` / `incoming_links`).
//! El índice es privado: aquí solo se observa a través de la API pública
//! (`get`, `related`, `props`, `links`, `exists`, `export`/`import`) que la
//! limpieza de `destroy` deja el grafo igual que un barrido completo.
use onyx_soul::{AttrRegistry, ConceptId, Graph, GraphView, Value, WorldGraph as G};
use std::collections::{BTreeMap, BTreeSet};

fn c(id: ConceptId) -> Value {
    Value::Concept(id)
}

/// Reasignar una propiedad a otro concepto saca al dueño del índice del viejo:
/// destruir el viejo no debe borrar la propiedad, que ya apunta al nuevo.
#[test]
fn set_reassigned_then_destroy_old_keeps_new_reference() {
    let mut g = G::new();
    let p = g.attr("p");
    let (a, old, new) = (g.create(), g.create(), g.create());
    g.set(a, p, c(old));
    g.set(a, p, c(new));
    g.destroy(old);
    assert_eq!(g.get(a, p), Some(c(new)));
    g.destroy(new);
    assert_eq!(g.get(a, p), None, "destruir el destino vigente sí limpia");
}

/// Igual que el anterior, pero sobrescribiendo con un escalar.
#[test]
fn set_overwritten_by_scalar_then_destroy_old_keeps_scalar() {
    let mut g = G::new();
    let p = g.attr("p");
    let (a, old) = (g.create(), g.create());
    g.set(a, p, c(old));
    g.set(a, p, Value::Int(7));
    g.destroy(old);
    assert_eq!(g.get(a, p), Some(Value::Int(7)));
}

/// Tras `unrelate`, destruir el antiguo destino no toca el resto de destinos
/// de la misma relación, y el destino sigue vivo hasta que se destruye.
#[test]
fn unrelate_then_destroy() {
    let mut g = G::new();
    let r = g.attr("r");
    let (a, b, d) = (g.create(), g.create(), g.create());
    g.relate(a, r, b);
    g.relate(a, r, d);
    g.unrelate(a, r, b);
    assert!(g.exists(b), "unrelate no destruye el destino");
    assert_eq!(g.related(a, r), vec![d]);
    g.destroy(b);
    assert_eq!(g.related(a, r), vec![d]);
    // Volver a relacionar tras unrelate debe volver a indexar.
    g.relate(a, r, d);
    g.destroy(d);
    assert!(g.related(a, r).is_empty());
    assert!(g.links(a).is_empty());
}

/// `import` reconstruye el índice: destruir tras importar limpia propiedades
/// y relaciones entrantes, también con los atributos renumerados.
#[test]
fn import_then_destroy_cleans_references() {
    let mut src = G::new();
    let (p, r) = (src.attr("p"), src.attr("r"));
    let (a, b) = (src.create(), src.create());
    src.set(a, p, c(b));
    src.relate(a, r, b);
    let bytes = src.export();

    let mut g = G::new();
    // Registro en otro orden para forzar la renumeración de atributos.
    let _ = (g.attr("x"), g.attr("r"), g.attr("p"));
    g.import(&bytes).expect("import");
    let (p, r) = (g.attr("p"), g.attr("r"));
    assert_eq!(g.get(a, p), Some(c(b)));
    g.destroy(b);
    assert_eq!(g.get(a, p), None);
    assert!(g.related(a, r).is_empty());
    assert!(g.props(a).is_empty() && g.links(a).is_empty());
}

/// Ciclo A↔B por propiedad y por relación, más un bucle propio.
#[test]
fn cycle_destroy_one_side() {
    let mut g = G::new();
    let (p, r) = (g.attr("p"), g.attr("r"));
    let (a, b) = (g.create(), g.create());
    g.set(a, p, c(b));
    g.set(b, p, c(a));
    g.relate(a, r, b);
    g.relate(b, r, a);
    let me = g.attr("self");
    g.set(b, me, c(b));
    g.destroy(a);
    assert!(g.exists(b));
    assert_eq!(g.get(b, p), None);
    assert!(g.related(b, r).is_empty());
    assert_eq!(g.props(b), vec![("self".to_string(), c(b))]);
    g.destroy(b);
    assert!(!g.exists(b));
}

// ---------------------------------------------------------------------------
// Prueba de propiedades contra un modelo de referencia sin índice.
// ---------------------------------------------------------------------------

/// SplitMix64: generador determinista propio, sin crates.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

const ATTRS: [&str; 3] = ["a0", "a1", "a2"];

/// Valores del modelo: `None` = escalar `Int`, `Some(id)` = concepto.
#[derive(Default)]
struct Model {
    props: BTreeMap<u64, BTreeMap<&'static str, Value>>,
    links: BTreeMap<u64, BTreeMap<&'static str, BTreeSet<u64>>>,
}

impl Model {
    fn alive(&self, id: u64) -> bool {
        self.props.contains_key(&id)
    }
    /// Barrido completo: la semántica que el índice debe reproducir.
    fn destroy(&mut self, id: u64) {
        if self.props.remove(&id).is_none() {
            return;
        }
        self.links.remove(&id);
        for props in self.props.values_mut() {
            props.retain(|_, v| !matches!(v, Value::Concept(t) if t.0 == id));
        }
        for links in self.links.values_mut() {
            for set in links.values_mut() {
                set.remove(&id);
            }
            links.retain(|_, s| !s.is_empty());
        }
    }
    fn props_of(&self, id: u64) -> Vec<(String, Value)> {
        self.props.get(&id).map_or_else(Vec::new, |m| {
            m.iter().map(|(k, v)| (k.to_string(), v.clone())).collect()
        })
    }
    fn links_of(&self, id: u64) -> Vec<(String, ConceptId)> {
        self.links.get(&id).map_or_else(Vec::new, |m| {
            m.iter()
                .flat_map(|(k, s)| s.iter().map(move |&t| (k.to_string(), ConceptId(t))))
                .collect()
        })
    }
}

fn check(g: &G, m: &Model, ids: &[u64], ctx: &str) {
    for &id in ids {
        let cid = ConceptId(id);
        assert_eq!(g.exists(cid), m.alive(id), "{ctx}: exists({id})");
        assert_eq!(g.props(cid), m.props_of(id), "{ctx}: props({id})");
        assert_eq!(g.links(cid), m.links_of(id), "{ctx}: links({id})");
    }
}

fn run_seed(seed: u64, steps: usize) {
    let mut rng = Rng(seed);
    let mut g = G::new();
    let mut m = Model::default();
    let mut ids: Vec<u64> = Vec::new();
    for step in 0..steps {
        let pick = |rng: &mut Rng, ids: &[u64]| ids[rng.below(ids.len())];
        let op = if ids.is_empty() { 0 } else { rng.below(100) };
        let attr_name = ATTRS[rng.below(ATTRS.len())];
        let attr = g.attr(attr_name);
        let ctx = format!("seed {seed} step {step} op {op}");
        match op {
            0..=14 => {
                let id = g.create();
                m.props.insert(id.0, BTreeMap::new());
                m.links.insert(id.0, BTreeMap::new());
                ids.push(id.0);
            }
            15..=24 => {
                let id = pick(&mut rng, &ids);
                g.destroy(ConceptId(id));
                m.destroy(id);
            }
            25..=44 => {
                let (a, b) = (pick(&mut rng, &ids), pick(&mut rng, &ids));
                g.set(ConceptId(a), attr, c(ConceptId(b)));
                if m.alive(a) && m.alive(b) {
                    m.props
                        .get_mut(&a)
                        .unwrap()
                        .insert(attr_name, c(ConceptId(b)));
                }
            }
            45..=52 => {
                let a = pick(&mut rng, &ids);
                let v = Value::Int(rng.next() as i64);
                g.set(ConceptId(a), attr, v.clone());
                if m.alive(a) {
                    m.props.get_mut(&a).unwrap().insert(attr_name, v);
                }
            }
            53..=72 => {
                let (a, b) = (pick(&mut rng, &ids), pick(&mut rng, &ids));
                g.relate(ConceptId(a), attr, ConceptId(b));
                if m.alive(a) && m.alive(b) {
                    let links = m.links.get_mut(&a).unwrap();
                    links.entry(attr_name).or_default().insert(b);
                }
            }
            73..=89 => {
                let (a, b) = (pick(&mut rng, &ids), pick(&mut rng, &ids));
                g.unrelate(ConceptId(a), attr, ConceptId(b));
                if let Some(links) = m.links.get_mut(&a) {
                    if let Some(set) = links.get_mut(attr_name) {
                        set.remove(&b);
                        if set.is_empty() {
                            links.remove(attr_name);
                        }
                    }
                }
            }
            _ => {
                // Ida y vuelta por el snapshot hacia un grafo con otro registro.
                let bytes = g.export();
                let mut fresh = G::new();
                for name in ATTRS.iter().rev() {
                    fresh.attr(name);
                }
                fresh.import(&bytes).expect("import");
                g = fresh;
            }
        }
        check(&g, &m, &ids, &ctx);
    }
}

#[test]
fn incoming_index_matches_full_scan_model() {
    for seed in 0..200 {
        run_seed(seed, 300);
    }
}
