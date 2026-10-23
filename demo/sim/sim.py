"""A whole indexer stack in one process, for demos and end-to-end tests of graph-sentinel.

It impersonates graph-node (index-node GraphQL, metrics, IPFS manifests), an Arbitrum RPC
(the REO and escrow view calls, eth_getLogs), the Graph Network subgraph, a TAP aggregator,
and tap-agent (receipts, RAVs and the denylist, in Postgres, in indexer-rs's real tables).

Everything is healthy until you break it:  POST /control/<incident>  (GET /control lists them).
"""

import hashlib
import json
import os
import random
import threading
import time
from datetime import datetime, timedelta, timezone
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlparse

import psycopg

PG = os.environ.get("PG_URL", "postgres://postgres:pw@postgres:5432/indexer")
PORT = int(os.environ.get("PORT", "8080"))
INDEXER = "0x5e1f0e1c0de0000000000000000000000000beef"
GATEWAY = "0x6aa2ace7ec77c06c1dab1b0afac9d56d9c9d0e50"  # configured sender
ROTATED = "0x4b1d0000000000000000000000000000000c0ffe"  # a sender the config does not know
MAX_LOSE_GRT = 2.0
B58 = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz"
TRANSFER = "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef"


def qm(name):
    h = hashlib.sha256(name.encode()).digest()
    return "Qm" + "".join(B58[b % 58] for b in h[:44] + h[:12])[:44]


def addr(name):
    return "0x" + hashlib.sha256(name.encode()).hexdigest()[:40]


# Chains move at demo speed: fast enough to see, slow enough to read.
CHAINS = {
    "arbitrum-one": {"head": 517_500_000, "rate": 4.0, "chain_id": 42161},
    "mainnet": {"head": 23_650_000, "rate": 0.5, "chain_id": 1},
    "base": {"head": 37_100_000, "rate": 2.0, "chain_id": 8453},
}

DEPLOYMENTS = []
for i, (name, chain, active) in enumerate([
    ("uniswap-v3-arbitrum", "arbitrum-one", True),
    ("graph-network-arbitrum", "arbitrum-one", True),
    ("aave-v3-mainnet", "mainnet", True),
    ("ens-mainnet", "mainnet", True),
    ("aerodrome-base", "base", True),
    ("quiet-dao-base", "base", False),  # a contract with no events: idle, not frozen
]):
    DEPLOYMENTS.append({
        "id": qm(name), "name": name, "network": chain, "active": active,
        "contract": addr(name), "lag": random.randint(0, 3),
        "latest": CHAINS[chain]["head"] - 3, "frozen": False, "failed": None,
        "allocation": addr("alloc-" + name),
    })
# More deployments served for REO, without liveness rows of their own.
EXTRA = [{"id": qm(f"reo-only-{i}"), "network": "arbitrum-one", "allocation": addr(f"alloc-reo-{i}")} for i in range(8)]

STATE = {
    "graph_node_up": True,
    "aggregator_up": True,
    "rav_stalled": False,
    "rotated_sender": False,
    "chain_stalled": None,
    "rpc_burn": False,
    "escrow_grt": 9_500.0,
    "disputes": [],
    "poi_due": False,
    "eligible": True,
}
METRICS = {"mainnet-alchemy|eth_getBlockByNumber": 0, "mainnet-alchemy|eth_getBlockReceipts": 0,
           "arbitrum-one-alchemy|eth_getLogs": 0, "base-public|eth_getBlockByNumber": 0}
LOCK = threading.Lock()
STARTED = time.time()


def now_ns():
    return int(time.time() * 1e9)


# --- the world ticking -------------------------------------------------------------------

def advance():
    last = time.time()
    while True:
        time.sleep(1)
        t = time.time()
        dt = t - last
        last = t
        with LOCK:
            for name, c in CHAINS.items():
                if STATE["chain_stalled"] != name:
                    c["head"] += c["rate"] * dt
            for d in DEPLOYMENTS:
                if not d["frozen"] and not d["failed"]:
                    d["latest"] = int(CHAINS[d["network"]]["head"]) - d["lag"]
            if STATE["graph_node_up"]:
                METRICS["mainnet-alchemy|eth_getBlockByNumber"] += 3
                METRICS["mainnet-alchemy|eth_getBlockReceipts"] += 1
                METRICS["arbitrum-one-alchemy|eth_getLogs"] += 1
                METRICS["base-public|eth_getBlockByNumber"] += 2
                if STATE["rpc_burn"]:
                    METRICS["arbitrum-one-alchemy|eth_getLogs"] += 900


def tap_agent():
    """Receipts every second; a RAV every 20 s when the aggregator answers."""
    with psycopg.connect(PG, autocommit=True) as db:
        last_rav = time.time()
        while True:
            time.sleep(1)
            ts = now_ns()
            served = DEPLOYMENTS[:5] + EXTRA
            for d in random.sample(served, 6):
                payer = GATEWAY
                insert_receipt(db, d["allocation"], payer, ts, 0.002)
            if STATE["rotated_sender"]:
                insert_receipt(db, DEPLOYMENTS[0]["allocation"], ROTATED, ts, 0.002)
            if time.time() - last_rav < 20:
                continue
            last_rav = time.time()
            if STATE["aggregator_up"] and not STATE["rav_stalled"]:
                aggregate(db, GATEWAY)
            # tap-agent's own safety valve: past the limit it denylists the sender.
            row = db.execute("SELECT COALESCE(SUM(value), 0)::text FROM tap_horizon_receipts WHERE payer = %s",
                             (GATEWAY[2:],)).fetchone()
            if float(row[0]) / 1e18 >= MAX_LOSE_GRT:
                db.execute("INSERT INTO tap_horizon_denylist (sender_address) VALUES (%s) ON CONFLICT DO NOTHING",
                           (GATEWAY[2:],))


def insert_receipt(db, allocation, payer, ts, grt):
    db.execute(
        "INSERT INTO tap_horizon_receipts (signer_address, signature, collection_id, payer, data_service,"
        " service_provider, timestamp_ns, nonce, value) VALUES (%s, '\\x00', %s, %s, %s, %s, %s, %s, %s)",
        (payer[2:], "0" * 24 + allocation[2:], payer[2:], "b2bb92d0de618878e438b55d5846cfecd9301105",
         INDEXER[2:], ts, random.randint(0, 2**60), int(grt * 1e18)))


def aggregate(db, payer):
    rows = db.execute(
        "SELECT collection_id, SUM(value)::text, MAX(timestamp_ns)::bigint FROM tap_horizon_receipts"
        " WHERE payer = %s GROUP BY collection_id", (payer[2:],)).fetchall()
    for coll, value, ts in rows:
        db.execute(
            "INSERT INTO tap_horizon_ravs (signature, collection_id, payer, data_service, service_provider,"
            " timestamp_ns, value_aggregate, metadata, created_at, updated_at)"
            " VALUES ('\\x00', %s, %s, %s, %s, %s, %s, '\\x00', now(), now())"
            " ON CONFLICT (payer, data_service, service_provider, collection_id) DO UPDATE SET"
            " value_aggregate = tap_horizon_ravs.value_aggregate + EXCLUDED.value_aggregate,"
            " timestamp_ns = EXCLUDED.timestamp_ns, updated_at = now()",
            (coll, payer[2:], "b2bb92d0de618878e438b55d5846cfecd9301105", INDEXER[2:], ts, value))
        db.execute("DELETE FROM tap_horizon_receipts WHERE payer = %s AND collection_id = %s AND timestamp_ns <= %s",
                   (payer[2:], coll, ts))


HISTORY = []  # (allocation, deployment) pairs that only exist in the past


def seed_history(db):
    """28 days of RAVs, one collection per served deployment per day, so the REO chart has a past."""
    random.seed(7)  # the same history on every run, so a restart rebuilds the same mapping
    fresh = db.execute("SELECT COUNT(*) FROM tap_horizon_ravs").fetchone()[0] == 0
    today = datetime.now(timezone.utc).replace(hour=12, minute=0, second=0, microsecond=0)
    pool = DEPLOYMENTS[:5] + EXTRA
    for back in range(1, 29):
        day = today - timedelta(days=back)
        n = 6 if back in (9, 10, 17) else random.randint(11, 13)
        for d in random.sample(pool, min(n, len(pool))):
            alloc = addr(f"hist-{back}-{d['id']}")
            HISTORY.append((alloc, d["id"]))
            if not fresh:
                continue
            db.execute(
                "INSERT INTO tap_horizon_ravs (signature, collection_id, payer, data_service, service_provider,"
                " timestamp_ns, value_aggregate, metadata) VALUES ('\\x00', %s, %s, %s, %s, %s, %s, '\\x00')",
                ("0" * 24 + alloc[2:], GATEWAY[2:], "b2bb92d0de618878e438b55d5846cfecd9301105", INDEXER[2:],
                 int(day.timestamp() * 1e9), int(0.8e18)))
    random.seed()


# --- the HTTP faces ----------------------------------------------------------------------

def manifest(d):
    return f"""specVersion: 0.0.5
dataSources:
  - kind: ethereum/contract
    name: Main
    network: {d['network']}
    source:
      address: "{d['contract']}"
      abi: Main
      startBlock: 1
    mapping:
      kind: ethereum/events
      apiVersion: 0.0.7
      language: wasm/assemblyscript
      entities: [Transfer]
      abis: [{{ name: Main, file: {{ /: /ipfs/QmAbi }} }}]
      eventHandlers:
        - event: Transfer(indexed address,indexed address,uint256)
          handler: handleTransfer
      file: {{ /: /ipfs/QmMapping }}
"""


def index_node():
    out = []
    with LOCK:
        for d in DEPLOYMENTS:
            c = CHAINS[d["network"]]
            out.append({
                "subgraph": d["id"], "synced": True, "health": "failed" if d["failed"] else "healthy",
                "paused": False, "node": "default",
                "fatalError": {"message": d["failed"]} if d["failed"] else None,
                "chains": [{"network": d["network"], "latestBlock": {"number": str(d["latest"])},
                            "chainHeadBlock": {"number": str(int(c["head"]))}}],
            })
    return {"data": {"indexingStatuses": out}}


def word(v):
    return "0x" + format(int(v), "064x")


def rpc(chain, req):
    m, p = req.get("method"), req.get("params", [])
    c = CHAINS.get(chain, CHAINS["arbitrum-one"])
    if m == "eth_blockNumber":
        r = hex(int(c["head"]))
    elif m == "eth_chainId":
        r = hex(c["chain_id"])
    elif m == "eth_getLogs":
        f = p[0]
        addrs = [a.lower() for a in (f.get("address") or [])]
        d = next((x for x in DEPLOYMENTS if x["contract"] in addrs), None)
        frm, to = int(f["fromBlock"], 16), int(f["toBlock"], 16)
        n = 0 if d is None or not d["active"] else max(1, (to - frm) // 3)
        r = [{"address": d["contract"] if d else None, "topics": [TRANSFER], "blockNumber": hex(frm)} for _ in range(min(n, 50))]
    elif m == "eth_call":
        data = p[0]["data"]
        today = datetime.now(timezone.utc).replace(hour=10, minute=0, second=0, microsecond=0)
        sel = data[2:10]
        r = {
            "66e305fd": word(1 if STATE["eligible"] else 0),
            "d353402d": word(today.timestamp() if STATE["eligible"] else 0),
            "d0a5379e": word(14 * 86400),
            "be626dd2": word(today.timestamp()),
            "d6bd603c": word(STATE["escrow_grt"] * 1e18),
        }.get(sel, word(0))
    else:
        return {"jsonrpc": "2.0", "id": req.get("id"), "error": {"code": -32601, "message": f"{m} not simulated"}}
    return {"jsonrpc": "2.0", "id": req.get("id"), "result": r}


def network_subgraph(q, variables):
    now = int(time.time())
    if "_meta" in q:
        return {"_meta": {"block": {"number": int(CHAINS["arbitrum-one"]["head"])}}}
    if "disputes" in q:
        return {"disputes": STATE["disputes"]}
    if "indexer(id" in q:
        return {"indexer": {
            "stakedTokens": str(int(250_000e18)), "provisionedTokens": str(int(250_000e18)),
            "allocatedTokens": str(int(612_000e18)), "thawingTokens": "0",
            "delegatedTokens": str(int(1_480_000e18)), "delegatedThawingTokens": str(int(52_000e18)),
            "rewardsEarned": str(int(318_400e18)), "queryFeesCollected": str(int(2_930e18)),
            "allocationCount": len(DEPLOYMENTS) - 1 + len(EXTRA), "provisions": [{"thawingUntil": "0"}],
        }}
    if "subgraphDeployments" in q:
        if variables.get("skip", 0) > 0:
            return {"subgraphDeployments": []}
        return {"subgraphDeployments": [
            {"ipfsHash": qm(f"candidate-{i}"), "signalledTokens": str(int((9_000 - i * 700) * 1e18)),
             "queryFeesAmount": str(int((40_000 - i * 3_000) * 1e18)), "deniedAt": 0,
             "manifest": {"network": ["arbitrum-one", "base", "mainnet", "gnosis"][i % 4]},
             "indexerAllocations": [{"id": "x"}] * (i % 4)} for i in range(10)]}
    if "allocations" in q:
        if variables.get("after"):
            return {"allocations": []}
        if "status: Active" in q:
            out = []
            for i, d in enumerate(DEPLOYMENTS[:5] + EXTRA):
                last = now - (26 * 86400 if STATE["poi_due"] and i == 2 else (2 + i) * 86400)
                out.append({
                    "id": d["allocation"], "allocatedTokens": str(int((40_000 + i * 9_000) * 1e18)),
                    "createdAt": now - 40 * 86400, "latestPoiPresentedAt": last, "poiCount": "6",
                    "indexingRewards": str(int((1_200 + i * 310) * 1e18)),
                    "subgraphDeployment": {"ipfsHash": d["id"], "signalledTokens": str(int(5_000e18)),
                                           "manifest": {"network": d["network"]}},
                })
            return {"allocations": out}
        pairs = [(d["allocation"], d["id"]) for d in DEPLOYMENTS[:5] + EXTRA] + HISTORY
        return {"allocations": [{"id": a, "subgraphDeployment": {"ipfsHash": h}} for a, h in pairs]}
    return {}


INCIDENTS = {
    "freeze": "freeze the Uniswap deployment while graph-node keeps calling it healthy",
    "unfreeze": "let it move again",
    "fail": "fail the ENS deployment with a fatal error",
    "unfail": "clear the failure",
    "chain-stall": "stop the base chain head",
    "chain-resume": "start it again",
    "aggregator-down": "the aggregator serves a Cloudflare 526 page instead of gRPC",
    "aggregator-up": "the aggregator recovers",
    "rotate-sender": "receipts start arriving from a sender the config does not know",
    "rotate-done": "stop them",
    "rav-stall": "tap-agent stops requesting RAVs while receipts keep coming",
    "rav-resume": "RAVs resume",
    "clear-denylist": "remove the gateway from the denylist",
    "rpc-burn": "something starts hammering eth_getLogs on Alchemy",
    "rpc-calm": "stop it",
    "graph-node-stop": "graph-node goes away (the spend guard's stop command)",
    "graph-node-start": "graph-node comes back",
    "escrow-low": "the gateway's escrow for you drops to 2 GRT",
    "escrow-ok": "escrow refilled",
    "dispute": "a fisherman files an indexing dispute against you",
    "dispute-clear": "the arbitrator rejects it",
    "poi-due": "one allocation's last POI is 26 days old",
    "poi-ok": "a fresh POI is presented",
    "ineligible": "the REO oracle marks you ineligible",
    "eligible": "eligible again",
}


def incident(name):
    with LOCK:
        d0, d3 = DEPLOYMENTS[0], DEPLOYMENTS[3]
        match name:
            case "freeze": d0["frozen"] = True
            case "unfreeze": d0["frozen"] = False
            case "fail": d3["failed"] = "Mapping aborted at handleTransfer: value is null, wasm backtrace: 0: 0x2a1f"
            case "unfail": d3["failed"] = None
            case "chain-stall": STATE["chain_stalled"] = "base"
            case "chain-resume": STATE["chain_stalled"] = None
            case "aggregator-down": STATE["aggregator_up"] = False
            case "aggregator-up": STATE["aggregator_up"] = True
            case "rotate-sender": STATE["rotated_sender"] = True
            case "rotate-done": STATE["rotated_sender"] = False
            case "rav-stall": STATE["rav_stalled"] = True
            case "rav-resume": STATE["rav_stalled"] = False
            case "rpc-burn": STATE["rpc_burn"] = True
            case "rpc-calm": STATE["rpc_burn"] = False
            case "graph-node-stop": STATE["graph_node_up"] = False
            case "graph-node-start": STATE["graph_node_up"] = True
            case "escrow-low": STATE["escrow_grt"] = 2.0
            case "escrow-ok": STATE["escrow_grt"] = 9_500.0
            case "dispute":
                STATE["disputes"] = [{
                    "id": "0x" + hashlib.sha256(b"demo-dispute").hexdigest(), "type": "Indexing",
                    "status": "Undecided", "createdAt": int(time.time()), "closedAt": 0,
                    "cancellableAt": str(int(time.time()) + 28 * 86400), "deposit": str(int(10_000e18)),
                    "tokensSlashed": "0", "indexer": {"id": INDEXER},
                    "fisherman": {"id": "0x083a086ca2ccec6526cbe97b2841a481f99834af"},
                    "subgraphDeployment": {"ipfsHash": d0["id"]},
                }]
            case "dispute-clear":
                for x in STATE["disputes"]:
                    x["status"], x["closedAt"] = "Rejected", int(time.time())
            case "poi-due": STATE["poi_due"] = True
            case "poi-ok": STATE["poi_due"] = False
            case "ineligible": STATE["eligible"] = False
            case "eligible": STATE["eligible"] = True
            case _: return False
    if name == "clear-denylist":
        with psycopg.connect(PG, autocommit=True) as db:
            db.execute("DELETE FROM tap_horizon_denylist")
    return True


class H(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def send(self, code, body, ctype="application/json", extra=()):
        data = body if isinstance(body, bytes) else (body if isinstance(body, str) else json.dumps(body)).encode()
        self.send_response(code)
        self.send_header("content-type", ctype)
        self.send_header("content-length", str(len(data)))
        for k, v in extra:
            self.send_header(k, v)
        self.end_headers()
        self.wfile.write(data)

    def body(self):
        n = int(self.headers.get("content-length") or 0)
        raw = self.rfile.read(n) if n else b""
        try:
            return json.loads(raw) if raw else {}
        except ValueError:
            return {}

    def do_GET(self):
        u = urlparse(self.path)
        if u.path == "/control":
            return self.send(200, {"incidents": INCIDENTS, "state": {k: v for k, v in STATE.items() if k != "disputes"}})
        if u.path == "/metrics":
            if not STATE["graph_node_up"]:
                return self.send(503, "graph-node stopped", "text/plain")
            lines = ["# TYPE endpoint_request counter"]
            for k, v in METRICS.items():
                prov, meth = k.split("|")
                lines.append(f'endpoint_request{{conn_type="rpc",provider="{prov}",req_type="{meth}",result="success"}} {v}')
            return self.send(200, "\n".join(lines) + "\n", "text/plain")
        self.send(404, {"error": "not here"})

    def do_POST(self):
        u = urlparse(self.path)
        # Read the body whatever the route: an unread body on a keep-alive connection is
        # parsed as the next request.
        self._body = self.body()
        if u.path.startswith("/control/"):
            ok = incident(u.path.split("/")[-1])
            return self.send(200 if ok else 404, {"ok": ok})
        if u.path == "/graphql":
            if not STATE["graph_node_up"]:
                return self.send(503, "graph-node stopped", "text/plain")
            return self.send(200, index_node())
        if u.path == "/network":
            b = self._body
            return self.send(200, {"data": network_subgraph(b.get("query", ""), b.get("variables") or {})})
        if u.path.startswith("/rpc/"):
            return self.send(200, rpc(u.path.split("/")[-1], self._body))
        if u.path == "/api/v0/cat":
            h = parse_qs(u.query).get("arg", [""])[0]
            d = next((x for x in DEPLOYMENTS if x["id"] == h), None)
            return self.send(200, manifest(d), "text/plain") if d else self.send(404, "no such manifest", "text/plain")
        if u.path.startswith("/aggregator/"):
            if STATE["aggregator_up"]:
                return self.send(200, b"", "application/grpc", [("grpc-status", "12")])
            return self.send(526, "<html><head><title>526 Invalid SSL certificate</title></head><body>cloudflare</body></html>", "text/html")
        self.send(404, {"error": "not here"})

    def log_message(self, *a):
        pass


if __name__ == "__main__":
    for _ in range(60):
        try:
            psycopg.connect(PG).close()
            break
        except psycopg.OperationalError:
            time.sleep(1)
    # History first, so a sentinel starting after our health check sees all 28 days.
    with psycopg.connect(PG, autocommit=True) as db:
        seed_history(db)
    threading.Thread(target=advance, daemon=True).start()
    threading.Thread(target=tap_agent, daemon=True).start()
    print(f"simulator on :{PORT}; incidents: GET /control", flush=True)
    ThreadingHTTPServer(("0.0.0.0", PORT), H).serve_forever()
