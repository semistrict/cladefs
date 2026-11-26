# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""libmachine: an embeddable virtual machine monitor."""

from ._libmachine import (
    GATEWAY_ADDR,
    GUEST_ADDR,
    SNAPSHOT_FORMAT_VERSION,
    Agent,
    AsyncProcess,
    AsyncSandbox,
    ConnectionRecord,
    ConnectionRequest,
    EgressPolicy,
    ExecResult,
    Exit,
    Machine,
    GuestBusyError,
    MachineError,
    Network,
    NetworkConfig,
    Process,
    Registers,
    Snapshot,
    StopHandle,
    Vsock,
)

__all__ = [
    "GATEWAY_ADDR",
    "GUEST_ADDR",
    "SNAPSHOT_FORMAT_VERSION",
    "Agent",
    "AsyncProcess",
    "AsyncSandbox",
    "ConnectionRecord",
    "ConnectionRequest",
    "EgressPolicy",
    "ExecResult",
    "Exit",
    "Machine",
    "GuestBusyError",
    "MachineError",
    "Network",
    "NetworkConfig",
    "Process",
    "Registers",
    "Snapshot",
    "StopHandle",
    "Vsock",
]
