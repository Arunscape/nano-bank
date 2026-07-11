import pytest
from agent.memory import QdrantMemory, AuditLog


@pytest.fixture
def mem():
    return QdrantMemory.in_memory()


def test_store_and_recall(mem):
    mem.store("client prefers e-transfer over cheque", customer_id="A")
    hits = mem.recall("how does the client like to send money", customer_id="A", k=3)
    assert any("e-transfer" in h for h in hits)


def test_recall_is_customer_scoped(mem):
    mem.store("A's secret goal is a boat", customer_id="A")
    assert mem.recall("boat", customer_id="B", k=5) == []


def test_invalidate_hides_fact(mem):
    fid = mem.store("old address is 1 Main St", customer_id="A")
    mem.invalidate(fid, reason="moved")
    assert all("1 Main St" not in h for h in mem.recall("address", customer_id="A", k=5))


def test_audit_append_and_read():
    a = AuditLog.in_memory()
    a.record({"customer_id": "A", "kind": "transfer", "amount": "50", "outcome": "proposed"})
    rows = a.for_customer("A")
    assert len(rows) == 1 and rows[0]["outcome"] == "proposed"
