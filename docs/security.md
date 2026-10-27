# Sécurité

## Modèle de menace

Le modèle, et tout ce qu'il lit (pages web, fichiers du dépôt, sorties d'outils, réponses MCP),
est considéré comme non fiable : il peut tenter de sortir du projet, lire des secrets, exfiltrer
des données ou contourner les approbations. Les protections ne dépendent pas de sa coopération :
elles sont appliquées en dehors de lui, avant chaque exécution d'outil.

## Moteur de permissions (`crusty-tools::permissions`)

Ordre d'évaluation d'un appel :

1. une règle **deny** correspond → refus ;
2. mode **plan** et outil d'édition ou d'exécution → refus ;
3. risque **critique** → refus, sauf règle allow explicite (alors demande) ;
4. une règle **ask** correspond → demande ;
5. risque **élevé** → demande, sauf règle allow explicite ;
6. une règle **allow** correspond → autorisé ;
7. mode `bypassPermissions` (faible et modéré) ou `acceptEdits` (éditions modérées dans le projet) → autorisé ;
8. risque faible → autorisé ; sinon demande.

Les commandes composées sont découpées comme par un shell (`&&`, `||`, `;`, `|`, `&`, `( )`, `$( )`,
`<( )`, accents graves) : une règle allow doit couvrir **chaque** sous-commande, une règle deny
s'applique si **une** sous-commande correspond. « Toujours autoriser » ne propose jamais de règle
préfixe pour un interpréteur, un enrobeur (`env`, `xargs`, `timeout`…) ou une commande avec
redirection.

## Risque des commandes (`crusty-shell::risk`)

Le niveau dépend des effets, pas du nom :

- **Critique** : `sudo`/`su`, `rm -rf` sur `/`, `~` ou hors du projet, `mkfs`, `dd of=/dev/…`,
  `curl … | sh`, lecture de fichiers d'identifiants (`.env`, clés, `.ssh`, `.aws`, `.npmrc`,
  `.git-credentials`, `gh/hosts.yml`, chemins protégés de la config), vidage de l'environnement.
- **Élevé** : suppression, `git push/reset --hard/clean`, installations de paquets, `kill`,
  `chmod`, envois réseau (`curl -d/-X POST`, `scp`, `ssh`), interpréteurs avec code en ligne
  (`python -c`, `node -e`, `bash -c`…), options qui exécutent du code ou écrivent dans des
  commandes « lecture seule » (`git -c`, `git -C`, `rg --pre`, `find -exec/-delete/-fprint`,
  `sort -o`, `tee` hors projet).
- **Faible** : lectures dans le projet (`ls`, `cat`, `git status/diff/log`, `grep`…). Un chemin
  hors du projet rend la commande au moins modérée.

## Sandbox OS des commandes (`crusty-sandbox`)

Les commandes `Bash` s'exécutent isolées selon `[sandbox]` (`mode` : `off`, `read-only`,
`workspace-write` ; `network`, `writable_roots`, `fail_if_unavailable`, `allow_unsandboxed_requests`).

| Clé | Rôle |
|---|---|
| `mode` | `off`, `read-only` (écrit seulement dans un dossier temporaire privé) ou `workspace-write` (défaut : projet, `writable_roots` et dossier temporaire privé) |
| `network` | accès réseau des commandes isolées (défaut `true`) |
| `writable_roots` | dossiers inscriptibles supplémentaires (cache de build partagé…) |
| `fail_if_unavailable` | refuser la commande si aucun mécanisme ne fonctionne, au lieu de l'exécuter sans isolation avec un avertissement |
| `allow_unsandboxed_requests` | permettre au modèle de demander `dangerouslyDisableSandbox` (défaut `true`) ; la demande est **toujours** soumise à l'humain (risque critique) |

| Système | Mécanisme |
|---|---|
| Linux | bubblewrap s'il est présent, sinon Landlock (ré-exécution de `crusty __sandbox-exec`) ; avec l'un ou l'autre, les chemins protégés (`.git/hooks`, `.claude`, `.mcp.json`, `.crusty`…) sont vérifiés et restaurés après chaque commande |
| macOS | Seatbelt (`sandbox-exec`) |
| Windows | Job Object via `crusty __job-exec` : tue tout l'arbre de processus. **Aucune isolation du système de fichiers ni du réseau** |

`dangerouslyDisableSandbox` n'est accepté que si `allow_unsandboxed_requests = true` et est
**toujours** demandé à l'humain (risque critique), quel que soit le mode.

## Laya : garde, DLP et mode `auto`

Laya (`laya-serve`) et son repli natif (`crusty-advisor`) jugent les commandes Bash et les
écritures sensibles (`[laya.guard]` : `commands`, `files`, `sensitive_paths`), masquent les
secrets et PII sortants (`[laya.dlp]` : placeholders `⟦SECRET_n⟧`, restaurés localement dans les
entrées d'outils et l'affichage) et décident du mode `auto` (`[laya.auto_mode] threshold`).

- Laya ne peut que **durcir** une décision (allow → ask → deny), sauf en mode `auto` pour un
  risque au plus modéré. Délai dépassé ou incertitude : on demande.
- MCP, `WebFetch` et `WebSearch` ne s'exécutent jamais seuls sans jugement de la garde.
- Un secret masqué inséré dans un outil autre qu'un éditeur de fichiers local est **toujours**
  demandé à l'humain, quel que soit le mode.
- La garde et Laya ne voient que du texte masqué. `laya.url` doit être en https ou en http sur
  boucle locale.

## Sandbox des outils fichiers

Chemins relatifs résolus depuis le projet, normalisés, canonicalisés (liens symboliques suivis,
ancêtre existant pour les nouveaux fichiers) puis confinés au projet et aux `--add-dir`.
`security.protected_paths` (secrets, internes git) rend critiques lecture et écriture ;
`security.write_protected_paths` (`.crusty/**`, `.claude/**`, `.mcp.json`, `.git/hooks/**`,
`.vscode/tasks.json`…) rend l'écriture critique. Un fichier existant doit avoir été lu avant
d'être modifié, et ne pas avoir changé depuis. Écritures atomiques. `Read` refuse les fichiers
non réguliers (FIFO, périphériques) et plafonne la taille renvoyée.

## Projets de confiance

Un dépôt cloné peut contenir `.crusty/config.toml` et `.mcp.json`. Tant que `crusty trust`
n'a pas été lancé dans le dossier, la couche projet ne peut pas régler : `providers`, `proxy`,
`hooks`, `mcp`, `security`, `permissions.allow`, `permissions.mode`, `permissions.additional_dirs`,
`permissions.workspace_only`, les URL externes des optimiseurs, le binaire RTK externe et le
dossier des sessions. Elle peut toujours ajouter des règles `deny`/`ask` et régler les modèles.
Les clés ignorées sont signalées au démarrage. `crusty doctor` indique l'état de confiance.

## Secrets

- Refusés dans les fichiers de config : noms évocateurs (`*_key`, `*_pat`, `token`, `password`,
  `credential`…, y compris dans les tableaux `env`/`headers`) et valeurs à forme connue
  (`sk-`, `ghp_`, `github_pat_`, `xoxb-`, `AKIA`…). Les références `${VAR}` sont acceptées.
- Les fournisseurs lisent leurs clés dans des variables d'environnement nommées par la config.
- Les commandes reçoivent un environnement sans les variables correspondant à
  `security.secret_env_patterns` ; en cas de motif invalide, seule une liste blanche passe.

## Réseau

`WebFetch` : schémas http(s) seulement, pas d'identifiants dans l'URL, domaines autorisés/bloqués,
résolution DNS vérifiée (boucle locale, réseaux privés, lien local, métadonnées cloud, CGNAT,
multicast, IPv6 locales et mappées refusés), adresse épinglée, aucun proxy système, redirections
suivies à la main et revérifiées, redirection vers un autre hôte rapportée au modèle au lieu
d'être suivie, taille et durée plafonnées. Le contenu web est marqué comme donnée non fiable.

Le proxy intégré n'a pas d'authentification propre : il transmet la clé du client. Il avertit
s'il écoute ailleurs qu'en boucle locale.

## Limites connues

- Sous Windows, la sandbox ne fait que tuer l'arbre de processus : ni système de fichiers ni
  réseau isolés. Les sandbox Linux et macOS sont testées unitairement, pas contre du code hostile.
- Le DLP ne scanne ni les images ni les blocs de réflexion (le modèle ne voit que des entrées masquées).
- Un hook `PreToolUse` qui réécrit une entrée d'outil est jugé tel qu'écrit.
- Laya exige un `laya-serve` séparé ; sans lui, heuristiques natives.
- Les shells d'arrière-plan sont plafonnés (8) et tués après
  `bash_max_timeout_secs`.
- Les hooks et serveurs MCP configurés par l'utilisateur s'exécutent avec ses droits : seuls les
  projets de confiance peuvent en définir.
- La classification des commandes reste heuristique : une commande inconnue est « modérée » et
  donc demandée en mode par défaut.
