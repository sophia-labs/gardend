# Rich Markdown Visual Parity Review

This imported Markdown file is meant for visual inspection in the local Tauri app. It touches headings, inline marks, lists, quotes, code, tables, and footnotes in one readable document.

## Inline Formatting

Plain text should sit beside **bold text**, *italic text*, ***bold italic text***, `inline code`, ~~struck text~~, and a [link to the local parity notes](../semantic-corpus/README.md).

Escaped punctuation should remain literal: \*not italic\*, \`not code\`, and \[not a link\].

## Callout Quote

> Local artifact ingestion should preserve enough structure that imported documents feel native.
>
> A second paragraph in the same quote checks spacing and block continuity.

## Nested Lists

- Top-level bullet with a sentence long enough to wrap in the editor pane and reveal indentation behavior.
  - Nested bullet with **bold emphasis**.
  - Nested bullet with `inline code`.
    - Third-level bullet for deeper nesting.
- Another top-level bullet after the nested run.

1. Ordered item one.
2. Ordered item two with a nested checklist:
   - [x] Completed parser coverage item.
   - [ ] Open visual review item.
   - [ ] Follow-up parity harness item.
3. Ordered item three.

## Code Blocks

```ts
type ArtifactReview = {
  title: string
  importedAt: string
  checks: Array<'headings' | 'marks' | 'lists' | 'tables' | 'footnotes'>
}

export const review: ArtifactReview = {
  title: 'Rich Markdown Visual Parity Review',
  importedAt: '2026-04-30',
  checks: ['headings', 'marks', 'lists', 'tables', 'footnotes'],
}
```

```bash
pnpm parity:loopback
```

## Table Coverage

| Surface | Expected Rendering | Notes |
| --- | --- | --- |
| Headings | Distinct levels | H1, H2, and H3 appear in sequence |
| Inline marks | Bold, italic, code, strike | Mixed marks should not collapse |
| Lists | Nested bullets and tasks | Indentation should be readable |
| Tables | Header and body rows | Cells should remain aligned |

### Small Heading After Table

Paragraph immediately after a table checks whether spacing recovers correctly.

---

## Footnote And Reference

This sentence has a footnote marker.[^visual-note] It should not swallow the surrounding paragraph.

[^visual-note]: Footnote body with **bold text**, `inline code`, and a short explanation for visual review.

## Final Checklist

- [x] Markdown source uploaded through local loopback.
- [x] Original bytes are preserved for download.
- [ ] Visual rendering reviewed in the Tauri app.
