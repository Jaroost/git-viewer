# Git Viewer

Visualiseur Git en Rust (egui + libgit2), pour Linux et Windows.

## Fonctionnalités

- Graphe des commits : lanes colorées, merges, avatar de l'auteur (initiales) cerclé de la couleur de la branche
- Colonne de branches/tags à gauche du graphe (local et remote fusionnés, avec icônes), colonnes auteur/date redimensionnables, dates relatives (« hier », « il y a 3 heures »)
- Détail d'un commit : message complet, fichiers, diff avec **coloration syntaxique** selon le langage, en mode « lignes modifiées », « fichier entier » ou « côte à côte » (version précédente à gauche, nouvelle à droite)
- Recherche (Ctrl+F) : résultats surlignés dans le graphe, navigation ▲ ▼ / Entrée / F3
- Panneau de gauche à onglets :
  - **Branches** : branches distantes (en haut) et locales (en bas) ; clic = aller au commit, double-clic = basculer ; publier une nouvelle branche sur le remote (⬆)
  - **Tags**
  - **Stashes** : mettre de côté, appliquer, pop, supprimer
- Modifications du working tree : stage / unstage, commit
- Créer / basculer / supprimer des branches (avec confirmation), tags, checkout détaché (clic droit sur un commit)
- Fetch, pull (fast-forward) et push de la branche courante
- Projets récents : menu déroulant sur le nom du projet, menu Projets > Organiser (épingler, réordonner, trier, nettoyer)
- Chargement en arrière-plan : l'interface ne se fige pas sur les gros dépôts

## Lancer

```bash
cargo run --release -- /chemin/vers/un/depot   # sans argument : dernier projet ouvert
```

## Dépendances Linux

```bash
sudo apt install build-essential pkg-config libssl-dev libxkbcommon-dev \
  libxkbcommon-x11-0 libwayland-dev libx11-dev libgl1-mesa-dev
```

Sous WSL, l'application force X11 (XWayland) car le rendu Wayland de WSLg laisse des
bords blancs après un redimensionnement. `GIT_VIEWER_WAYLAND=1` désactive ce comportement.

## Authentification

SSH : ssh-agent, puis `~/.ssh/id_ed25519` / `id_rsa` sans passphrase.
HTTPS : credential helper git configuré.

## Tests

```bash
cargo test
# Benchmark sur un gros dépôt :
GV_BENCH_REPO=/chemin/depot cargo test --release bench -- --ignored --nocapture
```

## Limites connues

- Pull : fast-forward uniquement ; en cas de divergence, fusionner en ligne de commande.
- 100 000 commits chargés au maximum par projet.
- Pas de gestion des clés SSH protégées par passphrase, ni du stash, rebase, cherry-pick.
