# demos/

Interactive **demo views** for nano-bank — guided Streamlit apps that drive the
real REST API to show a flow end to end. Distinct from `testing/` (the automated
harness + payment-network simulators): these are for *showing* the bank, one demo
per subdirectory.

| Demo | Dir | What it shows |
|------|-----|---------------|
| Onboarding | `onboarding/` | Create a customer → open accounts → post deposit/withdrawal/transfer, over the consumer API (identity from the customer JWT). |

_More demos ahead (each gets its own `demos/<name>/`)._

## Running a demo

Demos talk to the bank API. In the k8s setup the API isn't published to the host,
so port-forward it first:

```bash
kubectl --context kind-nano-bank -n nano-bank port-forward svc/bank-api 8081:8081
```

Then run the demo (point `DEMO_API_BASE` elsewhere if your API is not on
`localhost:8081`):

```bash
pip install -r demos/onboarding/requirements.txt
DEMO_API_BASE=http://localhost:8081 streamlit run demos/onboarding/app.py
```

(If you run the bank as a host `cargo run`, it's already on `http://localhost:8081`
and no port-forward is needed.)
