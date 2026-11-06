# Spec — Compaction déclenchée par l'agent

Statut : proposition validée sur le code de `main` @ v3.1.0 (`c96a6dbb`).
Branche : `feat/agent-compact`. Fork : `ValentinMalassigne/mistral-vibe`.

## 1. Objectif

Aujourd'hui `/compact` est une commande **utilisateur** : l'humain décide de compacter. Le
déclenchement automatique, lui, est purement réactif (seuil de tokens). Il manque le cas
intermédiaire : **l'agent sait** qu'il arrive à un point de rupture et veut compacter
lui-même, au bon moment.

Cas d'usage cible : l'agent suit un plan (`plan.md` ou équivalent) qui contient une étape
explicite du type « compacter le contexte avant de passer à la phase suivante ». Aujourd'hui
l'agent ne peut que le signaler en texte et attendre que l'humain tape `/compact`, ce qui
casse le flux.

L'agent peut donc demander une compaction, en fournissant une **note de passation**
(checkpoint) qui survit à la compaction. La compaction devient non destructive : le résumé
remplace l'historique, la note réinjecte ce qui compte pour la suite.

## 2. Non-objectifs

- Pas de compaction en cours de tour côté agent (voir §4.1 : la machine à états l'interdit).
- Pas de remplacement de l'autocompaction par seuil : les deux coexistent.
- Pas d'accès agent aux commandes slash en général. Un seul point d'entrée, étroit.
- Pas de changement du prompt de résumé côté agent : il ne peut pas réécrire la consigne de
  compaction, seulement ajouter des instructions libres.

## 3. Comportement attendu

```
agent suit plan.md
  └─ étape "compacter avant phase 2"
       ├─ 1. agent écrit son checkpoint (write_file) → .vibe/handoff-phase2.md
       ├─ 2. agent appelle request_compaction(handoff_path=..., extra_instructions=...)
       │       └─ le harness valide, met l'intention en attente
       ├─ 3. l'agent termine son tour (plus aucun outil à appeler)
       ├─ 4. le harness lance la compaction (trigger = agent)
       │       ├─ résumé de l'historique par le modèle
       │       ├─ historique remplacé par le résumé
       │       └─ le checkpoint est réinjecté juste après le résumé
       └─ 5. le tour reprend : l'agent reçoit le résumé + sa note et continue la phase 2
```

Points clés :

- **Étape 5 est le cœur de la valeur.** L'agent ne s'arrête pas pour attendre l'humain : le
  tour continue après la compaction, comme le fait déjà l'autocompaction en cours de tour
  (`CompactionResolution::ContinueTurn`). Le plan se déroule sans intervention.
- La compaction est **différée à la fin du tour**, pas immédiate (§4.1).
- Le résumé et la note arrivent dans le même tour, donc l'agent n'a pas besoin d'un nouveau
  message utilisateur pour reprendre.
- La note est un fichier sur disque : elle reste lisible par l'humain, et elle survit même
  si la compaction échoue.

## 4. Décisions de conception

### 4.1 La compaction est différée à la fin du tour — et c'est la seule option

`TurnEvent::ManualCompaction` n'est accepté que si le tour n'est ni `Active` ni `Compacting`
(`harness/core/src/core/turn.rs:129`), sinon `invalid_state`. Un outil qui tenterait de
compacter en cours de tour échouerait systématiquement.

Le chemin « prochain tour » n'est donc pas une préférence de prudence, c'est le seul
compatible avec la machine à états. Et il se trouve que c'est le comportement souhaité : la
compaction après la fin du tour, suivie d'une reprise automatique, est exactement ce qu'un
agent qui suit un plan attend.

### 4.2 Un outil direct, nommé `request_compaction`

Le nom dit ce qui se passe : on **demande**, on ne déclenche pas synchroniquement. Un modèle
qui appelle `compact` et reçoit « fait » alors que rien n'a encore été compacté serait
trompé.

Surface : **outil direct** (comme `wait`, `read_file`), pas le namespace TypeScript `self.*`.
Raison : l'outil ne sert pas à écrire des programmes, et le namespace programmatique impose
en plus le schéma de checkpoint des programmes. Détail des coûts en §2 du plan.

### 4.3 Le checkpoint est obligatoire

`handoff_path` est un paramètre **requis**. Sans note, une compaction est une perte
d'information assumée mais aveugle : c'est précisément l'accident que l'agent est le mieux
placé pour éviter, puisque c'est lui qui sait ce qui va lui manquer.

Le harness refuse la demande si le fichier est absent ou vide, avec un message qui dit à
l'agent d'écrire la note d'abord.

### 4.4 Le harness ne lit pas le fichier

Core ne fait pas d'I/O fichier : il émet des actions, le runtime les exécute. Dans cette
architecture, c'est le **runtime Python** qui lit le checkpoint au moment de l'exécution de
l'outil, et qui renvoie son contenu (borné) au core dans le résultat structuré de l'outil.
Core stocke cette note et l'injecte après le résumé. Aucune exception au principe de
propriété Core/Runtime.

Taille bornée (8 Kio par défaut, tronqué avec mention explicite) : une note trop grosse
mangerait la fenêtre qu'on vient de libérer.

### 4.5 Trace différenciée

Nouvelle valeur `CompactionTrigger::Agent` à côté de `Manual` et `Automatic`, pour que les
logs, la télémétrie et l'UI distinguent les trois origines. Le résumé est produit par le
même chemin que la compaction manuelle.

## 5. L'outil

```
request_compaction(
  handoff_path: string,          // requis, chemin relatif au workspace
  extra_instructions?: string,   // consignes additionnelles pour le résumé
  reason?: string                // une ligne, journalisée, pas envoyée au résumeur
)
```

Description exposée au modèle (à affiner) :

> Compact the conversation to free context. Use it when your current plan says to compact
> before continuing, or when you are about to start a long phase that needs room. Before
> calling it, write what must survive the compaction — decisions taken, files touched,
> remaining steps — to `handoff_path`. The compaction runs when your turn ends and the
> conversation resumes right after, with your handoff note re-injected.

Résultat structuré renvoyé au core par le runtime :

```json
{
  "handoff_note": "<contenu du fichier, borné>",
  "handoff_bytes": 1234,
  "truncated": false
}
```

Pas de champ « accepted » : le runtime ne décide de rien, il lit le fichier et le passe. C'est
le core qui accepte ou refuse, immédiatement, pour que le modèle voie le refus dans le même
tour.

En cas de refus, l'outil retourne un échec (`Failure`) et **rien n'est mis en attente** :
fichier absent ou vide, chemin hors workspace, flag désactivé, garde-fou de contexte, quota
atteint, ou compaction déjà en attente. Le message d'échec doit être actionnable (dire quoi
faire), parce que le modèle va le lire.

## 6. Garde-fous

| Garde-fou | Comportement | Défaut |
|---|---|---|
| Activation | Outil non exposé si désactivé (le modèle ne le voit même pas) | désactivé |
| Plancher de contexte | Refus si le contexte estimé est sous le plancher | 20 000 tokens |
| Quota par session | Refus au-delà de N compactions déclenchées par l'agent | 8 |
| Déjà en attente | Refus si une compaction est déjà en attente ou en cours | — |
| Croissance minimale | Refus si moins de X tokens ajoutés depuis la précédente compaction agent | 5 000 |
| Cooldown | Pas deux compactions agent dans le même tour | — |

Justification du plancher : sous ~20 K tokens, un résumé coûte un appel modèle, produit un
résumé moins bon que l'original, et fait perdre des détails pour un gain nul. Le plancher
empêche l'agent de « compacter pour compacter » en début de session.

Justification de la croissance minimale : c'est la protection anti-boucle réelle. Un quota
seul laisse passer une rafale de compactions inutiles en fin de session.

Les refus sont des échecs d'outil, pas des erreurs de protocole : le tour continue, le modèle
voit pourquoi et poursuit sans compacter.

## 7. Configuration

Un seul bloc, désactivé par défaut (cohérent avec la convention du repo : un outil qui change
le contexte ne s'active pas tout seul). `~/.vibe/config.toml` :

```toml
[tools.agent_compaction]
enabled = true
min_input_tokens = 20000
max_per_session = 8
min_growth_tokens = 5000
max_handoff_bytes = 8192
```

Le flag doit être visible dans `vibe config` et dans le schéma de config, avec migration
`vibe_schema.py` mise à jour.

## 8. Observabilité

- Observation `ContextCompacted { trigger: agent, .. }` : l'UI affiche l'origine.
- La note de passation est un message de contexte injecté, visible dans l'historique : on
  peut vérifier après coup ce que l'agent a décidé de conserver.
- Télémétrie : événement de compaction déjà existant, avec la nouvelle valeur de trigger.
- Log : `reason` de l'outil.

## 9. Critères d'acceptation

1. Flags désactivés : l'outil n'apparaît pas dans le catalogue ; aucun chemin de code agent
   ne peut compacter.
2. Plan qui demande une compaction : l'agent écrit sa note, appelle l'outil, le tour se
   termine, la compaction tourne, le tour reprend, et l'agent continue sans message humain.
3. Après reprise, l'historique contient le résumé **et** la note de passation, dans cet ordre.
4. Un fichier de passation absent ou vide fait échouer l'outil, sans compaction, avec un
   message qui dit d'écrire la note d'abord.
5. Sous le plancher de contexte : refus, message explicite, pas de compaction.
6. Quota atteint : refus, message explicite, pas de compaction.
7. Deux appels dans le même tour : le second est refusé, une seule compaction.
8. La compaction manuelle `/compact` et l'autocompaction par seuil sont inchangées.
9. Une compaction agent qui échoue (résumé refusé, trop gros, cut-off) laisse le tour
   reprendre avec l'historique d'origine : pas de perte silencieuse.
10. `/compact` reste utilisable pendant qu'une compaction agent est en attente.

## 10. Risques

- **Dérive upstream** : le point d'accroche principal (`apply_tool_batch_transition`,
  `turn.rs:701`) et la machine à états sont du code actif. C'est le principal risque de
  conflit, pas la taille du diff. Mitigation : diff petit et localisé, rebase régulier.
- **Le modèle appelle l'outil trop tôt ou trop souvent** : couvert par les garde-fous, et par
  le fait que l'outil n'est proposé que si activé.
- **Qualité du résumé** : identique à `/compact`, pas de régression attendue. La note de
  passation compense la perte de détail, c'est tout l'intérêt du design.
- **Surprise utilisateur** : une compaction non demandée change l'historique. Mitigation :
  trace `trigger = agent`, et le flag est opt-in.
