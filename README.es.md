# erl-raster

**Rasterización adaptativa para móvil: mide qué motor es más rápido y deja que el dispositivo decida.**

`erl-raster` es un pequeño workspace de Rust que responde a una pregunta, con honestidad, en la máquina
en la que se está ejecutando: *para este trozo de dibujo, ¿es más rápida la CPU o la GPU ahora mismo, y
cuándo merece la pena cambiar?* No sustituye a un motor gráfico y no intercepta ninguno. Rasteriza
hojas, mide cuánto cuesta eso y arbitra.

Lee este documento en otro idioma:
[中文](README.zh-CN.md) | [Français](README.fr.md) | **Español** | [Русский](README.ru.md)

- [1. El problema con el que nos topamos](#1-the-problem-we-ran-into)
- [2. Qué es ERL Raster, y qué se niega a ser](#2-what-erl-raster-is-and-what-it-refuses-to-be)
- [3. Las cuatro trampas en torno a las que se construye el diseño](#3-the-four-traps-the-design-is-built-around)
- [4. La implementación, crate por crate](#4-the-implementation-crate-by-crate)
- [5. La carga publicada](#5-the-published-load)
- [6. Ejecutarlo](#6-running-it)
- [7. En qué punto está el proyecto](#7-where-the-project-stands)
- [8. Hoja de ruta](#8-roadmap)
- [9. Por qué no hay cifras en este archivo](#9-why-there-are-no-numbers-in-this-file)
- [10. Licencia](#10-license)

---

## 1. El problema con el que nos topamos

Construimos una aplicación móvil en React Native. Desplazar una lista de conversaciones no era fluido, y
abrir una conversación tardaba un instante de más antes de que se asentara el primer fotograma. Nada en
el perfil decía "el hilo de JavaScript", nada decía "el layout", y nada decía "la red". Los fotogramas
se perdían donde una aplicación nativa los pierde, en la rasterización, y esa es la única parte de la
canalización que nuestra stack nunca había tenido motivo para mirar.

Perseguirlo produjo un hallazgo que en realidad no tiene que ver con nuestra aplicación.

**Ambas plataformas permiten que una aplicación rasterice un trozo de contenido en la CPU o en la GPU,
y ninguna de las dos mide cuál es más rápida.**

- En iOS, `CALayer.draw(in:)` da un contexto de CPU, un `CAMetalLayer` da un drawable de Metal, y
  `shouldRasterize` traslada el coste de un subárbol a una rasterización fuera de pantalla puntual. Son
  decisiones reales de canalización, y existen capa a capa, a mano.
- En Android, `View.setLayerType(LAYER_TYPE_SOFTWARE)` fuerza un subárbol a la CPU, un
  `SurfaceView` o un `TextureView` entrega una superficie que es tuya, y `RenderEffect` ejecuta
  desenfoque y trabajo de color en la GPU dentro de la canalización de hardware. De nuevo, decisiones
  reales, tomadas a mano, vista a vista.

Así que la palanca existe. Lo que no existe es nada que la accione. Un desarrollador que quiera saber
si un desenfoque debería ejecutarse en la CPU de este teléfono o en su GPU tiene que escribir un
benchmark, ejecutarlo, esperar que el teléfono no estuviera caliente, y luego codificar a fuego la
respuesta para un dispositivo que nunca volverá a ver.

Y escribir ese benchmark correctamente es sorprendentemente difícil. Nuestros propios primeros intentos
estaban equivocados de maneras que producían cifras seguras e inútiles:

- Un kernel medido sobre un búfer que ya había tocado es hasta cinco veces más rápido que el mismo
  kernel sobre un búfer que acaba de recibir, así que el bucle ingenuo mide la caché y lo llama
  cómputo.
- Ejecutar toda una canalización y luego toda la otra le concede a la segunda el beneficio de un reloj
  que ya ha subido de frecuencia y a la primera el coste de haberlo subido.
- Un teléfono que ha sufrido throttling mide su propio yo con throttling, y un perfil tomado en ese
  estado describe un dispositivo que ya no existe.
- Cambiar de canalización no es gratis. Bind, descriptors, fence, a veces una transferencia. Un
  despachador que se mueve ante una ganancia del cinco por ciento puede gastar fácilmente más tiempo
  moviéndose que dibujando.

El defecto de rendimiento que originó todo esto ya está comprendido y corregido en la propia aplicación.
Lo que expuso es la laguna que llena este repositorio: el arbitraje entre CPU y GPU es una decisión que
toda stack de renderizado toma, mal, a mano.

## 2. Qué es ERL Raster, y qué se niega a ser

`erl-raster` es un **motor de rasterización con una capa de arbitraje**. Describes un lote de dibujo
para un fotograma y lo entregas con un objetivo; la biblioteca lo ejecuta en aquella de sus
canalizaciones que se predice más rápida, bajo las condiciones en las que el dispositivo se encuentra
realmente.

No es un framework, ni un compositor, ni un sustituto de Core Animation, HWUI o Skia. No puede
interceptar las llamadas de dibujo de otro framework, y no lo intenta. La cuestión de compatibilidad se
resuelve por diseño y no por esperanza:

- **Una API, nunca un hook.** Sin swizzling, sin reemplazo de clases, sin comportamiento indefinido.
  Eso es lo que la hace distribuible, en una tienda de aplicaciones y en un registro de paquetes.
- **Un fallback a CPU permanente, nunca eliminado.** La ruta escalar de CPU está siempre en el
  conjunto de candidatas. El peor caso de adoptar la biblioteca es que se ejecute exactamente tan rápido
  como la referencia.
- **Una ruta de referencia determinista.** La referencia de CPU es solo enteros de principio a fin, así
  que una comparación de conformidad es bit a bit en lugar de aproximada. Un backend que diverge está
  equivocado, no "suficientemente cerca", y se desactiva en lugar de tolerarse en silencio.

La versión en una frase: *escribe el dibujo una vez, y deja que el arbitraje elija el motor.*

## 3. Las cuatro trampas en torno a las que se construye el diseño

### 3.1 Un benchmark deformado por la caché

La solución no es un bucle mejor, son dos mediciones y una regla sobre cuál se aplica.

- **Dos regímenes, ambos almacenados.** `Regime::Hot` reutiliza el objetivo y mantiene las fuentes
  residentes, lo que describe una operación repetida sobre datos que ya están en vuelo, como un atlas
  de glifos redibujado en cada fotograma. `Regime::Cold` asigna un objetivo nuevo y limpia las cachés
  antes de la iteración, lo que describe el primer fotograma de una pantalla o una imagen decodificada
  siendo redimensionada. En el régimen frío se escribe un búfer de dieciséis mebibytes entre
  iteraciones, lo que está por encima de la caché de último nivel de cualquier teléfono y de la mayoría
  de los portátiles, y la asignación se sitúa dentro de la región medida a propósito.
- **El anfitrión elige el régimen, no el benchmark.** Solo quien llama sabe si un fotograma dado está
  repitiendo trabajo residente o recibiendo fuentes nuevas. Eso es lo que lleva `FrameContext::regime`.
- **El calentamiento se descarta.** Las primeras iteraciones de una canalización contienen su primera
  asignación, su primera compilación de shader y el llenado de caché de todo lo demás. La configuración
  por defecto del harness descarta diez iteraciones antes de conservar sesenta.

### 3.2 El coste de cambiar de canalización

- **El lote es la unidad de decisión, no la primitiva.** Un cambio paga por el binding, las
  actualizaciones de descriptors y la sincronización; una decisión por glifo costaría más que el glifo.
  Un lote es el dibujo de un fotograma, así que un único cambio se amortiza sobre todo lo que contiene.
- **El coste de cambio se mide, no se estima.** `measure_switch_costs` ejecuta la misma carga
  inmediatamente después de una carga en la otra canalización y resta el tiempo que tarda tras una
  carga en la misma, por par ordenado, porque el coste no es simétrico. `SwitchCostModel::between`
  devuelve `None` para un par que nunca se ha medido, y `None` no se convierte en un valor por defecto:
  un movimiento no medido se rechaza en lugar de darse por gratis.
- **Como mucho, un movimiento de rendimiento por fotograma.** Un segundo movimiento de rendimiento
  pagaría el coste fijo dos veces dentro del fotograma que pretendía mejorar. Un movimiento hecho porque
  el titular *no puede* ejecutar el lote no está sujeto a ese presupuesto, ya que no es agitación.
- **Nunca durante una ventana crítica.** El anfitrión declara un fotograma crítico mientras una
  animación está comprometida, mientras un objetivo se comparte, o mientras una captura se está
  ejecutando. `FrameContext::frozen` lleva eso, y la evidencia para un cambio deja de acumularse en
  lugar de ser descartada.

### 3.3 Carga que cambia por debajo del perfil

- **Ningún intento de leer la "utilización de la GPU".** No está expuesta a las aplicaciones en ninguna
  de las dos plataformas. Una GPU ocupada se manifiesta más bien como una latencia de envío a
  finalización inestable, y es esa varianza la que el proyecto vigila, no un indicador hipotético.
- **El estado térmico forma parte de la identidad del perfil, no un campo junto a él.** `SystemState`
  indexa un perfil por dispositivo, plataforma y estado térmico. `ThermalState::permits_absolute_measurement`
  es verdadero solo para `Nominal`, y el runtime recurre al fallback y desconfía del perfil en el
  momento en que el estado deja de coincidir.
- **Histeresis con cifras escritas.** Un aspirante debe superar el coste del titular por el coste de
  cambio medido más un margen, por defecto el diez por ciento del titular, y debe hacerlo en tres
  evaluaciones consecutivas antes de ocupar su lugar. Una regresión es una ejecución que aterriza en más
  de una vez y media su propia predicción, cuatro veces seguidas.

### 3.4 Compatibilidad

- **Tres niveles de integración, de menor a mayor invasividad.** Un productor de imágenes (`render(op) ->
  surface`) no necesita ninguna integración nativa y funciona en un harness de pruebas en un escritorio.
  Una vista anfitriona es un componente Fabric en React Native, un `UIView` sobre un `CAMetalLayer` en
  iOS, un `View` sobre un `SurfaceView` en Android, y está reservada para contenido que cambia cada
  fotograma. Los adaptadores de plataforma son pequeñas capas de Swift, Objective-C++ y Kotlin para
  aquello que solo el lenguaje de la plataforma puede alcanzar: la interfaz térmica, un efecto de
  render, el layer type de una vista.
- **La conformidad se verifica, no se asume.** Cada backend se compara contra la referencia entera, con
  una tolerancia explícita por operación donde el backend declara no ser exacto bit a bit. Un backend
  que diverge se desactiva, no se aproxima en silencio.
- **Nada se descubre a espaldas de la plataforma.** El registro contiene lo que se enlazó explícitamente
  en la compilación, más la referencia. Una GPU que no está enlazada aparece en el informe con el motivo
  por el que falta, en lugar de con un espacio en blanco.

## 4. La implementación, crate por crate

El workspace son cuatro crates. El MVP **no tiene ninguna dependencia de terceros fuera del puente de
GPU**: el core, el benchmark y el despachador son `std` puro, lo que mantiene la compilación de código
abierto reproducible en cualquier máquina y mantiene la ruta del fotograma libre de código que pudiera
pausarla.

```
crates/
  erl-raster-core         primitivas y la referencia determinista
  erl-render-backends     las implementaciones, detrás de un contrato
  erl-benchmark           el motor de medición y el perfil del dispositivo
  erl-adaptive-runtime    el despachador
```

### 4.1 `erl-raster-core`

Rasterización independiente de la plataforma, y la referencia a la que se somete cada backend.

- `op` define lo que se puede pedir. `OpKind` es `FillGlyphs`, `Resample` o `Blur`, deliberadamente
  tres, porque los tres se comportan de manera lo bastante distinta como para que una única respuesta
  para todos ellos fuera equivocada para dos: una ejecución de glifos es pequeña, dispersa y limitada
  por la latencia; un remuestreo es una lectura en streaming; un desenfoque es un barrido de dos pasadas
  con un búfer de trabajo. Cada tipo expone un coste `Work` en *unidades de trabajo en lugar de tiempo*,
  porque un backend sabe lo que se le pidió hacer y solo el harness puede decir cuánto tardó. `Batch` es
  la unidad de decisión, y `OpKindSet` es un conjunto de tres bits para que el despachador por fotograma
  pueda construirlo en un registro y no asignar nunca memoria.
- `reference` es la ruta determinista. Aritmética entera de principio a fin: un kernel de coma flotante
  puede diferir entre una máquina de compilación x86, un teléfono de Apple silicon y un dispositivo
  Android ARM, lo que haría que una prueba de conformidad fuera o imposible de escribir o
  permanentemente en rojo. Los enteros dan igualdad bit a bit en los tres.
- `fixed` contiene los ayudantes de punto fijo, escritos de modo que un registro NEON de ocho carriles
  reproduzca el valor escalar exactamente. `image` contiene `Surface`, un búfer de texeles de cuatro
  bytes empaquetado de forma compacta, con una huella barata usada para decir *dónde* divergieron dos
  backends. `glyph` contiene `GlyphMask`, `GlyphSpan` y `GlyphRun`, con reutilización de máscaras al
  modo en que una caché de texto usa un atlas. `geom` y `error` son los tipos de apoyo. `system` define
  `SystemState` y `ThermalState`.
- `workload` construye la carga publicada, descrita en la sección 5.

El crate lleva `#![forbid(unsafe_code)]`. Los kernels vectorizados que necesitan `unsafe` viven en los
backends, detrás de la comprobación de arquitectura que los hace seguros de invocar.

### 4.2 `erl-render-backends`

Un contrato, varias implementaciones.

- `Backend` es el trait: `capabilities`, `cost_model`, `prepare`, `execute`, `release`.
  `prepare` es donde una canalización hace el trabajo que solo debería hacer una vez, para que la
  primera ejecución *cronometrada* sea representativa. `execute` ejecuta todo un lote contra un objetivo
  en memoria del anfitrión y devuelve un `Executed`.
- `Capabilities` describe lo que un backend *es*: qué tipos sirve, la superficie más grande que acepta,
  si reproduce la referencia byte a byte, y si volver a entrar en él es costoso. Un backend que sirve
  solo parte de un lote no es candidato en absoluto, porque dividir un lote entre dos canalizaciones
  cuesta un cambio y un paso de composición, y el MVP no lo hace.
- `CostModel` describe lo que un backend *cuesta*, en cuatro términos que el despachador necesita de
  forma independiente porque escalan con cosas distintas: un coste fijo por ejecución, un coste por
  operación, un coste por píxel de trabajo y un coste por byte movido, los dos últimos en milésimas de
  nanosegundo para que una canalización rápida sea expresable siquiera. `from_measurement` reconstruye
  un modelo a partir de una duración medida, y la sonda dedicada de coste fijo lo refina.
- `CpuBackend::scalar()` es la referencia y el fallback. `CpuBackend::neon()` existe solo en
  AArch64 y devuelve `None` en cualquier otro sitio en lugar de devolver el backend escalar bajo un
  nombre NEON, porque un backend que informara mal de qué canalización es envenenaría el perfil. Ambas
  variantes son un solo tipo distinguido por un flag, y la vectorizada reemplaza exactamente una cosa:
  la fila de composición de glifos, que es donde las mediciones sitúan el coste. El remuestreo es un
  gather que gana poco con registros anchos, y la normalización del box blur es una división entera sin
  forma paralela por carriles en esta arquitectura, así que ambos permanecen en la ruta de referencia y
  `Capabilities` lo dice así.
- `GpuBackend` es la costura más el dispositivo. `GpuSurfaceBridge` son tres llamadas, `device`,
  `prepare`, `execute`, sin trucos de lifetimes: una implementación por plataforma, en el lenguaje de
  esa plataforma, detrás de una fina capa FFI. La regla decisiva es que el objetivo es **memoria del
  anfitrión**: el puente lo sube, dibuja y descarga el resultado, así que la transferencia que decide la
  mayoría de los casos reales está *dentro* de la región que el harness cronometra. Un backend de GPU
  medido sobre una textura que ya poseía daría una cifra mucho mejor y describiría una situación en la
  que quien llama nunca se encuentra. `GpuBackend::absent` existe para que un informe pueda decir *por
  qué* una GPU no fue candidata en lugar de omitirla en silencio.
- `wgpu_bridge` es el primer puente real, compilado solo en macOS. Cuatro decisiones lo conforman:
  los kernels son dispatches de cómputo en lugar de render passes, porque una ejecución de glifos tiene
  que leer el píxel que está a punto de mezclar y un render pass no puede leer su propio attachment;
  todo es un storage buffer leído y escrito en su sitio en lugar de una textura, lo que elimina una
  copia de staging y un paso de de-striding; el `Work` informado se calcula en el anfitrión a partir de
  la misma aritmética de recorte que usa la referencia, porque contar píxeles en el dispositivo añadiría
  una lectura de vuelta en cada fotograma para aprender lo que el anfitrión ya sabe; y la regla de
  medición es la de la costura, subida y descarga dentro de la región.
- `registry` es donde aterriza la trampa de compatibilidad. Sin descubrimiento, sin sondeo.
  `default_registry()` devuelve primero la referencia escalar y la variante vectorizada cuando la
  arquitectura dispone de una; `registry_with_gpu` añade un puente enlazado.

### 4.3 `erl-benchmark`

El motor de medición, y lo único que sobrevive a un proceso.

- `stats` se niega a informar de una media por sí sola. Mediana y percentil noventa y cinco sobre un
  número fijo de iteraciones, porque una media oculta exactamente lo que es visible: los fotogramas
  largos. `is_conclusive` protege contra informar de una distribución construida con demasiado pocas
  muestras.
- `regime` es la separación entre caliente y frío de la sección 3.1.
- `harness` posee el reloj. `HarnessConfig` por defecto descarta diez iteraciones y luego conserva
  sesenta, en ambos regímenes, y un scrub de dieciséis mebibytes. `run_trials` ejecuta cada canalización
  contra cada carga, entrelazando las canalizaciones lote a lote para que ninguna reciba el ramp-up del
  reloj. `measure_switch_costs` produce los costes de cambio por par ordenado. Un `Trial` informa de
  `Measured`, `Unavailable`, `Unsupported` o `Failed`, con un motivo adjunto, de modo que una
  canalización que no se ejecutó es un hallazgo y no una laguna.
- `profile` es sobre lo que despacha una sesión posterior. `DeviceProfile` contiene el `SystemState`
  en el que se tomó, las entradas y los costes de cambio, y escribe en un formato de texto versionado
  que un humano puede leer y un diff puede revisar. `fastest_for_kind` responde a "quién ganó el
  desenfoque, en caliente, en este dispositivo" a partir de las cargas de un solo tipo, que es cómo una
  decisión sobre un tipo puede tomarse a partir de una medición de otra carga que lo contenía.

### 4.4 `erl-adaptive-runtime`

Donde el proyecto deja de medir y empieza a decidir. No contiene código de renderizado: las
canalizaciones llegan a través del trait `Backend`.

- `FrameContext` es la división entre lo que solo el anfitrión sabe y lo que la biblioteca deduce. El
  anfitrión declara el régimen y si el fotograma es crítico. Todo lo demás, el propio arbitraje, es de
  la biblioteca, porque no es algo que deba pedirse a una aplicación que haga bien.
  `AdaptiveRuntime::render` toma un lote y devuelve los bytes de su renderizado, y quien llama nunca
  nombra una canalización.
- `DispatchPolicy` es la histeresis en cifras: `minimum_gain_percent` (10),
  `required_consecutive_wins` (3), `regression_percent` (150), `regression_window` (4), y
  `switching_enabled`, que fija al titular y es el interruptor A/B que el proyecto usa para medir cuánto
  vale el arbitraje. `gain_clears_the_bar` es una única desigualdad a propósito: la ganancia tiene que
  pagar el movimiento de vuelta *y* superar el margen, de modo que no puede escribirse una política en
  la que un movimiento sea a la vez aprobado y no merezca la pena.
- `Decision` lleva el motivo por el que se tomó, incluidas las decisiones de no moverse, con las cuatro
  cifras que la respaldan: coste predicho del titular, coste predicho del aspirante, coste de movimiento
  medido y victorias consecutivas. Eso es lo que hace que la política sea comprobable sin un teléfono, y
  lo que hace que un despachador que se comporta mal sea depurable. Hay once motivos, entre ellos
  `GainBelowSwitchCost`, `SwitchCostUnknown`, `ChallengerNotProven`, `CriticalWindow`,
  `SwitchBudgetSpent`, `StaleProfile`, `IncumbentCannotServe` y `UnavailableFallback`.
- `AdaptiveRuntime` sostiene cinco reglas. El titular nunca se descarta por ser lento, solo se le
  desplaza por algo mejor; no hay un estado de "ninguna canalización". Un aspirante debe ganar
  repetidamente. Un movimiento tiene que pagarse a sí mismo en nanosegundos medidos, y un movimiento no
  medido se rechaza. Como mucho, un movimiento de rendimiento por fotograma. El perfil solo se usa en la
  condición en la que se tomó. Un runtime construido sin un fallback permanente se rechaza en la
  construcción en lugar de tolerarse.

## 5. La carga publicada

El benchmark solo es comparable consigo mismo si el trabajo nunca cambia, así que cada carga es fija,
procedimental y libre de cualquier archivo de fuente, archivo de imagen o asset. Las máscaras de glifos
vienen de un generador con semilla, la fuente del remuestreo igual, y el generador es un `xorshift64*`
de diecisiete líneas elegido precisamente porque ninguna actualización del compilador puede cambiar lo
que produce. Dos ejecuciones con seis meses de diferencia, en un portátil y en un teléfono, dibujan
exactamente los mismos píxeles.

Las cinco dibujan en el mismo objetivo de referencia, 1080 por 2340, un teléfono en vertical a densidad
triple, porque el objetivo es lo que un backend tiene que asignar, limpiar y entregar, y comparar entre
tamaños de objetivo plegaría el coste del objetivo dentro de la comparación.

| Carga de trabajo | Para qué sirve |
|---|---|
| `glyphs.tiny` | Cuatro máscaras cortas. El caso que la GPU pierde: enviar cuesta más que mezclar. |
| `glyphs.paragraph` | Una pantalla de texto. Muchas máscaras pequeñas sobre un plano mayormente transparente. |
| `resample.card` | Una estimación de 512 escalada a una tarjeta. Una lectura en streaming con bordes duros, donde bilineal y nearest discrepan visiblemente. |
| `blur.panel` | Un box blur de tres pasadas sobre un panel. Lectura-modificación-escritura con un búfer de trabajo. |
| `mixed.screen` | Los tres tipos en un solo lote. La granularidad a la que el despachador decide realmente, y la carga sobre la que se mide el coste de cambio. |

`glyphs.tiny` existe para ser un caso que la GPU pierde y `mixed.screen` existe para ser el caso en el
que una decisión para todo el lote o es correcta para todo él o es claramente errónea. Las formas se
eligen para hacer visible el arbitraje, no para parecerse a una pantalla real.

## 6. Ejecutarlo

```
cargo test --workspace
cargo build --workspace --release
cargo run --release --bin erl-raster-bench -- --help
```

El bench enlaza las canalizaciones que ofrezca esta compilación de la plataforma y conduce las cargas
publicadas a través de todas ellas bajo ambos regímenes.

```
usage: erl-raster-bench [--quick] [--thermal <state>] [--save <path>]

  --quick             run the short configuration: hot regime only, few iterations
  --thermal <state>   nominal, fair, serious, critical or unknown (default: unknown)
  --save <path>       write the device profile to a file when the run finishes

Only a run at nominal thermal pressure produces durations that may be compared with another run.
```

No decide nada. Imprime las canalizaciones que enlazó, una tabla de duraciones con mediana, percentil
noventa y cinco, recuento de píxeles y bytes movidos por carga, régimen y canalización, la lista de lo
que no se pudo medir con el motivo, la canalización más rápida por carga y régimen con el margen del
segundo, la canalización más rápida por tipo a partir de las cargas de un solo tipo, y los costes de
cambio medidos por par ordenado. Con `--save` escribe el perfil que el despachador leería después.

Se honran tres reglas al pie de la letra. El reloj pertenece al harness y nada se cronometra a mano. La
medición solo es absoluta a presión térmica nominal, que es por lo que el estado térmico viene de la
línea de comandos y no de una suposición, y por lo que el informe imprime una advertencia cuando no es
nominal. La primera ejecución en una máquina no tiene línea base y nunca se compara contra un perfil
almacenado.

Un argumento mal formado es un error, no un valor por defecto ignorado en silencio, porque un informe
producido bajo una configuración que nadie eligió es un informe que nadie puede interpretar.

## 7. En qué punto está el proyecto

Hecho y verificado:

- El workspace de Rust, cuatro crates, contratos estabilizados, `cargo test --workspace` en verde.
- El backend de CPU completo: referencia escalar y la fila vectorizada de composición en AArch64, ambos
  produciendo bytes idénticos.
- La referencia determinista de solo enteros a la que se someten los demás backends.
- El conjunto de cargas publicado y sin assets.
- El bench comparativo con ambos regímenes, el harness entrelazado, el calentamiento descartado, los
  costes de cambio medidos y el perfil de dispositivo versionado.
- El primer puente de GPU, en macOS a través de Metal. Reproduce la referencia escalar sobre el conjunto
  publicado, verificado por prueba.
- El runtime adaptativo: histeresis, el presupuesto de un movimiento por fotograma, ventanas críticas,
  el rechazo de cambios no medidos, el fallback nunca eliminado y la comprobación térmica del perfil.

Aún no:

- El puente de Android. Es el segundo adaptador detrás de la misma costura `GpuSurfaceBridge`, con su
  propia puerta de plataforma.
- La integración con la aplicación: un binding de JSI y TurboModule generado a partir de las mismas
  definiciones de interfaz, y la vista anfitriona, para que un componente de React Native pueda entregar
  un lote al runtime.
- La ruta zero-copy: `IOSurface` en iOS y `AHardwareBuffer` en Android, para la que la costura ya estaba
  modelada. Es una variante de la canalización por medir, no algo introducido de tapadillo en la línea
  base.
- Un runner de pruebas de conformidad que compare cada backend contra la referencia en todo el conjunto
  publicado con las tolerancias declaradas como datos en lugar de como pruebas.
- Integración continua que compile el workspace para `aarch64-apple-ios` y `aarch64-linux-android`.

## 8. Hoja de ruta

1. El puente Vulkan de Android, detrás de la costura existente.
2. El runner de conformidad, y la tabla de tolerancias como datos.
3. La vista anfitriona de React Native y su binding.
4. El benchmark embebido, limitado por cuota, que arranca *después* del primer fotograma y nunca antes,
   para que una primera sesión nunca se retrase por medirlo.
5. Ensayos multirresolución y bajo carga, incluida una carga competidora de GPU simulada, para que la
   trampa 3.3 se provoque a propósito en lugar de esperarse.
6. Las variantes zero-copy, medidas como variantes.
7. Una lectura publicada de latencia, fluidez y energía, frente a una ejecución sin la biblioteca.

## 9. Por qué no hay cifras en este archivo

Porque una duración tomada en un teléfono caliente describe un teléfono caliente. La propia regla del
proyecto es que ninguna medición es válida por encima de la presión térmica nominal, y una cifra copiada
en un README pierde lo único que la hacía significativa: la condición en la que se tomó. Las cifras
pertenecen al informe que imprime el bench y al perfil que guarda, junto al dispositivo, la plataforma y
el estado térmico en que se midieron. Ejecútalo en tu propio hardware.

## 10. Licencia

MIT OR Apache-2.0, a tu elección.
