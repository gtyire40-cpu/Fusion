# Fusion

**Computación distribuida y administración de recursos para ordenadores Linux.**
Open source · Rust · Linux · Apache 2.0 · Independiente del hardware

> Un sistema. Varias máquinas.

Fusion conecta varios ordenadores Linux, descubre sus capacidades, administra sus recursos y coordina la ejecución de trabajos desde un único sistema de control.

---

## 1. Qué es Fusion

Hoy, cada ordenador es una isla: tiene su CPU, su RAM, su GPU y su almacenamiento, y si uno está saturado mientras otro está inactivo, nadie aprovecha esa diferencia.

Fusion añade una capa de software que permite gestionar un conjunto de máquinas como un entorno coordinado. Desde un panel único podrás:

- Ver qué ordenadores están conectados y qué hardware tienen.
- Consultar su uso y disponibilidad en cada momento.
- Fijar cuántos recursos puede usar Fusion en cada equipo.
- Crear trabajos distribuidos, supervisarlos, cancelarlos y ver sus resultados y registros.

No es solo un monitor de hardware, ni un escritorio remoto, ni un sistema de archivos compartidos: es un administrador de recursos distribuidos con planificador (Scheduler), ejecución de trabajos, red propia y extensibilidad.

### Qué NO hace Fusion

Fusion **no** convierte varios ordenadores en uno solo. En concreto:

- No suma la RAM ni la VRAM de varios equipos.
- No permite que un juego convencional use la CPU o GPU de otro ordenador, ni aumenta los FPS.
- No distribuye cualquier aplicación sin adaptaciones: cada tarea debe ser compatible con un modelo distribuido.
- No garantiza que usar más equipos sea siempre más rápido.

Es una característica inherente a la computación distribuida: funciona bien con tareas que se pueden dividir en partes independientes (compilar, renderizar fotogramas, procesar datos) y mal con las que requieren comunicación constante y de baja latencia.

---

## 2. Cómo funciona

### Arquitectura Master-Worker

| Componente | Función |
| --- | --- |
| **Master** | Coordina el entorno: descubre y registra nodos, gestiona emparejamientos, crea trabajos, elige dónde ejecutarlos y guarda el estado en una base de datos local. También puede trabajar como Worker. |
| **Worker** | Agente ligero en segundo plano: detecta su hardware, informa de sus capacidades, recibe trabajos autorizados, aplica límites de recursos, los ejecuta y devuelve resultados. Tiene permisos limitados: no es un administrador remoto del equipo. |
| **Scheduler** | Parte del Master que decide qué nodo ejecuta cada trabajo según recursos, red y requisitos. |

### Subsistemas

- **Hardware Discovery:** detecta y describe CPU, GPU, RAM, almacenamiento y red con una estructura común, sin depender de un modelo concreto.
- **Resource Manager:** aplica políticas reales de uso (no solo muestra porcentajes). Incluye CPU Manager, GPU Manager, Memory Manager y Storage Manager. Los límites efectivos se implementan con cgroups v2 de Linux.
- **fusion-network:** comunicación entre equipos, con identificación, autenticación y cifrado.

### Tecnologías

| Tecnología | Función |
| --- | --- |
| Rust | Lenguaje principal |
| Tokio | Comunicación asíncrona |
| Tonic + Protocol Buffers | gRPC y definición de mensajes |
| TLS / mTLS | Cifrado y autenticación mutua |
| Serde | Serialización interna |
| mDNS | Descubrimiento local, sin nube |
| cgroups v2 | Límites de CPU y memoria |

### Principios de diseño

- **Hardware agnóstico:** Intel, AMD y ARM64; GPU NVIDIA, AMD e Intel mediante adaptadores. No se exige CUDA a quien no puede usarlo.
- **Linux, sin distribución ni escritorio concretos:** Ubuntu, Debian, Fedora, Arch, Mint, Pop!_OS, etc. Los equipos de desarrollo son solo bancos de prueba, no una dependencia.
- **Solo red local:** sin servidores externos ni servicios de terceros.
- **Descubrir no es autorizar:** que un ordenador aparezca en la red no significa que pueda ejecutar trabajos. Se requiere emparejamiento explícito.
- **Seguridad primero:** ejecutar trabajos remotos es ejecución remota de código, así que los Workers solo aceptan tipos de trabajo registrados.

### Conexiones

Ethernet (la opción más fiable), Wi-Fi (depende de la calidad de la red) y enlaces USB-C/Thunderbolt cuando el sistema permite un enlace IP. USB-C es un conector, no un protocolo de red, por lo que Fusion debe comprobar que exista un enlace IP utilizable.

### Mensajes del protocolo (previstos)

`NODE_REGISTER`, `NODE_STATUS`, `NODE_CAPABILITIES`, `JOB_SUBMIT`, `JOB_ACCEPT`, `JOB_START`, `JOB_PROGRESS`, `JOB_COMPLETE`, `JOB_FAILED`, `JOB_CANCEL`, `RESOURCE_UPDATE`, `NODE_DISCONNECT`

---

## 3. Hoja de ruta por fases

Cada fase deja algo funcionando y demostrable. No se pasa a la siguiente sin cumplir su criterio de finalización. Las tareas detalladas de cada fase están en [ROADMAP.md](ROADMAP.md).

### Fase 1: Red y emparejamiento seguro
- **Qué:** Master y Worker mínimos que se conectan y se identifican.
- **Cómo:** Rust con Tokio y Tonic; gRPC con mTLS; emparejamiento manual aprobado por el usuario; IP fija (sin mDNS todavía).
- **Listo cuando:** un Worker se registra en el Master e intercambian `NODE_REGISTER` y `NODE_STATUS`.

### Fase 2: Hardware Discovery básico
- **Qué:** cada Worker detecta y describe su CPU, RAM y red.
- **Cómo:** lectura del sistema (`/proc`, `/sys`) hacia una estructura común; el Master la guarda en una base de datos local.
- **Listo cuando:** un comando CLI muestra las capacidades de todos los nodos.

### Fase 3: Ejecución remota con límites
- **Qué:** el Master envía un trabajo, el Worker lo ejecuta y devuelve el resultado.
- **Cómo:** cgroups v2 para límites de CPU y memoria; mensajes de progreso, cancelación y error; detección de desconexiones y reintentos.
- **Listo cuando:** se lanza un trabajo remoto, se cancela, y se recupera correctamente tras cortar la red.

### Fase 4: Compilación distribuida (primer caso de uso real)
- **Qué:** repartir la compilación de un proyecto entre varios nodos.
- **Cómo:** entorno de compilación idéntico en todos (toolchain fijado o contenedor); división en unidades de compilación; recogida y enlazado de resultados. Se estudian distcc, icecream y sccache como referencia.
- **Listo cuando:** compilar un proyecto real con varios nodos es más rápido que hacerlo en uno solo. Aquí se mide el rendimiento antes de seguir.

### Fase 5: Scheduler y Resource Manager
- **Qué:** el Master decide automáticamente dónde ejecutar cada trabajo.
- **Cómo:** puntuación de nodos según CPU libre, RAM, red y arquitectura; presupuestos configurables por Worker (hilos y memoria).
- **Listo cuando:** los trabajos se reparten solos y respetan los límites configurados.

### Fase 6: Experiencia de uso
- **Qué:** que sea cómodo de instalar y usar.
- **Cómo:** descubrimiento mDNS (con emparejamiento explícito), dashboard, historial de trabajos y perfiles de funcionamiento (por ejemplo, un "modo juego" que pausa el Worker).
- **Listo cuando:** un usuario nuevo conecta un nodo sin editar archivos de configuración.

### Fase 7: Nuevos tipos de trabajo
- **Qué:** ampliar más allá de la compilación.
- **Cómo:** renderizado por fotogramas (p. ej. Blender), procesamiento de datos y Storage Manager con directorios autorizados (transferencias propias; NFS o SSHFS como alternativa).
- **Listo cuando:** un render se reparte entre dos nodos.

### Fase 8: GPU, ARM64 y más distribuciones
- **Qué:** soporte de GPU y de más plataformas.
- **Cómo:** adaptadores para NVIDIA (NVML/CUDA), AMD (AMDGPU/ROCm) e Intel (oneAPI); pruebas en Fedora, Debian y ARM64.
- **Listo cuando:** un trabajo con GPU corre en un nodo compatible y se rechaza en uno que no lo es.

---

## 4. Ciclo de vida de un trabajo

```text
Usuario ──► JOB_SUBMIT ──► Master
                             │ 1. Valida el trabajo y sus requisitos
                             │ 2. El Scheduler elige un nodo
                             ▼
                          JOB_ACCEPT ◄── Worker (comprueba que puede y que está autorizado)
                             │
                          JOB_START ──► el Worker ejecuta con límites (cgroups v2)
                             │
                          JOB_PROGRESS (periódico: estado, registros)
                             │
              ┌──────────────┼──────────────┐
         JOB_COMPLETE    JOB_FAILED     JOB_CANCEL
        (resultados)   (error, reintento) (a petición del usuario)
```

Casos especiales:

- **El Worker rechaza el trabajo:** el Scheduler elige otro nodo.
- **El Worker se desconecta:** el Master marca el trabajo como interrumpido y lo reasigna según la política de reintentos.
- **El Master se desconecta:** el Worker termina o cancela según la configuración del trabajo y no deja procesos huérfanos.
- **Timeout:** cada trabajo tiene un tiempo máximo; al superarlo se cancela.

---

## 5. Modelo de seguridad

Fusion ejecuta trabajos en otros ordenadores, por lo que equivale a una ejecución remota de código controlada. Se diseña con estas reglas:

| Medida | Descripción |
| --- | --- |
| **Emparejamiento explícito** | Un nodo solo se une tras aprobarlo el usuario. Descubrir un equipo no da permiso para usarlo. |
| **mTLS** | Master y Worker se autentican mutuamente con certificados; todo el tráfico va cifrado. |
| **Tipos de trabajo registrados** | El Worker solo ejecuta tipos de trabajo permitidos, no comandos arbitrarios. |
| **Permisos limitados** | El Worker corre sin privilegios de administrador y no es un acceso remoto completo al equipo. |
| **Límites de recursos** | CPU y memoria acotadas con cgroups v2, para que ningún trabajo sature el equipo. |
| **Almacenamiento autorizado** | Solo se comparten los directorios que el usuario autoriza. |
| **Red local** | Sin servidores externos ni nube. |

Las vulnerabilidades deben reportarse de forma privada (ver `SECURITY.md`).

---

## 6. Información que recopila Hardware Discovery

| Recurso | Datos |
| --- | --- |
| **CPU** | Fabricante, modelo, arquitectura, núcleos físicos, hilos lógicos, frecuencia, utilización, carga y temperatura (si está disponible). |
| **GPU** | Fabricante, modelo, memoria, controlador, API disponible, utilización, temperatura, disponibilidad; integrada o dedicada; varias por nodo. |
| **RAM** | Total, usada, disponible, libre, usada por trabajos de Fusion y presupuesto configurado. |
| **Almacenamiento** | Discos, particiones, sistemas de archivos, capacidad, espacio libre y directorios autorizados. |
| **Red** | Interfaces, IP local, tipo de conexión, velocidad de enlace, latencia y estado. |

Los datos que no estén disponibles en un equipo se tratan como opcionales: Fusion no debe fallar por no poder leer una temperatura.

La distinción entre **memoria total**, **disponibilidad real** y **presupuesto permitido para Fusion** es clave: el trabajo se ejecuta siempre en la RAM física del Worker; Fusion no crea memoria adicional para el Master.

---

## 7. Estructura prevista del repositorio

```text
fusion/
├── Cargo.toml              # workspace
├── crates/
│   ├── fusion-proto/       # mensajes Protocol Buffers
│   ├── fusion-network/     # gRPC, mTLS, mDNS
│   ├── fusion-discovery/   # Hardware Discovery
│   ├── fusion-resources/   # Resource Manager (CPU, memoria, almacenamiento, GPU)
│   ├── fusion-scheduler/   # planificador de trabajos
│   ├── fusion-master/      # servicio del Master
│   ├── fusion-worker/      # agente del Worker
│   └── fusion-cli/         # herramienta de línea de comandos
├── docs/
├── README.md
├── ROADMAP.md
├── CONTRIBUTING.md
├── SECURITY.md
└── LICENSE
```

La estructura puede cambiar a medida que avance el diseño.

---

## 8. Requisitos previstos

- Linux con kernel que soporte cgroups v2 (la mayoría de distribuciones actuales).
- Rust 1.85 o superior para compilar.
- Kernel de Linux 5.10 o superior recomendado; una distribución debe exponer cgroups v2 montado y utilizable.
- Red local entre los equipos (Ethernet recomendada).
- Para GPU: controladores y herramientas del fabricante (solo si se quiere usar GPU).

---

## 9. Estado del proyecto

Fase 0 completada. La implementación de software de las Fases 1 y 2 y sus pruebas locales están listas. Falta validar la conexión y la detección en dos equipos reales. Consulta [ROADMAP.md](ROADMAP.md) para el estado actualizado.

### Ejecutar el prototipo de la Fase 1

El Master debe anunciar una dirección IP alcanzable por los Workers. Su huella de CA se muestra al iniciar:

```bash
cargo run -p fusion-cli -- master start --advertise-host 192.168.1.10
```

En otro equipo, compara la huella indicada por el Master y solicita el emparejamiento:

```bash
cargo run -p fusion-cli -- worker join 192.168.1.10 --fingerprint <HUELLA_SHA256>
```

El Worker queda esperando aprobación. En otra terminal del Master:

```bash
cargo run -p fusion-cli -- master pending
cargo run -p fusion-cli -- master approve <NODE_ID>
cargo run -p fusion-cli -- nodes list
```

También se puede rechazar una solicitud con `fusion master reject <NODE_ID>`. El Worker permanece conectado y envía heartbeats hasta que se interrumpe con Ctrl+C. El prototipo usa los puertos TCP 50051 (emparejamiento), 50052 (mTLS) y 50053 (descarga de la CA); permite esos puertos solo en la red local. Se pueden cambiar con `--pairing-bind`, `--node-bind` y `--ca-bind` en el Master, y con `--pairing-port`, `--node-port` y `--ca-port` en el Worker.

La descarga inicial de la CA se hace por HTTP porque todavía no hay una raíz de confianza en el Worker. La seguridad depende de comparar la huella recibida con la mostrada en la consola del Master por un canal independiente antes de continuar. El Master firma solo solicitudes de certificado aprobadas y valida la prueba de posesión de la clave privada del Worker. Los certificados de nodo duran un año y la CA diez; la renovación y revocación aún no están implementadas. Protege y respalda el directorio de datos del Master: contiene la clave privada de la CA. Este prototipo registra nodos; aún no envía ni ejecuta trabajos.

### Consultar hardware de un nodo

Después de emparejar el Worker, sus capacidades se envían al registrarse y se actualizan cada 30 segundos:

```bash
cargo run -p fusion-cli -- nodes show <NODE_ID>
cargo run -p fusion-cli -- nodes show <NODE_ID> --json
```

La detección lee CPU y memoria desde `/proc`, información de enlaces desde `/sys` e interfaces/IP desde el sistema. Temperatura, frecuencia, carga y velocidad del enlace se muestran como no disponibles cuando el equipo o sus permisos no las exponen. La latencia al Master se estima con el tiempo de respuesta del heartbeat. Los valores corresponden al Worker; Fusion no suma memoria entre máquinas.

## 10. Contribuir

Contribuciones, ideas y críticas son bienvenidas. Antes de empezar, consulta [ROADMAP.md](ROADMAP.md) para ver en qué fase está el proyecto y `CONTRIBUTING.md` para las normas.

## 11. Licencia

Apache License 2.0.
