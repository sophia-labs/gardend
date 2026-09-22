# Semantic Corpus Fixtures

These stock documents are intentionally larger than smoke-test notes and use
stable IDs so the local-native and legacy stacks can be seeded with comparable
graphs for semantic search, block search, navigation, and document materialization
parity checks.

Use the seed harness from the prototype root:

```bash
pnpm parity:seed-fixtures
```

The harness creates or refreshes dedicated fixture graphs/workspaces in both
stacks, writes the documents through each stack's document API surface, flushes
local CRDT materialization, and refreshes the local semantic index.
