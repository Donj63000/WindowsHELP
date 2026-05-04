# Certificat de validation WindowsHELP

Date de validation : 4 mai 2026  
Projet : WindowsHELP  
Chemin : `C:\Users\nodig\RustroverProjects\WindowsHELP`  
Statut : **VALIDÉ**

## Périmètre validé

- Backend Rust : configuration, index de recherche, surveillance système, processus, thermique.
- Frontend natif egui/eframe : top bar, navigation, dashboard processus, KPI, table processus, panneau détail.
- Build debug et release.
- Tests unitaires, tests release, lint strict, smoke test UI réel avec capture d'écran.

## Correctifs validés

- Nettoyage de l'index quand une racine configurée disparaît ou est retirée.
- Nettoyage complet de l'index quand aucune racine configurée n'est active.
- Restauration thermique réessayable après un échec transitoire.
- Blocage des actions destructrices sur processus système/protégés.
- Filtre "Masquer Windows" visible et pagination remise à zéro lors des changements de filtres.
- Barre haute rendue robuste avec boutons fenêtre visibles en largeur réelle.
- Smoke test UI renforcé avec contrôle pixel de la zone droite de la barre haute.

## Résultats des tests

| Contrôle | Commande | Résultat |
| --- | --- | --- |
| Format Rust | `cargo fmt -- --check` | OK |
| Lint strict | `cargo clippy --all-targets -- -D warnings` | OK |
| Tests debug | `cargo test` | OK, 60 tests passés |
| Build release | `cargo build --release` | OK |
| Tests release | `cargo test --release` | OK, 60 tests passés |
| Smoke UI release | `powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\ui_smoke.ps1 -BuildRelease -LaunchDelaySeconds 10` | OK |

## Preuve visuelle

Capture générée : `target\ui-smoke\windowshelp-ui-smoke.png`  
Dimensions fenêtre : `1476x856`  
Zone utile écran : `{X=0,Y=0,Width=1536,Height=912}`  
Couleurs distinctes échantillonnées : `32`  
Pixels clairs zone top bar droite : `106`  
Premier plan vérifié : `True`

Validation visuelle manuelle :

- La fenêtre démarre dans la zone utile de l'écran.
- La top bar affiche le logo, le titre, la vue active, la recherche globale, le compteur d'alertes et les boutons réduire/agrandir/fermer.
- Les KPI CPU, RAM, réseau, stockage, processus et alertes sont visibles et non vides.
- La table processus affiche des lignes, la pagination et les filtres.
- Le panneau détail affiche le processus sélectionné, ses onglets, son statut et les actions.
- Aucun chevauchement critique ou écran vide n'a été constaté sur la capture finale.

## Artefacts

| Artefact | SHA-256 | Taille |
| --- | --- | --- |
| `target\release\WindowsHELP.exe` | `30BDD9D15B79DCFBF09C5B6FD3D493ABCAB5BDA411C39A56853087556A50EA43` | `8762368` octets |
| `target\ui-smoke\windowshelp-ui-smoke.png` | `6EC51515ED01F20C7F59A68470B7E513A758106E3708C9FF415EFA434632B598` | `151674` octets |

## Environnement

- `rustc 1.93.0 (254b59607 2026-01-19)`
- `cargo 1.93.0 (083ac5135 2025-12-15)`
- Windows, PowerShell.

## Conclusion

Après correction et simulations, le projet est validé sur les axes backend, frontend natif, build release et rendu visuel. Aucun bug bloquant n'a été constaté à la fin de la campagne de validation.
