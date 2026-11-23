# Fusion: Hoja de ruta

Este documento desglosa cada fase en tareas concretas. Se completa una fase antes de empezar la siguiente.

**Cómo leerlo**
- `[ ]` tarea pendiente, `[x]` tarea terminada.
- **Entregable:** lo que existe al terminar la fase.
- **Criterio de finalización:** la prueba objetiva de que la fase está hecha.
- **Riesgos y decisiones:** lo que conviene resolver antes o durante la fase.

| Fase | Tema | Resultado |
| --- | --- | --- |
| 1 | Red y emparejamiento seguro | Master y Worker se conectan y se autentican |
| 2 | Hardware Discovery básico | Se conocen las capacidades de cada nodo |
| 3 | Ejecución remota con límites | Se ejecutan, limitan y cancelan trabajos remotos |
| 4 | Compilación distribuida | Primer caso de uso real, medido |
| 5 | Scheduler y Resource Manager | Reparto automático y presupuestos |
| 6 | Experiencia de uso | mDNS, dashboard, perfiles |
| 7 | Nuevos tipos de trabajo | Renderizado, datos y almacenamiento |
| 8 | GPU, ARM64 y más distribuciones | Adaptadores y compatibilidad amplia |

---

## Fase 0: Preparación del repositorio

Antes de escribir funcionalidad.

- [x] Crear el repositorio con licencia Apache 2.0 (`LICENSE`), `README.md` y `ROADMAP.md`.
- [x] Crear un workspace de Cargo con los crates previstos (ver README).
- [x] Configurar `rustfmt`, `clippy` e integración continua (GitHub Actions: build, test y lint).
- [x] Crear `CONTRIBUTING.md` y `SECURITY.md` (cómo reportar vulnerabilidades).
- [x] Definir la versión mínima de Rust y de kernel soportada.

**Criterio de finalización:** `cargo build` y `cargo test` pasan en CI sobre un workspace vacío.

---

## Fase 1: Red y emparejamiento seguro

**Objetivo:** que un Master y un Worker se encuentren, se autentiquen mutuamente y se hablen.

**Tareas**
- [ ] Definir en Protocol Buffers los primeros mensajes: `NODE_REGISTER`, `NODE_STATUS`.
- [ ] Servidor gRPC en el Master (Tonic) y cliente en el Worker.
- [ ] Autoridad de certificados local: el Master genera su CA y certificados para cada Worker.
- [ ] mTLS: ambas partes verifican el certificado de la otra.
- [ ] Emparejamiento manual: el Worker pide unirse, el Master muestra un código y el usuario aprueba o rechaza.
- [ ] Persistir en el Master la lista de nodos emparejados (SQLite).
- [ ] Heartbeat periódico para saber si un nodo sigue vivo.
- [ ] CLI mínima: `fusion master start`, `fusion worker join <ip>`, `fusion nodes list`.
- [ ] Pruebas de integración con dos procesos en la misma máquina y luego con dos equipos reales.

**Entregable:** dos programas que se emparejan y mantienen una conexión cifrada.

**Criterio de finalización:** un Worker no emparejado es rechazado; uno emparejado aparece como "en línea" en el Master y pasa a "desconectado" si se corta la red.

**Riesgos y decisiones**
- Elegir cómo se renuevan o revocan certificados (aunque la implementación completa llegue después).
- Decidir la base de datos (SQLite es lo más simple para empezar).

---

## Fase 2: Hardware Discovery básico

**Objetivo:** que cada Worker describa su hardware con una estructura común.

**Tareas**
- [ ] Definir el mensaje `NODE_CAPABILITIES` y las estructuras comunes (CPU, RAM, red).
- [ ] CPU: fabricante, modelo, arquitectura, núcleos físicos, hilos lógicos, frecuencia, utilización, carga (`/proc/cpuinfo`, `/proc/stat`, `/sys`).
- [ ] RAM: total, usada, disponible, libre (`/proc/meminfo`).
- [ ] Red: interfaces, IP local, tipo de conexión, velocidad de enlace, latencia hacia el Master.
- [ ] Temperaturas cuando estén disponibles (`/sys/class/thermal`, `hwmon`), tratándolas como opcionales.
- [ ] Envío periódico de `RESOURCE_UPDATE` desde el Worker.
- [ ] Guardar capacidades y estado en la base de datos del Master.
- [ ] Comando `fusion nodes show <nodo>` con la información en formato legible y JSON.
- [ ] Pruebas con datos simulados para no depender de un hardware concreto.

**Entregable:** vista de capacidades y uso en tiempo casi real de todos los nodos.

**Criterio de finalización:** la CLI muestra correctamente la información en al menos dos equipos distintos (por ejemplo, Intel y AMD).

**Riesgos y decisiones**
- Muchos valores no están disponibles en todo el hardware: el diseño debe tratar los datos como opcionales.
- Distinguir memoria total, disponibilidad real y presupuesto permitido para Fusion.

---

## Fase 3: Ejecución remota con límites

**Objetivo:** ejecutar trabajos en un Worker de forma controlada y segura.

**Tareas**
- [ ] Mensajes: `JOB_SUBMIT`, `JOB_ACCEPT`, `JOB_START`, `JOB_PROGRESS`, `JOB_COMPLETE`, `JOB_FAILED`, `JOB_CANCEL`.
- [ ] Modelo de trabajo: identificador, tipo, requisitos (CPU, RAM, arquitectura), argumentos y estado.
- [ ] Registro de tipos de trabajo permitidos en el Worker (nada de comandos arbitrarios por defecto).
- [ ] Ejecución con un usuario sin privilegios.
- [ ] Límites con cgroups v2: CPU (cuota y pesos) y memoria (máximo).
- [ ] Captura de salida estándar, salida de error y código de retorno, enviados en streaming al Master.
- [ ] Cancelación: terminación limpia del proceso y de sus hijos.
- [ ] Tiempo máximo por trabajo (timeout).
- [ ] Desconexión: si el Worker pierde al Master, qué hace con el trabajo; si el Master pierde al Worker, marcar el trabajo y reintentar.
- [ ] Comandos CLI: `fusion job submit`, `fusion job list`, `fusion job logs`, `fusion job cancel`.

**Entregable:** un trabajo se envía, se ejecuta con límites, se puede cancelar y devuelve resultados.

**Criterio de finalización:** un trabajo que intenta consumir toda la RAM o CPU queda contenido; cortar la red a mitad no deja procesos huérfanos ni trabajos "fantasma".

**Riesgos y decisiones**
- cgroups v2 requiere permisos: validar la delegación con systemd en Pop!_OS, Mint, Fedora y Debian.
- Decidir el nivel de aislamiento: usuario sin privilegios + cgroups ahora; contenedores o namespaces después.

---

## Fase 4: Compilación distribuida

**Objetivo:** demostrar con un caso real que Fusion acelera una tarea.

**Tareas**
- [ ] Estudiar cómo funcionan distcc, icecream y sccache y qué aporta Fusion de distinto.
- [ ] Elegir el proyecto de referencia (uno en C/C++ y uno en Rust, por ejemplo).
- [ ] Definir el entorno de compilación idéntico: toolchain fijado o imagen de contenedor, y comprobarlo al unirse un nodo.
- [ ] Tipo de trabajo `compile_unit`: recibe el código ya preprocesado y devuelve el objeto compilado.
- [ ] Reparto de unidades entre nodos y recogida de resultados.
- [ ] Caché de resultados para no recompilar lo que no cambió.
- [ ] Manejo de fallos: si un nodo cae, la unidad se reasigna.
- [ ] Benchmarks: tiempo con 1 nodo, 2 nodos y con diferentes redes (Ethernet y Wi-Fi).
- [ ] Documentar los resultados, sean buenos o malos.

**Entregable:** `fusion build <comando>` que compila de forma distribuida.

**Criterio de finalización:** el tiempo de compilación con varios nodos es menor que con uno solo en el proyecto de referencia, o se documenta con datos por qué no y qué cambiar.

**Riesgos y decisiones**
- En redes lentas, enviar datos puede costar más que compilar.
- Diferencias de arquitectura o de versión de librerías producen resultados incompatibles.
- **Punto de decisión:** si no hay mejora, replantear el enfoque antes de seguir.

---

## Fase 5: Scheduler y Resource Manager

**Objetivo:** que el Master decida automáticamente y que los usuarios fijen presupuestos.

**Tareas**
- [ ] Cola de trabajos con prioridades y estados.
- [ ] Filtrado de nodos por requisitos (arquitectura, RAM, tipo de trabajo, disponibilidad).
- [ ] Puntuación de nodos: CPU libre, RAM, latencia y velocidad de red.
- [ ] Presupuesto de CPU por Worker (hilos) y de memoria (GB) configurables.
- [ ] Afinidad y prioridad de procesos (nice).
- [ ] Reintentos y reasignación automáticos.
- [ ] Política cuando el usuario del Worker necesita su equipo: pausar o reducir trabajos.
- [ ] Registro de decisiones del Scheduler para poder explicar por qué se eligió un nodo.
- [ ] Pruebas con nodos simulados para validar el algoritmo sin hardware real.

**Entregable:** `fusion job submit` ya no necesita indicar el nodo.

**Criterio de finalización:** con varios trabajos a la vez, el reparto respeta los presupuestos y ningún Worker supera su límite.

---

## Fase 6: Experiencia de uso

**Objetivo:** que otra persona pueda instalarlo y usarlo sin ayuda.

**Tareas**
- [ ] Descubrimiento con mDNS: el Master anuncia el servicio y los Workers lo encuentran, siempre con emparejamiento explícito.
- [ ] Paquetes de instalación (`.deb`, y otros según distro) y servicio systemd para el Worker.
- [ ] Dashboard (web local o TUI): nodos, uso, trabajos y registros.
- [ ] Perfiles de funcionamiento: por ejemplo "modo juego" (Worker en pausa), "modo máximo rendimiento".
- [ ] Archivo de configuración documentado y valores por defecto razonables.
- [ ] Mensajes de error claros y guía de resolución de problemas.
- [ ] Documentación de usuario y tutorial de inicio rápido.

**Entregable:** instalación y conexión de un nodo nuevo en pocos minutos.

**Criterio de finalización:** una persona ajena al proyecto monta un entorno de dos equipos siguiendo solo la documentación.

---

## Fase 7: Nuevos tipos de trabajo

**Objetivo:** demostrar que Fusion es una plataforma, no solo un compilador distribuido.

**Tareas**
- [ ] Diseñar una interfaz clara para registrar tipos de trabajo (plugins o adaptadores).
- [ ] Renderizado por fotogramas (Blender): reparto de rangos de fotogramas y recolección.
- [ ] Procesamiento de datos y compresión por bloques.
- [ ] Storage Manager: directorios autorizados por nodo, transferencias con el protocolo de Fusion.
- [ ] Evaluar NFS o SSHFS como alternativa de compartición.
- [ ] Transferencia de datos de entrada y salida con comprobación de integridad.
- [ ] Documentar cómo crear un tipo de trabajo nuevo.

**Entregable:** al menos dos tipos de trabajo además de la compilación.

**Criterio de finalización:** un render se reparte entre dos nodos y el resultado coincide con el de un solo nodo.

---

## Fase 8: GPU, ARM64 y más distribuciones

**Objetivo:** ampliar la compatibilidad de hardware y plataformas.

**Tareas**
- [ ] Detección de GPU: fabricante, modelo, VRAM, controlador, utilización, temperatura; integrada frente a dedicada; varias GPU por nodo.
- [ ] Adaptador NVIDIA (NVML/CUDA).
- [ ] Adaptador AMD (AMDGPU/ROCm).
- [ ] Adaptador Intel (oneAPI).
- [ ] Capacidades declaradas por el nodo (por ejemplo, "tiene CUDA") y requisitos declarados por el trabajo.
- [ ] Rechazo claro de trabajos con GPU en nodos incompatibles.
- [ ] Pruebas en Fedora, Debian, Arch y ARM64.
- [ ] Matriz de compatibilidad publicada en el repositorio.

**Entregable:** trabajos con GPU en nodos compatibles y matriz de plataformas soportadas.

**Criterio de finalización:** un trabajo de render con GPU se ejecuta en un nodo compatible y se rechaza con un mensaje claro en uno que no lo sea.

**Riesgos y decisiones**
- Es la fase que más hardware distinto necesita para probarse.
- La detección de GPU no implica poder ejecutar trabajos en ella: cada adaptador requiere trabajos preparados.

---

## Fuera del alcance (por diseño)

- Sumar RAM o VRAM de varios equipos en un único espacio.
- Mejorar los FPS de juegos usando otros ordenadores.
- Distribuir aplicaciones arbitrarias sin adaptación.
- Dependencias de nube o servidores externos.

## Seguimiento

Cada fase se gestionará como un *milestone* de GitHub y cada tarea como un *issue*. Los resultados de las mediciones (sobre todo los de la Fase 4) se publicarán en el repositorio.
