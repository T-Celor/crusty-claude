# Crusty-Claude

[![CI](https://github.com/T-Celor/crusty-claude/actions/workflows/ci.yml/badge.svg)](https://github.com/T-Celor/crusty-claude/actions/workflows/ci.yml)

Agent de développement en Rust pour le terminal, inspiré de Claude Code : **multi-fournisseur**,
**multi-agent** (l'agent principal est *toujours* un orchestrateur qui délègue à des exécuteurs
spécialisés, chacun avec son modèle), **optimisé en tokens** (RTK, HTML→Markdown, Graphify,
Headroom, Context Gate, pxpipe et la politique Ponytail, tous natifs en Rust) et **visible** :
une TUI complète affiche en temps réel ce que fait chaque agent et ce que chaque optimiseur
fait gagner.

> **État du projet — à lire.** Tout ce qui est décrit ici est implémenté, compilé et couvert par
> des tests automatisés (≈ 500 tests, voir [docs/testing.md](docs/testing.md)). En revanche, **aucun
> appel réel à l'API Anthropic ou OpenAI n'a été effectué pendant le développement** (pas de clé) :
> les fournisseurs sont testés contre des serveurs simulés reproduisant le protocole (SSE, outils,
> erreurs, reprises). Les gains de tokens affichés sont des **estimations** (4 caractères ≈ 1 token)
> tant que la mesure réelle (`measure_with_count_tokens`) n'est pas activée. Les fonctions absentes
> sont listées plus bas, sans détour.

---

## Sommaire

- [Ce que fait Crusty-Claude](#ce-que-fait-crusty-claude)
- [Installation](#installation)
- [Démarrage rapide](#démarrage-rapide)
- [L'interface (TUI)](#linterface-tui)
- [Orchestrateur, exécuteurs et modèles par domaine](#orchestrateur-exécuteurs-et-modèles-par-domaine)
- [Le pipeline d'optimisation](#le-pipeline-doptimisation)
- [Proxies d'économie de tokens (agnostiques)](#proxies-déconomie-de-tokens-agnostiques)
- [Permissions et sécurité](#permissions-et-sécurité)
- [Commandes slash](#commandes-slash)
- [Ligne de commande](#ligne-de-commande)
- [Compatibilité Claude Code](#compatibilité-claude-code)
- [Fonctionnalités absentes ou limitées](#fonctionnalités-absentes-ou-limitées)
- [Développement](#développement)
- [Licence et crédits](#licence-et-crédits)

## Ce que fait Crusty-Claude

```
TUI (ratatui)  ──UiCommand──▶  Contrôleur de session  ──▶  Orchestrateur (toujours)
   ▲                                                         │  Task(domaine, prompt)
   │ AppEvent (typés, temps réel)                            ▼
   └──────── métriques ◀──────────────────────────────  Exécuteurs : coder, reviewer, tester,
                                                         designer, graphiste, architect, security,
                                                         web_researcher, documentation, scout
                                                             │
                     outils ◀── moteur de permissions ◀──────┤   (Read/Write/Edit/Glob/Grep/LS,
                     (sandbox, risques, approbations)        │    Bash, WebFetch/WebSearch, Graph,
                                                             │    TodoWrite, Retrieve, MCP…)
                                                             ▼
    sorties d'outils ─▶ RTK · HTML→Markdown            requête modèle ─▶ Headroom ─▶ Context Gate
                                                                          (fonction de gain) ─▶ pxpipe
                                                             ▼
                                Fournisseurs : Anthropic · OpenAI-compatibles (OpenAI, OpenRouter,
                                Gemini, Ollama, vLLM…) · mock hors ligne — via proxies optionnels
```

- **Orchestrateur/exécuteur imposé** : l'orchestrateur ne dispose que d'outils de lecture, de
  `TodoWrite` et de `Task` ; toute modification passe par un exécuteur. Délégations parallèles,
  boucles de revue limitées, profondeur limitée, budgets en tokens et en dollars par agent.
- **Modèle par domaine** : chaque domaine (codeur, designer, graphiste, reviewer…) a son
  fournisseur, son modèle, son effort, ses outils et ses modèles de repli ; modifiable dans la
  config, en ligne de commande ou à chaud (`/domain coder openai:gpt-…`).
- **Ponytail natif pour tous les agents** : la politique d'ingénierie Ponytail (MIT) est injectée
  dans le prompt système de l'orchestrateur et de chaque exécuteur lancé (niveaux lite/full/ultra).
- **Optimiseurs natifs mesurés** : chaque étape émet ses mesures (octets, tokens estimés avant/après,
  durée, statut appliqué/observé/ignoré/repli), affichées en direct dans le panneau latéral.
- **Proxies agnostiques** : n'importe quel proxy compatible (Headroom, pxpipe, context-gate,
  LiteLLM…) s'ajoute par configuration ou à chaud (`/proxy add`), avec santé et statistiques
  visibles ; `crusty proxy` expose aussi le pipeline de Crusty à d'autres clients (Claude Code inclus).

## Installation

Prérequis : Rust **1.88+** (édition 2024).

```bash
git clone https://github.com/T-Celor/crusty-claude
cd crusty-claude
cargo install --path crates/crusty-cli
```

Le binaire s'appelle `crusty`. Linux et macOS : rien de plus (TLS natif ; sous Linux il faut
`libssl-dev`/`openssl-devel`). Windows : la toolchain MSVC standard fonctionne. Sans Visual Studio,
la toolchain `x86_64-pc-windows-gnullvm` avec [llvm-mingw](https://github.com/mstorsjo/llvm-mingw)
dans le `PATH` fonctionne aussi (c'est celle utilisée pour développer ce dépôt).

## Démarrage rapide

```bash
export ANTHROPIC_API_KEY=...        # ou OPENAI_API_KEY, OPENROUTER_API_KEY, GEMINI_API_KEY…
crusty                              # TUI dans le dossier courant
```

```bash
crusty "explique l'architecture de ce dépôt"          # TUI avec un premier prompt
crusty -p "liste les TODO du projet"                  # non interactif, réponse sur stdout
git diff | crusty -p "revois ce diff" --output-format json
crusty --continue                                      # reprendre la dernière session
```

Démo **entièrement hors ligne** (fournisseur simulé, aucune clé) :

```bash
crusty --config config/offline.toml
```

Vérifier l'installation, les clés et les outils externes optionnels :

```bash
crusty doctor
```

Créer une config de projet commentée :

```bash
crusty config init
```

## L'interface (TUI)

```
Crusty-Claude │ 1a2b3c4d │ mode: default │ orchestrator: anthropic:claude-opus-5-5 │ ctx 12%
┌ Chat · Exec · Log ─────────────────────────────┐┌ Optimizer (live) ──────────────────────────┐
│ > ajoute des tests au parseur                  ││ ● Ponytail  ponytail 5.3.2 (full) · 4 agents│
│ ● Je délègue l'écriture des tests à coder…     ││ ● RTK       12 runs  -8.4k  -61%           │
│ ↳ delegated to coder (claude-sonnet-5-5)       ││ ● HTML→MD    2 runs  -3.1k  -78%           │
│   ⏺ Read(src/parser.rs) ✓ 0.1s                 ││ ● Graphify   5 runs  …  (est.)             │
│   ⏺ Bash(cargo test parser) ✓ 4.2s             ││ ● Headroom   3 runs  -21k   -34%           │
│ ⎿ coder: 6 tests ajoutés, tous passent         ││ ● Gate       last: direct (zone_low)       │
│                                                ││ ◌ pxpipe     idle                          │
│                                                ││ saved ≈ 32.5k tok (est.) · cost $0.0421    │
│                                                │├ Agents ────────────────────────────────────┤
│                                                ││ ✓ coder-1   sonnet-5-5   18.2k  $0.012     │
│                                                │├ Plan ──────────────────────────────────────┤
│                                                ││ ☑ lire le parseur  ◐ écrire les tests      │
└────────────────────────────────────────────────┘└────────────────────────────────────────────┘
> _
⏎ send · ⇧⏎ newline · ⇧⇥ mode · esc interrupt · ^T panel · /help
```

- **Panneau latéral** : une ligne par étape (Ponytail, RTK, HTML→MD, Graphify, Headroom, Gate,
  pxpipe, Proxy) avec exécutions, tokens économisés, ratio estimé et ratio **réel** quand il est
  mesuré ; dernière décision du Context Gate ; santé des proxies ; totaux réels (tokens, cache,
  coût). Les estimations sont toujours marquées `est.` ; rien n'est affiché qui ne vienne d'un
  événement réel de l'application.
- **Agents** : chaque exécuteur actif ou terminé, son domaine, son modèle, ses tokens et son coût.
- **Approbations** : fenêtre modale avec niveau de risque, raison, aperçu de la commande ou du diff ;
  `y` une fois, `a` toujours (ajoute la règle proposée), `n` refuser, `f` refuser avec un message.
- **Touches** : `Entrée` envoyer, `Maj+Entrée`/`Alt+Entrée`/`Ctrl+J` nouvelle ligne, `Échap`
  interrompre, `Maj+Tab` changer de mode de permission, `Ctrl+T` panneau, `Ctrl+O` réflexion,
  `F1/F2/F3` onglets Chat/Exec/Log, `PgUp/PgDn` défilement, `Tab` complétion des commandes,
  `Ctrl+C` deux fois pour quitter. Les messages tapés pendant un tour sont mis en file.

## Orchestrateur, exécuteurs et modèles par domaine

Valeurs par défaut (modifiables) :

| Rôle | Modèle | Effort | Outils |
|---|---|---|---|
| orchestrator | anthropic:claude-opus-5-5 | xhigh | Read, Glob, Grep, LS, Graph, Retrieve, WebSearch, WebFetch, TodoWrite, Task |
| coder, tester, designer, graphiste, reviewer | claude-sonnet-5-5 | medium | tous (reviewer : lecture + Bash) |
| architect, security | claude-opus-5-5 | high | lecture (+ Bash pour security) |
| web_researcher, documentation, scout | claude-haiku-5-5 | low | selon le domaine |

Changer un modèle :

```toml
# .crusty/config.toml
[domains.coder]
provider = "openai"
model = "gpt-5-codex"
effort = "high"
fallback = ["anthropic:claude-sonnet-5-5"]

[domains.graphiste]            # domaine existant ou nouveau
description = "Visuels, SVG, palettes"
provider = "ollama"
model = "qwen3-coder"
```

```bash
crusty --model anthropic:claude-fable-5-1 --domain-model coder=openrouter:qwen/qwen3-coder
```

Dans la TUI : `/model anthropic:claude-opus-5-5 high`, `/domain designer openai:gpt-5 medium`,
`/domain designer reset`, `/models`. Des agents personnalisés au format Claude Code
(`.claude/agents/*.md`, `.crusty/agents/*.md`) deviennent des domaines.

## Le pipeline d'optimisation

| Étape | Où | Ce qu'elle fait | Natif / externe |
|---|---|---|---|
| **Ponytail** | prompt système de chaque agent | règles « plus petit changement complet », vérifications, raccourcis documentés | natif (texte MIT embarqué) |
| **RTK** | sortie de `Bash` | supprime le bruit (ANSI, barres de progression, « Compiling… »), regroupe git status/log/diff, tests, npm, listings, grep ; **ne supprime jamais une ligne d'erreur** | natif ; `prefer_external` délègue au binaire `rtk` |
| **HTML→Markdown** | `WebFetch` | `html-to-markdown-rs`, conserve titres, tableaux, code, liens, métadonnées (titre, canonique, date) | natif |
| **Graphify** | outil `Graph` + carte du dépôt | graphe de connaissances (fichiers, symboles, imports, références inférées, communautés, nœuds centraux), incrémental, `GRAPH_REPORT.md` | natif ; lit aussi `graphify-out/GRAPH_REPORT.md` |
| **Headroom** | requête | compresse les anciens résultats d'outils (JSON écrasé en gardant les anomalies, logs dédupliqués en gardant les erreurs), déduplique, garde les N derniers messages intacts ; frontière **par lots** pour préserver le cache de prompt | natif ; `mode = "external"` envoie au proxy Headroom |
| **Context Gate** | requête | fonction de gain portée de `context-gate.mjs` et durcie (voir ci-dessous) ; décide si pxpipe vaut le coup | natif |
| **pxpipe** | requête, si le gate le décide | rend l'ancien contexte volumineux en pages PNG monospaces (coût image = largeur×hauteur/750) avec une « fiche » des valeurs exactes | natif ; `mode = "external"` route vers le proxy pxpipe |

Tous les originaux raccourcis restent récupérables : l'agent appelle `Retrieve(handle="orig_…")`.
`optimizer.mode = "observe"` mesure tout sans rien appliquer ; `--no-optimizer` désactive tout.

**Context Gate, plus robuste et plus limitant que l'original** : fenêtre = vraie fenêtre du modèle
(pas 200 000 fixe) ; liste blanche de modèles capables de lire des images ; garde « verbatim »
(refuse d'imager du contenu dense en hachages, UUID, clés) ; économie minimale ; disjoncteur
après échecs répétés ; hystérésis de sortie ; marge adaptative selon l'erreur de prédiction
observée ; sessions collantes revérifiées à chaque requête ; dry-run mis en cache et compté
dans le disjoncteur s'il échoue ; calibration ignorée sans usage réel ; plafond de 20 pages.

**Mesures réelles** : avec `optimizer.measure_with_count_tokens = true` (Anthropic), la requête
d'origine est comptée par l'endpoint gratuit `count_tokens` et comparée à l'usage réel facturé :
le panneau affiche alors `real -x%` à côté de l'estimation.

**Compatibilité « preserved thinking »** : sur les modèles Claude récents, réécrire un ancien tour
invalide les blocs de réflexion suivants. Crusty envoie `thinking.block_binding =
{prefix_mismatch_behavior: "drop_block"}` (et l'en-tête bêta requis) pour que l'API ignore ces blocs
au lieu de rejeter la requête ; Headroom n'avance sa frontière que par lots pour limiter ces pertes.

## Proxies d'économie de tokens (agnostiques)

Tout proxy compatible Anthropic ou OpenAI s'ajoute sans code :

```toml
[[proxy.upstreams]]
name = "headroom"
url = "http://127.0.0.1:8787"
kind = "headroom"            # generic | headroom | pxpipe | context-gate | crusty
health_path = "/livez"
stats_path = "/stats"         # JSON affiché dans le panneau

[providers.anthropic]
via_proxy = "headroom"        # tous les appels de ce fournisseur passent par lui
```

À chaud : `/proxy add headroom http://127.0.0.1:8787 headroom`, `/proxy route anthropic headroom`,
`/proxy route anthropic off`, `/proxy`. La santé (latence) et les statistiques de chaque proxy
s'affichent dans le panneau latéral. Un avertissement apparaît si un proxy Headroom et le
Headroom natif sont actifs en même temps (double compression).

**Le proxy intégré** expose le pipeline de Crusty à n'importe quel client :

```bash
crusty proxy                              # écoute sur 127.0.0.1:47840
ANTHROPIC_BASE_URL=http://127.0.0.1:47840 claude
```

Il affiche une ligne par requête (décision du gate, gains, usage, coût) et répond à `/livez` et
`/stats`. Pour reproduire la chaîne de [token-optimizer](https://github.com/T-Celor/token-optimizer)
(Headroom externe puis gate puis pxpipe), faites pointer l'amont de Headroom vers `crusty proxy`.

## Permissions et sécurité

- **Modes** (`Maj+Tab`, `/mode`, `--permission-mode`) : `default`, `acceptEdits`, `plan` (lecture
  seule), `bypassPermissions` — qui **ne désactive jamais** les protections : le risque élevé
  reste demandé sauf règle explicite, le critique reste refusé sauf règle explicite (puis demandé).
- **Règles** façon Claude Code : `Bash(cargo test:*)`, `Edit(src/**)`, `Read(./docs/**)`,
  `WebFetch(domain:docs.rs)`, `mcp__github`. Ordre : refus → mode plan → critique → demande →
  risque élevé → autorisation → modes → faible risque.
- **Risque calculé sur l'effet réel** : `git -c core.fsmonitor=…`, `rg --pre`, `find -exec`,
  `python -c`, les lectures hors du projet ou de fichiers d'identifiants ne sont jamais « faibles ».
- **Sandbox** : chemins canonicalisés (liens symboliques résolus), confinement au projet et aux
  `--add-dir`, fichiers sensibles (`.env`, clés, `.ssh`, `.git/config`…) critiques, fichiers de
  configuration d'agent (`.crusty/**`, `.claude/**`, `.mcp.json`, `.git/hooks/**`) critiques en écriture.
- **Lecture avant écriture** : un fichier existant doit avoir été lu (et non modifié depuis).
- **Projet de confiance** : une config de projet (dépôt cloné) ne peut définir fournisseurs,
  proxies, hooks, serveurs MCP ni assouplir les permissions tant que vous n'avez pas lancé
  `crusty trust` dans ce dossier.
- **Secrets** : jamais dans les fichiers de config (rejetés) ; retirés de l'environnement des
  commandes ; protection SSRF de `WebFetch` (DNS épinglé, redirections revérifiées, pas de proxy
  système, redirection vers un autre hôte non suivie).

Détails : [docs/security.md](docs/security.md).

## Commandes slash

| Commande | Effet |
|---|---|
| `/help` | commandes et touches |
| `/clear`, `/compact [instructions]` | nouvelle conversation, résumé de l'historique |
| `/model [provider:model] [effort]`, `/models`, `/agents`, `/domain …` | modèles de l'orchestrateur et des domaines |
| `/cost`, `/context`, `/status`, `/config` | usage et coût, remplissage du contexte, état, config effective |
| `/permissions [allow <règle>]`, `/mode […]` | règles et mode de permission |
| `/memory`, `#note` | fichiers mémoire ; `#…` ajoute une note à `CRUSTY.md` |
| `/init`, `/review [cible]` | crée `CRUSTY.md` ; revue des changements |
| `/resume [id]`, `/sessions`, `/export [fichier]` | sessions sauvegardées, export Markdown |
| `/proxy …`, `/optimizer …`, `/gate`, `/graph …`, `/ponytail …` | proxies, étapes d'optimisation, décisions du gate, graphe de code, niveau Ponytail |
| `/mcp`, `/todos`, `/doctor`, `/exit` | serveurs MCP, plan courant, diagnostic, quitter |
| `!commande` | exécute une commande shell et l'ajoute au contexte |
| commandes perso | `.crusty/commands/*.md`, `.claude/commands/*.md` (`$ARGUMENTS`, `$1`…) |

## Ligne de commande

```
crusty [PROMPT] [options]
  -p, --print                 répondre sans TUI (stdin accepté)
  --output-format text|json|stream-json
  -c, --continue / -r, --resume [ID]
  --model provider:model      --effort low|medium|high|xhigh|max
  --domain-model DOMAINE=provider:model
  --permission-mode MODE      --dangerously-skip-permissions
  --allowed-tools RÈGLES      --disallowed-tools RÈGLES
  --add-dir DIR               --config FICHIER     --mcp-config FICHIER
  --system-prompt TEXTE       --append-system-prompt TEXTE
  --max-turns N               --no-optimizer       --optimizer-mode active|observe
  --cwd DIR                   --verbose            --debug

crusty proxy [--listen ADDR] [--forward URL]
crusty graph build|map|find Q|explain X|report
crusty config show|path|init|validate
crusty doctor | crusty trust [--remove] | crusty sessions | crusty mcp | crusty models [PROVIDER]
```

## Compatibilité Claude Code

Lu tel quel : `CLAUDE.md` / `CRUSTY.md` (utilisateur, dossiers parents, projet, `*.local.md`,
imports `@fichier`), `.claude/commands/**.md`, `.claude/agents/*.md`, `.mcp.json` (projet de
confiance), événements de hooks (`PreToolUse`, `PostToolUse`, `UserPromptSubmit`, `Stop`,
`SubagentStop`, `SessionStart`, `SessionEnd`, `PreCompact`, `Notification`) avec sorties JSON
(`decision`, `continue`, `hookSpecificOutput`) et code de sortie 2. Noms et schémas d'outils
identiques (`Read`, `Write`, `Edit`, `MultiEdit`, `Glob`, `Grep`, `LS`, `Bash`, `BashOutput`,
`KillShell`, `WebFetch`, `WebSearch`, `TodoWrite`, `NotebookEdit`, `Task`).

## Fonctionnalités absentes ou limitées

- **Non testé contre les vraies API** (voir l'encadré en tête) ; protocole vérifié sur serveurs simulés.
- Pas de fournisseurs Bedrock / Vertex natifs (signatures AWS/GCP) ; les passerelles compatibles
  OpenAI ou Anthropic fonctionnent via `base_url`.
- Pas de points de restauration (« checkpoints » / `/rewind`), pas d'intégration IDE, pas de
  mode vim de l'éditeur, pas de styles de sortie, pas de « skills » ni de plugins.
- `WebFetch` ne lit pas les PDF et ne passe pas la page à un petit modèle (la question est jointe
  au contenu pour l'agent appelant). `WebSearch` nécessite Brave, Tavily, SearXNG ou la recherche
  côté serveur d'Anthropic.
- Graphify natif : extraction par expressions régulières (pas tree-sitter), donc heuristique ;
  pas de visualisation HTML ni d'export Neo4j de l'outil Python d'origine.
- pxpipe natif : police bitmap Noto Sans Mono 16 px (moins dense que l'original), blocs contenant
  des caractères hors police laissés en texte ; la fidélité de lecture par le modèle n'a pas été
  mesurée — c'est pourquoi le gate le réserve aux contextes proches de la limite.
- Les commandes lancées en arrière-plan ne tuent pas leurs petits-enfants à l'interruption
  (pas de groupes de processus sans code `unsafe`).
- Le passage en mode observe à chaud (`/optimizer observe`) vaut pour les requêtes ; les filtres
  de sortie d'outils suivent le mode de la configuration.

## Développement

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
```

Le dépôt est un workspace de 17 crates ; [docs/architecture.md](docs/architecture.md) décrit leurs
rôles et leurs contrats. Les tests sont décrits dans [docs/testing.md](docs/testing.md),
les intégrations externes dans [docs/integrations.md](docs/integrations.md).

## Licence et crédits

MIT — voir [LICENSE](LICENSE). Crusty-Claude réimplémente en Rust des idées de
[RTK](https://github.com/rtk-ai/rtk), [Headroom](https://github.com/headroomlabs-ai/headroom),
[pxpipe](https://github.com/teamchong/pxpipe), [Graphify](https://github.com/blockflix/Graphify) et
[context-gate](https://github.com/T-Celor/token-optimizer/blob/main/scripts/context-gate.mjs) sans en
inclure le code ; il embarque le texte de [Ponytail](https://github.com/DietrichGebert/ponytail)
(MIT, © 2026 DietrichGebert). Inventaire des licences : [docs/licenses.md](docs/licenses.md).
