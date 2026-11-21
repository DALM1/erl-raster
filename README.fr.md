# erl-raster

**Rastérisation adaptative pour mobile : mesurer quel moteur est le plus rapide, puis laisser l'appareil décider.**

`erl-raster` est un petit workspace Rust qui répond à une seule question, honnêtement, sur la machine sur
laquelle il s'exécute : *pour ce morceau de dessin, le CPU ou le GPU est-il le plus rapide en ce moment,
et quand vaut-il la peine de basculer ?* Il ne remplace pas un moteur graphique et il n'en intercepte
aucun. Il rastérise des feuilles, mesure ce que cela coûte, et arbitre.

Lire ce document dans une autre langue :
[English](README.md) | [中文](README.zh-CN.md) | **Français** | [Español](README.es.md) | [Русский](README.ru.md)

- [1. Le problème que nous avons rencontré](#1-le-problème-que-nous-avons-rencontré)
- [2. Ce qu'est ERL Raster, et ce qu'il refuse d'être](#2-ce-quest-erl-raster-et-ce-quil-refuse-dêtre)
- [3. Les quatre pièges autour desquels la conception est bâtie](#3-les-quatre-pièges-autour-desquels-la-conception-est-bâtie)
- [4. L'implémentation, crate par crate](#4-limplémentation-crate-par-crate)
- [5. La charge publiée](#5-la-charge-publiée)
- [6. Comment l'exécuter](#6-comment-lexécuter)
- [7. Où en est le projet](#7-où-en-est-le-projet)
- [8. Feuille de route](#8-feuille-de-route)
- [9. Pourquoi il n'y a pas de chiffres dans ce fichier](#9-pourquoi-il-ny-a-pas-de-chiffres-dans-ce-fichier)
- [10. Licence](#10-licence)

---

## 1. Le problème que nous avons rencontré

Nous développons une application mobile en React Native. Le défilement d'une liste de conversations
n'était pas fluide, et l'ouverture d'une conversation prenait un temps de trop avant que la première
image ne se stabilise. Rien dans le profil ne disait « le fil JavaScript », rien ne disait « la mise en
page », et rien ne disait « le réseau ». Les images se perdaient là où une application native les perd,
dans la rastérisation, et c'est la seule partie du pipeline que notre pile n'avait jamais eu de raison
d'examiner.

En le poursuivant, nous avons abouti à une constatation qui, en réalité, ne concerne pas du tout notre
application.

**Les deux plateformes permettent à une application de rastériser un contenu sur le CPU ou sur le GPU,
et aucune des deux ne mesure lequel est le plus rapide.**

- Sur iOS, `CALayer.draw(in:)` fournit un contexte CPU, un `CAMetalLayer` fournit un drawable Metal, et
  `shouldRasterize` déplace le coût d'un sous-arbre vers une rastérisation hors écran ponctuelle. Ce
  sont de vrais choix de pipeline, et ils existent une couche à la fois, à la main.
- Sur Android, `View.setLayerType(LAYER_TYPE_SOFTWARE)` force un sous-arbre sur le CPU, un
  `SurfaceView` ou un `TextureView` cède une surface que vous possédez, et `RenderEffect` exécute le
  travail de flou et de couleur sur le GPU à l'intérieur du pipeline matériel. Là encore, de vrais
  choix, faits à la main, une vue à la fois.

Ainsi, le levier existe. Ce qui n'existe pas, c'est quoi que ce soit qui l'actionne. Un développeur qui
veut savoir si un flou doit s'exécuter sur le CPU ou sur le GPU de ce téléphone doit écrire un benchmark,
l'exécuter, espérer que le téléphone n'était pas chaud, puis coder en dur la réponse pour un appareil
qu'il ne reverra jamais.

Et écrire ce benchmark correctement est étonnamment difficile. Nos propres premières tentatives étaient
fausses de manières qui produisaient des chiffres assurés et inutiles :

- Un noyau mesuré sur un tampon qu'il a déjà touché est jusqu'à cinq fois plus rapide que le même noyau
  sur un tampon qu'il vient de recevoir, si bien que la boucle naïve mesure le cache et l'appelle le
  calcul.
- Exécuter tout un pipeline puis tout l'autre offre au second le bénéfice d'une horloge qui a monté en
  régime et au premier le coût de l'avoir fait monter.
- Un téléphone qui a bridé ses performances mesure son moi bridé, et un profil pris dans cet état décrit
  un appareil qui n'existe plus.
- Basculer de pipeline n'est pas gratuit. Liaison, descripteurs, barrière, parfois un transfert. Un
  répartiteur qui se déplace sur un gain de cinq pour cent peut facilement passer plus de temps à se
  déplacer qu'à dessiner.

Le défaut de performance qui a tout déclenché est désormais compris et corrigé dans l'application
elle-même. Ce qu'il a mis au jour, c'est la lacune que ce dépôt comble : l'arbitrage entre le CPU et le
GPU est une décision que chaque pile de rendu prend, mal, à la main.

## 2. Ce qu'est ERL Raster, et ce qu'il refuse d'être

`erl-raster` est un **moteur de rastérisation doté d'une couche d'arbitrage**. Vous décrivez un lot de
dessin pour une image et vous le remettez avec une cible ; la bibliothèque l'exécute sur celle de ses
pipelines dont on prédit qu'elle est la plus rapide, dans les conditions où l'appareil se trouve
réellement.

Ce n'est pas un framework, ni un compositeur, ni un remplacement pour Core Animation, HWUI ou Skia. Il
ne peut pas intercepter les appels de dessin d'un autre framework, et il ne l'essaie pas. La question de
la compatibilité est réglée par conception plutôt que par espoir :

- **Une API, jamais un crochet.** Pas de swizzling, pas de remplacement de classe, pas de comportement
  indéfini. C'est ce qui le rend distribuable, sur un magasin d'applications et sur un registre de
  paquets.
- **Un repli CPU permanent, jamais retiré.** Le chemin CPU scalaire fait toujours partie de l'ensemble
  des candidats. Le pire cas d'adoption de la bibliothèque est qu'elle s'exécute exactement aussi vite
  que la référence.
- **Un chemin de référence déterministe.** La référence CPU est en entiers uniquement, de bout en bout,
  si bien qu'une comparaison de conformité est bit à bit plutôt qu'approximative. Un backend qui diverge
  est faux, et non « suffisamment proche », et il est désactivé plutôt que toléré en silence.

La version en une phrase : *écrivez le dessin une fois, et laissez l'arbitrage choisir le moteur.*

## 3. Les quatre pièges autour desquels la conception est bâtie

### 3.1 Un benchmark faussé par le cache

La solution n'est pas une meilleure boucle, ce sont deux mesures et une règle sur celle qui s'applique.

- **Deux régimes, tous deux conservés.** `Regime::Hot` réutilise la cible et garde les sources
  résidentes, ce qui décrit une opération répétée sur des données déjà en vol, comme un atlas de glyphes
  redessiné à chaque image. `Regime::Cold` alloue une cible neuve et purge les caches avant l'itération,
  ce qui décrit la première image d'un écran ou une image décodée en cours de redimensionnement. Dans le
  régime froid, un tampon de seize mébioctets est écrit entre les itérations, ce qui dépasse le dernier
  niveau de cache de tous les téléphones et de la plupart des ordinateurs portables, et l'allocation se
  trouve à l'intérieur de la région mesurée, à dessein.
- **L'hôte choisit le régime, pas le benchmark.** Seul l'appelant sait si une image donnée répète un
  travail résident ou reçoit de nouvelles sources. C'est ce que porte `FrameContext::regime`.
- **Le préchauffage est jeté.** Les premières itérations d'un pipeline contiennent sa première
  allocation, sa première compilation de shader et le remplissage du cache de tout ce qui suit. La
  configuration de harnais par défaut écarte dix itérations avant d'en garder soixante.

### 3.2 Le coût du changement de pipeline

- **Le lot est l'unité de décision, non la primitive.** Un basculement paie la liaison, les mises à jour
  de descripteurs et la synchronisation ; une décision par glyphe coûterait plus que le glyphe. Un lot
  est le dessin d'une image, si bien qu'un basculement unique est amorti sur tout ce qu'il contient.
- **Le coût de basculement est mesuré, non estimé.** `measure_switch_costs` exécute la même charge
  immédiatement après une charge sur l'autre pipeline et soustrait le temps qu'elle prend après une
  charge sur le même, par paire ordonnée, car le coût n'est pas symétrique. `SwitchCostModel::between`
  renvoie `None` pour une paire qui n'a jamais été mesurée, et `None` n'est pas transformé en valeur par
  défaut : un déplacement non mesuré est refusé plutôt que supposé gratuit.
- **Au plus un déplacement de performance par image.** Un second déplacement de performance paierait
  deux fois le coût fixe à l'intérieur de l'image qu'il était censé améliorer. Un déplacement effectué
  parce que le titulaire *ne peut pas* exécuter le lot n'est pas soumis à ce budget, puisqu'il ne s'agit
  pas d'agitation.
- **Jamais pendant une fenêtre critique.** L'hôte déclare une image critique pendant qu'une animation
  est validée, pendant qu'une cible est partagée, ou pendant qu'une capture est en cours.
  `FrameContext::frozen` porte cela, et les preuves en faveur d'un changement cessent de s'accumuler
  plutôt que d'être jetées.

### 3.3 Une charge qui change sous le profil

- **Aucune tentative de lire la « utilisation du GPU ».** Elle n'est exposée aux applications sur
  aucune des deux plateformes. Un GPU occupé se manifeste plutôt comme une latence de soumission à
  achèvement instable, et c'est cette variance que le projet surveille, et non une jauge hypothétique.
- **L'état thermique fait partie de l'identité du profil, et non d'un champ à côté.** `SystemState`
  indexe un profil sur l'appareil, la plateforme et l'état thermique. `ThermalState::permits_absolute_measurement`
  n'est vrai que pour `Nominal`, et le runtime revient au repli et se méfie du profil dès l'instant où
  l'état ne correspond plus.
- **De l'hystérésis avec des chiffres écrits noir sur blanc.** Un prétendant doit surpasser le coût du
  titulaire de la valeur du coût de basculement mesuré plus une marge, par défaut dix pour cent du
  titulaire, et doit le faire sur trois évaluations consécutives avant de prendre la place. Une
  régression est une exécution qui atterrit à plus d'une fois et demie sa propre prédiction, quatre fois
  de suite.

### 3.4 Compatibilité

- **Trois niveaux d'intégration, du moins au plus invasif.** Un producteur d'images (`render(op) ->
  surface`) n'a besoin d'aucune intégration native et fonctionne dans un harnais de test sur un
  ordinateur de bureau. Une vue hôte est un composant Fabric sur React Native, une `UIView` au-dessus
  d'un `CAMetalLayer` sur iOS, une `View` au-dessus d'un `SurfaceView` sur Android, et est réservée au
  contenu qui change à chaque image. Les adaptateurs de plateforme sont de petites couches Swift,
  Objective-C++ et Kotlin pour ce que seul le langage de la plateforme peut atteindre : l'interface
  thermique, un effet de rendu, le type de couche d'une vue.
- **La conformité est vérifiée, non supposée.** Chaque backend est comparé à la référence en entiers,
  avec une tolérance explicite par opération là où le backend se déclare non exact au bit près. Un
  backend qui diverge est désactivé, et non approximé en silence.
- **Rien n'est découvert dans le dos de la plateforme.** Le registre contient ce qui a été explicitement
  lié dans la compilation, plus la référence. Un GPU qui n'est pas lié apparaît dans le rapport avec la
  raison de son absence, plutôt qu'avec un blanc.

## 4. L'implémentation, crate par crate

Le workspace compte quatre crates. Le MVP n'a **aucune dépendance tierce en dehors du pont GPU** : le
noyau, le benchmark et le répartiteur sont en `std` pur, ce qui garde la compilation open source
reproductible sur n'importe quelle machine et garde le chemin de l'image exempt de code susceptible de
le suspendre.

```
crates/
  erl-raster-core         les primitives et la référence déterministe
  erl-render-backends     les implémentations, derrière un seul contrat
  erl-benchmark           le moteur de mesure et le profil de l'appareil
  erl-adaptive-runtime    le répartiteur
```

### 4.1 `erl-raster-core`

Rastérisation indépendante de la plateforme, et la référence à laquelle tout backend est tenu.

- `op` définit ce qui peut être demandé. `OpKind` est `FillGlyphs`, `Resample` ou `Blur`, délibérément
  trois, parce que les trois se comportent de manière suffisamment différente pour qu'une seule réponse
  pour tous soit fausse pour deux : une séquence de glyphes est petite, dispersée et limitée par la
  latence ; un rééchantillonnage est une lecture en flux ; un flou est un balayage en deux passes avec
  un tampon temporaire. Chaque genre expose un coût `Work` en *unités de travail plutôt qu'en temps*,
  parce qu'un backend sait ce qu'on lui a demandé de faire et que seul le harnais peut dire combien de
  temps cela a pris. `Batch` est l'unité de décision, et `OpKindSet` est un ensemble de trois bits, afin
  que le répartiteur par image puisse le construire dans un registre et ne jamais allouer.
- `reference` est le chemin déterministe. Arithmétique entière de bout en bout : un noyau à virgule
  flottante peut différer entre une machine de compilation x86, un téléphone à silicium Apple et un
  appareil Android ARM, ce qui rendrait un test de conformité soit impossible à écrire, soit
  définitivement rouge. Les entiers donnent une égalité bit à bit sur les trois.
- `fixed` contient les assistants en virgule fixe, écrits de telle sorte qu'un registre NEON à huit
  voies reproduise exactement la valeur scalaire. `image` contient `Surface`, un tampon de texels serré
  de quatre octets, avec une empreinte peu coûteuse servant à dire *où* deux backends ont divergé.
  `glyph` contient `GlyphMask`, `GlyphSpan` et `GlyphRun`, avec la réutilisation de masques comme un
  cache de texte utilise un atlas. `geom` et `error` sont les types de support. `system` définit
  `SystemState` et `ThermalState`.
- `workload` construit la charge publiée, décrite à la section 5.

La crate porte `#![forbid(unsafe_code)]`. Les noyaux vectorisés qui ont besoin d'`unsafe` vivent dans les
backends, derrière le contrôle d'architecture qui les rend sûrs à appeler.

### 4.2 `erl-render-backends`

Un contrat, plusieurs implémentations.

- `Backend` est le trait : `capabilities`, `cost_model`, `prepare`, `execute`, `release`. `prepare` est
  l'endroit où un pipeline fait le travail qu'il ne devrait jamais faire qu'une fois, afin que la
  première exécution *chronométrée* soit représentative. `execute` exécute un lot entier contre une
  cible en mémoire hôte et renvoie un `Executed`.
- `Capabilities` décrit ce qu'un backend *est* : les genres qu'il sert, la plus grande surface qu'il
  accepte, s'il reproduit la référence octet pour octet, et si ré-entrer dedans coûte cher. Un backend
  qui ne sert qu'une partie d'un lot n'est pas candidat du tout, parce que scinder un lot sur deux
  pipelines coûte un basculement et une étape de composition, et le MVP ne le fait pas.
- `CostModel` décrit ce qu'un backend *coûte*, en quatre termes dont le répartiteur a besoin
  indépendamment parce qu'ils augmentent avec des choses différentes : un coût fixe par exécution, un
  coût par opération, un coût par pixel de travail et un coût par octet déplacé, les deux derniers en
  millièmes de nanoseconde afin qu'un pipeline rapide soit exprimable tout court. `from_measurement`
  reconstruit un modèle à partir d'une durée mesurée, et la sonde dédiée au coût fixe l'affine.
- `CpuBackend::scalar()` est la référence et le repli. `CpuBackend::neon()` n'existe que sur AArch64 et
  renvoie `None` ailleurs plutôt que de rendre le backend scalaire sous un nom NEON, parce qu'un backend
  qui déclarerait faussement quel pipeline il est empoisonnerait le profil. Les deux variantes sont un
  seul type distingué par un drapeau, et la version vectorisée remplace exactement une chose : la ligne
  de composition des glyphes, qui est l'endroit où les mesures situent le coût. Le rééchantillonnage est
  un rassemblement qui tire peu de profit de registres larges, et la normalisation du flou de boîte est
  une division entière sans forme parallèle par voies sur cette architecture, si bien que les deux
  restent sur le chemin de référence et que `Capabilities` le dit.
- `GpuBackend` est la couture plus l'appareil. `GpuSurfaceBridge` est trois appels, `device`, `prepare`,
  `execute`, sans astuces de durée de vie : une implémentation par plateforme, dans le langage de cette
  plateforme, derrière une fine couche FFI. La règle décisive est que la cible est de la **mémoire
  hôte** : le pont la téléverse, dessine, et télécharge le résultat, si bien que le transfert qui décide
  de la plupart des cas réels se trouve *à l'intérieur* de la région que le harnais chronomètre. Un
  backend GPU mesuré sur une texture qu'il possédait déjà afficherait un bien meilleur chiffre et
  décrirait une situation où l'appelant ne se trouve jamais. `GpuBackend::absent` existe pour qu'un
  rapport puisse dire *pourquoi* un GPU n'était pas candidat au lieu de l'omettre en silence.
- `wgpu_bridge` est le premier vrai pont, compilé uniquement sur macOS. Quatre décisions le façonnent :
  les noyaux sont des répartitions de calcul plutôt que des passes de rendu, parce qu'une séquence de
  glyphes doit lire le pixel qu'elle s'apprête à mélanger et qu'une passe de rendu ne peut pas lire sa
  propre attache ; tout est un tampon de stockage lu et écrit sur place plutôt qu'une texture, ce qui
  supprime une copie intermédiaire et une passe de suppression de foulée ; le `Work` rapporté est
  calculé sur l'hôte à partir de la même arithmétique de découpage que celle utilisée par la référence,
  parce que compter les pixels sur l'appareil ajouterait une lecture en retour à chaque image pour
  apprendre ce que l'hôte sait déjà ; et la règle de mesure est celle de la couture, téléversement et
  téléchargement à l'intérieur de la région.
- `registry` est l'endroit où le piège de compatibilité atterrit. Aucune découverte, aucun sondage.
  `default_registry()` renvoie d'abord la référence scalaire puis la variante vectorisée quand
  l'architecture en possède une ; `registry_with_gpu` ajoute un pont lié.

### 4.3 `erl-benchmark`

Le moteur de mesure, et la seule chose qui survit à un processus.

- `stats` refuse de rapporter une moyenne à elle seule. Médiane et quatre-vingt-quinzième centile sur un
  nombre fixe d'itérations, parce qu'une moyenne cache exactement ce qui est visible : les images
  longues. `is_conclusive` protège contre le fait de rapporter une distribution construite à partir de
  trop peu d'échantillons.
- `regime` est la séparation chaud/froid de la section 3.1.
- `harness` possède l'horloge. `HarnessConfig` prend par défaut dix itérations écartées puis soixante
  conservées, les deux régimes, et une purge de seize mébioctets. `run_trials` exécute chaque pipeline
  contre chaque charge, en entrelaçant les pipelines lot par lot afin qu'aucun n'ait la montée en régime
  de l'horloge. `measure_switch_costs` produit les coûts de basculement par paire ordonnée. Un `Trial`
  rapporte `Measured`, `Unavailable`, `Unsupported` ou `Failed`, avec une raison attachée, afin qu'un
  pipeline qui n'a pas tourné soit une constatation plutôt qu'une lacune.
- `profile` est ce sur quoi une session ultérieure répartit. `DeviceProfile` contient le `SystemState`
  dans lequel il a été pris, les entrées, et les coûts de basculement, et écrit dans un format texte
  versionné qu'un humain peut lire et qu'un diff peut examiner. `fastest_for_kind` répond à « qui a
  gagné le flou, à chaud, sur cet appareil » à partir des charges d'un seul genre, ce qui permet de
  prendre une décision sur un genre à partir d'une mesure d'une autre charge qui le contenait.

### 4.4 `erl-adaptive-runtime`

Là où le projet cesse de mesurer et commence à décider. Il ne contient pas de code de rendu : les
pipelines arrivent par le trait `Backend`.

- `FrameContext` est la séparation entre ce que seul l'hôte sait et ce que la bibliothèque détermine.
  L'hôte déclare le régime et si l'image est critique. Tout le reste, l'arbitrage lui-même, appartient à
  la bibliothèque, parce que ce n'est pas quelque chose qu'on devrait demander à une application de
  réussir. `AdaptiveRuntime::render` prend un lot et renvoie les octets de son rendu, et l'appelant ne
  nomme jamais de pipeline.
- `DispatchPolicy` est l'hystérésis en chiffres : `minimum_gain_percent` (10),
  `required_consecutive_wins` (3), `regression_percent` (150), `regression_window` (4), et
  `switching_enabled`, qui fige le titulaire et constitue l'interrupteur A/B que le projet utilise pour
  mesurer ce que vaut l'arbitrage. `gain_clears_the_bar` est une inégalité unique à dessein : le gain
  doit rembourser le déplacement *et* franchir la marge, afin qu'on ne puisse pas écrire de politique
  dans laquelle un déplacement soit à la fois approuvé et non rentable.
- `Decision` porte la raison pour laquelle elle a été prise, y compris les décisions de ne pas se
  déplacer, avec les quatre chiffres derrière elle : coût prédit du titulaire, coût prédit du prétendant,
  coût de déplacement mesuré, et victoires consécutives. C'est ce qui rend la politique testable sans
  téléphone, et ce qui rend un répartiteur qui se comporte mal débogable. Il y a onze raisons, parmi
  lesquelles `GainBelowSwitchCost`, `SwitchCostUnknown`, `ChallengerNotProven`, `CriticalWindow`,
  `SwitchBudgetSpent`, `StaleProfile`, `IncumbentCannotServe` et `UnavailableFallback`.
- `AdaptiveRuntime` détient cinq règles. Le titulaire n'est jamais abandonné parce qu'il est lent,
  seulement délogé par quelque chose de meilleur ; il n'existe pas d'état « aucun pipeline ». Un
  prétendant doit gagner de façon répétée. Un déplacement doit se rembourser lui-même en nanosecondes
  mesurées, et un déplacement non mesuré est refusé. Au plus un déplacement de performance par image. Le
  profil n'est utilisé que dans la condition dans laquelle il a été pris. Un runtime construit sans repli
  permanent est refusé à la construction plutôt que toléré.

## 5. La charge publiée

Le benchmark n'est comparable à lui-même que si le travail ne change jamais, aussi chaque charge est-elle
fixe, procédurale et dépourvue de tout fichier de police, fichier d'image ou ressource. Les masques de
glyphes proviennent d'un générateur initialisé par une graine, la source de rééchantillonnage de même,
et le générateur est un `xorshift64*` de dix-sept lignes, choisi précisément parce qu'aucune mise à
niveau du compilateur ne peut changer ce qu'il produit. Deux exécutions à six mois d'intervalle, sur un
ordinateur portable et sur un téléphone, dessinent exactement les mêmes pixels.

Toutes les cinq dessinent dans la même cible de référence, 1080 par 2340, un téléphone en portrait à
trois fois la densité, parce que la cible est ce qu'un backend doit allouer, effacer et remettre, et que
comparer entre des tailles de cible différentes replierait le coût de la cible dans la comparaison.

| Charge | À quoi elle sert |
|---|---|
| `glyphs.tiny` | Quatre masques courts. Le cas que le GPU perd : soumettre coûte plus que mélanger. |
| `glyphs.paragraph` | Un écran de texte. Beaucoup de petits masques sur un plan en grande partie transparent. |
| `resample.card` | Une estimation à 512 mise à l'échelle dans une carte. Une lecture en flux à bords nets, où bilinéaire et plus proche voisin divergent visiblement. |
| `blur.panel` | Un flou de boîte en trois passes sur un panneau. Lecture-modification-écriture avec un tampon temporaire. |
| `mixed.screen` | Les trois genres dans un seul lot. La granularité à laquelle le répartiteur décide réellement, et la charge sur laquelle le coût de basculement est mesuré. |

`glyphs.tiny` existe pour être un cas que le GPU perd et `mixed.screen` existe pour être le cas où une
seule décision pour tout le lot est soit juste pour l'ensemble, soit manifestement fausse. Les formes
sont choisies pour rendre l'arbitrage visible, non pour ressembler à un écran réel.

## 6. Comment l'exécuter

```
cargo test --workspace
cargo build --workspace --release
cargo run --release --bin erl-raster-bench -- --help
```

Le banc lie les pipelines que cette compilation de plateforme offre et fait passer les charges publiées
par tous, sous les deux régimes.

```
usage: erl-raster-bench [--quick] [--thermal <state>] [--save <path>]

  --quick             run the short configuration: hot regime only, few iterations
  --thermal <state>   nominal, fair, serious, critical or unknown (default: unknown)
  --save <path>       write the device profile to a file when the run finishes

Only a run at nominal thermal pressure produces durations that may be compared with another run.
```

Il ne décide rien. Il affiche les pipelines qu'il a liés, un tableau des durées avec médiane,
quatre-vingt-quinzième centile, nombre de pixels et octets déplacés par charge, régime et pipeline, la
liste de ce qui n'a pas pu être mesuré avec la raison, le pipeline le plus rapide par charge et par
régime avec la marge du second, le pipeline le plus rapide par genre à partir des charges d'un seul
genre, et les coûts de basculement mesurés par paire ordonnée. Avec `--save`, il écrit le profil que le
répartiteur lira plus tard.

Trois règles sont honorées à la lettre. L'horloge appartient au harnais et rien n'est chronométré à la
main. La mesure n'est absolue qu'à pression thermique nominale, ce qui explique pourquoi l'état thermique
vient de la ligne de commande et non d'une supposition, et pourquoi le rapport affiche un avertissement
lorsqu'il n'est pas nominal. La première exécution sur une machine n'a pas de référence de base et ne se
compare jamais à un profil enregistré.

Un argument mal formé est une erreur, et non une valeur par défaut ignorée en silence, parce qu'un
rapport produit sous une configuration que personne n'a choisie est un rapport que personne ne peut
interpréter.

## 7. Où en est le projet

Terminé et vérifié :

- Le workspace Rust, quatre crates, contrats stabilisés, `cargo test --workspace` au vert.
- Le backend CPU complet : référence scalaire et ligne de composition vectorisée sur AArch64, produisant
  tous deux des octets identiques.
- La référence déterministe en entiers uniquement à laquelle les autres backends sont tenus.
- L'ensemble de charges publié, sans ressource.
- Le banc comparatif avec les deux régimes, le harnais entrelacé, le préchauffage écarté, les coûts de
  basculement mesurés et le profil d'appareil versionné.
- Le premier pont GPU, sur macOS via Metal. Il reproduit la référence scalaire sur l'ensemble publié,
  vérifié par test.
- Le runtime adaptatif : hystérésis, le budget d'un déplacement par image, les fenêtres critiques, le
  refus des bascules non mesurées, le repli jamais retiré et le contrôle thermique sur le profil.

Pas encore :

- Le pont Android. C'est le second adaptateur derrière la même couture `GpuSurfaceBridge`, avec sa propre
  porte de plateforme.
- L'intégration applicative : une liaison JSI et TurboModule générée à partir des mêmes définitions
  d'interface, et la vue hôte, afin qu'un composant React Native puisse remettre un lot au runtime.
- Le chemin sans copie : `IOSurface` sur iOS et `AHardwareBuffer` sur Android, pour lequel la couture a
  déjà été façonnée. C'est une variante du pipeline à mesurer, et non quelque chose glissé dans la base
  de référence.
- Un exécuteur de tests de conformité qui compare chaque backend à la référence sur tout l'ensemble
  publié, avec les tolérances déclarées comme données plutôt que comme tests.
- Une intégration continue qui compile le workspace pour `aarch64-apple-ios` et `aarch64-linux-android`.

## 8. Feuille de route

1. Le pont Android Vulkan, derrière la couture existante.
2. L'exécuteur de conformité, et la table de tolérances comme données.
3. La vue hôte React Native et sa liaison.
4. Le benchmark embarqué, limité par un quota, qui démarre *après* la première image et jamais avant,
   afin qu'une première session ne soit jamais retardée par sa mesure.
5. Des essais multi-résolution et sous charge, y compris une charge concurrente simulée sur le GPU, afin
   que le piège 3.3 soit provoqué à dessein au lieu d'être attendu.
6. Les variantes sans copie, mesurées en tant que variantes.
7. Une lecture publiée de la latence, de la fluidité et de l'énergie, face à une exécution sans la
   bibliothèque.

## 9. Pourquoi il n'y a pas de chiffres dans ce fichier

Parce qu'une durée prise sur un téléphone chaud décrit un téléphone chaud. La règle du projet est
qu'aucune mesure n'est valide au-dessus de la pression thermique nominale, et un chiffre recopié dans un
README perd la seule chose qui le rendait significatif : la condition dans laquelle il a été pris. Les
chiffres appartiennent au rapport que le banc imprime et au profil qu'il enregistre, à côté de l'appareil,
de la plateforme et de l'état thermique dans lesquels ils ont été mesurés. Exécutez-le sur votre propre
matériel.

## 10. Licence

MIT OR Apache-2.0, au choix.
