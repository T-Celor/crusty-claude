# Plan v0.2 — suivi de réalisation

Ce fichier est la trace de travail de la version 0.2. Chaque point passe par
`[ ]` à faire → `[~]` en cours → `[x]` fait (compilé, testé, revu). Un point n'est
coché `[x]` qu'avec une preuve : tests qui passent, clippy propre, revue faite.

Légende des vagues : **V0** contrats partagés (orchestrateur), **V1** crates
indépendantes en parallèle, **V2** intégration, **V3** revues, docs, CI, publication.

## A. Limites restantes de la v0.1

| # | Point | Où | Vague | État |
|---|---|---|---|---|
| A1 | Isolation réelle des commandes (sandbox OS) : bubblewrap ou Landlock (Linux), Seatbelt (macOS), Job Object (Windows : arbre de processus tué, pas d'isolation fichiers/réseau) | `crusty-sandbox` (nouvelle), `crusty-shell` | V1 + V2 | [x] |
| A2 | Graphify avec de vrais parseurs (tree-sitter) au lieu des regex | `crusty-graph` | V1 | [x] |
| A3 | pxpipe pour tout Unicode (repli GNU Unifont : CJK, cyrillique, arabe, symboles…) | `crusty-gate` | V1 | [x] |
| A4 | Gains mesurés et non estimés : `count_tokens` actif par défaut (Anthropic, Bedrock, Vertex), comptage local exact pour OpenAI (tiktoken), affichage « mesuré » / « est. » | `crusty-providers`, `crusty-pipeline`, `crusty-tui` | V1 + V2 | [x] |
| A5 | MCP : transport SSE (HTTP+SSE historique) | `crusty-mcp` | V1 | [x] |
| A6 | Checkpoints et `/rewind` (code, conversation, ou les deux) | `crusty-session`, `crusty-tools`, `crusty-agent`, `crusty-tui` | V1 + V2 | [x] |
| A7 | Intégration IDE : `crusty acp` (Agent Client Protocol : Zed, JetBrains, Neovim, Emacs…) | `crusty-acp` (nouvelle), `crusty-cli` | V2 | [x] |
| A8 | Skills (`SKILL.md`, outil `Skill`, `/skills`) et plugins (`plugin.json` : commandes, agents, skills, hooks, MCP ; `crusty plugin …`) | `crusty-session`, `crusty-agent`, `crusty-cli` | V1 + V2 | [x] |
| A9 | Fournisseurs Amazon Bedrock (SigV4, event-stream) et Google Vertex AI (ADC, compte de service) | `crusty-providers` | V1 | [x] |
| A10 | Compilation sans MSVC : binaires précompilés publiés par la CI (Windows MSVC, Linux, macOS) + script d'installation de la toolchain GNU | `.github/workflows`, `scripts/` | V1 | [x] |

## B. Intégration de Laya (modèle de décision local, `laya-serve`)

Laya répond à des questions typées (`choice`, `score`, `noul`) via `POST /v1/systemone`.
Crusty l'appelle via un client HTTP ; si `laya-serve` est absent, chaque point a un
**repli natif** déterministe. Laya ne peut **jamais assouplir** une décision de sécurité
native : il ne peut que la durcir (sauf en mode `auto`, où il décide seulement des
demandes d'approbation, jamais du risque critique).

| # | Point d'intégration | Laya | Repli natif | Vague | État |
|---|---|---|---|---|---|
| B0 | Client `laya-serve` (santé, latence, batch), trait `Advisor` du cœur, panneau latéral | — | — | V0 + V1 | [x] |
| B1 | Intercepteur de commandes Bash | `allow/review/block` | analyseur de risque existant | V2 | [x] |
| B2 | DLP sortant : masquage des secrets et PII vers l'API, réhydratation locale des entrées d'outils | `noul` « contient un secret ? » sur les cas ambigus | motifs + entropie + Luhn/IBAN | V1 + V2 | [x] |
| B3 | Vérificateur d'écritures sensibles (`.env`, `.gitignore`, CI/CD, Dockerfile…) | `allow/review/block` | chemins sensibles + règles de diff | V2 | [x] |
| B4 | Reranker de fragments (Grep, Graph) | pertinence `noul` en batch | BM25 sur identifiants | V2 | [x] |
| B5 | Épurateur d'historique (lectures périmées, messages obsolètes) | pertinence vs objectif | lectures remplacées, doublons | V2 | [x] |
| B6 | Triage des logs de build/test en échec | score des blocs d'erreur | extraction d'erreurs et de traces | V2 | [x] |
| B7 | Routeur de modèles (complexité du 1er prompt → Haiku/Sonnet/Opus) | `choice` trivial/routine/hard/research | mots-clés et longueur | V2 | [x] |
| B8 | Détection d'intention + chargement différé des outils MCP (`ToolSearch`) | `choice` d'intention + sélection des serveurs | mots-clés | V2 | [x] |
| B9 | Auto-complétion prédictive dans la TUI (fichiers `@`, commandes, historique) | — (latence) | index de fichiers + fréquence | V2 | [x] |
| B10 | Mode `auto` : score de confiance avant de continuer sans validation humaine | `auto/ask` + confiance | risque natif (jamais auto au-delà de modéré) | V2 | [x] |

## C. Logo

| # | Point | Où | État |
|---|---|---|---|
| C1 | Logo pixel art « crousty » : une crevette panée (ebi furai), SVG + rendu terminal au démarrage + README | `assets/`, `crusty-tui` | [x] |

## D. Qualité et publication

| # | Point | État |
|---|---|---|
| D1 | `cargo fmt`, `cargo clippy --workspace --all-targets -D warnings`, `cargo test --workspace` | [x] |
| D2 | Revue de code et revue de sécurité (sandbox, DLP, garde Laya, plugins, ACP), corrections | [x] |
| D3 | Seconde passe de revue sur les corrections | [~] |
| D4 | README, `docs/` (architecture, sécurité, intégrations, Laya, sandbox), licences des nouvelles dépendances | [x] |
| D5 | Essai du binaire (smoke tests) et CI verte sur Linux, macOS, Windows | [~] |
| D6 | Push sur `T-Celor/crusty-claude` | [~] |

## Journal

- **2026-10-27** — v0.1 publiée (480 tests, CI verte). Démarrage de la v0.2 : plan rédigé.
- **2026-10-28** — v0.2 : A1–A10, B0–B10, C1 réalisés ; D1, D2 (revue de code + sécurité, corrections appliquées) et D4 terminés ; D3 (seconde passe sur les corrections), D5 (CI) et D6 (publication) en cours.
