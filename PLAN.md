# Plan — terminal SSH interactif et diagnostic distant

## Objectif

Piloter un serveur distant depuis un agent : shell avec état conservé (`cd`, variables, processus), diagnostic Docker, `psql` avec transaction et édition avec `vi`. Les appels courts du CLI retrouvent la même connexion SSH. La sortie est en Markdown par défaut, ou en JSON avec `--format json` ; les trames IPC internes restent en JSON.

## Architecture

```text
Appels CLI → IPC local authentifié → propriétaire local de session
                                      ↓
                              terminal local + OpenSSH
                                      ↓
                          terminal SSH → shell / outils
```

Le propriétaire local est démarré automatiquement par `open`, sans fenêtre sous Windows. Il conserve le processus OpenSSH et un terminal natif (ConPTY sous Windows, PTY sous Unix). Le terminal distant est alloué par SSH. Un analyseur VT maintient l'écran, le curseur et un historique borné en mémoire. Les entrées transitent par le terminal, sans construction d'une commande SSH pour chaque frappe.

Le serveur cible ne reçoit aucune installation. Prérequis : SSH, shell POSIX et `stty`. Les outils de diagnostic sont ceux déjà disponibles et autorisés pour le compte distant. Pour Docker : `docker ps`, `docker logs`, `docker inspect`, `docker exec -it <container> sh`, `docker cp`.

## Contrat

- `doctor` vérifie une commande SSH simple ; `open` crée la connexion interactive et attend son initialisation.
- `list`, `status`, `read` consultent les sessions du client local.
- `send`, `keys`, `resize` et `close` ciblent la session par son nom. Le client sélectionne la session courante et fixe automatiquement son UUID interne dans la requête IPC.
- `read` fournit un écran, pas une preuve de fin de commande ou de succès SQL.
- L'acquittement d'une saisie confirme son acceptation par le terminal local. Un échec après envoi peut être ambigu ; aucune mutation n'est rejouée automatiquement.
- Les métadonnées locales sont privées, avec jeton aléatoire pour l'IPC sur loopback. Aucun journal d'entrées ni sortie distante n'est stocké sur disque par le CLI.
- Une rupture SSH ou l'arrêt du propriétaire local termine la continuité. Aucune reprise de l'ancien `psql`, de sa transaction ou de l'éditeur n'est promise après déconnexion.

## Validation

Tests locaux : contrat JSON, validation avant transport, échappement du bootstrap, erreurs SSH, délais, écran alternatif Unicode, trames IPC.

Recette SSH : session dédiée sur WSL local, persistance du répertoire et des variables entre processus CLI, capture d'écran, touches, redimensionnement réel, identité et fermeture. Tester aussi un éditeur, un arrêt du shell et le refus d'une saisie dans une session terminée.

Recette PostgreSQL : dans une base jetable, relever `pg_backend_pid()`, ouvrir une transaction, vérifier le même backend entre appels, puis effectuer un rollback explicite. Aucun changement de configuration PostgreSQL.

Recette Docker : sur un serveur disposant de Docker, inspecter un conteneur de test, lire ses logs et ouvrir un shell avec `docker exec -it`. Aucun accès Docker n'est supposé sur les serveurs de validation.

### Résultats du 27 octobre 2026

- `cargo fmt --check`, `cargo test --offline`, `cargo clippy --offline --all-targets -- -D warnings` et compilation release : réussis. 13 tests passent ; le test optionnel de quoting POSIX est ignoré sous Windows.
- Recette réelle Windows OpenSSH → WSL avec le binaire release : réussie pour démarrage détaché, erreur de client SSH absent, doublon, UUID incorrect, variables et répertoire persistants, lecture, taille vérifiée par `stty size`, interruption Ctrl-C, édition/enregistrement `vi`, déconnexion et fermeture.
- Sous Windows, l'écran visible de `vi` est vérifié. Le drapeau `alternate_screen` reste indisponible (`null`), car ConPTY peut convertir un changement de buffer en commandes de rafraîchissement.
- Docker n'est pas disponible sur ce WSL : recette Docker non exécutée. PostgreSQL répond, mais le rôle de test attendu n'existe pas : transaction non validée, aucun rôle ni réglage de base modifié.
- Le backend Unix et la reprise après arrêt brutal du propriétaire local restent non validés. Aucune garantie de reprise après rupture SSH n'est fournie.

## Travail ultérieur

Les commandes `upload` et `download` sont disponibles pour les fichiers simples. La recette `tests/transfers.ps1` vérifie les transferts SFTP avec un terminal interactif occupé.

Validation du 29 octobre 2026 : suite Rust complète réussie (58 tests, un test POSIX optionnel ignoré), formatage et Clippy strict réussis. Recettes SSH et SFTP réussies avec `target/release/shell.exe` vers un hôte SSH de test. La recette SFTP couvre les fichiers binaires sur plusieurs blocs, les fichiers vides, les noms littéraux Unicode avec métacaractères, le refus d'écrasement, le remplacement avec troncature, l'UUID, les profils enregistrés, les chemins de clé relatifs après changement de dossier, un tunnel actif et vi au premier plan. Les tests de transport couvrent les délais avant et après une création potentielle et les diagnostics SSH. Les transferts écrivent directement à destination ; une erreur peut laisser un fichier partiel. Les sessions antérieures doivent être rouvertes pour enregistrer leurs paramètres de connexion.

- Publication atomique des fichiers transférés via SFTP (`send --file` reste une saisie de texte).
- Flux de sortie avec curseurs et indication de perte pour les journaux volumineux, en complément des captures d'écran.
- Recettes sur d'autres versions de Windows/OpenSSH et sur un client Unix.

## Visible attachment implemented

Windows `open --headed` and `show` launch an independent console process with
handle inheritance disabled. It authenticates to the existing owner, sends raw
VT keyboard chunks through the same serialized input path, renders formatted
VT screen snapshots, and follows its window size. Closing the viewer only
removes the display. The owner's SSH child remains independent. Ctrl-] is the
explicit detach key. Shared typing is serialized per request, not isolated;
multiple viewer sizes compete. No remote software is required. Live GUI keyboard
coverage and database transactions must be reported separately from IPC tests.

Validation on 2026-10-27: cargo fmt --check, cargo test --offline (16 passed,
one optional POSIX quoting test ignored), cargo clippy --offline --all-targets
-- -D warnings, and cargo build --release --offline passed. SSH smoke against
a test SSH host with -Headed passed, including open/show display process launch,
viewer input reaching the existing shell, terminating displays while SSH stays
alive, agent operations, resize, Ctrl-C, vi, disconnect and close. Test sessions
and viewer processes were cleaned up. Physical console keyboard and close-button
interaction were not automated; PostgreSQL/Docker remain unvalidated here.

## Correction de fermeture du terminal visible

Un test avec 200 ms entre connexion IPC et envoi de la requête reproduit une
réinitialisation systématique avec l'ancien binaire Windows. Les sockets
acceptées héritaient du mode non bloquant de la socket d'écoute ; le serveur
fermait la connexion si les premiers octets n'étaient pas encore arrivés.
Le passage explicite en mode bloquant rétablit l'attente bornée par le délai.

Le même test passe après correction. La recette `-Headed` garde désormais une
fenêtre ouverte durant vi et vérifie sa survie après l'enregistrement et le
retour au shell. Recette réussie sur un hôte SSH de test, dans des sessions de
test nettoyées ensuite. La session utilisateur `debug` n'a pas été modifiée.
Formatage, 16 tests locaux, Clippy strict et compilation release réussissent.

## Sortie Markdown — 28 octobre 2026

La sortie par défaut utilise `###screen`, le contenu brut du terminal, puis
`###metadata` avec `cursor`, `size` et `timestamp_ms`. `--detail` ajoute tous
les autres champs ; les erreurs activent automatiquement ces détails. Aucun
bloc de code englobant n'est ajouté autour de l'écran. L'historique demandé
précède l'écran actuel ; `list` affiche un tableau de sessions dans cette section.
Les scripts de recette utilisent explicitement `--format json`.

Validation : 26 tests réussis, un test POSIX optionnel ignoré, formatage et
Clippy strict réussis. La suite de tests nécessite une exécution hors sandbox
Windows pour que `icacls` protège les répertoires temporaires de session.

## Connexions et ergonomie — 28 octobre 2026

- Syntaxe `open SESSION HOST`, noms uniques entre tous les hôtes du même répertoire d'état ; verrou global conservé jusqu'à publication de la session.
- Profils JSON dans `~/.config/shell-cli/config.json`, sélection par `--profile` et surcharge par `--config`. Options PEM, port, bastion et tunnels TCP partagées entre `open` et `doctor`. Les passphrases passent par `ssh-agent` ; aucune authentification interactive par mot de passe n'est ajoutée.
- `--async` explicite sur `send` et `keys` : l'acquittement confirme l'écriture PTY, jamais la fin de la commande distante. Ce comportement était déjà asynchrone par défaut.
- `read --screen full|delta` : référence partagée entre les lectures, ajouts courts si leur origine est vérifiable, sinon écran complet. Les rafraîchissements, débordements et éditeurs peuvent imposer un écran complet. `screen_mode` expose le résultat ; `row_wrapped` décrit les retours automatiques du terminal sans prétendre retrouver les fins de ligne d'un fichier dans vi.

Validation : formatage, Clippy strict et 43 tests réussis (un test POSIX optionnel ignoré). Recette réelle Windows OpenSSH vers un hôte SSH de test réussie avec les binaires debug et release : noms uniques entre hôtes, UUID facultatifs, saisie asynchrone, lectures full/delta, état du shell, vi, redimensionnement et fermeture. Profil JSON avec port explicite et tunnel local validés par lecture d'une bannière SSH à travers le tunnel ; fermeture du port vérifiée après fermeture de session. Un ajout réel de 11 caractères a produit un delta de 11 caractères.

Le binaire release existant reste utilisé par une session active et Windows empêche son remplacement. Le nouveau binaire optimisé est compilé dans `target/updated/release/shell-cli.exe`, sans interrompre cette session. Les sessions ouvertes avec l'ancien propriétaire restent pilotables ; les nouvelles fonctions de capture nécessitent une nouvelle session.

## Attente de l'écran et deltas par défaut — 28 octobre 2026

`send` et `keys` attendent désormais une réaction du terminal suivie de 200 ms sans sortie, dans la limite de `--timeout` (15 s par défaut). Les réponses synchrones et `read` demandent un delta par défaut ; `--screen full` reste disponible, avec repli automatique vers une capture complète si le delta est incertain. `--async` conserve l'acquittement PTY immédiat sans capture. `wait_status`, visible dans les métadonnées ordinaires, distingue `quiet`, `timeout`, `closed`, `skipped` et les anciens propriétaires `unsupported`. Un délai atteint conserve l'acquittement et l'écran disponible, renvoie le code 6, sans tuer ni rejouer la commande. Le transport laisse deux secondes supplémentaires pour rendre cette réponse.

L'attente est une heuristique d'activité du terminal, sans détection de fin de commande : l'écho, une séquence de curseur ou une sortie de fond peuvent suffire. Les réponses en attente sont conservées dans la boucle du propriétaire pour continuer à servir la vue et Ctrl-C. Le verrou du fichier d'état protège sa lecture et le choix de l'UUID, puis est relâché avant une requête ordinaire ; ouverture et fermeture gardent leur protection. Les opérations d'entrée et les échanges de protocole restent sérialisés. La référence des deltas est partagée entre les lectures et les réponses synchrones.

Validation : `cargo fmt --check`, `cargo test --offline` (46 réussis, un test POSIX optionnel ignoré), `cargo clippy --offline --all-targets -- -D warnings` et compilation release réussis. Les tests Windows nécessitent de permettre `icacls` sur leurs répertoires temporaires. La recette SSH habituelle a réussi sur un hôte SSH de test. `tests/sync_wait.ps1` vérifie les captures après stabilisation, les délais sans sortie et avec sortie continue, une vue et Ctrl-C pendant l'attente d'un autre processus CLI, puis la survie du même shell. Cette recette a également réussi après l'ajustement final du verrou. Chaque recette utilise une session distincte, fermée dans `finally`, sans modifier `debug`.

Binaire optimisé disponible dans `target/sync-default/release/shell-cli.exe`. Les propriétaires déjà en cours gardent leur ancien comportement ; ouvrir une nouvelle session avec ce binaire active l'attente et les captures synchrones.

## Nettoyage des sessions après redémarrage

Correction : `open` ne réserve plus un nom sur la seule présence d'un fichier JSON. Il supprime les enregistrements dont le propriétaire a disparu, sous les verrous de cycle de vie existants, avant de créer la nouvelle session. Les nouveaux propriétaires détiennent un verrou système pendant toute leur durée de vie ; sa libération après arrêt brutal ou redémarrage permet de détecter un enregistrement mort même si son port a été réutilisé. Pour les anciens propriétaires, seul un refus de connexion prouve leur disparition. `list` et les commandes visant une session arrêtée nettoient également son enregistrement. Le nettoyage différé vérifie l'UUID pour préserver une génération recréée entre-temps.

Validation : 48 tests réussis, un test POSIX optionnel ignoré ; formatage, Clippy strict et compilation release réussis. Tests ajoutés pour le nettoyage automatique des anciens enregistrements, la réutilisation de nom sur le même hôte ou un autre hôte, le verrou actif/libéré avec port réutilisé et la préservation d'une génération de remplacement. Binaire corrigé : `target/release/shell-cli.exe`. Recette SSH réelle indisponible lors de cette validation : un hôte SSH de test refuse la connexion et un hôte SSH de test dépasse le délai de 5 secondes. Aucun redémarrage réel du PC effectué ; les tests reproduisent la libération du verrou avec conservation du fichier de session.


## Découverte locale et séparation des modules — 29 octobre 2026

`list` sans hôte affiche toutes les sessions locales avec leur hôte ; `list HOST` conserve son filtre et `list --profile NAME` utilise l'hôte du profil. `profiles list` expose les noms et hôtes configurés. `config show [HOST]` décrit le fichier source, le profil, les surcharges CLI et les réglages shell-cli effectifs, sans lire les clés ni appeler SSH ; il ne développe pas les réglages OpenSSH. Le rapport est disponible en Markdown et dans le champ JSON structuré `configuration`.

Le guide court `GETTING_STARTED.md` est séparé du README de référence. Le propriétaire de session conserve son orchestration dans `session.rs` ; stockage/verrous, IPC, terminal, captures/deltas et vue sont isolés dans `src/session/`. Les tests de régression restent regroupés dans `src/session/tests.rs` pour vérifier leurs interactions.

Validation : `cargo fmt --all --check`, `cargo clippy --offline --all-targets -- -D warnings` et suite complète hors sandbox réussis : 63 tests réussis, 2 tests optionnels ignorés (SSH réel et shell POSIX). Les tests Windows utilisent `icacls` uniquement sur leurs répertoires temporaires. Aucune session utilisateur ni connexion SSH réelle utilisée.

## Transferts récursifs et sélection multiple — 29 octobre 2026

`upload` et `download` acceptent plusieurs sources avant la destination et `--recursive` (`-r`) pour les dossiers. Une source utilise un chemin de destination exact ; plusieurs sources conservent leur nom dans le dossier cible. Les dossiers vides sont conservés et les dossiers existants fusionnés. Une seule connexion SFTP copie les fichiers successivement. Les collisions de destinations et les liens symboliques récursifs sont refusés ; l'écrasement des fichiers reste explicite. Le rapport expose toutes les sources et les totaux de fichiers, dossiers et octets. Le délai est global, y compris pour les créations locales de dossiers vides. Les écritures restent directes, sans publication atomique ni annulation des fichiers précédemment copiés.

Validation : formatage, Clippy strict, compilation release et suite Rust complète réussis (70 tests réussis, 2 tests optionnels ignorés). La recette `tests/transfers.ps1` passe avec les binaires debug et release vers un hôte SSH de test après démarrage de WSL : arborescences imbriquées, dossiers vides, fichiers binaires, sélection de fichiers et dossiers, collisions, refus des liens symboliques, écrasement, identité de session, profil enregistré, changement de répertoire, tunnel actif et vi au premier plan. Les sessions et dossiers de recette sont nettoyés ; aucune session utilisateur n'est modifiée.

## Exécution structurée et diagnostic Windows — 31 octobre 2026

`exec [HOST] --text/--stdin/--file` ouvre une connexion SSH indépendante sans PTY et attend la sortie du processus ainsi que la fermeture de ses flux. La réponse expose `stdout`, `stderr`, `exit_code` et `completed`, sans passage par l'écran ni l'historique du terminal. Les espaces et fins de ligne sont conservés ; chaque flux est limité à 1 MiB avec signalement de troncature et drainage des octets supplémentaires. Un échec distant conserve les flux et le code distant, avec code CLI 3. Les délais et erreurs de transport conservent les sorties déjà reçues, sans déclarer la commande terminée ni la rejouer. Le statut SSH 255 reste un résultat distant inconnu. La commande utilise les profils et options SSH habituels mais n'hérite pas du répertoire ou de l'environnement d'une session interactive et n'accède pas au répertoire local de sessions.

Le terminal initial passe à 120 × 40, avec dimensions toujours configurables. `send` conserve son attente de stabilisation de l'écran ; la documentation oriente vers `exec` pour une fin de commande vérifiable, ou vers un marqueur explicite si la commande doit utiliser l'état interactif.

L'erreur Windows de protection du répertoire indique désormais le chemin, le compte, l'opération `icacls`, son statut et ses deux flux de diagnostic. Elle explique qu'un bac à sable peut refuser le changement d'ACL malgré l'autorisation d'écrire des fichiers et propose une exécution hors sandbox sous le même compte ou un répertoire dédié via `SHELL_CLI_STATE_DIR`. La protection des identifiants de session reste obligatoire.

Validation : suite complète de 77 tests réussis, 2 tests optionnels ignorés ; les 6 nouveaux tests d'exécution passent également après les derniers ajustements. Formatage, Clippy strict et compilation release réussis. Essais SSH réels debug et release vers un hôte SSH de test : pause d'une seconde avant la fin, stdout et stderr séparés, code distant 7 ; sortie de 300 colonnes préservée avec le binaire release. Le test d'ouverture a reproduit l'accès refusé dans le bac à sable et vérifié le nouveau diagnostic ; une nouvelle session isolée hors sandbox a confirmé la taille 120 × 40 via `status`, puis a été fermée. Binaire disponible : `target/release/shell.exe`.
