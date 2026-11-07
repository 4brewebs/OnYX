# WorldGraph

## Uso y límites

`WorldGraph::new()` aporta un `Graph` EAV en memoria. La aplicación registra
atributos y define sus propios conceptos; no hay esquema de negocio ni inferencia.
`onyx-core` no depende de esta implementación. Su módulo público
`conformance::graph` recibe fábricas y no exige `Default`. La feature
`conformance` está desactivada por defecto; soul solo la activa como dependencia
de desarrollo. Es una API de verificación fuera de la estabilidad de v0.

## Representación elegida

Se consideraron dos representaciones: conjuntos globales de tríadas con índices
separados, y un mapa ordenado de conceptos con propiedades y relaciones salientes.
Se elige la segunda. Conserva conceptos vacíos sin centinelas y evita mantener
varios índices mutables sincronizados. `destroy` recorre el grafo para limpiar
referencias entrantes; no se promete un coste constante ni tiempo real.

El registro internado mantiene nombres e identificadores locales. Cada concepto
guarda propiedades por atributo y conjuntos de destinos por etiqueta de relación.
Las consultas ordenan por el contrato, incluido el orden por nombre de `props`
y `links`, que no es necesariamente el orden numérico de los atributos.
Un contador conserva el máximo ID emitido, incluso tras destruir conceptos.
Al agotar IDs o atributos, las operaciones infalibles fallan con un pánico antes
de modificar el estado; no se reciclan identificadores. WorldGraph empieza en 1
y rechaza el ID 0 en su formato privado; no se exige esto a otros souls.
Una captura con máximo `u64::MAX` se acepta, pero ya no permite crear conceptos.

## Snapshots privados

El codec pertenece exclusivamente a este soul. No es un formato de intercambio
ni una promesa de compatibilidad entre versiones, proyectos o souls. Se conserva
el patrón de bits de los flotantes, no una conversión decimal. No incorpora suma
de comprobación: una alteración que siga describiendo un grafo válido puede
aceptarse. Validación estructural y atomicidad no garantizan integridad de bytes.

La importación decodifica y valida un estado temporal antes de sustituir el
estado vivo. Comprueba tamaños contra los bytes disponibles antes de recorrer
contadores, nombres y entradas duplicadas, referencias, etiquetas de valores,
UTF-8, máximo ID y bytes sobrantes. No reserva memoria a partir de un contador
sin validar. Los nombres se remapean sobre una copia del registro receptor;
solo después se publica el nuevo estado y el máximo de ambos contadores.

Esto no recupera el bus, los sistemas ni TickState. La recuperación completa del
runtime y el destino de los eventos pendientes requieren un protocolo de captura
y restauración que no se implementa aquí. Una caída puede perder IDs emitidos
después de la captura, como establece el contrato de core.

## Verificación

La suite genérica compara conceptos, propiedades y relaciones observables, no
bytes ni números de atributo entre instancias. El soul aporta datos inválidos
y una sonda de su registro completo y máximo ID. La suite compara esa sonda antes
y después de un error, sin imponer una representación privada ni igualdad de
identificadores entre instancias. WorldGraph utiliza su exportación privada como
sonda completa en la misma instancia; no exige bytes canónicos a otros souls.
Las pruebas privadas del codec cubren datos truncados, corruptos y límites.

Los seis controles defectuosos de `onyx-core/tests/conformance_probes.rs`
usan IDs no consecutivos que empiezan en 42. Cada caso pasa sin el defecto y
falla al activarlo. El doble solo cubre esos casos y rechaza toda importación;
no implementa snapshots correctos ni demuestra intercambio entre souls.
