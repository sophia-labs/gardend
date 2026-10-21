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
  along the centre-to-centre line, 6 px outside each shape (Excalidraw's binding gap, 5 + strokeWidth/2).
* Links: a shape linked to a document has `link = "mnemosyne://document/<graphId>/<documentId>"` and
  `customData.mnemosyne = { kind, graphId, id, title }` (the Files canvas scheme, `excalidraw-scene-links.ts`).
  A link to anything else (`iri`, `url`) is the string itself.

## Attribution

Every element written by a gardend tool carries `customData.sophia = { "by": "<actor>", "at": <epoch ms> }`
with `<actor>` the caller's identity as gardend knows it (`owner`, `agent:<id>`, `service:<name>`). An element
changed by a person in the browser carries `by: "person"`. Only what is known is recorded.

## Kinds (the vocabulary of `whiteboard_read`)

`box` = rectangle with a transparent fill; `note` = rectangle with a fill; `ellipse`, `diamond`, `text`
(unbound text), `arrow`, `line`, `frame`, `image`, `freedraw`. A container's bound text is folded into its
`text`.

## The saved record

`body` = the outline markdown (one deterministic function, `outline::outline_markdown`), `blocks` = `[]`,
`tiptapXml` = `""`, the Y.Doc bytes verbatim. So `search_documents`, `search_blocks`, `read_document`,
`context_bundle` and the semantic index see the board's words.
