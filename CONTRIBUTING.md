# Contribuir a Fusion

Gracias por ayudar a construir Fusion. Este repositorio está organizado como un
workspace de Cargo y sigue las fases descritas en `ROADMAP.md`.

## Antes de empezar

- Lee `README.md` y `ROADMAP.md`.
- Trabaja en la fase indicada y completa su criterio de finalización.
- Describe el problema y el enfoque antes de hacer cambios grandes.
- No introduzcas dependencias de nube ni servicios externos.

## Desarrollo local

Fusion requiere Rust 1.85 o superior.

```bash
cargo fmt --all
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

## Cambios

- Mantén cada cambio pequeño y enfocado.
- Añade pruebas para comportamiento nuevo o errores corregidos.
- Ejecuta format, build, tests y clippy antes de abrir una solicitud de cambios.
- Documenta cambios visibles en `README.md` o en la documentación correspondiente.

## Pruebas con hardware

Cuando una prueba dependa de hardware o distribución concretos, separa la
herramienta de detección de los datos simulados. Las pruebas no deben fallar
porque un equipo no tenga GPU, sensores térmicos o interfaces concretas.

## Commits y solicitudes de cambios

- Usa mensajes de commit claros en presente.
- Incluye el alcance, por ejemplo: `network: validar configuración mTLS`.
- Explica qué se probó y qué hardware se utilizó, si aplica.
