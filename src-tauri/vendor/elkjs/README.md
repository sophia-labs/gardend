# elkjs (vendored)

The board layout engine: [ELK](https://eclipse.dev/elk/) compiled to JavaScript by
[elkjs](https://github.com/kieler/elkjs). gardend runs it on QuickJS (`src/elk/`); Shrubbery runs the same
version in a Web Worker, so a board laid out by an agent and one laid out by a person come out the same.

| | |
|---|---|
| package | `elkjs@0.12.0` (npm) |
| file | `lib/elk.bundled.js`, unmodified |
| sha256 | `1222e44f953ce7746af23801e723708f8e6f436b8b377a6a5fc7552f34a307b3` |
| licence | EPL-2.0 OR GPL-3.0-or-later (`LICENSE.md`, from the package) |

To upgrade: replace the file with the new release's `lib/elk.bundled.js`, update the version and sha256 here and the
`elkjs` version in `src/elk/presets.json`, and bump Shrubbery's pin in the same change. `elk::tests` checks the sha256.
