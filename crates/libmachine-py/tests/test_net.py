# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""The guest's network from Python: the egress policy and network
configuration API, and guests connecting to listeners on the host's
loopback through their gateway, with a decider, host entries and the
connection log."""

import os
import socket
import threading
import time
from pathlib import Path

import pytest

from libmachine import GATEWAY_ADDR, GUEST_ADDR, ConnectionRequest, EgressPolicy, Machine, NetworkConfig

GUEST = Path(os.environ.get("LIBMACHINE_GUEST_DIR", Path(__file__).parents[3] / "target" / "guest"))


def test_addresses():
    assert GUEST_ADDR == "10.0.2.2"
    assert GATEWAY_ADDR == "10.0.2.1"


def test_default_policy_denies_everything():
    p = EgressPolicy()
    assert p.rules == []
    assert not p.allows("1.1.1.1", 443)
    assert repr(p) == "EgressPolicy(deny *)"


def test_rules_in_order_first_match_wins():
    p = EgressPolicy().deny("1.1.1.1", 22).allow("*").allow("127.0.0.1", (8000, 8080))
    assert p.rules == [
        ("deny", "1.1.1.1", (22, 22)),
        ("allow", "*", (0, 65535)),
        ("allow", "127.0.0.1", (8000, 8080)),
    ]
    assert not p.allows("1.1.1.1", 22)
    assert p.allows("1.1.1.1", 443)
    assert p.allows("127.0.0.1", 8080)
    assert not p.allows("127.0.0.1", 8081)
    # "*" is the internet: never loopback, private or link-local addresses.
    for addr in ["127.0.0.2", "10.0.0.1", "192.168.1.1", "169.254.169.254", "::1"]:
        assert not p.allows(addr, 443), addr


def test_names_and_networks():
    p = EgressPolicy().allow("*.pypi.org", 443).allow("10.0.0.0/8", "22").allow("example.com", "8000-8001")
    assert p.allows("151.101.0.223", 443, ["files.pypi.org"])
    assert not p.allows("151.101.0.223", 443, ["pypi.org"])
    assert not p.allows("151.101.0.223", 443)
    assert not p.allows("10.1.1.1", 443, ["files.pypi.org"]), "names never reach private addresses"
    assert p.allows("10.200.0.1", 22)
    assert p.allows("93.184.216.34", 8001, ("example.com",))
    assert EgressPolicy.allow_all().allows("8.8.8.8", 53)
    assert p == EgressPolicy().allow("*.pypi.org", 443).allow("10.0.0.0/8", 22).allow("example.com", (8000, 8001))


@pytest.mark.parametrize(
    "host,ports",
    [("1.2.3.4/33", None), ("bad name", None), ("", None), ("*", 70000), ("*", (10, 1)), ("*", "x"), ("*", 1.5)],
)
def test_bad_rules_are_refused(host, ports):
    with pytest.raises(ValueError):
        EgressPolicy().allow(host, ports)


def test_ask_rules_and_host_names():
    p = EgressPolicy().allow("mock.test", 8080).ask("0.0.0.0/0", (1, 1024))
    assert p.rules == [("allow", "mock.test", (8080, 8080)), ("ask", "0.0.0.0/0", (1, 1024))]
    assert p.check("127.0.0.1", 80) == "ask"
    assert p.check("127.0.0.1", 8080) == "deny"
    assert not p.allows("127.0.0.1", 80)
    # A host entry's name reaches its loopback address; a learnt name does not.
    assert p.allows("127.0.0.1", 8080, host_names=["mock.test"])
    assert not p.allows("127.0.0.1", 8080, names=["mock.test"])
    assert repr(p) == "EgressPolicy(allow mock.test:8080, ask 0.0.0.0/0:1-1024, deny *)"


def test_network_config():
    c = NetworkConfig(
        EgressPolicy().allow("mock.test", 80),
        hosts={"Mock.Test": "127.0.0.1", "db.test": ["192.168.1.2", "192.168.1.3"]},
        dns_ttl=30,
        log_capacity=10,
        decider=lambda req: True,
    )
    assert c.policy == EgressPolicy().allow("mock.test", 80)
    assert c.hosts == {"mock.test": ["127.0.0.1"], "db.test": ["192.168.1.2", "192.168.1.3"]}
    assert (c.dns_ttl, c.log_capacity) == (30, 10)
    assert NetworkConfig().policy == EgressPolicy()
    for bad in [
        dict(hosts={"x.test": "127.0.0.2"}),
        dict(hosts={"x.test": "10.0.2.2"}),
        dict(hosts={"x.test": "::1"}),
        dict(hosts={"bad name": "1.2.3.4"}),
        dict(dns_ttl=0),
        dict(dns_ttl=86401),
        dict(decider=print, decider_default="maybe"),
        dict(decider=print, decider_timeout=0),
    ]:
        with pytest.raises(ValueError):
            NetworkConfig(**bad)
    with pytest.raises(TypeError):
        NetworkConfig(hosts={"x.test": 1})
    with pytest.raises(TypeError):
        Machine.boot_linux("/nonexistent", network="allow all")


def test_bare_machines_have_no_network():
    with pytest.raises(ValueError):
        Machine().network()


@pytest.mark.skipif(not os.access("/dev/kvm", os.R_OK | os.W_OK), reason="/dev/kvm is not accessible")
@pytest.mark.skipif(
    not (GUEST / "vmlinux").is_file(), reason=f"no test guest in {GUEST} (run crates/libmachine/scripts/build-test-guest.sh)"
)
def test_guest_reaches_only_allowed_listeners():
    listener = socket.create_server(("127.0.0.1", 0))
    port = listener.getsockname()[1]
    m = Machine.boot_linux(
        GUEST / "vmlinux",
        GUEST / "initramfs.cpio",
        "quiet libmachine_action=agent",
        mem_size=256 << 20,
        network=EgressPolicy(),
    )
    agent, net, stop = m.agent(), m.network(), m.stop_handle()
    exits = []
    runner = threading.Thread(target=lambda: exits.append(m.run(timeout=300)))
    runner.start()
    try:
        agent.wait_ready(60)
        denied = agent.exec(["nc", GATEWAY_ADDR, str(port)], stdin=b"")
        assert denied.exit_code != 0
        assert b"refused" in denied.stderr

        net.set_policy(EgressPolicy().allow("127.0.0.1", port))
        assert net.policy.rules == [("allow", "127.0.0.1", (port, port))]

        def serve():
            conn, _ = listener.accept()
            with conn:
                data = b""
                while not data.endswith(b"\n"):
                    data += conn.recv(64)
                conn.sendall(data.upper())

        server = threading.Thread(target=serve)
        server.start()
        out = agent.exec(["nc", GATEWAY_ADDR, str(port)], stdin=b"hello\n")
        server.join()
        assert out.stdout == b"HELLO\n", out
    finally:
        stop.stop()
        runner.join()
        listener.close()
    assert exits[0].kind == "stopped", m.serial_output().decode(errors="replace")


def _serve_once(listener, reply):
    def serve():
        conn, _ = listener.accept()
        with conn:
            data = b""
            while not data.endswith(b"\n"):
                chunk = conn.recv(64)
                if not chunk:
                    break
                data += chunk
            conn.sendall(reply)

    t = threading.Thread(target=serve)
    t.start()
    return t


def _log(net, n):
    """The log's records, waiting until there are `n` (a connection is
    logged when the stack sees its end, just after the guest command)."""
    log, deadline = [], time.monotonic() + 10
    while len(log) < n and time.monotonic() < deadline:
        log += net.take_log()
        time.sleep(0.01)
    assert len(log) == n, log
    return log


@pytest.mark.skipif(not os.access("/dev/kvm", os.R_OK | os.W_OK), reason="/dev/kvm is not accessible")
@pytest.mark.skipif(
    not (GUEST / "vmlinux").is_file(), reason=f"no test guest in {GUEST} (run crates/libmachine/scripts/build-test-guest.sh)"
)
# The failing decider's exception is reported as unraisable, as documented.
@pytest.mark.filterwarnings("ignore::pytest.PytestUnraisableExceptionWarning")
def test_decider_host_entries_and_log():
    allowed, failing, mock = (socket.create_server(("127.0.0.1", 0)) for _ in range(3))
    allowed_port, failing_port, mock_port = (s.getsockname()[1] for s in (allowed, failing, mock))
    asked = []

    def decide(req):
        assert isinstance(req, ConnectionRequest)
        asked.append((req.addr, req.port, req.rule))
        if req.port == failing_port:
            raise RuntimeError("a failing decider (this error is expected)")
        return req.port == allowed_port

    config = NetworkConfig(
        EgressPolicy().allow("mock.service", mock_port).ask("127.0.0.1"),
        hosts={"mock.service": "127.0.0.1"},
        decider=decide,
        decider_timeout=5.0,
    )
    m = Machine.boot_linux(
        GUEST / "vmlinux",
        GUEST / "initramfs.cpio",
        "quiet libmachine_action=agent",
        mem_size=256 << 20,
        network=config,
    )
    agent, net, stop = m.agent(), m.network(), m.stop_handle()
    assert net.hosts == {"mock.service": ["127.0.0.1"]}
    exits = []
    runner = threading.Thread(target=lambda: exits.append(m.run(timeout=300)))
    runner.start()
    try:
        agent.wait_ready(60)
        server = _serve_once(allowed, b"ALLOWED\n")
        out = agent.exec(["nc", GATEWAY_ADDR, str(allowed_port)], stdin=b"hi\n")
        server.join()
        assert out.stdout == b"ALLOWED\n", out

        failed = agent.exec(["nc", GATEWAY_ADDR, str(failing_port)], stdin=b"")
        assert failed.exit_code != 0 and b"refused" in failed.stderr

        # The host entry: answered by the VMM, reached through a name rule.
        lookup = agent.exec(["nslookup", "-type=a", "mock.service"])
        assert b"Address: 10.0.2.1" in lookup.stdout, lookup
        server = _serve_once(mock, b"MOCK\n")
        out = agent.exec(["nc", GATEWAY_ADDR, str(mock_port)], stdin=b"x\n")
        server.join()
        assert out.stdout == b"MOCK\n", out

        assert asked == [("127.0.0.1", allowed_port, 1), ("127.0.0.1", failing_port, 1)]
        log = {r.port: r for r in _log(net, 3)}
        a, f, k = log[allowed_port], log[failing_port], log[mock_port]
        assert (a.verdict, a.decided_by, a.rule, a.connected) == ("allow", "callback", None, True)
        assert (a.bytes_to_host, a.bytes_to_guest) == (3, 8)
        assert a.duration > 0 and a.started > time.time() - 300
        assert (f.verdict, f.decided_by, f.connected) == ("deny", "callback failed", False)
        assert (k.verdict, k.rule, k.names) == ("allow", 0, ["mock.service"])
        assert k.decided_by == f"rule 0 (allow mock.service:{mock_port})"
        stats = net.stats()
        assert stats["decisions"] == 2 and stats["decisions_failed"] == 1
        assert stats["connections_allowed"] == 2 and stats["connections_denied"] == 1
        assert stats["dns_queries"] >= 1

        # Without a decider, ask rules deny.
        net.set_decider(None)
        out = agent.exec(["nc", GATEWAY_ADDR, str(allowed_port)], stdin=b"")
        assert out.exit_code != 0
        assert _log(net, 1)[0].decided_by == "no callback"
    finally:
        stop.stop()
        runner.join()
        for s in (allowed, failing, mock):
            s.close()
    assert exits[0].kind == "stopped", m.serial_output().decode(errors="replace")
