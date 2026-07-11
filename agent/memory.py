from __future__ import annotations
import time
import uuid
from typing import Optional

from qdrant_client import QdrantClient, models


def _embedder():
    from fastembed import TextEmbedding
    return TextEmbedding()  # small default CPU model


class QdrantMemory:
    def __init__(self, client: QdrantClient, collection: str, embed):
        self.client = client
        self.collection = collection
        self._embed = embed
        self._dim = len(next(iter(embed.embed(["dim probe"]))))
        if not client.collection_exists(collection):
            client.create_collection(
                collection,
                vectors_config=models.VectorParams(size=self._dim, distance=models.Distance.COSINE))

    @classmethod
    def in_memory(cls, collection: str = "test_mem") -> "QdrantMemory":
        return cls(QdrantClient(":memory:"), collection, _embedder())

    @classmethod
    def from_settings(cls, settings) -> "QdrantMemory":
        return cls(QdrantClient(url=settings.qdrant_url), settings.qdrant_collection, _embedder())

    def _vec(self, text: str):
        return list(next(iter(self._embed.embed([text]))))

    def store(self, fact: str, *, customer_id: str, kind: str = "observation",
              source: str = "agent", thread_id: Optional[str] = None) -> str:
        pid = uuid.uuid4().hex
        self.client.upsert(self.collection, points=[models.PointStruct(
            id=pid, vector=self._vec(fact),
            payload={"customer_id": customer_id, "kind": kind, "source": source,
                     "fact": fact, "thread_id": thread_id,
                     "valid_from": time.time(), "valid_to": None})])
        return pid

    def invalidate(self, fact_id: str, reason: str) -> None:
        self.client.set_payload(self.collection, payload={"valid_to": time.time(),
                                "invalidated_reason": reason}, points=[fact_id])

    def _valid_filter(self, customer_id: str, kind: Optional[str], thread_id: Optional[str]):
        must = [models.FieldCondition(key="customer_id", match=models.MatchValue(value=customer_id)),
                models.IsNullCondition(is_null=models.PayloadField(key="valid_to"))]
        if kind:
            must.append(models.FieldCondition(key="kind", match=models.MatchValue(value=kind)))
        if thread_id:
            must.append(models.FieldCondition(key="thread_id", match=models.MatchValue(value=thread_id)))
        return models.Filter(must=must)

    def query_valid(self, customer_id: str, kind=None, thread_id=None) -> list[dict]:
        pts, _ = self.client.scroll(self.collection, limit=200,
                                    scroll_filter=self._valid_filter(customer_id, kind, thread_id))
        return [p.payload for p in pts]

    def recall(self, query: str, customer_id: str, k: int = 3, thread_id=None) -> list[str]:
        hits = self.client.query_points(
            self.collection, query=self._vec(query), limit=k,
            query_filter=self._valid_filter(customer_id, None, thread_id)).points
        return [h.payload["fact"] for h in hits]


class AuditLog:
    def __init__(self, client: QdrantClient, collection: str = "nano_manager_audit"):
        self.client = client
        self.collection = collection
        if not client.collection_exists(collection):
            client.create_collection(
                collection, vectors_config=models.VectorParams(size=1, distance=models.Distance.DOT))

    @classmethod
    def in_memory(cls) -> "AuditLog":
        return cls(QdrantClient(":memory:"))

    @classmethod
    def from_settings(cls, settings) -> "AuditLog":
        return cls(QdrantClient(url=settings.qdrant_url))

    def record(self, event: dict) -> str:
        pid = uuid.uuid4().hex
        event = {**event, "ts": time.time()}
        self.client.upsert(self.collection,
                           points=[models.PointStruct(id=pid, vector=[0.0], payload=event)])
        return pid

    def for_customer(self, customer_id: str) -> list[dict]:
        pts, _ = self.client.scroll(self.collection, limit=500,
            scroll_filter=models.Filter(must=[models.FieldCondition(
                key="customer_id", match=models.MatchValue(value=customer_id))]))
        return sorted((p.payload for p in pts), key=lambda e: e.get("ts", 0))
