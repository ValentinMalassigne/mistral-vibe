# Plan technique — Compaction déclenchée par l'agent

Cible : `main` @ `c96a6dbb` (v3.1.0). Les numéros de ligne sont ceux de ce commit.
Ce plan corrige la carte du code du handoff sur trois points structurants (voir §1).

Règle de travail retenue pour ce fork : **on suit les conventions du repo**. Une
fonctionnalité ajoutée s'aligne sur l'existant (défauts, nommage, structure, tests) plutôt que
d'imposer ses propres choix. En cas de conflit réel entre ce qu'on veut et la convention, on
le remonte et on ne tranche pas unilatéralement.

À lire avant de coder (le repo l'exige pour toute capacité agent) : `AGENTS.md` à la racine,
et les ADR [0002](../../docs/adr/0002-core-engine-and-delivery-surfaces.md) (propriété
Core/Runtime), [0004](../../docs/adr/0004-typed-permissioned-tools.md) (contrat des outils) et
[0018](../../docs/adr/0018-modular-app-server-capabilities.md) (nouvelle capacité).

## 1. Architecture réelle (et ce que le handoff supposait de travers)

```
vibe-rs (TUI Rust, client mince)
   │  app-server JSON-RPC
   ▼
vibe-app-server (Python)  ── config, sessions, projections UI
   │  Step Protocol (HarnessCommand / Action / Observation)
   ▼
Unified Harness Core (Rust, harness/core)  ── machine à états, outils, compaction
   ▲
   │  actions d'exécution
Runtime Python (harness/runtimes/python)  ── exécute réellement les outils
```

Trois corrections par rapport au handoff :

| Handoff | Réalité |
|---|---|
| L'outil s'implémente dans `harness/core` | Core **déclare** l'outil (schéma + route) mais le **runtime Python l'exécute**. Un outil ne peut pas muter l'état core ; il renvoie un résultat que core interprète. |
| Compaction mid-turn = option risquée | `TurnEvent::ManualCompaction` est refusé si `TurnState::Active` ou `Compacting` (`turn.rs:129`). Mid-turn est **impossible** sans réécrire la machine à états. |
| Le trigger est local | `CompactionTrigger` est sérialisé jusqu'au client (`protocol.py:1408,1419,1493`) : une nouvelle variante touche le runtime Python et la TUI. |

Deux bonnes nouvelles : `/compact` passe déjà par un chemin réutilisable
(`HarnessCommand::Compact` → `router.rs:80` → `TurnEvent::ManualCompaction`), et le moteur
sait déjà **reprendre un tour après compaction** (`CompactionResolution::ContinueTurn`,
`turn.rs:250`) : c'est exactement le mécanisme dont on a besoin pour que l'agent continue son
plan après avoir compacté.

## 2. Flux de bout en bout

```
1. modèle appelle  request_compaction{handoff_path, extra_instructions, reason}
2. core (assembly) a routé l'outil → Action ToolCall → runtime
3. runtime : lit le handoff, valide, renvoie Success{handoff_note, truncated, bytes}
4. core finish_tool_batch_action (batch.rs:481) : détecte le succès,
   stocke PendingAgentCompaction{handoff_note, extra_instructions} dans HarnessState
5. le modèle termine son tour (aucun outil restant)
6. core apply_tool_batch_transition (turn.rs:701), branche ContinueTurn :
   au lieu de continuer le tour, si pending → start_compaction(CompactionRequest::Agent{..})
7. compaction : résumé, remplacement de l'historique (compacted(), execution.rs:198)
8. apply_compaction_resolution → ContinueTurn (turn.rs:250) :
   la note de passation est injectée dans le contexte, puis continue_after_compaction
9. l'agent reçoit résumé + note et poursuit la phase suivante
```

Le point 6 est le seul vrai point d'insertion dans la machine à états. Il réutilise
`apply_in_turn_compaction_start` (`turn.rs:446`), déjà écrit pour l'autocompaction en cours de
tour.

## 3. Modifications Core (Rust)

### 3.1 `harness/core/src/core/features/agent_compaction.rs` — nouveau

Calqué sur `model_wait.rs` (même rôle : un outil conditionné par les settings).

```rust
//! Agent-requested compaction: the settings, the guards and the tool contract.

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::core::config::ToolSettings;
use crate::core::step_protocol::ToolDefinition;

pub(crate) const DEFAULT_MIN_INPUT_TOKENS: u64 = 20_000;
pub(crate) const DEFAULT_MAX_PER_SESSION: u32 = 8;
pub(crate) const DEFAULT_MIN_GROWTH_TOKENS: u64 = 5_000;
pub(crate) const DEFAULT_MAX_HANDOFF_BYTES: u64 = 8_192;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Settings {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "min_input_tokens")]
    pub min_input_tokens: u64,
    #[serde(default = "max_per_session")]
    pub max_per_session: u32,
    #[serde(default = "min_growth_tokens")]
    pub min_growth_tokens: u64,
    #[serde(default = "max_handoff_bytes")]
    pub max_handoff_bytes: u64,
}

impl Default for Settings { /* enabled: false, le reste aux défauts */ }

impl Settings {
    pub(crate) fn is_default(&self) -> bool { *self == Self::default() }
    pub(crate) fn is_offered(&self) -> bool { self.enabled }
}

/// Le résultat structuré que le runtime renvoie après lecture du handoff.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HandoffReply {
    pub handoff_note: String,
    #[serde(default)]
    pub truncated: bool,
}

/// Pourquoi une demande a été refusée. Chaque variant produit un message
/// actionnable : le modèle le lit et doit pouvoir se corriger.
pub(crate) enum Refusal {
    Disabled,
    BelowFloor { estimated: u64, floor: u64 },
    InsufficientGrowth { grown: u64, required: u64 },
    QuotaReached { done: u32, max: u32 },
    AlreadyPending,
}

impl Refusal {
    pub(crate) fn message(&self) -> String { /* … */ }
}

pub(crate) fn tool(description: &str, settings: &Settings) -> ToolDefinition {
    /* schéma JSON ci-dessous ; `description` vient des prompts, cf. §3.1b */
}
```

Schéma de l'outil :

```json
{
  "type": "object",
  "additionalProperties": false,
  "properties": {
    "handoff_path":   { "type": "string", "minLength": 1 },
    "extra_instructions": { "type": "string" },
    "reason":         { "type": "string", "maxLength": 200 }
  },
  "required": ["handoff_path"]
}
```

### 3.1b La description de l'outil va dans les prompts — convention du repo

`wait` ne code pas sa description en dur : elle est dans
`core/prompt/default_prompts.json` et surchargeable par `settings.prompts.wait_description`.
On suit la même règle, sinon l'outil sera le seul du catalogue à ne pas être personnalisable
(`prompt/settings.rs:38`). Quatre points de contact en plus :

1. `core/prompt/default_prompts.json` : entrée `request_compaction_description` (texte anglais,
   même ton que `wait_description`).
2. `core/prompt/settings.rs` : champ `pub request_compaction_description: String` dans
   `PromptSettings`.
3. `PromptSettings::tool_text_changed` (ligne 62) : ajouter la comparaison — sans quoi un
   changement de description ne révoque pas le catalogue d'outils.
4. `PromptSettings::validate` (ligne 70) : ajouter la paire au contrôle « prompt non vide ».

Texte proposé (anglais, à placer dans `default_prompts.json`) :

> Compact the conversation to free context. Use it when your current plan says to compact
> before continuing, or when you are about to start a long phase that needs room. Before
> calling it, write what must survive the compaction (decisions taken, files touched,
> remaining steps) to `handoff_path`. The compaction runs when your turn ends and the
> conversation resumes right after, with your handoff note re-injected.

### 3.2 `core/config.rs` — `ToolSettings` (ligne 277)

```rust
#[serde(default, skip_serializing_if = "agent_compaction::Settings::is_default")]
pub agent_compaction: agent_compaction::Settings,
```

`ToolSettings` a `deny_unknown_fields` : sans conséquence ici puisque core et runtime sont
rebuildés ensemble dans le fork. Pour une PR upstream, la direction compatible est celle-ci
(ancienne config + core neuf), cf. ADR 0014.

### 3.3 `core/tools/external.rs`

- `RuntimeBuiltinToolName::AgentCompactionRequest` (variant).
- `as_str()` → `"request_compaction"`.
- `from_str` (autour de la ligne 87) → le nom inverse.
- `direct_name()` (ligne 183 et 213 : la variante `Some(...)`) → `Some("request_compaction")`.
- `hook_target()` (ligne 243) → `HookToolTarget::SelfTool`. Choix assumé : c'est un outil
  interne de Core, et `self` est déjà le groupe des outils de contrôle internes
  (`self.wait`, `self.sleep`). Évite de toucher le contrat des hooks utilisateur.
- `checkpoint/v1/schema/program.rs` n'est à toucher que si l'outil doit être appelable depuis
  un programme TypeScript. **Non retenu en v1** → pas de changement de checkpoint.

### 3.4 `core/tools/resolved/assembly.rs`

Deux insertions, exactement au même endroit que `wait` :

- `builtin_direct_tools` (ligne 215) : si `is_offered()`,
  `tools.push(agent_compaction::tool(&config.settings.prompts.request_compaction_description, &config.settings.tools.agent_compaction))`
  — même forme que l'appel à `model_wait::tool` juste au-dessus.
- `direct_routes` (ligne 27) : si `is_offered()`, route `"request_compaction"` vers
  `ToolTarget::RuntimeBuiltin { name: AgentCompactionRequest }`.

Effet : flag désactivé → l'outil n'existe ni dans le catalogue du modèle, ni dans le routage.

### 3.5 `core/features/compaction/mod.rs` — `CompactionTrigger` (ligne 145)

```rust
pub(crate) enum CompactionTrigger {
    Manual,
    Automatic,
    Agent,
}
```

Sérialisé en snake_case → `"agent"`.

### 3.6 `core/features/compaction/execution.rs`

`CompactionContinuation` (ligne 25) doit transporter le trigger pour le cas « en cours de
tour » :

```rust
enum CompactionContinuation {
    InTurn { turn_id: String, iterations: u32, trigger: CompactionTrigger },
    BetweenTurns { trigger: CompactionTrigger },
}
```

`trigger()` (ligne 85) renvoie simplement le champ. `in_turn_identity()` (ligne 92) ne change
pas, la paire `(turn_id, iterations)` reste l'identité de reprise.

`CompactionRequest` (ligne 494) :

```rust
pub(crate) enum CompactionRequest<'a> {
    Automatic { turn_id: &'a str, iterations: u32 },
    Manual { extra_instructions: &'a str },
    Agent { turn_id: &'a str, iterations: u32, extra_instructions: &'a str },
}

impl<'a> CompactionRequest<'a> {
    pub(crate) fn agent(turn_id: &'a str, iterations: u32, extra_instructions: &'a str) -> Self { … }
}
```

`start_compaction` (ligne 512) : la branche `Agent` produit
`CompactionContinuation::InTurn { turn_id, iterations, trigger: CompactionTrigger::Agent }` —
donc continuation **dans le tour** (l'agent continue) avec la trace `agent`.

`PendingCompaction::in_turn` (ligne 31) passe `trigger: CompactionTrigger::Automatic` pour
l'autocompaction.

`compacted()` (ligne 198) est inchangé : l'injection de la note se fait dans `turn.rs` (§3.7),
pour ne pas faire dépendre le module compaction de l'état de session.

### 3.7 `core/turn.rs` + `core/state.rs`

Nouvel état (transitoire, comme l'est déjà l'état de compaction : rien de la compaction n'est
dans le schéma de checkpoint) :

```rust
pub(crate) struct PendingAgentCompaction {
    pub handoff_note: String,
    pub extra_instructions: String,
    pub reason: Option<String>,
}

// dans HarnessState
pub pending_agent_compaction: Option<PendingAgentCompaction>,
pub agent_compactions_done: u32,
pub agent_compaction_baseline_tokens: Option<u64>,
```

**(a) Les garde-fous tournent quand le résultat arrive, pas en fin de tour**

Décision ferme, et pas cosmétique : si le refus n'intervenait qu'en fin de tour, l'outil aurait
déjà répondu « accepté » au modèle, qui continuerait en croyant son contexte compacté. Le refus
doit être un **échec d'outil immédiat**, lu et exploité par le modèle dans le même tour.

Point d'insertion : `finish_pending_tool` (`turn.rs:330`). Il a déjà tout — `&mut HarnessState`,
`call_id`, le batch via `ActivePhase::AwaitingToolBatch`, et le résultat validé (lignes 337-353).

```rust
fn finish_pending_tool(state, action_id, call_id, report, determinism) -> … {
    // … validation existante …
    let mut result = /* résultat validé */;

    if let Some(call) = batch.call(&call_id) {          // accesseur à ajouter, cf. plus bas
        if call.name == "request_compaction" {
            match admit_agent_compaction(state, settings) {
                Ok(()) => {
                    let handoff = parse_handoff(&result)?;   // HandoffReply ← structured_content
                    state.pending_handoff_note = Some(handoff.handoff_note.clone());
                    state.pending_agent_compaction = Some(PendingAgentCompaction {
                        handoff_note: handoff.handoff_note,
                        extra_instructions: call.arguments…,   // cf. note ci-dessous
                        reason: …,
                    });
                }
                Err(refusal) => {
                    // Le modèle voit le refus tout de suite ; son tour continue.
                    result = ToolResult::Failure {
                        content: vec![],
                        structured_content: StructuredContent::Absent,
                        meta: None,
                        error: ProtocolError {
                            code: "tool_failed".to_string(),
                            message: refusal.message(),
                            retryable: false,
                            details: None,
                        },
                    };
                }
            }
        }
    }
    // … suite inchangée : finish_tool_batch_action(tools, batch, &action_id, result, …) …
}
```

Deux détails d'implémentation :

- `ToolBatch` n'expose pas le nom d'un call par `call_id` : ajouter
  `pub(crate) fn call(&self, call_id: &str) -> Option<&ToolCall>`. Seule addition d'API au
  module batch — `batch.rs` reste pur et aucune signature existante ne change.
- `extra_instructions` et `reason` sont dans les **arguments** de l'appel, pas dans le
  résultat. Les lire depuis `call.arguments` (JSON validé par le schéma) plutôt que de faire
  confiance au runtime pour les renvoyer.

`parse_handoff` échoue proprement (`CoreError::invalid_command`) si le runtime renvoie un
`structured_content` non conforme : un runtime ancien ou bogué ne doit pas corrompre l'état.

**(b) La garde**

```rust
fn admit_agent_compaction(state: &HarnessState, settings: &Settings) -> Result<(), Refusal>
```

Sources, toutes déjà présentes :
- taille estimée du contexte : `estimate_agent_request_tokens(state)` (`request_size.rs:179`) ;
- croissance depuis la dernière compaction agent : `agent_compaction_baseline_tokens` comparé à
  `state.last_reported_context_tokens` ;
- quota : `agent_compactions_done` ;
- « déjà en attente » : `pending_agent_compaction.is_some()`, et
  `matches!(state.turn, TurnState::Compacting { .. })`.

Un refus laisse l'état intact : pas de compaction, pas d'intention enregistrée, le tour continue
normalement.

**(c) Déclenchement** — `orchestration.rs` expose déjà le wrapper d'état `start_manual_compaction(state, extra_instructions)` (ligne 36), qui assemble `generated_system`, `compaction_inputs` et `compaction_count`. On ajoute le jumeau :

```rust
pub(crate) fn start_agent_compaction(
    state: &mut HarnessState,
    turn_id: &str,
    iterations: u32,
    extra_instructions: &str,
) -> Result<CompactionStartResolution, CoreError> {
    start(state, CompactionRequest::agent(turn_id, iterations, extra_instructions))
}
```

Puis, dans `apply_tool_batch_transition` (`turn.rs:701`), branche
`ToolBatchResolution::ContinueTurn` (ligne 718), avant `continue_active_turn` :

```rust
if let Some(pending) = state.pending_agent_compaction.take() {
    let (turn_id, iterations) = {
        let active = state.require_active()?;
        (active.turn_id.clone(), active.iterations)
    };
    let start = start_agent_compaction(
        state, &turn_id, iterations, &pending.extra_instructions,
    )?;
    state.agent_compactions_done += 1;
    state.agent_compaction_baseline_tokens = state.last_reported_context_tokens;
    state.pending_handoff_note = Some(pending.handoff_note);
    return Ok(outcome.after(apply_in_turn_compaction_start(state, start)?));
}
```

Attention à l'ordre : `apply_in_turn_compaction_start` installe
`ActivePhase::AwaitingCompaction` ; `pending_handoff_note` est posé avant, et doit survivre
jusqu'à la reprise.

Note d'implémentation : `turn.rs` importe déjà cette famille de fonctions sous un alias
(`turn.rs:23`, `start_manual_compaction as start_manual_compaction_request`) parce que le
fichier a sa propre `fn start_manual_compaction`. Garder la même convention.

**(d) Réinjection de la note** — `apply_compaction_resolution` (`turn.rs:245`), branche
`CompactionResolution::ContinueTurn` : si une note agent est en attente, l'injecter avant
`continue_after_compaction` :

```rust
if let Some(note) = state.pending_handoff_note.take() {
    state.context.push_before_next_turn(StoredMessage::injected(...))?;
}
```

`push_before_next_turn` est déjà utilisé par `handle_context_message` (`turn.rs:761`) pour
l'injection de contexte : même mécanique, la note entre dans l'historique persisté, juste
après le résumé. La détruire en cas d'échec de compaction (branche `FailTurn`) : sur échec,
l'historique d'origine est conservé et la note n'a plus lieu d'être injectée.

### 3.8 Compaction `BetweenTurns` : inchangée

`/compact` reste exactement sur son chemin actuel. Seule différence : `trigger()` d'une
compaction manuelle reste `Manual`.

## 4. Modifications Runtime Python

### 4.1 `harness/runtimes/python/python/mistralai_vibe_local_harness/`

- `protocol.py` :
  - `RustRuntimeBuiltinToolName` (ligne 34) et `RUNTIME_BUILTIN_TOOL_NAMES` (ligne 61) :
    ajouter `"request_compaction"`.
  - Les quatre `Literal["automatic", "manual"]` (lignes 1020, 1408, 1419, 1493) :
    ajouter `"agent"`.
  - Ligne 1050, la condition `if self.trigger == "automatic" and self.turn_id is not None`
    doit inclure `"agent"` : le trigger agent porte aussi un `turn_id` (continuation en cours
    de tour).
- `vibe/_agent_compaction.py` — nouveau : exécution de l'outil. Le runtime ne fait **que**
  lire le fichier et valider les arguments : aucun garde-fou ici (§3.7a), il ne connaît ni les
  tokens ni le quota.
  ```python
  async def execute_agent_compaction(action, *, config, max_bytes: int) -> Event
  ```
  - **réutiliser la machinerie existante**, ne pas réécrire une résolution de chemin :
    `_read_roots(config)`, `_path_policy(…)` et `_is_within(path, roots)` dans
    `vibe/_file_tools.py:323,339,368` sont exactement les règles de `file_system.read_file`
    (racines autorisées, symlinks). Une résolution maison divergerait du sandbox.
  - refuse hors racines, absente, vide, non-régulier — avec un message qui dit d'écrire la note
    d'abord ;
  - lit au plus `max_bytes`, tronque proprement (frontière UTF-8), renvoie
    `Success{handoff_note, handoff_bytes, truncated}`.
- `vibe/_runtime.py` (~ligne 2750, à côté de `execute_model_wait` / `self.sleep`) : un bras de
  dispatch pour `request_compaction`.
- `vibe/_hook_matcher.py` (ligne 63) : `"request_compaction": ("request_compaction",)`.
- `session_api/app_server/session/models/configuration.py` (à côté de la description de
  `tools.self.sleep`) : exposer le nouveau bloc de settings.

### 4.2 `vibe/` (app-server et config)

- `vibe/core/config/models.py` : modèle Pydantic `AgentCompactionSettings`
  (`enabled`, `min_input_tokens`, `max_per_session`, `min_growth_tokens`, `max_handoff_bytes`),
  branché dans le modèle `Tools` existant.
- `vibe/core/config/_defaults.py` + `vibe/core/config/vibe_schema.py` : défauts et schéma.
- `vibe/app_server/_runtime.py` (~1719, là où `model_wait=RustEnabledModelWait()` est posé) :
  mapper la config vers `RustToolSettings.agent_compaction`.
- `vibe/app_server/_unified_tool_projection.py` (~1807, à côté de `"self.sleep"`) :
  `"request_compaction": _ToolLabel("Compacting", "", "Compacted")`.
- `vibe/app_server/_session_api_commands.py` : rien à changer ; `compact()` reste la commande
  manuelle.

### 4.3 `vibe/cli-rust/`

Rien d'obligatoire en v1 : la ligne de statut de compaction est déjà peinte depuis les
checkpoints, et le trigger n'y est pas affiché. Si on veut expliciter l'origine, c'est
`ui/transcript/entry/status.rs:21` + un scénario `client-e2e` et son golden. À faire dans un
second temps.

## 5. Tests

### 5.0 Commandes — attention, `make` ne couvre pas le harness

Le `Makefile` de la racine pilote **uniquement le crate de la TUI**
(`M = --manifest-path vibe/cli-rust/Cargo.toml`). `make test_rust` ne testera donc *rien* du
harness. Pour `harness/core` :

```bash
cd harness/core
cargo nextest run          # suite complète (0.9.146 installé)
cargo test                 # repli si nextest indisponible
cargo fmt && cargo clippy --all-targets -- -D warnings
```

Le crate de la TUI a ses propres règles (`vibe/cli-rust/AGENTS.md`) : `make fmt`, `make lint`,
`make test_rust` depuis `vibe/`. Ne pas mélanger les deux périmètres.

### 5.1 Conventions de code

- Code, commentaires, noms de tests et messages de commit en **anglais** : c'est la langue du
  repo. Seuls `docs/agent-compaction/*.md` sont en français (doc de travail du fork).
- `harness/core` utilise des modules de test inline (`#[cfg(test)] mod tests`) et
  `src/tests/acceptance_scenarios/` — c'est acceptable ici. L'interdiction des tests inline
  ne concerne que `vibe/cli-rust`.
- Rust : `edition = 2024`, pas de `unwrap()` dans le code de production.
- Python : `uv run ruff check --fix .`, `uv run ruff format .`, `uv run pyright` (strict) après
  chaque changement — règle AGENTS.md.

### 5.2 Tests à écrire

Ordre de priorité, en même temps que le code :

1. `harness/core/src/tests/acceptance_scenarios/compaction/agent.rs` (nouveau, enregistré dans
   `compaction/mod.rs`), sur le modèle des fichiers voisins :
   - plan → handoff → outil → fin de tour → compaction → reprise dans le même tour ;
   - le contexte après reprise contient résumé **puis** note ;
   - handoff absent → échec d'outil, aucune compaction, le tour continue ;
   - sous le plancher → refus ;
   - quota atteint → refus ;
   - deux appels même tour → une seule compaction ;
   - échec de compaction (résumé refusé) → historique d'origine, tour repris ;
   - flag off → l'outil n'est pas dans le catalogue (`builtin_direct_tools`).
2. Unitaires core pour `admit_agent_compaction` et `Refusal::message`.
3. Python : `tests/` miroir pour `_agent_compaction.py` (lecture, troncature, échappement de
   chemin, fichier vide).
4. `vibe/` : test de mapping config → settings runtime.

## 6. Ordre d'implémentation

Chaque étape compile et passe ses tests avant la suivante.

1. Settings + **description dans les prompts** + outil déclaré + route, flag off : vérifier que
   le catalogue est inchangé quand le flag est à `false`.
2. `CompactionTrigger::Agent` + `CompactionRequest::Agent` + continuation `InTurn` avec
   trigger, sans outil : brancher un test unitaire qui démarre une compaction agent
   directement.
3. Outil actif : schéma, route, exécution runtime (lecture du handoff via `_file_tools`), note
   renvoyée.
4. Détection du succès dans `finish_pending_tool` + `PendingAgentCompaction` + accesseur
   `ToolBatch::call`.
5. Reprise du tour en fin de tour (`apply_tool_batch_transition`).
6. Réinjection de la note après le résumé.
7. `admit_agent_compaction` et les refus — à câbler avant toute utilisation réelle : sans cette
   étape, l'outil accepte tout.
8. Config `vibe/` + projection UI.
9. Acceptance tests complets.

Ne pas sauter l'étape 7 avant de tester à la main : un outil qui promet une compaction qu'il
n'exécute pas est pire que pas d'outil du tout.

## 7. Deux commandes côte à côte : `vibe` et `vibe-fork`

Contrainte : `vibe` doit continuer à lancer le Vibe officiel. Le fork s'installe donc sous un
second nom.

`[project.scripts]` expose `vibe-fork = "vibe.cli.launcher:main"` — même point d'entrée,
ajout purement additif. Le launcher est agnostique au nom : il ne lit pas `argv[0]` pour
choisir le binaire mais le résout depuis le paquet (`launcher.py:68`), et cherche l'app-server
comme **voisin de `sys.argv[0]` résolu** (`launcher.py:88`). Une entrée `vibe-fork` installée
dans un venv dédié est donc autonome : elle trouve son `vibe-app-server` dans le même `bin/`.

### Installation

Ne **pas** utiliser `uv tool install` : il écrirait aussi `vibe` dans `~/.local/bin` et
remplacerait l'officiel. Venv dédié, puis un seul lien symbolique :

```bash
uv venv ~/.vibe-fork/venv
uv pip install --python ~/.vibe-fork/venv/bin/python /Users/valentin/Code/Tests/mistral-vibe-fork
ln -sfn ~/.vibe-fork/venv/bin/vibe-fork ~/.local/bin/vibe-fork
```

`vibe` reste l'officiel, `vibe-fork` lance le fork. Après une modification Rust, relancer les
deux premières lignes.

### Développement

```bash
uv sync --all-extras
uv run vibe     # dans le repo, sans rien installer sur le PATH
```

Le build Python (`build_backend/maturin_backend.py`) compile **les deux** binaires Rust :
l'extension du harness (`harness/core` → `mistralai_vibe_local_harness._native`) et la TUI
(`vibe/cli-rust` → `vibe/_bin/vibe-rs`). Sans `cargo`, échec explicite
(`maturin_backend.py:101`).

Build non éditable assumé : chaque changement Rust exige de toute façon une recompilation,
autant la déclencher explicitement plutôt qu'à l'import.

### Toolchain (installé, vérifié)

| Élément | Version |
|---|---|
| `rustc` / `cargo` | 1.97.1 aarch64-apple-darwin, profil minimal |
| composants | `rustfmt`, `clippy`, `llvm-tools` |
| `cargo-nextest` | 0.9.146 (prébuilt, version épinglée par AGENTS.md) |
| `uv` | 0.11.7 (déjà présent) |

Installé via `rustup` avec `--no-modify-path` : `$HOME/.cargo/bin` n'est pas dans le `PATH`
global. Pour appeler `cargo` directement, `source "$HOME/.cargo/env"`.

### Config partagée

Les deux commandes lisent le même `~/.vibe/config.toml` : clé API, modèles et thème communs,
rien à dupliquer.

```toml
[tools.agent_compaction]
enabled = true
```

Conséquence à connaître : le Vibe officiel ne connaît pas cette section. Il ne plante pas —
le schéma tolère les clés inconnues (`extra="ignore"`, cf. le commentaire de
`config/models.py:107`) — mais il affiche « Config setting ignored : tools.agent_compaction »
(`vibe_schema.py:489`). Avec `strict_config = true`, c'est en revanche une erreur de
chargement. Si la gêne devient réelle, la sortie est un `VIBE_HOME` distinct pour le fork, à
trancher le moment venu.

## 8. Workflow fork / upstream

Déjà en place : `origin` = `ValentinMalassigne/mistral-vibe`, `upstream` =
`mistralai/mistral-vibe`, branche `feat/agent-compact` sur `main` @ v3.1.0.

```bash
git fetch upstream && git checkout main && git merge --ff-only upstream/main
git push origin main
git checkout feat/agent-compact && git rebase main
```

Le diff touche ~10 fichiers Rust et ~6 Python, tous localisés. Le risque n'est pas la taille
mais le point d'accroche `apply_tool_batch_transition` et `CompactionContinuation`, qui sont
du code actif.

L'entrée `vibe-fork` de `[project.scripts]` est une commodité **propre au fork** : à retirer
si on ouvre une PR upstream, elle n'a de sens que pour faire cohabiter les deux commandes.

## 9. Estimation

| Étape | Volume |
|---|---|
| Settings + outil + route | ~120 lignes Rust |
| Trigger + `CompactionRequest::Agent` | ~60 lignes Rust |
| État + détection + reprise + note | ~150 lignes Rust |
| Garde-fous | ~100 lignes Rust |
| Runtime Python (exécution + protocole) | ~120 lignes Python |
| Config app-server + projection | ~80 lignes Python |
| Tests | ~400 lignes |

Total ~1 000 lignes, dont la moitié de tests. 10 fichiers Rust, 6 Python.

## 10. Risques

| Risque | Mitigation |
|---|---|
| Conflit upstream sur `turn.rs` / `compaction/execution.rs` | diff minimal, rebase fréquent, pas de refactor opportuniste |
| Reprise après compaction mal ordonnée (note avant/après résumé) | test d'acceptance dédié sur l'ordre du contexte |
| Boucle de compactions | croissance minimale + quota, pas seulement le quota |
| Note trop grosse | borne en octets, troncature signalée au modèle (`truncated: true`) |
| Modèle qui appelle l'outil sans handoff | paramètre requis + refus actionnable, le modèle se corrige |
| État transitoire perdu à la reprise d'une session | assumé : l'état de compaction n'est déjà pas checkpointé ; le résultat d'outil, si |
