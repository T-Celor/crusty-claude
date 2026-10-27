# Intégrations

Chaque optimiseur existe en version native (Rust, dans le processus) ; certains peuvent aussi
déléguer à l'outil d'origine. Le mode natif est le défaut : aucun composant externe n'est requis.

## RTK

- Natif (`crusty-optimizer::rtk`) : filtres git status/log/diff, cargo/rustc, npm/pnpm/yarn/bun,
  pytest/jest/vitest/go/dotnet/maven, listings, grep/rg, déduplication générique, troncature
  tête/queue. Les lignes d'erreur sont toujours conservées ; l'original complet reste accessible
  par `Retrieve`.
- Externe : `[optimizer.rtk] prefer_external = true` exécute les commandes simples prises en charge
  via `rtk <commande>` (préfixe exact, jamais pour une commande au-delà du risque modéré). Le gain
  n'est alors pas mesuré par Crusty (affiché comme tel).

## HTML → Markdown

`html-to-markdown-rs` (MIT) dans `WebFetch`. Les blocs `script`, `style`, `nav`, `footer`, `svg`,
`iframe` sont retirés ; titre, URL canonique, description et date sont extraits.

## Graphify

- Natif (`crusty-graph`) : parseurs tree-sitter (Rust, Python, JS/TS, Go, Java, C/C++…), repli par expressions régulières ; index incrémental (hachage SHA-256 par fichier) de Rust, Python,
  JS/TS, Go, Java, Kotlin, C/C++, C#, Ruby, PHP, Swift, Scala, Lua, shell et Markdown ; arêtes
  extraites (contient, importe) et inférées (références), communautés par propagation d'étiquettes
  déterministe, nœuds centraux, liens surprenants. Outil `Graph` (find, explain, neighbors, path,
  map, stats, rebuild), carte du dépôt injectée dans le prompt de l'orchestrateur, rapport
  `.crusty/graph/GRAPH_REPORT.md`, commande `crusty graph`.
- Externe : si `graphify-out/GRAPH_REPORT.md` (outil Graphify Python) existe, un extrait est ajouté
  à la carte du dépôt (`use_external_report = true`).

## Headroom

- Natif : compression des anciens résultats d'outils et messages utilisateur volumineux, en
  protégeant le prompt système, les définitions d'outils, les blocs `tool_use`/réflexion et les
  derniers messages. Frontière gelée qui n'avance que par lots (`batch_tokens`) : le préfixe envoyé
  reste identique d'un tour à l'autre, ce qui préserve le cache de prompt du fournisseur.
- Externe : `[optimizer.headroom] mode = "external"` envoie les requêtes au proxy Headroom
  (`external_url`). Pour garder le gate et pxpipe après lui, configurez l'amont de Headroom vers
  `crusty proxy`.

## Context Gate

Portage de la politique de `context-gate.mjs` (token-optimizer) : estimation, calibration par
session à partir des réponses directes, sessions collantes, zones (basse, moyenne, haute, limite),
dry-run de pxpipe mis en cache, économie sur l'horizon de session avec prix du cache, marge en zone
moyenne. Ajouts : fenêtre réelle du modèle, liste blanche de modèles, garde verbatim, économie
minimale, disjoncteur, hystérésis, marge adaptative, revérification des sessions collantes,
plafond de pages. Déterministe ; le temps est passé en paramètre (tests reproductibles).

## pxpipe

- Natif (`crusty-gate::pxpipe`) : rendu des anciens blocs denses en PNG niveaux de gris avec la
  police Noto Sans Mono 16 px (repli GNU Unifont pour les écritures non latines), dernière page recadrée, note indiquant ce qui est imagé et fiche des
  valeurs exactes (hachages, UUID, nombres longs) gardée en texte.
- Externe : `[optimizer.pxpipe] mode = "external"` route la requête vers un proxy pxpipe
  (`external_url`, par défaut `http://127.0.0.1:47821`) quand le gate le décide ; le dry-run natif
  sert alors d'estimation.

## Ponytail

Texte de la politique (MIT, © 2026 DietrichGebert, version 5.3.2) injecté dans le prompt système
de **chaque** agent ; niveaux `lite`, `full`, `ultra` ; `/ponytail` le change à chaud. Les règles
de sécurité de Crusty priment sur la politique.

## Laya (`crusty-laya`, repli `crusty-advisor`)

Client de `laya-serve` (`POST /v1/systemone`, questions `noul`, `choice`, `score` ; santé, latence,
lots). Chaque point B0–B10 a un repli natif déterministe, utilisé si Laya est désactivé,
injoignable, trop lent ou sous `min_confidence`. Laya ne peut que durcir une décision de sécurité.

| Point | Rôle | Config |
|---|---|---|
| Garde | juge les commandes Bash et les écritures sensibles : `allow` / `review` / `block` | `[laya.guard]` `commands`, `files`, `sensitive_paths` |
| DLP sortant | secrets et PII masqués en `⟦SECRET_n⟧` avant l'envoi à l'API, restaurés localement dans les entrées d'outils et l'affichage | `[laya.dlp]` `secrets`, `pii`, `ambiguous_threshold`, `rehydrate`, `allow_values` |
| Reranker | trie les résultats de Grep, Glob et Graph | `[laya.rerank]` `min_items`, `keep` |
| Épurateur | remplace les anciens résultats d'outils périmés (par lots, cache stable, récupérables par `Retrieve`) | `[laya.pruner]` `keep_recent_messages`, `min_relevance`, `batch_messages` |
| Triage | garde les blocs d'erreur des logs de build/test en échec | `[laya.triage]` `min_lines`, `max_blocks`, `context_lines` |
| Routeur de modèles | classe le prompt (trivial / routine / hard / research) vers Haiku / Sonnet / Opus, sauf si `--model` ou `/model` fixe le modèle ; `/model auto` revient au routage | `[laya.router]` `every_prompt`, `min_confidence`, `tiers` |
| Intention | diffère les outils MCP non pertinents, chargés à la demande par l'outil `ToolSearch` | `[laya.intent]` `defer_mcp_tools`, `min_relevance`, `always_load` |
| Complétion TUI | `@fichiers`, `/commandes`, historique | — |
| Mode `auto` | continue sans validation humaine quand la confiance est suffisante | `[laya.auto_mode]` `threshold` |

Statistiques de l'advisor dans le panneau latéral. Le DLP vit dans `crusty-dlp`. Règles de
sécurité : [security.md](security.md).

## Éditeurs (ACP)

`crusty acp` : Agent Client Protocol sur stdio (Zed, JetBrains, Neovim, Emacs). Sessions
persistées, approbations passées par l'éditeur. Non testé de bout en bout avec un vrai éditeur.

## Skills et plugins

Skills : dossiers `SKILL.md`, outil `Skill`, `/skills` (`[skills]`). Plugins : `plugin.json`
(commandes, agents, skills, hooks, MCP), `crusty plugin list|install|remove` (`[plugins]`) ;
plugins de projet seulement après `crusty trust`.

## Proxies externes

Toute entrée `[[proxy.upstreams]]` est sondée (`health_path`, `stats_path`) et affichée dans le
panneau ; `via_proxy` ou `/proxy route` y envoie un fournisseur. Exemples : Headroom
(`/livez`, `/stats`), context-gate (`/livez`, `/stats`), pxpipe, LiteLLM.

## MCP

SDK officiel `rmcp` : serveurs stdio, HTTP streamable et SSE historique depuis `[mcp.servers]`, `.mcp.json`
(projet de confiance) et `--mcp-config`. Outils nommés `mcp__<serveur>__<outil>`, toujours soumis
aux permissions (règle `mcp__serveur` pour les autoriser), délai par appel.

## Fournisseurs

| `kind` | Pour | Notes |
|---|---|---|
| `anthropic` | API Anthropic et proxies compatibles | `anthropic-version` 2026-10-21 (sans paramètres d'échantillonnage), réflexion adaptative, effort, cache de prompt automatique (≤ 4 marqueurs), `eager_input_streaming` hors proxy, replis serveur sur les modèles qui le permettent, `count_tokens`, liaison de réflexion tolérante aux réécritures |
| `openai` | OpenAI, OpenRouter, Gemini (endpoint OpenAI), Ollama, vLLM, LM Studio | outils au format function calling, images en data URL, usage avec cache |
| `bedrock` | Amazon Bedrock | signatures SigV4 ou jeton bearer |
| `vertex` | Google Vertex AI | identifiants par défaut (ADC) |
| `mock` | tests et démo hors ligne | réponses scriptées ou écho |
