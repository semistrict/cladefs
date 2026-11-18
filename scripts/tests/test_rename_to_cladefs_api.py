# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Tests of scripts/rename-to-cladefs-api.sh (TASK-503): what it renames and what it keeps, by its --filter,
which renames text as it renames the checkout's files; and by its --tree, of a tree like the
Firecracker fork's, whose configuration layer's files move and whose Rust rustfmt left as it was
is formatted again."""

import os
import subprocess
import tempfile
import unittest
from pathlib import Path

from script import SCRIPTS

SCRIPT = SCRIPTS / "rename-to-cladefs-api.sh"


def filtered(text):
    return subprocess.run([SCRIPT, "--filter"], input=text, capture_output=True, text=True,
                          check=True).stdout


class Filter(unittest.TestCase):
    def test_the_extent_service_is_cladefsd(self):
        self.assertEqual(
            filtered("cladefs::extentd::serve, cladefs.extentd.v1.Extentd, go/extentd/v1 extentdv1\n"
                     "extentd_socket /run/extentd.sock extentd-stats.txt extentd.log\n"
                     "FcExitCode::ExtentdLost EXTENTD_LOST extentd-fetched-bytes\n"),
            "cladefs::cladefsd::serve, cladefs.cladefsd.v1.Cladefsd, go/cladefsd/v1 cladefsdv1\n"
            "cladefsd_socket /run/cladefsd.sock cladefsd-stats.txt cladefsd.log\n"
            "FcExitCode::CladefsdLost CLADEFSD_LOST cladefsd-fetched-bytes\n",
        )

    def test_thread_names_stay_within_linuxs_15_bytes(self):
        renamed = filtered('"extentd request" "extentd control" "extentd metrics" '
                           '"extentd signals" "extentd client"\n')
        self.assertEqual(renamed, '"cladefsd reqs" "cladefsd ctl" "cladefsd metric" '
                                  '"cladefsd signal" "cladefsd client"\n')

    def test_firecrackers_api_is_cladefs(self):
        self.assertEqual(
            filtered("PUT /extent/{id}, a VM's configuration file's `extent`, extent_overrides\n"
                     "use crate::vmm_config::extent::{ExtentBuilder, ExtentConfigError, "
                     "ExtentDeviceConfig};\n"
                     "VmmAction::InsertExtentDevice(c) => self.insert_extent_device(c), "
                     "VmmActionError::ExtentConfig\n"
                     "ExtentOverride UnknownExtentDevice putGuestExtentByID "
                     "request::extent::parse_put_extent test_parse_put_extent_request\n"
                     "self.vm_resources\n    .extent\n    .insert(cfg)\n"),
            "PUT /cladefs/{id}, a VM's configuration file's `cladefs`, cladefs_overrides\n"
            "use crate::vmm_config::cladefs::{CladefsBuilder, CladefsConfigError, "
            "CladefsDeviceConfig};\n"
            "VmmAction::InsertCladefsDevice(c) => self.insert_cladefs_device(c), "
            "VmmActionError::CladefsConfig\n"
            "CladefsOverride UnknownCladefsDevice putGuestCladefsByID "
            "request::cladefs::parse_put_cladefs test_parse_put_cladefs_request\n"
            "self.vm_resources\n    .cladefs\n    .insert(cfg)\n",
        )

    def test_the_guest_kernels_module_is_cladefs_at_its_interface(self):
        renamed = subprocess.run(
            [SCRIPT, "--filter"], capture_output=True, text=True, check=True,
            env={**os.environ, "GUEST_PARAMS": "dax_delay_us|follow_skip_drop",
                 "GUEST_SYMBOLS": "fxfs_dax_map|fxfs_vfs_fs|FXFS_IOC_IMPORT"},
            input="guest/linux/fs/fxfs/fxfs.h guest/sim/fxfs/fxfs.c guest/rust/fxfs-linux "
                  "libfxfs_linux.a include/linux/fxfs_tls.h 0004-fxfs.patch\n"
                  "fxfs.dax_delay_us=5 /sys/module/fxfs/parameters/follow_skip_drop "
                  "/proc/fs/fxfs/build mount -t fxfs CONFIG_FXFS_FS=y FXFS_IOC_IMPORT\n"
                  "tracepoint:fxfs:fxfs_dax_map events/fxfs fxfs:* fxfs_vfs_fs(dir)\n").stdout
        self.assertEqual(
            renamed,
            "guest/linux/fs/cladefs/cladefs.h guest/sim/cladefs/cladefs.c guest/rust/cladefs-linux "
            "libcladefs_linux.a include/linux/cladefs_tls.h 0004-cladefs.patch\n"
            "cladefs.dax_delay_us=5 /sys/module/cladefs/parameters/follow_skip_drop "
            "/proc/fs/cladefs/build mount -t cladefs CONFIG_CLADEFS_FS=y CLADEFS_IOC_IMPORT\n"
            "tracepoint:cladefs:cladefs_dax_map events/cladefs cladefs:* cladefs_vfs_fs(dir)\n")

    def test_fxfs_itself_keeps_its_name(self):
        kept = ("use fxfs::object_store; fxfs_trace::duration!; crates/fxfs fxfs-guest-dax "
                "test_fxfs_guest_checks fxfs-posix fxfs-sim-diff Fxfs's journal fxfs_dax_map\n")
        self.assertEqual(filtered(kept), kept)

    def test_the_device_and_the_domain_keep_their_names(self):
        kept = ("devices::virtio::extent::Extent, ExtentState, the snapshot's extent_devices, "
                "local_cell::ExtentId, virtio_extent, the extent store device, \"extent\", "
                "<snapshot>.extent-<id>, CLADEFSD_BUCKET\n")
        self.assertEqual(filtered(kept), kept)


class Tree(unittest.TestCase):
    def setUp(self):
        scratch = tempfile.TemporaryDirectory()
        self.addCleanup(scratch.cleanup)
        self.tree = Path(scratch.name)
        subprocess.run(["git", "init", "-q", str(self.tree)], check=True)

    def write(self, path, text):
        file = self.tree / path
        file.parent.mkdir(parents=True, exist_ok=True)
        file.write_text(text)
        subprocess.run(["git", "-C", str(self.tree), "add", path], check=True)

    def rename(self):
        subprocess.run([SCRIPT, "--tree"], cwd=self.tree, check=True, capture_output=True)
        return {str(p.relative_to(self.tree)): p.read_text()
                for p in self.tree.rglob("*") if p.is_file() and ".git" not in p.parts}

    def test_the_configuration_layer_moves_and_its_modules_keep_their_order(self):
        self.write("src/vmm/src/vmm_config/mod.rs",
                   "pub mod boot_source;\n#[cfg(feature = \"cladefs\")]\npub mod extent;\n"
                   "pub mod drive;\n")
        self.write("src/vmm/src/vmm_config/extent.rs",
                   "pub use virtio_extent::Config as ExtentDeviceConfig;\n")
        self.write("src/vmm/src/devices/virtio/mod.rs", "pub mod block;\npub mod extent;\n")
        self.assertEqual(self.rename(), {
            "src/vmm/src/vmm_config/mod.rs":
                "pub mod boot_source;\n#[cfg(feature = \"cladefs\")]\npub mod cladefs;\n"
                "pub mod drive;\n",
            "src/vmm/src/vmm_config/cladefs.rs":
                "pub use virtio_extent::Config as CladefsDeviceConfig;\n",
            "src/vmm/src/devices/virtio/mod.rs": "pub mod block;\npub mod extent;\n",
        })

    def test_the_guest_kernels_c_is_renamed_whole_but_what_fxfs_names(self):
        self.write("guest/linux/fs/fxfs/Makefile",
                   "obj-$(CONFIG_FXFS_FS) += fxfs.o\nfxfs-y := super.o\n"
                   "CFLAGS_super.o += -DFXFS_BUILD_COMMIT='\"$(FXFS_BUILD_COMMIT)\"'\n")
        self.write("crates/fxfs-sim-diff/shim/shim.c",
                   "void fxfs_sim_diff_panic(void); /* fxfs-guest-checks, crates/fxfs */\n"
                   "struct fxfs *fxfs_sim_new(void); int trace_fxfs_dax_map; "
                   "/* the_models_past_drifts_from_fxfs */\n")
        self.assertEqual(self.rename(), {
            "guest/linux/fs/cladefs/Makefile":
                "obj-$(CONFIG_CLADEFS_FS) += cladefs.o\ncladefs-y := super.o\n"
                "CFLAGS_super.o += -DCLADEFS_BUILD_COMMIT='\"$(CLADEFS_BUILD_COMMIT)\"'\n",
            "crates/fxfs-sim-diff/shim/shim.c":
                "void fxfs_sim_diff_panic(void); /* fxfs-guest-checks, crates/fxfs */\n"
                "struct cladefs *cladefs_sim_new(void); int trace_cladefs_dax_map; "
                "/* the_models_past_drifts_from_fxfs */\n",
        })

    def test_a_line_the_rename_takes_past_its_width_is_wrapped_unless_it_was_unformatted(self):
        long = ("    fn insert_extent_device(&mut self, cfg: ExtentDeviceConfig) -> Result<VmmData, "
                "VmmActionError> {\n")
        self.write("formatted.rs", "impl Vmm {\n" + long + "        todo!()\n    }\n}\n")
        self.write("unformatted.rs", "impl  Vmm {\n" + long + "        todo!()\n    }\n}\n")
        renamed = self.rename()
        self.assertEqual(renamed["formatted.rs"],
                         "impl Vmm {\n    fn insert_cladefs_device(\n        &mut self,\n"
                         "        cfg: CladefsDeviceConfig,\n    ) -> Result<VmmData, VmmActionError> {\n"
                         "        todo!()\n    }\n}\n")
        self.assertEqual(renamed["unformatted.rs"],
                         "impl  Vmm {\n    fn insert_cladefs_device(&mut self, cfg: CladefsDeviceConfig) "
                         "-> Result<VmmData, VmmActionError> {\n        todo!()\n    }\n}\n")


if __name__ == "__main__":
    unittest.main()
