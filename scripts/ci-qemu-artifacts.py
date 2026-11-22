#!/usr/bin/env python3
import hashlib
import json
import os
import pathlib
import shutil
import struct
import subprocess
import sys
import tarfile

ENVIRONMENT = json.loads(pathlib.Path(__file__).with_name("qemu-pilot-environment.json").read_text())
BUILDER_IMAGE = ENVIRONMENT["builder"]
TARGET_ROOT = pathlib.Path("/benchmark-target")


def workload_plan(workload):
    if workload == "single":
        return {"targets": ["--test", "capi_basic"], "profile": [],
                "filter": ["mount_and_umount_basic_image", "--exact", "--nocapture"], "floor": 1}
    if workload == "smoke":
        return {"targets": ["--lib", "--test", "ro_compat_write_guard"], "profile": [],
                "filter": ["--skip", "needs_host::"], "floor": 440}
    if workload == "full":
        return {"targets": [], "profile": ["--profile", "test-opt"], "filter": [], "floor": 1078}
    raise ValueError(f"unsupported workload: {workload}")


def normalize_arch(arch):
    aliases = {"amd64": "x86_64", "x86_64": "x86_64", "arm64": "aarch64", "aarch64": "aarch64"}
    if arch not in aliases:
        raise ValueError(f"unsupported architecture: {arch}")
    return aliases[arch]


def guest_policy(action, arch, accelerator, host_arch, cache_hit):
    if action not in ("run", "prepare", "prepare-local"):
        raise ValueError(f"unsupported guest operation: {action}")
    arch, host_arch = normalize_arch(arch), normalize_arch(host_arch)
    if accelerator == "hvf" and (arch != "aarch64" or host_arch != "aarch64"):
        raise ValueError("HVF requires a native ARM64 guest and host")
    if cache_hit:
        return
    if action == "run":
        raise ValueError("prepared guest cache miss; run the QEMU guest preparation workflow first; cold setup is forbidden in measured trials")
    if action == "prepare-local":
        return
    if arch != host_arch:
        raise ValueError("guest preparation requires a matching native host, not cross-architecture TCG")
    if arch == "x86_64" and accelerator != "kvm":
        raise ValueError("x86_64 guest preparation requires KVM")


def digest_file(path):
    digest = hashlib.sha256()
    with open(path, "rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def elf_machine(path, arch):
    with open(path, "rb") as source:
        header = source.read(20)
    if len(header) != 20 or header[:6] != b"\x7fELF\x02\x01":
        raise ValueError(f"not a little-endian 64-bit ELF executable: {path}")
    machine = struct.unpack_from("<H", header, 18)[0]
    if machine != {"x86_64": 62, "aarch64": 183}[arch]:
        raise ValueError(f"wrong ELF machine {machine} for {arch}: {path}")
    return machine


def collect(records, target_root, arch):
    artifacts = {}
    for record in records:
        if record.get("reason") != "compiler-artifact" or not record.get("executable"):
            continue
        path = pathlib.Path(record["executable"])
        path.resolve(strict=True).relative_to(target_root.resolve())
        elf_machine(path, arch)
        artifacts[str(path)] = {"path": str(path), "test": bool(record.get("profile", {}).get("test")),
                                "sha256": digest_file(path), "name": record["target"]["name"]}
    if not any(artifact["test"] for artifact in artifacts.values()):
        raise ValueError("no test executables built")
    return list(artifacts.values())


def verify(manifest, arch, workload):
    if manifest["arch"] != arch or manifest["workload"] != workload:
        raise ValueError("test bundle architecture or workload mismatch")
    if not any(artifact["test"] for artifact in manifest["artifacts"]):
        raise ValueError("no test executables in bundle")
    for artifact in manifest["artifacts"]:
        path = pathlib.Path(artifact["path"])
        elf_machine(path, arch)
        if digest_file(path) != artifact["sha256"]:
            raise ValueError(f"executable checksum mismatch: {path}")


def run_tests(manifest, arch, workload):
    verify(manifest, arch, workload)
    for artifact in manifest["artifacts"]:
        if artifact["test"]:
            print(f"Running prebuilt {artifact['name']} ({arch})", flush=True)
            status = subprocess.run([artifact["path"], *workload_plan(workload)["filter"]], check=False).returncode
            if status:
                return status if status > 0 else 128 - status
    return 0


def unpack(bundle, destination):
    root = destination.resolve()
    with tarfile.open(bundle, "r:gz") as archive:
        for member in archive:
            relative = pathlib.PurePosixPath(member.name)
            if (not member.isfile() or relative.is_absolute() or ".." in relative.parts
                    or not relative.parts or relative.parts[0] != "benchmark-target"):
                raise ValueError(f"unsafe test bundle member: {member.name}")
            target = (root / member.name).resolve()
            target.relative_to(root / "benchmark-target")
            target.parent.mkdir(parents=True, exist_ok=True)
            with archive.extractfile(member) as source, target.open("wb") as output:
                shutil.copyfileobj(source, output, 1024 * 1024)
            target.chmod(0o755)


def hash_inputs(root, names):
    digest = hashlib.sha256()
    for name in sorted(names):
        path = root / name
        digest.update(os.fsencode(name) + b"\0")
        digest.update((os.readlink(path) if path.is_symlink() else digest_file(path)).encode() + b"\0")
    return digest.hexdigest()


def tracked_inputs(root):
    return subprocess.check_output(
        ["git", "-C", str(root), "ls-files", "-z", "--cached", "--others", "--exclude-standard"]
    ).decode().strip("\0").split("\0")


def runner_recipe(root, core, arch):
    inputs = ["scripts/qemu-runner/Dockerfile", "scripts/qemu-runner/setup.sh",
              "scripts/qemu-runner/packages.txt", "rust-toolchain.toml"]
    recipe = {"schema": 1, "arch": normalize_arch(arch), "base": BUILDER_IMAGE,
              "epoch": ENVIRONMENT["refresh_epoch"], "oras": ENVIRONMENT["oras"],
              "setup": hash_inputs(root, inputs),
              "core_setup": hash_inputs(core, ["scripts/guest-rust-toolchain.sh"])}
    key = hashlib.sha256(json.dumps(recipe, sort_keys=True).encode()).hexdigest()
    return {"key": key, "recipe": recipe, "arch": recipe["arch"]}


def cache_keys(root, core, arch, workload):
    plan = workload_plan(workload)
    setup = ["scripts/ci-qemu-guest.sh", "scripts/ci-qemu-boot.sh", "scripts/ci-qemu-image-clean.sh",
             "scripts/qemu-guest-network.yaml", "scripts/vm-setup.sh",
             "test-disks/guest-build-images.sh", "rust-toolchain.toml"]
    setup += [name for name in tracked_inputs(root) if name.startswith("tests/lwext4/")]
    environment = {"schema": 1, "epoch": ENVIRONMENT["refresh_epoch"], "arch": arch, "workload": workload,
                   "cloud_image": ENVIRONMENT["cloud_images"][arch],
                   "setup": hash_inputs(root, setup),
                   "core_setup": hash_inputs(core, ["scripts/guest-rust-toolchain.sh"])}
    guest_key = hashlib.sha256(json.dumps(environment, sort_keys=True).encode()).hexdigest()
    build = {"schema": 1, "arch": arch, "workload": workload, "plan": plan,
             "builder": BUILDER_IMAGE, "source": hash_inputs(root, tracked_inputs(root)),
             "core": hash_inputs(core, tracked_inputs(core))}
    test_key = hashlib.sha256(json.dumps(build, sort_keys=True).encode()).hexdigest()
    return {"guest": guest_key, "tests": test_key, "environment": environment, "build": build}


def main(arguments):
    action = arguments[0]
    if action == "builder":
        print(BUILDER_IMAGE)
    elif action == "runner-recipe":
        root = pathlib.Path(arguments[1]).resolve()
        print(json.dumps(runner_recipe(root, root.parent / "rust-fs-core", arguments[2]), indent=2))
    elif action == "image":
        image = ENVIRONMENT["cloud_images"][arguments[1]]
        print(f"{image['name']} {image['sha512']} {ENVIRONMENT['cloud_build']}")
    elif action == "targets":
        plan = workload_plan(arguments[1])
        print("\n".join(plan["profile"] + plan["targets"]))
    elif action == "floor":
        print(workload_plan(arguments[1])["floor"])
    elif action == "policy":
        guest_policy(*arguments[1:5], arguments[5] == "true")
    elif action == "keys":
        root = pathlib.Path(arguments[1]).resolve()
        print(json.dumps(cache_keys(root, root.parent / "rust-fs-core", arguments[2], arguments[3]), indent=2))
    elif action == "pack":
        evidence, arch, workload = pathlib.Path(arguments[1]), arguments[2], arguments[3]
        records = [json.loads(line) for line in (evidence / "artifacts.jsonl").read_text().splitlines()]
        artifacts = collect(records, TARGET_ROOT, arch)
        manifest = {"arch": arch, "workload": workload, "artifacts": artifacts}
        (evidence / "test-bundle.json").write_text(json.dumps(manifest, indent=2) + "\n")
        with tarfile.open(evidence / "test-bundle.tar.gz", "w:gz") as archive:
            for artifact in artifacts:
                archive.add(artifact["path"], arcname=str(pathlib.Path(artifact["path"]).relative_to("/")))
    elif action == "unpack":
        unpack(arguments[1], pathlib.Path("/"))
    elif action == "run":
        manifest = json.loads(pathlib.Path(arguments[1]).read_text())
        return run_tests(manifest, arguments[2], arguments[3])
    else:
        raise ValueError(f"unknown artifact operation: {action}")
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main(sys.argv[1:]))
    except (ValueError, OSError, KeyError) as error:
        sys.exit(f"QEMU artifact error: {error}")
