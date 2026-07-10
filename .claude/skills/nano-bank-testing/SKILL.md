---
name: nano-bank-testing
description: Use when testing nano-bank, running the simulators, writing Bruno API requests, or resetting local data — covers the container harness, the Streamlit viewer, Bruno's format quirk, and cleanup.
---

# nano-bank testing & dev harness

No Rust tests exist yet (dev-dependencies are wired). Exercise the API through
the Python container harness in `testing/` and the Bruno collections in `bruno/`.

## Container harness (testing/)

- `generator/generate_customers.py` — seeds fake Canadian customers + accounts
  via the API.
- `visa/visa_simulator.py` — drives the full card authorize → capture → settle
  loop.
- `viewer/app.py` — Streamlit live dashboard on **:8504**.
- New rails add their own simulator (e.g. `testing/interac/interac_simulator.py`
  plays the Interac network by reading the notification outbox) and extend the
  viewer with a tab.

## Reset local data

- `testing/cleanup.sh --dry-run` — preview row counts.
- `testing/cleanup.sh` — `TRUNCATE customers CASCADE` (wipes all data; GL /
  system accounts self-heal on next startup).

## Bruno .bru format quirk

Bruno IGNORES a `body:json {}` block unless the `post {}` block ALSO declares
`body: json` and `auth: inherit` alongside the URL. Copy the format Bruno
generates from its own UI (see the working `_2` files in `bruno/`); don't
hand-write it from scratch.

## Infra

`kind create cluster --config k8s/kind-cluster-config.yaml`, then
`./k8s/deploy.sh` (Postgres + DDL init Job). The init Job loads
`src/core/tables/*.sql` in filename order — add new schema as the next-numbered
file.
