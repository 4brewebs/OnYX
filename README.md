# OnYX

**Un Lego de cerebros, no una solución cerrada.**

OnYX te da un núcleo pequeño, auditable y sin dependencias, y un alma por defecto lista para correr. Si tu realidad es distinta, escribe tu propia alma. El contrato es lo que importa.

```
onyx-core   = contratos (eventos, bus, grafo, sistemas, blueprints, puertos)   · 0 deps
onyx-soul   = implementaciones por defecto que cumplen esos contratos          · 0 deps
adaptadores = TUI, WASM, TOML, Redis, Kafka... se enchufan a los puertos
```

`core + soul` es un cerebro listo para correr. Cambia el soul cuando quieras.

## Estado

v0. El diseño del contrato está cerrándose. No lo uses todavía donde un fallo tenga consecuencias.

## Licencia

MIT o Apache-2.0, a tu elección. Ver [LICENSE-MIT](LICENSE-MIT) y [LICENSE-APACHE](LICENSE-APACHE).
