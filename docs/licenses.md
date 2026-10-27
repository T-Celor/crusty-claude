# Licences et dépendances

Crusty-Claude est publié sous licence MIT ([LICENSE](../LICENSE)).

## Code ou texte tiers inclus

| Élément | Licence | Où |
|---|---|---|
| Texte de la politique Ponytail 5.3.2, © 2026 DietrichGebert | MIT | `crates/crusty-optimizer/src/ponytail.rs` |

Aucun code de RTK, Headroom, pxpipe, Graphify ou context-gate n'est inclus : leurs idées sont
réimplémentées. Les intégrations externes (binaire `rtk`, proxies Headroom/pxpipe, rapport
Graphify) sont des appels à des outils installés séparément, sous leurs propres licences.

## Dépendances directes (versions résolues)

| Crate | Version | Licence |
|---|---|---|
| tokio, tokio-util | 1.54, 0.7 | MIT |
| reqwest | 0.13 | MIT OR Apache-2.0 |
| axum | 0.8 | MIT |
| ratatui | 0.30 | MIT |
| crossterm | 0.29 | MIT |
| rmcp (SDK MCP officiel) | 3.7 | Apache-2.0 |
| html-to-markdown-rs | 3.18 | MIT |
| noto-sans-mono-bitmap (glyphes Noto Sans Mono pré-rendus) | 0.3 | MIT |
| png | 0.18 | MIT OR Apache-2.0 |
| serde, serde_json, toml | 1, 1, 0.8 | MIT OR Apache-2.0 |
| clap | 4.6 | MIT OR Apache-2.0 |
| regex, globset, ignore | 1, 0.4, 0.4 | MIT OR Apache-2.0 / Unlicense OR MIT |
| similar | 2.7 | Apache-2.0 |
| tracing, tracing-subscriber, tracing-appender | 0.1, 0.3, 0.2 | MIT |
| thiserror, anyhow, async-trait, futures | — | MIT OR Apache-2.0 |
| uuid, chrono, sha2, base64, url, dirs, shlex, unicode-width | — | MIT OR Apache-2.0 |
| tempfile, wiremock (tests) | — | MIT OR Apache-2.0 |

## Arbre complet

436 paquets résolus. Répartition des licences (champ `license` de `cargo metadata`) : très
majoritairement MIT et/ou Apache-2.0 ; quelques BSD-2/3-Clause, ISC, Zlib, Unicode-3.0,
Unlicense, CC0-1.0, 0BSD, BSL-1.0, WTFPL ; une dépendance transitive MPL-2.0 (copyleft faible par
fichier, compatible avec une distribution MIT du code de ce dépôt). Pour régénérer :

```bash
cargo metadata --format-version 1 | jq -r '.packages[] | "\(.name) \(.version) \(.license)"' | sort -u
```

Recommandé avant une publication binaire : `cargo deny check licenses` et `cargo audit`
(non exécutés pour cette version).
