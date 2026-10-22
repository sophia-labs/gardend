# The whiteboard document format (`sophia.whiteboard/1`)

A whiteboard is a document whose workspace entry has `documentKind: "whiteboard"`.
Its Y.Doc (the document's room, the same room the editor joins) has **no** TipTap
`content` fragment. It has exactly these roots. Gardend (Rust, `whiteboard/`) and
Shrubbery (`packages/runtime/src/whiteboard/`) both read and write them, and both test
against one fixture, `src-tauri/tests/fixtures/whiteboard/whiteboard-fixture-v1.json`.

## Roots

| root | type | holds |
|---|---|---|
| `elements` | `Y.Map<string, string>` | key = Excalidraw element id; value = that element's full JSON, stringified |
| `files` | `Y.Map<string, string>` | key = file id; value = `BinaryFileData` JSON. **v1 does not share images**: the shell says "images are not shared yet" and drops them from the scene; gardend never writes this root. |
| `meta` | `Y.Map` | `schema: "sophia.whiteboard/1"`, `viewBackgroundColor` (a CSS colour) |

Scroll, zoom, selection and the active tool are per viewer and never go in the Y.Doc.

## Elements

* The collaboration unit is the **whole element**: last writer wins per element id, as Excalidraw's own
  collaboration reconciles. Why strings and not nested Y.Maps: one atomic value per element matches
  Excalidraw's reconcile model, makes the Rust side a JSON round trip, and avoids partial-element merges
  Excalidraw cannot render.
* A writer bumps `version` and sets a fresh random `versionNonce` (an integer) on every change, and sets
  `updated` (epoch ms). Between two copies of one element, the higher `version` wins; on a tie the lower
  `versionNonce` wins (Excalidraw's rule).
* A deletion is a **tombstone**: the element stays with `isDeleted: true` and a bumped `version`, so a stale peer
  cannot resurrect it.
* Z-order is Excalidraw's fractional `index` string.
* Bound text: the text element carries `containerId`; the container carries
  `boundElements: [{ "type": "text", "id": "<text id>" }]` (a container is a box, ellipse, diamond, or an arrow
  for a label). Arrows list `{ "type": "arrow", "id": ... }` on every shape they are bound to.
* Arrow bindings follow the pinned build, `@excalidraw/excalidraw 0.18.0-4ce38fb`, which has no `focus`/`gap`:
  `startBinding` / `endBinding` = `{ "elementId", "fixedPoint": [0.5, 0.5], "mode": "orbit" }`. The arrow's
  `x, y` is its first point and `points` are relative to it; a tool-routed arrow runs from outline to outline
  along the centre-to-centre line, 5.5 px outside each shape (Excalidraw's binding gap, 5 + strokeWidth/2, with the thin stroke below).
* Links: a shape linked to a document has `link = "mnemosyne://document/<graphId>/<documentId>"` and
  `customData.mnemosyne = { kind, graphId, id, title }` (the Files canvas scheme, `excalidraw-scene-links.ts`).
  A link to anything else (`iri`, `url`) is the string itself.

## How new elements look (the Sophia hand)

Gardend's tools and Shrubbery's pen draw new elements with the same values, so a board reads as one hand. These are
defaults for **new** elements only; an element already on a board is never rewritten to match.

| | value |
|---|---|
| `roughness` | `0` (architect, no wobble) |
| `strokeWidth` | `1` (thin) |
| `fillStyle` | `solid` |
| `roundness` | `{type: 3}` on rectangles and notes, `{type: 2}` on diamonds, `null` on ellipses, arrows and lines |
| `fontFamily` | `6` (Nunito, the built-in family closest to Sophia's text), `fontSize` 20 |
| `strokeColor` | `#1d1c19`, Sophia ink (the old default `#1e1e1e` still reads as `black`) |
| arrows | a tool-drawn arrow is a thin, sharp, straight segment (`elbowed: false`); a person's pen draws elbow arrows (`elbowed: true`). A tool re-route of an elbow arrow makes it straight |

The colour names of `whiteboard_apply` are Sophia hues (light values, from `reference.css`; Excalidraw's dark theme
inverts the canvas by filter): `green` fern-600 on fern-100, `yellow` ochre-600 on ochre-100, `red` carmine-700 on
carmine-100, `blue` cobalt-500 on cobalt-100, `violet` indigo-600 on indigo-100, `grey` parchment-500 on parchment-100,
`black` ink. A board drawn with the earlier open-color picks still reads back by the same names.

An arrow's label sits on its middle. When a label would touch the shapes (the default `near` gap is 60 px), the tool
pushes apart a shape **made in the same call** along the line between the two centres until the label clears both;
shapes already on the board are never moved for it.

## Attribution

Every element written by a gardend tool carries `customData.sophia = { "by": "<actor>", "at": <epoch ms> }`
with `<actor>` the caller's identity as gardend knows it (`owner`, `agent:<id>`, `service:<name>`). An element
changed by a person in the browser carries `by: "person"`. Only what is known is recorded.

## Kinds (the vocabulary of `whiteboard_read`)

`box` = rectangle with a transparent fill; `note` = rectangle with a fill; `ellipse`, `diamond`, `text`
(unbound text), `arrow`, `line`, `frame`, `image`, `freedraw`. A container's bound text is folded into its
`text`. `card` = an `embeddable` whose `link` is a `mnemosyne://document|object` link (a typed object or a document
shown live on the board); its `text` is the label its `customData.mnemosyne` snapshot keeps. An `embeddable` with
any other link reads as a `box`. A card is not a text container: it has no bound text.

## The saved record

`body` = the outline markdown (one deterministic function, `outline::outline_markdown`), `tiptapXml` = `""`, the
Y.Doc bytes verbatim. `blocks` = one block per live shape, in the outline's reading order (`blocks::blocks_of`):

| field | value |
|---|---|
| `id` | the element id, exactly (the same id the lane's `#block-{id}` subject and a wire's `sourceBlockId` use) |
| `type` | the shape's kind: `box`, `note`, `ellipse`, `diamond`, `text`, `frame`, `image`, `freedraw`, `card` |
| `content` | the shape's words; then, after an em dash, its link title when that adds words; a card says `{class label}: {label}` (just the label for a document card) |
| `parentId` | the id of the frame the shape sits in, when it has one that is live |
| `order` | 0, 1, 2, ... in reading order |
| `level`, `checked`, `language`, `marks` | none |

Arrows, bound text and deleted elements are not blocks; frames are. A shape with no words still gets a block (it
can be a wire end). A board with no live shapes has `blocks: []`, and the semantic index falls back to the body.
Re-saving an unchanged board gives byte-identical blocks. So `search_documents`, `search_blocks`, `read_blocks`,
`read_document`, `context_bundle` and the semantic index see the board's words, one passage per shape.

A board is drawn, not written: `insert_blocks`, `update_blocks`, `edit_block_text` and `delete_blocks` refuse a
whiteboard (`{tool}: a whiteboard is drawn, not written: use whiteboard_apply`). Use `whiteboard_apply`.

## Cards, wires and customData

* A card: `type: "embeddable"`, `link` = the object or document link, no text, `boundElements: []`. Its snapshot is
  `customData.mnemosyne` = `{kind: "object", graphId, iri, label, classLabel}` or `{kind: "document", graphId, id,
  title}`.
* A promoted arrow carries `customData.mnemosyneWire = {wireId, predicate, strokeWas, labelFromPredicate}`; the
  `wire` move gives it the accent stroke (`#326f4c`) and, when it had no label, the predicate as its label.
