# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Bzlmod repository extension providing Cargo manifests for crate_universe.

1. `workspace`: Mirrors the main cladefs Cargo workspace manifests with `madsim-tokio`
   un-aliased (working around rules_rust 0.75.0 duplicate hub alias collision with direct
   `tokio = "=1.53.1"`), keeps `Cargo.lock` 100% locked and untouched, materializes
   optional/cfg(madsim)/seccompiler deps in `@crates`, and mirrors out-of-workspace path
   dependencies inside the external repo so `cargo-bazel` never writes BUILD files into
   `vendor/firecracker` or `guest/rust`.

2. `firecracker`: Mirrors `vendor/firecracker`'s Cargo workspace inside an external repo
   so `@firecracker_crates` can resolve `vendor/firecracker/Cargo.lock` without writing
   a single file inside the `vendor/firecracker` git submodule.
"""

def _rewrite_root_manifest(text):
    return text.replace(
        'tokio = { package = "madsim-tokio",',
        'madsim-tokio = {',
    )

def _rewrite_member_manifest(text, crate_name):
    text = text.replace("tokio.workspace = true", "madsim-tokio.workspace = true")
    text = text.replace("tokio = { workspace = true", "madsim-tokio = { workspace = true")
    if crate_name == "local_cell":
        text = text.replace(
            'fail = { version = "0.5.1", features = ["failpoints"] }',
            'fail = "0.5.1"',
        )
        text = text.replace("[dev-dependencies]\n", "[dev-dependencies]\nshuttle.workspace = true\n", 1)
    elif crate_name == "virtio_extent":
        text = text.replace("[dev-dependencies]\n", "[dev-dependencies]\nloom.workspace = true\n", 1)
    elif crate_name == "bucket_cell":
        # bucket_cell's `tokio = { workspace = true, optional = true }` is rewritten above
        # to `madsim-tokio`, so its feature entry `"dep:tokio"` must follow.
        text = text.replace('"dep:tokio"', '"dep:madsim-tokio"')
    elif crate_name == "fuchsia-async":
        # target_os = "none" (guest kernel) is built by //guest/rust, never by @crates;
        # stripping the out-of-workspace path dep keeps @crates 100% hermetic and prevents
        # cargo-bazel from overwriting guest/rust/linux-rt/BUILD.bazel.
        text = text.replace(
            'linux-rt = { path = "../../guest/rust/linux-rt" }\n',
            "",
        )
    elif crate_name == "node_local":
        # Materialize locked deps of fail, madsim, madsim-tokio, madsim-tonic, and serde_core
        # in @crates so //tools/bazel:{fail,madsim,madsim_tokio,madsim_tonic}_lib and
        # @firecracker_crates' serde unification can reference them without touching Cargo.toml.
        text = text + """
[dev-dependencies]
ahash = "0.8"
async-channel = "2"
async-stream = "0.3"
chrono = { version = "0.4", default-features = false, features = ["clock"] }
downcast-rs = "1.2"
errno = "0.3"
futures-util = "0.3.34"
lazy_static = "1.5.1"
madsim-macros = "0.2.12"
naive-timer = "0.2"
once_cell = "1.21.4"
panic-message = "0.3"
rand08 = { package = "rand", version = "0.8.8" }
rand_xoshiro = "0.6"
real-tonic = { package = "tonic", version = "0.14.6", default-features = false, features = ["tls-aws-lc"] }
rustversion = "1"
serde = { version = "1.0.229", features = ["rc"] }
serde_core = "=1.0.229"
spin = "0.9.8"
tokio-util = { version = "0.7.20", features = ["codec"] }
toml = "0.9"
tracing = "0.1.44"
tracing-subscriber = "0.3.23"
"""
    elif crate_name in ("cladefs_core", "cladefs_fs", "cladefs_wire", "cladefsd"):
        text = text.replace('features = ["failpoints", "testing", "tests-kms"]', 'features = ["testing", "tests-kms"]')
    elif crate_name == "cladefs_fuse":
        # Its own failpoints too, its tests' dependency on itself turning them on.
        text = text.replace('cladefs_fuse = { path = ".", features = ["failpoints"] }\n', "")
        text = text.replace('features = ["failpoints", "testing", "tests-kms"]', 'features = ["testing", "tests-kms"]')
    elif crate_name == "cladefs":
        # Keep fail/failpoints out of the unified @crates//:fail target (controlled by //tools/bazel:fail_lib
        # so release/musl builds satisfy TASK-320's !fail::has_failpoints() assertion).
        text = text.replace(
            'cladefs = { path = ".", features = ["failpoints", "testing", "tests-kms"] }',
            'cladefs = { path = ".", features = ["testing", "tests-kms"] }',
        )
        text = text.replace('features = ["failpoints", "testing", "tests-kms"]', 'features = ["testing", "tests-kms"]')
        # Replace the out-of-workspace seccompiler path dev-dep with its locked crate deps
        # so //vendor:seccompiler links them natively and cargo-bazel never touches vendor/firecracker.
        text = text.replace(
            'seccompiler = { path = "../../vendor/firecracker/src/seccompiler" }\n',
            "",
        )
        text = text.replace(
            "[dev-dependencies]\n",
            '[dev-dependencies]\nbitcode = { version = "0.6.9", features = ["serde"] }\nclap = { version = "4.6.1", features = ["derive", "string"] }\ndisplaydoc = "0.2.6"\n',
            1,
        )
    return text

def _cargo_manifests_repo_impl(rctx):
    root_toml = rctx.path(rctx.attr.root_manifest)
    lock_file = rctx.path(rctx.attr.cargo_lockfile)

    # Mirror root Cargo.toml with madsim-tokio un-aliased, and mirror Cargo.lock into this
    # external repo so crate_universe never overwrites the workspace's Cargo.lock in place.
    rctx.file("Cargo.toml", _rewrite_root_manifest(rctx.read(root_toml)))
    rctx.file("Cargo.lock", rctx.read(lock_file))

    # Mirror each workspace crate directory, rewriting only Cargo.toml.
    member_labels = ['"Cargo.toml"', '"Cargo.lock"']
    for manifest_label in rctx.attr.member_manifests:
        src_toml = rctx.path(manifest_label)
        crate_dir = src_toml.dirname
        crate_name = crate_dir.basename
        rel_dir = "crates/" + crate_name
        rctx.file(rel_dir + "/Cargo.toml", _rewrite_member_manifest(rctx.read(src_toml), crate_name))
        for entry in crate_dir.readdir():
            if entry.basename == "Cargo.toml":
                continue
            rctx.symlink(entry, rel_dir + "/" + entry.basename)
        member_labels.append('"{}/Cargo.toml"'.format(rel_dir))

    rctx.file(
        "BUILD.bazel",
        'exports_files([{}], visibility = ["//visibility:public"])\n'.format(
            ", ".join(member_labels),
        ),
    )

_cargo_manifests_repo = repository_rule(
    implementation = _cargo_manifests_repo_impl,
    attrs = {
        "cargo_lockfile": attr.label(mandatory = True, allow_single_file = True),
        "root_manifest": attr.label(mandatory = True, allow_single_file = True),
        "member_manifests": attr.label_list(mandatory = True, allow_files = True),
    },
    doc = "Mirrors Cargo workspace manifests with madsim-tokio un-aliased for crate_universe.",
)

def _firecracker_manifests_repo_impl(rctx):
    base_file = rctx.path(rctx.attr.firecracker_base)
    fc_dir = base_file.dirname.get_child("firecracker")

    rctx.file("Cargo.toml", rctx.read(fc_dir.get_child("Cargo.toml")))
    rctx.file("Cargo.lock", rctx.read(fc_dir.get_child("Cargo.lock")))

    exported = ['"Cargo.toml"', '"Cargo.lock"']
    src_root = fc_dir.get_child("src")
    for member_dir in src_root.readdir():
        if not member_dir.is_dir:
            continue
        manifest = member_dir.get_child("Cargo.toml")
        if not manifest.exists:
            continue
        rel_dir = "src/" + member_dir.basename
        text = rctx.read(manifest)
        # Ensure `futures` (optional dep of vmm's `cladefs` feature) is materialized in @firecracker_crates.
        if member_dir.basename == "vmm":
            text = text.replace(
                'futures = { version = "0.3.34", default-features = false, features = ["executor"], optional = true }',
                'futures = { version = "0.3.34", default-features = false, features = ["executor"] }',
            )
            # Strip cladefs feature's dep:futures since futures is now unconditional in the mirrored manifest.
            text = text.replace('cladefs = ["dep:futures"]', 'cladefs = []')
        rctx.file(rel_dir + "/Cargo.toml", text)
        for entry in member_dir.readdir():
            if entry.basename in ("Cargo.toml", "BUILD", "BUILD.bazel"):
                continue
            rctx.symlink(entry, rel_dir + "/" + entry.basename)
        exported.append('"{}/Cargo.toml"'.format(rel_dir))

    rctx.file(
        "BUILD.bazel",
        'exports_files([{}], visibility = ["//visibility:public"])\n'.format(
            ", ".join(exported),
        ),
    )

_firecracker_manifests_repo = repository_rule(
    implementation = _firecracker_manifests_repo_impl,
    attrs = {
        "firecracker_base": attr.label(mandatory = True, allow_single_file = True),
    },
    doc = "Mirrors vendor/firecracker's Cargo manifests into an external repo so crate_universe never writes inside the git submodule.",
)

def _cargo_manifests_ext_impl(mctx):
    for mod in mctx.modules:
        for cfg in mod.tags.workspace:
            _cargo_manifests_repo(
                name = cfg.name,
                cargo_lockfile = cfg.cargo_lockfile,
                root_manifest = cfg.root_manifest,
                member_manifests = cfg.member_manifests,
            )
        for cfg in mod.tags.firecracker:
            _firecracker_manifests_repo(
                name = cfg.name,
                firecracker_base = cfg.firecracker_base,
            )

_workspace_tag = tag_class(
    attrs = {
        "name": attr.string(mandatory = True),
        "cargo_lockfile": attr.label(mandatory = True, allow_single_file = True),
        "root_manifest": attr.label(mandatory = True, allow_single_file = True),
        "member_manifests": attr.label_list(mandatory = True, allow_files = True),
    },
)

_firecracker_tag = tag_class(
    attrs = {
        "name": attr.string(mandatory = True),
        "firecracker_base": attr.label(mandatory = True, allow_single_file = True),
    },
)

cargo_manifests = module_extension(
    implementation = _cargo_manifests_ext_impl,
    tag_classes = {
        "firecracker": _firecracker_tag,
        "workspace": _workspace_tag,
    },
)
