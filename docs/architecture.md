# Architecture

Crusty-Claude est un workspace Cargo de 20 crates. `crusty-core` définit les contrats
(types et traits) ; chaque autre crate n'en dépend que par ces contrats, ce qui permet de les
développer et de les tester séparément. Le binaire `crusty` (crate `crusty-cli`) assemble tout.

## Crates

| Crate | Rôle | Dépend de |
|---|---|---|
| `crusty-core` | messages (format Anthropic, blocs inconnus conservés), traits `Provider`, `Tool`, `RequestOptimizer`, `ContentFilter`, `PermissionPolicy`, `Approver`, `HookRunner`, événements `AppEvent`/`UiCommand`, métriques, configuration en couches, estimation de tokens, magasin des originaux, confiance des projets | — |
| `crusty-providers` | Anthropic Messages (SSE, outils, réflexion, cache, comptage, replis serveur), OpenAI Chat Completions et compatibles, fournisseur simulé, reprises | core |
| `crusty-tools` | outils fichiers (Read, Write, Edit, MultiEdit, Glob, Grep, LS, NotebookEdit), TodoWrite, Retrieve, sandbox, moteur de permissions | core |
| `crusty-shell` | Bash, BashOutput, KillShell, classification du risque des commandes, environnement sans secrets, passage à RTK externe | core |
| `crusty-web` | WebFetch (SSRF, DNS épinglé, redirections, HTML→Markdown), WebSearch (Brave, Tavily, SearXNG) | core |
| `crusty-optimizer` | filtres RTK, HTML→Markdown, Headroom, texte Ponytail | core |
| `crusty-gate` | Context Gate (fonction de gain) et rendu pxpipe | core |
| `crusty-pipeline` | enchaîne Headroom → Gate → pxpipe, implémente `RequestOptimizer` et `ContentFilter`, bascules à chaud | core, optimizer, gate |
| `crusty-graph` | graphe de code natif (Graphify), outil `Graph` | core |
| `crusty-mcp` | client MCP sur le SDK officiel `rmcp` (stdio, HTTP streamable) | core |
| `crusty-session` | transcripts JSONL, mémoire `CLAUDE.md`/`CRUSTY.md`, commandes et agents personnalisés, hooks shell | core |
| `crusty-agent` | moteur orchestrateur/exécuteur, délégation `Task`, compaction, contrôleur de session, commandes slash, mode `-p` | core, session |
| `crusty-proxy` | proxy HTTP optimisant compatible Anthropic, sondes de santé des proxies externes | core |
| `crusty-tui` | interface ratatui : conversation, exécution, journal, panneau d'optimisation, agents, plan, approbations | core |
| `crusty-sandbox` | isolation OS des commandes Bash (bubblewrap/Landlock, Seatbelt, Job Object) | core |
| `crusty-laya` | client `laya-serve` (`noul`, `choice`, `score`) | core |
| `crusty-advisor` | points d'intégration Laya B0–B10 et replis natifs | core, laya |
| `crusty-dlp` | masquage des secrets et PII sortants, réhydratation locale | core |
| `crusty-acp` | Agent Client Protocol sur stdio (`crusty acp`) | core, agent |
| `crusty-cli` | binaire `crusty` : arguments, câblage, sous-commandes | tous |

## Flux d'une requête

1. La TUI envoie `UiCommand::Submit` au contrôleur (`crusty-agent::controller`).
2. Le contrôleur exécute le hook `UserPromptSubmit`, ajoute le message et lance un tour de
   l'orchestrateur en tâche de fond (annulable par `Échap`).
3. `engine::run_agent` construit la requête : prompt système (base, domaines, environnement,
   mémoire, carte du dépôt, Ponytail), outils autorisés triés (cache stable), réflexion adaptative
   et effort pour les modèles Claude récents.
4. Le pipeline d'optimisation réécrit la requête (Headroom, gate, pxpipe) et choisit la route
   (directe ou via un proxy).
5. Le fournisseur diffuse la réponse ; les deltas deviennent des `AppEvent::TextDelta`.
   Indisponibilité → modèle de repli (requête reconstruite pour lui) ; proxy injoignable → envoi direct.
6. Chaque `tool_use` passe par : disponibilité pour cet agent, JSON valide, hook `PreToolUse`,
   moteur de permissions (éventuellement approbation), exécution, hook `PostToolUse`. Les appels
   en lecture seule et les délégations s'exécutent en parallèle, les autres en séquence ; les
   résultats reviennent dans l'ordre des appels.
7. `Task` lance un exécuteur (`delegate::run_task`) : même boucle, autre modèle, autres outils,
   budget propre, pool de parallélisme par profondeur, limite de boucles de revue.
8. L'usage réel calibre le gate et alimente les métriques ; la boucle continue jusqu'à la fin
   du tour, une limite (pas, budget), un refus ou une annulation. Tout `tool_use` reçoit un
   `tool_result`, même quand l'agent s'arrête.

## Événements et métriques

Toute information affichée vient d'un `AppEvent` émis par le moteur, les outils ou le pipeline.
`EventSink::emit` met à jour `Metrics` (totaux par étape, par modèle, du gate, des proxies) puis
transmet l'événement à la TUI ; le panneau lit un instantané à chaque rafraîchissement. Les tokens
estimés (`*_est`) et réels (`*_real`, fournis par le fournisseur) sont stockés séparément ; le
mode observation compte ce qui *aurait* été économisé sans l'ajouter aux économies réalisées.

## Configuration

Couches fusionnées clé par clé : défauts → `~/.crusty/config.toml` → `<projet>/.crusty/config.toml`
→ `<projet>/.crusty/config.local.toml` → `--config`. Les couches de projet ne peuvent régler les
paramètres sensibles (fournisseurs, proxies, hooks, MCP, sécurité, autorisations) qu'après
`crusty trust`. Les secrets ne sont jamais lus depuis ces fichiers. Exemple commenté :
[config/config.example.toml](../config/config.example.toml).

## Persistance

Transcripts JSONL dans `~/.crusty/projects/<projet>/<session>.jsonl` (méta, messages de chaque
agent, compactions, usage). `--continue`, `--resume <id>` et `/resume` rechargent la conversation
de l'orchestrateur. Le graphe de code est stocké dans `<projet>/.crusty/graph/`.
