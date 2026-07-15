from agent.external_agent.agent import ExternalAgent, _idem_key


class FakeGW:
    def __init__(self): self.calls = []
    def act(self, op, params): self.calls.append((op, params)); return {"decision": "allow", "result": {}}
    def message(self, msg): self.calls.append(("message", msg)); return {"answer": "savings are good", "trace": []}


def test_planned_steps_call_the_gateway():
    gw = FakeGW()
    a = ExternalAgent.from_plan([("act", "transfer_out", {"amount": "50"}),
                                 ("message", "benefits of a savings account?")], gateway=gw)
    events = a.run("pay my Epcor bill and tell me about savings")
    ops = [c[0] for c in gw.calls]
    assert "transfer_out" in ops and "message" in ops
    assert any(e["kind"] == "result" for e in events)
    assert events[0]["kind"] == "plan"


class _RecordingGW:
    def __init__(self): self.acts = []
    def act(self, op, params): self.acts.append((op, dict(params))); return {"decision": "allow"}
    def message(self, msg): return {"answer": "ok", "trace": []}


def test_idem_key_is_stable_within_a_run_and_step():
    # A retried act (same run, same step, same params) must dedupe at the bank:
    # identical key. This is the #3 protection, now scoped to one run+step.
    a = _idem_key("transfer_out", {"amount": "50"}, run_id="RUN1", step_idx=0)
    b = _idem_key("transfer_out", {"amount": "50"}, run_id="RUN1", step_idx=0)
    assert a == b


def test_idem_key_differs_across_runs():
    # blashkar's new edge: a legitimate repeat (e.g. next month's bill, same op,
    # same params) must NOT collide with the earlier transaction under the bank's
    # unbounded replay window. A per-run component makes it a distinct payment.
    a = _idem_key("transfer_out", {"amount": "50"}, run_id="RUN1", step_idx=0)
    b = _idem_key("transfer_out", {"amount": "50"}, run_id="RUN2", step_idx=0)
    assert a != b


def test_idem_key_differs_across_steps_in_one_plan():
    # Two identical steps within one plan ("pay $50 twice") must be two payments,
    # not one deduped call.
    a = _idem_key("transfer_out", {"amount": "50"}, run_id="RUN1", step_idx=0)
    b = _idem_key("transfer_out", {"amount": "50"}, run_id="RUN1", step_idx=1)
    assert a != b


def test_two_runs_with_identical_params_get_different_keys():
    # End-to-end: replaying the exact same instruction as a fresh run() must
    # produce a different idempotency key so the bank treats it as a new payment.
    gw = _RecordingGW()
    agent = ExternalAgent.from_plan([("act", "transfer_out", {"amount": "50"})], gw)
    agent.run("pay the bill")
    agent.run("pay the bill")
    k1 = gw.acts[0][1]["idempotency_key"]
    k2 = gw.acts[1][1]["idempotency_key"]
    assert k1 != k2, "a legitimate repeat run must not reuse the earlier key"


def test_duplicate_steps_within_one_plan_get_different_keys():
    gw = _RecordingGW()
    agent = ExternalAgent.from_plan(
        [("act", "transfer_out", {"amount": "50"}),
         ("act", "transfer_out", {"amount": "50"})], gw)
    agent.run("pay the bill twice")
    assert gw.acts[0][1]["idempotency_key"] != gw.acts[1][1]["idempotency_key"]
