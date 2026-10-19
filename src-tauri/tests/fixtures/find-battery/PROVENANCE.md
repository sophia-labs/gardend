# find-battery fixtures: provenance

Question sets for the `find` regression battery (`scripts/find-battery.py`). The questions are hand-written; the gold
answers point at public data.

- `catalogue.json`: questions whose golds are dataset IRIs in the Uruguayan open-data catalogue
  (catalogodatos.gub.uy). Uruguay open data; attribution to the publishing agencies per the catalogue's licence terms.
- `cows.json`: questions whose golds are `urn:sophia:cow:*` IRIs from the cow-tools demo graph, built from Uruguayan
  open agricultural data (same attribution as above).
- `wikidata.json`: questions whose golds are Wikidata entity IRIs (a Uruguay and Argentina slice). Wikidata is CC0.

The fixtures contain no personal data and no private graph content.
