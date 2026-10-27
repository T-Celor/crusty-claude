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

- Natif (`crusty-graph`) : index incrémental (hachage SHA-256 par fichier) de Rust, Python,
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
  police Noto Sans Mono 16 px, dernière page recadrée, note indiquant ce qui est imagé et fiche des
  valeurs exactes (hachages, UUID, nombres longs) gardée en texte.
- Externe : `[optimizer.pxpipe] mode = "external"` route la requête vers un proxy pxpipe
  (`external_url`, par défaut `http://127.0.0.1:47821`) quand le gate le décide ; le dry-run natif
  sert alors d'estimation.

## Ponytail

Texte de la politique (MIT, © 2026 DietrichGebert, version 5.3.2) injecté dans le prompt système
de **chaque** agent ; niveaux `lite`, `full`, `ultra` ; `/ponytail` le change à chaud. Les règles
de sécurité de Crusty priment sur la politique.

## Proxies externes

Toute entrée `[[proxy.upstreams]]` est sondée (`health_path`, `stats_path`) et affichée dans le
panneau ; `via_proxy` ou `/proxy route` y envoie un fournisseur. Exemples : Headroom
(`/livez`, `/stats`), context-gate (`/livez`, `/stats`), pxpipe, LiteLLM.

## MCP

SDK officiel `rmcp` : serveurs stdio et HTTP streamable depuis `[mcp.servers]`, `.mcp.json`
(projet de confiance) et `--mcp-config`. Outils nommés `mcp__<serveur>__<outil>`, toujours soumis
aux permissions (règle `mcp__serveur` pour les autoriser), délai par appel.

## Fournisseurs

| `kind` | Pour | Notes |
|---|---|---|
| `anthropic` | API Anthropic et proxies compatibles | `anthropic-version` 2026-10-21 (sans paramètres d'échantillonnage), réflexion adaptative, effort, cache de prompt automatique (≤ 4 marqueurs), `eager_input_streaming` hors proxy, replis serveur sur les modèles qui le permettent, `count_tokens`, liaison de réflexion tolérante aux réécritures |
| `openai` | OpenAI, OpenRouter, Gemini (endpoint OpenAI), Ollama, vLLM, LM Studio | outils au format function calling, images en data URL, usage avec cache |
| `mock` | tests et démo hors ligne | réponses scriptées ou écho |
